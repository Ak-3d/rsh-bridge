//! `rsh-bridge mcp`: the MCP server an agent session talks to, over stdin
//! and stdout, one JSON-RPC message per line.

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::approval::{self, Ask, Verdict};
use crate::run::{self, Cancel, Captured, INLINE, KEEP};
use crate::transcript::{Event, Transcript, now};
use crate::{config, ssh};

const PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 3600;
/// How long a command may wait for the user's yes or no.
const APPROVAL_WAIT: Duration = Duration::from_secs(600);
/// Finished commands whose output stays available to `read_output`.
const KEPT_RUNS: usize = 8;

const INSTRUCTIONS: &str = "Runs shell commands on remote servers through SSH connections that \
the user opened beforehand. You cannot open or authenticate connections: call `hosts` to see what \
is open, and if the server you need is missing, ask the user to open it in their terminal with \
`rsh-bridge open <name> -- <their ssh login command>`. Every command and its output is logged for \
the user, who can watch them live and may have to approve each one.";

struct Kept {
    id: String,
    out: Captured,
    err: Captured,
}

struct Server {
    stdout: Mutex<io::Stdout>,
    log: Transcript,
    /// Random id of this agent session, used in command ids.
    sid: String,
    /// What the user sees for this session: project folder plus `sid`.
    label: String,
    counter: AtomicU64,
    /// Last working directory given for each host.
    cwd: Mutex<HashMap<String, String>>,
    kept: Mutex<VecDeque<Kept>>,
    /// Tool calls in progress, by JSON-RPC request id.
    active: Mutex<HashMap<String, Arc<Cancel>>>,
}

fn session_id() -> String {
    let mut b = [0u8; 3];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if read.is_err() {
        b = (std::process::id() as u32).to_le_bytes()[..3].try_into().unwrap();
    }
    format!("{:02x}{:02x}{:02x}", b[0], b[1], b[2])
}

fn tools() -> Value {
    json!([
        {
            "name": "hosts",
            "description": "List the remote servers the user has opened for you right now. You \
cannot open connections yourself: if the server you need is not listed as open, ask the user to \
open it in their terminal with `rsh-bridge open <name> -- <their ssh login command>`.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "run",
            "description": "Run a shell command on a remote server over an SSH connection the \
user already opened. The whole string is run by the remote login shell, so pipes, `&&` and \
multi-line scripts work. It is non-interactive: there is no terminal and stdin is empty, so use \
flags that avoid prompts (a command that waits for input fails or hits the timeout). Every call \
starts a fresh shell: `cd` and environment variables do not carry over to the next call; pass \
`cwd` instead, which is remembered per host for later calls. A background process must redirect \
its output (`nohup cmd >log 2>&1 &`) or the call waits for it. The user can watch each command and \
its output live and may be asked to approve it first. Returns the exit code, stdout and stderr.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Name of an open host, as listed by `hosts`." },
                    "command": { "type": "string", "description": "The full shell command to run." },
                    "cwd": { "type": "string", "description": "Directory to run in. Optional; defaults to the last `cwd` used for this host, or the login directory." },
                    "timeout_seconds": { "type": "integer", "description": "Stop waiting after this many seconds. Default 120, maximum 3600." }
                },
                "required": ["host", "command"],
                "additionalProperties": false
            }
        },
        {
            "name": "read_output",
            "description": "Read more of a recent command's output when `run` returned only the \
beginning and end. Output of the last few commands is kept, up to 4 MB per stream.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string", "description": "The run id from the first line of a `run` result." },
                    "stream": { "type": "string", "enum": ["stdout", "stderr"], "description": "Default stdout." },
                    "offset": { "type": "integer", "description": "Byte offset to start at. Default 0." },
                    "limit": { "type": "integer", "description": "Bytes to return. Default 24000, maximum 100000." }
                },
                "required": ["run_id"],
                "additionalProperties": false
            }
        }
    ])
}

fn number(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

impl Server {
    fn send(&self, message: Value) {
        let mut out = self.stdout.lock().unwrap();
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    fn reply(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn dispatch(self: &Arc<Self>, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.request(method, id, params),
            (Some("notifications/cancelled"), None) => {
                if let Some(request) = params.get("requestId")
                    && let Some(cancel) = self.active.lock().unwrap().get(&request.to_string()) {
                        cancel.fire();
                    }
            }
            _ => {} // other notifications, and replies to requests we never send
        }
    }

    fn request(self: &Arc<Self>, method: &str, id: Value, params: Value) {
        match method {
            "initialize" => {
                let wanted = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = if PROTOCOLS.contains(&wanted) { wanted } else { DEFAULT_PROTOCOL };
                self.reply(id, json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "rsh-bridge", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS
                }));
            }
            "ping" => self.reply(id, json!({})),
            "tools/list" => self.reply(id, json!({ "tools": tools() })),
            "tools/call" => {
                // Each call gets its own thread so a slow command never
                // blocks another call or a cancellation.
                let cancel = Arc::new(Cancel::default());
                let key = id.to_string();
                self.active.lock().unwrap().insert(key.clone(), cancel.clone());
                let me = self.clone();
                thread::spawn(move || {
                    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    let (text, is_error) = match params.get("name").and_then(Value::as_str) {
                        Some("hosts") => me.tool_hosts(),
                        Some("run") => me.tool_run(&args, &cancel),
                        Some("read_output") => me.tool_read(&args),
                        other => (format!("Unknown tool: {}", other.unwrap_or("(none)")), true),
                    };
                    me.active.lock().unwrap().remove(&key);
                    me.reply(id, json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": is_error
                    }));
                });
            }
            _ => self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            })),
        }
    }

    fn tool_hosts(&self) -> (String, bool) {
        let cfg = config::load();
        let open = ssh::list_open();
        let mut text = String::new();
        if open.is_empty() {
            text.push_str("No hosts are open. Ask the user to open one in their terminal: `rsh-bridge open <name> -- <their ssh login command>`.\n");
        } else {
            text.push_str("Open hosts:\n");
            for (name, _) in &open {
                let approval = if ssh::auto_approve(name) {
                    "commands run without asking"
                } else {
                    "the user approves each command"
                };
                let cwd = match self.cwd.lock().unwrap().get(name) {
                    Some(dir) => format!("; working directory {dir}"),
                    None => String::new(),
                };
                text.push_str(&format!("- {name} ({approval}{cwd})\n"));
            }
        }
        // Saved logins the user could open. Only `open` needs the config
        // file, so a broken one is not this session's problem.
        if let Ok(c) = &cfg {
            let closed: Vec<&str> = c
                .hosts
                .keys()
                .map(String::as_str)
                .filter(|k| !open.iter().any(|(name, _)| name == k))
                .collect();
            if !closed.is_empty() {
                text.push_str(&format!("Saved but closed (the user can run `rsh-bridge open <name>`): {}\n", closed.join(", ")));
            }
        }
        (text, false)
    }

    fn tool_run(&self, args: &Value, cancel: &Cancel) -> (String, bool) {
        let Some(host) = args.get("host").and_then(Value::as_str) else {
            return ("Missing `host`.".into(), true);
        };
        let Some(command) = args.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) else {
            return ("Missing `command`.".into(), true);
        };
        let given_cwd = args.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty());
        if command.contains('\0') || given_cwd.is_some_and(|c| c.contains('\0')) {
            return ("`command` and `cwd` must not contain NUL bytes.".into(), true);
        }
        let timeout = number(args.get("timeout_seconds")).unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);

        if !ssh::valid_name(host) {
            return (format!("'{host}' is not a host name. Call `hosts` to see the open hosts."), true);
        }
        if !ssh::sock_exists(host) {
            let open: Vec<String> = ssh::list_open().into_iter().map(|(name, _)| name).collect();
            let open = if open.is_empty() { "none".to_string() } else { open.join(", ") };
            return (
                format!(
                    "Host '{host}' is not open (open hosts: {open}). You cannot open it yourself: \
ask the user to open it in their terminal with `rsh-bridge open {host} -- <their ssh login command>`."
                ),
                true,
            );
        }
        let cwd: Option<String> = match given_cwd {
            Some(dir) => {
                self.cwd.lock().unwrap().insert(host.to_string(), dir.to_string());
                Some(dir.to_string())
            }
            None => self.cwd.lock().unwrap().get(host).cloned(),
        };
        let id = format!("{}-{}", self.sid, self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        let denied = |why: &str| {
            self.log.write(&Event::Denied {
                id: id.clone(),
                ts: now(),
                label: self.label.clone(),
                host: host.to_string(),
                cwd: cwd.clone(),
                cmd: command.to_string(),
                why: why.to_string(),
            });
        };

        if !ssh::auto_approve(host) {
            let ask = Ask {
                id: id.clone(),
                label: self.label.clone(),
                host: host.to_string(),
                cwd: cwd.clone(),
                cmd: command.to_string(),
            };
            match approval::ask(&ask, cancel, APPROVAL_WAIT) {
                Verdict::Approved => {}
                Verdict::Rejected => {
                    denied("rejected by the user");
                    return ("Not run: the user rejected this command. Don't retry it unchanged; ask the user how to proceed.".into(), true);
                }
                Verdict::NoWatcher => {
                    denied("nobody was there to approve it");
                    return (
                        format!(
                            "Not run: commands on '{host}' need the user's approval, and nothing is open \
to approve them. Ask the user to run `rsh-bridge watch` in a terminal, then try again."
                        ),
                        true,
                    );
                }
                Verdict::TimedOut => {
                    denied("no answer in time");
                    return ("Not run: the user did not answer the approval request in time.".into(), true);
                }
                Verdict::Cancelled => {
                    denied("cancelled before approval");
                    return ("Cancelled before the command was sent.".into(), true);
                }
            }
        }

        self.log.write(&Event::Start {
            id: id.clone(),
            ts: now(),
            label: self.label.clone(),
            host: host.to_string(),
            cwd: cwd.clone(),
            cmd: command.to_string(),
        });
        let end = |code: Option<i32>, ms: u64, note: Option<&str>| {
            self.log.write(&Event::End { id: id.clone(), ts: now(), code, ms, note: note.map(str::to_string) });
        };
        let done = match run::execute(host, cwd.as_deref(), command, Duration::from_secs(timeout), &id, &self.log, cancel) {
            Ok(d) => d,
            Err(e) => {
                end(None, 0, Some("could not start ssh"));
                return (format!("Could not start the local ssh program: {e}"), true);
            }
        };

        // ssh itself exits 255 when the connection is gone, but a remote
        // command may also exit 255, so check before blaming the link.
        if done.code == Some(255) && !done.timed_out && !done.cancelled && !ssh::is_open(host) {
            end(None, done.ms, Some("connection closed"));
            return (
                format!(
                    "The connection to '{host}' is closed, so the command did not complete. Ask the \
user to open it again with `rsh-bridge open {host} -- <their ssh login command>`.\n{}",
                    done.err.inline()
                ),
                true,
            );
        }

        let note = if done.timed_out {
            Some(format!("timed out after {timeout}s"))
        } else if done.cancelled {
            Some("cancelled".to_string())
        } else {
            None
        };
        end(done.code, done.ms, note.as_deref());

        let status = match (&note, done.code) {
            (Some(n), _) => format!("{n} (the remote process may still be running)"),
            (None, Some(code)) => format!("exit {code}"),
            (None, None) => "stopped by a signal".to_string(),
        };
        let place = match &cwd {
            Some(dir) => format!("{host}:{dir}"),
            None => host.to_string(),
        };
        let mut text = format!("{status} · {:.2}s · {place} · run {id}\n", done.ms as f64 / 1000.0);
        if done.out.total == 0 && done.err.total == 0 {
            text.push_str("(no output)\n");
        }
        for (name, stream) in [("stdout", &done.out), ("stderr", &done.err)] {
            if stream.total == 0 {
                continue;
            }
            text.push_str(&format!("--- {name} ---\n{}", stream.inline()));
            if !text.ends_with('\n') {
                text.push('\n');
            }
            if stream.total as usize > INLINE {
                let kept = if stream.total as usize > KEEP { " (only the first 4 MB are kept)" } else { "" };
                text.push_str(&format!(
                    "[{name} is {} bytes; the middle is omitted. read_output with run_id \"{id}\" returns more{kept}.]\n",
                    stream.total
                ));
            }
        }
        let is_error = note.is_some();
        let mut kept = self.kept.lock().unwrap();
        kept.push_back(Kept { id, out: done.out, err: done.err });
        while kept.len() > KEPT_RUNS {
            kept.pop_front();
        }
        (text, is_error)
    }

    fn tool_read(&self, args: &Value) -> (String, bool) {
        let Some(run_id) = args.get("run_id").and_then(Value::as_str) else {
            return ("Missing `run_id`.".into(), true);
        };
        let stream = args.get("stream").and_then(Value::as_str).unwrap_or("stdout");
        let offset = number(args.get("offset")).unwrap_or(0) as usize;
        let limit = (number(args.get("limit")).unwrap_or(INLINE as u64) as usize).clamp(1, 100_000);
        let kept = self.kept.lock().unwrap();
        let Some(run) = kept.iter().find(|k| k.id == run_id) else {
            return (format!("No kept output for run '{run_id}'. Only the last {KEPT_RUNS} commands of this session are kept."), true);
        };
        let data = match stream {
            "stdout" => &run.out,
            "stderr" => &run.err,
            _ => return ("`stream` must be \"stdout\" or \"stderr\".".into(), true),
        };
        if offset >= data.head.len() {
            return (
                format!("Nothing at offset {offset}: {} of {} bytes of {stream} are kept.", data.head.len(), data.total),
                false,
            );
        }
        let end = (offset + limit).min(data.head.len());
        (
            format!(
                "{stream} bytes {offset}-{end} of {} (run {run_id})\n{}",
                data.total,
                String::from_utf8_lossy(&data.head[offset..end])
            ),
            false,
        )
    }
}

pub fn main() -> Result<(), String> {
    let sid = session_id();
    let folder = std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "agent".into());
    let server = Arc::new(Server {
        stdout: Mutex::new(io::stdout()),
        log: Transcript::open()?,
        label: format!("{folder}·{sid}"),
        sid,
        counter: AtomicU64::new(0),
        cwd: Mutex::new(HashMap::new()),
        kept: Mutex::new(VecDeque::new()),
        active: Mutex::new(HashMap::new()),
    });
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.dispatch(msg),
            Err(_) => server.send(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "parse error" }
            })),
        }
    }
    // The agent session is gone: stop anything still running for it.
    for cancel in server.active.lock().unwrap().values() {
        cancel.fire();
    }
    Ok(())
}
