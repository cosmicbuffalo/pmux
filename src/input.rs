//! Delivered input supersedes an old approval signal until the agent updates it.
use crate::api::{AgentInfo, ProcessInfo, build_state_snapshot};
use crate::session::SessionInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InputReceipt {
    pane_id: String,
    created_epoch: i64,
    delivered_at: i64,
    // ai-attn timestamps have second precision. Remember the exact pre-input
    // signal as well, so an approval answered in that second stays superseded.
    previous_waiting: Option<AgentInfo>,
}

impl InputReceipt {
    pub fn before_send(session: &SessionInfo, sessions: &[SessionInfo]) -> Self {
        let previous_waiting = build_state_snapshot(sessions)
            .sessions
            .into_iter()
            .find(|p| p.pane_id == session.pane_id)
            .and_then(|p| p.agent)
            .filter(|a| a.state.as_deref() == Some("waiting"));
        Self {
            pane_id: session.pane_id.clone(),
            created_epoch: session.created_epoch,
            delivered_at: 0,
            previous_waiting,
        }
    }

    pub fn record(mut self, name: &str) -> Result<()> {
        self.delivered_at = crate::util::now_epoch();
        crate::state::write_state_file(
            &crate::state::pmux_last_inputs_dir()?.join(name),
            &serde_json::to_string(&self)?,
        )
        .context("input was delivered, but recording its receipt failed; do not blindly resend")
    }

    fn supersedes(&self, agent: &AgentInfo) -> bool {
        agent.state.as_deref() == Some("waiting")
            && (agent.updated_at.is_some_and(|at| at < self.delivered_at)
                || self.previous_waiting.as_ref() == Some(agent))
    }
}

pub(crate) fn waiting_was_answered(process: &ProcessInfo) -> bool {
    let read = || -> Option<InputReceipt> {
        let path = crate::state::pmux_last_inputs_dir()
            .ok()?
            .join(&process.name);
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    };
    read().is_some_and(|receipt| {
        receipt.pane_id == process.pane_id
            && receipt.created_epoch == process.created_at
            && process
                .agent
                .as_ref()
                .is_some_and(|a| receipt.supersedes(a))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_rejects_old_waiting_but_accepts_new_signals() {
        let mut agent = AgentInfo {
            kind: "codex".into(),
            state: Some("waiting".into()),
            updated_at: Some(10),
            source: Some("ai-attn".into()),
            reason: Some("PermissionRequest".into()),
            session_id: None,
        };
        let receipt = InputReceipt {
            pane_id: "%1".into(),
            created_epoch: 1,
            delivered_at: 10,
            previous_waiting: Some(agent.clone()),
        };
        assert!(receipt.supersedes(&agent));
        agent.reason = Some("AnotherPermissionRequest".into());
        assert!(!receipt.supersedes(&agent)); // distinct request in the same second
        agent.updated_at = Some(9);
        assert!(receipt.supersedes(&agent)); // older regardless of reason
        agent.updated_at = Some(11);
        assert!(!receipt.supersedes(&agent));
        agent.state = Some("done".into());
        agent.updated_at = Some(9);
        assert!(!receipt.supersedes(&agent)); // only waiting is superseded
    }
}
