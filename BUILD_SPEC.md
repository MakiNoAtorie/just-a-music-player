# Minimal Music Player — Build Spec (Draft 2)

## Goal
A functional, ultra-lightweight desktop music player for Windows, structured so a macOS port later is mostly a build target, not a rewrite. Fast startup, low memory, near-zero idle CPU, clean minimalist look.

## Budgets (targets, not hard limits)
- Release binary: under 10 MB
- Idle memory while playing: under 40 MB
- Idle CPU while playing: near zero outside decode bursts
- Cold start to visible window: under 300 ms

## Stack
- Language: Rust (stable)
- UI: Slint (winit backend, software renderer first; switch to femtovg only if rendering CPU is too high)
- Decoding: symphonia (MP3, FLAC, WAV, Ogg Vorbis, AAC-LC/ALAC in M4A). Check current Opus support; if absent, leave Opus out of v1.
- Resampling: rubato
- Audio output: cpal (WASAPI shared mode on Windows, CoreAudio on macOS)
- Ring buffer: rtrb (lock-free SPSC)
- Tags and cover art: lofty, with image (jpeg + png features only) for decoding and downscaling art
- Media keys / OS now-playing: souvlaki
- Folder scanning: walkdir
- Config and session: serde + toml, stored under the OS config dir via directories

## v1 Features
- Open files and folders (dialog and drag-and-drop); folders scanned recursively for supported formats
- Play queue: add, remove, reorder, clear, double-click to play
- Transport: play/pause, stop, next, previous, seek bar, volume (perceptual curve), mute
- Shuffle and repeat (off / all / one)
- Now-playing area: cover art, title, artist, album, elapsed/total time
- Gapless playback between consecutive tracks
- Keyboard shortcuts for all transport actions
- Media keys and Windows media overlay via souvlaki
- Files passed as command-line arguments are queued and played (supports "Open with")
- Session restore: queue, current track, position, volume, shuffle/repeat, window size
- Unreadable or unsupported files are skipped with a small non-blocking notice

## Deferred (post-v1)
- Exclusive-mode output on Windows (bit-perfect, native sample rate per track) via the wasapi crate, as an opt-in setting
- Single-instance behavior (forward files from a second launch to the running one)
- ReplayGain
- Library view / indexed collection
- Mini player mode
- Taskbar thumbnail buttons
- Equalizer, crossfade, playlists saved as files

## Architecture
Three layers that only talk through channels and shared atomics:

1. UI thread (Slint event loop): renders state, sends commands.
2. Engine thread: owns the queue, the decoder, and the resampler. Decodes ahead into the ring buffer, preloads the next track near the end of the current one for gapless playback, handles commands.
3. Audio callback: only reads from the ring buffer, applies volume, writes to the device. It never allocates, locks, or blocks.

Commands flow UI → engine over a channel (Play, Pause, Seek, Next, SetVolume, Enqueue, etc.). State flows engine → UI via `slint::invoke_from_event_loop` with a weak window handle, throttled to roughly 10 updates per second for position.

Audio output sits behind a small backend trait. v1 ships the cpal (shared mode) backend only; the design must allow adding a Windows exclusive-mode backend (wasapi crate) later without touching the engine.

### Engine rules
- Playback position comes from a frame counter incremented by the audio callback, never from decode progress.
- Each seek or track change bumps an epoch counter; the callback drops samples tagged with an older epoch.
- All audio is converted to the device's sample rate, channel count, and sample format before entering the ring buffer. Mono is upmixed; more than two channels are downmixed to stereo.
- Volume and mute are atomics read by the callback.
- On stream errors (device unplugged, default device changed), the engine reopens the stream on the current default device and resumes from the last played position.

## UI
- Single compact window, dark minimalist theme, resizable.
- Layout: cover art and track info at the top, transport and seek bar in the middle, queue list below.
- Cover art decoded off the UI thread and downscaled before display.
- Respects Windows DPI scaling.
- Include Slint attribution if using its royalty-free license (About dialog is fine).

## Platform Layer
Keep all OS-specific code in one `platform` module: media controls setup (passing the native window handle to souvlaki), config paths, and file-association / argument handling. Everything else stays cross-platform so macOS needs only this module and packaging work.

Windows specifics:
- `#![windows_subsystem = "windows"]` in release builds
- Embedded app icon and version info

## Release Profile
```toml
[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

## Milestones
- M0 (gate): headless playback of a single file through symphonia → rubato → rtrb → cpal on Windows, at correct speed and pitch on a 48 kHz device.
- M1: engine complete: queue, seek, volume, gapless, shuffle/repeat, device recovery.
- M2: Slint UI wired to the engine.
- M3: Windows integration: media keys, command-line files, session restore.
- M4: size and memory pass against the budgets.