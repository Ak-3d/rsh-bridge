//! Run one command over a shared connection and collect what it prints.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ssh;
use crate::transcript::{Event, Transcript};

/// Bytes of each stream kept for `read_output`.
pub const KEEP: usize = 4 * 1024 * 1024;
/// Bytes of each stream returned directly by `run`.
pub const INLINE: usize = 24_000;
/// Bytes of output per command copied into the transcript.
const LOG_BUDGET: i64 = 256 * 1024;
/// How often a waiting command checks whether it was cancelled.
const TICK: Duration = Duration::from_millis(50);
/// After a timeout or cancellation, how long to keep reading output that
/// is already on its way.
const DRAIN: Duration = Duration::from_millis(150);

/// Lets another thread stop a command: a client cancellation, or shutdown.
#[derive(Default)]
pub struct Cancel {
    pub flag: AtomicBool,
    pid: Mutex<Option<u32>>,
}

impl Cancel {
    pub fn fire(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // The pid is cleared before the process is reaped, so this can
        // never signal an unrelated process that reused the number.
        if let Some(pid) = *self.pid.lock().unwrap() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
    }
}

/// One output stream: everything up to `KEEP` bytes, plus the very end of
/// anything longer.
#[derive(Default)]
pub struct Captured {
    pub head: Vec<u8>,
    tail: Vec<u8>,
    pub total: u64,
}

impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        let take = (KEEP - self.head.len()).min(chunk.len());
        self.head.extend_from_slice(&chunk[..take]);
        let rest = &chunk[take..];
        if !rest.is_empty() {
            self.tail.extend_from_slice(rest);
            if self.tail.len() > INLINE {
                let cut = self.tail.len() - INLINE;
                self.tail.drain(..cut);
            }
        }
    }

    /// The text to return straight away: all of it when short, otherwise
    /// the beginning and the end.
    pub fn inline(&self) -> String {
        if self.total as usize <= INLINE {
            return String::from_utf8_lossy(&self.head).into_owned();
        }
        let half = INLINE / 2;
        let mut end: Vec<u8> = Vec::with_capacity(half);
        if self.tail.len() >= half {
            end.extend_from_slice(&self.tail[self.tail.len() - half..]);
        } else {
            let from_head = half - self.tail.len();
            end.extend_from_slice(&self.head[self.head.len() - from_head..]);
            end.extend_from_slice(&self.tail);
        }
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            String::from_utf8_lossy(&self.head[..half]),
            self.total as usize - 2 * half,
            String::from_utf8_lossy(&end)
        )
    }
}

pub struct Done {
    /// The remote command's exit code. `None` if the local ssh was killed.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub ms: u64,
    pub out: Captured,
    pub err: Captured,
}

/// One of the command's two output streams while it is being read.
struct Stream {
    file: Option<std::fs::File>,
    name: &'static str,
    cap: Captured,
    /// Bytes not yet copied to the transcript (an unfinished line).
    pending: Vec<u8>,
}

/// Copies output into the transcript, up to a per-command limit.
struct Logger<'a> {
    id: &'a str,
    log: &'a Transcript,
    budget: i64,
}

impl Logger<'_> {
    fn emit(&mut self, stream: &str, bytes: &[u8]) {
        if self.budget <= 0 || bytes.is_empty() {
            return;
        }
        self.budget -= bytes.len() as i64;
        self.log.write(&Event::Out {
            id: self.id.to_string(),
            s: stream.to_string(),
            d: String::from_utf8_lossy(bytes).into_owned(),
        });
        if self.budget <= 0 {
            self.log.write(&Event::Out {
                id: self.id.to_string(),
                s: "x".into(),
                d: "transcript limit reached; the rest of this output is not shown here".into(),
            });
        }
    }
}

impl Stream {
    fn new(file: impl Into<OwnedFd>, name: &'static str) -> Self {
        Stream { file: Some(std::fs::File::from(file.into())), name, cap: Captured::default(), pending: Vec::new() }
    }

    /// Read what is available. The transcript gets whole lines where
    /// possible, so lines from the two streams don't cut into each other.
    fn read(&mut self, buf: &mut [u8], logger: &mut Logger) {
        let Some(file) = self.file.as_mut() else { return };
        match file.read(buf) {
            Ok(0) => self.finish(logger),
            Ok(n) => {
                self.cap.push(&buf[..n]);
                self.pending.extend_from_slice(&buf[..n]);
                let cut = match self.pending.iter().rposition(|&b| b == b'\n') {
                    Some(i) => i + 1,
                    None if self.pending.len() >= 8 * 1024 => self.pending.len(),
                    None => 0,
                };
                if cut > 0 {
                    logger.emit(self.name, &self.pending[..cut]);
                    self.pending.drain(..cut);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.finish(logger),
        }
    }

    fn finish(&mut self, logger: &mut Logger) {
        logger.emit(self.name, &self.pending);
        self.pending.clear();
        self.file = None;
    }
}

/// Send `command` to `host` and wait for it. The caller has already checked
/// the host and obtained approval.
pub fn execute(
    host: &str,
    cwd: Option<&str>,
    command: &str,
    timeout: Duration,
    id: &str,
    log: &Transcript,
    cancel: &Cancel,
) -> io::Result<Done> {
    let started = Instant::now();
    // `--` before the host stops ssh from reading anything after it as an
    // option, so a command that starts with `-` stays a command.
    let mut child = ssh::client(host)
        .arg("-T")
        .arg("--")
        .arg(host)
        .arg(ssh::remote_script(cwd, command))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut streams = [
        Stream::new(child.stdout.take().expect("piped"), "o"),
        Stream::new(child.stderr.take().expect("piped"), "e"),
    ];
    let pid = child.id() as libc::pid_t;
    *cancel.pid.lock().unwrap() = Some(child.id());

    let mut logger = Logger { id, log, budget: LOG_BUDGET };
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = started + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    // Set once we give up. The shared connection keeps its own handles on
    // our pipes until the remote command ends, so after stopping the local
    // ssh we can't wait for the pipes to close: read what is already on its
    // way for a moment, then return.
    let mut stop_at: Option<Instant> = None;

    while streams.iter().any(|s| s.file.is_some()) {
        let now = Instant::now();
        if stop_at.is_none() {
            cancelled = cancel.flag.load(Ordering::SeqCst);
            timed_out = !cancelled && now >= deadline;
            if cancelled || timed_out {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                stop_at = Some(now + DRAIN);
            }
        }
        let until = stop_at.unwrap_or(deadline);
        if stop_at.is_some() && now >= until {
            break;
        }
        // Wake up at least every TICK to notice a cancellation.
        let wait = until.saturating_duration_since(now).min(TICK).as_millis().max(1) as libc::c_int;
        let mut fds = [libc::pollfd { fd: -1, events: libc::POLLIN, revents: 0 }; 2];
        for (slot, stream) in fds.iter_mut().zip(&streams) {
            if let Some(f) = &stream.file {
                slot.fd = f.as_raw_fd();
            }
        }
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) } <= 0 {
            continue;
        }
        for (slot, stream) in fds.iter().zip(&mut streams) {
            if slot.revents != 0 {
                stream.read(&mut buf, &mut logger);
            }
        }
    }
    for stream in &mut streams {
        stream.finish(&mut logger);
    }

    *cancel.pid.lock().unwrap() = None;
    let status = child.wait()?;
    let [out, err] = streams;
    Ok(Done {
        code: status.code(),
        timed_out,
        cancelled,
        ms: started.elapsed().as_millis() as u64,
        out: out.cap,
        err: err.cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_returned_whole() {
        let mut c = Captured::default();
        c.push(b"hello\n");
        assert_eq!(c.inline(), "hello\n");
        assert_eq!(c.total, 6);
    }

    #[test]
    fn long_output_keeps_both_ends() {
        let mut c = Captured::default();
        c.push(b"START");
        c.push(&vec![b'.'; INLINE * 2]);
        c.push(b"END");
        let text = c.inline();
        assert!(text.starts_with("START"));
        assert!(text.ends_with("END"));
        assert!(text.contains(&format!("[... {} bytes omitted ...]", INLINE * 2 + 8 - INLINE)));
    }

    #[test]
    fn huge_output_is_capped_but_keeps_its_end() {
        let mut c = Captured::default();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..6 {
            c.push(&chunk);
        }
        c.push(b"THE END");
        assert_eq!(c.head.len(), KEEP);
        assert_eq!(c.total, 6 * 1024 * 1024 + 7);
        assert!(c.inline().ends_with("THE END"));
    }
}
