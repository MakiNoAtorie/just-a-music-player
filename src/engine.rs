use crate::{
    audio::{Output, OutputBackend, Shared},
    decoder::Source,
    platform,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Sender},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}
impl Repeat {
    fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub queue: Vec<PathBuf>,
    pub current: Option<usize>,
    pub position: f64,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub width: f32,
    pub height: f32,
    /// None until the user picks one, so first run can follow the OS theme.
    pub dark_mode: Option<bool>,
    pub compact: bool,
    /// Mini player floats on top with no title bar (only meaningful with `compact`).
    pub pinned: bool,
    /// Float the mini player automatically when the user switches to another window.
    pub auto_float: bool,
}

/// Everything about how the window looks; the UI sends it whole on any change.
#[derive(Clone, Copy)]
pub struct Look {
    pub dark: Option<bool>,
    pub compact: bool,
    pub pinned: bool,
    pub auto_float: bool,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            current: None,
            position: 0.0,
            volume: 0.8,
            muted: false,
            shuffle: false,
            repeat: Repeat::Off,
            width: 460.0,
            height: 660.0,
            dark_mode: None,
            compact: false,
            pinned: false,
            auto_float: false,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
}

pub enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Select(usize),
    Seek(f64),
    SeekRelative(f64),
    SeekFraction(f32),
    Volume(f32),
    VolumeRelative(f32),
    Mute,
    Shuffle,
    Repeat,
    Import(Vec<Track>, bool),
    Remove(usize),
    Move(usize, usize),
    Clear,
    Loading(bool),
    Notice(String),
    Cover(u64, Option<image::RgbaImage>),
    WindowSize(f32, f32),
    Look(Look),
    Shutdown,
}

pub struct State {
    pub rows: Option<Vec<Track>>,
    pub current: Option<usize>,
    pub track: Option<Track>,
    pub playing: bool,
    pub stopped: bool,
    pub position: f64,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub notice: String,
    /// Informational notices are calm; everything else is a warning.
    pub notice_info: bool,
    pub loading: bool,
    pub cover: Option<Option<image::RgbaImage>>,
    pub cover_url: Option<String>,
    pub transport_changed: bool,
    pub device_rate: u32,
    pub underruns: u64,
    /// Playback generation; tests use it to prove a command did not reopen the track.
    #[cfg_attr(not(test), allow(dead_code))]
    pub epoch: u64,
}

struct Segment {
    token: u64,
    id: u64,
    start: f64,
    written: u64,
    finished: bool,
}

pub fn spawn(
    session: Session,
    path: PathBuf,
    initial_notice: String,
    backend: impl OutputBackend + Send + 'static,
    mut publish: impl FnMut(State) + Send + 'static,
) -> (Sender<Command>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let commands = tx.clone();
    let handle = std::thread::Builder::new()
        .name("maki-engine".into())
        .spawn(move || {
            let shared = Arc::new(Shared::default());
            shared.paused.store(true, Ordering::Relaxed);
            let restored_index = session.current;
            let mut engine = Engine {
                queue: Vec::new(),
                current: None,
                position: session.position.max(0.0),
                playing: false,
                stopped: false,
                volume: if session.volume.is_finite() {
                    session.volume.clamp(0.0, 1.0)
                } else {
                    0.8
                },
                muted: session.muted,
                shuffle: session.shuffle,
                repeat: session.repeat,
                size: (session.width, session.height),
                look: Look {
                    dark: session.dark_mode,
                    compact: session.compact,
                    pinned: session.pinned,
                    auto_float: session.auto_float,
                },
                shared,
                backend: Box::new(backend),
                output: None,
                source: None,
                pending: VecDeque::new(),
                segments: VecDeque::new(),
                epoch: 0,
                token: 0,
                id: 0,
                bag: Vec::new(),
                history: Vec::new(),
                failed: HashSet::new(),
                random: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64
                    | 1,
                queue_dirty: true,
                changed: true,
                transport_changed: true,
                cover: Some(None),
                cover_id: None,
                cover_url: None,
                notice: initial_notice,
                notice_until: Instant::now() + Duration::from_secs(8),
                notice_info: false,
                lost: Vec::new(),
                loading: 0,
                tx: commands,
                session_path: path,
                config_dirty: false,
                recovery: false,
                resume_after_recovery: false,
            };
            for (original_index, path) in session.queue.into_iter().enumerate() {
                match platform::read_track(path.clone()) {
                    Ok(mut track) => {
                        engine.id += 1;
                        track.id = engine.id;
                        if Some(original_index) == restored_index {
                            engine.current = Some(track.id);
                        }
                        engine.queue.push(track);
                    }
                    Err(_) => {
                        // Keep the path: an unplugged drive must not erase the saved queue.
                        engine.lost.push(path);
                        engine
                            .notice("Some session files are unavailable; they stay saved.".into());
                    }
                }
            }
            if engine.current.is_none() {
                engine.current = engine.queue.first().map(|t| t.id);
                engine.position = 0.0;
            }
            engine.shared.set_volume(engine.volume);
            engine.shared.mute.store(engine.muted, Ordering::Relaxed);
            engine.reset_bag();
            engine.request_cover();
            engine.publish(&mut publish);
            let mut last_publish = Instant::now();
            let mut last_save = Instant::now();
            let mut last_device_check = Instant::now();
            loop {
                let timeout = if engine.playing || engine.recovery {
                    Duration::from_millis(20)
                } else {
                    Duration::from_millis(250)
                };
                match rx.recv_timeout(timeout) {
                    Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(command) => engine.command(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                for command in rx.try_iter().take(64) {
                    if matches!(command, Command::Shutdown) {
                        engine.save();
                        return;
                    }
                    engine.command(command);
                }
                engine.update_clock();
                if engine.playing {
                    engine.fill();
                }
                let device_error = engine.shared.device_error.swap(false, Ordering::Relaxed);
                if last_device_check.elapsed() >= Duration::from_secs(2) || device_error {
                    let changed = engine.output.as_ref().is_some_and(|o| {
                        engine.backend.default_device_name().as_deref() != Some(&o.device_name)
                    });
                    if changed || engine.recovery || device_error {
                        engine.recover();
                    }
                    last_device_check = Instant::now();
                }
                if !engine.notice.is_empty() && Instant::now() >= engine.notice_until {
                    engine.notice.clear();
                    engine.changed = true;
                }
                if engine.changed
                    || (engine.playing && last_publish.elapsed() >= Duration::from_millis(100))
                {
                    engine.publish(&mut publish);
                    last_publish = Instant::now();
                }
                if last_save.elapsed() >= Duration::from_secs(5) {
                    if engine.config_dirty || engine.playing {
                        engine.save();
                    }
                    last_save = Instant::now();
                }
            }
            engine.update_clock();
            engine.save();
        })
        .expect("start engine thread");
    (tx, handle)
}

struct Engine {
    queue: Vec<Track>,
    current: Option<u64>,
    position: f64,
    playing: bool,
    stopped: bool,
    volume: f32,
    muted: bool,
    shuffle: bool,
    repeat: Repeat,
    size: (f32, f32),
    look: Look,
    shared: Arc<Shared>,
    backend: Box<dyn OutputBackend + Send>,
    output: Option<Output>,
    source: Option<Source>,
    pending: VecDeque<[f32; 2]>,
    segments: VecDeque<Segment>,
    epoch: u64,
    token: u64,
    id: u64,
    bag: Vec<u64>,
    history: Vec<u64>,
    failed: HashSet<u64>,
    random: u64,
    queue_dirty: bool,
    changed: bool,
    transport_changed: bool,
    cover: Option<Option<image::RgbaImage>>,
    cover_id: Option<u64>,
    cover_url: Option<String>,
    notice: String,
    notice_until: Instant,
    notice_info: bool,
    /// Session paths that could not be read at startup; written back on save.
    lost: Vec<PathBuf>,
    loading: usize,
    tx: Sender<Command>,
    session_path: PathBuf,
    config_dirty: bool,
    recovery: bool,
    resume_after_recovery: bool,
}
impl Engine {
    fn index(&self, id: u64) -> Option<usize> {
        self.queue.iter().position(|t| t.id == id)
    }
    fn current_index(&self) -> Option<usize> {
        self.current.and_then(|id| self.index(id))
    }
    fn notice(&mut self, message: String) {
        self.notice = message;
        self.notice_info = false;
        self.notice_until = Instant::now() + Duration::from_secs(8);
        self.changed = true;
    }
    fn reset_bag(&mut self) {
        self.bag = self
            .queue
            .iter()
            .filter(|t| Some(t.id) != self.current && !self.failed.contains(&t.id))
            .map(|t| t.id)
            .collect();
        for i in (1..self.bag.len()).rev() {
            let j = self.rand() as usize % (i + 1);
            self.bag.swap(i, j);
        }
    }
    fn rand(&mut self) -> u64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        self.random
    }
    /// Nothing is stale until a successor track has been opened (or nothing is queued at all).
    fn successor_loaded(&self) -> bool {
        self.segments.len() > 1 || self.source.is_none()
    }
    fn next_id(&mut self, id: u64, automatic: bool) -> Option<u64> {
        if self.queue.is_empty() {
            return None;
        }
        if automatic && self.repeat == Repeat::One && !self.failed.contains(&id) {
            return Some(id);
        }
        if self.shuffle {
            if self.bag.is_empty() && self.repeat != Repeat::Off {
                let old = self.current;
                self.current = Some(id);
                self.reset_bag();
                self.current = old;
                if self.bag.is_empty() && !self.failed.contains(&id) {
                    return Some(id);
                }
            }
            while let Some(next) = self.bag.pop() {
                if self.index(next).is_some() && !self.failed.contains(&next) {
                    return Some(next);
                }
            }
            return None;
        }
        let index = self.index(id)?;
        for step in 1..=self.queue.len() {
            let next = index + step;
            if next >= self.queue.len() && self.repeat == Repeat::Off {
                return None;
            }
            let track = &self.queue[next % self.queue.len()];
            if !self.failed.contains(&track.id) {
                return Some(track.id);
            }
        }
        None
    }
    fn invalidate(&mut self) {
        // Return preloaded shuffle selections that have not reached the speakers.
        if self.shuffle {
            for segment in self.segments.iter().skip(1).rev() {
                if Some(segment.id) != self.current && !self.bag.contains(&segment.id) {
                    self.bag.push(segment.id);
                }
            }
        }
        self.epoch += 1;
        self.shared.epoch.store(self.epoch, Ordering::Release);
        self.source = None;
        self.pending.clear();
        self.segments.clear();
    }
    fn ensure_output(&mut self) -> bool {
        if self.output.is_some() {
            return true;
        }
        match self.backend.open(self.shared.clone()) {
            Ok(output) => {
                self.output = Some(output);
                self.recovery = false;
                true
            }
            Err(error) => {
                self.notice(error);
                if !self.recovery {
                    self.resume_after_recovery = self.playing;
                }
                self.recovery = true;
                false
            }
        }
    }
    fn open_source(&mut self, id: u64, position: f64) -> bool {
        let Some(index) = self.index(id) else {
            return false;
        };
        let rate = self.output.as_ref().unwrap().rate;
        match Source::open(&self.queue[index].path, rate, position) {
            Ok(source) => {
                if source.duration > 0.0
                    && (source.duration - self.queue[index].duration).abs() > 0.001
                {
                    self.queue[index].duration = source.duration;
                    self.queue_dirty = true;
                }
                self.token += 1;
                self.segments.push_back(Segment {
                    token: self.token,
                    id,
                    start: source.start,
                    written: 0,
                    finished: false,
                });
                self.source = Some(source);
                true
            }
            Err(error) => {
                self.failed.insert(id);
                self.notice(format!("Skipped {}: {error}", self.queue[index].title));
                false
            }
        }
    }
    fn start(&mut self, id: u64, position: f64, playing: bool) {
        self.update_clock();
        if self.current != Some(id)
            && let Some(previous) = self.current
        {
            self.history.push(previous);
            if self.history.len() > 512 {
                self.history.remove(0);
            }
        }
        self.shared.paused.store(true, Ordering::Release);
        self.invalidate();
        if self.shuffle {
            self.bag.retain(|next| *next != id);
        }
        self.current = Some(id);
        self.position = position.max(0.0);
        self.stopped = false;
        self.playing = playing;
        self.transport_changed = true;
        self.changed = true;
        self.config_dirty = true;
        self.request_cover();
        if !self.ensure_output() {
            self.playing = false;
            return;
        }
        let mut candidate = Some(id);
        for _ in 0..self.queue.len() {
            let Some(next) = candidate else {
                break;
            };
            if self.open_source(next, if next == id { position } else { 0.0 }) {
                if next != id {
                    self.current = Some(next);
                    self.position = 0.0;
                    self.request_cover();
                }
                self.fill();
                self.shared.paused.store(!self.playing, Ordering::Release);
                return;
            }
            candidate = self.next_id(next, false);
        }
        self.playing = false;
        self.stopped = true;
        self.recovery = false;
    }
    fn update_clock(&mut self) {
        let (epoch, token, frame) = self.shared.clock();
        if epoch != self.epoch {
            return;
        }
        if let Some(index) = self.segments.iter().position(|s| s.token == token) {
            for _ in 0..index {
                self.segments.pop_front();
            }
            let segment = self.segments.front().unwrap();
            let id = segment.id;
            self.position = segment.start
                + frame as f64 / self.output.as_ref().map_or(48000, |o| o.rate) as f64;
            if self.current != Some(id) {
                if let Some(previous) = self.current {
                    self.history.push(previous);
                    if self.history.len() > 512 {
                        self.history.remove(0);
                    }
                }
                self.current = Some(id);
                self.changed = true;
                self.request_cover();
            }
            let ended = self.segments.len() == 1
                && self
                    .segments
                    .front()
                    .is_some_and(|s| s.finished && frame >= s.written)
                && self.source.is_none()
                && self.pending.is_empty();
            if ended && self.playing {
                self.playing = false;
                self.stopped = true;
                self.shared.paused.store(true, Ordering::Release);
                self.changed = true;
                self.transport_changed = true;
                self.config_dirty = true;
            }
        }
    }
    fn fill(&mut self) {
        if self.output.is_none() || self.source.is_none() && self.pending.is_empty() {
            return;
        }
        // Half a second of look-ahead bounds memory and covers next-file open latency.
        for _ in 0..64 {
            let output = self.output.as_mut().unwrap();
            let room = output.samples.slots();
            if room == 0 {
                return;
            }
            if !self.pending.is_empty() {
                let segment = self.segments.back_mut().unwrap();
                for _ in 0..room.min(self.pending.len()) {
                    let frame = self.pending.pop_front().unwrap();
                    segment.written += 1;
                    output.samples.push(
                        frame,
                        output.channels,
                        self.epoch,
                        segment.token,
                        segment.written,
                    );
                }
                continue;
            }
            let Some(source) = self.source.as_mut() else {
                return;
            };
            match source.next_chunk() {
                Ok(frames) if !frames.is_empty() => self.pending.extend(frames),
                result => {
                    let segment = self.segments.back_mut().unwrap();
                    segment.finished = true;
                    let id = segment.id;
                    if let Err(error) = result {
                        self.failed.insert(id);
                        self.notice(format!("Audio read error: {error}"));
                    } else if segment.written == 0 && segment.start == 0.0 {
                        self.failed.insert(id);
                        self.notice("Skipped a track with no decodable audio.".into());
                    }
                    if self.segments.back().is_some_and(|s| s.written == 0) {
                        self.segments.pop_back();
                    }
                    self.source = None;
                    let mut next = self.next_id(id, true);
                    for _ in 0..self.queue.len() {
                        let Some(id) = next else {
                            break;
                        };
                        if self.open_source(id, 0.0) {
                            break;
                        }
                        next = self.next_id(id, false);
                    }
                    if self.source.is_none() {
                        if self.segments.iter().all(|s| s.written == 0) {
                            self.playing = false;
                            self.stopped = true;
                            self.shared.paused.store(true, Ordering::Release);
                            self.changed = true;
                        }
                        return;
                    }
                }
            }
        }
    }
    fn request_cover(&mut self) {
        if self.cover_id == self.current {
            return;
        }
        self.cover_id = self.current;
        self.cover = Some(None);
        self.cover_url = None;
        if let Some(index) = self.current_index() {
            let track = self.queue[index].clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let art = platform::read_cover(&track.path);
                let _ = tx.send(Command::Cover(track.id, art));
            });
        }
    }
    fn replan(&mut self) {
        self.update_clock();
        // Reopening the current track makes an audible dropout; only do it when a preloaded
        // successor may be stale. Otherwise the new queue/repeat/shuffle state is read at the next end.
        if self.successor_loaded()
            && let Some(id) = self.current
            && self.output.is_some()
        {
            self.start(id, self.position, self.playing);
        }
        self.config_dirty = true;
        self.changed = true;
    }
    fn command(&mut self, command: Command) {
        self.update_clock();
        match command {
            Command::Play => {
                if !self.playing {
                    if self.source.is_some() && !self.stopped {
                        self.playing = true;
                        self.shared.paused.store(false, Ordering::Release);
                    } else if let Some(id) =
                        self.current.or_else(|| self.queue.first().map(|t| t.id))
                    {
                        self.failed.remove(&id);
                        self.start(id, if self.stopped { 0.0 } else { self.position }, true);
                    }
                }
                self.transport_changed = true;
            }
            Command::Pause => {
                self.playing = false;
                self.resume_after_recovery = false;
                self.shared.paused.store(true, Ordering::Release);
                self.transport_changed = true;
            }
            Command::Toggle => {
                self.command(if self.playing {
                    Command::Pause
                } else {
                    Command::Play
                });
            }
            Command::Stop => {
                self.shared.paused.store(true, Ordering::Release);
                self.invalidate();
                self.playing = false;
                self.recovery = false;
                self.resume_after_recovery = false;
                self.position = 0.0;
                self.stopped = true;
                self.transport_changed = true;
            }
            Command::Select(index) => {
                if let Some(track) = self.queue.get(index) {
                    let id = track.id;
                    self.failed.remove(&id);
                    self.start(id, 0.0, true);
                }
            }
            Command::Next => {
                self.invalidate();
                if let Some(id) = self.current {
                    if let Some(next) = self.next_id(id, false) {
                        self.start(next, 0.0, true);
                    } else {
                        self.command(Command::Stop);
                    }
                }
            }
            Command::Previous => {
                if self.position > 3.0 {
                    if let Some(id) = self.current {
                        self.start(id, 0.0, self.playing);
                    }
                } else {
                    let previous = if self.shuffle {
                        self.history.pop().filter(|id| self.index(*id).is_some())
                    } else {
                        self.current_index().and_then(|i| {
                            if i > 0 {
                                Some(self.queue[i - 1].id)
                            } else if self.repeat == Repeat::All {
                                self.queue.last().map(|t| t.id)
                            } else {
                                None
                            }
                        })
                    };
                    if let Some(id) = previous {
                        let remaining_history = self.history.len();
                        self.start(id, 0.0, true);
                        if self.shuffle {
                            self.history.truncate(remaining_history);
                        }
                    } else if let Some(id) = self.current {
                        self.start(id, 0.0, self.playing);
                    }
                }
            }
            Command::Seek(seconds) => {
                if seconds.is_finite()
                    && let Some(index) = self.current_index()
                {
                    let duration = self.queue[index].duration;
                    self.start(
                        self.queue[index].id,
                        seconds
                            .max(0.0)
                            .min(if duration > 0.0 { duration } else { f64::MAX }),
                        self.playing,
                    );
                }
            }
            Command::SeekRelative(amount) => self.command(Command::Seek(self.position + amount)),
            Command::SeekFraction(fraction) => {
                if let Some(index) = self.current_index() {
                    self.command(Command::Seek(
                        self.queue[index].duration * fraction.clamp(0.0, 1.0) as f64,
                    ));
                }
            }
            Command::Volume(value) => {
                if value.is_finite() {
                    self.volume = value.clamp(0.0, 1.0);
                    self.shared.set_volume(self.volume);
                    // Moving the slider means "let me hear it"; staying silent looks broken.
                    if self.muted && self.volume > 0.0 {
                        self.muted = false;
                        self.shared.mute.store(false, Ordering::Relaxed);
                    }
                }
            }
            Command::VolumeRelative(amount) => self.command(Command::Volume(self.volume + amount)),
            Command::Mute => {
                self.muted = !self.muted;
                self.shared.mute.store(self.muted, Ordering::Relaxed);
            }
            Command::Shuffle => {
                if self.successor_loaded() {
                    self.invalidate();
                }
                self.shuffle = !self.shuffle;
                self.reset_bag();
                self.replan();
            }
            Command::Repeat => {
                self.repeat = self.repeat.next();
                self.replan();
            }
            Command::Import(tracks, autoplay) => {
                let first = self.queue.len();
                for mut track in tracks {
                    self.id += 1;
                    track.id = self.id;
                    // Random slot, not the tail: the tail is what shuffle plays next.
                    let slot = self.rand() as usize % (self.bag.len() + 1);
                    self.bag.insert(slot, track.id);
                    self.queue.push(track);
                }
                self.queue_dirty = true;
                if self.current.is_none() {
                    self.current = self.queue.first().map(|t| t.id);
                    self.request_cover();
                    self.reset_bag();
                }
                if autoplay && first < self.queue.len() {
                    self.command(Command::Select(first));
                } else if self.source.is_none() && self.playing {
                    self.replan();
                }
            }
            Command::Remove(index) => {
                if index < self.queue.len() {
                    let removed = self.queue.remove(index);
                    self.queue_dirty = true;
                    self.bag.retain(|id| *id != removed.id);
                    self.history.retain(|id| *id != removed.id);
                    if self.current == Some(removed.id) {
                        self.invalidate();
                        self.current = self
                            .queue
                            .get(index.min(self.queue.len().saturating_sub(1)))
                            .map(|t| t.id);
                        self.position = 0.0;
                        if let Some(id) = self.current {
                            self.start(id, 0.0, self.playing);
                        } else {
                            self.command(Command::Stop);
                            self.request_cover();
                        }
                    } else {
                        self.replan();
                    }
                }
            }
            Command::Move(from, to) => {
                if from < self.queue.len() && to < self.queue.len() && from != to {
                    let track = self.queue.remove(from);
                    self.queue.insert(to, track);
                    self.queue_dirty = true;
                    self.replan();
                }
            }
            Command::Clear => {
                self.command(Command::Stop);
                self.queue.clear();
                self.current = None;
                self.bag.clear();
                self.history.clear();
                self.failed.clear();
                self.lost.clear();
                self.queue_dirty = true;
                self.request_cover();
            }
            Command::Loading(loading) => {
                if loading {
                    self.loading += 1;
                } else {
                    self.loading = self.loading.saturating_sub(1);
                }
            }
            Command::Notice(message) => self.notice(message),
            Command::Cover(id, art) => {
                if self.current == Some(id) {
                    self.cover_url = art
                        .as_ref()
                        .and_then(|pixels| platform::cache_cover(&self.session_path, pixels));
                    self.cover = Some(art);
                }
            }
            Command::WindowSize(width, height) => {
                if !self.look.compact && width >= 400.0 && height >= 580.0 {
                    self.size = (width, height);
                }
            }
            Command::Look(look) => self.look = look,
            Command::Shutdown => {}
        }
        self.changed = true;
        self.config_dirty = true;
    }
    fn recover(&mut self) {
        let resume = if self.recovery {
            self.resume_after_recovery
        } else {
            self.playing
        };
        self.update_clock();
        self.shared.paused.store(true, Ordering::Release);
        self.invalidate();
        self.output = None;
        self.resume_after_recovery = resume;
        if self.ensure_output() {
            if let Some(id) = self.current {
                self.start(id, self.position, resume);
            }
            self.notice("Audio output reconnected.".into());
            self.notice_info = true;
        } else {
            self.playing = false;
            self.transport_changed = true;
        }
    }
    fn save(&mut self) {
        let session = Session {
            queue: self
                .queue
                .iter()
                .map(|t| t.path.clone())
                .chain(self.lost.iter().cloned())
                .collect(),
            current: self.current_index(),
            position: self.position,
            volume: self.volume,
            muted: self.muted,
            shuffle: self.shuffle,
            repeat: self.repeat,
            width: self.size.0,
            height: self.size.1,
            dark_mode: self.look.dark,
            compact: self.look.compact,
            pinned: self.look.pinned,
            auto_float: self.look.auto_float,
        };
        match platform::save(&self.session_path, &session) {
            Ok(()) => self.config_dirty = false,
            Err(e) => self.notice(e),
        }
    }
    fn publish(&mut self, publish: &mut impl FnMut(State)) {
        publish(State {
            rows: if self.queue_dirty {
                Some(self.queue.clone())
            } else {
                None
            },
            current: self.current_index(),
            track: self.current_index().map(|i| self.queue[i].clone()),
            playing: self.playing,
            stopped: self.stopped,
            position: self.position,
            volume: self.volume,
            muted: self.muted,
            shuffle: self.shuffle,
            repeat: self.repeat,
            notice: self.notice.clone(),
            notice_info: self.notice_info,
            loading: self.loading > 0,
            cover: self.cover.take(),
            cover_url: self.cover_url.clone(),
            transport_changed: self.transport_changed,
            device_rate: self.output.as_ref().map_or(0, |o| o.rate),
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            epoch: self.epoch,
        });
        self.queue_dirty = false;
        self.changed = false;
        self.transport_changed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn until(rx: &mpsc::Receiver<State>, predicate: impl Fn(&State) -> bool) -> State {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("engine state deadline");
            if predicate(&state) {
                return state;
            }
        }
    }
    fn spawn_test_engine(
        dir: &std::path::Path,
        session: Session,
    ) -> (
        Sender<Command>,
        mpsc::Receiver<State>,
        std::thread::JoinHandle<()>,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let (states, rx) = mpsc::channel();
        let (tx, handle) = spawn(
            session,
            dir.join("session.toml"),
            String::new(),
            crate::audio::testing::Backend,
            move |state| {
                let _ = states.send(state);
            },
        );
        (tx, rx, handle)
    }
    #[test]
    fn unreadable_session_files_survive_a_save() {
        let dir = std::env::temp_dir().join(format!("maki-lost-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (gone, here) = (dir.join("unplugged.wav"), dir.join("here.wav"));
        std::fs::write(&here, crate::decoder::tests::wav(48000, 4800)).unwrap();
        let session = Session {
            queue: vec![gone.clone(), here.clone()],
            current: Some(1),
            ..Session::default()
        };
        let (tx, rx, handle) = spawn_test_engine(&dir, session);
        let first = until(&rx, |_| true);
        assert_eq!(first.rows.as_ref().map(|r| r.len()), Some(1));
        tx.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
        let saved = platform::load(&dir.join("session.toml")).unwrap();
        assert_eq!(saved.queue, vec![here, gone]);
        assert_eq!(saved.current, Some(0));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn repeat_and_shuffle_do_not_restart_the_playing_track() {
        let dir = std::env::temp_dir().join(format!("maki-norestart-{}", std::process::id()));
        let (tx, rx, handle) = spawn_test_engine(&dir, Session::default());
        let flac = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stereo-44100.flac");
        tx.send(Command::Import(
            vec![platform::read_track(flac).unwrap()],
            true,
        ))
        .unwrap();
        let playing = until(&rx, |s| s.playing && s.position > 0.1);
        tx.send(Command::Repeat).unwrap();
        tx.send(Command::Shuffle).unwrap();
        let after = until(&rx, |s| s.repeat == Repeat::All && s.shuffle);
        assert_eq!(after.epoch, playing.epoch, "playback was reopened");
        tx.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn moving_volume_while_muted_unmutes() {
        let dir = std::env::temp_dir().join(format!("maki-mute-{}", std::process::id()));
        let (tx, rx, handle) = spawn_test_engine(&dir, Session::default());
        tx.send(Command::Mute).unwrap();
        until(&rx, |s| s.muted);
        tx.send(Command::Volume(0.5)).unwrap();
        until(&rx, |s| !s.muted && (s.volume - 0.5).abs() < 0.001);
        tx.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn seeking_to_the_end_advances_instead_of_failing_the_track() {
        let dir = std::env::temp_dir().join(format!("maki-seekend-{}", std::process::id()));
        let (tx, rx, handle) = spawn_test_engine(&dir, Session::default());
        let flac = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stereo-44100.flac");
        let wav = dir.join("next.wav");
        std::fs::write(&wav, crate::decoder::tests::wav(48000, 48000)).unwrap();
        tx.send(Command::Import(
            vec![
                platform::read_track(flac).unwrap(),
                platform::read_track(wav).unwrap(),
            ],
            true,
        ))
        .unwrap();
        until(&rx, |s| {
            s.playing && s.current == Some(0) && s.position > 0.1
        });
        tx.send(Command::SeekFraction(1.0)).unwrap();
        let next = until(&rx, |s| {
            s.current == Some(1) && s.playing && s.position > 0.05
        });
        assert_eq!(next.notice, "", "end-seek was reported as a failure");
        tx.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn queue_transport_gapless_and_session_work_without_an_audio_device() {
        let dir = std::env::temp_dir().join(format!("maki-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.wav");
        let second = dir.join("second.wav");
        std::fs::write(&first, crate::decoder::tests::wav(44100, 44100)).unwrap();
        std::fs::write(&second, crate::decoder::tests::wav(48000, 48000)).unwrap();
        let (states, rx) = mpsc::channel();
        let (tx, handle) = spawn(
            Session::default(),
            dir.join("session.toml"),
            String::new(),
            crate::audio::testing::Backend,
            move |state| {
                let _ = states.send(state);
            },
        );
        until(&rx, |_| true);
        tx.send(Command::Import(
            vec![
                platform::read_track(first.clone()).unwrap(),
                platform::read_track(second.clone()).unwrap(),
            ],
            true,
        ))
        .unwrap();
        let first_state = until(&rx, |s| s.playing && s.position > 0.1);
        assert_eq!(first_state.device_rate, 48000);
        let next = until(&rx, |s| s.current == Some(1) && s.position > 0.1);
        assert_eq!(next.underruns, 0, "inserted a gap between tracks");
        tx.send(Command::Pause).unwrap();
        let paused = until(&rx, |s| !s.playing && !s.stopped);
        std::thread::sleep(Duration::from_millis(150));
        tx.send(Command::Volume(0.3)).unwrap();
        let quiet = until(&rx, |s| (s.volume - 0.3).abs() < 0.001);
        assert!(
            (quiet.position - paused.position).abs() <= 0.011,
            "paused clock advanced"
        );
        tx.send(Command::Seek(0.25)).unwrap();
        let seeked = until(&rx, |s| !s.playing && (s.position - 0.25).abs() < 0.001);
        assert!(!seeked.stopped);
        tx.send(Command::Move(1, 0)).unwrap();
        let moved = until(&rx, |s| s.rows.is_some() && s.current == Some(0));
        assert_eq!(moved.rows.unwrap()[0].path, second);
        tx.send(Command::Repeat).unwrap();
        tx.send(Command::Shuffle).unwrap();
        tx.send(Command::Mute).unwrap();
        until(&rx, |s| s.repeat == Repeat::All && s.shuffle && s.muted);
        tx.send(Command::Remove(1)).unwrap();
        until(&rx, |s| s.rows.as_ref().is_some_and(|r| r.len() == 1));
        tx.send(Command::Play).unwrap();
        until(&rx, |s| s.playing && s.position > 0.85);
        until(&rx, |s| s.playing && s.position < 0.15);
        tx.send(Command::Repeat).unwrap();
        until(&rx, |s| s.repeat == Repeat::One);
        until(&rx, |s| s.playing && s.position > 0.85);
        until(&rx, |s| s.playing && s.position < 0.15);
        tx.send(Command::Pause).unwrap();
        until(&rx, |s| !s.playing);
        tx.send(Command::Seek(0.25)).unwrap();
        until(&rx, |s| (s.position - 0.25).abs() < 0.001);
        tx.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
        let session = platform::load(&dir.join("session.toml")).unwrap();
        assert_eq!(session.queue, vec![second]);
        assert_eq!(session.current, Some(0));
        assert!(session.muted && session.shuffle);
        assert_eq!(session.repeat, Repeat::One);
        assert!((session.position - 0.25).abs() < 0.011);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
