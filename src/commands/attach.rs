//! Attaching clients to processes, process cycling, and outer host-pane tracking.

use crate::config::ensure_pmux_config;
use crate::relay::{ensure_ai_attn_relay, trigger_relay_sync};
use crate::session::SessionInfo;
use crate::state::{
    read_attach_history, read_last_attached, read_pane_last_process, read_session_host_pane,
    read_session_host_socket, record_attached_transition, write_pane_last_process,
    write_session_host_pane,
};
use crate::tmux::{
    current_tmux_socket, detect_current_pmux_session, inside_outer_tmux, inside_pmux_client,
    list_sessions, tmux_cmd,
};
use crate::util::{exact_session_target, find_pmux_session_in_tree};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::io::{self, IsTerminal};
use std::process::Stdio;

/// Attach to a pmux session by name, selector, or interactive picker. A past
/// agent session named by id is resumed in a new process instead.
/// Called by `pmux attach [target]`.
pub fn attach(target: Option<String>) -> Result<()> {
    ensure_pmux_config()?;
    let sessions = list_sessions()?;
    let selection = match target {
        // A name that is no running process can be a past agent session's
        // id (see `pmux history`), which is resumed in a new process.
        Some(t) => match resolve_attach_target(&sessions, &t) {
            Ok(session) => crate::attach_picker::PickerSelection::Attach(session.name.clone()),
            Err(err) => {
                let mut resumables =
                    crate::history(&crate::HistoryQuery::default()).unwrap_or_default();
                resumables.retain(crate::HistoryEntry::can_resume);
                match crate::history::find_entry(&resumables, &t) {
                    Ok(entry) => crate::attach_picker::PickerSelection::Resume(format!(
                        "{}:{}",
                        entry.agent, entry.session_id
                    )),
                    Err(_) if sessions.is_empty() && resumables.is_empty() => {
                        bail!("no running or resumable pmux processes found")
                    }
                    Err(_) => return Err(err),
                }
            }
        },
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!("no interactive terminal for the picker; run 'pmux attach <name>'");
            }
            crate::attach_picker::pick_session_tui(&sessions)?
        }
    };
    match selection {
        crate::attach_picker::PickerSelection::Attach(name) => attach_by_name(&name),
        crate::attach_picker::PickerSelection::Resume(target) => {
            // Picked by a person, so not spawned by the pane's process.
            crate::resume(&target, None, false, &crate::ParentSpec::None).map(|_| ())
        }
    }
}

/// Switch to the previous pmux session for the current pane, or the last attached session.
/// Called by `pmux last` and the `l` keybinding.
pub fn last() -> Result<()> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        bail!("no running pmux processes found");
    }
    let current = detect_current_pmux_session(&sessions).ok();
    let pane_id = current
        .as_deref()
        .map(read_session_host_pane)
        .transpose()?
        .flatten()
        .or_else(|| {
            env::var("TMUX_PANE")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let target = fallback_last_target(&sessions, current.as_deref(), pane_id.as_deref())?;
    attach_by_name(&target)
}

/// Cycle to the next pmux session by creation order. Called by `pmux next-process`.
pub fn next_from(current: Option<String>, client: Option<String>) -> Result<()> {
    jump_relative(1, current, client)
}

/// Cycle to the previous pmux session by creation order. Called by `pmux prev-process`.
pub fn prev_from(current: Option<String>, client: Option<String>) -> Result<()> {
    jump_relative(-1, current, client)
}

// Cycle to the next (+1) or previous (-1) session by creation order with wrap-around.
// Shared implementation for next_from and prev_from.
fn jump_relative(
    direction: i64,
    explicit_current: Option<String>,
    explicit_client: Option<String>,
) -> Result<()> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        bail!("no running pmux processes found");
    }

    let current_name = explicit_current
        .clone()
        .or_else(|| detect_current_pmux_session(&sessions).ok())
        .or(read_last_attached()?)
        .ok_or_else(|| {
            anyhow!("could not determine current pmux process; use 'pmux attach' first")
        })?;

    let ordered = ordered_creation_sessions(sessions);

    let idx = ordered
        .iter()
        .position(|s| s.name == current_name)
        .ok_or_else(|| anyhow!("current process '{}' is no longer running", current_name))?;

    let len = ordered.len() as i64;
    let next_idx = (idx as i64 + direction).rem_euclid(len) as usize;
    attach_target(
        &ordered[next_idx].name,
        explicit_client.as_deref(),
        Some(current_name.as_str()),
    )
}

// Attach to a session, using client-targeted switch if a client is specified.
pub(super) fn attach_target(
    name: &str,
    explicit_client: Option<&str>,
    current_session: Option<&str>,
) -> Result<()> {
    if let Some(client) = explicit_client {
        return attach_for_client(name, client, current_session);
    }
    attach_by_name(name)
}

/// Update the host-pane mapping for a pmux session visible in an outer tmux pane.
///
/// Walks the process tree of the outer pane to find a `tmux -L pmux ... attach-session -t <name>`
/// process, then updates the host_pane file and triggers a relay sync.
pub fn update_host_pane(pane_id: &str, socket: Option<&str>) -> Result<()> {
    match update_host_pane_inner(pane_id, socket)? {
        HostPaneOutcome::Updated => Ok(()),
        HostPaneOutcome::NoSession => {
            Err(anyhow!("no pmux attach-session found in pane process tree"))
        }
    }
}

/// Single entry point for the outer `pane-focus-in` hook: keep the relay's
/// host-pane mapping current for the focused pane, then nudge that window's
/// sidebar to re-check focus immediately. Both steps are best-effort — focusing
/// a non-pmux pane (no session to map) is normal and must not fail the hook, and
/// a window without a sidebar simply has nothing to nudge.
pub fn on_focus(pane_id: &str, window_id: &str, socket: &str) -> Result<()> {
    // A focused pane that hosts no pmux session is the common case (any ordinary
    // shell): `update_host_pane_inner` reports that as `Ok(NoSession)`, so `?`
    // here surfaces only genuine tmux failures, never the benign skip.
    update_host_pane_inner(pane_id, Some(socket))?;
    crate::sidebar::nudge_window_sidebars(window_id, socket)
}

/// Outcome of attempting to refresh a single pane's host-pane mapping.
enum HostPaneOutcome {
    /// The pane hosts a pmux session and its mapping was written.
    Updated,
    /// The pane has no attached pmux session in its process tree (benign skip).
    NoSession,
}

/// Core of [`update_host_pane`]: distinguishes a benign "no pmux session here"
/// skip from a genuine error, so batch callers can report the two separately.
fn update_host_pane_inner(pane_id: &str, socket: Option<&str>) -> Result<HostPaneOutcome> {
    let mut cmd = crate::tmux::tmux_command();
    if let Some(sock) = socket {
        cmd.arg("-S").arg(sock);
    }
    let output = cmd
        .arg("display-message")
        .arg("-t")
        .arg(pane_id)
        .arg("-p")
        .arg("#{pane_pid}")
        .output()
        .context("failed to query outer pane PID")?;
    if !output.status.success() {
        bail!("outer tmux display-message failed");
    }
    let pane_pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("invalid pane PID")?;

    let Some(session_name) = find_pmux_session_in_tree(pane_pid) else {
        return Ok(HostPaneOutcome::NoSession);
    };

    // `socket` is the outer tmux server hosting this pane (passed by the
    // pane-focus-in hook as #{socket_path}); persist it so the relay can scope
    // the record to that server.
    write_session_host_pane(&session_name, pane_id, socket)?;
    trigger_relay_sync();
    Ok(HostPaneOutcome::Updated)
}

/// Summary of a [`update_all_host_panes`] sweep.
pub struct HostPaneRefresh {
    /// Panes whose host-pane mapping was (re)written.
    pub updated: usize,
    /// Panes skipped because they host no pmux session.
    pub skipped: usize,
    /// Per-pane failures (`"<pane_id>: <error>"`) that were not benign skips.
    pub errors: Vec<String>,
}

/// Refresh host-pane mappings for every pane on the given outer tmux socket.
///
/// Panes that do not contain an attached pmux session are skipped. This is
/// intended for diagnostics and recovery after moving panes between windows or
/// after installing socket-scoped relay support on an existing tmux server.
pub fn update_all_host_panes(socket: &str) -> Result<HostPaneRefresh> {
    let output = crate::tmux::tmux_command()
        .arg("-S")
        .arg(socket)
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg("#{pane_id}")
        .output()
        .context("failed to list outer tmux panes")?;
    if !output.status.success() {
        bail!("outer tmux list-panes failed");
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut refresh = HostPaneRefresh {
        updated: 0,
        skipped: 0,
        errors: Vec::new(),
    };
    for pane_id in raw.lines().map(str::trim).filter(|pane| !pane.is_empty()) {
        match update_host_pane_inner(pane_id, Some(socket)) {
            Ok(HostPaneOutcome::Updated) => refresh.updated += 1,
            Ok(HostPaneOutcome::NoSession) => refresh.skipped += 1,
            Err(err) => refresh.errors.push(format!("{pane_id}: {err}")),
        }
    }
    Ok(refresh)
}

// ---------------------------------------------------------------------------
// Attachment & session resolution logic (moved from tmux.rs for clarity)
// ---------------------------------------------------------------------------
// Attach to a pmux session by name, handling switch-client if already inside pmux,
// or attach-session if called from outside. Records attachment history and host-pane state.
pub(crate) fn attach_by_name(name: &str) -> Result<()> {
    attach_by_name_inner(name, false)
}

pub(super) fn attach_by_name_inner(name: &str, suppress_terminal_errors: bool) -> Result<()> {
    if let Err(error) = crate::queue::ensure_existing_worker() {
        eprintln!("pmux: could not restart message scheduler: {error}");
    }
    let outer_pane_id = env::var("TMUX_PANE")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut current_session = None;
    let mut host_pane = outer_pane_id.clone();
    // The tmux server that owns host_pane. When invoked from an outer tmux this
    // is the server we're running under ($TMUX). When switching *within* a pmux
    // client $TMUX is the pmux backend — never the outer server that owns the
    // host pane — so we must not fall back to it; the real outer socket comes
    // from the session we're leaving (or stays unset, i.e. legacy unsocketed).
    let mut host_socket = if inside_pmux_client() {
        None
    } else {
        current_tmux_socket()
    };
    if inside_pmux_client()
        && let Ok(known_sessions) = list_sessions()
        && let Ok(current_name) = detect_current_pmux_session(&known_sessions)
    {
        host_pane = read_session_host_pane(&current_name)?.or(outer_pane_id.clone());
        host_socket = read_session_host_socket(&current_name)?.or(host_socket);
        current_session = Some(current_name);
        if let Some(ref pane_id) = host_pane {
            write_session_host_pane(name, pane_id, host_socket.as_deref())?;
            try_ensure_relay();
        }
        let switch_status = tmux_cmd()
            .arg("switch-client")
            .arg("-t")
            .arg(exact_session_target(name))
            .status()
            .context("failed to execute tmux switch-client")?;
        if switch_status.success() {
            if let (Some(pane_id), Some(previous)) = (host_pane.as_ref(), current_session.as_ref())
            {
                write_pane_last_process(pane_id, previous)?;
            }
            trigger_relay_sync();
            return record_attached_transition(name, current_session.as_deref());
        }
    }

    if let Some(ref pane_id) = host_pane {
        write_session_host_pane(name, pane_id, host_socket.as_deref())?;
        try_ensure_relay();
        trigger_relay_sync();
    }

    let mut cmd = tmux_cmd();
    cmd.env_remove("TMUX");
    cmd.arg("attach-session")
        .arg("-t")
        .arg(exact_session_target(name))
        .arg("-f")
        .arg("active-pane,ignore-size");
    if suppress_terminal_errors {
        cmd.stderr(Stdio::null());
    }

    let status = cmd
        .status()
        .context("failed to execute tmux attach-session")?;
    if !status.success() {
        bail!("failed to attach to pmux process '{}'", name);
    }

    if let (Some(pane_id), Some(previous)) = (host_pane.as_ref(), current_session.as_ref()) {
        write_pane_last_process(pane_id, previous)?;
    }
    record_attached_transition(name, current_session.as_deref())
}

// Switch a specific tmux client to a different pmux session.
// Used by jump_relative and kill_current_and_next when a client TTY is known.
pub(crate) fn attach_for_client(
    name: &str,
    target_client: &str,
    current_session: Option<&str>,
) -> Result<()> {
    attach_for_client_with(name, current_session, || {
        tmux_cmd()
            .arg("switch-client")
            .arg("-c")
            .arg(target_client)
            .arg("-t")
            .arg(exact_session_target(name))
            .status()
            .context("failed to execute tmux switch-client for target client")
            .map(|status| status.success())
    })
}

fn attach_for_client_with<F>(
    name: &str,
    current_session: Option<&str>,
    switch_client: F,
) -> Result<()>
where
    F: FnOnce() -> Result<bool>,
{
    let host_mapping = current_session
        .map(|current| {
            Ok::<_, anyhow::Error>((
                read_session_host_pane(current)?,
                read_session_host_socket(current)?,
            ))
        })
        .transpose()?;
    if !switch_client()? {
        bail!("failed to switch target client to pmux process '{}'", name);
    }

    if let Some((Some(pane_id), socket)) = &host_mapping {
        write_session_host_pane(name, pane_id, socket.as_deref())?;
        try_ensure_relay();
    }
    if let (Some((Some(pane_id), _)), Some(previous)) = (&host_mapping, current_session) {
        write_pane_last_process(pane_id, previous)?;
    }
    trigger_relay_sync();
    record_attached_transition(name, current_session)
}

// Resolve a target string (name, "newest", or "oldest") to a SessionInfo.
// Used by attach, kill, and other commands that accept a target argument.
pub(crate) fn resolve_attach_target<'a>(
    sessions: &'a [SessionInfo],
    target: &str,
) -> Result<&'a SessionInfo> {
    // An exact session name wins, so a process literally named `newest` or
    // `oldest` stays addressable; the selectors apply only otherwise.
    if let Some(session) = sessions.iter().find(|s| s.name == target) {
        return Ok(session);
    }

    if target.eq_ignore_ascii_case("newest") {
        return sessions
            .iter()
            .max_by(|a, b| {
                a.created_epoch
                    .cmp(&b.created_epoch)
                    .then(a.name.cmp(&b.name))
            })
            .ok_or_else(|| anyhow!("no processes available"));
    }

    if target.eq_ignore_ascii_case("oldest") {
        return sessions
            .iter()
            .min_by(|a, b| {
                a.created_epoch
                    .cmp(&b.created_epoch)
                    .then(a.name.cmp(&b.name))
            })
            .ok_or_else(|| anyhow!("no processes available"));
    }

    sessions
        .iter()
        .find(|s| s.name == target)
        .ok_or_else(|| anyhow!("process '{}' not found", target))
}

// Sort sessions by creation time ascending (ties broken by name). Used for cycling.
pub(crate) fn ordered_creation_sessions(mut sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
    sessions.sort_by(|a, b| {
        a.created_epoch
            .cmp(&b.created_epoch)
            .then_with(|| a.name.cmp(&b.name))
    });
    sessions
}

// Determine the best "last" session to switch to: pane-local history, then
// global attach history, then newest session. Used by the `last` command.
pub(crate) fn fallback_last_target(
    sessions: &[SessionInfo],
    current: Option<&str>,
    pane_id: Option<&str>,
) -> Result<String> {
    if let Some(pane_id) = pane_id
        && let Some(name) = read_pane_last_process(pane_id)?
        && current != Some(name.as_str())
        && sessions.iter().any(|s| s.name == name)
    {
        return Ok(name);
    }

    for name in read_attach_history()? {
        if current.is_some_and(|current_name| current_name == name) {
            continue;
        }
        if sessions.iter().any(|s| s.name == name) {
            return Ok(name);
        }
    }

    if let Some(name) = read_last_attached()?
        && current != Some(name.as_str())
        && sessions.iter().any(|s| s.name == name)
    {
        return Ok(name);
    }

    sessions
        .iter()
        .filter(|s| current != Some(s.name.as_str()))
        .max_by(|a, b| {
            a.created_epoch
                .cmp(&b.created_epoch)
                .then(a.name.cmp(&b.name))
        })
        .map(|s| s.name.clone())
        .or_else(|| current.map(str::to_string))
        .ok_or_else(|| anyhow!("no running pmux sessions found"))
}

// Start the ai-attn relay daemon if running inside an outer tmux session.
// Called after spawn and attach operations.
pub(super) fn try_ensure_relay() {
    if inside_outer_tmux()
        && let Err(e) = ensure_ai_attn_relay()
    {
        eprintln!("pmux: failed to start ai-attn relay: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionInfo;
    use crate::state::{
        read_session_host_pane, read_session_host_socket, record_attached_transition,
        write_pane_last_process, write_session_host_pane,
    };
    use crate::tmux::inside_pmux_client;

    use crate::commands::test_support::with_temp_home;

    #[test]
    fn failed_client_switch_does_not_change_host_mapping() {
        with_temp_home(|_| {
            write_session_host_pane("current", "%9", Some("/tmp/outer")).unwrap();

            let result = attach_for_client_with("target", Some("current"), || Ok(false));

            assert!(result.is_err());
            assert!(read_session_host_pane("target").unwrap().is_none());
            assert_eq!(
                read_session_host_pane("current").unwrap().as_deref(),
                Some("%9")
            );
            assert_eq!(
                read_session_host_socket("current").unwrap().as_deref(),
                Some("/tmp/outer")
            );
        });
    }

    // --- helpers for moved-from-tmux tests ---
    // Helper to build a detached (0 clients) SessionInfo for tests.
    fn detached_sess(name: &str, created: i64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pane_id: format!("%{}", name),
            created_epoch: created,
            last_activity_epoch: created,
            current_command: "sh".to_string(),
            start_command: None,
            current_path: None,
            launch_command: None,
            launch_cwd: None,
            attached_clients: 0,
        }
    }

    // --- resolve_attach_target ---
    // Tests that resolve_attach_target works for "newest", "oldest", and exact name.
    #[test]
    fn test_resolve_target_named() {
        let sessions = vec![
            SessionInfo {
                name: "a".to_string(),
                pane_id: "%a".to_string(),
                created_epoch: 10,
                last_activity_epoch: 11,
                current_command: "codex".to_string(),
                start_command: Some("codex".to_string()),
                current_path: Some("/tmp".to_string()),
                launch_command: None,
                launch_cwd: None,
                attached_clients: 0,
            },
            SessionInfo {
                name: "b".to_string(),
                pane_id: "%b".to_string(),
                created_epoch: 20,
                last_activity_epoch: 22,
                current_command: "claude".to_string(),
                start_command: Some("claude".to_string()),
                current_path: Some("/home/user".to_string()),
                launch_command: None,
                launch_cwd: None,
                attached_clients: 1,
            },
        ];
        assert_eq!(
            resolve_attach_target(&sessions, "newest").unwrap().name,
            "b"
        );
        assert_eq!(
            resolve_attach_target(&sessions, "oldest").unwrap().name,
            "a"
        );
        assert_eq!(resolve_attach_target(&sessions, "a").unwrap().name, "a");
    }

    // Tests that resolving a nonexistent target returns an error.
    #[test]
    fn test_resolve_target_not_found_is_error() {
        let sessions = vec![detached_sess("alpha", 10)];
        assert!(resolve_attach_target(&sessions, "beta").is_err());
    }

    // Tests that "NEWEST" and "Oldest" are matched case-insensitively.
    #[test]
    fn test_resolve_target_newest_case_insensitive() {
        let sessions = vec![detached_sess("old", 1), detached_sess("new", 100)];
        assert_eq!(
            resolve_attach_target(&sessions, "NEWEST").unwrap().name,
            "new"
        );
        assert_eq!(
            resolve_attach_target(&sessions, "Oldest").unwrap().name,
            "old"
        );
    }

    // Regression: the selectors shadowed sessions literally named `newest` or
    // `oldest`, which could never be attached, killed, or waited on by name.
    #[test]
    fn test_resolve_target_exact_name_wins_over_selectors() {
        let sessions = vec![
            detached_sess("newest", 1),
            detached_sess("oldest", 50),
            detached_sess("other", 100),
        ];
        assert_eq!(
            resolve_attach_target(&sessions, "newest").unwrap().name,
            "newest"
        );
        assert_eq!(
            resolve_attach_target(&sessions, "oldest").unwrap().name,
            "oldest"
        );
        // Case-insensitive selectors still apply when no name matches exactly.
        assert_eq!(
            resolve_attach_target(&sessions, "NEWEST").unwrap().name,
            "other"
        );
    }

    // Tests that ties in creation epoch are broken by session name.
    #[test]
    fn test_resolve_target_newest_tiebreak_by_name() {
        let sessions = vec![detached_sess("beta", 50), detached_sess("alpha", 50)];
        assert_eq!(
            resolve_attach_target(&sessions, "newest").unwrap().name,
            "beta"
        );
        assert_eq!(
            resolve_attach_target(&sessions, "oldest").unwrap().name,
            "alpha"
        );
    }

    // --- ordered_creation_sessions ---
    // Tests that sessions are sorted ascending by creation time.
    #[test]
    fn test_ordered_creation_sessions_sorted_asc() {
        let sessions = vec![
            detached_sess("c", 300),
            detached_sess("a", 100),
            detached_sess("b", 200),
        ];
        let ordered = ordered_creation_sessions(sessions);
        assert_eq!(ordered[0].name, "a");
        assert_eq!(ordered[1].name, "b");
        assert_eq!(ordered[2].name, "c");
    }

    // Tests that tied creation epochs are sorted by name.
    #[test]
    fn test_ordered_creation_sessions_tie_breaks_by_name() {
        let sessions = vec![detached_sess("beta", 10), detached_sess("alpha", 10)];
        let ordered = ordered_creation_sessions(sessions);
        assert_eq!(ordered[0].name, "alpha");
        assert_eq!(ordered[1].name, "beta");
    }

    // --- fallback_last_target ---
    // Tests that fallback_last_target returns the most recently attached session.
    #[test]
    fn test_fallback_last_target_uses_last_attached() {
        with_temp_home(|_| {
            let sessions = vec![detached_sess("old", 1), detached_sess("recent", 999)];
            record_attached_transition("old", None).unwrap();
            let target = fallback_last_target(&sessions, None, None).unwrap();
            assert_eq!(target, "old");
        });
    }

    // Tests that fallback picks the newest session when no history exists.
    #[test]
    fn test_fallback_last_target_falls_back_to_newest_when_no_state() {
        with_temp_home(|_| {
            let sessions = vec![detached_sess("alpha", 1), detached_sess("beta", 999)];
            let target = fallback_last_target(&sessions, None, None).unwrap();
            assert_eq!(target, "beta");
        });
    }

    // Tests that stale history (dead session) falls back to the newest live session.
    #[test]
    fn test_fallback_last_target_stale_state_falls_back_to_newest() {
        with_temp_home(|_| {
            let sessions = vec![detached_sess("current", 999)];
            record_attached_transition("dead-session", None).unwrap();
            let target = fallback_last_target(&sessions, None, None).unwrap();
            assert_eq!(target, "current");
        });
    }

    // Tests that fallback with no sessions returns an error.
    #[test]
    fn test_fallback_last_target_empty_sessions_is_error() {
        with_temp_home(|_| {
            assert!(fallback_last_target(&[], None, None).is_err());
        });
    }

    // Tests that fallback skips the current session and uses previous history.
    #[test]
    fn test_fallback_last_target_skips_current_and_uses_previous_history() {
        with_temp_home(|_| {
            let sessions = vec![
                detached_sess("alpha", 1),
                detached_sess("beta", 2),
                detached_sess("gamma", 3),
            ];
            record_attached_transition("beta", Some("alpha")).unwrap();
            let target = fallback_last_target(&sessions, Some("beta"), None).unwrap();
            assert_eq!(target, "alpha");
        });
    }

    // Tests that stale entries in attach history are skipped.
    #[test]
    fn test_fallback_last_target_ignores_stale_current_history_entry() {
        with_temp_home(|_| {
            let sessions = vec![detached_sess("alpha", 1), detached_sess("beta", 2)];
            record_attached_transition("dead", Some("beta")).unwrap();
            let target = fallback_last_target(&sessions, Some("beta"), None).unwrap();
            assert_eq!(target, "alpha");
        });
    }

    // Tests that pane-local previous process takes priority over global history.
    #[test]
    fn test_fallback_last_target_prefers_pane_local_previous_process() {
        with_temp_home(|_| {
            let sessions = vec![
                detached_sess("alpha", 1),
                detached_sess("beta", 2),
                detached_sess("gamma", 3),
            ];
            write_pane_last_process("%1", "alpha").unwrap();
            record_attached_transition("gamma", Some("beta")).unwrap();
            let target = fallback_last_target(&sessions, Some("beta"), Some("%1")).unwrap();
            assert_eq!(target, "alpha");
        });
    }

    // --- inside_pmux_client ---
    // Tests that TMUX containing a pmux socket path is detected as inside pmux.
    #[test]
    fn test_inside_pmux_client_true_for_pmux_socket() {
        let mut env = crate::EnvGuard::new();
        // Pin the socket name so an ambient PMUX_SOCKET override can't skew this.
        env.set("PMUX_SOCKET", "pmux");
        env.set("TMUX", "/tmp/tmux-1000/pmux,123,0");
        assert!(inside_pmux_client());
    }

    // Tests that a non-pmux TMUX socket is not detected as inside pmux.
    #[test]
    fn test_inside_pmux_client_false_for_non_pmux_socket() {
        let mut env = crate::EnvGuard::new();
        env.set("PMUX_SOCKET", "pmux");
        env.set("TMUX", "/tmp/tmux-1000/default,123,0");
        assert!(!inside_pmux_client());
    }
}
