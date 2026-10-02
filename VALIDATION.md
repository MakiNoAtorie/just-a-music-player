# Validation — Windows x64, 2 October 2026

## Automated checks

- `cargo test --offline`: eleven tests pass. The engine test exercises a WAV transition between mixed sample rates with no intermediate underflow, callback-derived position, pause and seek, queue reorder/removal, volume/mute, shuffle, repeat, and session restore on a simulated 48 kHz device.
- A stereo FLAC sweep at 44.1, 48, and 96 kHz is decoded to 48 kHz. Every output sample is compared with the generated waveform to detect changed speed, missing blocks, or discontinuities; all three are exactly four seconds long. A separate WAV test checks duration, pitch, leading/trailing samples, and seeking.
- Callback tests check complete stereo frames across epoch changes, device-format conversion, pause behavior, and unsigned-device silence.
- `cargo clippy --offline --all-targets` and `cargo fmt --check` are clean.
- Full dark, full light, and compact layouts were rendered by the real Slint window and visually inspected. The queue rows no longer overlap, the progress bar fills its full width, and compact controls fit the window.

## Local device playback

Headless mode exercises Symphonia → Rubato → rtrb → CPAL/WASAPI shared mode on this computer's 48 kHz speaker output. The generated 44.1, 48, and 96 kHz stereo FLAC files each reached a reported final position of exactly four seconds. Played in one three-track sequence, they finished in 12.046 seconds, with all three tracks reported and two callback underflows, including final-buffer drain. Individual runs counted two or three underflows. The counter alone cannot locate those events. The affected FLAC file reported by the user was not available during these checks, so this validation does not prove that every real-world FLAC case is resolved.

The output ring now stores complete device-format frames. A seek or track change discards an old frame as one unit, preventing left and right channels from being paired across different epochs. The callback still performs no allocation, locking, or logging.

## Budget sample

The updated release measured 11,642,368 bytes (11.6 MB) for the executable, 32,006,144 bytes (32.0 MB) working set while playing, 12,247,040 bytes (12.2 MB) private allocation, 2.493% of one CPU core (0.208% of this machine), and 65 ms to a native window handle. These are point-in-time measurements, not guarantees. The executable remains above the draft's 10 MB target; memory remains below its 40 MB target. Window-handle timing is a startup proxy, not a strict cold-cache first-paint measure.

## Remaining verification and codec limits

- Physical unplug/replug and default-device changes have not been tested on hardware. Recovery code reopens the current default output and preserves playback position/state; checks use device names every two seconds.
- Media-control initialization and metadata publishing worked without errors. Hardware media-key presses, native dialogs, and actual drag/drop interaction need a manual pass.
- Generated FLAC and WAV are exercised. MP3, Vorbis, AAC-LC, and ALAC decoders are enabled but have not all been exercised with representative real-world files.
- Symphonia 0.5.5 supports gapless MP3/FLAC/WAV/Vorbis. Sample-exact AAC/M4A encoder-delay and padding removal remains incomplete. Opus is excluded.
- macOS has not been compiled or tested on a Mac.
