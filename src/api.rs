//! Stable, machine-readable views of live pmux processes.
//!
//! The process lifecycle fields apply to every pmux session. Agent fields are
//! optional enrichment from ai-attn and are never required for a session to be
//! represented in this API.

use crate::session::{
    AiAttnSnapshot, SessionInfo, agent_counts, agent_cwd_counts, is_sole_agent, is_sole_agent_cwd,
    session_agent, session_ai_record,
};
use crate::util::now_epoch;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Version of the JSON schema emitted by `pmux ls`, `pmux inspect`, and
/// `pmux events`. Increment only for breaking schema changes.
pub const API_VERSION: u32 = 1;

/// A complete snapshot of the currently running pmux processes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StateSnapshot {
    pub api_version: u32,
    pub generated_at: i64,
    pub sessions: Vec<ProcessInfo>,
}

/// Machine-readable information about one running pmux process.
///
/// `lifecycle` is intentionally process-oriented rather than agent-oriented:
/// it is `"running"` for every live pmux session, including shells, REPLs,
/// test commands, and services. `agent` is present only for a recognized AI
/// command and may have no current ai-attn signal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProcessInfo {
    pub name: String,
    pub pane_id: String,
    pub lifecycle: String,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub attached_clients: i64,
    pub current_command: String,
    pub start_command: Option<String>,
    pub current_path: Option<String>,
    pub launch_command: Option<String>,
    pub launch_cwd: Option<String>,
    pub agent: Option<AgentInfo>,
    /// Present only for a recognized agent without a current-generation signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<crate::startup::StartupInfo>,
    /// Present only while ai-auto (auto mode) is on for the process's pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoInfo>,
    /// The process that started this one, when it is known and (for a local
    /// parent) still alive. See `crate::lineage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<crate::SpawnedBy>,
}

/// ai-auto state for one pane, read from the pane options ai-auto maintains.
///
/// While auto mode is on, ai-auto reviews the agent's permission prompts
/// itself. `escalated_at` is set when a review concluded a human must answer
/// the current prompt (with ai-auto's reason in `escalation`), and is cleared
/// when the next review starts. Notification consumers should stay quiet for
/// permission waits on such panes and notify on a new `escalated_at` instead.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AutoInfo {
    /// True only while the ai-auto daemon is serving the pane, judged by a
    /// heartbeat it refreshes every few seconds. A pane left with auto mode
    /// switched on after the daemon stopped reports `false`, so consumers go
    /// back to notifying for its permission prompts.
    pub enabled: bool,
    pub heartbeat_at: Option<i64>,
    pub status: Option<String>,
    pub escalated_at: Option<i64>,
    pub escalation: Option<String>,
}

/// Optional agent-specific information for a recognized agent command.
///
/// `state` is the most recent direct ai-attn value that can be matched to the
/// session. It is deliberately `null` rather than guessed when no such signal
/// exists. Consumers must not infer agent state from terminal output.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentInfo {
    pub kind: String,
    pub state: Option<String>,
    pub updated_at: Option<i64>,
    pub source: Option<String>,
    /// The ai-attn reason for `state`, e.g. `PermissionRequest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The agent's own conversation id (what `claude --resume` or
    /// `codex resume` takes). Only `pmux inspect` looks it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Build a state snapshot from already-listed sessions.
///
/// Keeping this transformation separate from tmux I/O makes the JSON contract
/// unit-testable and lets callers reuse one ai-attn read for the full list.
pub fn build_state_snapshot(sessions: &[SessionInfo]) -> StateSnapshot {
    let snapshot = AiAttnSnapshot::load();
    let mut state = build_state_snapshot_from(sessions, &snapshot, now_epoch());
    confirm_ready_screens(
        sessions,
        &mut state.sessions,
        &crate::agents::load_ready_texts(),
    );
    let auto = load_auto_infos(now_epoch());
    let mut parents = crate::lineage::live_parents(sessions);
    for process in &mut state.sessions {
        process.auto = auto.get(&process.pane_id).cloned();
        process.spawned_by = parents.remove(&process.name);
    }
    state
}

/// Marks unconfirmed agents ready when their configured ready text
/// (`[agents.<kind>] ready_text`) is on the visible screen. A match is saved
/// with the launch record, so the pane is captured only until it first
/// matches; respawn and restore start a new record.
fn confirm_ready_screens(
    sessions: &[SessionInfo],
    processes: &mut [ProcessInfo],
    ready_texts: &crate::agents::ReadyTexts,
) {
    if ready_texts.is_empty() {
        return;
    }
    for process in processes {
        let (Some(agent), Some(startup)) = (&process.agent, &mut process.startup) else {
            continue;
        };
        let texts = ready_texts.for_agent(&agent.kind);
        if texts.is_empty() {
            continue;
        }
        let created_epoch = sessions
            .iter()
            .find(|session| session.name == process.name)
            .map_or(process.created_at, |session| session.created_epoch);
        let saved = crate::startup::read_start(&process.name, created_epoch)
            .and_then(|start| start.ready_at);
        if let Some(ready_at) = saved {
            startup.mark_ready(ready_at);
            continue;
        }
        let Some(screen) = visible_screen(&process.pane_id) else {
            continue;
        };
        startup.ready_text_seen = Some(false);
        if crate::startup::screen_shows_ready(&screen, texts) {
            let now = now_epoch();
            startup.mark_ready(now);
            // Best effort: without the record the pane is simply captured
            // again next time.
            let _ = crate::startup::mark_ready(&process.name, created_epoch, now);
        }
    }
}

/// The pane's visible screen as plain text, or None if tmux cannot capture it.
fn visible_screen(pane_id: &str) -> Option<String> {
    let output = crate::tmux::tmux_cmd()
        .args(["capture-pane", "-p", "-t", pane_id])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .split('\n')
            .map(crate::util::sanitize_terminal_text)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

// ai-auto's pane options, keyed by pane id. One tmux call per snapshot;
// panes without auto mode on are omitted.
/// How stale ai-auto's heartbeat may be before its pane counts as unserved.
const AUTO_HEARTBEAT_STALE_SECS: i64 = 30;

fn load_auto_infos(now: i64) -> HashMap<String, AutoInfo> {
    let output = crate::tmux::tmux_cmd()
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{@ai_auto}\t#{@ai_auto_heartbeat}\t#{@ai_auto_status}\t#{@ai_auto_escalated_at}\t#{@ai_auto_escalation}",
        ])
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_auto_infos(&String::from_utf8_lossy(&output.stdout), now)
}

pub(crate) fn parse_auto_infos(raw: &str, now: i64) -> HashMap<String, AutoInfo> {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    raw.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(6, '\t');
            let pane = cols.next()?;
            if cols.next()? != "on" {
                return None;
            }
            let heartbeat_at: Option<i64> = cols.next().unwrap_or_default().parse().ok();
            let status = cols.next().unwrap_or_default();
            let escalated_at = cols.next().unwrap_or_default();
            let escalation = cols.next().unwrap_or_default();
            Some((
                pane.to_string(),
                AutoInfo {
                    enabled: heartbeat_at.is_some_and(|at| now - at <= AUTO_HEARTBEAT_STALE_SECS),
                    heartbeat_at,
                    status: non_empty(status),
                    escalated_at: escalated_at.parse().ok(),
                    escalation: non_empty(escalation),
                },
            ))
        })
        .collect()
}

pub(crate) fn build_state_snapshot_from(
    sessions: &[SessionInfo],
    snapshot: &AiAttnSnapshot,
    generated_at: i64,
) -> StateSnapshot {
    let counts = agent_counts(sessions);
    let cwd_counts = agent_cwd_counts(sessions);
    let sessions = sessions
        .iter()
        .map(|session| {
            let agent = session_agent(session).map(|kind| {
                let signal = session_ai_record(
                    session,
                    is_sole_agent(session, &counts),
                    is_sole_agent_cwd(session, &cwd_counts),
                    snapshot,
                );
                AgentInfo {
                    kind: kind.to_string(),
                    state: signal.map(|r| r.state.clone().unwrap_or_default()),
                    updated_at: signal.map(|r| r.updated_at),
                    source: signal.map(|_| "ai-attn".to_string()),
                    reason: signal
                        .and_then(|r| r.reason.clone())
                        .filter(|r| !r.is_empty()),
                    session_id: None,
                }
            });

            let startup = agent
                .as_ref()
                .filter(|agent| agent.state.is_none())
                .map(|_| {
                    let started_at =
                        crate::startup::read_start(&session.name, session.created_epoch)
                            .map(|start| start.started_at)
                            .unwrap_or(session.created_epoch);
                    crate::startup::unconfirmed(&session.name, started_at, generated_at)
                });
            ProcessInfo {
                name: session.name.clone(),
                pane_id: session.pane_id.clone(),
                lifecycle: "running".to_string(),
                created_at: session.created_epoch,
                last_activity_at: session.last_activity_epoch,
                attached_clients: session.attached_clients,
                current_command: session.current_command.clone(),
                start_command: session.start_command.clone(),
                current_path: session.current_path.clone(),
                launch_command: session.launch_command.clone(),
                launch_cwd: session.launch_cwd.clone(),
                agent,
                startup,
                auto: None,
                spawned_by: None,
            }
        })
        .collect();

    StateSnapshot {
        api_version: API_VERSION,
        generated_at,
        sessions,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn auto_infos_parse_only_enabled_panes() {
        let raw = "%1\ton\t995\ty(x2)\t1792191785\tscript unavailable\n%2\t\t\t\t\t\n%3\ton\t\t\t\t\n%4\ton\t900\t\t\t\n";
        let infos = super::parse_auto_infos(raw, 1000);
        assert_eq!(infos.len(), 3);
        let one = &infos["%1"];
        assert!(one.enabled, "fresh heartbeat means served");
        assert_eq!(one.status.as_deref(), Some("y(x2)"));
        assert_eq!(one.escalated_at, Some(1792191785));
        assert_eq!(one.escalation.as_deref(), Some("script unavailable"));
        assert!(!infos["%3"].enabled, "no heartbeat means no daemon");
        assert!(
            !infos["%4"].enabled,
            "a stale heartbeat means the daemon stopped"
        );
        assert_eq!(infos["%4"].heartbeat_at, Some(900));
    }

    use super::build_state_snapshot_from;
    use crate::session::{AiAttnSnapshot, AiAttnState, SessionInfo};

    fn session(name: &str, command: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pane_id: "%1".to_string(),
            created_epoch: 10,
            last_activity_epoch: 20,
            current_command: command.to_string(),
            start_command: Some(command.to_string()),
            current_path: Some("/workspace".to_string()),
            launch_command: Some(command.to_string()),
            launch_cwd: Some("/workspace".to_string()),
            attached_clients: 2,
        }
    }

    #[test]
    fn represents_non_agent_processes_without_agent_state() {
        let sessions = vec![session("shell", "zsh")];
        let snapshot = AiAttnSnapshot {
            records: Vec::new(),
        };

        let state = build_state_snapshot_from(&sessions, &snapshot, 40);
        assert_eq!(state.api_version, 1);
        assert_eq!(state.generated_at, 40);
        assert_eq!(state.sessions[0].lifecycle, "running");
        assert_eq!(state.sessions[0].agent, None);
        assert_eq!(state.sessions[0].current_command, "zsh");
        let json = serde_json::to_value(&state).expect("serialize state");
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["sessions"][0]["lifecycle"], "running");
        assert!(json["sessions"][0]["agent"].is_null());
    }

    #[test]
    fn enriches_recognized_agents_with_matching_ai_attn_state() {
        let sessions = vec![session("agent", "codex")];
        let snapshot = AiAttnSnapshot {
            records: vec![AiAttnState {
                state: Some("waiting".to_string()),
                updated_at: 25,
                agent: Some("codex".to_string()),
                session_id: None,
                pane_id: Some("%1".to_string()),
                socket: None,
                cwd: None,
                reason: None,
            }],
        };

        let state = build_state_snapshot_from(&sessions, &snapshot, 40);
        let agent = state.sessions[0].agent.as_ref().expect("agent metadata");
        assert_eq!(agent.kind, "codex");
        assert_eq!(agent.state.as_deref(), Some("waiting"));
        assert_eq!(agent.updated_at, Some(25));
        assert_eq!(agent.source.as_deref(), Some("ai-attn"));
    }

    // The JSON contract is public: external tools parse it. Renaming,
    // retyping, or dropping a field must fail here, not in someone's script.
    // Additive changes update the fixture; breaking ones bump API_VERSION.
    #[test]
    fn serialized_state_matches_the_published_v1_fixture() {
        let sessions = vec![session("agent", "codex"), session("shell", "zsh")];
        let snapshot = AiAttnSnapshot {
            records: vec![AiAttnState {
                state: Some("waiting".to_string()),
                updated_at: 25,
                agent: Some("codex".to_string()),
                session_id: None,
                pane_id: Some("%1".to_string()),
                socket: None,
                cwd: None,
                reason: None,
            }],
        };

        let state = build_state_snapshot_from(&sessions, &snapshot, 40);
        let actual = serde_json::to_value(&state).expect("serialize state");
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/api/state-v1.json"))
                .expect("parse fixture");
        assert_eq!(actual, expected);
    }
}
