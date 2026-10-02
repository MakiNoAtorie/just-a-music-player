use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

#[derive(Default)]
pub struct Shared {
    pub epoch: AtomicU64,
    pub paused: AtomicBool,
    pub gain: AtomicU32,
    pub mute: AtomicBool,
    pub device_error: AtomicBool,
    pub underruns: AtomicU64,
    sequence: AtomicU64,
    played_epoch: AtomicU64,
    played_token: AtomicU64,
    played_frame: AtomicU64,
}

impl Shared {
    pub fn clock(&self) -> (u64, u64, u64) {
        loop {
            let seq = self.sequence.load(Ordering::SeqCst);
            if seq & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let result = (
                self.played_epoch.load(Ordering::SeqCst),
                self.played_token.load(Ordering::SeqCst),
                self.played_frame.load(Ordering::SeqCst),
            );
            if seq == self.sequence.load(Ordering::SeqCst) {
                return result;
            }
        }
    }
    pub fn set_volume(&self, value: f32) {
        // The slider is perceptual: half its range is one eighth of full amplitude.
        self.gain
            .store(value.clamp(0.0, 1.0).powi(3).to_bits(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
pub struct Tagged<T> {
    epoch: u64,
    token: u64,
    frame: u64,
    values: [T; 2],
}

pub enum Samples {
    I8(Producer<Tagged<i8>>),
    I64(Producer<Tagged<i64>>),
    U8(Producer<Tagged<u8>>),
    U64(Producer<Tagged<u64>>),
    F32(Producer<Tagged<f32>>),
    F64(Producer<Tagged<f64>>),
    I16(Producer<Tagged<i16>>),
    I32(Producer<Tagged<i32>>),
    U16(Producer<Tagged<u16>>),
    U32(Producer<Tagged<u32>>),
}

impl Samples {
    pub fn slots(&mut self) -> usize {
        match self {
            Self::I8(p) => p.slots(),
            Self::I64(p) => p.slots(),
            Self::U8(p) => p.slots(),
            Self::U64(p) => p.slots(),
            Self::F32(p) => p.slots(),
            Self::F64(p) => p.slots(),
            Self::I16(p) => p.slots(),
            Self::I32(p) => p.slots(),
            Self::U16(p) => p.slots(),
            Self::U32(p) => p.slots(),
        }
    }
    pub fn push(&mut self, stereo: [f32; 2], channels: usize, epoch: u64, token: u64, frame: u64) {
        let stereo = if channels == 1 {
            let mono = (stereo[0] + stereo[1]) * 0.5;
            [mono, mono]
        } else {
            stereo
        };
        let stereo = stereo.map(|value| {
            if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        });
        macro_rules! push {
            ($p:expr, $t:ty) => {
                $p.push(Tagged {
                    epoch,
                    token,
                    frame,
                    values: stereo.map(<$t>::from_sample),
                })
                .expect("reserved ring slot")
            };
        }
        match self {
            Self::I8(p) => push!(p, i8),
            Self::I64(p) => push!(p, i64),
            Self::U8(p) => push!(p, u8),
            Self::U64(p) => push!(p, u64),
            Self::F32(p) => push!(p, f32),
            Self::F64(p) => push!(p, f64),
            Self::I16(p) => push!(p, i16),
            Self::I32(p) => push!(p, i32),
            Self::U16(p) => push!(p, u16),
            Self::U32(p) => push!(p, u32),
        }
    }
}

pub struct Output {
    pub samples: Samples,
    pub rate: u32,
    pub channels: usize,
    pub device_name: String,
    _stream: Box<dyn OutputStream>,
}

pub trait OutputStream {}
impl OutputStream for cpal::Stream {}

// This is the one extension point requested by the spec. A WASAPI exclusive
// backend can supply the same typed producer and callback clock later.
pub trait OutputBackend {
    fn open(&self, shared: Arc<Shared>) -> Result<Output, String>;
    fn default_device_name(&self) -> Option<String>;
}

pub struct CpalBackend;
impl OutputBackend for CpalBackend {
    fn default_device_name(&self) -> Option<String> {
        cpal::default_host()
            .default_output_device()
            .and_then(|d| d.name().ok())
    }
    fn open(&self, shared: Arc<Shared>) -> Result<Output, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No audio output device. Connect speakers or headphones.")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        if config.channels == 0 {
            return Err("Output device reports no channels".into());
        }
        let rate = config.sample_rate.0;
        let channels = config.channels as usize;
        let capacity = (rate as usize / 2).max(4096);
        macro_rules! stream {
            ($t:ty, $variant:ident) => {{
                let (producer, consumer) = RingBuffer::<Tagged<$t>>::new(capacity);
                let state = shared.clone();
                let errors = shared.clone();
                let mut callback = Callback {
                    consumer,
                    shared: state,
                    channels,
                };
                let stream = device
                    .build_output_stream(
                        &config,
                        move |data: &mut [$t], _| callback.render(data),
                        move |_| {
                            errors.device_error.store(true, Ordering::Relaxed);
                        },
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                stream.play().map_err(|e| e.to_string())?;
                (Samples::$variant(producer), stream)
            }};
        }
        let (samples, stream) = match format {
            SampleFormat::I8 => stream!(i8, I8),
            SampleFormat::I64 => stream!(i64, I64),
            SampleFormat::U8 => stream!(u8, U8),
            SampleFormat::U64 => stream!(u64, U64),
            SampleFormat::F32 => stream!(f32, F32),
            SampleFormat::F64 => stream!(f64, F64),
            SampleFormat::I16 => stream!(i16, I16),
            SampleFormat::I32 => stream!(i32, I32),
            SampleFormat::U16 => stream!(u16, U16),
            SampleFormat::U32 => stream!(u32, U32),
            _ => return Err(format!("Unsupported device sample format: {format}")),
        };
        Ok(Output {
            samples,
            rate,
            channels,
            device_name: device.name().unwrap_or_default(),
            _stream: Box::new(stream),
        })
    }
}

struct Callback<T> {
    consumer: Consumer<Tagged<T>>,
    shared: Arc<Shared>,
    channels: usize,
}
impl<T: SizedSample + FromSample<f32>> Callback<T>
where
    f32: FromSample<T>,
{
    // Real-time path: no allocation, locking, logging, channels, or OS calls.
    fn render(&mut self, output: &mut [T]) {
        output.fill(T::EQUILIBRIUM);
        if self.shared.paused.load(Ordering::Acquire) {
            return;
        }
        let gain = if self.shared.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.shared.gain.load(Ordering::Relaxed))
        };
        let mut latest = None;
        let mut missing = false;
        for frame in output.chunks_exact_mut(self.channels) {
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            let next = loop {
                match self.consumer.pop() {
                    Ok(sample) if sample.epoch == epoch => break Some(sample),
                    Ok(_) => continue,
                    Err(_) => break None,
                }
            };
            let Some(next) = next else {
                missing = true;
                break;
            };
            for (value, sample) in frame.iter_mut().zip(next.values) {
                *value = T::from_sample(f32::from_sample(sample) * gain);
            }
            // A command may invalidate this frame while it is being read.
            if self.shared.epoch.load(Ordering::Acquire) != epoch {
                frame.fill(T::EQUILIBRIUM);
                continue;
            }
            latest = Some((next.epoch, next.token, next.frame));
        }
        if let Some((epoch, token, frame)) = latest {
            self.shared.sequence.fetch_add(1, Ordering::SeqCst);
            self.shared.played_epoch.store(epoch, Ordering::SeqCst);
            self.shared.played_token.store(token, Ordering::SeqCst);
            self.shared.played_frame.store(frame, Ordering::SeqCst);
            self.shared.sequence.fetch_add(1, Ordering::SeqCst);
        }
        if missing {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    pub struct Backend;
    struct Stream {
        running: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl OutputStream for Stream {}
    impl Drop for Stream {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    impl OutputBackend for Backend {
        fn default_device_name(&self) -> Option<String> {
            Some("test-48k".into())
        }
        fn open(&self, shared: Arc<Shared>) -> Result<Output, String> {
            let (producer, consumer) = RingBuffer::new(24000);
            let running = Arc::new(AtomicBool::new(true));
            let live = running.clone();
            let thread = std::thread::spawn(move || {
                let mut callback = Callback {
                    consumer,
                    shared,
                    channels: 2,
                };
                let mut data = [0.0f32; 960];
                while live.load(Ordering::Relaxed) {
                    callback.render(&mut data);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            Ok(Output {
                samples: Samples::F32(producer),
                rate: 48000,
                channels: 2,
                device_name: "test-48k".into(),
                _stream: Box::new(Stream {
                    running,
                    thread: Some(thread),
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_discards_epochs_and_counts_only_audible_frames() {
        let shared = Arc::new(Shared::default());
        shared.set_volume(1.0);
        shared.epoch.store(2, Ordering::Relaxed);
        let (mut producer, consumer) = RingBuffer::new(8);
        producer
            .push(Tagged {
                epoch: 1,
                token: 7,
                frame: 47,
                values: [0.2f32, -0.2],
            })
            .unwrap();
        producer
            .push(Tagged {
                epoch: 2,
                token: 7,
                frame: 48,
                values: [0.5f32, -0.5],
            })
            .unwrap();
        let mut callback = Callback {
            consumer,
            shared: shared.clone(),
            channels: 2,
        };
        let mut data = [9.0; 2];
        callback.render(&mut data);
        assert_eq!(data, [0.5, -0.5]);
        assert_eq!(shared.clock(), (2, 7, 48));
        shared.paused.store(true, Ordering::Relaxed);
        callback.render(&mut data);
        assert_eq!(data, [0.0; 2]);
        assert_eq!(shared.clock(), (2, 7, 48));
    }
    #[test]
    fn unsigned_device_silence_is_the_midpoint() {
        let shared = Arc::new(Shared::default());
        let (_, consumer) = RingBuffer::<Tagged<u16>>::new(2);
        let mut callback = Callback {
            consumer,
            shared,
            channels: 1,
        };
        let mut data = [0u16; 2];
        callback.render(&mut data);
        assert_eq!(data, [32768; 2]);
    }
    #[test]
    fn device_frame_is_converted_once_and_keeps_stereo_aligned() {
        let shared = Arc::new(Shared::default());
        shared.set_volume(1.0);
        let (mut producer, consumer) = RingBuffer::<Tagged<i16>>::new(2);
        let mut samples = Samples::I16(producer);
        samples.push([0.25, -0.5], 2, 0, 3, 1);
        producer = match samples {
            Samples::I16(p) => p,
            _ => unreachable!(),
        };
        assert_eq!(producer.slots(), 1);
        let mut callback = Callback {
            consumer,
            shared,
            channels: 2,
        };
        let mut output = [0i16; 2];
        callback.render(&mut output);
        assert!(output[0] > 8000 && output[1] < -16000);
    }
}
