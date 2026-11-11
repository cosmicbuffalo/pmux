//! The key hints behind the `prefix ?` popup, read from the live server.
//!
//! A fixed cheat sheet goes stale the moment someone rebinds a key, so
//! `pmux key-hints` lists what the server really has bound instead. It
//! compares the prefix and root key tables with a config-free tmux server's:
//! a binding is shown when its command or its note differs from tmux's
//! default, so untouched defaults stay out of the way while pmux's keys and
//! anything the user bound appear.
//!
//! A binding is described by its note (`bind -N "group: text" ...`). The
//! group picks the section; `process`, `agents`, `copy`, and `pmux` are
//! pmux's own, `ai-auto` and `ai-attn` join Agents and are flagged when that
//! tool is not installed, and any other group gets a section of its own.
//! Bindings without a note show their command.

use crate::tmux::{tmux_cmd, tmux_command};
use crate::util::{display_width, truncate_col};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::process::Command;

/// A config-free server for tmux's default bindings. One fixed name keeps a
/// single socket file around rather than one per popup.
const DEFAULTS_SOCKET: &str = "pmux-key-defaults";

/// Key tables the hints cover. Root bindings fire without the prefix.
const TABLES: [&str; 2] = ["prefix", "root"];

/// pmux's keys in the order its cheat sheet has always used; other keys
/// follow in tmux's order.
const KEY_ORDER: [&str; 19] = [
    "s", "c", "C", "n", "p", "l", ",", "k", "x", "d", "a", "Tab", "I", "q", "[", "?", "/", "h", "r",
];

const WITHOUT_NOTE: &str = "Your keys";
const WITHOUT_PREFIX: &str = "Without the prefix";
const SECTION_ORDER: [&str; 3] = ["Processes", "Agents", "Scroll and copy"];
const LAST_SECTIONS: [&str; 3] = [WITHOUT_NOTE, WITHOUT_PREFIX, "pmux"];

/// Widest command shown for a binding that has no note.
const MAX_COMMAND_WIDTH: usize = 44;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    table: String,
    key: String,
    command: String,
    note: Option<String>,
}

/// One line of the popup: the keys that share a description.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hint {
    keys: Vec<String>,
    text: String,
    needs: Option<&'static str>,
}

/// Print the key hints for the pmux server, with ANSI color when `color`.
pub fn print_key_hints(color: bool) -> Result<()> {
    let live = read_bindings(&tmux_cmd)?;
    let defaults = read_bindings(&defaults_cmd)?;
    let prefix = tmux_output(&tmux_cmd, &["show-options", "-gv", "prefix"])
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "C-p".to_string());
    print!(
        "{}",
        render(&prefix, &sections(&live, &defaults, &tool_installed), color)
    );
    Ok(())
}

pub(crate) fn defaults_cmd() -> Command {
    let mut cmd = tmux_command();
    cmd.args([
        "-L",
        DEFAULTS_SOCKET,
        "-f",
        "/dev/null",
        "start-server",
        ";",
    ]);
    cmd
}

pub(crate) fn tmux_output(make: &dyn Fn() -> Command, args: &[&str]) -> Result<String> {
    let output = make().args(args).output()?;
    if !output.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_bindings(make: &dyn Fn() -> Command) -> Result<Vec<Binding>> {
    let mut bindings = Vec::new();
    for table in TABLES {
        let notes = parse_notes(&tmux_output(
            make,
            &["list-keys", "-N", "-P", "", "-T", table],
        )?);
        for (key, command) in parse_list_keys(&tmux_output(make, &["list-keys", "-T", table])?) {
            let note = notes.get(&key).cloned();
            bindings.push(Binding {
                table: table.to_string(),
                key,
                command,
                note,
            });
        }
    }
    Ok(bindings)
}

/// `(key, command)` pairs from `list-keys -T TABLE`, whose lines read
/// `bind-key [-r] -T TABLE KEY COMMAND...` with special keys escaped (`\;`).
fn parse_list_keys(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("bind-key")?.trim_start();
            let rest = rest.strip_prefix("-r").map_or(rest, str::trim_start);
            let rest = rest.strip_prefix("-T")?.trim_start();
            let (_table, rest) = rest.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (key, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let key = key.strip_prefix('\\').unwrap_or(key);
            Some((key.to_string(), command.trim().to_string()))
        })
        .collect()
}

/// Notes from `list-keys -N -P "" -T TABLE`, whose lines read `KEY  NOTE`
/// with the key unescaped. An empty `-P` stops tmux 3.8 from writing the
/// prefix key before each prefix-table key, though it still leaves a leading
/// space. Keys without a note are not listed.
pub(crate) fn parse_notes(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (key, note) = line.trim_start().split_once(char::is_whitespace)?;
            let note = note.trim();
            (!note.is_empty()).then(|| (key.to_string(), note.to_string()))
        })
        .collect()
}

/// The configured bindings, grouped into popup sections in display order.
fn sections(
    live: &[Binding],
    defaults: &[Binding],
    installed: &dyn Fn(&str) -> bool,
) -> Vec<(String, Vec<Hint>)> {
    let defaults: HashMap<(&str, &str), &Binding> = defaults
        .iter()
        .map(|binding| ((binding.table.as_str(), binding.key.as_str()), binding))
        .collect();
    let mut sections: Vec<(String, Vec<Hint>)> = Vec::new();
    let mut shown: Vec<&Binding> = live
        .iter()
        .filter(|binding| {
            defaults
                .get(&(binding.table.as_str(), binding.key.as_str()))
                .is_none_or(|default| {
                    default.command != binding.command || default.note != binding.note
                })
        })
        .collect();
    shown.sort_by_key(|binding| {
        KEY_ORDER
            .iter()
            .position(|key| *key == binding.key)
            .unwrap_or(KEY_ORDER.len())
    });
    for binding in shown {
        let (section, text, needs) = describe(binding, installed);
        let hints = match sections.iter_mut().find(|(name, _)| *name == section) {
            Some((_, hints)) => hints,
            None => {
                sections.push((section, Vec::new()));
                &mut sections.last_mut().expect("just pushed").1
            }
        };
        match hints.iter_mut().find(|hint| hint.text == text) {
            Some(hint) => hint.keys.push(binding.key.clone()),
            None => hints.push(Hint {
                keys: vec![binding.key.clone()],
                text,
                needs,
            }),
        }
    }
    sections.sort_by_key(|(name, _)| {
        let rank = |order: &[&str]| order.iter().position(|section| section == name);
        match (rank(&SECTION_ORDER), rank(&LAST_SECTIONS)) {
            (Some(first), _) => first,
            (None, Some(last)) => SECTION_ORDER.len() + 1 + last,
            (None, None) => SECTION_ORDER.len(),
        }
    });
    sections
}

/// The section, text, and missing tool for one binding.
fn describe(
    binding: &Binding,
    installed: &dyn Fn(&str) -> bool,
) -> (String, String, Option<&'static str>) {
    let (group, text) = match binding.note.as_deref() {
        Some(note) => match note.split_once(": ") {
            Some((group, text)) if is_group(group) => (Some(group), text.to_string()),
            _ => (None, note.to_string()),
        },
        None => (None, truncate_col(&binding.command, MAX_COMMAND_WIDTH)),
    };
    let needs = match group {
        Some("ai-auto") if !installed("ai-auto") => Some("ai-auto"),
        Some("ai-attn") if !installed("ai-attn") => Some("ai-attn"),
        _ => None,
    };
    let section = if binding.table == "root" {
        WITHOUT_PREFIX.to_string()
    } else {
        match group {
            Some("process" | "processes") => "Processes".to_string(),
            Some("agents" | "ai-auto" | "ai-attn") => "Agents".to_string(),
            Some("copy") => "Scroll and copy".to_string(),
            Some("pmux") => "pmux".to_string(),
            Some(other) => capitalize(other),
            None => WITHOUT_NOTE.to_string(),
        }
    };
    (section, text, needs)
}

/// A note's group is a short label, not the start of a sentence that happens
/// to contain a colon.
fn is_group(group: &str) -> bool {
    !group.is_empty()
        && group.len() <= 24
        && group
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn render(prefix: &str, sections: &[(String, Vec<Hint>)], color: bool) -> String {
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let heading = |text: &str| paint("1;38;5;117", text);
    let key = |text: &str| paint("38;5;117", text);
    let dim = |text: &str| paint("2", text);

    let labels: Vec<Vec<String>> = sections
        .iter()
        .map(|(_, hints)| hints.iter().map(|hint| hint.keys.join(", ")).collect())
        .collect();
    let key_width = labels
        .iter()
        .flatten()
        .map(|label| display_width(label))
        .max()
        .unwrap_or(1)
        .max(6);
    let title = "pmux keys";
    let prefix_label = format!("prefix {prefix}");
    let text_width = sections
        .iter()
        .flat_map(|(_, hints)| hints)
        .map(|hint| display_width(&hint.text))
        .max()
        .unwrap_or(0);
    let gap = (key_width + 3 + text_width)
        .saturating_sub(display_width(title) + display_width(&prefix_label))
        .max(4);

    let mut out = format!(
        " {}{}{} {}\n",
        heading(title),
        " ".repeat(gap),
        dim("prefix"),
        key(prefix)
    );
    for ((name, hints), labels) in sections.iter().zip(&labels) {
        out.push_str(&format!("\n {}\n", heading(name)));
        for (hint, label) in hints.iter().zip(labels) {
            let padding = " ".repeat(key_width - display_width(label) + 2);
            let keys = hint
                .keys
                .iter()
                .map(|part| key(part))
                .collect::<Vec<_>>()
                .join(&dim(", "));
            let needs = hint
                .needs
                .map(|tool| format!(" {}", dim(&format!("(needs {tool})"))))
                .unwrap_or_default();
            out.push_str(&format!("   {keys}{padding}{}{needs}\n", hint.text));
        }
    }
    out.push_str(&format!(
        "\n {} {} {}\n",
        dim("In your own tmux,"),
        key("<prefix> C-p"),
        dim("toggles the pmux sidebar.")
    ));
    out
}

fn tool_installed(tool: &str) -> bool {
    let name = match tool {
        "ai-auto" => std::env::var("AI_AUTO_BIN").unwrap_or_else(|_| tool.to_string()),
        _ => tool.to_string(),
    };
    if name.contains('/') {
        return std::path::Path::new(&name).is_file();
    }
    let home_bin = std::env::var_os("HOME")
        .map(|home| std::path::Path::new(&home).join(".local/bin"))
        .into_iter();
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(home_bin)
        .any(|dir| dir.join(&name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(table: &str, key: &str, command: &str, note: Option<&str>) -> Binding {
        Binding {
            table: table.to_string(),
            key: key.to_string(),
            command: command.to_string(),
            note: note.map(str::to_string),
        }
    }

    fn texts(sections: &[(String, Vec<Hint>)]) -> Vec<(String, Vec<(String, String)>)> {
        sections
            .iter()
            .map(|(name, hints)| {
                (
                    name.clone(),
                    hints
                        .iter()
                        .map(|hint| (hint.keys.join(", "), hint.text.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn parses_list_keys_lines_with_repeat_flags_and_escaped_keys() {
        let text = "bind-key    -T prefix \\;      last-pane\n\
                    bind-key -r -T prefix Up      select-pane -U\n\
                    bind-key    -T prefix C-p     send-prefix\n";
        assert_eq!(
            parse_list_keys(text),
            vec![
                (";".to_string(), "last-pane".to_string()),
                ("Up".to_string(), "select-pane -U".to_string()),
                ("C-p".to_string(), "send-prefix".to_string()),
            ]
        );
    }

    #[test]
    fn parses_notes_keyed_by_unescaped_key() {
        let notes =
            parse_notes(";       Move to the previously active pane\n Tab     ai-auto: toggle\n");
        assert_eq!(notes[";"], "Move to the previously active pane");
        assert_eq!(notes["Tab"], "ai-auto: toggle");
    }

    #[test]
    fn untouched_defaults_are_hidden_and_changes_are_shown() {
        let defaults = vec![
            binding(
                "prefix",
                "\"",
                "split-window",
                Some("Split window vertically"),
            ),
            binding(
                "prefix",
                "d",
                "detach-client",
                Some("Detach the current client"),
            ),
            binding("prefix", "x", "kill-pane", Some("Kill the active pane")),
            binding("prefix", "s", "choose-tree -Zs", Some("Choose a session")),
        ];
        let live = vec![
            // Untouched default.
            binding(
                "prefix",
                "\"",
                "split-window",
                Some("Split window vertically"),
            ),
            // Same command, pmux's note.
            binding("prefix", "d", "detach-client", Some("process: detach")),
            // Rebound by the user without a note.
            binding("prefix", "x", "display-message hi", None),
            // pmux's replacement for a default.
            binding(
                "prefix",
                "s",
                "run-shell pmux attach",
                Some("process: switch"),
            ),
            // New keys: a user note with and without a group.
            binding(
                "prefix",
                "g",
                "run-shell lazygit",
                Some("git: open lazygit"),
            ),
            binding("prefix", "G", "run-shell tig", Some("Open tig")),
            binding(
                "prefix",
                "?",
                "run-shell help",
                Some("pmux: show key hints"),
            ),
            binding(
                "prefix",
                "/",
                "run-shell help",
                Some("pmux: show key hints"),
            ),
            binding("root", "M-x", "kill-pane", None),
        ];
        let sections = sections(&live, &defaults, &|_| true);
        assert_eq!(
            texts(&sections),
            vec![
                (
                    "Processes".to_string(),
                    vec![
                        ("s".to_string(), "switch".to_string()),
                        ("d".to_string(), "detach".to_string()),
                    ]
                ),
                (
                    "Git".to_string(),
                    vec![("g".to_string(), "open lazygit".to_string())]
                ),
                (
                    "Your keys".to_string(),
                    vec![
                        ("x".to_string(), "display-message hi".to_string()),
                        ("G".to_string(), "Open tig".to_string()),
                    ]
                ),
                (
                    "Without the prefix".to_string(),
                    vec![("M-x".to_string(), "kill-pane".to_string())]
                ),
                (
                    "pmux".to_string(),
                    vec![("?, /".to_string(), "show key hints".to_string())]
                ),
            ]
        );
    }

    #[test]
    fn missing_addons_are_flagged_and_unbound_keys_disappear() {
        let live = vec![
            binding(
                "prefix",
                "Tab",
                "run-shell ai-auto",
                Some("ai-auto: toggle auto mode"),
            ),
            binding(
                "prefix",
                "a",
                "run-shell ai-attn",
                Some("ai-attn: clear attention"),
            ),
        ];
        let sections = sections(&live, &[], &|tool| tool == "ai-attn");
        assert_eq!(sections.len(), 1);
        let (name, hints) = &sections[0];
        assert_eq!(name, "Agents");
        let needs = |key: &str| {
            hints
                .iter()
                .find(|hint| hint.keys == [key])
                .map(|hint| hint.needs)
        };
        assert_eq!(needs("Tab"), Some(Some("ai-auto")));
        assert_eq!(needs("a"), Some(None));
        assert!(
            !hints
                .iter()
                .any(|hint| hint.keys.contains(&"q".to_string()))
        );
    }

    #[test]
    fn reads_a_real_server_and_hides_its_untouched_defaults() {
        // The defaults server's `-L` socket lives under TMUX_TMPDIR, which a
        // test holding ENV_LOCK may point at a directory it deletes when it
        // ends. Hold the lock so the environment is stable throughout.
        let _env = crate::EnvGuard::new();
        let Some(server) = crate::tmux::TestTmuxServer::start() else {
            return;
        };
        let output = server.source(concat!(
            include_str!("../templates/help-binding.conf"),
            "bind -N \"process: detach\" d detach-client\n",
            "bind -N \"git: open lazygit\" g display-popup -E lazygit\n",
            "bind \\; display-message semicolon\n",
            "unbind z\n",
        ));
        assert!(output.status.success(), "{output:?}");
        let live = read_bindings(&|| server.command()).unwrap();
        let defaults = read_bindings(&defaults_cmd).unwrap();
        let text = texts(&sections(&live, &defaults, &|_| true));
        let find = |section: &str| {
            text.iter()
                .find(|(name, _)| name == section)
                .map(|(_, hints)| hints.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            find("Processes"),
            vec![("d".to_string(), "detach".to_string())]
        );
        assert_eq!(
            find("Git"),
            vec![("g".to_string(), "open lazygit".to_string())]
        );
        assert_eq!(
            find("pmux"),
            vec![("?, /, h".to_string(), "show key hints".to_string())]
        );
        assert_eq!(
            find("Your keys"),
            vec![(";".to_string(), "display-message semicolon".to_string())]
        );
        // Untouched defaults such as `"` (split) and `c` (new window) stay out.
        assert_eq!(text.len(), 4, "{text:?}");
    }

    #[test]
    fn render_aligns_keys_and_marks_missing_tools() {
        let sections = vec![(
            "Agents".to_string(),
            vec![
                Hint {
                    keys: vec!["Tab".to_string()],
                    text: "toggle auto mode".to_string(),
                    needs: Some("ai-auto"),
                },
                Hint {
                    keys: vec!["?".to_string(), "/".to_string()],
                    text: "show key hints".to_string(),
                    needs: None,
                },
            ],
        )];
        let text = render("C-p", &sections, false);
        assert!(text.starts_with(" pmux keys"), "{text}");
        assert!(text.contains("prefix C-p"), "{text}");
        assert!(
            text.contains("   Tab     toggle auto mode (needs ai-auto)\n"),
            "{text}"
        );
        assert!(text.contains("   ?, /    show key hints\n"), "{text}");
        assert!(!text.contains('\x1b'));
        assert!(render("C-p", &sections, true).contains("\x1b[38;5;117mTab\x1b[0m"));
    }
}
