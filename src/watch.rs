//! `rsh-bridge watch`: a live view of what agents run, and the place where
//! the user approves or rejects commands.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::approval::{Answer, Ask};
use crate::term::{self, Key};
use crate::transcript::{Event, Follower};
use crate::{paths, ssh};

struct Meta {
    host: String,
    tag: String,
}

/// Everything printed goes through this, so the two threads (transcript
/// follower and approval prompt) never write over each other.
struct Screen {
    color: bool,
    runs: HashMap<String, Meta>,
    /// The command whose output was printed last.
    last: Option<String>,
    /// A one-line reminder while an approval is waiting for a key.
    waiting: Option<String>,
}

type Ui = Arc<Mutex<Screen>>;

/// "tmp·a3f2c1 #4" from a label and an id like "a3f2c1-4".
fn tag(label: &str, id: &str) -> String {
    let n = id.rsplit('-').next().unwrap_or(id);
    format!("{} #{}", term::visible(label), term::visible(n))
}

fn seconds(ms: u64) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

impl Screen {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    fn command(&self, gutter: &str, cmd: &str) {
        for (i, line) in cmd.split('\n').enumerate() {
            let lead = if i == 0 { "$" } else { " " };
            println!("{gutter}{} {}", self.paint("1", lead), term::visible(line));
        }
    }

    /// When output switches to a different command, say which one.
    fn switch_to(&mut self, id: &str) {
        if self.last.as_deref() == Some(id) {
            return;
        }
        let what = match self.runs.get(id) {
            Some(m) => format!("{} {}", m.host, m.tag),
            None => term::visible(id),
        };
        println!("{}", self.paint("2", &format!("  ┄ {what}")));
        self.last = Some(id.to_string());
    }

    fn show(&mut self, event: Event) {
        match event {
            Event::Start { id, ts, label, host, cwd, cmd } => {
                let tag = tag(&label, &id);
                let host = term::visible(&host);
                let place = cwd.map(|c| format!("  {}", term::visible(&c))).unwrap_or_default();
                println!();
                println!(
                    "{} {} {}{}",
                    self.paint("2", &term::clock(ts)),
                    self.paint("1;36", &host),
                    self.paint("2", &tag),
                    self.paint("2", &place)
                );
                self.command("  ", &cmd);
                self.runs.insert(id.clone(), Meta { host, tag });
                self.last = Some(id);
            }
            Event::Out { id, s, d } => {
                self.switch_to(&id);
                let (bar, code) = match s.as_str() {
                    "e" => ("│", "31"),
                    "x" => ("·", "33"),
                    _ => ("│", "0"),
                };
                let text = d.strip_suffix('\n').unwrap_or(&d);
                for line in text.split('\n') {
                    // A line redrawn with carriage returns (a progress
                    // bar): show its final state.
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    let line = line.rsplit('\r').next().unwrap_or(line);
                    println!("  {} {}", self.paint("2", bar), self.paint(code, &term::visible(line)));
                }
            }
            Event::End { id, code, ms, note, .. } => {
                self.switch_to(&id);
                let took = seconds(ms);
                let line = match (note, code) {
                    (Some(n), _) => self.paint("31", &format!("  ✗ {} · {took}", term::visible(&n))),
                    (None, Some(0)) => self.paint("32", &format!("  ✓ exit 0 · {took}")),
                    (None, Some(c)) => self.paint("31", &format!("  ✗ exit {c} · {took}")),
                    (None, None) => self.paint("31", &format!("  ✗ stopped · {took}")),
                };
                println!("{line}");
                self.runs.remove(&id);
                self.last = None;
            }
            Event::Denied { id, ts, label, host, cwd, cmd, why } => {
                let place = cwd.map(|c| format!("  {}", term::visible(&c))).unwrap_or_default();
                println!();
                println!(
                    "{} {} {}{}",
                    self.paint("2", &term::clock(ts)),
                    self.paint("1;36", &term::visible(&host)),
                    self.paint("2", &tag(&label, &id)),
                    self.paint("2", &place)
                );
                self.command("  ", &cmd);
                println!("{}", self.paint("33", &format!("  ⊘ not run: {}", term::visible(&why))));
                self.last = None;
            }
        }
        if let Some(w) = &self.waiting {
            println!("{}", self.paint("1;33", w));
        }
        let _ = std::io::stdout().flush();
    }
}

/// Answer one approval request.
fn answer(conn: UnixStream, ui: &Ui) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).unwrap_or(0) == 0 {
        return; // another watch window checking whether we are alive
    }
    let Ok(ask) = serde_json::from_str::<Ask>(&line) else { return };
    let host = term::visible(&ask.host);
    let who = format!("{host} {}", tag(&ask.label, &ask.id));
    // Only a real, open host can be switched to auto-approve.
    let can_stop_asking = ssh::valid_name(&ask.host) && ssh::sock_exists(&ask.host);
    {
        let mut s = ui.lock().unwrap();
        let place = ask.cwd.as_deref().map(|c| format!("  {}", term::visible(c))).unwrap_or_default();
        println!();
        println!("{}", s.paint("1;33", &format!("┌ APPROVE?  {who}{place}")));
        s.command(&s.paint("1;33", "│ "), &ask.cmd);
        let stop = if can_stop_asking { format!("    a = run, and stop asking for {host}") } else { String::new() };
        println!("{}", s.paint("1;33", &format!("└ y = run    n = reject{stop}")));
        s.waiting = Some(format!("  … waiting for y / n on {who}"));
        s.last = None;
        let _ = std::io::stdout().flush();
    }
    let mut stop_asking = false;
    let verdict = loop {
        match term::read_key(conn.as_raw_fd()) {
            Key::Byte(b'y' | b'Y') => break Some(true),
            Key::Byte(b'n' | b'N') => break Some(false),
            Key::Byte(b'a' | b'A') if can_stop_asking => {
                stop_asking = true;
                break Some(true);
            }
            Key::Byte(_) => {}
            Key::Hangup => break None,
        }
    };
    if stop_asking {
        ssh::set_auto_approve(&ask.host, true);
    }
    {
        let mut s = ui.lock().unwrap();
        s.waiting = None;
        let line = match verdict {
            Some(true) if stop_asking => s.paint(
                "32",
                &format!("  approved. {host} is now on auto-approve (`rsh-bridge open {host}` makes it ask again)"),
            ),
            Some(true) => s.paint("32", "  approved"),
            Some(false) => s.paint("31", "  rejected"),
            None => s.paint("2", "  request withdrawn"),
        };
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
    if let Some(ok) = verdict
        && let Ok(mut reply) = serde_json::to_string(&Answer { ok }) {
            reply.push('\n');
            let _ = (&conn).write_all(reply.as_bytes());
        }
}

/// Become the window that answers approvals, if no other window already is.
/// Returns a line describing the outcome.
fn start_approver(ui: &Ui) -> Result<String, String> {
    if !term::stdin_is_tty() {
        term::restore_on_exit(None);
        return Ok("view only (input is not a terminal)".into());
    }
    let path = paths::approve_sock();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                term::restore_on_exit(None);
                return Ok("view only (another watch window answers approvals)".into());
            }
            // Left behind by a window that was killed.
            let _ = fs::remove_file(&path);
            UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    term::restore_on_exit(Some(&path));
    let ui = ui.clone();
    thread::spawn(move || {
        // One at a time: further requests wait their turn.
        for conn in listener.incoming().flatten() {
            answer(conn, &ui);
        }
    });
    Ok("answered in this window (y = run, n = reject, a = run and stop asking for that host)".into())
}

/// Print transcript events as they are appended. Never returns normally.
fn follow(ui: &Ui, history: bool) -> Result<(), String> {
    let mut follower = Follower::open(history)?;
    loop {
        let events = follower.read_new();
        if events.is_empty() {
            thread::sleep(Duration::from_millis(40));
        }
        for event in events {
            ui.lock().unwrap().show(event);
        }
    }
}

pub fn main(args: &[String]) -> Result<(), String> {
    let mut history = false;
    for a in args {
        match a.as_str() {
            "--history" => history = true,
            other => return Err(format!("watch: unknown option '{other}' (the only option is --history)")),
        }
    }
    paths::private_dir(&paths::runtime_dir())?;
    paths::private_dir(&paths::state_dir())?;
    let color = term::stdout_is_tty() && std::env::var_os("NO_COLOR").is_none();
    let ui: Ui = Arc::new(Mutex::new(Screen { color, runs: HashMap::new(), last: None, waiting: None }));
    let approvals = start_approver(&ui)?;
    let open: Vec<String> = ssh::list_open().into_iter().map(|(name, _)| name).collect();
    println!("rsh-bridge watch: agent commands appear below. Ctrl-C to quit.");
    println!("Approvals: {approvals}");
    if open.is_empty() {
        println!("Open hosts: none (open one with `rsh-bridge open <name>`)");
    } else {
        println!("Open hosts: {}", open.join(", "));
    }
    follow(&ui, history)
}
