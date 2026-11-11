//! Agent launch generations keep a pre-respawn ai-attn signal from looking ready.
use crate::session::{AiAttnSnapshot, AiAttnState};
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub(crate) const STARTUP_GRACE_SECS: i64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentStart {
    pub created_epoch: i64,
    pub started_at: i64,
    // Exact pre-launch records also reject stale signals with second-resolution
    // timestamps equal to started_at. New changed records in that second count.
    pub previous: Vec<AiAttnState>,
    /// When the agent's configured ready text was first seen on screen during
    /// this launch, so later snapshots need not capture the pane again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<i64>,
}

impl AgentStart {
    pub fn new(created_epoch: i64, previous: AiAttnSnapshot) -> Self {
        Self {
            created_epoch,
            started_at: crate::util::now_epoch(),
            previous: previous.records,
            ready_at: None,
        }
    }
    pub fn accepts(&self, record: &AiAttnState) -> bool {
        record.updated_at >= self.started_at && !self.previous.contains(record)
    }
}

pub(crate) fn read_start(name: &str, created_epoch: i64) -> Option<AgentStart> {
    let path = crate::state::pmux_agent_starts_dir().ok()?.join(name);
    let start: AgentStart = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (start.created_epoch == created_epoch).then_some(start)
}

pub(crate) fn write_start(name: &str, start: &AgentStart) -> Result<()> {
    crate::state::write_state_file(
        &crate::state::pmux_agent_starts_dir()?.join(name),
        &serde_json::to_string(start)?,
    )
}

pub(crate) fn restore_start(name: &str, previous: Option<AgentStart>) -> Result<()> {
    if let Some(start) = previous {
        write_start(name, &start)
    } else {
        match std::fs::remove_file(crate::state::pmux_agent_starts_dir()?.join(name)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupInfo {
    pub started_at: i64,
    /// `starting`, `not_ready`, or `ready` when the agent's configured ready
    /// text is on screen (`[agents.<kind>] ready_text`).
    pub status: String,
    pub capture_command: String,
    /// When the ready text was first seen; only set with status `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<i64>,
    /// What confirmed readiness: `screen` for configured ready text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Whether the agent's configured ready text is on screen; absent when
    /// none is configured for its kind. `false` while unconfirmed usually
    /// means a startup or approval dialog is showing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_text_seen: Option<bool>,
}

impl StartupInfo {
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }

    /// Marks the agent ready from its configured ready text, first seen at
    /// `ready_at`.
    pub(crate) fn mark_ready(&mut self, ready_at: i64) {
        self.status = "ready".into();
        self.ready_at = Some(ready_at);
        self.source = Some("screen".into());
        self.ready_text_seen = Some(true);
    }

    /// Whether configured ready text exists for this agent but is not on
    /// screen, so the agent is likely showing a startup or approval dialog.
    pub fn ready_text_missing(&self) -> bool {
        self.ready_text_seen == Some(false)
    }
}

/// Whether any of `texts` appears on one line of the visible `screen`. Empty
/// texts never match.
pub(crate) fn screen_shows_ready(screen: &str, texts: &[String]) -> bool {
    screen.lines().any(|line| {
        texts
            .iter()
            .any(|text| !text.is_empty() && line.contains(text.as_str()))
    })
}

/// Records that `name`'s current launch showed its ready text at `ready_at`.
/// Does nothing for a session pmux has no launch record for.
pub(crate) fn mark_ready(name: &str, created_epoch: i64, ready_at: i64) -> Result<()> {
    let Some(mut start) = read_start(name, created_epoch) else {
        return Ok(());
    };
    if start.ready_at.is_none() {
        start.ready_at = Some(ready_at);
        write_start(name, &start)?;
    }
    Ok(())
}

pub(crate) fn unconfirmed(name: &str, started_at: i64, now: i64) -> StartupInfo {
    StartupInfo {
        started_at,
        status: if now.saturating_sub(started_at) < STARTUP_GRACE_SECS {
            "starting"
        } else {
            "not_ready"
        }
        .into(),
        capture_command: crate::util::shell_join(&["pmux".into(), "capture".into(), name.into()]),
        ready_at: None,
        source: None,
        ready_text_seen: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_text_matches_within_one_screen_line() {
        let texts = vec!["Ask Codex to do anything".to_string()];
        assert!(screen_shows_ready(
            "  banner\n\u{203a} Ask Codex to do anything\n  ? for shortcuts\n",
            &texts
        ));
        assert!(!screen_shows_ready(
            "  Trust this folder?\n\u{203a} 1. Trust and continue\n",
            &texts
        ));
        assert!(!screen_shows_ready("anything", &[String::new()]));
        assert!(!screen_shows_ready("Ask Codex to\ndo anything", &texts));
    }

    #[test]
    fn launch_records_without_ready_at_still_load() {
        let start: AgentStart =
            serde_json::from_str(r#"{"created_epoch":1,"started_at":2,"previous":[]}"#).unwrap();
        assert_eq!(start.ready_at, None);
        let json = serde_json::to_string(&start).unwrap();
        assert!(!json.contains("ready_at"), "{json}");
    }

    #[test]
    fn unconfirmed_startup_json_is_unchanged_until_ready() {
        let mut info = unconfirmed("worker", 100, 110);
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["status"], "not_ready");
        assert!(
            json.get("ready_at").is_none()
                && json.get("source").is_none()
                && json.get("ready_text_seen").is_none(),
            "{json}"
        );
        info.mark_ready(108);
        assert!(info.is_ready());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            (json["ready_at"].as_i64(), json["source"].as_str()),
            (Some(108), Some("screen"))
        );
    }
}
