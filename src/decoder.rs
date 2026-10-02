use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::{collections::VecDeque, fs::File, path::Path};
use symphonia::core::{
    audio::{Channels, SampleBuffer},
    codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions},
    errors::{Error, SeekErrorKind},
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
    units::{Time, TimeBase},
};

const CHUNK: usize = 1024;
pub struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: TimeBase,
    input_rate: u32,
    output_rate: u32,
    pending: VecDeque<[f32; 2]>,
    resampler: Option<SincFixedIn<f32>>,
    input_frames: u64,
    emitted: u64,
    eof: bool,
    pub duration: f64,
    pub start: f64,
}

impl Source {
    pub fn open(path: &Path, output_rate: u32, position: f64) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|s| s.to_str()) {
            hint.with_extension(extension);
        }
        let result = symphonia::default::get_probe()
            .format(
                &hint,
                MediaSourceStream::new(Box::new(file), Default::default()),
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| e.to_string())?;
        let mut format = result.format;
        let track = format
            .default_track()
            .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .or_else(|| {
                format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            })
            .ok_or("No supported audio track")?;
        let track_id = track.id;
        let input_rate = track
            .codec_params
            .sample_rate
            .ok_or("Unknown sample rate")?;
        if input_rate == 0 || output_rate == 0 {
            return Err("Invalid sample rate".into());
        }
        let time_base = track
            .codec_params
            .time_base
            .unwrap_or(TimeBase::new(1, input_rate));
        let duration = track
            .codec_params
            .n_frames
            .map(|n| {
                let t = time_base.calc_time(n);
                t.seconds as f64 + t.frac
            })
            .unwrap_or(0.0);
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| e.to_string())?;
        let start = if position.is_finite() {
            position
                .max(0.0)
                .min(if duration > 0.0 { duration } else { f64::MAX })
        } else {
            0.0
        };
        let mut eof = false;
        if start > 0.0 {
            let seek = format.seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(start),
                    track_id: Some(track_id),
                },
            );
            match seek {
                Ok(_) => {}
                // Seeking to or past the last frame means "the track is over", not a bad file.
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    eof = true
                }
                Err(Error::SeekError(SeekErrorKind::OutOfRange)) => eof = true,
                Err(e) => return Err(e.to_string()),
            }
            decoder.reset();
        }
        let resampler = if input_rate == output_rate {
            None
        } else {
            Some(
                SincFixedIn::new(
                    output_rate as f64 / input_rate as f64,
                    1.0,
                    SincInterpolationParameters {
                        sinc_len: 128,
                        f_cutoff: 0.95,
                        interpolation: SincInterpolationType::Linear,
                        oversampling_factor: 128,
                        window: WindowFunction::BlackmanHarris2,
                    },
                    CHUNK,
                    2,
                )
                .map_err(|e| e.to_string())?,
            )
        };
        // SincFixedIn starts its interpolation index at -sinc_len/2, so its
        // first returned frame is already aligned with the input. output_delay
        // describes look-ahead latency here, not leading samples to discard.
        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            input_rate,
            output_rate,
            pending: VecDeque::new(),
            resampler,
            input_frames: 0,
            emitted: 0,
            eof,
            duration,
            start,
        })
    }

    fn read_packet(&mut self) -> Result<(), String> {
        let packet = loop {
            match self.format.next_packet() {
                Ok(p) if p.track_id() == self.track_id => break p,
                Ok(_) => continue,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(());
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        let decoded = match self.decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // Isolated corrupt packets can be skipped; terminal errors are surfaced.
            Err(Error::DecodeError(_)) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let spec = *decoded.spec();
        if spec.channels.count() == 0 {
            return Err("Audio track has no channels".into());
        }
        if spec.rate != self.input_rate {
            return Err("Sample rate changes inside a track are unsupported".into());
        }
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        let timestamp = self.time_base.calc_time(packet.ts());
        let seconds = timestamp.seconds as f64 + timestamp.frac;
        let skip = ((self.start - seconds).max(0.0) * self.input_rate as f64).round() as usize;
        for frame in samples
            .samples()
            .chunks_exact(spec.channels.count())
            .skip(skip)
        {
            self.pending.push_back(mix_stereo(frame, spec.channels));
            self.input_frames += 1;
        }
        Ok(())
    }

    pub fn next_chunk(&mut self) -> Result<Vec<[f32; 2]>, String> {
        loop {
            while self.pending.len() < CHUNK && !self.eof {
                self.read_packet()?;
            }
            if self.resampler.is_none() {
                return Ok(self
                    .pending
                    .drain(..self.pending.len().min(CHUNK))
                    .collect());
            }
            let target = (self.input_frames as f64 * self.output_rate as f64
                / self.input_rate as f64)
                .round() as u64;
            if self.eof && self.emitted >= target {
                return Ok(Vec::new());
            }
            let mut input = [vec![0.0; CHUNK], vec![0.0; CHUNK]];
            let count = self.pending.len().min(CHUNK);
            let [left, right] = &mut input;
            for (left, right) in left.iter_mut().zip(right.iter_mut()).take(count) {
                let frame = self.pending.pop_front().unwrap();
                *left = frame[0];
                *right = frame[1];
            }
            let output = self
                .resampler
                .as_mut()
                .unwrap()
                .process(&input, None)
                .map_err(|e| e.to_string())?;
            let available = output[0].len();
            let count = if self.eof {
                available.min(target.saturating_sub(self.emitted) as usize)
            } else {
                available
            };
            self.emitted += count as u64;
            let frames: Vec<_> = (0..count).map(|i| [output[0][i], output[1][i]]).collect();
            if !frames.is_empty() {
                return Ok(frames);
            }
        }
    }
}

fn mix_stereo(frame: &[f32], channels: Channels) -> [f32; 2] {
    if frame.len() == 1 {
        return [frame[0], frame[0]];
    }
    if frame.len() == 2 {
        return [frame[0], frame[1]];
    }
    let mut stereo = [0.0f32; 2];
    let mut weights = [0.0f32; 2];
    for (sample, channel) in frame.iter().zip(channels.iter()) {
        let weight = if channel == Channels::LFE1 || channel == Channels::LFE2 {
            [0.0, 0.0]
        } else if channel == Channels::FRONT_LEFT {
            [1.0, 0.0]
        } else if channel == Channels::FRONT_RIGHT {
            [0.0, 1.0]
        } else if channel == Channels::REAR_LEFT || channel == Channels::SIDE_LEFT {
            [0.707, 0.0]
        } else if channel == Channels::REAR_RIGHT || channel == Channels::SIDE_RIGHT {
            [0.0, 0.707]
        } else {
            [0.707, 0.707]
        };
        for side in 0..2 {
            stereo[side] += sample * weight[side];
            weights[side] += weight[side];
        }
    }
    for side in 0..2 {
        stereo[side] /= weights[side].max(1.0);
    }
    stereo
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn flac_stereo_waveform_is_continuous_at_device_rate() {
        for rate in [44100, 48000, 96000] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/stereo-{rate}.flac"));
            let mut source = Source::open(&path, 48000, 0.0).unwrap();
            let mut output = Vec::new();
            loop {
                let chunk = source.next_chunk().unwrap();
                if chunk.is_empty() {
                    break;
                }
                output.extend(chunk);
            }
            assert_eq!(output.len(), 192000, "{rate}: duration");
            // Compare every sample of two independent sweeps, including packet and
            // resampler block boundaries. A single fixed phase offset is allowed.
            let error = (-4..=4)
                .map(|offset| {
                    output
                        .iter()
                        .enumerate()
                        .skip(128)
                        .take(output.len() - 256)
                        .flat_map(|(i, frame)| {
                            let t = (i as f64 + offset as f64) / 48000.0;
                            [
                                (frame[0] as f64
                                    - 0.1
                                        * (std::f64::consts::TAU * (330.0 * t + 35.0 * t * t))
                                            .sin())
                                .abs(),
                                (frame[1] as f64
                                    - 0.1
                                        * (std::f64::consts::TAU * (550.0 * t + 45.0 * t * t))
                                            .sin())
                                .abs(),
                            ]
                        })
                        .fold(0.0_f64, f64::max)
                })
                .fold(f64::INFINITY, f64::min);
            assert!(
                error < 0.006,
                "{rate}: discontinuity or wrong speed, max error {error}"
            );
        }
    }
    pub fn wav(rate: u32, frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames as u32 * 2).to_le_bytes());
        for i in 0..frames {
            let value =
                ((i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 10000.0) as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn opening_at_or_past_the_end_is_not_an_error() {
        let wav_path = std::env::temp_dir().join(format!("maki-end-{}.wav", std::process::id()));
        std::fs::write(&wav_path, wav(44100, 44100)).unwrap();
        let flac_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stereo-44100.flac");
        for path in [&wav_path, &flac_path] {
            let duration = Source::open(path, 48000, 0.0).unwrap().duration;
            for position in [duration, duration + 5.0] {
                let result = Source::open(path, 48000, position).map(|_| ());
                assert!(result.is_ok(), "{path:?} at {position}: {result:?}");
            }
        }
        std::fs::remove_file(wav_path).unwrap();
    }
    #[test]
    fn resampling_preserves_duration_pitch_tail_and_mono() {
        let path = std::env::temp_dir().join(format!("maki-resample-{}.wav", std::process::id()));
        std::fs::write(&path, wav(44100, 44100)).unwrap();
        let mut source = Source::open(&path, 48000, 0.0).unwrap();
        let mut output = Vec::new();
        loop {
            let chunk = source.next_chunk().unwrap();
            if chunk.is_empty() {
                break;
            }
            output.extend(chunk);
        }
        assert_eq!(output.len(), 48000);
        assert!(output.iter().all(|f| f[0] == f[1]));
        let mut armed = false;
        let mut crossings = 0;
        for frame in &output {
            if frame[0] < -0.05 {
                armed = true;
            } else if frame[0] > 0.05 && armed {
                crossings += 1;
                armed = false;
            }
        }
        assert!((439..=441).contains(&crossings), "pitch: {crossings} Hz");
        let last_signal = output.iter().rposition(|f| f[0].abs() > 0.05).unwrap();
        assert!(
            last_signal > 47990,
            "tail was truncated: last signal {last_signal}"
        );
        assert!(
            output.iter().position(|f| f[0].abs() > 0.05).unwrap() < 10,
            "leading silence was inserted"
        );
        drop(source);
        let mut source = Source::open(&path, 48000, 0.5).unwrap();
        let mut frames = 0;
        loop {
            let chunk = source.next_chunk().unwrap();
            if chunk.is_empty() {
                break;
            }
            frames += chunk.len();
        }
        assert!(
            (23999..=24001).contains(&frames),
            "seek remaining: {frames}"
        );
        drop(source);
        std::fs::remove_file(path).unwrap();
    }
}
