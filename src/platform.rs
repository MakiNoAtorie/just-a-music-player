use crate::engine::{Command, Session, State, Track};
use directories::ProjectDirs;
use lofty::{
    config::ParseOptions,
    file::{AudioFile, TaggedFileExt},
    probe::Probe,
    tag::Accessor,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    Model,
    winit_030::{WinitWindowAccessor, winit},
};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

pub const EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "oga", "m4a", "mp4", "aac"];
pub fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| EXTENSIONS.contains(&s.to_ascii_lowercase().as_str()))
}

/// Top-right corner of the outer window (physical pixels), to keep fixed across a resize.
pub fn top_right(window: &slint::Window) -> Option<(i32, i32)> {
    window
        .with_winit_window(|w| {
            let pos = w.outer_position().ok()?;
            Some((pos.x + w.outer_size().width as i32, pos.y))
        })
        .flatten()
}

/// Put the window's top-right corner back at `corner`, then nudge it fully onto its monitor.
pub fn anchor_top_right(window: &slint::Window, (right, top): (i32, i32)) {
    window.with_winit_window(|w| {
        let size = w.outer_size();
        let (mut x, mut y) = (right - size.width as i32, top);
        if let Some(m) = w.current_monitor() {
            let (at, extent) = (m.position(), m.size());
            x = x
                .min(at.x + extent.width as i32 - size.width as i32)
                .max(at.x);
            y = y
                .min(at.y + extent.height as i32 - size.height as i32)
                .max(at.y);
        }
        w.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
    });
}

pub fn session_path(override_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let directory = override_dir
        .or_else(|| ProjectDirs::from("", "", "Maki").map(|d| d.config_dir().to_owned()))
        .ok_or("Unable to locate the OS config directory")?;
    Ok(directory.join("session.toml"))
}
pub fn load(path: &Path) -> Result<Session, String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let mut session: Session =
                toml::from_str(&contents).map_err(|e| format!("Could not restore session: {e}"))?;
            if !session.position.is_finite() {
                session.position = 0.0;
            }
            if !session.width.is_finite() {
                session.width = 460.0;
            }
            if !session.height.is_finite() {
                session.height = 660.0;
            }
            Ok(session)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Session::default()),
        Err(e) => Err(format!("Could not read session: {e}")),
    }
}
pub fn save(path: &Path, session: &Session) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let contents = toml::to_string_pretty(session).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, contents).map_err(|e| e.to_string())?;
    fs::rename(&temporary, path).map_err(|e| format!("Could not save session: {e}"))
}

pub fn read_track(path: PathBuf) -> Result<Track, String> {
    // Verify that it can actually be opened. Content/codec validation happens at playback.
    fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut track = Track {
        id: 0,
        title: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        artist: String::new(),
        album: String::new(),
        duration: 0.0,
        path,
    };
    if let Ok(file) = Probe::open(&track.path)
        .and_then(|p| p.options(ParseOptions::new().read_cover_art(false)).read())
    {
        track.duration = file.properties().duration().as_secs_f64();
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            if let Some(title) = tag.title().filter(|t| !t.is_empty()) {
                track.title = title.into_owned();
            }
            track.artist = tag.artist().map(|s| s.into_owned()).unwrap_or_default();
            track.album = tag.album().map(|s| s.into_owned()).unwrap_or_default();
        }
    }
    Ok(track)
}

pub fn import(paths: Vec<PathBuf>, autoplay: bool, tx: Sender<Command>) {
    std::thread::spawn(move || {
        let mut autoplay = autoplay;
        let _ = tx.send(Command::Loading(true));
        let mut tracks = Vec::new();
        let mut skipped = 0;
        for path in paths {
            let files = if path.is_dir() {
                walkdir::WalkDir::new(path)
                    .follow_links(false)
                    .sort_by_file_name()
                    .into_iter()
                    .filter_map(|entry| match entry {
                        Ok(e) if e.file_type().is_file() && supported(e.path()) => {
                            Some(e.into_path())
                        }
                        Ok(_) => None,
                        Err(_) => {
                            skipped += 1;
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![path]
            };
            for path in files {
                if !supported(&path) {
                    skipped += 1;
                    continue;
                }
                match read_track(path) {
                    Ok(track) => tracks.push(track),
                    Err(_) => skipped += 1,
                }
                if tracks.len() == 64 {
                    let _ = tx.send(Command::Import(std::mem::take(&mut tracks), autoplay));
                    autoplay = false;
                }
            }
        }
        let _ = tx.send(Command::Import(tracks, autoplay));
        if skipped > 0 {
            let _ = tx.send(Command::Notice(format!(
                "Skipped {skipped} unreadable or unsupported file(s)."
            )));
        }
        let _ = tx.send(Command::Loading(false));
    });
}

thread_local! { static DIALOG_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Owned by the main window (so it stays on top and blocks it) and one at a time.
pub fn open_dialog(window: &slint::Window, folder: bool, tx: Sender<Command>) {
    if DIALOG_OPEN.replace(true) {
        return;
    }
    let dialog = window
        .with_winit_window(|w| rfd::AsyncFileDialog::new().set_parent(w))
        .unwrap_or_default()
        .set_title(if folder {
            "Add a music folder"
        } else {
            "Add music"
        });
    let _ = slint::spawn_local(async move {
        let paths: Option<Vec<PathBuf>> = if folder {
            dialog
                .pick_folder()
                .await
                .map(|f| vec![f.path().to_owned()])
        } else {
            dialog
                .add_filter("Audio", EXTENSIONS)
                .pick_files()
                .await
                .map(|files| files.iter().map(|f| f.path().to_owned()).collect())
        };
        DIALOG_OPEN.set(false);
        if let Some(paths) = paths {
            import(paths, false, tx);
        }
    });
}

pub fn read_cover(path: &Path) -> Option<image::RgbaImage> {
    let file = Probe::open(path)
        .ok()?
        .options(ParseOptions::new().read_properties(false))
        .read()
        .ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    let picture = tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(picture.data()))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    Some(reader.decode().ok()?.thumbnail(256, 256).into_rgba8())
}

pub fn cache_cover(session: &Path, pixels: &image::RgbaImage) -> Option<String> {
    let directory = session.parent()?;
    fs::create_dir_all(directory).ok()?;
    let path = directory.join("now-playing.png");
    pixels.save(&path).ok()?;
    url::Url::from_file_path(fs::canonicalize(path).ok()?)
        .ok()
        .map(|url| url.to_string())
}

struct Media {
    controls: MediaControls,
    track: Option<u64>,
    cover: Option<String>,
    next_update: Instant,
}
thread_local! { static MEDIA: RefCell<Option<Media>> = const { RefCell::new(None) }; }

pub fn attach_window(
    weak: slint::Weak<crate::PlayerWindow>,
    tx: Sender<Command>,
) -> Result<(), slint::EventLoopError> {
    use slint::ComponentHandle;
    slint::spawn_local(async move {
        let Some(app) = weak.upgrade() else {
            return;
        };
        match app.window().winit_window().await {
            Ok(_native_window) => setup_window(app.window(), tx, weak),
            Err(error) => {
                let _ = tx.send(Command::Notice(format!(
                    "Window integration unavailable: {error}"
                )));
            }
        }
    })
    .map(|_| ())
}

fn setup_window(
    window: &slint::Window,
    tx: Sender<Command>,
    app: slint::Weak<crate::PlayerWindow>,
) {
    use slint::ComponentHandle;
    let hwnd = window
        .with_winit_window(|w| match w.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get() as *mut std::ffi::c_void),
            _ => None,
        })
        .flatten();
    #[cfg(target_os = "windows")]
    if hwnd.is_none() {
        let _ = tx.send(Command::Notice(
            "Media controls unavailable: missing window handle".into(),
        ));
        return;
    }
    let result = (|| -> Result<MediaControls, souvlaki::Error> {
        let mut controls = MediaControls::new(PlatformConfig {
            display_name: "Just a music player",
            dbus_name: "maki",
            hwnd,
        })?;
        let commands = tx.clone();
        let raise = app.clone();
        controls.attach(move |event| {
            let command = match event {
                MediaControlEvent::Play => Command::Play,
                MediaControlEvent::Pause => Command::Pause,
                MediaControlEvent::Toggle => Command::Toggle,
                MediaControlEvent::Stop => Command::Stop,
                MediaControlEvent::Next => Command::Next,
                MediaControlEvent::Previous => Command::Previous,
                MediaControlEvent::SetPosition(p) => Command::Seek(p.0.as_secs_f64()),
                MediaControlEvent::SetVolume(v) => Command::Volume(v as f32),
                MediaControlEvent::Seek(direction) => {
                    Command::SeekRelative(if direction == SeekDirection::Forward {
                        5.0
                    } else {
                        -5.0
                    })
                }
                MediaControlEvent::SeekBy(direction, amount) => Command::SeekRelative(
                    amount.as_secs_f64()
                        * if direction == SeekDirection::Forward {
                            1.0
                        } else {
                            -1.0
                        },
                ),
                MediaControlEvent::Raise => {
                    let weak = raise.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            app.window().with_winit_window(|w| {
                                w.set_minimized(false);
                                w.focus_window();
                            });
                        }
                    });
                    return;
                }
                MediaControlEvent::Quit => {
                    let _ = slint::quit_event_loop();
                    return;
                }
                _ => return,
            };
            let _ = commands.send(command);
        })?;
        Ok(controls)
    })();
    match result {
        Ok(controls) => MEDIA.with(|m| {
            *m.borrow_mut() = Some(Media {
                controls,
                track: None,
                cover: None,
                next_update: Instant::now(),
            })
        }),
        Err(e) => {
            let _ = tx.send(Command::Notice(format!("Media controls unavailable: {e}")));
        }
    }
    let commands = tx;
    let mut modifiers = winit::keyboard::ModifiersState::empty();
    let drops = std::rc::Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let drop_timer = slint::Timer::default();
    let mut last_seek = Instant::now();
    let blur_timer = slint::Timer::default();
    window.on_winit_window_event(move |w, event| {
        match event {
            winit::event::WindowEvent::DroppedFile(path) => {
                // winit sends one event per file; gather them so the queue keeps drop order.
                drops.borrow_mut().push(path.clone());
                let (drops, tx) = (drops.clone(), commands.clone());
                drop_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(80),
                    move || import(std::mem::take(&mut *drops.borrow_mut()), false, tx.clone()),
                );
            }
            winit::event::WindowEvent::Resized(size) => {
                let size = size.to_logical::<f32>(w.scale_factor().into());
                let _ = commands.send(Command::WindowSize(size.width, size.height));
            }
            winit::event::WindowEvent::Focused(true) => blur_timer.stop(),
            winit::event::WindowEvent::Focused(false) => {
                // Wait a beat: minimising or one of our own dialogs also drops focus, and neither
                // should turn the player into a floating square.
                let app = app.clone();
                blur_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(250),
                    move || {
                        let Some(app) = app.upgrade() else { return };
                        let away = app
                            .window()
                            .with_winit_window(|w| !w.has_focus() && w.is_minimized() != Some(true))
                            .unwrap_or(false);
                        if away
                            && app.get_show_float_setting()
                            && app.get_float_on_blur()
                            && !(app.get_compact() && app.get_pinned())
                            && !DIALOG_OPEN.get()
                        {
                            app.invoke_change_look(app.get_dark_mode(), true, true, true);
                        }
                    },
                );
            }
            winit::event::WindowEvent::ModifiersChanged(value) => modifiers = value.state(),
            winit::event::WindowEvent::KeyboardInput { event, .. }
                if event.state == winit::event::ElementState::Pressed =>
            {
                use winit::keyboard::{KeyCode as K, PhysicalKey};
                let PhysicalKey::Code(key) = event.physical_key else {
                    return slint::winit_030::EventResult::Propagate;
                };
                let Some(app) = app.upgrade() else {
                    return slint::winit_030::EventResult::Propagate;
                };
                let selected = app.get_selected();
                let count = app.get_queue().row_count() as i32;
                // Bare keys only: Ctrl+R/Ctrl+S/Alt+Left etc. must not hit transport shortcuts.
                let plain =
                    !(modifiers.control_key() || modifiers.alt_key() || modifiers.super_key());
                // Logical key, so +/- work wherever the layout puts them (physical Equal/Minus don't on es-*).
                let volume_step = match &event.logical_key {
                    winit::keyboard::Key::Character(c) => match c.as_str() {
                        "+" | "=" => Some(0.05),
                        "-" | "_" => Some(-0.05),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(step) = volume_step.filter(|_| plain) {
                    let _ = commands.send(Command::VolumeRelative(step));
                    return slint::winit_030::EventResult::PreventDefault;
                }
                let command = match key {
                    // The queue is hidden behind About and in mini mode; don't edit it blind.
                    K::Delete | K::Enter | K::ArrowUp | K::ArrowDown
                        if app.get_about_visible() || app.get_compact() =>
                    {
                        None
                    }
                    K::KeyO if modifiers.control_key() => {
                        open_dialog(app.window(), modifiers.shift_key(), commands.clone());
                        None
                    }
                    K::Delete if modifiers.control_key() => {
                        app.set_selected(-1);
                        Some(Command::Clear)
                    }
                    K::Delete if selected >= 0 => {
                        app.set_selected(-1);
                        Some(Command::Remove(selected as usize))
                    }
                    K::ArrowUp | K::ArrowDown if modifiers.alt_key() && selected >= 0 => {
                        let target = (selected + if key == K::ArrowUp { -1 } else { 1 })
                            .clamp(0, (count - 1).max(0));
                        app.set_selected(target);
                        Some(Command::Move(selected as usize, target as usize))
                    }
                    K::ArrowUp | K::ArrowDown => {
                        if count > 0 {
                            app.set_selected(
                                (selected + if key == K::ArrowUp { -1 } else { 1 })
                                    .clamp(0, count - 1),
                            );
                        }
                        None
                    }
                    K::Enter if selected >= 0 => Some(Command::Select(selected as usize)),
                    K::Space if plain && !event.repeat => Some(Command::Toggle),
                    K::F1 => Some(Command::Previous),
                    K::F2 => Some(Command::Next),
                    K::KeyX if plain => Some(Command::Stop),
                    K::ArrowLeft | K::ArrowRight if plain => {
                        // Held keys repeat ~30/s and every seek reopens the file; scrub ~7 steps/s.
                        if event.repeat && last_seek.elapsed() < Duration::from_millis(150) {
                            None
                        } else {
                            last_seek = Instant::now();
                            Some(Command::SeekRelative(if key == K::ArrowLeft {
                                -5.0
                            } else {
                                5.0
                            }))
                        }
                    }
                    K::KeyM if plain && !event.repeat => Some(Command::Mute),
                    K::KeyS if plain && !event.repeat => Some(Command::Shuffle),
                    K::KeyR if plain && !event.repeat => Some(Command::Repeat),
                    K::Escape => {
                        app.set_about_visible(false);
                        None
                    }
                    _ => return slint::winit_030::EventResult::Propagate,
                };
                if let Some(command) = command {
                    let _ = commands.send(command);
                }
                return slint::winit_030::EventResult::PreventDefault;
            }
            _ => {}
        }
        slint::winit_030::EventResult::Propagate
    });
}

#[derive(Default)]
pub struct Arguments {
    pub files: Vec<PathBuf>,
    pub headless: bool,
    pub seconds: Option<f64>,
    pub config_dir: Option<PathBuf>,
    pub snapshot: Option<PathBuf>,
    pub report: Option<PathBuf>,
}
pub fn arguments() -> Result<Arguments, String> {
    let mut result = Arguments::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--headless") => result.headless = true,
            Some("--seconds") => {
                let value = args
                    .next()
                    .ok_or("--seconds requires a number")?
                    .to_string_lossy()
                    .parse::<f64>()
                    .map_err(|e| e.to_string())?;
                if !value.is_finite() || value <= 0.0 {
                    return Err("--seconds must be positive and finite".into());
                }
                result.seconds = Some(value);
            }
            Some("--config-dir") => {
                result.config_dir = Some(args.next().ok_or("--config-dir requires a path")?.into())
            }
            Some("--snapshot") => {
                result.snapshot = Some(args.next().ok_or("--snapshot requires a PNG path")?.into())
            }
            Some("--report") => {
                result.report = Some(args.next().ok_or("--report requires a path")?.into())
            }
            Some("--") => {
                result.files.extend(args.map(PathBuf::from));
                break;
            }
            Some(value) if value.starts_with("--") => {
                return Err(format!("Unknown option: {value}"));
            }
            _ => result.files.push(arg.into()),
        }
    }
    Ok(result)
}

pub fn update_media(state: &State) {
    MEDIA.with(|media| {
        let mut media = media.borrow_mut();
        let Some(media) = media.as_mut() else {
            return;
        };
        if media.track != state.track.as_ref().map(|t| t.id) || media.cover != state.cover_url {
            if let Some(track) = &state.track {
                let _ = media.controls.set_metadata(MediaMetadata {
                    title: Some(&track.title),
                    artist: Some(&track.artist),
                    album: Some(&track.album),
                    duration: Some(Duration::from_secs_f64(track.duration.max(0.0))),
                    cover_url: state.cover_url.as_deref(),
                });
            } else {
                let _ = media.controls.set_metadata(MediaMetadata::default());
            }
            media.track = state.track.as_ref().map(|t| t.id);
            media.cover = state.cover_url.clone();
            media.next_update = Instant::now();
        }
        if Instant::now() >= media.next_update || state.transport_changed {
            let progress = Some(MediaPosition(Duration::from_secs_f64(
                state.position.max(0.0),
            )));
            let playback = if state.playing {
                MediaPlayback::Playing { progress }
            } else if state.stopped {
                MediaPlayback::Stopped
            } else {
                MediaPlayback::Paused { progress }
            };
            let _ = media.controls.set_playback(playback);
            media.next_update = Instant::now() + Duration::from_secs(1);
        }
    });
}
