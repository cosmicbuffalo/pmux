//! Tracking outer-tmux focus: the SIGUSR1 refresh nudge, focus polling, and
//! following the process in the active outer pane.

use super::*;

/// The focus, selection, and viewport state a focus poll can change. See
/// [`SidebarApp::view_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct ViewState {
    window_active: bool,
    sidebar_focused: bool,
    active_outer_pane: Option<String>,
    focused_outer_pane: Option<String>,
    selected: usize,
    header_selected: bool,
    first_visible: usize,
    first_visible_line: usize,
    /// Changes when a focus change auto-unfolds a group, even if the selected
    /// row index happens to stay the same.
    row_count: usize,
}

/// Set by the SIGUSR1 handler when the outer focus-hook nudges this sidebar to
/// re-check focus immediately (see #3). The main loop consumes it each tick, so
/// even if the signal does not interrupt the event poll the flag is observed
/// within one `EVENT_POLL` interval.
static FOCUS_REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_focus_refresh_signal(_sig: libc::c_int) {
    // Async-signal-safe: an atomic store is the only work done here.
    FOCUS_REFRESH_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the SIGUSR1 handler that lets the outer `pane-focus-in` hook tell
/// this sidebar to refresh now instead of waiting out its (possibly backed-off)
/// poll interval. Safe to call once at loop start; failure is non-fatal because
/// the periodic poll still keeps focus state correct on its own.
pub(in crate::tui) fn install_focus_refresh_handler() {
    // SAFETY: `handle_focus_refresh_signal` only performs an atomic store, which
    // is async-signal-safe, and we install it exactly once before the loop.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            handle_focus_refresh_signal as *const () as usize as libc::sighandler_t,
        );
    }
}

/// Whether the outer focus-hook has requested an immediate refresh since the
/// last check, clearing the request in the process.
pub(in crate::tui) fn take_focus_refresh_request() -> bool {
    FOCUS_REFRESH_REQUESTED.swap(false, Ordering::SeqCst)
}

/// Outer-tmux focus state for the sidebar's own window, gathered in a single
/// `list-panes` call so the poll cadence can adapt to visibility (see #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct OuterFocus {
    /// True when the sidebar's window is the active (visible) window of its
    /// session. All panes in a window share this flag.
    pub(in crate::tui) window_active: bool,
    /// The active pane in that window, present only while the window is active.
    pub(in crate::tui) active_pane: Option<String>,
}

impl SidebarApp {
    /// Whether outer focus should be queried now: the focus hook asked for it,
    /// or the (visibility-dependent) poll interval has elapsed. A sidebar that
    /// does not know its own pane never queries.
    pub(in crate::tui) fn focus_refresh_due(&self, now: Instant) -> bool {
        self.sidebar_pane.is_some()
            && (self.force_focus_refresh
                || now.saturating_duration_since(self.last_focus_refresh)
                    >= self.focus_poll_interval)
    }

    /// Restart the focus poll interval when a query has been started.
    pub(in crate::tui) fn mark_focus_requested(&mut self, now: Instant) {
        self.force_focus_refresh = false;
        self.last_focus_refresh = now;
    }

    /// Apply an outer-focus observation from the background poller.
    pub(in crate::tui) fn apply_focus(&mut self, focus: OuterFocus, list_height: u16) {
        let Some(sidebar_pane) = self.sidebar_pane.as_deref() else {
            return;
        };
        // #2: back off hard while our window is in the background. The
        // sidebar is not visible there, so its highlight only needs to be
        // right once the window is foregrounded again — which the focus-hook
        // signal (#3) makes instant. This is what stops several open-but-
        // unwatched sidebars from polling the outer server five times a
        // second each.
        self.focus_poll_interval = match (focus.window_active, self.focus_hook_fires) {
            (true, true) => FOCUS_POLL_HOOKED,
            (true, false) => FOCUS_POLL_ACTIVE,
            (false, _) => FOCUS_POLL_INACTIVE,
        };
        self.window_active = focus.window_active;
        self.sidebar_focused = focus.active_pane.as_deref() == Some(sidebar_pane);
        self.active_outer_pane = focus.active_pane.filter(|pane| pane != sidebar_pane);
        if self.active_outer_pane.is_some() {
            self.focused_outer_pane = self.active_outer_pane.clone();
        }
        let active_process_changed = self.follow_active_outer_process();
        // Reveal a newly active process once. Subsequent focus polls leave
        // manual mouse-wheel scrolling alone until the active process
        // changes again.
        if active_process_changed {
            // Do the same on the focus-poll path as on a state-stream
            // refresh. Otherwise jumping between pmux panes can leave the
            // newly selected drawer clipped at the bottom.
            self.ensure_followed_process_visible(list_height);
        } else if self.sidebar_focused {
            self.ensure_visible(list_height);
        }
    }

    /// The focus, selection, and viewport state a focus poll can change. The
    /// run loop compares it before and after `apply_focus` so an unchanged
    /// poll does not force a redraw.
    pub(in crate::tui) fn view_state(&self) -> ViewState {
        ViewState {
            window_active: self.window_active,
            sidebar_focused: self.sidebar_focused,
            active_outer_pane: self.active_outer_pane.clone(),
            focused_outer_pane: self.focused_outer_pane.clone(),
            selected: self.selected,
            header_selected: self.header_selected,
            first_visible: self.first_visible,
            first_visible_line: self.first_visible_line,
            row_count: self.rows.len(),
        }
    }

    /// How stale the process-location view may get before the periodic refresh
    /// re-queries the outer server, widened while the window is backgrounded so
    /// hidden sidebars stop polling for locations nobody is looking at (#2).
    pub(in crate::tui) fn location_poll_interval(&self) -> Duration {
        if self.window_active {
            LOCATION_POLL_ACTIVE
        } else {
            LOCATION_POLL_INACTIVE
        }
    }

    /// Select the process attached to the active outer pane and report whether
    /// that pane moved to a different pmux process since the previous refresh.
    pub(super) fn follow_active_outer_process(&mut self) -> bool {
        let Some(active_pane) = self.active_outer_pane.clone() else {
            self.followed_active_process = None;
            return false;
        };
        if let Some(process_index) = self.processes.iter().position(|process| {
            process
                .attachments()
                .iter()
                .any(|location| location.pane_id == active_pane)
        }) {
            let followed = format!(
                "{active_pane}\t{}",
                self.processes[process_index].process.name
            );
            let changed = self.followed_active_process.as_deref() != Some(followed.as_str());
            self.followed_active_process = Some(followed);
            if changed {
                self.reveal_landed_process(process_index, &active_pane);
            }
            self.header_selected = false;
            self.selected = if self.rows.is_empty() {
                process_index
            } else {
                // A process attached in several windows has a row per window;
                // land on the one for the pane that is actually focused.
                let rows_for_process = || {
                    self.rows.iter().enumerate().filter(|(_, row)| {
                        matches!(row, SidebarListRow::Process { process_index: index, .. } if *index == process_index)
                    })
                };
                rows_for_process()
                    .find(|(_, row)| {
                        matches!(row, SidebarListRow::Process { location: Some(location), .. } if location.pane_id == active_pane)
                    })
                    .or_else(|| rows_for_process().next())
                    .map_or(self.selected, |(index, _)| index)
            };
            return changed;
        }
        self.followed_active_process = None;
        false
    }

    /// Unfold the folded group(s) hiding a process focus just landed on, so its
    /// row can be selected. A group holding the exact attachment that was
    /// landed on (its window, under window grouping) wins; otherwise every
    /// group listing the process is an equal candidate. Nothing unfolds when a
    /// winning candidate is already open, and the result is persisted like a
    /// manual unfold (the grouping state is reloaded on every snapshot).
    fn reveal_landed_process(&mut self, process_index: usize, active_pane: &str) {
        let reveal = self.groups_to_reveal(process_index, active_pane);
        if reveal.is_empty() {
            return;
        }
        if let Err(err) = self.save_grouping(crate::GroupingChange::Unfold(reveal)) {
            self.message = Some(format!("Could not save group fold state: {err}"));
        }
        self.rebuild_rows();
    }

    /// The folded groups [`Self::reveal_landed_process`] should open.
    pub(in crate::tui) fn groups_to_reveal(
        &self,
        process_index: usize,
        active_pane: &str,
    ) -> Vec<String> {
        let Some(process) = self.processes.get(process_index) else {
            return Vec::new();
        };
        // `(group identity, matches the landed attachment)`; `None` is the
        // ungrouped list, which cannot fold.
        let mut candidates: Vec<(Option<String>, bool)> = Vec::new();
        if self.grouping.automatic == crate::AutomaticGrouping::Ungrouped {
            candidates.push((None, false));
        } else {
            let mode = self.grouping.automatic.state_key();
            for (key, _, location) in self.automatic_group_entries(process) {
                let exact = location.is_some_and(|location| location.pane_id == active_pane);
                candidates.push((Some(format!("auto:{mode}:{key}")), exact));
            }
        }
        for group in &self.grouping.manual {
            if group
                .processes
                .iter()
                .any(|name| name == &process.process.name)
            {
                candidates.push((Some(format!("manual:{}", group.id)), false));
            }
        }
        if candidates.iter().any(|(_, exact)| *exact) {
            candidates.retain(|(_, exact)| *exact);
        }
        let folded = |identity: &Option<String>| {
            identity
                .as_ref()
                .is_some_and(|identity| self.grouping.collapsed.contains(identity))
        };
        if candidates.iter().any(|(identity, _)| !folded(identity)) {
            return Vec::new();
        }
        candidates
            .into_iter()
            .filter_map(|(identity, _)| identity)
            .collect()
    }

    /// Whether a process is attached in the most recently focused outer pane.
    /// It, and its copies under other windows, take the active border color.
    pub(in crate::tui) fn process_is_focused(&self, process_index: usize) -> bool {
        self.focused_outer_pane.as_deref().is_some_and(|pane| {
            self.processes.get(process_index).is_some_and(|process| {
                process
                    .attachments()
                    .iter()
                    .any(|location| location.pane_id == pane)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    #[test]
    fn focus_queries_follow_the_visibility_backoff_and_the_focus_hook() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        let now = Instant::now();
        assert!(app.focus_refresh_due(now), "startup queries at once");
        app.mark_focus_requested(now);
        assert!(!app.focus_refresh_due(now + FOCUS_POLL_ACTIVE / 2));
        assert!(app.focus_refresh_due(now + FOCUS_POLL_ACTIVE));

        // A hidden window backs off.
        app.apply_focus(
            OuterFocus {
                window_active: false,
                active_pane: None,
            },
            20,
        );
        assert!(!app.window_active);
        assert!(!app.focus_refresh_due(now + FOCUS_POLL_ACTIVE));
        assert!(app.focus_refresh_due(now + FOCUS_POLL_INACTIVE));
        // ...unless the focus hook asks for a refresh.
        app.force_focus_refresh = true;
        assert!(app.focus_refresh_due(now));

        app.apply_focus(
            OuterFocus {
                window_active: true,
                active_pane: Some("%sidebar".to_string()),
            },
            20,
        );
        assert!(app.window_active && app.sidebar_focused);
        assert_eq!(app.focus_poll_interval, FOCUS_POLL_ACTIVE);
        // With the focus hook firing, polling is only a safety net.
        app.focus_hook_fires = true;
        app.apply_focus(
            OuterFocus {
                window_active: true,
                active_pane: Some("%sidebar".to_string()),
            },
            20,
        );
        assert_eq!(app.focus_poll_interval, FOCUS_POLL_HOOKED);

        // A sidebar that does not know its own pane never queries.
        assert!(!SidebarApp::new(None).focus_refresh_due(now));
    }

    fn two_window_process() -> crate::SidebarProcess {
        let location = |window: &str, pane: &str| crate::OuterPaneLocation {
            pane_id: pane.to_string(),
            pane_index: "1".to_string(),
            window_id: format!("@{window}"),
            window_index: window.to_string(),
            window_name: format!("w{window}"),
        };
        let mut process = test_process(Some("/work"), None);
        process.process.attached_clients = 2;
        process.outer_locations = vec![location("1", "%10"), location("9", "%90")];
        process.outer_location = Some(process.outer_locations[0].clone());
        process
    }

    #[test]
    fn landing_unfolds_only_the_window_group_of_the_landed_attachment() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![two_window_process()];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Window,
            collapsed: ["auto:window:@1", "auto:window:@9"]
                .map(str::to_string)
                .into(),
            ..Default::default()
        };
        assert_eq!(app.groups_to_reveal(0, "%90"), ["auto:window:@9"]);
        assert_eq!(app.groups_to_reveal(0, "%10"), ["auto:window:@1"]);
        // Already open: nothing to do.
        app.grouping.collapsed.remove("auto:window:@9");
        assert!(app.groups_to_reveal(0, "%90").is_empty());
    }

    #[test]
    fn landing_with_no_better_candidate_unfolds_every_group_listing_it() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![two_window_process()];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Cwd,
            manual: vec![crate::ManualGroup {
                id: "saved".to_string(),
                name: "Saved".to_string(),
                processes: vec!["demo".to_string()],
            }],
            collapsed: ["auto:cwd:/work", "manual:saved"]
                .map(str::to_string)
                .into(),
            ..Default::default()
        };
        let mut reveal = app.groups_to_reveal(0, "%90");
        reveal.sort();
        assert_eq!(reveal, ["auto:cwd:/work", "manual:saved"]);
        // One of the tied groups is open, so the cursor can land there.
        app.grouping.collapsed.remove("manual:saved");
        assert!(app.groups_to_reveal(0, "%90").is_empty());
        // The ungrouped list cannot fold, so a manual group never needs to open.
        app.grouping.automatic = crate::AutomaticGrouping::Ungrouped;
        app.grouping.collapsed.insert("manual:saved".to_string());
        assert!(app.groups_to_reveal(0, "%90").is_empty());
    }

    #[test]
    fn following_a_landing_unfolds_its_group_and_selects_that_copy() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        let mut app = SidebarApp::new(None);
        app.processes = vec![two_window_process()];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Window,
            collapsed: ["auto:window:@1", "auto:window:@9"]
                .map(str::to_string)
                .into(),
            ..Default::default()
        };
        app.rebuild_rows();
        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row, SidebarListRow::Process { .. }))
        );

        app.active_outer_pane = Some("%90".to_string());
        assert!(app.follow_active_outer_process());
        assert!(!app.grouping.collapsed.contains("auto:window:@9"));
        assert!(app.grouping.collapsed.contains("auto:window:@1"));
        assert!(matches!(
            &app.rows[app.selected],
            SidebarListRow::Process { location: Some(location), .. } if location.pane_id == "%90"
        ));
        // The unfold is persisted, since grouping state reloads from disk.
        let saved = crate::read_sidebar_grouping().unwrap();
        assert!(!saved.collapsed.contains("auto:window:@9"));

        // Folding it again by hand sticks until the next landing.
        app.grouping.collapsed.insert("auto:window:@9".to_string());
        app.rebuild_rows();
        assert!(!app.follow_active_outer_process());
        assert!(app.grouping.collapsed.contains("auto:window:@9"));
    }

    #[test]
    fn following_a_process_scrolled_off_the_top_reveals_it() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        app.grouping = crate::SidebarGroupingState::default();
        for index in 0..8 {
            let mut process = test_process(Some("/work"), None);
            process.process.name = format!("process-{index}");
            // Newest first, so process-1 sorts second from the top.
            process.process.created_at = 100 - index;
            process.outer_location = (index == 1).then(|| crate::OuterPaneLocation {
                pane_id: "%host".to_string(),
                pane_index: "1".to_string(),
                window_id: "@1".to_string(),
                window_index: "1".to_string(),
                window_name: "work".to_string(),
            });
            app.processes.push(process);
        }
        app.rebuild_rows();
        // Scrolled to the bottom, the process-1 row is above the viewport.
        let last = app.rows.len() - 1;
        app.selected = last;
        app.ensure_selected_item_visible(12);
        assert!(app.viewport_start_line() > app.row_start_line(1));

        // The outer pane switches to process-1.
        app.active_outer_pane = Some("%host".to_string());
        assert!(app.follow_active_outer_process());
        assert_eq!(app.selected, 1);
        app.ensure_followed_process_visible(12);

        let start = app.row_start_line(app.selected);
        let end = start + app.row_line_count(app.selected);
        let viewport_start = app.viewport_start_line();
        assert!(
            start >= viewport_start && end <= viewport_start + list_content_height(12),
            "followed row {start}..{end} must be fully inside the viewport from {viewport_start}"
        );
    }

    #[test]
    fn inactive_sidebar_tracks_only_the_active_outer_process() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        let mut process = test_process(Some("/work"), Some("/work"));
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        app.processes.push(process);
        app.active_outer_pane = Some("%2".to_string());
        app.follow_active_outer_process();

        assert_eq!(app.selected, 0);
        assert!(app.cursor_is_visible_for(0));

        app.active_outer_pane = None;
        assert!(!app.cursor_is_visible_for(0));

        app.sidebar_focused = true;
        assert!(app.cursor_is_visible_for(0));
    }

    #[test]
    fn inactive_sidebar_scroll_does_not_pull_the_active_process_back_into_view() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        for index in 0..8 {
            let mut process = test_process(Some("/work"), Some("/work"));
            process.process.name = format!("process-{index}");
            app.processes.push(process);
        }
        app.selected = 5;
        app.first_visible = 5;

        app.scroll_viewport(-3);

        assert_eq!(app.selected, 5);
        assert_eq!(app.first_visible, 2);
        assert!(app.selected >= visible_end(&app.processes, app.first_visible, 6));

        app.sidebar_focused = true;
        app.ensure_visible(6);
        assert!(app.selected < visible_end(&app.processes, app.first_visible, 6));
    }

    #[test]
    fn active_process_change_scrolls_its_sidebar_row_into_view_once() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        app.active_outer_pane = Some("%2".to_string());
        for index in 0..8 {
            let mut process = test_process(Some("/work"), Some("/work"));
            process.process.name = format!("process-{index}");
            if index == 7 {
                process.outer_location = Some(crate::OuterPaneLocation {
                    pane_id: "%2".to_string(),
                    pane_index: "1".to_string(),
                    window_id: "@1".to_string(),
                    window_index: "1".to_string(),
                    window_name: "work".to_string(),
                });
            }
            app.processes.push(process);
        }

        assert!(app.follow_active_outer_process());
        app.ensure_visible(6);
        assert_eq!(app.selected, 7);
        assert!(app.first_visible > 0);
        assert!(app.selected < app.visible_end(app.first_visible, 6));

        // Polling the same active process does not override a deliberate scroll.
        app.scroll_viewport(-8);
        assert!(!app.follow_active_outer_process());
        assert!(app.selected >= app.visible_end(app.first_visible, 6));

        // Leaving and returning to the outer pane counts as a fresh follow.
        app.active_outer_pane = None;
        assert!(!app.follow_active_outer_process());
        app.active_outer_pane = Some("%2".to_string());
        assert!(app.follow_active_outer_process());
        app.ensure_visible(6);
        assert!(app.selected < app.visible_end(app.first_visible, 6));

        // Switching the same outer pane to another pmux process reveals that row.
        app.processes[7].outer_location = None;
        app.processes[0].outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "1".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        assert!(app.follow_active_outer_process());
        app.ensure_visible(6);
        assert_eq!(app.selected, 0);
        assert_eq!(app.first_visible, 0);
    }
}
