use crate::restore::SessionSnapshot;
use crate::session::{
    AiAttnSnapshot, AiAttnState, SessionInfo, agent_counts, agent_cwd_counts, is_sole_agent,
    is_sole_agent_cwd, session_agent, session_ai_state,
};
use crate::state::{
    ai_attn_state_dir, clear_detached_session_host_pane, pmux_relay_pid_file,
    read_ai_attn_relay_pid, read_session_host_pane, read_session_host_socket, with_state_lock,
    write_ai_attn_relay_pid,
};
use crate::tmux::list_sessions;
use crate::util::{process_argv, relative_age, shell_quote_lossy};
use anyhow::{Context, Result, anyhow};
use notify::{EventKind, RecursiveMode, Watcher};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEBOUNCE_DURATION: Duration = Duration::from_millis(50);
const MIN_AI_ATTN_VERSION: (u32, u32, u32) = (0, 3, 2);

/// Fire a one-shot relay sync in the background so the attention state
/// for the just-attached session is propagated immediately, without
/// waiting for the next fsnotify event.
pub fn trigger_relay_sync() {
    let Ok(exe) = crate::util::resolve_pmux_binary() else {
        return;
    };
    if let Ok(mut child) = Command::new(exe)
        .args(["relay-ai-attn"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap the child in a detached thread to avoid zombie accumulation.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Start the ai-attn relay daemon if it's not already running.
/// Called when attaching a session inside an outer tmux.
pub fn ensure_ai_attn_relay() -> Result<()> {
    ensure_compatible_ai_attn()?;
    // Check and spawn under the state lock. Without it, two attaches at once
    // (for example several outer panes restored together) both see no relay,
    // both start one, and the loser of the PID-file write is orphaned.
    with_state_lock(|| {
        if relay_running()? {
            return Ok(());
        }
        spawn_relay()
    })
}

fn spawn_relay() -> Result<()> {
    let current_exe = crate::util::resolve_pmux_binary()?;
    let log_path = pmux_relay_log_path()?;
    if let Some(parent) = log_path.parent() {
        crate::util::create_private_dir_all(parent)?;
    }
    trim_relay_log(&log_path);

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("failed to clone {}", log_path.display()))?;

    let mut cmd = Command::new(current_exe);
    cmd.arg("relay-ai-attn")
        .arg("--loop")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    detach_relay_command(&mut cmd);

    let child = cmd.spawn().context("failed to spawn ai attention relay")?;
    let written = write_ai_attn_relay_pid(child.id());
    // `pmux attach` stays alive (blocked in attach-session) long after the
    // relay may have exited; reap it so it never lingers as a zombie.
    reap_in_background(child);
    written
}

// Wait for `child` on a detached thread. If this process exits first, the
// child is reparented to init, which reaps it instead.
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

// The relay log is capped at this size. It is checked at startup and on every
// reconcile, so a relay that runs for weeks cannot grow it without bound.
const MAX_RELAY_LOG_BYTES: u64 = 10 * 1024 * 1024;

// Reset an oversized relay log in place. The running relay's stdout/stderr
// were opened in append mode, so truncating the file is safe: later writes
// continue at the new end of file.
fn trim_relay_log(log_path: &Path) {
    if fs::metadata(log_path).is_ok_and(|meta| meta.len() > MAX_RELAY_LOG_BYTES) {
        let _ = fs::write(log_path, b"[log truncated]\n");
    }
}

/// Stop any existing relay daemon and start a fresh one.
pub fn restart_ai_attn_relay() -> Result<()> {
    if let Some(pid) = read_ai_attn_relay_pid()?
        && relay_process_alive(pid)
    {
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        for _ in 0..20 {
            // A zombie still answers kill(pid, 0); check the argv too.
            if !relay_process_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    if let Ok(pid_file) = pmux_relay_pid_file() {
        let _ = fs::remove_file(pid_file);
    }
    ensure_ai_attn_relay()
}

/// Replace an existing relay after a binary upgrade without starting one for
/// users who do not use the outer-tmux integration.
pub(crate) fn restart_ai_attn_relay_if_running() -> Result<bool> {
    if !relay_running()? {
        return Ok(false);
    }
    restart_ai_attn_relay()?;
    Ok(true)
}

/// Run the ai-attn relay: one-shot sync or continuous loop with filesystem watching.
/// Called by `pmux relay-ai-attn [--loop]`.
pub fn relay_ai_attn(loop_forever: bool) -> Result<()> {
    ensure_compatible_ai_attn()?;
    // Track the last-written relay state per session so we only call ai-attn
    // when something actually changes, avoiding redundant subprocess calls.
    let mut last_state: HashMap<String, RelayState> = HashMap::new();
    let mut last_snapshot_sessions: Option<Vec<SessionSnapshot>> = None;

    if !loop_forever {
        relay_ai_attn_once(&mut last_state, &mut last_snapshot_sessions)?.relay_result?;
        return Ok(());
    }

    // Watch the ai-attn state directory for changes instead of polling.
    let state_dir = ai_attn_state_dir();
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create {}", state_dir.display()))?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    let _ = tx.send(());
                }
            }
            // A watcher error can mean missed events; say so in the relay log
            // and reconcile now rather than waiting for the 30s timeout.
            Err(error) => {
                eprintln!("relay: file watcher error: {error}");
                let _ = tx.send(());
            }
        }
    })
    .context("failed to create file watcher")?;

    watcher
        .watch(&state_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", state_dir.display()))?;

    let mut known_agent_states = HashMap::new();
    let mut label_refreshes: Vec<Instant> = Vec::new();

    // Initial sync. Exit immediately if there are no sessions to serve.
    match relay_ai_attn_once(&mut last_state, &mut last_snapshot_sessions) {
        Ok(outcome) => {
            known_agent_states = outcome.agent_states;
            if let Err(e) = outcome.relay_result {
                eprintln!("relay: {e:#}");
            }
            if outcome.sessions_empty && relay_may_exit(list_sessions) {
                return Ok(());
            }
        }
        Err(e) => eprintln!("relay: {e:#}"),
    }

    loop {
        let timeout = label_refreshes
            .iter()
            .min()
            .map_or(Duration::from_secs(30), |at| {
                at.saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(30))
            });
        match rx.recv_timeout(timeout) {
            Ok(()) => {
                // Debounce: drain any events that arrive within a short window.
                while rx.recv_timeout(DEBOUNCE_DURATION).is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Ok(log_path) = pmux_relay_log_path() {
            trim_relay_log(&log_path);
        }
        // Reconcile on both filesystem events and the periodic timeout. The
        // latter is what expires TTL-bound records when no producer writes a
        // new state file.
        match relay_ai_attn_once(&mut last_state, &mut last_snapshot_sessions) {
            Ok(outcome) => {
                if let Err(e) = outcome.relay_result {
                    eprintln!("relay: {e:#}");
                }
                if outcome.sessions_empty && relay_may_exit(list_sessions) {
                    return Ok(());
                }
                if agent_state_changed(&known_agent_states, &outcome.agent_states) {
                    schedule_label_refreshes(&mut label_refreshes, Instant::now());
                }
                known_agent_states = outcome.agent_states;
            }
            Err(e) => eprintln!("relay: {e:#}"),
        }
        run_due_label_refreshes(&mut label_refreshes, Instant::now());
    }

    // Clean up PID file on exit.
    remove_relay_pid_if_owned(std::process::id());
    Ok(())
}

/// Render the ai-attn commands a one-shot relay sync would execute.
pub fn relay_ai_attn_dry_run(verbose: bool) -> Result<String> {
    let sessions = list_sessions()?;
    let snapshot = AiAttnSnapshot::load();
    let counts = agent_counts(&sessions);
    let cwd_counts = agent_cwd_counts(&sessions);
    let mut out = String::new();

    let live: HashSet<&str> = sessions
        .iter()
        .map(|session| session.name.as_str())
        .collect();
    for record in active_relay_records(&snapshot) {
        let Some(stale) = relay_record_session_name(record) else {
            continue;
        };
        if live.contains(stale) {
            continue;
        }
        let args = relay_record_clear_args(record, stale);
        if verbose {
            out.push_str(&format!("# stale relay session: {stale}\n"));
        }
        out.push_str(&format!("{}\n", format_ai_attn_command(&args)));
    }

    for session in &sessions {
        let decision = relay_decision(session, &counts, &cwd_counts, &snapshot)?;
        if verbose {
            out.push_str(&format!(
                "# session {}: {}\n",
                session.name, decision.reason
            ));
        }
        match &decision.desired_state {
            RelayState::Cleared => {
                for record in active_relay_records_for_session(&snapshot, &session.name) {
                    let args = relay_record_clear_args(record, &session.name);
                    out.push_str(&format!("{}\n", format_ai_attn_command(&args)));
                }
            }
            RelayState::Active { .. } => {
                for record in obsolete_active_relay_records(&snapshot, &session.name, &decision) {
                    let args = relay_record_clear_args(record, &session.name);
                    out.push_str(&format!("{}\n", format_ai_attn_command(&args)));
                }
                out.push_str(&format!("{}\n", format_ai_attn_command(&decision.args)));
            }
        }
    }

    if out.is_empty() {
        out.push_str("No relay actions.\n");
    }
    Ok(out)
}

/// Report pmux/ai-attn relay health and the current per-session routing state.
pub fn doctor_ai_attn() -> Result<String> {
    let sessions = list_sessions()?;
    let snapshot = AiAttnSnapshot::load();
    let counts = agent_counts(&sessions);
    let cwd_counts = agent_cwd_counts(&sessions);
    let state_dir = ai_attn_state_dir();
    let relay_pid = read_ai_attn_relay_pid()?;
    let relay_alive = relay_pid.is_some_and(process_alive);
    let relay_pid_text = match relay_pid {
        Some(pid) if relay_alive => format!("{pid} (running)"),
        Some(pid) => format!("{pid} (not running)"),
        None => "none".to_string(),
    };
    let relay_pid_file = pmux_relay_pid_file()?;
    let log_path = pmux_relay_log_path()?;

    let mut out = String::new();
    out.push_str("pmux ai-attn diagnostics\n\n");
    out.push_str("ai-attn:\n");
    out.push_str(&format!("  binary: {}\n", ai_attn_version_text()));
    out.push_str(&format!(
        "  state_dir: {}\n",
        crate::util::display_path_buf(&state_dir)
    ));
    out.push_str(&format!("  records: {}\n\n", snapshot.records.len()));

    out.push_str("relay:\n");
    out.push_str(&format!("  pid: {relay_pid_text}\n"));
    out.push_str(&format!(
        "  pid_file: {}\n",
        crate::util::display_path_buf(&relay_pid_file)
    ));
    out.push_str(&format!(
        "  log: {}\n\n",
        crate::util::display_path_buf(&log_path)
    ));

    out.push_str("sessions:\n");
    if sessions.is_empty() {
        out.push_str("  none\n");
    } else {
        for session in &sessions {
            out.push_str(&format_session_summary(
                session,
                &counts,
                &cwd_counts,
                &snapshot,
            )?);
        }
    }

    out.push_str("\nrelay records:\n");
    let mut relay_records: Vec<&AiAttnState> = snapshot
        .records
        .iter()
        .filter(|record| record.agent.as_deref() == Some("pmux"))
        .collect();
    relay_records.sort_by_key(|record| Reverse(record.updated_at));
    if relay_records.is_empty() {
        out.push_str("  none\n");
    } else {
        for record in relay_records {
            out.push_str(&format_record_line(record));
        }
    }

    Ok(out)
}

/// Report relay matching details for a single pmux session.
pub fn debug_ai_attn_session(session_name: &str) -> Result<String> {
    let sessions = list_sessions()?;
    let session = sessions
        .iter()
        .find(|session| session.name == session_name)
        .ok_or_else(|| anyhow!("process '{}' not found", session_name))?;
    let snapshot = AiAttnSnapshot::load();
    let counts = agent_counts(&sessions);
    let cwd_counts = agent_cwd_counts(&sessions);
    let decision = relay_decision(session, &counts, &cwd_counts, &snapshot)?;
    let mut records = relevant_records(session, &snapshot);
    records.sort_by_key(|record| Reverse(record.updated_at));

    let mut out = String::new();
    out.push_str(&format!("pmux ai-attn session: {session_name}\n\n"));
    out.push_str("session:\n");
    out.push_str(&format_session_summary(
        session,
        &counts,
        &cwd_counts,
        &snapshot,
    )?);
    out.push_str("\nrelay decision:\n");
    out.push_str(&format!("  reason: {}\n", decision.reason));
    out.push_str(&format!(
        "  command: {}\n",
        format_ai_attn_command(&decision.args)
    ));

    out.push_str("\nmatching records:\n");
    if records.is_empty() {
        out.push_str("  none\n");
    } else {
        for record in records {
            out.push_str(&format_record_line(record));
        }
    }

    Ok(out)
}

/// What was last written for a session's relay record.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RelayState {
    /// Last wrote an active state to this target pane on this outer server.
    Active {
        state: String,
        target_pane: String,
        socket: Option<String>,
    },
    /// Last wrote clear-state (or session is idle/detached).
    Cleared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RelayDecision {
    args: Vec<String>,
    reason: String,
    desired_state: RelayState,
}

struct RelaySyncOutcome {
    sessions_empty: bool,
    relay_result: Result<()>,
    /// Each agent conversation's ai-attn state, from the same snapshot.
    agent_states: HashMap<String, String>,
}

/// After an agent's state changes, refresh the status-bar labels once the
/// prompt has reached its transcript, and again once a generated title has
/// had time to land. Agents that announce titles through the pane title
/// (Claude, Codex) also trigger `pmux status-labels` from tmux's
/// pane-title-changed hook; Cortex never sets a pane title, so without this
/// its label showed neither its first prompt nor its title.
const LABEL_REFRESH_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(15)];

/// Each agent conversation's state, leaving out the records the relay
/// writes itself (agent `pmux`), so its own writes never schedule refreshes.
fn agent_states(snapshot: &AiAttnSnapshot) -> HashMap<String, String> {
    snapshot
        .records
        .iter()
        .filter(|record| record.agent.as_deref() != Some("pmux"))
        .filter_map(|record| Some((record.session_id.clone()?, record.state.clone()?)))
        .collect()
}

/// Whether a conversation appeared or changed state between two reads.
/// Records expiring or being collected change no label.
fn agent_state_changed(before: &HashMap<String, String>, after: &HashMap<String, String>) -> bool {
    after
        .iter()
        .any(|(session, state)| before.get(session) != Some(state))
}

/// Schedule [`LABEL_REFRESH_DELAYS`] from `now`, replacing pending ones.
fn schedule_label_refreshes(due: &mut Vec<Instant>, now: Instant) {
    due.clear();
    due.extend(LABEL_REFRESH_DELAYS.iter().map(|delay| now + *delay));
}

/// Publish labels for every refresh due by `now`, keeping later ones.
fn run_due_label_refreshes(due: &mut Vec<Instant>, now: Instant) {
    let before = due.len();
    due.retain(|at| *at > now);
    if due.len() < before
        && let Err(error) = crate::status_labels::publish_status_labels()
    {
        eprintln!("relay: status labels: {error:#}");
    }
}

// Perform a single relay sync: read all sessions and propagate their ai-attn state.
fn relay_ai_attn_once(
    last_state: &mut HashMap<String, RelayState>,
    last_snapshot_sessions: &mut Option<Vec<SessionSnapshot>>,
) -> Result<RelaySyncOutcome> {
    let sessions = list_sessions()?;
    let snapshot = AiAttnSnapshot::load();
    let counts = agent_counts(&sessions);
    let cwd_counts = agent_cwd_counts(&sessions);
    let relay_result: Result<()> = (|| {
        clear_stale_relay_records(&sessions, &snapshot, last_state)?;
        // One session's failed ai-attn call must not leave every later
        // session unsynced until the next reconcile; report the first error.
        let mut first_error = None;
        for session in &sessions {
            if let Err(error) =
                sync_session_attention(session, &counts, &cwd_counts, &snapshot, last_state)
            {
                first_error.get_or_insert(error.context(format!("session '{}'", session.name)));
            }
        }
        first_error.map_or(Ok(()), Err)
    })();

    // Direct agent records often appear shortly after `pmux run`. Reuse this
    // reconciliation's loaded data and persist only when resumable content
    // changes, rather than spawning more tmux commands or writing every tick.
    let _ =
        crate::restore::refresh_snapshot_from_loaded(&sessions, &snapshot, last_snapshot_sessions);
    Ok(RelaySyncOutcome {
        sessions_empty: sessions.is_empty(),
        relay_result,
        agent_states: agent_states(&snapshot),
    })
}

// Remove relay records for sessions that no longer exist.
fn clear_stale_relay_records(
    sessions: &[SessionInfo],
    snapshot: &AiAttnSnapshot,
    last_state: &mut HashMap<String, RelayState>,
) -> Result<()> {
    let live: HashSet<&str> = sessions
        .iter()
        .map(|session| session.name.as_str())
        .collect();
    for record in active_relay_records(snapshot) {
        let Some(session_name) = relay_record_session_name(record) else {
            continue;
        };
        if !live.contains(session_name) {
            clear_relay_record(record, session_name)?;
            last_state.remove(session_name);
        }
    }
    Ok(())
}

// Sync one session's ai-attn state to the outer tmux via ai-attn set/clear commands.
// Skips if the state hasn't changed since the last sync.
fn sync_session_attention(
    session: &SessionInfo,
    counts: &std::collections::HashMap<&'static str, usize>,
    cwd_counts: &std::collections::HashMap<(String, String), usize>,
    snapshot: &AiAttnSnapshot,
    last_state: &mut HashMap<String, RelayState>,
) -> Result<()> {
    let decision = relay_decision(session, counts, cwd_counts, snapshot)?;
    if session.attached_clients <= 0 {
        let _ = clear_detached_session_host_pane(&session.name);
    }

    match &decision.desired_state {
        RelayState::Cleared => {
            for record in active_relay_records_for_session(snapshot, &session.name) {
                clear_relay_record(record, &session.name)?;
            }
            last_state.insert(session.name.clone(), RelayState::Cleared);
            return Ok(());
        }
        RelayState::Active { .. } => {
            for record in obsolete_active_relay_records(snapshot, &session.name, &decision) {
                clear_relay_record(record, &session.name)?;
            }
        }
    }

    if last_state.get(&session.name) != Some(&decision.desired_state)
        || !desired_relay_record_exists(snapshot, &session.name, &decision)
    {
        run_relay_args(&decision.args)?;
    }
    last_state.insert(session.name.clone(), decision.desired_state);
    Ok(())
}

fn active_relay_records(snapshot: &AiAttnSnapshot) -> impl Iterator<Item = &AiAttnState> {
    snapshot.records.iter().filter(|record| {
        record.agent.as_deref() == Some("pmux")
            && record
                .state
                .as_deref()
                .is_some_and(|state| !state.trim().is_empty())
            && relay_record_session_name(record).is_some()
    })
}

fn active_relay_records_for_session<'a>(
    snapshot: &'a AiAttnSnapshot,
    session_name: &'a str,
) -> impl Iterator<Item = &'a AiAttnState> {
    active_relay_records(snapshot)
        .filter(move |record| relay_record_session_name(record) == Some(session_name))
}

fn obsolete_active_relay_records<'a>(
    snapshot: &'a AiAttnSnapshot,
    session_name: &'a str,
    decision: &'a RelayDecision,
) -> impl Iterator<Item = &'a AiAttnState> {
    active_relay_records_for_session(snapshot, session_name)
        .filter(move |record| !relay_record_matches_decision(record, decision))
}

fn relay_record_session_name(record: &AiAttnState) -> Option<&str> {
    record
        .session_id
        .as_deref()?
        .strip_prefix("pmux:")
        .filter(|name| !name.is_empty())
}

fn relay_record_matches_decision(record: &AiAttnState, decision: &RelayDecision) -> bool {
    let RelayState::Active {
        state,
        target_pane,
        socket,
    } = &decision.desired_state
    else {
        return false;
    };
    record.state.as_deref() == Some(state.as_str())
        && record.pane_id.as_deref() == Some(target_pane.as_str())
        && record.socket.as_deref().filter(|value| !value.is_empty())
            == socket.as_deref().filter(|value| !value.is_empty())
}

fn desired_relay_record_exists(
    snapshot: &AiAttnSnapshot,
    session_name: &str,
    decision: &RelayDecision,
) -> bool {
    active_relay_records_for_session(snapshot, session_name)
        .any(|record| relay_record_matches_decision(record, decision))
}

fn relay_record_clear_args(record: &AiAttnState, session_name: &str) -> Vec<String> {
    build_relay_args(
        "clear-state",
        session_name,
        record.pane_id.as_deref(),
        record.socket.as_deref(),
        None,
    )
}

fn clear_relay_record(record: &AiAttnState, session_name: &str) -> Result<()> {
    let mut cmd = ai_attn_cmd();
    cmd.args(relay_record_clear_args(record, session_name));
    run_ai_attn(cmd)
}

fn run_relay_args(args: &[String]) -> Result<()> {
    let mut cmd = ai_attn_cmd();
    cmd.args(args);
    run_ai_attn(cmd)
}

// Execute an ai-attn command and wait for it to finish.
fn run_ai_attn(mut cmd: Command) -> Result<()> {
    let output = cmd
        .output()
        .context("failed to execute ai-attn relay command")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!(": {stdout}"),
            (true, false) => format!(": {stderr}"),
            (false, false) => format!(": {stderr}; stdout: {stdout}"),
        };
        anyhow::bail!("ai-attn exited with {}{}", output.status, detail);
    }
    Ok(())
}

fn detach_relay_command(cmd: &mut Command) {
    #[cfg(unix)]
    {
        // Keep the relay daemon out of the caller's process group so it
        // survives short-lived wrapper commands that clean up descendants.
        cmd.process_group(0);
    }
}

// Build a Command for the ai-attn CLI, respecting the AI_ATTN_BIN override.
fn ai_attn_cmd() -> Command {
    if let Ok(bin) = env::var("AI_ATTN_BIN") {
        return Command::new(bin);
    }
    Command::new("ai-attn")
}

fn ai_attn_version_text() -> String {
    let mut cmd = ai_attn_cmd();
    cmd.arg("--version");
    match cmd.output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            match parse_ai_attn_version(&text) {
                Some(version) if version >= MIN_AI_ATTN_VERSION => format!("{text} (compatible)"),
                Some(_) => format!(
                    "{text} (requires >= {}.{}.{})",
                    MIN_AI_ATTN_VERSION.0, MIN_AI_ATTN_VERSION.1, MIN_AI_ATTN_VERSION.2
                ),
                None if text.is_empty() => "incompatible (version unknown)".to_string(),
                None if is_ai_attn_dev_build(&text) => {
                    format!("{text} (unversioned development build; assumed compatible)")
                }
                None => format!(
                    "{text} (incompatible; requires >= {}.{}.{})",
                    MIN_AI_ATTN_VERSION.0, MIN_AI_ATTN_VERSION.1, MIN_AI_ATTN_VERSION.2
                ),
            }
        }
        Ok(output) => format!("unavailable ({})", output.status),
        Err(err) => format!("unavailable ({err})"),
    }
}

fn parse_ai_attn_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches('v');
        let token = token.trim_end_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.');
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch_text = parts.next()?;
        let patch = patch_text
            .split_once('-')
            .map_or(patch_text, |(value, _)| value)
            .parse()
            .ok()?;
        Some((major, minor, patch))
    })
}

fn ensure_compatible_ai_attn() -> Result<()> {
    let mut cmd = ai_attn_cmd();
    let output = cmd
        .arg("--version")
        .output()
        .context("failed to execute ai-attn; install ai-attn v0.3.2 or newer")?;
    if !output.status.success() {
        anyhow::bail!("ai-attn --version exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    validate_ai_attn_version(&text)
}

// A build from source without a version stamp (a plain `go build`) reports
// `ai-attn dev`. It is someone working on ai-attn, most likely on a current
// checkout, so it is trusted rather than turning the relay off for every
// process; `pmux doctor ai-attn` says it is unversioned.
fn is_ai_attn_dev_build(text: &str) -> bool {
    text.split_whitespace().any(|token| token == "dev")
}

fn validate_ai_attn_version(text: &str) -> Result<()> {
    if parse_ai_attn_version(text).is_none() && is_ai_attn_dev_build(text) {
        return Ok(());
    }
    let version = parse_ai_attn_version(text)
        .ok_or_else(|| anyhow!("could not parse ai-attn version from '{}'", text.trim()))?;
    if version < MIN_AI_ATTN_VERSION {
        anyhow::bail!(
            "ai-attn {}.{}.{} is too old; pmux requires >= {}.{}.{}",
            version.0,
            version.1,
            version.2,
            MIN_AI_ATTN_VERSION.0,
            MIN_AI_ATTN_VERSION.1,
            MIN_AI_ATTN_VERSION.2
        );
    }
    Ok(())
}

// Build the relay session ID string (e.g. "pmux:session-name") for ai-attn records.
fn relay_session_id(session_name: &str) -> String {
    format!("pmux:{}", session_name)
}

fn relay_decision(
    session: &SessionInfo,
    counts: &std::collections::HashMap<&'static str, usize>,
    cwd_counts: &std::collections::HashMap<(String, String), usize>,
    snapshot: &AiAttnSnapshot,
) -> Result<RelayDecision> {
    let target_pane = read_session_host_pane(&session.name)?;
    // The outer tmux server that owns target_pane. Stamped onto the relay
    // record so the outer daemon claims it and the pmux-backend daemon ignores
    // it; pane IDs collide across servers otherwise.
    let socket = read_session_host_socket(&session.name)?;

    if session.attached_clients <= 0 {
        return Ok(clear_decision(
            session,
            target_pane.as_deref(),
            socket.as_deref(),
            "session is detached",
        ));
    }

    let Some(target_pane) = target_pane else {
        return Ok(clear_decision(
            session,
            None,
            socket.as_deref(),
            "session has no host-pane mapping",
        ));
    };

    if let Some((ai_state, updated_at)) = session_ai_state(
        session,
        is_sole_agent(session, counts),
        is_sole_agent_cwd(session, cwd_counts),
        snapshot,
    ) {
        let args = build_relay_args(
            "set-state",
            &session.name,
            Some(&target_pane),
            socket.as_deref(),
            Some(ai_state),
        );
        return Ok(RelayDecision {
            args,
            reason: format!(
                "relay {ai_state} from direct ai-attn record updated {}",
                relative_age(updated_at)
            ),
            desired_state: RelayState::Active {
                state: ai_state.to_string(),
                target_pane,
                socket,
            },
        });
    }

    Ok(clear_decision(
        session,
        Some(&target_pane),
        socket.as_deref(),
        "no current direct ai-attn state matched this session",
    ))
}

fn clear_decision(
    session: &SessionInfo,
    target_pane: Option<&str>,
    socket: Option<&str>,
    reason: &str,
) -> RelayDecision {
    RelayDecision {
        args: build_relay_args("clear-state", &session.name, target_pane, socket, None),
        reason: reason.to_string(),
        desired_state: RelayState::Cleared,
    }
}

// Build the CLI arguments for an ai-attn set-state or clear-state command.
fn build_relay_args(
    action: &str,
    session_name: &str,
    target_pane: Option<&str>,
    socket: Option<&str>,
    state: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        action.to_string(),
        "--agent".to_string(),
        "pmux".to_string(),
        "--session-id".to_string(),
        relay_session_id(session_name),
        "--pane-id".to_string(),
        target_pane.unwrap_or("").to_string(),
        // Scope the record to the outer tmux server that owns target_pane so
        // its daemon claims it and other servers ignore the pane ID.
        "--socket".to_string(),
        socket.unwrap_or("").to_string(),
        "--cwd".to_string(),
        ".".to_string(),
        "--reason".to_string(),
        "pmux_relay".to_string(),
    ];
    if let Some(s) = state {
        args.extend(["--state".to_string(), s.to_string()]);
    }
    args
}

fn format_ai_attn_command(args: &[String]) -> String {
    std::iter::once("ai-attn")
        .chain(args.iter().map(String::as_str))
        .map(shell_quote_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_session_summary(
    session: &SessionInfo,
    counts: &std::collections::HashMap<&'static str, usize>,
    cwd_counts: &std::collections::HashMap<(String, String), usize>,
    snapshot: &AiAttnSnapshot,
) -> Result<String> {
    let agent = session_agent(session).unwrap_or("-");
    let host_pane = read_session_host_pane(&session.name)?.unwrap_or_else(|| "-".to_string());
    let socket = read_session_host_socket(&session.name)?.unwrap_or_else(|| "-".to_string());
    let matched = session_ai_state(
        session,
        is_sole_agent(session, counts),
        is_sole_agent_cwd(session, cwd_counts),
        snapshot,
    );
    let state = match matched {
        Some((state, updated_at)) => format!("{} ({} ago)", state, relative_age(updated_at)),
        None => "-".to_string(),
    };
    Ok(format!(
        "  {} agent={} attached={} inner_pane={} host_pane={} socket={} state={}\n",
        session.name, agent, session.attached_clients, session.pane_id, host_pane, socket, state
    ))
}

fn relevant_records<'a>(
    session: &SessionInfo,
    snapshot: &'a AiAttnSnapshot,
) -> Vec<&'a AiAttnState> {
    let agent = session_agent(session);
    let relay_id = relay_session_id(&session.name);
    snapshot
        .records
        .iter()
        .filter(|record| {
            record.session_id.as_deref() == Some(relay_id.as_str())
                || record
                    .pane_id
                    .as_deref()
                    .is_some_and(|pane| pane == session.pane_id)
                || agent.is_some_and(|agent| record.agent.as_deref() == Some(agent))
        })
        .collect()
}

fn format_record_line(record: &AiAttnState) -> String {
    format!(
        "  agent={} session_id={} pane_id={} socket={} state={} reason={} cwd={} age={}\n",
        opt(record.agent.as_deref()),
        opt(record.session_id.as_deref()),
        opt(record.pane_id.as_deref()),
        opt(record.socket.as_deref()),
        opt(record.state.as_deref()),
        opt(record.reason.as_deref()),
        opt(record.cwd.as_deref()),
        relative_age(record.updated_at)
    )
}

fn opt(value: Option<&str>) -> &str {
    value.filter(|value| !value.is_empty()).unwrap_or("-")
}

// Check whether the ai-attn relay daemon is alive by sending signal 0 to its PID.
fn relay_running() -> Result<bool> {
    let Some(pid) = read_ai_attn_relay_pid()? else {
        return Ok(false);
    };
    Ok(relay_process_alive(pid))
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn argv_is_relay(argv: &[String]) -> bool {
    let executable_is_pmux = argv
        .first()
        .and_then(|value| std::path::Path::new(value).file_name())
        .and_then(|value| value.to_str())
        == Some("pmux");
    executable_is_pmux
        && argv
            .windows(2)
            .any(|pair| pair[0] == "relay-ai-attn" && pair[1] == "--loop")
}

fn relay_process_alive(pid: u32) -> bool {
    process_alive(pid) && process_argv(pid).is_some_and(|argv| argv_is_relay(&argv))
}

// Decide whether an idle relay may exit. `ensure_ai_attn_relay` checks the
// PID file under the state lock, so re-list sessions and remove our PID file
// under the same lock: either a new session shows up here and the relay keeps
// serving it, or the PID file is gone before anyone can decide this relay is
// still running. Errors keep the relay alive until the next reconcile.
fn relay_may_exit(list: impl FnOnce() -> Result<Vec<SessionInfo>>) -> bool {
    with_state_lock(|| {
        if !list()?.is_empty() {
            return Ok(false);
        }
        remove_relay_pid_if_owned(std::process::id());
        Ok(true)
    })
    .unwrap_or(false)
}

fn remove_relay_pid_if_owned(pid: u32) {
    let Ok(Some(recorded_pid)) = read_ai_attn_relay_pid() else {
        return;
    };
    if recorded_pid != pid {
        return;
    }
    if let Ok(pid_file) = pmux_relay_pid_file() {
        let _ = fs::remove_file(pid_file);
    }
}

// Return the path to the relay daemon log file.
fn pmux_relay_log_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("pmux")
        .join("ai-attn-relay.log"))
}

#[cfg(test)]
mod tests {
    use super::{
        LABEL_REFRESH_DELAYS, active_relay_records, agent_state_changed, agent_states,
        argv_is_relay, build_relay_args, parse_ai_attn_version, relay_record_clear_args,
        relay_record_session_name, run_due_label_refreshes, schedule_label_refreshes,
        validate_ai_attn_version,
    };
    use crate::session::{AiAttnSnapshot, AiAttnState};
    use std::collections::HashMap;
    use std::time::Instant;

    // Regression: an idle relay removed its PID file and exited without the
    // state lock, so a concurrent `ensure_ai_attn_relay` could see it as
    // running just before it exited, leaving a new session with no relay.
    #[test]
    fn idle_relay_exits_only_after_relisting_under_the_state_lock() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        env.remove("PMUX_SOCKET");
        crate::state::write_ai_attn_relay_pid(std::process::id()).unwrap();

        // A session appeared after the sync that saw none: keep serving.
        let session = crate::session::SessionInfo {
            name: "late".to_string(),
            pane_id: "%1".to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "sh".to_string(),
            start_command: None,
            current_path: None,
            launch_command: None,
            launch_cwd: None,
            attached_clients: 0,
        };
        assert!(!super::relay_may_exit(|| Ok(vec![session])));
        assert_eq!(
            crate::state::read_ai_attn_relay_pid().unwrap(),
            Some(std::process::id())
        );
        // A listing error keeps the relay alive too.
        assert!(!super::relay_may_exit(|| Err(anyhow::anyhow!(
            "tmux hiccup"
        ))));

        // The decision waits for whoever holds the lock (an ensure in
        // progress), then removes the PID file before reporting exit.
        let guard = crate::state::lock_state().unwrap();
        let waiter = std::thread::spawn(|| super::relay_may_exit(|| Ok(Vec::new())));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !waiter.is_finished(),
            "exit decision ignored the state lock"
        );
        drop(guard);
        assert!(waiter.join().unwrap());
        assert_eq!(crate::state::read_ai_attn_relay_pid().unwrap(), None);
    }

    // Regression: the relay spawned by `pmux attach` was never waited for,
    // so it lingered as a zombie (which still answers kill(pid, 0)) while
    // the attach blocked in attach-session.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawned_relay_is_reaped_when_it_exits() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        super::reap_in_background(child);
        let stat = format!("/proc/{pid}/stat");
        let reaped = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::read_to_string(&stat).is_err()
        });
        assert!(reaped, "child {pid} was left as a zombie");
        // A zombie or a recycled PID with another argv is not the relay.
        assert!(!super::relay_process_alive(pid));
    }

    // The relay's log was only trimmed at startup, so a long-running relay
    // could grow it without bound. Trimming in place must keep working for a
    // writer that holds the file open in append mode, as the relay does.
    #[test]
    fn oversized_relay_log_is_trimmed_in_place_for_append_writers() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.log");
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writer
            .write_all(&vec![b'x'; (super::MAX_RELAY_LOG_BYTES + 1) as usize])
            .unwrap();
        super::trim_relay_log(&path);
        writer.write_all(b"after\n").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "[log truncated]\nafter\n");

        // A log under the cap is left alone.
        super::trim_relay_log(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    fn relay_record(state: &str) -> AiAttnState {
        AiAttnState {
            state: Some(state.to_string()),
            updated_at: 1,
            agent: Some("pmux".to_string()),
            session_id: Some("pmux:old-session".to_string()),
            pane_id: Some("%42".to_string()),
            socket: Some("/tmp/tmux-1000/outer".to_string()),
            cwd: Some(".".to_string()),
            reason: Some("pmux_relay".to_string()),
        }
    }

    #[test]
    fn parses_ai_attn_release_versions() {
        assert_eq!(parse_ai_attn_version("ai-attn v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_ai_attn_version("ai-attn 1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_ai_attn_version("ai-attn dev"), None);
    }

    #[test]
    fn enforces_socket_scoped_ai_attn_release() {
        assert!(validate_ai_attn_version("ai-attn v0.3.2").is_ok());
        assert!(validate_ai_attn_version("ai-attn v0.4.0").is_ok());
        assert!(validate_ai_attn_version("ai-attn v0.3.1").is_err());
        assert!(validate_ai_attn_version("ai-attn devel").is_err());
        assert!(validate_ai_attn_version("ai-attn").is_err());
    }

    #[test]
    fn trusts_an_unversioned_ai_attn_dev_build() {
        assert!(validate_ai_attn_version("ai-attn dev").is_ok());
        assert!(validate_ai_attn_version("ai-attn dev\n").is_ok());
        // A versioned build is still checked, whatever else it says.
        assert!(validate_ai_attn_version("ai-attn v0.3.1 dev").is_err());
    }

    #[test]
    fn relay_process_identity_requires_loop_subcommand() {
        let relay = vec![
            "/home/user/.local/bin/pmux".to_string(),
            "relay-ai-attn".to_string(),
            "--loop".to_string(),
        ];
        let one_shot = vec![
            "/home/user/.local/bin/pmux".to_string(),
            "relay-ai-attn".to_string(),
        ];
        let unrelated = vec![
            "echo".to_string(),
            "relay-ai-attn".to_string(),
            "--loop".to_string(),
        ];

        assert!(argv_is_relay(&relay));
        assert!(!argv_is_relay(&one_shot));
        assert!(!argv_is_relay(&unrelated));
    }

    #[test]
    fn stale_relay_clear_preserves_recorded_target_identity() {
        let record = relay_record("waiting");

        assert_eq!(relay_record_session_name(&record), Some("old-session"));
        let args = relay_record_clear_args(&record, "old-session");
        let pane = args.iter().position(|arg| arg == "--pane-id").unwrap();
        let socket = args.iter().position(|arg| arg == "--socket").unwrap();
        assert_eq!(args[pane + 1], "%42");
        assert_eq!(args[socket + 1], "/tmp/tmux-1000/outer");
    }

    #[test]
    fn cleared_relay_records_are_not_refreshed() {
        let snapshot = AiAttnSnapshot {
            records: vec![relay_record("waiting"), relay_record("")],
        };

        let active: Vec<_> = active_relay_records(&snapshot).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].state.as_deref(), Some("waiting"));
    }

    // Tests that set-state relay args include session identity, outer pane ID,
    // owning socket, and state fields.
    #[test]
    fn set_relay_args_use_stable_identity_fields() {
        let args = build_relay_args(
            "set-state",
            "session-1",
            Some("%42"),
            Some("/tmp/tmux-1000/homelab"),
            Some("working"),
        );
        let joined = args.join("\n");

        assert!(joined.contains("set-state"));
        assert!(joined.contains("pmux:session-1"));
        assert!(joined.contains("--state"));
        assert!(joined.contains("working"));

        // --pane-id should be the outer pane
        let pane_idx = args.iter().position(|arg| arg == "--pane-id").unwrap();
        assert_eq!(args[pane_idx + 1], "%42");

        // --socket should be the outer server that owns the pane
        let socket_idx = args.iter().position(|arg| arg == "--socket").unwrap();
        assert_eq!(args[socket_idx + 1], "/tmp/tmux-1000/homelab");
    }

    // Tests that clear-state relay args use the same identity fields without --state.
    #[test]
    fn clear_relay_args_use_same_stable_key_fields() {
        let args = build_relay_args("clear-state", "session-1", None, None, None);
        let joined = args.join("\n");

        assert!(joined.contains("clear-state"));
        assert!(joined.contains("pmux:session-1"));
        assert!(!joined.contains("--state"));

        // --pane-id should be empty when no target pane
        let pane_idx = args.iter().position(|arg| arg == "--pane-id").unwrap();
        assert_eq!(args[pane_idx + 1], "");

        // --socket should be present but empty when unknown
        let socket_idx = args.iter().position(|arg| arg == "--socket").unwrap();
        assert_eq!(args[socket_idx + 1], "");
    }

    #[test]
    fn cortex_direct_signal_is_relayed_to_its_host_pane() {
        use super::{RelayState, relay_decision};
        use crate::session::SessionInfo;
        use std::collections::HashMap;
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());
        env.set("PMUX_SOCKET", "cortex-test-backend");
        let session = SessionInfo {
            name: "cortex-test".into(),
            pane_id: "%124".into(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "sh".into(),
            launch_command: Some("cortex".into()),
            start_command: None,
            current_path: None,
            launch_cwd: None,
            attached_clients: 1,
        };
        crate::state::write_session_host_pane(&session.name, "%9", Some("/tmp/outer")).unwrap();
        let snapshot = AiAttnSnapshot {
            records: vec![AiAttnState {
                state: Some("waiting".into()),
                updated_at: crate::util::now_epoch(),
                agent: Some("cortex".into()),
                session_id: Some("cx-native".into()),
                pane_id: Some("%124".into()),
                socket: Some("/tmp/cortex-test-backend".into()),
                cwd: None,
                reason: Some("permission_prompt".into()),
            }],
        };
        let decision =
            relay_decision(&session, &HashMap::new(), &HashMap::new(), &snapshot).unwrap();
        assert!(
            matches!(decision.desired_state, RelayState::Active { state, target_pane, socket }
            if state == "waiting" && target_pane == "%9" && socket.as_deref() == Some("/tmp/outer"))
        );
        assert_eq!(
            crate::session::session_ai_record(&session, false, false, &snapshot)
                .and_then(|r| r.session_id.as_deref()),
            Some("cx-native")
        );
    }

    fn agent_record(agent: &str, session: &str, state: &str) -> AiAttnState {
        AiAttnState {
            state: Some(state.into()),
            agent: Some(agent.into()),
            session_id: Some(session.into()),
            ..relay_record(state)
        }
    }

    #[test]
    fn agent_states_skip_the_relays_own_records() {
        let snapshot = AiAttnSnapshot {
            records: vec![
                agent_record("cortex", "cx-1", "working"),
                agent_record("pmux", "relay-1", "working"),
            ],
        };
        let states = agent_states(&snapshot);
        assert_eq!(states.len(), 1);
        assert_eq!(states.get("cx-1").map(String::as_str), Some("working"));
    }

    #[test]
    fn label_refreshes_follow_state_changes_only() {
        let before = HashMap::from([("cx-1".to_string(), "working".to_string())]);
        // Same state, a record collected: no label can have changed.
        assert!(!agent_state_changed(&before, &before));
        assert!(!agent_state_changed(&before, &HashMap::new()));
        // A finished turn or a new conversation can bring a title.
        let done = HashMap::from([("cx-1".to_string(), "done".to_string())]);
        assert!(agent_state_changed(&before, &done));
        let mut more = before.clone();
        more.insert("cx-2".into(), "working".into());
        assert!(agent_state_changed(&before, &more));
    }

    #[test]
    fn due_label_refreshes_run_once_and_later_ones_wait() {
        let start = Instant::now();
        let mut due = Vec::new();
        schedule_label_refreshes(&mut due, start);
        assert_eq!(due.len(), LABEL_REFRESH_DELAYS.len());
        run_due_label_refreshes(&mut due, start);
        assert_eq!(due.len(), 2, "nothing is due yet");
        // Rescheduling replaces pending refreshes rather than stacking them.
        schedule_label_refreshes(&mut due, start);
        assert_eq!(due.len(), 2);
        assert!(due.iter().all(|at| *at > start));
    }
}
