//! `pmux restart --repair`: put pmux's dedicated tmux server back to a clean state
//! without touching its processes.
//!
//! The usual way the server gets broken is a tmux config meant for the user's
//! own tmux being loaded into it — `tmux source-file ~/.tmux.conf` typed in a
//! pmux pane, or an agent running `tmux set -g …` there, since `$TMUX` inside
//! a pmux pane names pmux's server. A soft restart only re-sources pmux's
//! config, so everything the foreign config added (its prefix, bindings,
//! hooks, plugin options) stays behind. Repair first returns the server to
//! tmux's own defaults: every key table is emptied and refilled with tmux's
//! default bindings (read, with their notes, from a config-free server),
//! every global hook is removed, and every global option is unset. Then it
//! runs the normal soft restart, which reloads pmux's config and re-applies
//! what pmux sets at runtime. Sessions, panes, and per-pane state are left
//! alone, so every process keeps running.

use crate::key_hints::{defaults_cmd, parse_notes, tmux_output};
use crate::tmux::{pmux_server_running, tmux_cmd, tmux_quote};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::process::{Command, Stdio};

/// Option scopes `show-options` and `set-option` share: server, session,
/// window, and pane.
const OPTION_SCOPES: [&str; 4] = ["-s", "-g", "-gw", "-gp"];
/// Hook scopes: session hooks, then window and pane hooks.
const HOOK_SCOPES: [&str; 3] = ["-g", "-gw", "-gp"];

#[derive(Debug, PartialEq, Eq)]
pub enum RepairResult {
    Repaired,
    NoServer,
}

/// Reset pmux's server to tmux's defaults, then reload pmux's config.
pub fn repair() -> Result<RepairResult> {
    if !pmux_server_running()? {
        return Ok(RepairResult::NoServer);
    }
    let script = reset_script(&tmux_cmd)?;
    source(&tmux_cmd, &script)?;
    crate::restart(false)?;
    Ok(RepairResult::Repaired)
}

/// The tmux commands that return the server behind `live` to tmux's defaults.
fn reset_script(live: &dyn Fn() -> Command) -> Result<String> {
    let mut script = String::new();

    let tables: BTreeSet<String> = binding_lines(&tmux_output(live, &["list-keys"])?)
        .map(|binding| binding.table.to_string())
        .collect();
    for table in &tables {
        script.push_str(&format!("unbind-key -a -T {}\n", tmux_quote(table)));
    }

    for scope in HOOK_SCOPES {
        let hooks = tmux_output(live, &["show-hooks", scope]).unwrap_or_default();
        for hook in set_hooks(&hooks) {
            script.push_str(&format!("set-hook {scope}u {hook}\n"));
        }
    }

    for scope in OPTION_SCOPES {
        let options = tmux_output(live, &["show-options", scope]).unwrap_or_default();
        for option in option_names(&options) {
            script.push_str(&format!("set-option {scope}qu {option}\n"));
        }
    }

    script.push_str(&default_bindings()?);
    Ok(script)
}

/// tmux's default bindings as `bind-key` commands, notes included, so the
/// key hints popup still recognizes them as untouched defaults.
fn default_bindings() -> Result<String> {
    let listing = tmux_output(&defaults_cmd, &["list-keys"])?;
    let mut notes: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut out = String::new();
    for binding in binding_lines(&listing) {
        let table_notes = notes.entry(binding.table.to_string()).or_insert_with(|| {
            tmux_output(
                &defaults_cmd,
                &["list-keys", "-N", "-P", "", "-T", binding.table],
            )
            .map(|text| parse_notes(&text))
            .unwrap_or_default()
        });
        match table_notes.get(&binding.key) {
            Some(note) => out.push_str(&format!(
                "bind-key -N {} {}\n",
                tmux_quote(note),
                binding.rest
            )),
            None => out.push_str(&format!("bind-key {}\n", binding.rest)),
        }
    }
    Ok(out)
}

fn source(make: &dyn Fn() -> Command, script: &str) -> Result<()> {
    let mut child = make()
        .args(["source-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run tmux source-file")?;
    child
        .stdin
        .take()
        .context("tmux source-file stdin")?
        .write_all(script.as_bytes())
        .context("failed to send the reset commands to tmux")?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "tmux could not reset pmux's server: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// One `list-keys` line: `bind-key [-r] -T TABLE KEY COMMAND...`.
struct BindingLine<'a> {
    table: &'a str,
    /// The key with tmux's escaping removed, as `list-keys -N` prints it.
    key: String,
    /// Everything after `bind-key`, still valid tmux command syntax.
    rest: &'a str,
}

fn binding_lines(text: &str) -> impl Iterator<Item = BindingLine<'_>> {
    text.lines().filter_map(|line| {
        let rest = line.strip_prefix("bind-key")?.trim_start();
        let mut words = rest.split_whitespace();
        let mut word = words.next()?;
        if word == "-r" {
            word = words.next()?;
        }
        if word != "-T" {
            return None;
        }
        let table = words.next()?;
        let key = words.next()?;
        Some(BindingLine {
            table,
            key: key.strip_prefix('\\').unwrap_or(key).to_string(),
            rest,
        })
    })
}

/// Hooks that have a command, as `name[index]`, from `show-hooks`. Hooks
/// with nothing set print a bare name and are skipped.
fn set_hooks(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.split_once(' ').map(|(name, _)| name))
        .filter(|name| !name.is_empty())
        .collect()
}

/// Option names from `show-options`, with array indexes dropped so one unset
/// clears the whole array.
fn option_names(text: &str) -> BTreeSet<&str> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| name.split_once('[').map_or(name, |(base, _)| base))
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::TestTmuxServer;

    #[test]
    fn parses_bindings_hooks_and_options() {
        let keys = "bind-key    -T prefix \\;      last-pane\n\
                    bind-key -r -T prefix Up      select-pane -U\n\
                    bind-key    -T copy-mode-vi v send-keys -X begin-selection\n";
        let parsed: Vec<_> = binding_lines(keys)
            .map(|b| (b.table, b.key, b.rest))
            .collect();
        assert_eq!(
            parsed,
            vec![
                ("prefix", ";".to_string(), "-T prefix \\;      last-pane"),
                (
                    "prefix",
                    "Up".to_string(),
                    "-r -T prefix Up      select-pane -U"
                ),
                (
                    "copy-mode-vi",
                    "v".to_string(),
                    "-T copy-mode-vi v send-keys -X begin-selection"
                ),
            ]
        );
        assert_eq!(
            set_hooks("after-new-window[0] display x\nalert-bell\npane-exited[3] y\n"),
            vec!["after-new-window[0]", "pane-exited[3]"]
        );
        let options = option_names("@mine 1\nterminal-features[0] a\nterminal-features[1] b\n");
        assert_eq!(
            options.into_iter().collect::<Vec<_>>(),
            ["@mine", "terminal-features"]
        );
    }

    /// A foreign config loaded into a server is fully undone, and the
    /// server's bindings match a config-free server's again.
    #[test]
    fn reset_undoes_a_foreign_config() {
        // The defaults server's `-L` socket lives under TMUX_TMPDIR, which a
        // test holding ENV_LOCK may point at a directory it deletes when it
        // ends. Hold the lock so the environment is stable throughout.
        let _env = crate::EnvGuard::new();
        let Some(server) = TestTmuxServer::start() else {
            return;
        };
        let defaults = |args: &[&str]| tmux_output(&defaults_cmd, args).unwrap();
        let live = |args: &[&str]| tmux_output(&|| server.command(), args).unwrap();
        let pristine_keys = defaults(&["list-keys"]);
        let pristine_notes = defaults(&["list-keys", "-N", "-P", "", "-T", "prefix"]);

        let output = server.source(concat!(
            "set -g prefix C-a\n",
            "unbind C-b\n",
            "bind C-a send-prefix\n",
            "bind r source-file ~/.tmux.conf\n",
            "bind -T copy-mode-vi y send-keys -X copy-selection\n",
            "bind -T custom x display-message custom\n",
            "unbind '\"'\n",
            "set -g @plugin 'tmux-plugins/tpm'\n",
            "set -ga terminal-features ',xterm*:RGB'\n",
            "setw -g mode-keys vi\n",
            "set -g status-left 'FOREIGN'\n",
            "set-hook -g after-new-window 'display foreign'\n",
            "set-hook -gw pane-focus-in 'display foreign'\n",
        ));
        assert!(output.status.success(), "{output:?}");
        assert_ne!(live(&["list-keys"]), pristine_keys);

        let script = reset_script(&|| server.command()).unwrap();
        source(&|| server.command(), &script).unwrap();

        assert_eq!(live(&["list-keys"]), pristine_keys);
        assert_eq!(
            live(&["list-keys", "-N", "-P", "", "-T", "prefix"]),
            pristine_notes
        );
        assert_eq!(live(&["show-options", "-gv", "prefix"]).trim(), "C-b");
        assert_eq!(live(&["show-options", "-gqv", "@plugin"]).trim(), "");
        assert_eq!(live(&["show-options", "-gwv", "mode-keys"]).trim(), "emacs");
        assert!(!live(&["show-options", "-gv", "status-left"]).contains("FOREIGN"));
        assert!(!live(&["show-hooks", "-g"]).contains("foreign"));
        assert!(!live(&["show-hooks", "-gw"]).contains("foreign"));
        // The server and its session survive.
        assert!(live(&["list-sessions"]).contains("base"));
    }
}
