use crate::agents::Agent;
use crate::tmux::pmux_socket;
use crate::util::non_empty;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[cfg(test)]
use crate::util::now_epoch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub pane_id: String,
    pub created_epoch: i64,
    pub last_activity_epoch: i64,
    pub current_command: String,
    pub start_command: Option<String>,
    pub current_path: Option<String>,
    pub launch_command: Option<String>,
    pub launch_cwd: Option<String>,
    pub attached_clients: i64,
}

// Parse tab-separated tmux list-panes output into SessionInfo structs.
// Test helper; list_sessions parses records via session_from_columns.
#[cfg(test)]
pub(crate) fn parse_sessions(raw: &str) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    for line in raw.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != 8 {
            continue;
        }
        sessions.push(session_from_columns(line, &cols)?);
    }
    Ok(sessions)
}

// Build one SessionInfo from the eight list-panes columns, in the order
// name, created, activity, current command, start command, current path,
// attached clients, pane id. `line` is only used in error messages.
pub(crate) fn session_from_columns(line: &str, cols: &[&str]) -> Result<SessionInfo> {
    let created_epoch = cols[1]
        .parse::<i64>()
        .with_context(|| format!("invalid created epoch in line '{}': {}", line, cols[1]))?;
    let last_activity_epoch = if cols[2].trim().is_empty() {
        created_epoch
    } else {
        cols[2]
            .parse::<i64>()
            .with_context(|| format!("invalid activity epoch in line '{}': {}", line, cols[2]))?
    };
    let attached_clients = cols[6]
        .parse::<i64>()
        .with_context(|| format!("invalid attached count in line '{}': {}", line, cols[6]))?;

    Ok(SessionInfo {
        name: cols[0].to_string(),
        pane_id: cols[7].to_string(),
        created_epoch,
        last_activity_epoch,
        current_command: cols[3].to_string(),
        start_command: non_empty(cols[4]),
        current_path: non_empty(cols[5]),
        launch_command: None,
        launch_cwd: None,
        attached_clients,
    })
}

/// Return the human-readable command name for a session (launch command preferred).
/// Used in the picker and `ls` output.
pub fn display_command(session: &SessionInfo) -> String {
    launch_command_name(session).unwrap_or_else(|| session.current_command.clone())
}

/// Return the current ai-attn state for this session's AI agent, if any.
///
/// Matches ai-attn records to sessions by inner tmux pane ID (stable across
/// CWD changes) or by an unambiguous exact agent/CWD pair for detached agent
/// runtimes, then falls back to agent-only matching when this is the sole
/// session running its agent type.
///
/// Returns `Some((state, updated_at))` for the matched record, or `None`.
/// Pass a pre-loaded `AiAttnSnapshot` to avoid redundant file I/O when
/// checking multiple sessions in a loop.
pub fn session_ai_state<'a>(
    session: &SessionInfo,
    is_sole_agent: bool,
    is_sole_agent_cwd: bool,
    snapshot: &'a AiAttnSnapshot,
) -> Option<(&'a str, i64)> {
    session_ai_record(session, is_sole_agent, is_sole_agent_cwd, snapshot).map(|record| {
        (
            record.state.as_deref().unwrap_or_default(),
            record.updated_at,
        )
    })
}

/// The ai-attn record behind [`session_ai_state`], for callers that also
/// need its reason.
pub(crate) fn session_ai_record<'a>(
    session: &SessionInfo,
    is_sole_agent: bool,
    is_sole_agent_cwd: bool,
    snapshot: &'a AiAttnSnapshot,
) -> Option<&'a AiAttnState> {
    let agent = session_agent(session)?;

    // Prefer the most recent direct lifecycle record for a pane, then fall
    // back to an unambiguous exact-CWD match and finally an agent-only match.
    // `terminal-bell` records are from a retired tmux-ai-attn fallback; ignore
    // them while they age out so an old completion bell cannot surface as
    // `waiting` for up to the configured TTL.
    let generation = crate::startup::read_start(&session.name, session.created_epoch);
    let mut pane_match: Option<&AiAttnState> = None;
    let mut cwd_match: Option<&AiAttnState> = None;
    let mut agent_only_match: Option<&AiAttnState> = None;
    for record in &snapshot.records {
        if generation
            .as_ref()
            .is_some_and(|start| !start.accepts(record))
        {
            continue;
        }
        let state = record.state.as_deref().unwrap_or("");
        if state.is_empty() {
            continue;
        }
        if is_terminal_bell(record) {
            continue;
        }
        if record.agent.as_deref() != Some(agent) {
            continue;
        }
        // Skip relay records written by pmux itself (agent="pmux") — we
        // only want direct agent signals from the actual AI CLI.
        if record.agent.as_deref() == Some("pmux") {
            continue;
        }
        if !record_matches_pmux_backend_socket(record) {
            continue;
        }

        // Match by inner tmux pane ID — this is stable across CWD changes
        // within the same session.
        let is_pane_match = record
            .pane_id
            .as_deref()
            .is_some_and(|p| !p.is_empty() && p == session.pane_id);
        if is_pane_match {
            if pane_match.is_none_or(|current| is_preferred_signal(record, current)) {
                pane_match = Some(record);
            }
        } else if is_sole_agent_cwd
            && record.pane_id.as_deref().is_none_or(|pane| pane.is_empty())
            && record
                .socket
                .as_deref()
                .is_none_or(|socket| socket.is_empty())
            && record.cwd.as_deref() == session_match_cwd(session)
        {
            if cwd_match.is_none_or(|current| is_preferred_signal(record, current)) {
                cwd_match = Some(record);
            }
        } else if agent_only_match.is_none_or(|current| is_preferred_signal(record, current)) {
            agent_only_match = Some(record);
        }
    }

    match (pane_match, cwd_match) {
        (Some(pane), Some(cwd)) => {
            let preferred = if is_preferred_signal(cwd, pane) {
                cwd
            } else {
                pane
            };
            return Some(preferred);
        }
        (Some(pane), None) => return Some(pane),
        (None, Some(cwd)) => return Some(cwd),
        (None, None) => {}
    }

    // Only fall back to agent-only matching when this is the sole session
    // running this agent type — otherwise we'd incorrectly flag all sessions
    // of the same agent as needing input.
    if is_sole_agent {
        agent_only_match
    } else {
        None
    }
}

fn is_preferred_signal(candidate: &AiAttnState, current: &AiAttnState) -> bool {
    candidate.updated_at > current.updated_at
}

fn is_terminal_bell(record: &AiAttnState) -> bool {
    record.reason.as_deref() == Some("terminal-bell")
}

pub(crate) fn record_matches_pmux_backend_socket(record: &AiAttnState) -> bool {
    let Some(socket) = record.socket.as_deref().filter(|socket| !socket.is_empty()) else {
        // Older ai-attn records did not include socket scope.
        return true;
    };
    // Compare by basename so different absolute paths to the same server still
    // match. This would false-match an unrelated outer server also launched as
    // `tmux -L pmux`, but that name collision is not expected in practice.
    let backend_socket = pmux_socket();
    Path::new(socket).file_name().and_then(|v| v.to_str()) == Some(backend_socket.as_str())
}

// Extract the basename of the launch command, falling back to start_command.
fn launch_command_name(session: &SessionInfo) -> Option<String> {
    let launch = session
        .launch_command
        .as_deref()
        .and_then(command_name_from_line);
    if launch.is_some() {
        return launch;
    }
    session
        .start_command
        .as_deref()
        .and_then(command_name_from_line)
}

// Extract the command basename from a shell command line string.
fn command_name_from_line(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }

    if let Some(parts) = shlex::split(line)
        && let Some(first) = parts.first()
    {
        return Some(command_basename(first));
    }

    line.split_whitespace().next().map(command_basename)
}

// Return the file name component of a command path (e.g. "/usr/bin/codex" → "codex").
fn command_basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(value)
        .to_string()
}

/// Identify the AI agent type for a session without allocating.
///
/// Checks the launch/start command first (cheaper — avoids `shlex::split`
/// in `display_command`), then falls back to `current_command`.
pub(crate) fn session_agent(session: &SessionInfo) -> Option<&'static str> {
    // Try launch_command, then start_command, then current_command.
    if let Some(ref lc) = session.launch_command
        && let Some(agent) = Agent::detect(lc).map(Agent::name)
    {
        return Some(agent);
    }
    if let Some(ref sc) = session.start_command
        && let Some(agent) = Agent::detect(sc).map(Agent::name)
    {
        return Some(agent);
    }
    Agent::detect(&session.current_command).map(Agent::name)
}

/// Pre-compute a map of agent type → count for a list of sessions.
///
/// Use this to avoid O(n²) `is_sole_agent_in` calls when iterating
/// all sessions in listing/picker/relay code.
pub fn agent_counts(sessions: &[SessionInfo]) -> std::collections::HashMap<&'static str, usize> {
    let mut counts = std::collections::HashMap::new();
    for s in sessions {
        if let Some(agent) = session_agent(s) {
            *counts.entry(agent).or_insert(0) += 1;
        }
    }
    counts
}

/// Pre-compute counts for each agent and exact current working directory.
/// Detached agent runtimes can omit tmux identity from hook records; the count
/// lets callers use CWD only when it identifies one session unambiguously.
pub fn agent_cwd_counts(
    sessions: &[SessionInfo],
) -> std::collections::HashMap<(String, String), usize> {
    let mut counts = std::collections::HashMap::new();
    for session in sessions {
        let (Some(agent), Some(cwd)) = (session_agent(session), session_match_cwd(session)) else {
            continue;
        };
        *counts
            .entry((agent.to_string(), cwd.to_string()))
            .or_insert(0) += 1;
    }
    counts
}

pub fn is_sole_agent_cwd(
    session: &SessionInfo,
    counts: &std::collections::HashMap<(String, String), usize>,
) -> bool {
    let (Some(agent), Some(cwd)) = (session_agent(session), session_match_cwd(session)) else {
        return false;
    };
    counts
        .get(&(agent.to_string(), cwd.to_string()))
        .copied()
        .unwrap_or(0)
        == 1
}

fn session_match_cwd(session: &SessionInfo) -> Option<&str> {
    session
        .current_path
        .as_deref()
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| session.launch_cwd.as_deref().filter(|cwd| !cwd.is_empty()))
}

/// Check if this session is the only one running its agent type,
/// using a pre-computed agent count map.
pub fn is_sole_agent(
    session: &SessionInfo,
    counts: &std::collections::HashMap<&'static str, usize>,
) -> bool {
    match session_agent(session) {
        Some(agent) => counts.get(agent).copied().unwrap_or(0) == 1,
        None => false,
    }
}

use crate::state::ai_attn_state_dir;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(crate) struct AiAttnState {
    pub(crate) state: Option<String>,
    pub(crate) updated_at: i64,
    pub(crate) agent: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) pane_id: Option<String>,
    pub(crate) socket: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) reason: Option<String>,
}

/// Pre-loaded ai-attn state for efficient batch queries.
/// Load once with `AiAttnSnapshot::load()`, then pass to `session_ai_state()`.
pub struct AiAttnSnapshot {
    pub(crate) records: Vec<AiAttnState>,
}

impl AiAttnSnapshot {
    /// Load all ai-attn state files from disk into memory for batch queries.
    pub fn load() -> Self {
        let state_dir = ai_attn_state_dir();
        let entries = match fs::read_dir(state_dir) {
            Ok(entries) => entries,
            Err(_) => {
                return Self {
                    records: Vec::new(),
                };
            }
        };

        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let data = match fs::read_to_string(&path) {
                Ok(data) => data,
                Err(_) => continue,
            };
            let state: AiAttnState = match serde_json::from_str(&data) {
                Ok(state) => state,
                Err(_) => continue,
            };
            records.push(state);
        }

        Self { records }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AiAttnSnapshot, AiAttnState, SessionInfo, display_command, parse_sessions, session_ai_state,
    };
    use std::env;
    use std::fs;

    // Tests that well-formed tmux output is parsed into correct SessionInfo structs.
    #[test]
    fn test_parse_sessions() {
        let raw = "alpha\t100\t150\tcodex\tcodex --model x\t/home/user\t1\t%0\nbeta\t200\t240\tzsh\tzsh\t/home/user/tmp\t0\t%1\n";
        let sessions = parse_sessions(raw).expect("parse failed");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "alpha");
        assert_eq!(sessions[0].pane_id, "%0");
        assert_eq!(sessions[1].current_command, "zsh");
        assert_eq!(sessions[1].pane_id, "%1");
        assert_eq!(sessions[0].current_path.as_deref(), Some("/home/user"));
    }

    // Tests that empty activity epoch falls back to created epoch.
    #[test]
    fn test_parse_sessions_empty_activity_uses_created_time() {
        let raw = "demo-shell\t100\t\tbash\tbash\t/home/user\t0\t%0\n";
        let sessions = parse_sessions(raw).expect("parse failed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].created_epoch, 100);
        assert_eq!(sessions[0].last_activity_epoch, 100);
    }

    // Tests that display_command prefers launch_command over current_command.
    #[test]
    fn test_display_command_prefers_launch_command() {
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "node".to_string(),
            start_command: Some("node /x/y/z.js".to_string()),
            current_path: None,
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: None,
            attached_clients: 0,
        };
        assert_eq!(display_command(&session), "codex");
    }

    // Tests that a matching ai-attn record by pane_id returns the correct state.
    #[test]
    fn test_session_ai_state_true_for_matching_ai_attn_state() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        fs::write(
            tmp.join("entry.json"),
            format!(
                r#"{{"state":"waiting","updated_at":{},"agent":"codex","pane_id":"%0"}}"#,
                now
            ),
        )
        .expect("state");
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        assert!(session_ai_state(&session, true, false, &snapshot).is_some());
        let _ = fs::remove_dir_all(&tmp);
    }

    // Terminal-bell records came from a retired tmux fallback and must never
    // override a direct Codex lifecycle record, even when the bell is newer.
    #[test]
    fn test_session_ai_state_ignores_legacy_terminal_bell_record() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-recent-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        // The bell was emitted after the lifecycle Stop event, which was the
        // source of the false waiting state this regression covers.
        fs::write(
            tmp.join("bell.json"),
            format!(
                r#"{{"state":"waiting","reason":"terminal-bell","updated_at":{},"agent":"codex","pane_id":"%0"}}"#,
                now + 1
            ),
        )
        .expect("bell");
        // Codex's lifecycle hook is the authoritative final state.
        fs::write(
            tmp.join("notify.json"),
            format!(
                r#"{{"state":"done","updated_at":{},"agent":"codex","pane_id":"%0"}}"#,
                now
            ),
        )
        .expect("notify");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        let (state, _) = session_ai_state(&session, true, false, &snapshot).expect("match");
        assert_eq!(state, "done", "legacy terminal bells must be ignored");
        let _ = fs::remove_dir_all(&tmp);
    }

    // Tests that direct records from another tmux server do not match a pmux
    // backend pane even when the agent and pane ID collide.
    #[test]
    fn test_session_ai_state_ignores_non_pmux_socket_records() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-socket-scope-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        fs::write(
            tmp.join("outer.json"),
            format!(
                r#"{{"state":"waiting","updated_at":{},"agent":"codex","pane_id":"%0","socket":"/tmp/tmux-1000/outer"}}"#,
                now
            ),
        )
        .expect("outer");
        fs::write(
            tmp.join("backend.json"),
            format!(
                r#"{{"state":"done","updated_at":{},"agent":"codex","pane_id":"%0","socket":"/tmp/tmux-1000/{}"}}"#,
                now - 30,
                super::pmux_socket()
            ),
        )
        .expect("backend");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        let (state, _) = session_ai_state(&session, true, false, &snapshot).expect("match");
        assert_eq!(state, "done", "expected pmux backend record to win");
        let _ = fs::remove_dir_all(&tmp);
    }

    // Tests that non-AI commands (bash) return no ai state.
    #[test]
    fn test_session_ai_state_false_for_non_ai_command() {
        let now = super::now_epoch();
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "bash".to_string(),
            start_command: Some("bash".to_string()),
            current_path: Some("/tmp".to_string()),
            launch_command: Some("bash".to_string()),
            launch_cwd: Some("/tmp".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        assert!(session_ai_state(&session, true, false, &snapshot).is_none());
    }

    // Tests that agent-only fallback works when sole agent, but not when multiple exist.
    #[test]
    fn test_session_ai_state_true_on_agent_only_fallback() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-agent-only-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        fs::write(
            tmp.join("entry.json"),
            format!(
                r#"{{"state":"waiting","updated_at":{},"agent":"codex","pane_id":"%99"}}"#,
                now
            ),
        )
        .expect("state");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        assert!(session_ai_state(&session, true, false, &snapshot).is_some());
        // When not the sole agent, fallback should NOT trigger.
        assert!(session_ai_state(&session, false, false, &snapshot).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_session_ai_state_matches_unique_unscoped_agent_cwd() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-ai-attn-agent-cwd-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        fs::write(
            tmp.join("entry.json"),
            format!(
                r#"{{"state":"working","updated_at":{},"agent":"claude","pane_id":"","cwd":"/work/tweaker"}}"#,
                now
            ),
        )
        .expect("state");
        fs::write(
            tmp.join("older-scoped.json"),
            format!(
                r#"{{"state":"done","updated_at":{},"agent":"claude","pane_id":"%21","socket":"/tmp/tmux-1000/pmux","cwd":"/work/tweaker"}}"#,
                now - 30
            ),
        )
        .expect("older scoped state");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%21".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "claude".to_string(),
            start_command: Some("claude".to_string()),
            current_path: Some("/work/tweaker".to_string()),
            launch_command: Some("claude".to_string()),
            launch_cwd: Some("/work/tweaker".to_string()),
            attached_clients: 1,
        };
        let snapshot = AiAttnSnapshot::load();
        let matched = session_ai_state(&session, false, true, &snapshot).expect("cwd match");
        assert_eq!(matched.0, "working");
        let scoped = session_ai_state(&session, false, false, &snapshot).expect("scoped match");
        assert_eq!(scoped.0, "done");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_session_ai_state_does_not_cwd_match_scoped_record() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-ai-attn-scoped-cwd-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        fs::write(
            tmp.join("entry.json"),
            format!(
                r#"{{"state":"working","updated_at":{},"agent":"claude","pane_id":"","socket":"/tmp/tmux-1000/pmux","cwd":"/work/tweaker"}}"#,
                now
            ),
        )
        .expect("state");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%21".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "claude".to_string(),
            start_command: Some("claude".to_string()),
            current_path: Some("/work/tweaker".to_string()),
            launch_command: Some("claude".to_string()),
            launch_cwd: Some("/work/tweaker".to_string()),
            attached_clients: 1,
        };
        let snapshot = AiAttnSnapshot::load();
        assert!(session_ai_state(&session, false, true, &snapshot).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    // Old ai-attn records remain the session's last known signal. Freshness is
    // owned by ai-attn's storage lifecycle, not inferred again by pmux.
    #[test]
    fn test_session_ai_state_keeps_old_signal_regardless_of_ttl() {
        let now = super::now_epoch();
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot {
            records: vec![AiAttnState {
                state: Some("waiting".to_string()),
                updated_at: 1,
                agent: Some("codex".to_string()),
                session_id: None,
                pane_id: Some("%0".to_string()),
                socket: None,
                cwd: None,
                reason: None,
            }],
        };
        assert_eq!(
            session_ai_state(&session, true, false, &snapshot),
            Some(("waiting", 1))
        );
    }

    // Tests that malformed JSON state files are silently skipped.
    #[test]
    fn test_session_ai_state_ignores_malformed_state_files() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-malformed-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        fs::write(tmp.join("bad.json"), "{not-json").expect("bad");

        let now = super::now_epoch();
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        assert!(session_ai_state(&session, true, false, &snapshot).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    // Tests that relay records (agent: "pmux") are filtered out.
    #[test]
    fn test_session_ai_state_ignores_pmux_relay_records() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-relay-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        // This record has agent "pmux" — it's a relay record, not a direct agent signal.
        fs::write(
            tmp.join("relay.json"),
            format!(
                r#"{{"state":"waiting","updated_at":{},"agent":"pmux","pane_id":"%0"}}"#,
                now
            ),
        )
        .expect("state");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("codex --model gpt-5".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        // Should NOT match because agent is "pmux" (a relay record).
        assert!(session_ai_state(&session, true, false, &snapshot).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    // Tests that direct agent records (agent != "pmux") are matched.
    #[test]
    fn test_session_ai_state_matches_direct_agent_records() {
        // Shares the crate-wide env lock: other tests set PMUX_SOCKET,
        // which decides whether a socket-scoped record matches.
        let mut env = crate::EnvGuard::new();
        env.remove("PMUX_SOCKET");
        let tmp = env::temp_dir().join("pmux-needs-input-tmux-pane-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        env.set("AI_ATTN_STATE_DIR", &tmp);
        env.set("AI_ATTN_CONFIG", tmp.join("cfg.json"));
        fs::write(tmp.join("cfg.json"), r#"{"ttl_seconds":600}"#).expect("cfg");
        let now = super::now_epoch();
        // Direct agent record — agent is "claude", not "pmux".
        fs::write(
            tmp.join("direct.json"),
            format!(
                r#"{{"state":"waiting","updated_at":{},"agent":"claude","pane_id":"%5"}}"#,
                now
            ),
        )
        .expect("state");

        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%5".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: "node".to_string(),
            start_command: Some("claude".to_string()),
            current_path: Some("/home/user/pmux".to_string()),
            launch_command: Some("claude".to_string()),
            launch_cwd: Some("/home/user/pmux".to_string()),
            attached_clients: 0,
        };
        let snapshot = AiAttnSnapshot::load();
        // SHOULD match — agent "claude" is a direct agent signal, not a relay.
        assert!(session_ai_state(&session, true, false, &snapshot).is_some());
        let _ = fs::remove_dir_all(&tmp);
    }

    // T4: Malformed tmux output handling

    // Tests that lines with wrong number of tab-separated fields are skipped.
    #[test]
    fn test_parse_sessions_skips_lines_with_wrong_field_count() {
        // Lines with ≠8 tab-separated fields are silently skipped.
        let raw = "too-few\t100\t150\tcodex\n\
                   good\t200\t250\tclaud\tclaud\t/home\t0\t%0\n\
                   too-many\t300\t350\tcmd\tcmd\t/x\t0\t%0\textra\n";
        let sessions = parse_sessions(raw).expect("parse failed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "good");
    }

    // Tests that a non-numeric epoch field returns a parse error.
    #[test]
    fn test_parse_sessions_invalid_epoch_returns_error() {
        let raw = "demo\tNOT_A_NUMBER\t150\tcodex\tcodex\t/home\t0\t%0\n";
        assert!(parse_sessions(raw).is_err());
    }

    // Tests that a non-numeric attached count field returns a parse error.
    #[test]
    fn test_parse_sessions_invalid_attached_count_returns_error() {
        let raw = "demo\t100\t150\tcodex\tcodex\t/home\tNAN\t%0\n";
        assert!(parse_sessions(raw).is_err());
    }

    // Tests that empty optional fields (start_command, current_path) become None.
    #[test]
    fn test_parse_sessions_empty_optional_fields() {
        // start_command and current_path may be empty; they become None.
        let raw = "demo\t100\t150\tcodex\t\t\t0\t%0\n";
        let sessions = parse_sessions(raw).expect("parse failed");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].start_command.is_none());
        assert!(sessions[0].current_path.is_none());
    }

    // Tests that session_agent prefers launch_command agent over start_command.
    #[test]
    fn test_session_agent_priority() {
        use super::session_agent;
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "bash".to_string(),
            start_command: Some("gemini".to_string()),
            current_path: None,
            launch_command: Some("claude --model sonnet".to_string()),
            launch_cwd: None,
            attached_clients: 0,
        };
        assert_eq!(session_agent(&session), Some("claude"));
    }

    // Tests that session_agent falls back to start_command when launch_command is not an agent.
    #[test]
    fn test_session_agent_fallback() {
        use super::session_agent;
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "bash".to_string(),
            start_command: Some("codex --model gpt-5".to_string()),
            current_path: None,
            launch_command: Some("my-custom-tool".to_string()),
            launch_cwd: None,
            attached_clients: 0,
        };
        assert_eq!(session_agent(&session), Some("codex"));
    }

    // Tests that session_agent returns None when no fields contain a known agent.
    #[test]
    fn test_session_agent_none() {
        use super::session_agent;
        let session = SessionInfo {
            name: "demo".to_string(),
            pane_id: "%0".to_string(),
            created_epoch: 1,
            last_activity_epoch: 1,
            current_command: "bash".to_string(),
            start_command: Some("zsh".to_string()),
            current_path: None,
            launch_command: Some("my-tool --flag".to_string()),
            launch_cwd: None,
            attached_clients: 0,
        };
        assert_eq!(session_agent(&session), None);
    }
}
