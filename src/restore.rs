//! Cross-restart session & agent restoration.
//!
//! pmux persists a snapshot of its live sessions (name, cwd, launch command,
//! and — for AI agents — the native session id captured from ai-attn) to
//! `~/.local/state/pmux/restore.json`. On a cold start (e.g. after a machine
//! reboot) `restore()` recreates any snapshotted session that isn't already
//! running, relaunching AI agents with their native resume command so the
//! conversation continues where it left off.
//!
//! This is deliberately self-contained: pmux already knows every session's
//! command and cwd, so the core restore needs no external plugin.
//! tmux-resurrect / tmux-continuum remain an optional enhancement for pane
//! scrollback (documented in docs/restore.md), not a hard dependency.
//!
//! **Honest limitation:** a reboot kills the live process. Restored agents
//! come back idle at their prompt with full conversation history — not
//! mid-task. Continuity of context, not of a running turn.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;

use crate::SpawnedBy;
use crate::agents::{resume_argv, valid_session_id};
use crate::config::{ensure_pmux_config, pmux_config_toml_path};
use crate::session::{
    AiAttnSnapshot, SessionInfo, record_matches_pmux_backend_socket, session_agent,
};
use crate::state::{
    cleanup_stale_session_metadata, cleanup_stale_session_metadata_now, ensure_pmux_state_dir,
    lock_state, read_stored_session_agent_identity, restore_snapshot_path, with_state_lock,
    write_session_agent_identity, write_session_launch_command, write_session_launch_cwd,
};
use crate::tmux::{list_sessions, pmux_server_running, sync_environment_to_server, tmux_cmd};
use crate::util::{SessionName, now_epoch, shell_join, shell_quote_lossy};

const SNAPSHOT_VERSION: u32 = 2;
const LEGACY_SNAPSHOT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// User-facing restore configuration, read from `~/.config/pmux/config.toml`
/// under the `[restore]` table. All fields are opt-out-able.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreConfig {
    /// Master switch: keep a snapshot fresh and allow `pmux restore`.
    pub enabled: bool,
    /// Rebuild agent conversations (resume) after recreating sessions.
    pub resume_agents: bool,
    /// Whether the boot service should be installed/enabled by `init`.
    pub autostart_on_boot: bool,
    /// Which saved non-agent commands restore may run again.
    pub relaunch: RelaunchPolicy,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            resume_agents: true,
            autostart_on_boot: false,
            relaunch: RelaunchPolicy::default(),
        }
    }
}

/// Which saved non-agent commands `pmux restore` (and the boot service) may
/// run again. Agents are governed by `resume_agents` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelaunchPolicy {
    /// Recreate plain interactive shells only. Any other command (a deploy
    /// or migration script, a server with side effects) is held back rather
    /// than re-run at boot without the user asking, and in an environment
    /// that only has HOME and PATH.
    #[default]
    Safe,
    /// Recreate agent sessions only.
    Agents,
    /// Re-run every saved command.
    All,
}

impl RelaunchPolicy {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "safe" => Some(Self::Safe),
            "agents" => Some(Self::Agents),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Agents => "agents",
            Self::All => "all",
        }
    }

    /// Whether a saved non-agent `command` may be re-run.
    fn allows(self, command: &str) -> bool {
        match self {
            Self::All => true,
            Self::Agents => false,
            Self::Safe => is_interactive_shell(command),
        }
    }
}

// A bare interactive shell (`zsh`, `/bin/bash -l`) is safe to start again:
// it runs nothing until someone types into it.
fn is_interactive_shell(command: &str) -> bool {
    const SHELLS: &[&str] = &[
        "bash", "dash", "fish", "ksh", "nu", "pwsh", "sh", "tcsh", "zsh",
    ];
    const SHELL_FLAGS: &[&str] = &["-i", "-l", "--interactive", "--login"];
    let Some(parts) = shlex::split(command) else {
        return false;
    };
    let Some((program, args)) = parts.split_first() else {
        return false;
    };
    let program = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .trim_start_matches('-');
    SHELLS.contains(&program) && args.iter().all(|arg| SHELL_FLAGS.contains(&arg.as_str()))
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    restore: Option<RawRestore>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRestore {
    enabled: Option<bool>,
    resume_agents: Option<bool>,
    autostart_on_boot: Option<bool>,
    relaunch: Option<String>,
}

impl RawConfig {
    fn into_config(self) -> RestoreConfig {
        let defaults = RestoreConfig::default();
        match self.restore {
            Some(r) => RestoreConfig {
                enabled: r.enabled.unwrap_or(defaults.enabled),
                resume_agents: r.resume_agents.unwrap_or(defaults.resume_agents),
                autostart_on_boot: r.autostart_on_boot.unwrap_or(defaults.autostart_on_boot),
                relaunch: r
                    .relaunch
                    .as_deref()
                    .and_then(RelaunchPolicy::parse)
                    .unwrap_or(defaults.relaunch),
            },
            None => defaults,
        }
    }
}

/// Parse restore config from TOML text. Falls back to defaults on any parse
/// error so a malformed config never breaks core pmux operation.
pub(crate) fn parse_config(text: &str) -> RestoreConfig {
    match toml::from_str::<RawConfig>(text) {
        Ok(raw) => raw.into_config(),
        Err(_) => RestoreConfig::default(),
    }
}

/// Load restore config from disk, returning defaults if the file is absent
/// or unreadable.
pub fn load_config() -> RestoreConfig {
    let Ok(path) = pmux_config_toml_path() else {
        return RestoreConfig::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => parse_config(&text),
        Err(_) => RestoreConfig::default(),
    }
}

// ---------------------------------------------------------------------------
// Snapshot model
// ---------------------------------------------------------------------------

/// One pmux session captured in the restore snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub name: String,
    /// Distinguishes a recreated session from stale metadata with the same name.
    #[serde(default)]
    pub created_epoch: i64,
    pub cwd: String,
    /// The shell command line the session was launched with.
    pub command: String,
    /// The AI agent detected for this session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The agent's native session id (from ai-attn), used to resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// The live process that spawned this one, re-linked on restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<SpawnedBy>,
}

/// The persisted snapshot written to `restore.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSnapshot {
    pub version: u32,
    pub saved_at: i64,
    pub sessions: Vec<SessionSnapshot>,
}

/// Read ai-attn state files into a map of `pane_id -> (agent, session_id)`,
/// keeping the freshest socket-scoped record per pane. Backend-socket
/// filtering prevents pane IDs from an unrelated tmux server from being
/// attached to a restore entry.
fn ai_attn_sessions_by_pane() -> HashMap<String, (String, String)> {
    let snapshot = AiAttnSnapshot::load();
    ai_attn_sessions_from_snapshot(&snapshot)
}

fn ai_attn_sessions_from_snapshot(snapshot: &AiAttnSnapshot) -> HashMap<String, (String, String)> {
    // pane_id -> (agent, session_id, updated_at)
    let mut best: HashMap<String, (String, String, i64)> = HashMap::new();
    for record in &snapshot.records {
        if !record_matches_pmux_backend_socket(record) {
            continue;
        }
        let (Some(agent), Some(pane_id), Some(session_id)) = (
            record.agent.as_deref(),
            record.pane_id.as_deref(),
            record.session_id.as_deref(),
        ) else {
            continue;
        };
        if agent == "pmux" || pane_id.is_empty() || session_id.is_empty() {
            continue;
        }
        match best.get(pane_id) {
            Some((_, _, ts)) if *ts >= record.updated_at => {}
            _ => {
                best.insert(
                    pane_id.to_string(),
                    (agent.to_string(), session_id.to_string(), record.updated_at),
                );
            }
        }
    }

    best.into_iter()
        .map(|(pane, (agent, sid, _))| (pane, (agent, sid)))
        .collect()
}

/// Pick the command line to persist for a session, preferring the recorded
/// launch command, then the pane's start command, then the current command.
fn snapshot_command(session: &SessionInfo) -> String {
    session
        .launch_command
        .clone()
        .or_else(|| session.start_command.clone())
        .unwrap_or_else(|| session.current_command.clone())
}

/// Pick the cwd to persist, preferring the pane's current path, then the
/// recorded launch cwd.
fn snapshot_cwd(session: &SessionInfo) -> Option<String> {
    session
        .current_path
        .clone()
        .or_else(|| session.launch_cwd.clone())
}

/// Build a restore snapshot from the live sessions and the ai-attn pane map.
/// Pure function (no I/O) so it can be unit-tested directly.
pub(crate) fn build_snapshot(
    sessions: &[SessionInfo],
    ai_ids: &HashMap<String, (String, String)>,
    saved_at: i64,
) -> RestoreSnapshot {
    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let command = snapshot_command(session);
        if command.trim().is_empty() {
            continue;
        }
        let Some(cwd) = snapshot_cwd(session) else {
            continue;
        };
        let agent = session_agent(session).map(str::to_string);
        // Only attach a session id when the ai-attn record for this pane is
        // for the same agent we detected — avoids cross-wiring ids.
        let agent_session_id = agent.as_deref().and_then(|agent| {
            ai_ids
                .get(&session.pane_id)
                .filter(|(rec_agent, _)| rec_agent == agent)
                .map(|(_, sid)| sid.clone())
        });
        out.push(SessionSnapshot {
            name: session.name.clone(),
            created_epoch: session.created_epoch,
            cwd,
            command,
            agent,
            agent_session_id,
            spawned_by: None,
        });
    }
    RestoreSnapshot {
        version: SNAPSHOT_VERSION,
        saved_at,
        sessions: out,
    }
}

/// Keep the native resume id after the short-lived ai-attn signal expires.
/// Fresh observations are also written to per-session pmux metadata so an
/// unrelated snapshot rewrite cannot erase them, unless `persist` is false
/// (a side-effect-free preview).
fn retain_agent_session_ids(sessions: &[SessionInfo], snap: &mut RestoreSnapshot, persist: bool) {
    let previous = read_snapshot().ok().flatten();
    for saved in &mut snap.sessions {
        let Some(live) = sessions.iter().find(|session| session.name == saved.name) else {
            continue;
        };
        if let (Some(agent), Some(session_id)) =
            (saved.agent.as_deref(), saved.agent_session_id.as_deref())
        {
            if persist {
                let _ = write_session_agent_identity(
                    &saved.name,
                    agent,
                    session_id,
                    live.created_epoch,
                );
            }
            continue;
        }

        let persisted = read_stored_session_agent_identity(&saved.name)
            .ok()
            .flatten()
            .filter(|identity| {
                (identity.created_epoch == 0 || identity.created_epoch == live.created_epoch)
                    && saved.agent.as_deref() == Some(identity.agent.as_str())
                    && valid_session_id(&identity.session_id)
            })
            .map(|identity| identity.session_id);
        if persisted.is_some() {
            saved.agent_session_id = persisted;
            continue;
        }

        saved.agent_session_id = previous
            .as_ref()
            .and_then(|prior| prior.sessions.iter().find(|item| item.name == saved.name))
            .filter(|item| {
                (item.created_epoch == 0 || item.created_epoch == live.created_epoch)
                    && item.agent == saved.agent
            })
            .and_then(|item| item.agent_session_id.clone())
            .filter(|session_id| valid_session_id(session_id));
        if let (true, Some(agent), Some(session_id)) = (
            persist,
            saved.agent.as_deref(),
            saved.agent_session_id.as_deref(),
        ) {
            let _ =
                write_session_agent_identity(&saved.name, agent, session_id, live.created_epoch);
        }
    }
}

/// Record each session's live parent, so restore can re-link it to the
/// recreated parent.
fn retain_parents(sessions: &[SessionInfo], snap: &mut RestoreSnapshot) {
    let mut parents = crate::lineage::live_parents(sessions);
    for saved in &mut snap.sessions {
        saved.spawned_by = parents.remove(&saved.name);
    }
}

/// What a snapshot would record for each live session right now (command,
/// cwd, agent and the resume id restore would use), without writing the
/// snapshot or any per-session metadata. Sessions a snapshot would skip (no
/// command or cwd) are absent.
pub(crate) fn preview_snapshot(sessions: &[SessionInfo]) -> Vec<SessionSnapshot> {
    let mut snap = build_snapshot(sessions, &ai_attn_sessions_by_pane(), now_epoch());
    retain_agent_session_ids(sessions, &mut snap, false);
    retain_parents(sessions, &mut snap);
    snap.sessions
}

// ---------------------------------------------------------------------------
// Snapshot I/O
// ---------------------------------------------------------------------------

/// Write a snapshot atomically (temp file + rename) at mode 0600: it holds
/// launch commands, whose argv may carry secrets, and working directories.
fn write_snapshot(snap: &RestoreSnapshot) -> Result<()> {
    ensure_pmux_state_dir()?;
    let path = restore_snapshot_path()?;
    let json =
        serde_json::to_string_pretty(snap).context("failed to serialize restore snapshot")?;
    crate::util::write_private_file_atomic(&path, json.as_bytes())
}

/// Refresh the restore snapshot from state the relay already loaded.
///
/// The relay calls this on a hot path, so the common unchanged case must not
/// query tmux, rescan the ai-attn directory, reinstall hooks, or write. The
/// relay's data can be older than a snapshot a tmux hook just wrote, so it
/// is only used to detect a change: the write itself re-captures fresh data
/// under the state lock (see [`capture_snapshot`]).
pub(crate) fn refresh_snapshot_from_loaded(
    sessions: &[SessionInfo],
    ai_snapshot: &AiAttnSnapshot,
    cached_sessions: &mut Option<Vec<SessionSnapshot>>,
) -> Result<bool> {
    refresh_snapshot_with(sessions, ai_snapshot, cached_sessions, capture_snapshot)
}

fn refresh_snapshot_with(
    sessions: &[SessionInfo],
    ai_snapshot: &AiAttnSnapshot,
    cached_sessions: &mut Option<Vec<SessionSnapshot>>,
    capture_fresh: impl FnOnce() -> Result<(RestoreSnapshot, Option<KeepReason>)>,
) -> Result<bool> {
    if !load_config().enabled {
        *cached_sessions = None;
        return Ok(false);
    }

    let ai_ids = ai_attn_sessions_from_snapshot(ai_snapshot);
    let mut snap = build_snapshot(sessions, &ai_ids, now_epoch());
    retain_agent_session_ids(sessions, &mut snap, false);
    retain_parents(sessions, &mut snap);
    let snapshot_exists = restore_snapshot_path()?.is_file();
    let unchanged = if let Some(previous) = cached_sessions.as_ref() {
        snapshot_exists && previous == &snap.sessions
    } else {
        snapshot_exists
            && read_snapshot()
                .ok()
                .flatten()
                .is_some_and(|saved| saved.sessions == snap.sessions)
    };
    if unchanged {
        if cached_sessions.is_none() {
            *cached_sessions = Some(snap.sessions);
        }
        return Ok(false);
    }

    let (fresh, kept) = capture_fresh()?;
    if kept.is_some() {
        return Ok(false);
    }
    *cached_sessions = Some(fresh.sessions);
    Ok(true)
}

/// Read the persisted snapshot, returning `None` when no snapshot exists.
pub(crate) fn read_snapshot() -> Result<Option<RestoreSnapshot>> {
    let path = restore_snapshot_path()?;
    match fs::read_to_string(&path) {
        Ok(data) => {
            let mut snap: RestoreSnapshot = serde_json::from_str(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if !matches!(snap.version, LEGACY_SNAPSHOT_VERSION | SNAPSHOT_VERSION) {
                bail!(
                    "unsupported restore snapshot version {} in {} (expected {} or {})",
                    snap.version,
                    path.display(),
                    LEGACY_SNAPSHOT_VERSION,
                    SNAPSHOT_VERSION
                );
            }
            // Version 1 did not record creation epochs. A subsequent write
            // upgrades it after matching the still-live session by name.
            snap.version = SNAPSHOT_VERSION;
            Ok(Some(snap))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {}", path.display(), e)),
    }
}

/// Why a fresh capture must not replace the persisted snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepReason {
    /// The backend is gone (crash, OOM kill, `kill-server`, `restart --hard`).
    /// `list_sessions` reports that as zero sessions, but the previous
    /// snapshot is exactly what a later `pmux restore` needs.
    ServerMissing,
    /// The machine or user session is shutting down. Panes die one at a time
    /// and each `session-closed` hook would otherwise shrink the snapshot right
    /// before the reboot it exists to survive.
    ShuttingDown,
}

/// Decide whether `next` may replace `previous`. The probes are only invoked
/// when needed so the common path never spawns extra processes.
fn snapshot_keep_reason(
    next: &[SessionSnapshot],
    previous: Option<&[SessionSnapshot]>,
    server_running: impl FnOnce() -> bool,
    shutting_down: impl FnOnce() -> bool,
) -> Option<KeepReason> {
    let previous = previous.unwrap_or_default();
    if next.is_empty() && !previous.is_empty() && !server_running() {
        return Some(KeepReason::ServerMissing);
    }
    if snapshot_drops_sessions(next, previous) && shutting_down() {
        return Some(KeepReason::ShuttingDown);
    }
    None
}

fn snapshot_drops_sessions(next: &[SessionSnapshot], previous: &[SessionSnapshot]) -> bool {
    let kept: HashSet<&str> = next.iter().map(|s| s.name.as_str()).collect();
    previous.iter().any(|s| !kept.contains(s.name.as_str()))
}

/// Best-effort shutdown detection. systemd reports `stopping` for the system
/// during poweroff/reboot and for the user manager during logout. Other
/// platforms fall back to the server-missing guard and `restore.prev.json`.
fn system_shutting_down() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    [
        &["is-system-running"][..],
        &["--user", "is-system-running"][..],
    ]
    .iter()
    .any(|args| {
        std::process::Command::new("systemctl")
            .args(*args)
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "stopping")
    })
}

/// Persist `snap` unless a guard says the previous snapshot must be kept.
/// Before a write that drops sessions, the previous snapshot is copied to
/// `restore.prev.json` so an unexpected shrink is always recoverable.
fn persist_snapshot(snap: &RestoreSnapshot) -> Result<Option<KeepReason>> {
    let previous = read_snapshot().ok().flatten();
    let previous_sessions = previous.as_ref().map(|p| p.sessions.as_slice());
    if let Some(reason) = snapshot_keep_reason(
        &snap.sessions,
        previous_sessions,
        || pmux_server_running().unwrap_or(false),
        system_shutting_down,
    ) {
        return Ok(Some(reason));
    }
    if previous_sessions.is_some_and(|prev| snapshot_drops_sessions(&snap.sessions, prev)) {
        let path = restore_snapshot_path()?;
        // Not fs::copy: that would carry over a 0644 mode from snapshots
        // written by older versions.
        if let Ok(previous_bytes) = fs::read(&path) {
            let _ = crate::util::write_private_file_atomic(
                &path.with_file_name("restore.prev.json"),
                &previous_bytes,
            );
        }
    }
    write_snapshot(snap)?;
    Ok(None)
}

// Snapshot writers and metadata cleanup run under the state lock, from a
// session list taken inside it, so neither can act on a stale list.
fn capture_snapshot() -> Result<(RestoreSnapshot, Option<KeepReason>)> {
    capture_snapshot_from(list_sessions, ai_attn_sessions_by_pane)
}

fn capture_snapshot_from(
    list: impl FnOnce() -> Result<Vec<SessionInfo>>,
    load_ai_ids: impl FnOnce() -> HashMap<String, (String, String)>,
) -> Result<(RestoreSnapshot, Option<KeepReason>)> {
    with_state_lock(|| capture_snapshot_locked(list()?, load_ai_ids()))
}

fn capture_snapshot_locked(
    sessions: Vec<SessionInfo>,
    ai_ids: HashMap<String, (String, String)>,
) -> Result<(RestoreSnapshot, Option<KeepReason>)> {
    let mut snap = build_snapshot(&sessions, &ai_ids, now_epoch());
    retain_agent_session_ids(&sessions, &mut snap, true);
    retain_parents(&sessions, &mut snap);
    let kept = persist_snapshot(&snap)?;
    // Metadata cleanup trusts the session list, so it must not run against
    // the empty list a missing server produces.
    if kept.is_none() {
        let _ = cleanup_stale_session_metadata(&sessions);
    }
    Ok((snap, kept))
}

/// Capture the current live sessions into `restore.json` and return the
/// snapshot. Used by `pmux snapshot` and by the best-effort hooks after
/// spawn/kill.
pub fn snapshot() -> Result<RestoreSnapshot> {
    let enabled = load_config().enabled;
    install_snapshot_hook_best_effort();
    if !enabled {
        bail!("restore snapshots are disabled in config ([restore] enabled = false)");
    }
    match capture_snapshot()? {
        (snap, None) => Ok(snap),
        (_, Some(KeepReason::ServerMissing)) => {
            bail!("the pmux server is not running; kept the existing snapshot")
        }
        (_, Some(KeepReason::ShuttingDown)) => {
            bail!("the system is shutting down; kept the existing snapshot")
        }
    }
}

/// Best-effort lifecycle refresh after spawn/kill. Never surfaces an error;
/// when restore is disabled it only keeps hooks and stale metadata consistent.
pub fn snapshot_best_effort() {
    if !load_config().enabled {
        install_snapshot_hook_best_effort();
        let _ = cleanup_stale_session_metadata_now();
        return;
    }
    // Server setup installs the runtime hook. Avoid re-running two tmux
    // configuration commands on every spawn/kill just to refresh the snapshot.
    let _ = capture_snapshot();
}

const SNAPSHOT_HOOK_NAME: &str = "session-closed[100]";

fn snapshot_hook(binary: &str) -> String {
    let command = format!(
        "{} snapshot >/dev/null 2>&1 || true",
        shell_quote_lossy(binary)
    );
    // run-shell format-expands its command, so a `#` in the install path
    // must be escaped for tmux as well as quoted for the shell.
    format!(
        "run-shell -b {}",
        shell_quote_lossy(&crate::tmux::tmux_format_escape(&command))
    )
}

/// Install the runtime close hook on the active server without replacing
/// other hook entries. This also upgrades already-running servers whose
/// generated tmux.conf predates restore support.
pub(crate) fn install_snapshot_hook_best_effort() {
    let enabled = load_config().enabled;
    let _ = tmux_cmd()
        .args([
            "set-option",
            "-g",
            "exit-empty",
            if enabled { "off" } else { "on" },
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !enabled {
        let _ = tmux_cmd()
            .args(["set-hook", "-gu", SNAPSHOT_HOOK_NAME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return;
    }
    let _ = tmux_cmd()
        .arg("set-hook")
        .arg("-g")
        .arg(SNAPSHOT_HOOK_NAME)
        .arg(snapshot_hook(&pmux_binary_path()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// A single session to recreate during restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestorePlanItem {
    pub name: String,
    pub cwd: String,
    /// Final shell command to launch (resume command or the original).
    pub shell_cmd: String,
    /// Whether this launch is an agent resume vs. a plain relaunch.
    pub resumed: bool,
    pub agent: Option<String>,
    pub agent_session_id: Option<String>,
}

/// Compute which snapshotted sessions to recreate and how. Pure function so
/// the resume-vs-relaunch decision is unit-testable without touching tmux.
///
/// Sessions already live (by name) are skipped so restore is idempotent.
pub(crate) fn plan_restore(
    snap: &RestoreSnapshot,
    live_names: &HashSet<String>,
    resume_agents: bool,
    relaunch: RelaunchPolicy,
) -> Vec<RestorePlanItem> {
    let mut plan = Vec::new();
    for session in &snap.sessions {
        if live_names.contains(&session.name) {
            continue;
        }
        if session.agent.is_none() && !relaunch.allows(&session.command) {
            continue;
        }
        let resume = if resume_agents {
            match (
                session.agent.as_deref(),
                session.agent_session_id.as_deref(),
            ) {
                (Some(agent), Some(sid)) => resume_argv(agent, sid).map(|argv| shell_join(&argv)),
                _ => None,
            }
        } else {
            None
        };
        // Never silently relaunch an AI agent as a fresh conversation. A
        // missing or unsupported resume id means the saved context cannot be
        // restored faithfully, so leave the session for the user to resolve.
        if resume_agents && session.agent.is_some() && resume.is_none() {
            continue;
        }
        let (shell_cmd, resumed) = match resume {
            Some(cmd) => (cmd, true),
            None => (session.command.clone(), false),
        };
        plan.push(RestorePlanItem {
            name: session.name.clone(),
            cwd: session.cwd.clone(),
            shell_cmd,
            resumed,
            agent: session.agent.clone(),
            agent_session_id: session.agent_session_id.clone(),
        });
    }
    plan
}

/// Result of a restore run, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreOutcome {
    pub restored: usize,
    pub resumed: usize,
    pub skipped: usize,
    pub missing_resume_id: usize,
    /// Non-agent sessions whose command the relaunch policy held back.
    pub held: usize,
    pub failed: usize,
    pub disabled: bool,
}

/// Recreate a single session on the pmux tmux server, mirroring `spawn`'s
/// `new-session` call and persisting the launch metadata.
fn recreate_session(name: &str, cwd: &str, shell_cmd: &str) -> Result<()> {
    // Snapshot names come from a file and become state-file names, so they get
    // the same checks as names a person types (no `/`, `..`, control chars).
    let name = SessionName::parse(name)
        .with_context(|| format!("snapshot entry has an invalid session name '{name}'"))?;
    let name = name.as_str();
    let cwd_path = std::path::Path::new(cwd);
    if !cwd_path.is_dir() {
        bail!("recorded working directory '{}' no longer exists", cwd);
    }
    let mut generation = crate::startup::AgentStart::new(0, crate::AiAttnSnapshot::load());
    let status = tmux_cmd()
        .env("PATH", crate::tmux::guarded_path_env())
        .arg("new-session")
        .arg("-d")
        // Both values are format-expanded by tmux; see `tmux_format_escape`.
        .arg("-s")
        .arg(crate::tmux::tmux_format_escape(name))
        .arg("-c")
        .arg(crate::tmux::tmux_format_escape_path(cwd_path))
        .arg(shell_cmd)
        .status()
        .context("failed to execute tmux new-session during restore")?;
    if !status.success() {
        bail!("failed to recreate pmux session '{}'", name);
    }
    if let Ok(Some(epoch)) = crate::tmux::session_created_epoch(name) {
        generation.created_epoch = epoch;
        let _ = crate::startup::write_start(name, &generation);
    }
    let _ = write_session_launch_command(name, shell_cmd);
    let _ = write_session_launch_cwd(name, cwd);
    Ok(())
}

/// Recreate any snapshotted session that isn't already running. Called by
/// `pmux restore` and the boot service. Safe to run repeatedly (idempotent).
pub fn restore(_boot: bool) -> Result<RestoreOutcome> {
    let cfg = load_config();
    if !cfg.enabled {
        return Ok(RestoreOutcome {
            disabled: true,
            ..Default::default()
        });
    }
    let Some(snap) = read_snapshot()? else {
        return Ok(RestoreOutcome::default());
    };

    // Make sure the tmux.conf/scripts exist before we create sessions.
    ensure_pmux_config()?;

    let live = list_sessions()?;
    let live_names: HashSet<String> = live.iter().map(|s| s.name.clone()).collect();
    let plan = plan_restore(&snap, &live_names, cfg.resume_agents, cfg.relaunch);
    let held = snap
        .sessions
        .iter()
        .filter(|session| {
            !live_names.contains(&session.name)
                && session.agent.is_none()
                && !cfg.relaunch.allows(&session.command)
        })
        .count();
    let skipped = snap.sessions.len().saturating_sub(plan.len());
    let missing_resume_id = if cfg.resume_agents {
        snap.sessions
            .iter()
            .filter(|session| {
                !live_names.contains(&session.name)
                    && session.agent.is_some()
                    && session.agent_session_id.is_none()
            })
            .count()
    } else {
        0
    };

    if plan.is_empty() {
        return Ok(RestoreOutcome {
            skipped,
            missing_resume_id,
            held,
            ..Default::default()
        });
    }

    crate::tmux::ensure_tmux_supported()?;

    // Forward env (API keys, PATH, …) to an already-running server before
    // recreating. On a cold start the server isn't up yet — skip the sync
    // (which would fail noisily) since each new session inherits this
    // process's environment directly.
    if pmux_server_running().unwrap_or(false) {
        sync_environment_to_server();
    }

    let mut outcome = RestoreOutcome {
        skipped,
        missing_resume_id,
        held,
        ..Default::default()
    };
    let mut restored = HashSet::new();
    for item in &plan {
        // Hold the state lock while the session and its metadata appear, so
        // a concurrent metadata cleanup cannot delete them in between.
        let state_lock = lock_state()?;
        let recreated = recreate_session(&item.name, &item.cwd, &item.shell_cmd);
        match recreated {
            Ok(()) => {
                outcome.restored += 1;
                restored.insert(item.name.as_str());
                if item.resumed {
                    outcome.resumed += 1;
                }
                if let (Some(agent), Some(session_id)) =
                    (item.agent.as_deref(), item.agent_session_id.as_deref())
                {
                    // Stamp the recreated session's real creation epoch so
                    // the identity cannot attach to a later session that
                    // reuses this name.
                    if let Ok(Some(epoch)) = crate::tmux::session_created_epoch(&item.name) {
                        let _ = write_session_agent_identity(&item.name, agent, session_id, epoch);
                    }
                }
                drop(state_lock);
                install_snapshot_hook_best_effort();
            }
            Err(err) => {
                outcome.failed += 1;
                eprintln!("pmux: failed to restore '{}': {err}", item.name);
            }
        }
    }
    if !restored.is_empty()
        && let Err(err) = relink_restored_parents(&snap, &restored)
    {
        eprintln!("pmux: failed to restore spawned-by links: {err:#}");
    }
    Ok(outcome)
}

/// Re-record the parents of restored sessions. A recreated parent has a new
/// `session_created`, so a local link follows the snapshot's parent entry to
/// the session now running under that name. Links to a parent that was not
/// restored are dropped; remote links are kept as recorded.
fn relink_restored_parents(snap: &RestoreSnapshot, restored: &HashSet<&str>) -> Result<()> {
    with_state_lock(|| {
        let live: HashMap<String, i64> = list_sessions()?
            .into_iter()
            .map(|session| (session.name, session.created_epoch))
            .collect();
        let saved: HashMap<&str, i64> = snap
            .sessions
            .iter()
            .map(|session| (session.name.as_str(), session.created_epoch))
            .collect();
        for session in &snap.sessions {
            let (Some(parent), Some(&child_epoch)) = (&session.spawned_by, live.get(&session.name))
            else {
                continue;
            };
            // A child that stayed alive keeps its record unless its parent
            // was the one recreated.
            let was_restored = restored.contains(session.name.as_str());
            if !was_restored && child_epoch != session.created_epoch {
                continue;
            }
            match relinked_parent(parent, &saved, &live, restored) {
                Some(parent) => crate::lineage::write_parent(&session.name, child_epoch, &parent)?,
                None if was_restored => crate::lineage::clear_parent(&session.name)?,
                None => {}
            }
        }
        Ok(())
    })
}

// The snapshot's `parent` as it should be recorded now, given the snapshot's
// and the live sessions' creation epochs by name: the same live session, or
// the session this restore recreated from the snapshot's parent entry.
fn relinked_parent(
    parent: &SpawnedBy,
    saved: &HashMap<&str, i64>,
    live: &HashMap<String, i64>,
    restored: &HashSet<&str>,
) -> Option<SpawnedBy> {
    if parent.is_remote() {
        return Some(parent.clone());
    }
    let live_epoch = *live.get(&parent.process)?;
    let recreated = restored.contains(parent.process.as_str())
        && saved.get(parent.process.as_str()).copied() == parent.created_at;
    (recreated || parent.created_at == Some(live_epoch))
        .then(|| SpawnedBy::local(&parent.process, live_epoch))
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

/// Human-readable diagnostics for the restore subsystem (`pmux doctor restore`).
pub fn doctor() -> Result<String> {
    let cfg = load_config();
    let mut out = String::new();
    out.push_str("pmux restore diagnostics\n");
    out.push_str("------------------------\n");
    out.push_str(&format!("enabled:            {}\n", cfg.enabled));
    out.push_str(&format!("resume_agents:      {}\n", cfg.resume_agents));
    out.push_str("agent without id:   skipped (always)\n");
    out.push_str(&format!("relaunch:           {}\n", cfg.relaunch.name()));
    out.push_str(&format!("autostart_on_boot:  {}\n", cfg.autostart_on_boot));

    let snapshot_path = restore_snapshot_path()?;
    out.push_str(&format!(
        "snapshot file:      {}\n",
        crate::util::display_path_buf(&snapshot_path)
    ));
    match read_snapshot()? {
        Some(snap) => {
            let age = now_epoch().saturating_sub(snap.saved_at).max(0);
            let resumable = snap
                .sessions
                .iter()
                .filter(
                    |s| match (s.agent.as_deref(), s.agent_session_id.as_deref()) {
                        (Some(agent), Some(sid)) => resume_argv(agent, sid).is_some(),
                        _ => false,
                    },
                )
                .count();
            let missing: Vec<&str> = snap
                .sessions
                .iter()
                .filter(|session| session.agent.is_some())
                .filter(|session| {
                    match (
                        session.agent.as_deref(),
                        session.agent_session_id.as_deref(),
                    ) {
                        (Some(agent), Some(session_id)) => resume_argv(agent, session_id).is_none(),
                        _ => true,
                    }
                })
                .map(|session| session.name.as_str())
                .collect();
            out.push_str(&format!(
                "snapshot:           {} session(s), {resumable} resumable, saved {age}s ago\n",
                snap.sessions.len()
            ));
            if !missing.is_empty() {
                out.push_str(&format!("missing resume id: {}\n", missing.join(", ")));
            }
        }
        None => out.push_str("snapshot:           (none yet — run `pmux snapshot`)\n"),
    }

    out.push_str(&format!(
        "boot service:       {}\n",
        boot_service_status_line()
    ));
    Ok(out)
}

fn boot_service_status_line() -> String {
    match boot_service_path() {
        Ok(Some(path)) if path.exists() => {
            format!("installed ({})", crate::util::display_path_buf(&path))
        }
        Ok(Some(path)) => format!(
            "not installed (would be {})",
            crate::util::display_path_buf(&path)
        ),
        _ => "unsupported on this platform".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Boot service
// ---------------------------------------------------------------------------

/// Path to the platform boot-service unit file, or `None` on unsupported
/// platforms.
fn boot_service_path() -> Result<Option<std::path::PathBuf>> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let home = std::path::PathBuf::from(home);
    if cfg!(target_os = "linux") {
        Ok(Some(home.join(".config/systemd/user/pmux-restore.service")))
    } else if cfg!(target_os = "macos") {
        Ok(Some(
            home.join("Library/LaunchAgents/dev.pmux.restore.plist"),
        ))
    } else {
        Ok(None)
    }
}

/// Best-effort path to the pmux binary for use in the boot unit.
fn pmux_binary_path() -> String {
    // Prefer the running binary, but resolve away any `" (deleted)"` suffix a
    // reinstall leaves on `/proc/self/exe`. Only use it when it names a real
    // absolute path; otherwise fall back to the conventional install location.
    if let Ok(exe) = crate::util::resolve_pmux_binary()
        && exe.is_absolute()
        && let Some(s) = exe.to_str()
    {
        return s.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.local/bin/pmux")
}

fn service_path(home: &str, inherited: Option<&str>) -> String {
    let mut entries = vec![
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];
    if let Some(inherited) = inherited {
        entries.extend(
            inherited
                .split(':')
                .filter(|entry| !entry.is_empty())
                .map(str::to_string),
        );
    }
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.clone()));
    entries.join(":")
}

fn systemd_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('\n', "")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_unit(binary: &str, home: &str, path: &str) -> String {
    let binary = systemd_escape(binary);
    let home = systemd_escape(home);
    let path = systemd_escape(path);
    format!(
        "[Unit]\n\
         Description=pmux cross-restart session restore\n\
         After=default.target\n\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         KillMode=process\n\
         Environment=\"HOME={home}\"\n\
         Environment=\"PATH={path}\"\n\
         ExecStart=\"{binary}\" restore --boot\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn launchd_plist(binary: &str, home: &str, path: &str) -> String {
    let binary = xml_escape(binary);
    let home = xml_escape(home);
    let path = xml_escape(path);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>dev.pmux.restore</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{binary}</string>\n\
         \x20   <string>restore</string>\n\
         \x20   <string>--boot</string>\n\
         \x20 </array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict>\n\
         \x20   <key>HOME</key>\n\
         \x20   <string>{home}</string>\n\
         \x20   <key>PATH</key>\n\
         \x20   <string>{path}</string>\n\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Write the platform boot-service unit and return next-step instructions.
///
/// This does not enable/load the service itself — we only write the unit and
/// print the exact command to activate it, to avoid running privileged or
/// side-effecting `systemctl`/`launchctl` calls implicitly (see PR blockers
/// for the auto-enable decision).
pub fn install_boot_service() -> Result<String> {
    let Some(path) = boot_service_path()? else {
        bail!("boot autostart is not supported on this platform yet");
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let binary = pmux_binary_path();
    let home = std::env::var("HOME").context("HOME is not set")?;
    let inherited_path = std::env::var("PATH").ok();
    let service_path = service_path(&home, inherited_path.as_deref());
    let (content, activate) = if cfg!(target_os = "macos") {
        (
            launchd_plist(&binary, &home, &service_path),
            // `launchctl load -w` is deprecated; bootstrap into the user's
            // GUI domain, which is where a LaunchAgent belongs.
            format!(
                "launchctl bootstrap \"gui/$(id -u)\" {}",
                shell_quote_lossy(&path.display().to_string())
            ),
        )
    } else {
        (
            systemd_unit(&binary, &home, &service_path),
            format!(
                "loginctl enable-linger \"$USER\"\n  systemctl --user enable --now {}",
                path.file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("pmux-restore.service")
            ),
        )
    };
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(format!(
        "Wrote boot service unit: {}\nActivate it with:\n  {activate}",
        crate::util::display_path_buf(&path)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::AiAttnState;

    fn session(name: &str, pane: &str, launch: Option<&str>, cwd: Option<&str>) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pane_id: pane.to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "node".to_string(),
            start_command: Some("node x.js".to_string()),
            current_path: cwd.map(str::to_string),
            launch_command: launch.map(str::to_string),
            launch_cwd: Some("/fallback".to_string()),
            attached_clients: 0,
        }
    }

    // --- valid_session_id ---

    // --- resume_argv ---

    // --- config parsing ---

    #[test]
    fn parse_config_defaults_when_empty() {
        assert_eq!(parse_config(""), RestoreConfig::default());
    }

    #[test]
    fn parse_config_reads_restore_table() {
        let text = "[restore]\nenabled = false\nresume_agents = false\nautostart_on_boot = true\n";
        let cfg = parse_config(text);
        assert!(!cfg.enabled);
        assert!(!cfg.resume_agents);
        assert!(cfg.autostart_on_boot);
    }

    #[test]
    fn parse_config_partial_uses_defaults_for_missing() {
        let cfg = parse_config("[restore]\nenabled = false\n");
        assert!(!cfg.enabled);
        assert!(cfg.resume_agents); // default preserved
        assert!(!cfg.autostart_on_boot); // default preserved
    }

    #[test]
    fn parse_config_ignores_removed_fresh_agent_relaunch_setting() {
        let cfg = parse_config("[restore]\nrelaunch_agents_without_session_id = true\n");
        assert_eq!(cfg, RestoreConfig::default());
    }

    #[test]
    fn parse_config_malformed_falls_back_to_defaults() {
        assert_eq!(
            parse_config("not = valid = toml ="),
            RestoreConfig::default()
        );
    }

    // --- build_snapshot ---

    #[test]
    fn build_snapshot_captures_command_cwd_and_agent() {
        let sessions = vec![session(
            "proj",
            "%1",
            Some("claude --model x"),
            Some("/work"),
        )];
        let mut ai = HashMap::new();
        ai.insert(
            "%1".to_string(),
            ("claude".to_string(), "sess-abc".to_string()),
        );
        let snap = build_snapshot(&sessions, &ai, 1000);
        assert_eq!(snap.version, SNAPSHOT_VERSION);
        assert_eq!(snap.sessions.len(), 1);
        let s = &snap.sessions[0];
        assert_eq!(s.name, "proj");
        assert_eq!(s.created_epoch, 1);
        assert_eq!(s.cwd, "/work");
        assert_eq!(s.command, "claude --model x");
        assert_eq!(s.agent.as_deref(), Some("claude"));
        assert_eq!(s.agent_session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn build_snapshot_skips_id_when_ai_agent_mismatches_pane() {
        // ai-attn record for this pane says codex, but the session is claude.
        let sessions = vec![session("proj", "%1", Some("claude"), Some("/work"))];
        let mut ai = HashMap::new();
        ai.insert("%1".to_string(), ("codex".to_string(), "sid".to_string()));
        let snap = build_snapshot(&sessions, &ai, 1);
        assert_eq!(snap.sessions[0].agent.as_deref(), Some("claude"));
        assert_eq!(snap.sessions[0].agent_session_id, None);
    }

    #[test]
    fn build_snapshot_falls_back_to_launch_cwd() {
        let sessions = vec![session("proj", "%1", Some("codex"), None)];
        let snap = build_snapshot(&sessions, &HashMap::new(), 1);
        assert_eq!(snap.sessions[0].cwd, "/fallback");
        assert_eq!(snap.sessions[0].agent.as_deref(), Some("codex"));
        assert_eq!(snap.sessions[0].agent_session_id, None);
    }

    fn snap_entry(name: &str) -> SessionSnapshot {
        SessionSnapshot {
            name: name.to_string(),
            created_epoch: 1,
            cwd: "/work".to_string(),
            command: "bash".to_string(),
            agent: None,
            agent_session_id: None,
            spawned_by: None,
        }
    }

    #[test]
    fn missing_server_never_empties_a_snapshot() {
        let prev = vec![snap_entry("a"), snap_entry("b")];
        let reason = snapshot_keep_reason(&[], Some(&prev), || false, || false);
        assert_eq!(reason, Some(KeepReason::ServerMissing));
    }

    #[test]
    fn closing_the_last_session_on_a_live_server_empties_the_snapshot() {
        let prev = vec![snap_entry("a")];
        assert_eq!(
            snapshot_keep_reason(&[], Some(&prev), || true, || false),
            None
        );
    }

    #[test]
    fn shutdown_keeps_a_shrinking_snapshot_but_not_a_growing_one() {
        let prev = vec![snap_entry("a"), snap_entry("b")];
        let shrunk = vec![snap_entry("a")];
        assert_eq!(
            snapshot_keep_reason(&shrunk, Some(&prev), || true, || true),
            Some(KeepReason::ShuttingDown)
        );
        let grown = vec![snap_entry("a"), snap_entry("b"), snap_entry("c")];
        let probed = std::cell::Cell::new(false);
        assert_eq!(
            snapshot_keep_reason(
                &grown,
                Some(&prev),
                || true,
                || {
                    probed.set(true);
                    true
                }
            ),
            None
        );
        assert!(!probed.get(), "growth must not spawn the shutdown probe");
    }

    #[test]
    fn first_snapshot_is_always_written() {
        assert_eq!(snapshot_keep_reason(&[], None, || false, || true), None);
    }

    #[test]
    fn shrinking_write_backs_up_previous_snapshot() {
        let mut env = crate::EnvGuard::new();
        let tmp =
            std::env::temp_dir().join(format!("pmux-restore-prev-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        env.set("HOME", &tmp);

        let two = RestoreSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at: 1,
            sessions: vec![snap_entry("a"), snap_entry("b")],
        };
        write_snapshot(&two).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let path = restore_snapshot_path().unwrap();
            let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600, "snapshot holds argv and cwds");
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        let one = RestoreSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at: 2,
            sessions: vec![snap_entry("a")],
        };
        // A non-empty capture never probes the server, and outside systemd's
        // "stopping" state the shutdown probe reports false.
        let kept = persist_snapshot(&one).unwrap();
        let prev_path = restore_snapshot_path()
            .unwrap()
            .with_file_name("restore.prev.json");
        let backed_up: RestoreSnapshot =
            serde_json::from_str(&fs::read_to_string(&prev_path).unwrap()).unwrap();
        let prev_mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&prev_path).unwrap().permissions().mode() & 0o777
        };

        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(prev_mode, 0o600);
        assert_eq!(kept, None);
        assert_eq!(backed_up.sessions.len(), 2);
    }

    #[test]
    fn relay_refresh_writes_only_when_snapshot_content_changes() {
        let mut env = crate::EnvGuard::new();
        let tmp =
            std::env::temp_dir().join(format!("pmux-restore-refresh-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        env.set("HOME", &tmp);

        let sessions = vec![session("proj", "%1", Some("codex"), Some("/work"))];
        let mut initial_ai = HashMap::new();
        initial_ai.insert(
            "%1".to_string(),
            ("codex".to_string(), "durable-session-id".to_string()),
        );
        let initial = build_snapshot(&sessions, &initial_ai, 1);
        write_snapshot(&initial).unwrap();
        let ai_snapshot = AiAttnSnapshot {
            records: Vec::new(),
        };
        let mut cache = None;

        assert!(!refresh_snapshot_from_loaded(&sessions, &ai_snapshot, &mut cache).unwrap());
        assert_eq!(read_snapshot().unwrap().unwrap().saved_at, 1);
        assert_eq!(
            read_snapshot().unwrap().unwrap().sessions[0]
                .agent_session_id
                .as_deref(),
            Some("durable-session-id")
        );

        let mut changed = sessions;
        changed[0].launch_command = Some("codex --model gpt-5".to_string());
        let fresh = changed.clone();
        assert!(
            refresh_snapshot_with(&changed, &ai_snapshot, &mut cache, || {
                capture_snapshot_from(|| Ok(fresh), HashMap::new)
            })
            .unwrap()
        );
        assert_eq!(
            read_snapshot().unwrap().unwrap().sessions[0].command,
            "codex --model gpt-5"
        );
        assert_eq!(
            read_snapshot().unwrap().unwrap().sessions[0]
                .agent_session_id
                .as_deref(),
            Some("durable-session-id")
        );

        fs::remove_dir_all(&tmp).ok();
    }

    // Regression: the relay refreshed the snapshot from its cached session
    // list, so it could overwrite a newer hook-written snapshot with stale
    // data (resurrecting a session that had just been killed). The relay's
    // list only detects a change; the write uses a fresh capture.
    #[test]
    fn relay_refresh_writes_fresh_sessions_not_its_stale_list() {
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());

        let live = session("keep", "%1", Some("bash"), Some("/work"));
        let killed = session("killed", "%2", Some("bash"), Some("/work"));
        let hook_written = build_snapshot(std::slice::from_ref(&live), &HashMap::new(), 5);
        write_snapshot(&hook_written).unwrap();

        let stale = vec![live.clone(), killed];
        let ai_snapshot = AiAttnSnapshot {
            records: Vec::new(),
        };
        let mut cache = None;
        let fresh = vec![live];
        assert!(
            refresh_snapshot_with(&stale, &ai_snapshot, &mut cache, || {
                capture_snapshot_from(|| Ok(fresh), HashMap::new)
            })
            .unwrap()
        );
        let saved = read_snapshot().unwrap().unwrap();
        let names: Vec<&str> = saved.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep"]);
        assert_eq!(cache.unwrap().len(), 1);
    }

    #[test]
    fn ai_attn_session_ids_use_the_most_recent_socket_scoped_record() {
        let _env = crate::EnvGuard::new();
        let prior_socket = std::env::var("PMUX_SOCKET").ok();
        #[allow(deprecated)]
        unsafe {
            std::env::set_var("PMUX_SOCKET", "pmux-restore-test");
        }

        let record = |session_id: &str, updated_at: i64, socket: &str| AiAttnState {
            state: Some("waiting".into()),
            updated_at,
            agent: Some("codex".into()),
            session_id: Some(session_id.into()),
            pane_id: Some("%1".into()),
            socket: Some(socket.into()),
            cwd: None,
            reason: None,
        };
        let snapshot = AiAttnSnapshot {
            records: vec![
                record("wrong-server", 100, "/tmp/tmux-1000/other"),
                record("older", 80, "/tmp/tmux-1000/pmux-restore-test"),
                record("correct", 99, "/tmp/tmux-1000/pmux-restore-test"),
            ],
        };

        let sessions = ai_attn_sessions_from_snapshot(&snapshot);
        assert_eq!(
            sessions.get("%1"),
            Some(&("codex".to_string(), "correct".to_string()))
        );

        #[allow(deprecated)]
        unsafe {
            match prior_socket {
                Some(value) => std::env::set_var("PMUX_SOCKET", value),
                None => std::env::remove_var("PMUX_SOCKET"),
            }
        }
    }

    // --- plan_restore ---

    fn snap_of(sessions: Vec<SessionSnapshot>) -> RestoreSnapshot {
        RestoreSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at: 0,
            sessions,
        }
    }

    #[test]
    fn plan_restore_skips_live_sessions() {
        let snap = snap_of(vec![
            SessionSnapshot {
                name: "alive".into(),
                created_epoch: 1,
                cwd: "/a".into(),
                command: "codex".into(),
                agent: Some("codex".into()),
                agent_session_id: None,
                spawned_by: None,
            },
            SessionSnapshot {
                name: "dead".into(),
                created_epoch: 1,
                cwd: "/b".into(),
                command: "zsh".into(),
                agent: None,
                agent_session_id: None,
                spawned_by: None,
            },
        ]);
        let mut live = HashSet::new();
        live.insert("alive".to_string());
        let plan = plan_restore(&snap, &live, true, RelaunchPolicy::All);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "dead");
        assert_eq!(plan[0].shell_cmd, "zsh");
        assert!(!plan[0].resumed);
    }

    #[test]
    fn plan_restore_uses_resume_command_for_agents() {
        let snap = snap_of(vec![SessionSnapshot {
            name: "proj".into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: "claude --model x".into(),
            agent: Some("claude".into()),
            agent_session_id: Some("sid1".into()),
            spawned_by: None,
        }]);
        let plan = plan_restore(&snap, &HashSet::new(), true, RelaunchPolicy::All);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].resumed);
        assert_eq!(plan[0].shell_cmd, "claude --resume sid1");
    }

    #[test]
    fn plan_restore_relaunches_bare_when_resume_disabled() {
        let snap = snap_of(vec![SessionSnapshot {
            name: "proj".into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: "claude --model x".into(),
            agent: Some("claude".into()),
            agent_session_id: Some("sid1".into()),
            spawned_by: None,
        }]);
        let plan = plan_restore(&snap, &HashSet::new(), false, RelaunchPolicy::All);
        assert!(!plan[0].resumed);
        assert_eq!(plan[0].shell_cmd, "claude --model x");
    }

    #[test]
    fn plan_restore_never_relaunches_agent_without_session_id() {
        let snap = snap_of(vec![SessionSnapshot {
            name: "proj".into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: "codex".into(),
            agent: Some("codex".into()),
            agent_session_id: None,
            spawned_by: None,
        }]);
        let plan = plan_restore(&snap, &HashSet::new(), true, RelaunchPolicy::All);
        assert!(plan.is_empty());
    }

    fn plain(name: &str, command: &str) -> SessionSnapshot {
        SessionSnapshot {
            name: name.into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: command.into(),
            agent: None,
            agent_session_id: None,
            spawned_by: None,
        }
    }

    // Regression: restore used to re-run every saved command at boot, so a
    // `make deploy` or migration that was running before a reboot ran again.
    #[test]
    fn safe_relaunch_recreates_shells_and_agents_but_holds_commands() {
        let mut sessions = vec![
            plain("shell", "zsh"),
            plain("login", "/bin/bash -l"),
            plain("deploy", "make deploy"),
            plain("script", "bash ./migrate --apply"),
        ];
        sessions.push(SessionSnapshot {
            agent: Some("claude".into()),
            agent_session_id: Some("sid1".into()),
            spawned_by: None,
            ..plain("agent", "claude")
        });
        let snap = snap_of(sessions);
        let names = |policy| {
            plan_restore(&snap, &HashSet::new(), true, policy)
                .into_iter()
                .map(|item| item.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(RelaunchPolicy::Safe), ["shell", "login", "agent"]);
        assert_eq!(names(RelaunchPolicy::Agents), ["agent"]);
        assert_eq!(
            names(RelaunchPolicy::All),
            ["shell", "login", "deploy", "script", "agent"]
        );
    }

    #[test]
    fn relaunch_policy_parses_from_config_and_defaults_to_safe() {
        assert_eq!(parse_config("").relaunch, RelaunchPolicy::Safe);
        assert_eq!(
            parse_config("[restore]\nrelaunch = \"all\"\n").relaunch,
            RelaunchPolicy::All
        );
        assert_eq!(
            parse_config("[restore]\nrelaunch = \"Agents\"\n").relaunch,
            RelaunchPolicy::Agents
        );
        assert_eq!(
            parse_config("[restore]\nrelaunch = \"bogus\"\n").relaunch,
            RelaunchPolicy::Safe
        );
    }

    #[test]
    fn systemd_unit_leaves_the_restored_server_running_on_stop() {
        // The tmux server restore starts lives in the unit's cgroup; without
        // KillMode=process, `systemctl --user stop` killed every session.
        assert!(systemd_unit("/bin/pmux", "/home/u", "/usr/bin").contains("KillMode=process"));
    }

    // Point every tmux call at a throwaway backend for tests that reach
    // recreate_session. If the validation under test ever regresses, the
    // session lands on this socket (killed on drop), never the user's live
    // `-L pmux` server.
    struct IsolatedBackend {
        _env: crate::EnvGuard,
        dir: tempfile::TempDir,
        socket: String,
    }

    impl IsolatedBackend {
        fn new() -> Self {
            let mut env = crate::EnvGuard::new();
            let dir = tempfile::Builder::new()
                .prefix("pmux-rt-")
                .tempdir_in("/tmp")
                .unwrap();
            let socket = format!("pmux-restore-test-{}", std::process::id());
            env.set("HOME", dir.path());
            env.set("TMUX_TMPDIR", dir.path());
            env.set("PMUX_SOCKET", &socket);
            env.remove("TMUX");
            Self {
                _env: env,
                dir,
                socket,
            }
        }
    }

    impl Drop for IsolatedBackend {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .env("TMUX_TMPDIR", self.dir.path())
                .args(["-L", &self.socket, "kill-server"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    #[test]
    fn recreate_rejects_missing_recorded_cwd() {
        let _backend = IsolatedBackend::new();
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let err = recreate_session("missing-cwd", missing.to_str().unwrap(), "sh")
            .expect_err("missing cwd should fail before invoking tmux");
        assert!(err.to_string().contains("no longer exists"));
    }

    // Regression: snapshot names become state-file paths, so a hand-edited or
    // corrupted restore.json must not be able to write outside the state dir.
    #[test]
    fn recreate_rejects_unsafe_snapshot_names_before_touching_tmux() {
        let _backend = IsolatedBackend::new();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_str().unwrap();
        for bad in ["../../escape", "a\tb", "-flag"] {
            let err = recreate_session(bad, cwd, "sh").expect_err("unsafe name must be rejected");
            assert!(
                format!("{err:#}").contains("invalid session name"),
                "{bad:?}: {err:#}"
            );
        }
    }

    // --- snapshot serialization round-trip ---

    #[test]
    fn restored_children_follow_their_recreated_parent() {
        let saved: HashMap<&str, i64> = [("parent", 10), ("kept", 30)].into_iter().collect();
        let live: HashMap<String, i64> = [("parent".to_string(), 100), ("kept".to_string(), 30)]
            .into_iter()
            .collect();
        let restored: HashSet<&str> = ["parent", "child"].into_iter().collect();
        let relink = |parent: SpawnedBy| relinked_parent(&parent, &saved, &live, &restored);
        assert_eq!(
            relink(SpawnedBy::local("parent", 10)),
            Some(SpawnedBy::local("parent", 100)),
            "recreated parent gets its new epoch"
        );
        assert_eq!(
            relink(SpawnedBy::local("kept", 30)),
            Some(SpawnedBy::local("kept", 30)),
            "a parent that stayed alive is kept"
        );
        assert_eq!(relink(SpawnedBy::local("parent", 9)), None, "older parent");
        assert_eq!(relink(SpawnedBy::local("gone", 5)), None, "parent not back");
        let remote = SpawnedBy {
            process: "lineage ios".into(),
            created_at: None,
            machine: Some("macbook".into()),
            url: Some("http://100.72.15.45:9470".into()),
        };
        assert_eq!(relink(remote.clone()), Some(remote));
        // A later, unrelated session under the parent's name is not adopted.
        let not_restored = HashSet::new();
        assert_eq!(
            relinked_parent(
                &SpawnedBy::local("parent", 10),
                &saved,
                &live,
                &not_restored
            ),
            None
        );
    }

    #[test]
    fn snapshot_json_roundtrips() {
        let snap = snap_of(vec![SessionSnapshot {
            name: "proj".into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: "codex".into(),
            agent: Some("codex".into()),
            agent_session_id: Some("sid".into()),
            spawned_by: None,
        }]);
        let json = serde_json::to_string(&snap).unwrap();
        let back: RestoreSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // --- boot unit generation ---

    #[test]
    fn systemd_unit_references_restore_boot() {
        let unit = systemd_unit("/usr/bin/pmux", "/home/test", "/test/bin:/usr/bin");
        assert!(unit.contains("ExecStart=\"/usr/bin/pmux\" restore --boot"));
        assert!(unit.contains("Environment=\"HOME=/home/test\""));
        assert!(unit.contains("Environment=\"PATH=/test/bin:/usr/bin\""));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_references_restore_boot() {
        let plist = launchd_plist("/usr/bin/pmux", "/Users/test", "/test/bin:/usr/bin");
        assert!(plist.contains("dev.pmux.restore"));
        assert!(plist.contains("<string>restore</string>"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<string>/test/bin:/usr/bin</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn boot_templates_escape_platform_values() {
        let unit = systemd_unit("/home/100%/pmux", "/home/100%", "/a/\"quoted\"");
        assert!(unit.contains("100%%"));
        assert!(unit.contains("\\\"quoted\\\""));

        let plist = launchd_plist("/Users/a&b/pmux", "/Users/a&b", "/a/<bin>");
        assert!(plist.contains("/Users/a&amp;b/pmux"));
        assert!(plist.contains("/a/&lt;bin&gt;"));
    }

    #[test]
    fn snapshot_hook_uses_numbered_entry_and_current_binary() {
        let hook = snapshot_hook("/tmp/pmux test");
        assert_eq!(SNAPSHOT_HOOK_NAME, "session-closed[100]");
        assert!(hook.contains("pmux test"));
        assert!(hook.contains("snapshot"));
        assert!(hook.starts_with("run-shell -b "));
    }

    #[test]
    fn plan_restore_resumes_cortex_conversation() {
        let snap = snap_of(vec![SessionSnapshot {
            name: "cortex-worker".into(),
            created_epoch: 1,
            cwd: "/w".into(),
            command: "cortex".into(),
            agent: Some("cortex".into()),
            agent_session_id: Some("cx-123".into()),
            spawned_by: None,
        }]);
        let plan = plan_restore(&snap, &HashSet::new(), true, RelaunchPolicy::All);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].resumed);
        assert_eq!(plan[0].shell_cmd, "cortex resume cx-123");
    }
}
