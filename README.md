# rsh-bridge

Let an AI agent run commands on a server over an SSH connection that **you**
opened, without the agent ever holding a key or a passphrase.

You log in once. The connection stays open in the background, and agents send
whole commands through it and get back stdout, stderr and the exit code. You
watch every command live, and can require your approval before each one runs.
Close the connection and the agent's access ends.

## Install

```sh
git clone https://github.com/Ak-3d/rsh-bridge && cd rsh-bridge
cargo install --path . --root ~/.local      # puts rsh-bridge in ~/.local/bin
claude mcp add -s user rsh-bridge -- rsh-bridge mcp
```

Needs OpenSSH on this machine. Nothing is installed on the servers.

## Use

Put `rsh-bridge open <name> --` in front of the login command you already
type. `<name>` is what the agent will call the server.

```sh
rsh-bridge open web1 -- ssh -i ~/.ssh/id_ed25519 root@web1.example.com
```

You unlock the key as usual and land in your normal shell on the server. It
is a plain SSH session: rsh-bridge shows nothing in it. While it is open,
agents can run commands on `web1`.

To see what agents do, and to answer their requests, run this in another
terminal:

```sh
rsh-bridge watch
```

```
┌ APPROVE?  web1 myproject·a3f2c1 #4  /srv/app
│ $ systemctl status nginx
└ y = run    n = reject    a = run, and stop asking for web1
  approved

14:02:11 web1 myproject·a3f2c1 #4  /srv/app
  $ systemctl status nginx
  │ ● nginx.service - A high performance web server
  ✓ exit 0 · 0.21s
```

- `watch` is the only place that shows agent commands and their output, for
  every open host. It is also the only place that answers requests.
- **`y` / `n`** answer a request. **`a`** runs it and switches that host to
  auto-approve.
- With auto-approve on, nothing asks, but each command and its result is
  still shown in `watch`. Start that way with `open web1 --auto-approve -- ...`.
- If a host asks first and no `watch` window is open, agent commands on it are
  refused.
- Commands that ran while `watch` was closed are in the transcript:
  `rsh-bridge watch --history` replays it.
- **Leaving the shell** (`exit`, or closing the terminal) closes the
  connection, and agent access ends with it.

Agent commands do not pass through your shell. They run as separate requests
over the same connection, in their own fresh shell, so they never type into
whatever you are doing and are not affected by it.

| Command | What it does |
|---|---|
| `rsh-bridge open web1 [--auto-approve] -- <login command>` | Log in, share the connection, and give you a shell |
| `rsh-bridge open web1 --background -- <login command>` | The same without a shell: the connection just stays open |
| `rsh-bridge watch` | Show all agent commands on all hosts, and answer their requests |
| `rsh-bridge attach web1` | One more shell on an open host. Leaving it keeps the host open |
| `rsh-bridge open web1 [--auto-approve]` | On an open host: make it ask again, or with `--auto-approve` stop asking. Otherwise: use a saved login (below) |
| `rsh-bridge list` | Open and saved hosts, and whether each open one asks first |
| `rsh-bridge close web1` | End the connection. `--all` closes every host |

Only one `watch` window answers requests: the first one opened. Others are
view-only.

The login command runs in an interactive bash, so shell functions from
`~/.bashrc` (such as a wrapper that unlocks your key from a password manager)
work. rsh-bridge adds the ssh options that keep
the login open as a shared background connection
(`-M -S <socket> -N -f -o ServerAliveInterval=30`) at the end of the command.
If they need to go somewhere else, write `{flags}` at that spot.

Approval belongs to the open connection. Closing the connection forgets it, so
the next `open` asks again unless you pass `--auto-approve`.

### Optional: saved logins

To avoid retyping a login command, save it in
`~/.config/rsh-bridge/config.toml`. Then `rsh-bridge open web1` is enough.

```toml
[hosts.web1]
# {flags} is required here: it marks where the ssh options go.
open = "ssh {flags} -i ~/.ssh/id_ed25519 root@web1.example.com"

[hosts.staging]
open = "ssh {flags} deploy@staging.example.com"
approval = false          # open this host in auto-approve mode by default
```

A top-level `approval = false` makes auto-approve the default for every host.
The file is read only by `open`; agent sessions never read it.

The agent gets three tools:

- `hosts`: which servers are open right now.
- `run`: `host`, `command`, optional `cwd` and `timeout_seconds`. Returns the
  exit code, stdout and stderr.
- `read_output`: more of a long output that `run` returned only the ends of.

## How a command travels

1. The agent calls `run`. If the host has no open connection, it is refused.
2. Unless the host is in auto-approve mode, the request appears in the
   `watch` window and waits for `y` or `n`. With no `watch` window open, it
   is refused.
3. rsh-bridge runs `ssh -S <socket> <host> -- <command>`. That reuses your
   login: there is no new handshake and no key is read.
4. The command and its output are appended to the transcript and returned to
   the agent.

Each command runs in a fresh, non-interactive shell with no terminal and empty
stdin. `cd` and environment variables do not carry over between commands; the
`cwd` argument does, per host and per agent session.

## What it protects against, and what it doesn't

It does:

- keep keys and passphrases away from agents: rsh-bridge has no login code
- limit agents to hosts you have open, for as long as you keep them open
- ignore your ssh config for agent commands, so agent forwarding and similar
  settings never apply to them
- refuse instead of dialling a new connection when a shared one has died
- show control characters in commands and output as visible escapes, so
  neither an agent nor a server can hide text in the watch window

It does not:

- stop an agent that has a general shell on your machine from using the
  socket file directly (`ssh -S ...`) or switching a host to auto-approve,
  skipping approval and the transcript. Those files belong to your user.
  Running agents as a separate Unix user is the fix for that.
- restrict what an approved command does on the server. The agent has the
  rights of the account you logged in as.
- kill a remote command that hit its timeout or was cancelled. The agent gets
  control back, but without a terminal the remote process is not signalled
  and may keep running.

## Files

| Path | Contents |
|---|---|
| `~/.config/rsh-bridge/config.toml` | Optional saved logins |
| `$XDG_RUNTIME_DIR/rsh-bridge/hosts/<name>.sock` | Shared connections; gone at logout |
| `$XDG_RUNTIME_DIR/rsh-bridge/hosts/<name>.auto` | Present while that connection is in auto-approve mode |
| `$XDG_RUNTIME_DIR/rsh-bridge/approve.sock` | Where the `watch` window receives approval requests |
| `~/.local/state/rsh-bridge/transcript.jsonl` | Every command, with up to 256 KB of output each. Rotated at 16 MB |

The transcript holds server output in plain text (mode 600). Delete it when
you no longer want that history.

## Tests

`cargo test` covers quoting, host-name checks, output trimming and display
escaping. End-to-end behaviour was checked by hand against a throwaway local
`sshd`; there is no automated end-to-end suite in the repository yet.

## License

MIT. See [LICENSE](LICENSE).
