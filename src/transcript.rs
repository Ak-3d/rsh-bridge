//! The transcript: one JSON line per event, appended by every agent session
//! and followed by `rsh-bridge watch`.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::paths;

/// Start a fresh file once the old one passes this size.
const ROTATE_AT: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    /// A command was sent to the server.
    Start { id: String, ts: u64, label: String, host: String, cwd: Option<String>, cmd: String },
    /// Output from a running command. `s` is "o" (stdout), "e" (stderr) or
    /// "x" (a note from rsh-bridge itself).
    Out { id: String, s: String, d: String },
    /// A command finished.
    End { id: String, ts: u64, code: Option<i32>, ms: u64, note: Option<String> },
    /// A command was requested but never sent.
    Denied { id: String, ts: u64, label: String, host: String, cwd: Option<String>, cmd: String, why: String },
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct Transcript {
    file: Mutex<File>,
}

impl Transcript {
    pub fn open() -> Result<Self, String> {
        paths::private_dir(&paths::state_dir())?;
        let path = paths::transcript();
        if fs::metadata(&path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
            let _ = fs::rename(&path, path.with_extension("jsonl.1"));
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Transcript { file: Mutex::new(file) })
    }

    /// Append one event. Each event is a single write, so lines from
    /// different sessions never mix. Logging must not break a command, so
    /// write errors are dropped.
    pub fn write(&self, event: &Event) {
        let Ok(mut line) = serde_json::to_string(event) else { return };
        line.push('\n');
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Reads events as they are appended to the transcript.
pub struct Follower {
    path: PathBuf,
    file: File,
    pos: u64,
    carry: Vec<u8>,
    checked: Instant,
}

impl Follower {
    /// Start at the beginning of the transcript, or at its current end.
    pub fn open(from_start: bool) -> Result<Self, String> {
        paths::private_dir(&paths::state_dir())?;
        let path = paths::transcript();
        let mut file = Self::open_file(&path)?;
        let pos = if from_start { 0 } else { file.seek(SeekFrom::End(0)).unwrap_or(0) };
        Ok(Follower { path, file, pos, carry: Vec::new(), checked: Instant::now() })
    }

    fn open_file(path: &PathBuf) -> Result<File, String> {
        let fail = |e: std::io::Error| format!("{}: {e}", path.display());
        OpenOptions::new().append(true).create(true).mode(0o600).open(path).map_err(fail)?;
        File::open(path).map_err(fail)
    }

    /// Events appended since the last call. Returns at once when there
    /// are none.
    pub fn read_new(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match self.file.read(&mut buf) {
                Ok(n) if n > 0 => {
                    self.pos += n as u64;
                    self.carry.extend_from_slice(&buf[..n]);
                    while let Some(i) = self.carry.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = self.carry.drain(..=i).collect();
                        if let Ok(event) = serde_json::from_slice::<Event>(&line) {
                            events.push(event);
                        }
                    }
                }
                _ => break,
            }
        }
        // Now and then, notice a transcript that was rotated or deleted
        // and move to the new one.
        if events.is_empty() && self.checked.elapsed() >= Duration::from_secs(1) {
            self.checked = Instant::now();
            let replaced = match (fs::metadata(&self.path), self.file.metadata()) {
                (Ok(now), Ok(mine)) => now.ino() != mine.ino() || now.len() < self.pos,
                (Err(_), _) => true,
                _ => false,
            };
            if replaced && let Ok(file) = Self::open_file(&self.path) {
                self.file = file;
                self.pos = 0;
                self.carry.clear();
            }
        }
        events
    }
}
