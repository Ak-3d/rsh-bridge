//! Where rsh-bridge keeps its files.
//!
//! - runtime dir (`$XDG_RUNTIME_DIR/rsh-bridge`): the shared-connection
//!   sockets and the approval socket. Gone at logout.
//! - state dir (`~/.local/state/rsh-bridge`): the transcript.
//! - config (`~/.config/rsh-bridge/config.toml`).

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> PathBuf {
    env_dir("HOME").unwrap_or_else(|| PathBuf::from("/"))
}

pub fn runtime_dir() -> PathBuf {
    match env_dir("XDG_RUNTIME_DIR") {
        Some(d) => d.join("rsh-bridge"),
        None => PathBuf::from(format!("/tmp/rsh-bridge-{}", uid())),
    }
}

pub fn hosts_dir() -> PathBuf {
    runtime_dir().join("hosts")
}

/// Where the `watch` window answers approvals.
pub fn approve_sock() -> PathBuf {
    runtime_dir().join("approve.sock")
}

pub fn state_dir() -> PathBuf {
    env_dir("XDG_STATE_HOME")
        .unwrap_or_else(|| home().join(".local/state"))
        .join("rsh-bridge")
}

pub fn transcript() -> PathBuf {
    state_dir().join("transcript.jsonl")
}

pub fn config_file() -> PathBuf {
    env_dir("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home().join(".config"))
        .join("rsh-bridge/config.toml")
}

/// Create `p` if needed and make sure it is a real directory that only this
/// user can enter. Refuses a symlink or a directory owned by someone else.
pub fn private_dir(p: &Path) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", p.display());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .map_err(fail)?;
    let md = fs::symlink_metadata(p).map_err(fail)?;
    if !md.is_dir() || md.uid() != uid() {
        return Err(format!("{}: not a directory owned by you", p.display()));
    }
    if md.mode() & 0o077 != 0 {
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).map_err(fail)?;
    }
    Ok(())
}
