//! Copy text to the clipboard of the person at the keyboard, for bindings
//! like `prefix + y` (copy the process name).
//!
//! No single mechanism works everywhere, so every route that can work is
//! tried, and a copy that reaches the clipboard twice is harmless:
//!
//! 1. pmux's own tmux `set-buffer -w`: fills pmux's paste buffer and sends
//!    OSC 52 to the terminal pmux's client runs in. This is enough when pmux
//!    is attached straight from a terminal, locally or over SSH.
//! 2. The outer tmux that shows this process, when pmux knows it (it records
//!    the host socket at attach). tmux's default `set-clipboard external`
//!    drops OSC 52 from programs inside it, so route 1 alone never reaches the
//!    terminal through a stock outer tmux; the outer server's own
//!    `set-buffer -w` does, without changing its settings.
//! 3. A local clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`,
//!    `clip.exe`), only when the user is not on SSH, for terminals that do not
//!    support OSC 52 (macOS Terminal.app, for one).

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::tmux::{tmux_cmd, tmux_command};

/// Copy `text` for the pmux client `client` (its tty, tmux's `client_name`)
/// that is showing the pmux process `session`. Succeeds when any route
/// accepted the text.
pub(crate) fn copy_to_clipboard(text: &str, client: &str, session: &str) -> Result<()> {
    let mut copied = tmux_cmd()
        .args(["set-buffer", "-w", "-t", client, "--", text])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());

    // The server whose client is the user's own terminal: the outer tmux
    // when there is one, otherwise pmux's.
    let mut user_server = UserServer::Pmux {
        session: session.to_string(),
    };
    if let Ok(Some(socket)) = crate::state::read_session_host_socket(session)
        && let Some(outer_client) = outer_client_for(&socket, client)
    {
        copied |= outer_tmux(&socket)
            .args(["set-buffer", "-w", "-t", &outer_client, "--", text])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        user_server = UserServer::Outer {
            socket,
            client: outer_client,
        };
    }

    let env = |name: &str| user_server.environment(name);
    if !is_remote(&env)
        && let Some(argv) = local_clipboard_command(&env, std::env::consts::OS, &|program| {
            program_on_path(program)
        })
    {
        copied |= pipe_to(&argv, text, &env);
    }

    if !copied {
        bail!("could not copy to the clipboard");
    }
    Ok(())
}

enum UserServer {
    Pmux { session: String },
    Outer { socket: String, client: String },
}

impl UserServer {
    /// A variable as the user's own tmux server sees it for their session:
    /// tmux refreshes `update-environment` variables (`SSH_CONNECTION`,
    /// `DISPLAY`, ...) there on every attach. Falls back to the server's
    /// global environment, then to this process's.
    fn environment(&self, name: &str) -> Option<String> {
        let session_value = match self {
            UserServer::Pmux { session } => show_environment(
                tmux_cmd(),
                &["-t", &crate::util::exact_session_target(session)],
                name,
            ),
            UserServer::Outer { socket, client } => {
                let session = outer_tmux(socket)
                    .args(["display-message", "-p", "-c", client, "#{session_id}"])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                    .filter(|id| !id.is_empty());
                session.and_then(|id| show_environment(outer_tmux(socket), &["-t", &id], name))
            }
        };
        let global_value = || {
            let cmd = match self {
                UserServer::Pmux { .. } => tmux_cmd(),
                UserServer::Outer { socket, .. } => outer_tmux(socket),
            };
            show_environment(cmd, &["-g"], name)
        };
        match session_value.or_else(global_value) {
            Some(EnvValue::Set(value)) => Some(value),
            Some(EnvValue::Removed) => None,
            None => std::env::var(name).ok().filter(|value| !value.is_empty()),
        }
    }
}

#[derive(Debug, PartialEq)]
enum EnvValue {
    Set(String),
    Removed,
}

fn show_environment(mut cmd: Command, scope: &[&str], name: &str) -> Option<EnvValue> {
    let output = cmd
        .arg("show-environment")
        .args(scope)
        .arg(name)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_show_environment(&String::from_utf8_lossy(&output.stdout), name)
}

/// tmux prints `NAME=value` for a set variable and `-NAME` for one removed
/// from this scope (for a session: the attaching client did not have it).
fn parse_show_environment(output: &str, name: &str) -> Option<EnvValue> {
    let line = output.lines().next()?.trim_end();
    if line == format!("-{name}") {
        return Some(EnvValue::Removed);
    }
    let value = line.strip_prefix(name)?.strip_prefix('=')?;
    Some(if value.is_empty() {
        EnvValue::Removed
    } else {
        EnvValue::Set(value.to_string())
    })
}

fn outer_tmux(socket: &str) -> Command {
    let mut cmd = tmux_command();
    cmd.arg("-S").arg(socket);
    cmd
}

/// The outer tmux client looking at the pmux client `client_tty`: a pmux
/// client runs in an outer pane, so its tty is that pane's tty.
fn outer_client_for(socket: &str, client_tty: &str) -> Option<String> {
    let query = |args: &[&str]| {
        outer_tmux(socket)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let panes = query(&["list-panes", "-a", "-F", "#{pane_tty}\t#{session_id}"])?;
    let clients = query(&[
        "list-clients",
        "-F",
        "#{client_name}\t#{session_id}\t#{client_activity}",
    ])?;
    pick_outer_client(&panes, &clients, client_tty)
}

/// From `list-panes -a -F '#{pane_tty}\t#{session_id}'` and
/// `list-clients -F '#{client_name}\t#{session_id}\t#{client_activity}'`,
/// the most recently active client on the session holding the pane whose tty
/// is `client_tty`.
fn pick_outer_client(panes: &str, clients: &str, client_tty: &str) -> Option<String> {
    let session = panes.lines().find_map(|line| {
        let (tty, session) = line.split_once('\t')?;
        (tty == client_tty).then_some(session)
    })?;
    clients
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (name, client_session, activity) = (fields.next()?, fields.next()?, fields.next()?);
            (client_session == session).then(|| (activity.parse::<u64>().unwrap_or(0), name))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, name)| name.to_string())
}

/// Whether the user's terminal is on another machine: their tmux session was
/// attached over SSH. A local clipboard tool would fill this machine's
/// clipboard, not theirs.
fn is_remote(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some()
}

/// The command that puts its stdin on this machine's clipboard, if any.
fn local_clipboard_command(
    env: &dyn Fn(&str) -> Option<String>,
    os: &str,
    on_path: &dyn Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let argv = |words: &[&str]| Some(words.iter().map(|w| w.to_string()).collect());
    if os == "macos" {
        return on_path("pbcopy").then(|| argv(&["pbcopy"])).flatten();
    }
    if on_path("clip.exe") && (env("WSL_DISTRO_NAME").is_some() || env("WSL_INTEROP").is_some()) {
        return argv(&["clip.exe"]);
    }
    if env("WAYLAND_DISPLAY").is_some() && on_path("wl-copy") {
        return argv(&["wl-copy"]);
    }
    if env("DISPLAY").is_some() {
        if on_path("xclip") {
            return argv(&["xclip", "-selection", "clipboard"]);
        }
        if on_path("xsel") {
            return argv(&["xsel", "--clipboard", "--input"]);
        }
    }
    None
}

fn program_on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
                && std::fs::metadata(&candidate).is_ok_and(|meta| {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                })
        })
    })
}

/// Run `argv` with `text` on stdin and the user's display variables. X11 and
/// Wayland tools stay behind to own the selection, so only wait briefly.
fn pipe_to(argv: &[String], text: &str, env: &dyn Fn(&str) -> Option<String>) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(value) = env(name) {
            cmd.env(name, value);
        }
    }
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return written && status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    // Still running: a tool holding the selection. Its input was delivered.
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn picks_the_most_recent_client_on_the_session_showing_the_pmux_client() {
        let panes = "/dev/pts/3\t$1\n/dev/pts/47\t$2\n/dev/pts/9\t$2\n";
        let clients = "/dev/pts/1\t$1\t500\n/dev/pts/2\t$2\t100\n/dev/pts/5\t$2\t300\n";
        assert_eq!(
            pick_outer_client(panes, clients, "/dev/pts/47").as_deref(),
            Some("/dev/pts/5")
        );
        assert_eq!(
            pick_outer_client(panes, clients, "/dev/pts/3").as_deref(),
            Some("/dev/pts/1")
        );
        // Not shown in that tmux, or nobody looking at it.
        assert_eq!(pick_outer_client(panes, clients, "/dev/pts/88"), None);
        assert_eq!(pick_outer_client(panes, "", "/dev/pts/47"), None);
    }

    #[test]
    fn reads_set_removed_and_missing_variables() {
        assert_eq!(
            parse_show_environment("DISPLAY=:0\n", "DISPLAY"),
            Some(EnvValue::Set(":0".into()))
        );
        assert_eq!(
            parse_show_environment("SSH_CONNECTION=1.2.3.4 5 6.7.8.9 22\n", "SSH_CONNECTION"),
            Some(EnvValue::Set("1.2.3.4 5 6.7.8.9 22".into()))
        );
        assert_eq!(
            parse_show_environment("-SSH_CONNECTION\n", "SSH_CONNECTION"),
            Some(EnvValue::Removed)
        );
        assert_eq!(parse_show_environment("", "DISPLAY"), None);
        // A longer name sharing the prefix is not the variable.
        assert_eq!(parse_show_environment("DISPLAY_X=1\n", "DISPLAY"), None);
    }

    #[test]
    fn ssh_sessions_are_remote() {
        assert!(is_remote(&env_of(&[("SSH_CONNECTION", "a b c d")])));
        assert!(is_remote(&env_of(&[("SSH_TTY", "/dev/pts/1")])));
        assert!(!is_remote(&env_of(&[("DISPLAY", ":0")])));
    }

    #[test]
    fn chooses_the_platform_clipboard_tool() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let cmd = |env: &[(&str, &str)], os, on_path: &dyn Fn(&str) -> bool| {
            local_clipboard_command(&env_of(env), os, on_path).map(|argv| argv.join(" "))
        };
        assert_eq!(cmd(&[], "macos", &all).as_deref(), Some("pbcopy"));
        assert_eq!(cmd(&[], "macos", &none), None);
        assert_eq!(
            cmd(
                &[("WSL_DISTRO_NAME", "Ubuntu"), ("DISPLAY", ":0")],
                "linux",
                &all
            )
            .as_deref(),
            Some("clip.exe")
        );
        assert_eq!(
            cmd(
                &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
                "linux",
                &all
            )
            .as_deref(),
            Some("wl-copy")
        );
        assert_eq!(
            cmd(&[("DISPLAY", ":0")], "linux", &all).as_deref(),
            Some("xclip -selection clipboard")
        );
        assert_eq!(
            cmd(&[("DISPLAY", ":0")], "linux", &|p: &str| p == "xsel").as_deref(),
            Some("xsel --clipboard --input")
        );
        // A headless Linux box (a server you SSH into) has no local clipboard.
        assert_eq!(cmd(&[], "linux", &all), None);
        // clip.exe on PATH outside WSL is not this machine's clipboard tool.
        assert_eq!(cmd(&[], "linux", &|p: &str| p == "clip.exe"), None);
    }
}
