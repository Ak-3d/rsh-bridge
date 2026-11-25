//! Asking the user before a command runs.
//!
//! The `watch` window listens on a socket. An agent session connects, sends
//! one JSON line describing the command, and waits for one JSON line back.

use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::paths;
use crate::run::Cancel;

#[derive(Serialize, Deserialize)]
pub struct Ask {
    pub id: String,
    pub label: String,
    pub host: String,
    pub cwd: Option<String>,
    pub cmd: String,
}

#[derive(Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
}

pub enum Verdict {
    Approved,
    Rejected,
    /// No watch window is answering, or it closed before answering.
    NoWatcher,
    TimedOut,
    Cancelled,
}

pub fn ask(request: &Ask, cancel: &Cancel, limit: Duration) -> Verdict {
    let Ok(mut conn) = UnixStream::connect(paths::approve_sock()) else {
        return Verdict::NoWatcher;
    };
    let Ok(mut line) = serde_json::to_string(request) else { return Verdict::NoWatcher };
    line.push('\n');
    if conn.write_all(line.as_bytes()).is_err() {
        return Verdict::NoWatcher;
    }
    // Wake up regularly so a cancellation or the time limit is noticed.
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let deadline = Instant::now() + limit;
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        if cancel.flag.load(Ordering::SeqCst) {
            return Verdict::Cancelled;
        }
        if Instant::now() >= deadline {
            return Verdict::TimedOut;
        }
        match conn.read(&mut buf) {
            Ok(0) => return Verdict::NoWatcher,
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if reply.contains(&b'\n') {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(_) => return Verdict::NoWatcher,
        }
    }
    match serde_json::from_slice::<Answer>(&reply) {
        Ok(Answer { ok: true }) => Verdict::Approved,
        Ok(Answer { ok: false }) => Verdict::Rejected,
        // Anything unreadable counts as "no".
        Err(_) => Verdict::Rejected,
    }
}
