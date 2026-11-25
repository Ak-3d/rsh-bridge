//! Small terminal helpers: single-key input, safe display of untrusted
//! text, and clock formatting.

use std::cell::UnsafeCell;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

struct Saved(UnsafeCell<MaybeUninit<libc::termios>>);
// Written only by the one thread that prompts, before RAW is set; read by
// the signal handler only while RAW is set.
unsafe impl Sync for Saved {}

static ORIG: Saved = Saved(UnsafeCell::new(MaybeUninit::uninit()));
static RAW: AtomicBool = AtomicBool::new(false);
static CLEANUP: OnceLock<CString> = OnceLock::new();

pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

pub fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

extern "C" fn on_signal(sig: libc::c_int) {
    // Only async-signal-safe calls here.
    unsafe {
        if RAW.load(Ordering::SeqCst) {
            libc::tcsetattr(0, libc::TCSANOW, (*ORIG.0.get()).as_ptr());
        }
        if let Some(p) = CLEANUP.get() {
            libc::unlink(p.as_ptr());
        }
        libc::_exit(128 + sig);
    }
}

/// On Ctrl-C or kill: put the terminal back and remove `socket`.
pub fn restore_on_exit(socket: Option<&Path>) {
    if let Some(p) = socket
        && let Ok(c) = CString::new(p.as_os_str().as_bytes()) {
            let _ = CLEANUP.set(c);
        }
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

pub enum Key {
    Byte(u8),
    /// The other end of `conn` went away while we were waiting.
    Hangup,
}

/// Wait for one keypress, without needing Enter, or for `conn` to close.
/// Anything typed before the call is thrown away so a stray key can't
/// answer a question that wasn't on screen yet.
pub fn read_key(conn: RawFd) -> Key {
    unsafe {
        let mut orig = MaybeUninit::<libc::termios>::uninit();
        let have_tty = libc::tcgetattr(0, orig.as_mut_ptr()) == 0;
        if have_tty {
            *ORIG.0.get() = orig;
            let mut raw = orig.assume_init();
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            RAW.store(true, Ordering::SeqCst);
            libc::tcflush(0, libc::TCIFLUSH);
        }
        let key = loop {
            let mut fds = [
                libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: conn, events: libc::POLLIN, revents: 0 },
            ];
            if libc::poll(fds.as_mut_ptr(), 2, -1) < 0 {
                continue;
            }
            // The requester sends nothing after its question, so anything
            // readable on `conn` means it closed.
            if fds[1].revents != 0 {
                break Key::Hangup;
            }
            if fds[0].revents != 0 {
                let mut b = 0u8;
                match libc::read(0, (&raw mut b).cast(), 1) {
                    1 => break Key::Byte(b),
                    0 => break Key::Hangup,
                    _ => continue,
                }
            }
        };
        if have_tty {
            libc::tcsetattr(0, libc::TCSANOW, orig.as_ptr());
            RAW.store(false, Ordering::SeqCst);
        }
        key
    }
}

/// Run `f` (which starts an interactive shell) and make sure we own the
/// terminal again afterwards. An interactive shell takes the terminal for
/// itself and does not always hand it back.
pub fn keep_foreground<T>(f: impl FnOnce() -> T) -> T {
    unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
    let result = f();
    unsafe {
        if libc::isatty(0) == 1 {
            libc::tcsetpgrp(0, libc::getpgrp());
        }
        libc::signal(libc::SIGTTOU, libc::SIG_DFL);
    }
    result
}

/// Make untrusted text safe to print: control characters and invisible or
/// text-direction characters are shown as escapes, so neither server output
/// nor an agent's command can move the cursor, recolour the screen or hide
/// part of itself.
pub fn visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let hidden = matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
        if c == '\t' {
            out.push_str("    ");
        } else if c.is_control() || hidden {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Local wall-clock time as HH:MM:SS.
pub fn clock(ts: u64) -> String {
    let t = ts as libc::time_t;
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    let tm = unsafe {
        libc::localtime_r(&t, tm.as_mut_ptr());
        tm.assume_init()
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::visible;

    #[test]
    fn untrusted_text_cannot_control_the_terminal() {
        assert_eq!(visible("plain ünïcode ✓"), "plain ünïcode ✓");
        assert_eq!(visible("a\x1b[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(visible("safe\rhidden"), "safe\\u{d}hidden");
        assert_eq!(visible("x\u{202e}y"), "x\\u{202e}y");
        assert_eq!(visible("a\tb"), "a    b");
    }
}
