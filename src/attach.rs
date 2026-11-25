//! `rsh-bridge attach`: your own interactive shell on an open host.
//!
//! This is plain `ssh`, started on the shared connection and given your
//! terminal directly. rsh-bridge draws nothing in it: agent commands travel
//! as separate requests over the same connection, and are shown and approved
//! only in `rsh-bridge watch`.

use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::{ssh, term};

/// The ssh process running your shell, for the signal handler.
static SHELL: AtomicI32 = AtomicI32::new(0);

/// While your shell runs we must outlive it, so the connection can be closed
/// afterwards. Ctrl-C and Ctrl-\ belong to the shell; a hangup or a kill is
/// passed on to it, and we leave once it has ended.
extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP || sig == libc::SIGTERM {
        let pid = SHELL.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGHUP) };
        }
    }
}

const SIGNALS: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];

/// Print a line, without failing if the terminal is already gone.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// Run your interactive shell on `name` until it ends. With `close_on_exit`,
/// ending the shell also ends the shared connection.
pub fn run(name: &str, close_on_exit: bool) -> Result<(), String> {
    if !term::stdin_is_tty() || !term::stdout_is_tty() {
        return Err("a shell needs a terminal".into());
    }
    if !ssh::is_open(name) {
        return Err(format!("{name} is not open. Open it with `rsh-bridge open {name} -- <login command>`."));
    }

    let lasts = if close_on_exit { "until you leave this shell" } else { "also after you leave this shell" };
    let approvals = if ssh::auto_approve(name) {
        "Their commands run without asking; see them with `rsh-bridge watch`."
    } else {
        "Their commands wait for your y/n in `rsh-bridge watch` (run it in another terminal)."
    };
    say(&format!("[rsh-bridge] {name} is open for agents {lasts}. {approvals}"));

    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        for sig in SIGNALS {
            libc::signal(sig, handler);
        }
    }
    // Your shell is one more session on the shared connection. It uses your
    // normal ssh settings; only the connection itself is fixed.
    let spawned = Command::new("ssh")
        .arg("-S")
        .arg(ssh::sock_path(name))
        .args(["-o", "ControlMaster=no", "-o", "ProxyCommand=false", "-t", "--", name])
        .spawn();
    let result = match spawned {
        Ok(mut child) => {
            SHELL.store(child.id() as i32, Ordering::SeqCst);
            let _ = child.wait();
            SHELL.store(0, Ordering::SeqCst);
            Ok(())
        }
        Err(e) => Err(format!("could not start ssh: {e}")),
    };
    unsafe {
        for sig in SIGNALS {
            libc::signal(sig, libc::SIG_DFL);
        }
    }

    if close_on_exit {
        ssh::close(name);
        say(&format!("[rsh-bridge] {name} closed. Agents can no longer use it."));
    } else if ssh::is_open(name) {
        say(&format!("[rsh-bridge] {name} is still open for agents. End it with `rsh-bridge close {name}`."));
    } else {
        say(&format!("[rsh-bridge] {name} is closed."));
    }
    result
}
