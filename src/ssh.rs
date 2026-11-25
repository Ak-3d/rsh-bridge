//! Everything that touches OpenSSH. rsh-bridge never authenticates: it only
//! hands commands to a connection the user already opened, through that
//! connection's control socket.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::paths;

/// Host names become file names and ssh arguments, so keep them boring.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && !s.starts_with(['-', '.'])
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn sock_path(name: &str) -> PathBuf {
    paths::hosts_dir().join(format!("{name}.sock"))
}

pub fn sock_exists(name: &str) -> bool {
    fs::symlink_metadata(sock_path(name))
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// An ssh client that can only use the shared connection for `name`.
///
/// - `-F none`: ignore every ssh config file, so nothing in it (agent
///   forwarding, environment passing, a `Host` entry with the same name)
///   applies to agent commands.
/// - `BatchMode=yes`: never prompt.
/// - `ProxyCommand=false`: if the shared connection is gone, ssh would fall
///   back to dialling a fresh one. This makes that fallback fail instantly
///   instead of reaching the network.
pub fn client(name: &str) -> Command {
    let mut c = Command::new("ssh");
    c.arg("-F").arg("none").arg("-S").arg(sock_path(name)).args([
        "-o",
        "ControlMaster=no",
        "-o",
        "BatchMode=yes",
        "-o",
        "ProxyCommand=false",
    ]);
    c
}

fn control(name: &str, op: &str) -> bool {
    client(name)
        .args(["-O", op, "--", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is there a live shared connection for `name`?
pub fn is_open(name: &str) -> bool {
    sock_exists(name) && control(name, "check")
}

/// Close the shared connection and remove its socket.
pub fn close(name: &str) -> bool {
    let was_open = sock_exists(name) && control(name, "exit");
    let _ = fs::remove_file(sock_path(name));
    set_auto_approve(name, false);
    was_open
}

/// A marker file next to the socket: present means commands on this
/// connection run without asking the user. It lives and dies with the
/// connection, so every new `open` starts from "ask" unless told otherwise.
fn auto_path(name: &str) -> PathBuf {
    paths::hosts_dir().join(format!("{name}.auto"))
}

pub fn set_auto_approve(name: &str, on: bool) {
    if on {
        let _ = fs::write(auto_path(name), b"");
    } else {
        let _ = fs::remove_file(auto_path(name));
    }
}

pub fn auto_approve(name: &str) -> bool {
    auto_path(name).is_file()
}

/// Live shared connections, with the time each was opened.
pub fn list_open() -> Vec<(String, Option<SystemTime>)> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(paths::hosts_dir()) else { return out };
    for entry in dir.flatten() {
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".sock")) else { continue };
        if valid_name(name) && is_open(name) {
            let opened = entry.metadata().ok().and_then(|m| m.modified().ok());
            out.push((name.to_string(), opened));
        }
    }
    out.sort();
    out
}

/// The options that turn a normal `ssh` login into a shared connection
/// that stays in the background, as separate arguments.
pub fn master_flags(name: &str) -> Vec<String> {
    vec![
        "-M".into(),
        "-S".into(),
        sock_path(name).to_string_lossy().into_owned(),
        "-N".into(),
        "-f".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
    ]
}

/// The same options as one piece of shell text.
pub fn master_flags_text(name: &str) -> String {
    master_flags(name).iter().map(|f| shell_quote(f)).collect::<Vec<_>>().join(" ")
}

/// Quote `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b));
    if safe { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// The text sent to the remote login shell: optionally change directory,
/// then run the command exactly as given.
pub fn remote_script(cwd: Option<&str>, command: &str) -> String {
    let Some(dir) = cwd else { return command.to_string() };
    let quoted = if dir == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        format!("\"$HOME\"/{}", shell_quote(rest))
    } else {
        shell_quote(dir)
    };
    format!("cd -- {quoted} || exit 1\n{command}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names() {
        for good in ["web1", "db-eu.prod_2", "A"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-oProxyCommand=x", ".hidden", "../x", "a/b", "a b", "a;b", "é"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/srv/app-1"), "/srv/app-1");
        assert_eq!(shell_quote("my dir"), "'my dir'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn scripts() {
        assert_eq!(remote_script(None, "ls -la"), "ls -la");
        assert_eq!(remote_script(Some("/srv/app"), "ls"), "cd -- /srv/app || exit 1\nls");
        assert_eq!(remote_script(Some("~"), "ls"), "cd -- \"$HOME\" || exit 1\nls");
        assert_eq!(remote_script(Some("~/my app"), "ls"), "cd -- \"$HOME\"/'my app' || exit 1\nls");
        assert_eq!(remote_script(Some("; rm -rf /"), "ls"), "cd -- '; rm -rf /' || exit 1\nls");
    }
}
