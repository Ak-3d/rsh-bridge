//! rsh-bridge: let an agent run commands on a server over an SSH connection
//! that you opened, without the agent ever holding a key.

mod approval;
mod attach;
mod config;
mod mcp;
mod paths;
mod run;
mod ssh;
mod term;
mod transcript;
mod watch;

use std::process::{Command, ExitCode};
use std::time::SystemTime;

const HELP: &str = "\
rsh-bridge: let an agent run commands over an SSH connection you opened.

For you:
  rsh-bridge open <name> [--auto-approve] [--background] -- <login command...>
        Log in with your usual command, for example:
          rsh-bridge open web1 -- ssh -i ~/.ssh/key root@web1
        You get your normal shell on the server, and nothing else is shown in
        it. While it is open, agents can run commands on the server under
        <name>. Each command waits for your y/n in `rsh-bridge watch` unless
        you add --auto-approve. Leaving the shell closes the connection.
        With --background you get no shell; the connection just stays open.
  rsh-bridge open <name>      The same, using a login command saved in the config file.
                              On a host that is already open: make it ask again
                              (or, with --auto-approve, stop asking).
  rsh-bridge watch            The one place that shows what agents run, on all
                              hosts, live, with the output. Approve here:
                              y = run, n = reject, a = run and stop asking.
                              Without it, commands that need approval are refused.
                              (--history replays the saved transcript first)
  rsh-bridge attach <name>    One more shell on an open host. Leaving it keeps
                              the host open.
  rsh-bridge close <name>     Close a host (--all closes every open host)
  rsh-bridge list             Show saved and open hosts
  rsh-bridge flags <name>     Print the ssh options that share a connection

For the agent:
  rsh-bridge mcp              MCP server on stdin/stdout. Register it once with:
                              claude mcp add -s user rsh-bridge -- rsh-bridge mcp

Optional config for saved logins: ~/.config/rsh-bridge/config.toml (see README.md)
";

fn name_arg<'a>(args: &'a [String], command: &str) -> Result<&'a str, String> {
    match args {
        [name] if ssh::valid_name(name) => Ok(name),
        [name] => Err(format!("'{name}' is not a usable host name (letters, digits, - _ . only)")),
        _ => Err(format!("usage: rsh-bridge {command} <name>")),
    }
}

/// `open <name> [--auto-approve] [--background] [-- <login command...>]`
fn cmd_open(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: rsh-bridge open <name> [--auto-approve] [--background] [-- <login command...>]";
    let (ours, login) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &args[..0]),
    };
    let mut name: Option<&str> = None;
    let mut flag_auto = false;
    let mut background = false;
    for arg in ours {
        match arg.as_str() {
            "--auto-approve" => flag_auto = true,
            "--background" | "-b" => background = true,
            a if a.starts_with('-') => return Err(format!("open: unknown option '{a}'\n{USAGE}")),
            a if name.is_none() => name = Some(a),
            _ => return Err(format!("{USAGE}\n(put `--` before the login command)")),
        }
    }
    let name = name.ok_or(USAGE)?;
    if !ssh::valid_name(name) {
        return Err(format!("'{name}' is not a usable host name (letters, digits, - _ . only)"));
    }
    if args.contains(&"--".to_string()) && login.is_empty() {
        return Err(format!("nothing after `--`\n{USAGE}"));
    }

    let cfg = config::load()?;
    let auto = flag_auto || !cfg.needs_approval(name);
    // Without a terminal there is no shell to give you, so stay in the
    // background.
    let background = background || !term::stdin_is_tty() || !term::stdout_is_tty();
    let mode = if auto {
        "Agent commands run WITHOUT asking you (auto-approve)."
    } else {
        "Each agent command waits for your y/n in `rsh-bridge watch`."
    };
    paths::private_dir(&paths::runtime_dir())?;
    paths::private_dir(&paths::hosts_dir())?;
    if ssh::is_open(name) {
        ssh::set_auto_approve(name, auto);
        println!("{name} is already open. {mode}\nFor a shell on it: `rsh-bridge attach {name}`.");
        return Ok(());
    }
    let _ = std::fs::remove_file(ssh::sock_path(name)); // left by a dead connection
    ssh::set_auto_approve(name, false);

    let shell = cfg.shell.clone().unwrap_or_else(|| vec!["bash".into(), "-ic".into()]);
    let (program, shell_args) = shell.split_first().ok_or("`shell` in the config file is empty")?;
    let mut command = Command::new(program);
    command.args(shell_args);
    if login.is_empty() {
        // The saved login command from the config file.
        let template = cfg.hosts.get(name).and_then(|h| h.open.as_deref()).ok_or_else(|| {
            format!(
                "no saved login for '{name}' in {}.\nGive the login command after `--`, for example:\n  \
                 rsh-bridge open {name} -- ssh user@server",
                paths::config_file().display()
            )
        })?;
        if !template.contains("{flags}") {
            return Err(format!("the `open` command for {name} must contain {{flags}} where the ssh options go"));
        }
        command.arg(template.replace("{flags}", &ssh::master_flags_text(name)));
    } else {
        // The login command given on the command line. It runs through the
        // shell as `"$@"`, so a shell function from ~/.bashrc works and every
        // argument stays exactly as typed. The sharing options go where
        // `{flags}` is, or at the end (ssh accepts options after the host).
        let flags = ssh::master_flags(name);
        let mut words: Vec<String> = Vec::new();
        let mut placed = false;
        for word in login {
            if word == "{flags}" {
                words.extend(flags.iter().cloned());
                placed = true;
            } else {
                words.push(word.clone());
            }
        }
        if !placed {
            words.extend(flags);
        }
        command.arg("\"$@\"").arg("rsh-bridge").args(words);
    }
    let status = term::keep_foreground(|| command.status()).map_err(|e| format!("could not run {program}: {e}"))?;

    if ssh::is_open(name) {
        ssh::set_auto_approve(name, auto);
        if background {
            println!(
                "{name} is open in the background until you run `rsh-bridge close {name}`. {mode}\n\
                 For a shell on it: `rsh-bridge attach {name}`."
            );
            Ok(())
        } else {
            // Your shell. Leaving it closes the connection.
            attach::run(name, true)
        }
    } else {
        Err(format!("{name} did not open (the login command ended with {status})"))
    }
}

fn cmd_attach(args: &[String]) -> Result<(), String> {
    let name = name_arg(args, "attach")?;
    attach::run(name, false)
}

fn cmd_close(args: &[String]) -> Result<(), String> {
    if args.len() == 1 && args[0] == "--all" {
        let open = ssh::list_open();
        if open.is_empty() {
            println!("Nothing is open.");
        }
        for (name, _) in open {
            ssh::close(&name);
            println!("{name} closed.");
        }
        return Ok(());
    }
    let name = name_arg(args, "close")?;
    if ssh::close(name) {
        println!("{name} closed.");
    } else {
        println!("{name} was not open.");
    }
    Ok(())
}

fn ago(t: Option<SystemTime>) -> String {
    let Some(secs) = t.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()) else {
        return String::new();
    };
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

fn cmd_list() -> Result<(), String> {
    let cfg = config::load()?;
    let open = ssh::list_open();
    let mut names: Vec<&str> = cfg.hosts.keys().map(String::as_str).collect();
    for (name, _) in &open {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    if names.is_empty() {
        println!("No hosts yet. Add one to {}", paths::config_file().display());
        return Ok(());
    }
    for name in names {
        let state = match open.iter().find(|(n, _)| n == name) {
            Some((_, since)) => format!("open {}", ago(*since)),
            None => "closed".to_string(),
        };
        let is_open = open.iter().any(|(n, _)| n == name);
        let approval = match (is_open, ssh::auto_approve(name)) {
            (true, true) => "auto-approve",
            (true, false) => "asks you first",
            (false, _) => "",
        };
        println!("{name:<20} {state:<14} {approval}");
    }
    Ok(())
}

fn cmd_flags(args: &[String]) -> Result<(), String> {
    let name = name_arg(args, "flags")?;
    paths::private_dir(&paths::runtime_dir())?;
    paths::private_dir(&paths::hosts_dir())?;
    println!("{}", ssh::master_flags_text(name));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    let result = match args.first().map(String::as_str) {
        Some("open") => cmd_open(rest),
        Some("attach") => cmd_attach(rest),
        Some("close") => cmd_close(rest),
        Some("list" | "ls") => cmd_list(),
        Some("flags") => cmd_flags(rest),
        Some("watch") => watch::main(rest),
        Some("mcp") => mcp::main(),
        Some("help" | "-h" | "--help") | None => {
            print!("{HELP}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("rsh-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(format!("unknown command '{other}' (try `rsh-bridge help`)")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rsh-bridge: {e}");
            ExitCode::FAILURE
        }
    }
}
