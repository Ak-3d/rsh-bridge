//! `~/.config/rsh-bridge/config.toml`: optional. It saves login commands
//! so `rsh-bridge open web1` works without typing them again, and sets
//! whether a host opens in "ask" or "auto-approve" mode by default.
//!
//! ```toml
//! approval = true                 # ask in the watch window before each command
//!
//! [hosts.web1]
//! open = "ssh {flags} -i ~/.ssh/id_ed25519 root@web1.example.com"
//! approval = false                # per-host override
//! ```
//!
//! The file is read when a host is opened. Agent sessions never read it.

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::paths;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Default for hosts that don't say otherwise.
    #[serde(default = "yes")]
    pub approval: bool,
    /// How `open` lines are run. Default: `bash -ic`, so shell functions
    /// from `~/.bashrc` are available.
    #[serde(default)]
    pub shell: Option<Vec<String>>,
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// The command you would type to connect, with `{flags}` where the
    /// connection-sharing options go.
    pub open: Option<String>,
    pub approval: Option<bool>,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config { approval: true, shell: None, hosts: BTreeMap::new() }
    }
}

impl Config {
    pub fn needs_approval(&self, host: &str) -> bool {
        self.hosts.get(host).and_then(|h| h.approval).unwrap_or(self.approval)
    }
}

/// A missing file is fine (defaults apply). A broken file is an error, so a
/// typo can never silently open a host in auto-approve mode.
pub fn load() -> Result<Config, String> {
    let path = paths::config_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}
