# Just a music player

A compact native Windows music player built from [the draft spec](BUILD_SPEC.md). Rust, Slint's software renderer, and a streaming audio engine. No web runtime or music library database.

## Run

Open `dist/Maki/maki-player.exe`, then add files/folders or drop them into the window. Double-click a queue row to play it. Click a row to select it; the selected row shows move-up, move-down and remove buttons (also Alt+Up/Down and Delete), and **Clear** sits in the queue header. The sun/moon button switches theme (the first run follows the OS). **Mini** collapses the window to a single strip with previous/play/next, title, time and a seek line; the mouse wheel changes volume over it. In Mini, the pin button makes it float on top with no title bar, and you can drag it from any empty spot; the expand button brings the full player back. The `?` button opens Settings (theme, and an option to float the mini player automatically when you switch to another window), the shortcut list, and credits. Hovering any control shows what it does in the footer line. Theme, mode and settings are saved.

The executable also accepts files and folders as arguments, queues them, and plays the first newly added file. This supports Windows **Open with → Choose an app → Choose an app on your PC**. It does not change file associations automatically.

```powershell
.\dist\Maki\maki-player.exe "C:\Music\album"
```

**YouTube:** copy a video or playlist link and press **Ctrl+V** (or **+ Link**). Audio streams without a download and seeks to any point. The queue keeps the links between sessions. This needs [yt-dlp](https://github.com/yt-dlp/yt-dlp) and `curl` on PATH (Windows 10 and later include curl). Starting a video takes about 2 s for yt-dlp; while one plays, the next one in the queue is prepared, so they play back to back. After 3 videos in a row fail (for example when offline), playback stops. Live streams are not supported. When YouTube changes, playback fails until you update yt-dlp (`yt-dlp -U`, or `pip install -U yt-dlp`).

Sessions restore paused, with queue, selected playing track, playback position, volume/mute, shuffle/repeat, theme, display mode, and full-view window size. The default Windows session file is `%APPDATA%\Maki\config\session.toml`. Saves happen every five seconds while playing, after settings changes, and on orderly exit. File imports append to the queue; restored duplicate tracks keep their original queue index.

## Build and checks

Use stable Rust with the MSVC C++ build tools and Windows SDK. The local development toolchain is in `.tools` and is excluded from source control. The helper automatically uses it when available; otherwise it uses `cargo` from PATH.

```powershell
.\scripts\cargo.ps1 build --release --locked
.\scripts\cargo.ps1 test --locked
.\scripts\cargo.ps1 clippy --locked --all-targets
.\scripts\cargo.ps1 fmt --check
.\scripts\package.ps1
```

Standard `cargo build --release` also works with a normal Rust installation. The release profile matches the draft: size optimization, LTO, one codegen unit, abort on panic, stripped symbols. `Cargo.lock` pins the verified dependencies.

For original, quiet test tones, run `python scripts/make-fixtures.py`. Headless mode uses the same engine, decoder, resampler, ring, and WASAPI device as the UI:

```powershell
.\target\debug\maki-player.exe --headless --seconds 5 --config-dir test-output\check-config --report test-output\check.txt test-output\01-first.wav test-output\02-next.wav
.\scripts\measure.ps1
```

`--config-dir` isolates development sessions. `--seconds` exits after a specified duration. `--snapshot file.png` renders a real Slint window after two seconds and exits. For release GUI-subsystem executables, use `Start-Process -Wait` when a script must wait for completion; headless reports are written with `--report`.

## Implementation

- The UI sends commands over a channel. Engine state reaches Slint through a weak handle and `invoke_from_event_loop`, at approximately 10 Hz during playback. Queue models rebuild only when the queue changes.
- The engine owns queue navigation, Symphonia decoders, Rubato sinc resampling, and a half-second output buffer. Files stream in bounded chunks. Tags, folder scans, and downscaled JPEG/PNG cover art are read off the UI thread.
- Each typed ring entry contains one complete device-format audio frame and epoch/segment/frame identifiers. The callback reads the SPSC buffer, applies the atomic cubic volume curve/mute, and writes output. It does not allocate, lock, log, or send messages. Paused playback preserves buffered audio.
- Playback time comes from a coherent callback clock, including after seek, pause, and device recovery. Explicit seeks and track changes invalidate previous epochs. Natural gapless transitions preserve the epoch and change the segment token at the audio boundary.
- Mono is duplicated to stereo; multichannel input is normalized into stereo, excluding LFE. Mono devices receive the stereo average; additional output-device channels receive silence. CPAL's f32/f64 and signed/unsigned 8/16/32/64-bit device formats are supported.
- `OutputBackend` is injected into the engine. An exclusive WASAPI implementation can replace CPAL without editing the engine. Tests use a simulated 48 kHz backend and the production callback.
- Device errors trigger reopening the default device; changes of default-device name are checked every two seconds. The last rendered position and paused/playing state are preserved. Devices with indistinguishable names may require an actual stream error before switching.
- Platform integration lives in `src/platform.rs`: native window handle, file dialogs/drop events, keyboard/media controls, config paths, CLI parsing, and OS cover-art cache. The macOS code path is present but has not been built or tested here.

## Validation and limits

See [VALIDATION.md](VALIDATION.md) for measured release budgets and the exact checks performed. The size, memory, CPU, and startup figures are measurements from this computer, not guarantees. The draft's budgets are targets.

Supported extensions: MP3, FLAC, WAV, Ogg Vorbis (`.ogg`/`.oga`), AAC, M4A/MP4 with AAC-LC or ALAC. Opus is excluded. Unknown codecs, missing files, empty audio, and decode errors produce a short notice and are skipped.

The engine adds no intentional silence at consecutive track boundaries. Tests verify WAV transitions at mixed sample rates without intermediate underflow, and a generated stereo FLAC sweep checks decoded audio for timing and discontinuities at 44.1, 48, and 96 kHz. **AAC/M4A encoder delay and padding are not fully handled by the selected Symphonia demuxer/decoder**; sample-exact gapless playback for those files remains a limitation. MP3/Vorbis gapless metadata is enabled. Other codec combinations and physical unplug/replug behavior need broader hardware/real-file validation.

Exclusive output, single instance, ReplayGain, library indexing, taskbar buttons, EQ, crossfade, and playlist files remain deferred as requested.

## Licenses

Maki source is MIT licensed. Slint is used under its Royalty-free Desktop, Mobile, and Web Applications License, with the actual `AboutSlint` widget in the About panel. See `licenses/Slint-Royalty-free-2.0.md`. Packaging includes dependency license files and a generated inventory in `THIRD_PARTY_NOTICES.md`.
