//! Publish each process's sidebar label to the pmux server's status bar.
//!
//! The sidebar labels a process with its user-chosen name or, for an
//! auto-named process, its agent's conversation title, drawn in the process's
//! tag color. The status bar cannot work either out itself: titles come from
//! the agent history catalog, and tag colors from the full process set. So
//! `pmux status-labels` writes both onto each session as tmux options, and
//! `status.conf` draws the same pill from them. tmux hooks re-run it whenever
//! the inputs change (see `templates/managed-tmux-lifecycle.conf`).

use crate::tmux::{list_sessions, output_indicates_missing_server, tmux_cmd};
use crate::util::exact_session_target;
use anyhow::{Result, anyhow};
use ratatui::style::Color;
use std::collections::HashMap;

/// The label text, already escaped for a tmux format.
pub(crate) const LABEL_OPTION: &str = "@pmux_status_label";
/// `1` when the label is the agent's title rather than a user-chosen name, so
/// the status bar draws it italic like the sidebar does.
pub(crate) const LABEL_DERIVED_OPTION: &str = "@pmux_status_label_derived";
/// The process's tag color, as a tmux color name.
pub(crate) const PILL_OPTION: &str = "@pmux_status_pill";

/// Widest label the status bar shows. Codex falls back to the first prompt,
/// which can run to a paragraph.
const MAX_LABEL_WIDTH: usize = 48;

/// What one session's options should say. `None` unsets the option.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StatusLabel {
    label: Option<String>,
    derived: Option<String>,
    pill: Option<String>,
}

/// Recompute every session's status label and write the ones that changed.
pub fn publish_status_labels() -> Result<()> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        return Ok(());
    }
    let snapshot = crate::api::build_state_snapshot(&sessions);
    let processes = crate::picker_processes(&snapshot);
    let colors = crate::render::assign_pill_colors(&processes);
    // History is best-effort, including successful reads that omit a title.
    let titles = live_agent_titles().ok();

    let current = current_labels()?;
    let mut args: Vec<String> = Vec::new();
    for process in &processes {
        let name = &process.process.name;
        let existing = current.get(name).cloned().unwrap_or_default();
        let mut desired = desired_label(
            name,
            titles
                .as_ref()
                .and_then(|titles| titles.get(name))
                .map(String::as_str),
            colors.get(name).copied(),
        );
        retain_missing_title(&mut desired, &existing);
        push_changes(&mut args, name, &existing, &desired);
    }
    if args.is_empty() {
        return Ok(());
    }

    let output = tmux_cmd().args(&args).output()?;
    if !output.status.success() && !output_indicates_missing_server(&output.stderr) {
        return Err(anyhow!(
            "failed to publish status labels: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // A label changed, most often because an agent's title just arrived: tell
    // the sidebars, which otherwise re-read titles only every
    // HISTORY_REFRESH_INTERVAL (and only while their window is visible).
    crate::state::touch_labels_changed();
    Ok(())
}

/// A missing title is not evidence that the conversation was renamed. History
/// discovery and identity reads can omit entries without returning an error
/// (for example while an agent rewrites a transcript or its store is offline).
/// Keep a previously published conversation title until a replacement arrives.
/// Only retain derived labels: renaming a user-named session to an auto name
/// must not leave the old user name behind.
fn retain_missing_title(desired: &mut StatusLabel, existing: &StatusLabel) {
    if desired.label.is_none() && existing.derived.as_deref() == Some("1") {
        desired.label = existing.label.clone();
        desired.derived = existing.derived.clone();
    }
}

/// The label a session should show, mirroring the sidebar's precedence: a
/// user-chosen name wins, an auto-named process borrows its agent's title,
/// and a process with neither shows no pill.
fn desired_label(name: &str, title: Option<&str>, color: Option<Color>) -> StatusLabel {
    let (label, derived) = match crate::display_session_name(name) {
        Some(user_name) => (Some(user_name), false),
        None => (title, true),
    };
    let label = label
        .map(|label| crate::truncate_col(label, MAX_LABEL_WIDTH))
        .map(|label| escape_format(&label));
    StatusLabel {
        derived: (label.is_some() && derived).then(|| "1".to_string()),
        label,
        pill: color.and_then(tmux_color),
    }
}

/// Append the `set-option` commands that move `existing` to `desired`,
/// separated by tmux's `;` so a single tmux invocation applies them all.
fn push_changes(args: &mut Vec<String>, name: &str, existing: &StatusLabel, desired: &StatusLabel) {
    let target = exact_session_target(name);
    for (option, old, new) in [
        (LABEL_OPTION, &existing.label, &desired.label),
        (LABEL_DERIVED_OPTION, &existing.derived, &desired.derived),
        (PILL_OPTION, &existing.pill, &desired.pill),
    ] {
        if old == new {
            continue;
        }
        if !args.is_empty() {
            args.push(";".to_string());
        }
        match new {
            Some(value) => {
                args.extend(["set-option", "-t", &target, option, value].map(str::to_string))
            }
            None => args.extend(["set-option", "-u", "-t", &target, option].map(str::to_string)),
        }
    }
}

/// Each running process's agent title, keyed by process name, the same way
/// the sidebar derives them.
fn live_agent_titles() -> Result<HashMap<String, String>> {
    let query = crate::HistoryQuery {
        include_live: true,
        ..crate::HistoryQuery::default()
    };
    Ok(crate::history(&query)?
        .into_iter()
        .filter_map(|entry| Some((entry.pmux_process?, entry.title?)))
        .collect())
}

/// The status label options as each session has them now.
fn current_labels() -> Result<HashMap<String, StatusLabel>> {
    let format = format!(
        "#{{session_name}}\t#{{{LABEL_OPTION}}}\t#{{{LABEL_DERIVED_OPTION}}}\t#{{{PILL_OPTION}}}"
    );
    let output = tmux_cmd().args(["list-sessions", "-F", &format]).output()?;
    if !output.status.success() {
        if output_indicates_missing_server(&output.stderr) {
            return Ok(HashMap::new());
        }
        return Err(anyhow!(
            "failed to read status labels: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_current_labels(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_current_labels(listing: &str) -> HashMap<String, StatusLabel> {
    let non_empty = |field: Option<&str>| field.filter(|f| !f.is_empty()).map(str::to_string);
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_string();
            // Option values come back verbatim, `##` escapes included, so
            // they compare equal to what was written.
            let label = non_empty(fields.next());
            let derived = non_empty(fields.next());
            let pill = non_empty(fields.next());
            Some((
                name,
                StatusLabel {
                    label,
                    derived,
                    pill,
                },
            ))
        })
        .collect()
}

/// Make arbitrary text safe to print from a status format: tmux would read
/// `#[...]` in a title as a style change, and `##` prints a literal `#`.
fn escape_format(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('#', "##")
}

/// A tag color as tmux spells it. The palette is all xterm-256 indexes.
fn tmux_color(color: Color) -> Option<String> {
    match color {
        Color::Indexed(index) => Some(format!("colour{index}")),
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: &str = "1790869355-vurcl60h";

    #[test]
    fn auto_named_process_shows_its_agent_title_marked_derived() {
        let label = desired_label(AUTO, Some("Fix the relay"), Some(Color::Indexed(210)));
        assert_eq!(label.label.as_deref(), Some("Fix the relay"));
        assert_eq!(label.derived.as_deref(), Some("1"));
        assert_eq!(label.pill.as_deref(), Some("colour210"));
    }

    #[test]
    fn user_name_wins_over_the_agent_title() {
        let label = desired_label("web", Some("Fix the relay"), Some(Color::Indexed(114)));
        assert_eq!(label.label.as_deref(), Some("web"));
        assert_eq!(label.derived, None);
    }

    #[test]
    fn auto_named_process_without_a_title_has_no_label() {
        let label = desired_label(AUTO, None, Some(Color::Indexed(80)));
        assert_eq!(label.label, None);
        assert_eq!(label.derived, None);
        assert_eq!(label.pill.as_deref(), Some("colour80"));
    }

    #[test]
    fn missing_history_retains_title_but_updates_color() {
        let existing = desired_label(AUTO, Some("Keep this title"), Some(Color::Indexed(80)));
        let mut desired = desired_label(AUTO, None, Some(Color::Indexed(114)));
        retain_missing_title(&mut desired, &existing);
        assert_eq!(desired.label, existing.label);
        assert_eq!(desired.derived.as_deref(), Some("1"));
        assert_eq!(desired.pill.as_deref(), Some("colour114"));
    }

    #[test]
    fn new_titles_and_explicit_renames_replace_retained_titles() {
        let existing = desired_label(AUTO, Some("Old title"), None);
        for (name, title, expected) in [
            (AUTO, Some("New title"), "New title"),
            ("renamed", None, "renamed"),
        ] {
            let mut desired = desired_label(name, title, None);
            retain_missing_title(&mut desired, &existing);
            assert_eq!(desired.label.as_deref(), Some(expected));
        }

        let mut desired = desired_label(AUTO, None, None);
        retain_missing_title(&mut desired, &desired_label("old user name", None, None));
        assert_eq!(desired.label, None);
    }

    #[test]
    fn titles_cannot_inject_tmux_styles_and_stay_short() {
        let label = desired_label(AUTO, Some("a #[fg=red]b\u{1b}c"), None);
        assert_eq!(label.label.as_deref(), Some("a ##[fg=red]bc"));

        let long = "word ".repeat(30);
        let label = desired_label(AUTO, Some(&long), None).label.unwrap();
        assert!(crate::display_width(&label) <= MAX_LABEL_WIDTH);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn only_changed_options_are_written_in_one_command() {
        let existing = StatusLabel {
            label: Some("Old".into()),
            derived: Some("1".into()),
            pill: Some("colour210".into()),
        };
        let desired = StatusLabel {
            label: Some("New".into()),
            derived: Some("1".into()),
            pill: None,
        };
        let mut args = Vec::new();
        push_changes(&mut args, AUTO, &existing, &existing);
        assert!(args.is_empty());

        push_changes(&mut args, AUTO, &existing, &desired);
        let target = format!("={AUTO}:");
        assert_eq!(
            args,
            [
                "set-option",
                "-t",
                &target,
                LABEL_OPTION,
                "New",
                ";",
                "set-option",
                "-u",
                "-t",
                &target,
                PILL_OPTION,
            ]
        );
    }

    #[test]
    fn current_labels_read_back_escaped_text_verbatim() {
        let listing = format!("{AUTO}\ta ##[x]\t1\tcolour80\nweb\t\t\t\n");
        let labels = parse_current_labels(&listing);
        assert_eq!(labels[AUTO].label.as_deref(), Some("a ##[x]"));
        assert_eq!(labels[AUTO].pill.as_deref(), Some("colour80"));
        assert_eq!(labels["web"], StatusLabel::default());
    }
}
