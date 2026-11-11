//! `pmux doctor tmux`: check that pmux's dedicated tmux server and your own
//! tmux are each configured as themselves.
//!
//! The ways a two-server setup goes wrong are mostly invisible from inside
//! either server: a config loaded into the wrong one, two prefixes that
//! collide, a plugin loaded twice, or a server still running an older tmux
//! after an upgrade. This looks at pmux's server and every tmux server pmux
//! has seen hosting one of its processes (plus the one this runs in, or the
//! one named with --socket) and says what to do about each problem.

use crate::config::{server_conf_path, tmux_guard_dir};
use crate::tmux::{pmux_server_running, tmux_cmd, tmux_command};
use anyhow::Result;
use std::collections::BTreeSet;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Note,
    Warning,
    Problem,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Warning => "warning",
            Level::Problem => "problem",
        }
    }
}

struct Report {
    text: String,
    worst: Level,
}

impl Report {
    fn section(&mut self, title: &str) {
        self.text.push_str(&format!("\n{title}\n"));
    }

    fn line(&mut self, level: Level, message: &str, fix: Option<&str>) {
        self.text
            .push_str(&format!("  {:<8} {message}\n", level.label()));
        if let Some(fix) = fix {
            self.text.push_str(&format!("           fix: {fix}\n"));
        }
        if level as u8 > self.worst as u8 {
            self.worst = level;
        }
    }
}

fn output(mut cmd: Command, args: &[&str]) -> Option<String> {
    let out = cmd.args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn on_socket(socket: &str) -> Command {
    let mut cmd = tmux_command();
    cmd.arg("-S").arg(socket).env_remove("TMUX");
    cmd
}

/// The `tmux -V` version of the tmux command on PATH, as `3.5a`.
fn client_version() -> Option<String> {
    output(tmux_command(), &["-V"]).map(|v| v.trim_start_matches("tmux ").to_string())
}

/// Directories tmux-ai-attn's hooks start its sync loop from.
fn ai_attn_plugin_dirs(hooks: &str) -> BTreeSet<String> {
    hooks
        .split(['"', '\'', ' ', '\\'])
        .filter_map(|word| word.strip_suffix("/scripts/start-loop.sh"))
        .filter(|dir| dir.starts_with('/'))
        .map(str::to_string)
        .collect()
}

/// Run every check and render the report. The second value is true when a
/// problem was found.
pub fn doctor(extra_socket: Option<&str>) -> Result<(String, bool)> {
    let mut report = Report {
        text: "pmux tmux diagnostics\n---------------------\n".to_string(),
        worst: Level::Ok,
    };
    let client = client_version();
    let mut pmux_prefixes = Vec::new();

    report.section("pmux's dedicated tmux server");
    if !pmux_server_running()? {
        report.line(
            Level::Note,
            "not running; it starts with the first pmux process",
            None,
        );
    } else {
        let show = |option: &str| output(tmux_cmd(), &["show-options", "-gqv", option]);
        let version = output(tmux_cmd(), &["display-message", "-p", "#{version}"]);
        if show("@pmux_server").as_deref() == Some("1") {
            report.line(Level::Ok, "running pmux's config", None);
        } else {
            report.line(
                Level::Warning,
                "not marked as pmux's server (started by an older pmux)",
                Some("pmux restart"),
            );
        }
        if show("@pmux_foreign_config").as_deref() == Some("1") {
            report.line(
                Level::Problem,
                "key bindings from outside pmux's config were loaded into it",
                Some("pmux restart --repair (resets it to pmux's config; every process keeps running)"),
            );
        }
        for option in ["prefix", "prefix2"] {
            if let Some(key) = show(option).filter(|key| key != "None" && !key.is_empty()) {
                pmux_prefixes.push(key);
            }
        }
        let guard = tmux_guard_dir()?.display().to_string();
        let path = output(tmux_cmd(), &["show-environment", "-g", "PATH"]).unwrap_or_default();
        if path
            .strip_prefix("PATH=")
            .is_some_and(|path| path.split(':').next() == Some(guard.as_str()))
        {
            report.line(Level::Ok, "new processes get the tmux guard", None);
        } else {
            report.line(
                Level::Warning,
                "new processes do not get the tmux guard yet",
                Some("pmux restart (processes started earlier keep their old PATH)"),
            );
        }
        if let (Some(server), Some(client)) = (&version, &client)
            && server != client
        {
            report.line(
                Level::Warning,
                &format!("runs tmux {server}, but the tmux command is {client}"),
                Some("when convenient: pmux snapshot && pmux restart --hard --yes && pmux restore"),
            );
        }
        if !server_conf_path()?.exists() {
            report.line(
                Level::Problem,
                "~/.config/pmux/server.conf is missing",
                Some("pmux restart"),
            );
        }
    }

    let mut sockets: BTreeSet<String> = crate::state::known_host_sockets()?.into_iter().collect();
    if let Some(socket) = extra_socket {
        sockets.insert(socket.to_string());
    }
    if !crate::tmux::inside_pmux_client()
        && let Some(socket) = crate::tmux::current_tmux_socket()
    {
        sockets.insert(socket);
    }
    let pmux_socket = output(tmux_cmd(), &["display-message", "-p", "#{socket_path}"]);
    sockets.retain(|socket| Some(socket) != pmux_socket.as_ref());

    if sockets.is_empty() {
        report.section("your tmux");
        report.line(
            Level::Note,
            "pmux has not seen your tmux yet; run this inside it, or pass --socket",
            None,
        );
    }
    for socket in &sockets {
        report.section(&format!("your tmux ({socket})"));
        let Some(version) = output(on_socket(socket), &["display-message", "-p", "#{version}"])
        else {
            report.line(Level::Note, "not running", None);
            continue;
        };
        let show = |option: &str| output(on_socket(socket), &["show-options", "-gqv", option]);
        if show("@pmux_server").as_deref() == Some("1") {
            report.line(
                Level::Problem,
                "pmux's server config (server.conf) was loaded into this tmux",
                Some("reload your own tmux config, or restart this tmux server"),
            );
        }
        let prefixes: Vec<String> = ["prefix", "prefix2"]
            .iter()
            .filter_map(|option| show(option))
            .filter(|key| key != "None" && !key.is_empty())
            .collect();
        if let Some(shared) = prefixes.iter().find(|key| pmux_prefixes.contains(key)) {
            report.line(
                Level::Problem,
                &format!(
                    "uses {shared} as a prefix, like pmux, so pmux's prefix never reaches pmux"
                ),
                Some("set a different prefix in ~/.config/pmux/tmux.conf or in your tmux config"),
            );
        } else if !prefixes.is_empty() {
            report.line(
                Level::Ok,
                &format!(
                    "prefix {} does not collide with pmux's",
                    prefixes.join(", ")
                ),
                None,
            );
        }
        let keys = output(on_socket(socket), &["list-keys", "-T", "prefix"]).unwrap_or_default();
        if keys.contains("toggle-sidebar") {
            report.line(Level::Ok, "pmux's integration (sidebar) is loaded", None);
        } else {
            report.line(
                Level::Note,
                "pmux's integration is not loaded, so there is no sidebar",
                Some("add `source-file -q ~/.config/pmux/outer-tmux.conf` to your tmux config"),
            );
        }
        let hooks = output(on_socket(socket), &["show-hooks", "-g"]).unwrap_or_default();
        let plugins = ai_attn_plugin_dirs(&hooks);
        if plugins.len() > 1 {
            report.line(
                Level::Warning,
                &format!(
                    "tmux-ai-attn is loaded twice, from {}",
                    plugins.into_iter().collect::<Vec<_>>().join(" and ")
                ),
                Some(
                    "load it one way only (TPM or pmux's outer-tmux.conf), then restart this tmux server",
                ),
            );
        }
        if let Some(client) = &client
            && &version != client
        {
            report.line(
                Level::Note,
                &format!("runs tmux {version}, but the tmux command is {client}"),
                None,
            );
        }
    }

    let found_problem = report.worst == Level::Problem;
    report.text.push_str(match report.worst {
        Level::Problem => "\nProblems found; see the fixes above.\n",
        Level::Warning => "\nNo problems; see the warnings above.\n",
        _ => "\nAll good.\n",
    });
    Ok((report.text, found_problem))
}

#[cfg(test)]
mod tests {
    use super::ai_attn_plugin_dirs;

    #[test]
    fn finds_each_tmux_ai_attn_checkout_once() {
        let hooks = r#"session-created[0] run-shell -b "if [ -x \"/home/me/.config/pmux/plugins/tmux-ai-attn/scripts/start-loop.sh\" ]; then \"/home/me/.config/pmux/plugins/tmux-ai-attn/scripts/start-loop.sh\" x; fi"
client-attached[1] run-shell -b "if [ -x \"/home/me/.tmux/plugins/tmux-ai-attn/scripts/start-loop.sh\" ]; then :; fi"
after-new-window[0] display-message hi"#;
        let dirs: Vec<_> = ai_attn_plugin_dirs(hooks).into_iter().collect();
        assert_eq!(
            dirs,
            [
                "/home/me/.config/pmux/plugins/tmux-ai-attn",
                "/home/me/.tmux/plugins/tmux-ai-attn"
            ]
        );
    }
}
