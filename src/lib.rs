//! The pmux binary's implementation. `main.rs` only calls [`cli_main`]; the
//! rest of the crate is private and reached by its modules through the
//! crate-internal re-exports below. Machine consumers should use the CLI's
//! versioned JSON (`pmux ls --json`, `pmux events`), not this crate.

mod activity;
mod agent_docs;
mod agent_rename;
mod agents;
mod api;
mod attach_picker;
mod cli;
mod clipboard;
mod commands;
mod config;
mod doctor_tmux;
mod dredge;
mod groups;
mod history;
mod history_search;
mod input;
mod key_hints;
mod keys;
mod lineage;
mod onboard;
mod queue;
mod queue_time;
mod queue_ui;
mod relay;
mod render;
mod repair;
mod restore;
mod session;
mod sidebar;
mod startup;
mod state;
mod status_labels;
mod tmux;
mod tui;
mod util;

/// Run the pmux command line. Called by `main.rs`.
pub fn cli_main() -> anyhow::Result<()> {
    cli::run().map_err(tmux::explain_tmux_failure)
}

pub(crate) use activity::{
    AXIS_PRESETS, ActivityData, DEFAULT_AXIS_INDEX, DrawerMetric, DrawerState, braille_rows,
    load_activity, load_transcript_activity, warp_bucket_peaks, warp_buckets,
};
pub(crate) use api::{API_VERSION, ProcessInfo, StateSnapshot, build_state_snapshot};
pub(crate) use commands::{
    ParentReport, RespawnOptions, RestartResult, WaitCondition, attach, history,
    init_with_autostart, inspect, kill, kill_current_and_next, kill_spawned, last, ls, next_from,
    on_focus, parent, prev_from, prompt_for_row_preset, rename_session, respawn, restart, resume,
    row_preset_list, run, run_in_cwd, send, set_row_preset, spawn, spawned_names, state, unwrap,
    update_all_host_panes, update_host_pane, wait, wrap, wraps,
};
pub(crate) use groups::{
    AutomaticGrouping, GroupingChange, SidebarGroupingState, add_process_to_manual_group,
    create_manual_group, cycle_automatic_grouping, dissolve_manual_group, read_sidebar_grouping,
    reorder_automatic_group, reorder_manual_group, save_grouping_change,
};
pub(crate) use history::{HistoryEntry, HistoryQuery};
pub(crate) use lineage::{ParentSpec, SpawnedBy};
pub(crate) use relay::{
    debug_ai_attn_session, doctor_ai_attn, relay_ai_attn, relay_ai_attn_dry_run,
    restart_ai_attn_relay,
};
pub(crate) use restore::{doctor as restore_doctor, restore, snapshot};
pub(crate) use session::{
    AiAttnSnapshot, SessionInfo, agent_counts, agent_cwd_counts, display_command, is_sole_agent,
    is_sole_agent_cwd, session_ai_state,
};
pub(crate) use sidebar::{
    DEFAULT_SIDEBAR_WIDTH, FLASH_PHASE_DURATION, OuterPaneLocation, SIDEBAR_QUERY_TIMEOUT,
    SidebarContext, SidebarProcess, apply_tmux_init, attach_group_in_new_outer_window,
    attach_in_new_outer_split, attach_in_new_outer_window, clear_stale_flash_options,
    close_window_if_only_sidebar, display_session_name, equalize_panes, flash_outer_location,
    flash_phase_on, jump_to_outer_pane, picker_processes, record_sidebar_width,
    resolve_sidebar_host, resume_in_new_outer_split, resume_in_new_outer_window, sidebar_agent_tag,
    sidebar_attachment_detail_at, sidebar_context, sidebar_double_click_installed,
    sidebar_process_cwd, sidebar_processes, stabilize_sidebar_width, stop_outer_flash,
    tmux_init_script, toggle_sidebar,
};
pub(crate) use tmux::{current_session_name, list_sessions};
pub(crate) use util::{
    display_path, display_path_buf, display_width, relative_age, resolve_pmux_binary, truncate_col,
};
// Referenced by name only from tests.
#[cfg(test)]
pub(crate) use api::AgentInfo;
#[cfg(test)]
pub(crate) use groups::ManualGroup;
#[cfg(test)]
pub(crate) use history::TitleSource;
#[cfg(test)]
pub(crate) use sidebar::sidebar_attachment_detail;

/// Mutex used by all test modules that need to temporarily mutate HOME or SHELL.
/// Holding this lock ensures env-sensitive tests don't race each other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Scoped environment changes for tests.
///
/// Holds [`ENV_LOCK`] for its lifetime and restores every variable it changed
/// when dropped, including when the test panics, so one test can never leak
/// HOME, SHELL, TMUX, or PMUX_SOCKET into the tests that run after it.
#[cfg(test)]
pub(crate) struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl EnvGuard {
    pub(crate) fn new() -> Self {
        Self {
            saved: Vec::new(),
            _lock: ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    fn remember(&mut self, key: &'static str) {
        if !self.saved.iter().any(|(saved, _)| *saved == key) {
            self.saved.push((key, std::env::var_os(key)));
        }
    }

    pub(crate) fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        self.remember(key);
        // SAFETY: every env-mutating test holds ENV_LOCK through this guard.
        unsafe { std::env::set_var(key, value) };
    }

    pub(crate) fn remove(&mut self, key: &'static str) {
        self.remember(key);
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(key) };
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: the lock is still held; it is released after this runs.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[cfg(test)]
mod env_guard_tests {
    #[test]
    fn env_guard_restores_variables_even_when_the_test_panics() {
        let key = "PMUX_ENV_GUARD_TEST";
        let result = std::panic::catch_unwind(|| {
            let mut env = crate::EnvGuard::new();
            env.set(key, "inside");
            panic!("simulated test failure");
        });
        assert!(result.is_err());
        let _env = crate::EnvGuard::new();
        assert_eq!(std::env::var_os(key), None);
    }
}
