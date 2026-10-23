#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
mod audio;
mod decoder;
mod engine;
mod platform;
mod youtube;
use engine::{Command, Look, Repeat, State};
use slint::{
    ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::WinitWindowAccessor,
};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
slint::include_modules!();

const MINI_SIZE: slint::LogicalSize = slint::LogicalSize::new(360.0, 84.0);

fn time_text(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn render(app: &PlayerWindow, state: State) {
    platform::update_media(&state);
    if let Some(rows) = state.rows {
        app.set_queue(ModelRc::new(VecModel::from(
            rows.into_iter()
                .map(|track| QueueItem {
                    title: track.title.into(),
                    artist: track.artist.into(),
                    duration: if track.duration > 0.0 {
                        time_text(track.duration.ceil())
                    } else {
                        "—".into()
                    }
                    .into(),
                })
                .collect::<Vec<_>>(),
        )));
        if app.get_selected() >= app.get_queue().row_count() as i32 {
            app.set_selected(-1);
        }
    }
    let duration = state.track.as_ref().map_or(0.0, |t| t.duration);
    if let Some(track) = state.track {
        app.set_track_title(track.title.into());
        app.set_track_artist(if track.artist.is_empty() {
            "Unknown artist".into()
        } else {
            track.artist.into()
        });
        app.set_track_album(track.album.into());
    } else {
        app.set_track_title("Nothing playing".into());
        app.set_track_artist("".into());
        app.set_track_album("".into());
    }
    app.set_current(state.current.map_or(-1, |i| i as i32));
    app.set_playing(state.playing);
    app.set_progress(if duration > 0.0 {
        (state.position / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    });
    app.set_elapsed(time_text(state.position).into());
    app.set_total(time_text(duration.ceil()).into());
    app.set_volume(state.volume);
    app.set_muted(state.muted);
    app.set_shuffle(state.shuffle);
    app.set_repeat_mode(match state.repeat {
        Repeat::Off => 0,
        Repeat::All => 1,
        Repeat::One => 2,
    });
    app.set_notice(state.notice.into());
    app.set_notice_info(state.notice_info);
    app.set_loading(state.loading);
    if let Some(art) = state.cover {
        app.set_has_cover(art.is_some());
        app.set_cover(
            art.map(|pixels| {
                let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    pixels.as_raw(),
                    pixels.width(),
                    pixels.height(),
                );
                slint::Image::from_rgba8(buffer)
            })
            .unwrap_or_default(),
        );
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = platform::arguments()?;
    let path = platform::session_path(args.config_dir)?;
    let (session, notice) = match platform::load(&path) {
        Ok(session) => (session, String::new()),
        Err(e) => (engine::Session::default(), e),
    };
    if args.headless {
        return headless(args.files, args.seconds, args.report, session, path, notice);
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    let app = PlayerWindow::new()?;
    // First run (no saved choice) follows the OS theme.
    app.set_dark_mode(session.dark_mode.unwrap_or_else(|| app.get_system_dark()));
    app.invoke_apply_theme();
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.set_compact(session.compact);
    app.set_pinned(session.pinned);
    app.set_float_on_blur(session.auto_float);
    let full_size = std::rc::Rc::new(std::cell::Cell::new(slint::LogicalSize::new(
        session.width.clamp(400.0, 1600.0),
        session.height.clamp(580.0, 1600.0),
    )));
    app.window().set_size(if session.compact {
        MINI_SIZE
    } else {
        full_size.get()
    });
    let weak = app.as_weak();
    let (tx, engine) = engine::spawn(session, path, notice, audio::CpalBackend, move |state| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                render(&app, state);
            }
        });
    });
    let commands = tx.clone();
    let weak = app.as_weak();
    app.on_change_look(move |dark, compact, pinned, float| {
        if let Some(app) = weak.upgrade() {
            let was_compact = app.get_compact();
            let was_floating = app.get_compact() && app.get_pinned();
            // Mini <-> Full resizes around the top-right corner, not the top-left.
            let corner = platform::top_right(app.window());
            if compact && !was_compact {
                full_size.set(app.window().size().to_logical(app.window().scale_factor()));
            }
            // `pinned` is a preference that survives leaving Mini; it only floats the window in Mini.
            let floating = compact && pinned;
            app.set_dark_mode(dark);
            app.set_compact(compact);
            app.set_pinned(pinned);
            app.set_float_on_blur(float);
            if compact {
                app.set_about_visible(false);
            }
            let _ = commands.send(Command::Look(Look {
                dark: Some(dark),
                compact,
                pinned,
                auto_float: float,
            }));
            // Toggling the title bar changes the outer frame, so pin changes resize too.
            if compact != was_compact || floating != was_floating {
                // Apply the new layout constraints before asking the OS to resize.
                let weak = app.as_weak();
                let size = if compact { MINI_SIZE } else { full_size.get() };
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(app) = weak.upgrade() {
                        // Frame first, then size: the size is the client area inside whichever frame is on.
                        app.window()
                            .with_winit_window(|w| w.set_decorations(!floating));
                        app.window().set_size(size);
                        if let Some(corner) = corner {
                            platform::anchor_top_right(app.window(), corner);
                        }
                    }
                });
            }
        }
    });
    let weak = app.as_weak();
    app.on_drag_window(move || {
        if let Some(app) = weak.upgrade() {
            app.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    for folder in [false, true] {
        let (commands, weak) = (tx.clone(), app.as_weak());
        let open = move || {
            if let Some(app) = weak.upgrade() {
                platform::open_dialog(app.window(), folder, commands.clone());
            }
        };
        if folder {
            app.on_open_folder(open);
        } else {
            app.on_open_files(open);
        }
    }
    let commands = tx.clone();
    app.on_transport(move |action| {
        let command = match action.as_str() {
            "toggle" => Command::Toggle,
            "stop" => Command::Stop,
            "next" => Command::Next,
            "previous" => Command::Previous,
            "shuffle" => Command::Shuffle,
            "repeat" => Command::Repeat,
            "mute" => Command::Mute,
            _ => return,
        };
        let _ = commands.send(command);
    });
    let commands = tx.clone();
    app.on_change_volume(move |value| {
        let _ = commands.send(Command::Volume(value));
    });
    let commands = tx.clone();
    app.on_play_track(move |index| {
        if index >= 0 {
            let _ = commands.send(Command::Select(index as usize));
        }
    });
    let commands = tx.clone();
    app.on_remove_track(move |index| {
        if index >= 0 {
            let _ = commands.send(Command::Remove(index as usize));
        }
    });
    let commands = tx.clone();
    app.on_move_track(move |from, to| {
        if from >= 0 && to >= 0 {
            let _ = commands.send(Command::Move(from as usize, to as usize));
        }
    });
    let commands = tx.clone();
    app.on_clear_queue(move || {
        let _ = commands.send(Command::Clear);
    });
    let commands = tx.clone();
    app.on_paste_link(move || platform::paste_link(commands.clone()));
    let commands = tx.clone();
    app.on_seek(move |fraction| {
        let _ = commands.send(Command::SeekFraction(fraction));
    });
    app.show()?;
    platform::attach_window(app.as_weak(), tx.clone())?;
    if !args.files.is_empty() {
        platform::import(args.files, true, tx.clone());
    }
    if let Some(path) = args.snapshot {
        let weak = app.as_weak();
        slint::Timer::single_shot(Duration::from_secs(2), move || {
            if let Some(app) = weak.upgrade() {
                let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                    let buffer = app.window().take_snapshot()?;
                    image::save_buffer(
                        &path,
                        buffer.as_bytes(),
                        buffer.width(),
                        buffer.height(),
                        image::ColorType::Rgba8,
                    )?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = std::fs::write(path.with_extension("error.txt"), error.to_string());
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    if let Some(seconds) = args.seconds {
        slint::Timer::single_shot(Duration::from_secs_f64(seconds), || {
            let _ = slint::quit_event_loop();
        });
    }
    let result = slint::run_event_loop();
    let _ = tx.send(Command::Shutdown);
    let _ = engine.join();
    result?;
    Ok(())
}

fn headless(
    files: Vec<PathBuf>,
    seconds: Option<f64>,
    report: Option<PathBuf>,
    mut session: engine::Session,
    path: PathBuf,
    notice: String,
) -> Result<(), Box<dyn std::error::Error>> {
    if files.is_empty() {
        return Err("Headless playback requires an audio file or folder".into());
    }
    session.queue.clear();
    session.current = None;
    session.position = 0.0;
    session.repeat = Repeat::Off;
    session.shuffle = false;
    let (states, rx) = mpsc::sync_channel(16);
    let (tx, handle) = engine::spawn(session, path, notice, audio::CpalBackend, move |state| {
        let _ = states.try_send(state);
    });
    platform::import(files, true, tx.clone());
    let start = Instant::now();
    let mut began = false;
    let mut last_position = 0.0;
    let mut rate = 0;
    let mut underruns = 0;
    let mut tracks = std::collections::HashSet::new();
    let mut notices = Vec::new();
    loop {
        if let Ok(state) = rx.recv_timeout(Duration::from_millis(100)) {
            if !state.notice.is_empty() && !notices.contains(&state.notice) {
                notices.push(state.notice.clone());
            }
            if state.position > 0.0 && state.device_rate > 0 {
                began = true;
                if let Some(track) = state.track {
                    tracks.insert(track.id);
                }
            }
            last_position = state.position;
            rate = state.device_rate;
            underruns = state.underruns;
            if began && !state.playing && state.stopped {
                break;
            }
        }
        if seconds.is_some_and(|seconds| start.elapsed().as_secs_f64() >= seconds) {
            break;
        }
        if !began && start.elapsed() > Duration::from_secs(10) {
            break;
        }
    }
    let _ = tx.send(Command::Shutdown);
    let _ = handle.join();
    let text = format!(
        "played={began}\ndevice_rate={rate}\nlast_position_seconds={last_position:.6}\ntracks_played={}\ncallback_underruns={underruns}\nwall_seconds={:.3}\nnotices={}\n",
        tracks.len(),
        start.elapsed().as_secs_f64(),
        notices.join(" | ")
    );
    if let Some(path) = report {
        std::fs::write(path, &text)?;
    } else {
        println!("{text}");
    }
    if !began {
        return Err("No audio reached the playback engine; see the diagnostic report".into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Maki: {error}");
        #[cfg(all(target_os = "windows", not(debug_assertions)))]
        if !std::env::args_os().any(|arg| arg == "--headless") {
            rfd::MessageDialog::new()
                .set_title("Just a music player could not start")
                .set_description(error.to_string())
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        std::process::exit(1);
    }
}
