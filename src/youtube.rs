//! YouTube links: yt-dlp finds the audio, curl streams it. Both must be on PATH.
use crate::engine::{self, Track};
use std::{
    io::{self, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Mutex, MutexGuard, mpsc::Sender},
    time::{SystemTime, UNIX_EPOCH},
};
use symphonia::core::io::MediaSource;

/// Stream entries keep their page URL in `Track::path`.
pub fn is_stream(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|s| s.starts_with("https://") || s.starts_with("http://"))
}

pub fn is_link(text: &str) -> bool {
    text.starts_with("https://") && (text.contains("youtube.com/") || text.contains("youtu.be/"))
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: without it the GUI app flashes a console for each call.
        command.creation_flags(0x0800_0000);
    }
    command
}

/// The engine thread waits on these calls, so a dead connection must fail, not hang.
fn curl(range: &str, url: &str) -> Command {
    command(
        "curl",
        &[
            "-sSfL",
            "--connect-timeout",
            "10",
            "--speed-limit",
            "1",
            "--speed-time",
            "20",
            "-r",
            range,
            url,
        ],
    )
}

fn yt_dlp(args: &[&str]) -> Command {
    let mut command = command("yt-dlp", &["--no-warnings", "--socket-timeout", "15"]);
    command.args(args);
    command
}

fn missing(program: &str, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        format!("{program} is not installed or not on PATH")
    } else {
        format!("{program}: {error}")
    }
}

fn run(mut command: Command) -> Result<Vec<u8>, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|e| missing(&program, e))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(error
            .lines()
            .rev()
            .find(|l| l.starts_with("ERROR:") || l.starts_with("curl:"))
            .unwrap_or("failed")
            .trim_start_matches("ERROR: ")
            .to_string());
    }
    Ok(output.stdout)
}

/// One track for a video, one per entry for a playlist page.
pub fn list(link: &str) -> Result<Vec<Track>, String> {
    // --no-playlist: a video opened from a Mix (watch?v=..&list=RD..) adds only that video.
    let output = run(yt_dlp(&[
        "--flat-playlist",
        "--no-playlist",
        "--encoding",
        "utf-8",
        "--print",
        "%(id)s\t%(title)s\t%(channel,uploader|)s\t%(duration|0)s",
        link,
    ]))?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (id, title, artist, duration) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            Some(Track {
                id: 0,
                path: PathBuf::from(format!("https://www.youtube.com/watch?v={id}")),
                title: title.into(),
                artist: artist.into(),
                album: "YouTube".into(),
                duration: duration.parse().unwrap_or(0.0),
            })
        })
        .collect())
}

fn query(url: &str, key: &str) -> Option<u64> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)?
        .1
        .parse()
        .ok()
}

/// What a stream needs before it can play: the audio URL, its header and fragment table.
#[derive(Clone)]
struct Audio {
    url: String,
    /// The file up to the first `moof`.
    head: Vec<u8>,
    fragments: Vec<(u64, f64)>,
}

/// Page URL -> prepared audio. Seeks reopen the stream, so they reuse it with no network call.
static CACHE: Mutex<Vec<(String, Audio)>> = Mutex::new(Vec::new());
/// Pages that a background thread prepares now.
static BUSY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The cache without expired URLs.
fn cache() -> MutexGuard<'static, Vec<(String, Audio)>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(_, audio)| query(&audio.url, "expire").unwrap_or(0) > now + 60);
    cache
}

/// Network calls (~2 s): keep them off the engine thread through `prepare`.
/// Also replaces a cached URL, which can stop working early, for example after a network change.
fn fetch(page: &str) -> Result<Audio, String> {
    // 140 is YouTube's AAC/M4A audio; Symphonia has no Opus decoder for the other formats.
    let output = run(yt_dlp(&["-f", "140", "-g", "--no-playlist", page]))?;
    let url = String::from_utf8_lossy(&output).trim().to_string();
    let mut head = run(curl("0-65535", &url))?;
    let (moof, fragments) = index(&head).ok_or("Unsupported YouTube audio layout")?;
    head.truncate(moof);
    let audio = Audio {
        url,
        head,
        fragments,
    };
    let mut cache = cache();
    cache.retain(|(p, _)| p != page);
    cache.push((page.into(), audio.clone()));
    Ok(audio)
}

pub fn ready(page: &Path) -> bool {
    let page = page.to_string_lossy();
    cache().iter().any(|(p, _)| *p == page)
}

/// Fetch in the background, then report `Command::Prepared`. One thread per page at a time.
pub fn prepare(page: PathBuf, tx: Sender<engine::Command>) {
    let key = page.to_string_lossy().into_owned();
    {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if busy.contains(&key) {
            return;
        }
        busy.push(key.clone());
    }
    std::thread::spawn(move || {
        let result = fetch(&key).map(|_| ());
        BUSY.lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|p| *p != key);
        let _ = tx.send(engine::Command::Prepared(page, result));
    });
}

/// Offset of the first `moof`, and (byte offset, start seconds) of each fragment from `sidx`.
fn index(head: &[u8]) -> Option<(usize, Vec<(u64, f64)>)> {
    let be = |at: usize, n: usize| {
        head.get(at..at + n)
            .map(|b| b.iter().fold(0u64, |v, b| v << 8 | *b as u64))
    };
    let mut fragments = Vec::new();
    let mut at = 0;
    loop {
        let size = be(at, 4)? as usize;
        match head.get(at + 4..at + 8)? {
            b"sidx" => {
                let wide = if *head.get(at + 8)? == 0 { 4 } else { 8 };
                let timescale = be(at + 16, 4)?.max(1) as f64;
                let mut offset = (at + size) as u64 + be(at + 20 + wide, wide)?;
                let mut time = 0;
                let count = at + 22 + 2 * wide;
                for entry in (0..be(count, 2)? as usize).map(|i| count + 2 + i * 12) {
                    fragments.push((offset, time as f64 / timescale));
                    offset += be(entry, 4)? & 0x7fff_ffff;
                    time += be(entry + 4, 4)?;
                }
            }
            b"moof" => return (!fragments.is_empty()).then_some((at, fragments)),
            _ => {}
        }
        if size < 8 {
            return None;
        }
        at += size;
    }
}

/// The file header followed by the fragments from `position` on, read forward only.
pub struct Stream {
    head: Cursor<Vec<u8>>,
    page: String,
    url: String,
    /// Next file byte to read; a dropped connection resumes here.
    at: u64,
    end: Option<u64>,
    curl: Option<(Child, ChildStdout)>,
    failures: u32,
}

impl Stream {
    /// Also returns the start time of the first fragment, where decoded timestamps restart at 0.
    pub fn open(page: &Path, position: f64) -> Result<(Self, f64), String> {
        let page = page.to_string_lossy().into_owned();
        // Bound first: fetch() takes the cache lock too.
        let cached = cache()
            .iter()
            .find(|(p, _)| *p == page)
            .map(|(_, a)| a.clone());
        let Audio {
            url,
            head,
            fragments,
        } = match cached {
            Some(audio) => audio,
            None => fetch(&page)?,
        };
        let (at, start) = *fragments
            .iter()
            .rev()
            .find(|(_, start)| *start <= position)
            .unwrap_or(&fragments[0]);
        Ok((
            Self {
                head: Cursor::new(head),
                end: query(&url, "clen"),
                page,
                url,
                at,
                curl: None,
                failures: 0,
            },
            start,
        ))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.head.read(buf)?;
        if n > 0 || buf.is_empty() {
            return Ok(n);
        }
        loop {
            if self.curl.is_none() {
                let mut child = curl(&format!("{}-", self.at), &self.url)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
                self.curl = Some((child, stdout));
            }
            let (child, stdout) = self.curl.as_mut().unwrap();
            let read = stdout.read(buf);
            if let Ok(n) = read
                && n > 0
            {
                self.at += n as u64;
                self.failures = 0;
                return Ok(n);
            }
            // Wait without kill at EOF: killing an exiting curl would turn success into failure.
            if read.is_err() {
                let _ = child.kill();
            }
            let clean = child.wait().is_ok_and(|s| s.success());
            self.curl = None;
            if read.is_ok() && clean && self.end.is_none_or(|end| self.at >= end) {
                return Ok(0);
            }
            self.failures += 1;
            match self.failures {
                1 => {}
                2 => {
                    self.url = fetch(&self.page).map_err(io::Error::other)?.url;
                }
                _ => return Err(io::Error::other("the YouTube stream stopped")),
            }
        }
    }
}

impl Seek for Stream {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for Stream {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some((mut child, _)) = self.curl.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_maps_sidx_fragments_to_offsets_and_times() {
        let mut head = vec![0, 0, 0, 8, b'f', b't', b'y', b'p'];
        // sidx v0: timescale 1000, two fragments of 100/200 bytes and 2.5/3 s.
        let sidx: Vec<u8> = [
            &[0, 0, 0, 56][..],
            b"sidx",
            &[
                0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 3, 232, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
            ],
            &[0, 0, 0, 100, 0, 0, 9, 196, 0, 0, 0, 0],
            &[0, 0, 0, 200, 0, 0, 11, 184, 0, 0, 0, 0],
        ]
        .concat();
        head.extend(&sidx);
        head.extend([0, 0, 0, 8, b'm', b'o', b'o', b'f']);
        let (moof, fragments) = index(&head).unwrap();
        assert_eq!(moof, 64);
        assert_eq!(fragments, vec![(64, 0.0), (164, 2.5)]);
    }
}
