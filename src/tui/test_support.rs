//! Builders shared by the tui submodules' tests.

use super::*;

pub(super) fn test_process(
    current_cwd: Option<&str>,
    launch_cwd: Option<&str>,
) -> crate::SidebarProcess {
    crate::SidebarProcess {
        process: crate::ProcessInfo {
            name: "demo".to_string(),
            pane_id: "%2".to_string(),
            lifecycle: "running".to_string(),
            created_at: 1,
            last_activity_at: 2,
            attached_clients: 0,
            current_command: "sleep".to_string(),
            start_command: None,
            current_path: current_cwd.map(str::to_string),
            launch_command: None,
            launch_cwd: launch_cwd.map(str::to_string),
            agent: None,
            startup: None,
            auto: None,
            spawned_by: None,
        },
        outer_location: None,
        outer_locations: Vec::new(),
    }
}

pub(super) fn test_context() -> crate::SidebarContext {
    crate::SidebarContext {
        socket: "/tmp/outer".to_string(),
        host_pane: "%1".to_string(),
        outer_session_id: "$1".to_string(),
        host_window_id: "@1".to_string(),
        sidebar_on_right: false,
    }
}

pub(super) fn ready_activity() -> crate::DrawerState {
    crate::DrawerState::Ready(Box::new(crate::ActivityData {
        churn: vec![(100, 5.0), (200, 8.0), (300, 3.0)],
        out_tokens: vec![(100, 40.0), (250, 90.0)],
        in_tokens: vec![(100, 400.0), (250, 900.0)],
        prompts: 3,
        total_in: 45_678,
        total_out: 12_345,
        added: 10,
        deleted: 4,
        tools: 7,
        first: Some(100),
        last: Some(300),
        summary: Some("do the thing".to_string()),
        recent_prompts: vec!["do the thing".to_string()],
    }))
}

pub(super) fn open_test_drawers(app: &mut SidebarApp, names: &[&str]) {
    app.open_drawers = names.iter().map(|name| (*name).to_string()).collect();
}

/// A past (not running) claude session named `id`, last active at `epoch`.
pub(super) fn test_history_entry(id: &str, cwd: &str, epoch: i64) -> crate::HistoryEntry {
    crate::HistoryEntry {
        agent: "claude".to_string(),
        session_id: id.to_string(),
        title: Some(format!("title {id}")),
        title_source: Some(crate::TitleSource::Ai),
        first_prompt: None,
        cwd: Some(cwd.to_string()),
        cwd_exists: true,
        git_branch: None,
        started_epoch: None,
        last_activity_epoch: epoch,
        transcript: format!("/tmp/{id}.jsonl"),
        live: false,
        pmux_process: None,
        resume_command: Some(vec![
            "claude".to_string(),
            "--resume".to_string(),
            id.to_string(),
        ]),
    }
}
