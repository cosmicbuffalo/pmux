//! Sidebar state: processes, grouping, drawers, selection, flashes, and live reload.
//!
//! This file holds `SidebarApp` and its core input state. Its other methods
//! are split by concern: `rows` (grouping and row building), `viewport`
//! (cursor and scrolling), `drawers`, `focus` (outer focus tracking), and
//! `flash`; `reload` is the binary watcher and hot-reload state. Items those
//! submodules share with the rest of `tui` are `pub(in crate::tui)` and
//! re-exported here.

use super::*;

mod drawers;
mod family;
mod flash;
mod focus;
mod reload;
mod rows;
mod search;
mod viewport;

pub(super) use drawers::DrawerKey;
use family::attention_tier;
pub(super) use flash::{FlashSource, PaneFlashTracker};
pub(super) use focus::{OuterFocus, install_focus_refresh_handler, take_focus_refresh_request};
pub(super) use reload::{SidebarBinaryWatcher, SidebarLoopExit, SidebarReloadState};
use rows::{Course, pmux_process_chip_background};
pub(super) use rows::{
    SidebarGroupKind, SidebarGroupRow, SidebarListRow, group_header_background, group_header_line,
    resumable_target,
};
pub(super) use search::SidebarSearch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PopupTrigger {
    Keyboard,
    Mouse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingMouseMenu {
    pub(super) row_identity: String,
    pressed_at: Instant,
    released_at: Option<Instant>,
}

impl PendingMouseMenu {
    pub(super) fn new(row_identity: String, now: Instant) -> Self {
        Self {
            row_identity,
            pressed_at: now,
            released_at: None,
        }
    }

    pub(super) fn mark_released(&mut self, now: Instant) {
        self.released_at = Some(now);
    }

    /// When the menu may open: once the release has landed, and never before
    /// the double-click window closes, so a second click can still turn this
    /// click into a double-click. A tap that reports no release falls back to
    /// opening after [`MOUSE_MENU_TAP_FALLBACK`].
    pub(super) fn ready_at(&self) -> Instant {
        let double_click_closed = self.pressed_at + MOUSE_DOUBLE_CLICK_INTERVAL;
        match self.released_at {
            Some(released) => (released + MOUSE_MENU_RELEASE_DELAY).max(double_click_closed),
            None => (self.pressed_at + MOUSE_MENU_TAP_FALLBACK).max(double_click_closed),
        }
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        now >= self.ready_at()
    }
}

/// A double-click between its second press and tmux's own DoubleClick event;
/// see [`MOUSE_DOUBLE_CLICK_SETTLE`]. Without pmux's outer click bindings the
/// jump/open is held until then. With them it has already acted, and this
/// only swallows the press tmux forwards so it is not taken as a new click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingDoubleClick {
    pub(super) row_identity: String,
    clicked_at: Instant,
    /// The jump/open already ran on the second press.
    pub(super) acted: bool,
}

impl PendingDoubleClick {
    pub(super) fn new(row_identity: String, now: Instant, acted: bool) -> Self {
        Self {
            row_identity,
            clicked_at: now,
            acted,
        }
    }

    /// When a held action runs (or a swallowing entry expires) if tmux never
    /// forwards its DoubleClick event.
    pub(super) fn ready_at(&self) -> Instant {
        self.clicked_at + MOUSE_DOUBLE_CLICK_SETTLE
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        now >= self.ready_at()
    }
}

/// What a left-click press on a process or resumable row amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowClick {
    Single,
    /// The second press on the same row within [`MOUSE_DOUBLE_CLICK_INTERVAL`].
    Double,
    /// A further press belonging to a double-click already handled, such as
    /// the copy tmux forwards for its own double-click event.
    Repeat,
}

/// The most recent left-click press on a row, for double-click detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LastRowClick {
    row_identity: String,
    at: Instant,
    /// Already counted as the second press of a double-click.
    doubled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrimaryAction {
    Jump,
    AttachInNewSplit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SidebarResizeDrag {
    pane_left: u16,
    fixed_right: u16,
    pub(super) width: u16,
    last_pointer_x: u16,
}

impl SidebarResizeDrag {
    pub(super) fn new(pane_left: u16, width: u16, pointer_column: u16) -> Self {
        Self {
            pane_left,
            fixed_right: pane_left.saturating_add(width),
            width,
            last_pointer_x: pane_left.saturating_add(pointer_column),
        }
    }

    pub(super) fn desired_width(&self, pointer_column: u16, sidebar_on_right: bool) -> (u16, u16) {
        let current_left = if sidebar_on_right {
            self.fixed_right.saturating_sub(self.width)
        } else {
            self.pane_left
        };
        let pointer_x = current_left.saturating_add(pointer_column);
        let pointer_delta = i32::from(pointer_x) - i32::from(self.last_pointer_x);
        let width_delta = if sidebar_on_right {
            -pointer_delta
        } else {
            pointer_delta
        };
        let width = (i32::from(self.width) + width_delta).clamp(1, i32::from(u16::MAX)) as u16;
        (width, pointer_x)
    }

    pub(super) fn commit(&mut self, width: u16, pointer_x: u16) {
        self.width = width;
        self.last_pointer_x = pointer_x;
    }
}

/// What the header's second line briefly names after it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum HeaderNotice {
    /// The automatic grouping mode just switched to.
    Grouping,
    /// The drawer graph's metric just switched to.
    Graph,
}

#[derive(Debug)]
pub(super) struct SidebarApp {
    pub(super) processes: Vec<crate::SidebarProcess>,
    pub(super) rows: Vec<SidebarListRow>,
    pub(super) grouping: crate::SidebarGroupingState,
    pub(super) selected: usize,
    pub(super) first_visible: usize,
    /// Lines scrolled through the first visible process/drawer/separator block.
    pub(super) first_visible_line: usize,
    pub(super) message: Option<String>,
    /// Whether the `pmux events` feed is down and being respawned,
    /// shown as a quiet footer status while no message is.
    pub(super) event_stream_down: bool,
    pub(super) help_open: bool,
    /// Lines the keymap help is scrolled by. Drawing clamps it to the
    /// content, so input can move it freely.
    pub(super) help_scroll: usize,
    /// Rows of the keymap help visible at the last draw, the page size for
    /// scrolling it.
    pub(super) help_page: u16,
    pub(super) last_location_refresh: Instant,
    /// The newest state snapshot from the event stream, re-enriched by each
    /// periodic location refresh.
    pub(super) last_snapshot: Option<Arc<crate::StateSnapshot>>,
    pub(super) sidebar_pane: Option<String>,
    pub(super) sidebar_focused: bool,
    active_outer_pane: Option<String>,
    /// The non-sidebar pane most recently focused in this window. Unlike
    /// `active_outer_pane` it survives focusing the sidebar itself, so the
    /// process you came from stays highlighted while you browse the list.
    focused_outer_pane: Option<String>,
    /// Process (and pane) most recently followed from the active outer pane.
    /// This lets a process switch reveal its row once without undoing later
    /// manual scrolls.
    followed_active_process: Option<String>,
    pub(super) active_pane_border_color: Color,
    pub(super) active_pane_background_color: Color,
    pub(super) inactive_pane_background_color: Color,
    pub(super) group_header_background_color: Color,
    last_focus_refresh: Instant,
    focus_poll_interval: Duration,
    /// Whether the outer tmux fires `pane-focus-in`, whose hook signals focus
    /// changes, so polling can relax to [`FOCUS_POLL_HOOKED`].
    pub(super) focus_hook_fires: bool,
    pub(super) force_focus_refresh: bool,
    pub(super) window_active: bool,
    pub(super) toggle_hint: String,
    pub(super) resize_drag: Option<SidebarResizeDrag>,
    /// When the pointer was last seen over the resize handle glyph. The handle
    /// renders with a hover highlight while this is recent (see
    /// [`HANDLE_HOVER_LINGER`]); `None` once it has expired or the pointer moved
    /// off the handle.
    pub(super) handle_hover_seen: Option<Instant>,
    /// Set while a config from outside pmux has changed pmux's tmux server
    /// (its `@pmux_foreign_config` flag), read by a background thread. The
    /// header then says to run `pmux restart --repair`. The sidebar lives in the user's
    /// tmux, so a foreign config cannot hide this the way it can replace
    /// pmux's own status line.
    pub(super) pmux_config_changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Process names whose details drawers are expanded. Rows remain open while
    /// the cursor moves; Enter toggles only the selected row.
    pub(super) open_drawers: HashSet<String>,
    /// Past sessions in the Resumable group whose drawers are open, by
    /// [`resumable_target`]. Their activity comes from the saved transcript.
    pub(super) open_past_drawers: HashSet<String>,
    past_drawer_cache: HashMap<String, crate::DrawerState>,
    past_drawer_tx: Sender<(String, crate::DrawerState)>,
    past_drawer_rx: Receiver<(String, crate::DrawerState)>,
    /// Whether keyboard focus is on the graph control in the header instead of
    /// on a process row. Enter on this target toggles every drawer together.
    pub(super) header_selected: bool,
    /// Last rendered list width, used to keep drawer height accounting exactly
    /// aligned with the width-dependent wrapped prompt lines.
    drawer_width: u16,
    /// Which quantity the drawer timeline plots (cycled with `m`).
    pub(super) drawer_metric: crate::DrawerMetric,
    /// Index into [`crate::AXIS_PRESETS`] for the timeline x-axis (cycled with `x`).
    pub(super) drawer_axis: usize,
    /// While present and unexpired, the header graph indicator shows the newly
    /// selected x-axis preset instead of its usual metric label.
    drawer_axis_hint_until: Option<Instant>,
    /// What the header's second line names after a change, and until when.
    header_notice: Option<(HeaderNotice, Instant)>,
    /// Per-process parsed activity. Filled lazily by background fetch threads
    /// and pruned to the live processes on every snapshot.
    drawer_cache: HashMap<DrawerKey, crate::DrawerState>,
    /// Fetch results arrive here from background threads and are drained each tick.
    drawer_tx: Sender<(DrawerKey, crate::DrawerState)>,
    drawer_rx: Receiver<(DrawerKey, crate::DrawerState)>,
    /// When the background pass over every agent's activity last started, and
    /// whether one is still running. See [`Self::ensure_activity_refresh`].
    activity_refreshed_at: Option<Instant>,
    activity_refresh_running: Arc<AtomicBool>,
    /// Whether a pass over the visible past sessions' transcripts is running.
    /// See [`Self::ensure_past_prompt_counts`].
    past_count_running: Arc<AtomicBool>,
    pub(super) keymap: SidebarKeymap,
    /// Keys pressed so far toward a multi-key binding such as `gg`.
    pub(super) pending_keys: PendingKeys,
    /// A second click schedules the native actions menu after its release so
    /// that release cannot immediately dismiss the newly-created tmux menu.
    pub(super) pending_mouse_menu: Option<PendingMouseMenu>,
    /// The latest left-click press on a row, so a second press can be told
    /// apart as a double-click.
    last_row_click: Option<LastRowClick>,
    /// A double-click waiting for tmux's DoubleClick event before it acts.
    pub(super) pending_double_click: Option<PendingDoubleClick>,
    pub(super) pane_flash: PaneFlashTracker,
    /// The row whose pane is flashing and when that flash started, so the
    /// row's own `pane N` / `window N` text can blink in step.
    active_flash: Option<(String, Instant, FlashSource)>,
    /// The agent history catalog, live sessions included (they supply process
    /// titles). Loaded on a background thread; see [`Self::ensure_history_fetch`].
    pub(super) history: Vec<crate::HistoryEntry>,
    /// Agent conversation titles by the pmux process hosting them.
    agent_titles: HashMap<String, String>,
    history_tx: Sender<Result<Vec<crate::HistoryEntry>, String>>,
    history_rx: Receiver<Result<Vec<crate::HistoryEntry>, String>>,
    history_loading: bool,
    history_loaded_at: Option<Instant>,
    /// Live process names when the catalog was last requested. A change means
    /// a session started or exited, moving it into or out of Resumable.
    history_process_names: Vec<String>,
    /// The labels-changed stamp as last seen, and when it was last read. A new
    /// value (see [`crate::state::touch_labels_changed`]) requests a history
    /// reload so new agent titles show without waiting for the periodic one.
    labels_stamp: Option<String>,
    labels_stamp_checked: Option<Instant>,
    history_reload_requested: bool,
    /// What each process (by pane) was last doing and since when, which
    /// the list is ordered by. See [`Self::settle_courses`].
    pub(super) courses: HashMap<String, Course>,
    /// The `/` query filtering the list. See [`search`].
    pub(super) search: SidebarSearch,
}

impl SidebarApp {
    pub(super) fn new(sidebar_pane: Option<String>) -> Self {
        let (drawer_tx, drawer_rx) = mpsc::channel();
        let (history_tx, history_rx) = mpsc::channel();
        let (past_drawer_tx, past_drawer_rx) = mpsc::channel();
        Self {
            processes: Vec::new(),
            rows: Vec::new(),
            grouping: crate::read_sidebar_grouping().unwrap_or_default(),
            selected: 0,
            first_visible: 0,
            first_visible_line: 0,
            message: None,
            event_stream_down: false,
            help_open: false,
            help_scroll: 0,
            help_page: 0,
            last_location_refresh: Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now),
            last_snapshot: None,
            sidebar_pane,
            sidebar_focused: false,
            active_outer_pane: None,
            focused_outer_pane: None,
            followed_active_process: None,
            active_pane_border_color: ACTIVE_PANE_BORDER_FALLBACK_COLOR,
            active_pane_background_color: ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
            inactive_pane_background_color: INACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
            group_header_background_color: pmux_process_chip_background(),
            last_focus_refresh: Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now),
            // Assume visible until the first poll proves otherwise, so startup
            // establishes focus promptly rather than after a backoff.
            focus_poll_interval: FOCUS_POLL_ACTIVE,
            focus_hook_fires: false,
            force_focus_refresh: false,
            // Assume visible until the first focus poll proves otherwise.
            window_active: true,
            // Populated with the live outer prefix once the loop starts; this
            // fallback is only shown if the prefix cannot be read.
            toggle_hint: sidebar_toggle_hint_text(None),
            resize_drag: None,
            handle_hover_seen: None,
            pmux_config_changed: Default::default(),
            open_drawers: HashSet::new(),
            open_past_drawers: HashSet::new(),
            past_drawer_cache: HashMap::new(),
            past_drawer_tx,
            past_drawer_rx,
            header_selected: false,
            drawer_width: crate::DEFAULT_SIDEBAR_WIDTH,
            drawer_metric: crate::DrawerMetric::OutputTokens,
            drawer_axis: crate::DEFAULT_AXIS_INDEX,
            drawer_axis_hint_until: None,
            header_notice: None,
            drawer_cache: HashMap::new(),
            drawer_tx,
            drawer_rx,
            activity_refreshed_at: None,
            activity_refresh_running: Arc::new(AtomicBool::new(false)),
            past_count_running: Arc::new(AtomicBool::new(false)),
            keymap: SidebarKeymap::default(),
            pending_keys: PendingKeys::default(),
            pending_mouse_menu: None,
            last_row_click: None,
            pending_double_click: None,
            pane_flash: PaneFlashTracker::default(),
            active_flash: None,
            history: Vec::new(),
            agent_titles: HashMap::new(),
            history_tx,
            history_rx,
            history_loading: false,
            history_loaded_at: None,
            history_process_names: Vec::new(),
            labels_stamp: None,
            labels_stamp_checked: None,
            history_reload_requested: false,
            courses: HashMap::new(),
            search: SidebarSearch::default(),
        }
    }

    pub(super) fn selected_row_identity(&self) -> Option<&str> {
        if self.rows.is_empty() {
            return self.selected_process_name();
        }
        self.rows
            .get(self.selected)
            .and_then(SidebarListRow::identity)
    }

    pub(super) fn selected_process_index(&self) -> Option<usize> {
        self.process_index_for_row(self.selected)
    }

    fn process_index_for_row(&self, row: usize) -> Option<usize> {
        if self.rows.is_empty() {
            return (row < self.processes.len()).then_some(row);
        }
        match self.rows.get(row) {
            Some(SidebarListRow::Process { process_index, .. }) => Some(*process_index),
            _ => None,
        }
    }

    pub(super) fn selected_process_name(&self) -> Option<&str> {
        self.selected_process_index()
            .and_then(|index| self.processes.get(index))
            .map(|entry| entry.process.name.as_str())
    }

    /// Take the pending double-click if it belongs to `row_identity`. A press
    /// on that row while it is pending is tmux forwarding its DoubleClick
    /// event (after its binding, if pmux's override is not loaded, re-selected
    /// the sidebar pane).
    pub(super) fn take_pending_double_click(
        &mut self,
        row_identity: &str,
    ) -> Option<PendingDoubleClick> {
        if self
            .pending_double_click
            .as_ref()
            .is_some_and(|pending| pending.row_identity == row_identity)
        {
            return self.pending_double_click.take();
        }
        None
    }

    /// Classify a left-click press on the row `row_identity`. A second press
    /// on the same row inside [`MOUSE_DOUBLE_CLICK_INTERVAL`] is a
    /// double-click; presses after that within the window repeat it (tmux can
    /// forward a double-click's second press twice) and are ignored.
    pub(super) fn register_row_click(&mut self, row_identity: &str, now: Instant) -> RowClick {
        let kind = match &self.last_row_click {
            Some(last)
                if last.row_identity == row_identity
                    && now.saturating_duration_since(last.at) <= MOUSE_DOUBLE_CLICK_INTERVAL =>
            {
                if last.doubled {
                    RowClick::Repeat
                } else {
                    RowClick::Double
                }
            }
            _ => RowClick::Single,
        };
        if kind != RowClick::Repeat {
            self.last_row_click = Some(LastRowClick {
                row_identity: row_identity.to_string(),
                at: now,
                doubled: kind == RowClick::Double,
            });
        }
        kind
    }

    #[cfg(test)]
    fn selected_name(&self) -> Option<&str> {
        (!self.header_selected)
            .then(|| self.selected_process_name())
            .flatten()
    }

    /// Show a freshly enriched process list, pruning per-process state for
    /// processes that are gone.
    pub(super) fn apply_processes(
        &mut self,
        processes: Vec<crate::SidebarProcess>,
        list_height: u16,
    ) {
        self.processes = processes;
        self.settle_courses();
        if let Ok(grouping) = crate::read_sidebar_grouping() {
            self.grouping = grouping;
        }
        let live_names: HashSet<&str> = self
            .processes
            .iter()
            .map(|process| process.process.name.as_str())
            .collect();
        self.open_drawers
            .retain(|name| live_names.contains(name.as_str()));
        let live_keys: HashSet<DrawerKey> = self.processes.iter().map(DrawerKey::of).collect();
        self.drawer_cache.retain(|key, _| live_keys.contains(key));
        // Best-effort: the order is re-derived on every rebuild anyway.
        if let Some(change) = self.rebuild_rows_tracking_automatic_order() {
            let _ = crate::save_grouping_change(&change);
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.first_visible = self.first_visible.min(self.rows.len().saturating_sub(1));
        self.normalize_viewport_line();
        let active_process_changed = self.follow_active_outer_process();
        if active_process_changed {
            // Following an outer-pane jump is equivalent to an
            // explicit selection change: a visible header alone is
            // not enough when the selected process has an expanded
            // drawer. Keep its complete item, including the drawer's
            // bottom rule, inside the viewport.
            self.ensure_followed_process_visible(list_height);
        } else if self.sidebar_focused {
            self.ensure_visible(list_height);
        }
        self.message = None;
        self.last_location_refresh = Instant::now();
    }

    /// Advance time-driven input state to `now`: expire the resize-handle
    /// hover, and fire a scheduled mouse menu or a held double-click once due
    /// (each only if its row is still the selected one). Returns the effects
    /// to perform, plus [`Effect::Redraw`] when only the display changed.
    pub(super) fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        // tmux never says when the pointer leaves the pane; treat a hover
        // with no motion for the linger window as gone.
        if self
            .handle_hover_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) > HANDLE_HOVER_LINGER)
        {
            self.handle_hover_seen = None;
            effects.push(Effect::Redraw);
        }
        if self.header_notice.is_some() && self.header_notice(now).is_none() {
            self.header_notice = None;
            effects.push(Effect::Redraw);
        }
        if self
            .pending_mouse_menu
            .as_ref()
            .is_some_and(|pending| pending.ready(now))
            && let Some(pending) = self.pending_mouse_menu.take()
            && self.selected_row_identity() == Some(pending.row_identity.as_str())
        {
            effects.push(Effect::OpenActions(PopupTrigger::Mouse));
        }
        // A held double-click whose forwarded tmux event never arrived acts
        // once tmux's own DoubleClick handling has had time to finish; an
        // entry that already acted just expires.
        if self
            .pending_double_click
            .as_ref()
            .is_some_and(|pending| pending.ready(now))
            && let Some(pending) = self.pending_double_click.take()
            && !pending.acted
            && self.selected_row_identity() == Some(pending.row_identity.as_str())
        {
            effects.push(Effect::RunDoubleClickAction);
        }
        effects
    }

    /// Start a drag-resize from the measured sidebar geometry, or report why
    /// it could not be measured.
    pub(super) fn begin_resize(&mut self, result: Result<SidebarResizeDrag>) {
        match result {
            Ok(drag) => self.resize_drag = Some(drag),
            Err(err) => self.message = Some(format!("Could not start sidebar resize: {err}")),
        }
    }

    /// The `(width, pointer_x)` the active drag wants for the pointer at
    /// `column`, when that needs the pane resized. A pointer move that keeps
    /// the width is recorded here and needs nothing more.
    pub(super) fn resize_drag_request(
        &mut self,
        column: u16,
        sidebar_on_right: bool,
    ) -> Option<(u16, u16)> {
        let drag = self.resize_drag.as_mut()?;
        let (width, pointer_x) = drag.desired_width(column, sidebar_on_right);
        if width == drag.width {
            drag.commit(width, pointer_x);
            return None;
        }
        Some((width, pointer_x))
    }

    /// Record the outcome of resizing the pane for [`Self::resize_drag_request`]:
    /// the width tmux actually applied, or why it failed.
    pub(super) fn finish_resize_step(&mut self, result: Result<u16>, pointer_x: u16) {
        match result {
            Ok(width) => {
                if let Some(drag) = self.resize_drag.as_mut() {
                    drag.commit(width, pointer_x);
                }
            }
            Err(err) => self.message = Some(format!("Could not resize sidebar: {err}")),
        }
    }

    /// The outer location a row stands for: its own attachment under window
    /// grouping, otherwise its process's primary location.
    pub(super) fn row_location(&self, row: usize) -> Option<&crate::OuterPaneLocation> {
        if self.rows.is_empty() {
            return self
                .processes
                .get(row)
                .and_then(|process| process.outer_location.as_ref());
        }
        match self.rows.get(row)? {
            SidebarListRow::Process {
                location: Some(location),
                ..
            } => Some(location),
            SidebarListRow::Process { process_index, .. } => self
                .processes
                .get(*process_index)
                .and_then(|process| process.outer_location.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    use ratatui::{
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn physical_click_menu_waits_for_release_but_taps_have_a_fallback() {
        let pressed = Instant::now();
        let mut physical = PendingMouseMenu::new("demo".to_string(), pressed);
        assert!(!physical.ready(pressed + MOUSE_MENU_RELEASE_DELAY));
        let released = pressed + Duration::from_millis(100);
        physical.mark_released(released);
        assert!(!physical.ready(released));
        assert!(physical.ready(pressed + MOUSE_DOUBLE_CLICK_INTERVAL));

        let tap = PendingMouseMenu::new("demo".to_string(), pressed);
        assert!(!tap.ready(pressed + MOUSE_MENU_TAP_FALLBACK - Duration::from_millis(1)));
        assert!(tap.ready(pressed + MOUSE_MENU_TAP_FALLBACK));
    }

    #[test]
    fn click_menu_waits_out_the_double_click_window() {
        // A quick click releases long before a second click could arrive; the
        // menu still waits so that second click can make it a double-click.
        let pressed = Instant::now();
        let mut quick = PendingMouseMenu::new("demo".to_string(), pressed);
        quick.mark_released(pressed + Duration::from_millis(60));
        assert!(!quick.ready(pressed + Duration::from_millis(200)));
        assert_eq!(quick.ready_at(), pressed + MOUSE_DOUBLE_CLICK_INTERVAL);
        assert!(quick.ready(pressed + MOUSE_DOUBLE_CLICK_INTERVAL));
    }

    #[test]
    fn double_click_waits_for_tmux_then_acts_once() {
        let mut app = SidebarApp::new(None);
        let clicked = Instant::now();
        let pending = PendingDoubleClick::new("row-a".to_string(), clicked, false);
        // Without a forwarded tmux event, it acts after tmux's 300ms
        // triple-click timer and DoubleClick binding have run.
        assert!(!pending.ready(clicked + Duration::from_millis(300)));
        assert!(pending.ready(clicked + MOUSE_DOUBLE_CLICK_SETTLE));

        app.pending_double_click = Some(pending);
        // A press on another row is not tmux's forwarded event.
        assert!(app.take_pending_double_click("row-b").is_none());
        assert!(app.pending_double_click.is_some());
        // The forwarded press on the same row consumes it exactly once.
        assert!(
            app.take_pending_double_click("row-a")
                .is_some_and(|pending| !pending.acted)
        );
        assert!(app.take_pending_double_click("row-a").is_none());
        assert!(app.pending_double_click.is_none());

        // With pmux's outer click override, the action ran on the second press;
        // the entry only swallows tmux's forwarded press.
        app.pending_double_click =
            Some(PendingDoubleClick::new("row-a".to_string(), clicked, true));
        assert!(
            app.take_pending_double_click("row-a")
                .is_some_and(|pending| pending.acted)
        );
    }

    #[test]
    fn row_clicks_pair_into_double_clicks_and_ignore_repeats() {
        let mut app = SidebarApp::new(None);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert_eq!(app.register_row_click("a", at(0)), RowClick::Single);
        assert_eq!(app.register_row_click("a", at(250)), RowClick::Double);
        // tmux may forward the second press again for its own double-click.
        assert_eq!(app.register_row_click("a", at(260)), RowClick::Repeat);
        // Once the window has passed, the row takes ordinary clicks again.
        assert_eq!(app.register_row_click("a", at(1000)), RowClick::Single);
        // Two clicks on different rows are not a double-click.
        assert_eq!(app.register_row_click("b", at(1100)), RowClick::Single);
        // Nor are two slow clicks on the same row.
        assert_eq!(
            app.register_row_click(
                "b",
                at(1100) + MOUSE_DOUBLE_CLICK_INTERVAL + Duration::from_millis(1)
            ),
            RowClick::Single
        );
    }

    #[test]
    fn double_click_jumps_to_attached_processes_or_opens_a_new_window() {
        let mut process = test_process(Some("/work"), Some("/work"));
        assert_eq!(
            double_click_action_for_process(&process),
            DoubleClickAction::AttachInNewWindow
        );
        // Attached only in another outer session still opens a window here.
        process.process.attached_clients = 1;
        assert_eq!(
            double_click_action_for_process(&process),
            DoubleClickAction::AttachInNewWindow
        );
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@9".to_string(),
            window_index: "9".to_string(),
            window_name: "elsewhere".to_string(),
        });
        assert_eq!(
            double_click_action_for_process(&process),
            DoubleClickAction::Jump
        );
    }

    #[test]
    fn selected_rows_use_a_highlight_instead_of_italics() {
        let style = content_style(
            Style::default().fg(Color::White),
            Some(super::CURSOR_HIGHLIGHT_COLOR),
        );
        assert_eq!(style.bg, Some(super::CURSOR_HIGHLIGHT_COLOR));
        assert!(!style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn unfocused_active_row_uses_a_dim_gray_highlight() {
        let style = content_style(
            Style::default().fg(Color::White),
            Some(super::UNFOCUSED_CURSOR_HIGHLIGHT_COLOR),
        );
        assert_eq!(style.bg, Some(super::UNFOCUSED_CURSOR_HIGHLIGHT_COLOR));
    }

    #[test]
    fn selected_row_highlight_does_not_replace_session_name_pills() {
        let style = session_name_pill_style(Color::Green, Color::Indexed(234));
        assert_eq!(style.fg, Some(Color::Indexed(234)));
        assert_eq!(style.bg, Some(Color::Green));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn session_name_pill_colors_are_stable_from_the_process_name() {
        // Deterministic: a given name always maps to the same palette color...
        assert_eq!(
            session_name_pill_color("alpha"),
            session_name_pill_color("alpha")
        );
        // ...and it is always one of the curated palette entries.
        assert!(
            crate::render::SESSION_NAME_PILL_COLORS.contains(&session_name_pill_color("alpha"))
        );
        assert!(crate::render::SESSION_NAME_PILL_COLORS.contains(&session_name_pill_color("beta")));
    }

    #[test]
    fn agent_column_reserves_the_widest_visible_agent_label() {
        let mut codex = test_process(None, None);
        codex.process.agent = Some(crate::AgentInfo {
            kind: "codex".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        let mut claude = codex.clone();
        claude.process.agent.as_mut().unwrap().kind = "claude".to_string();

        assert_eq!(agent_tag_column_width(&[codex, claude]), 6);
    }

    #[test]
    fn graph_button_padding_is_clickable_and_toggles_all_drawers() {
        let mut app = SidebarApp::new(None);
        app.processes = (0..2)
            .map(|index| {
                let mut process = test_process(Some("/work"), None);
                process.process.name = format!("process-{index}");
                process
            })
            .collect();
        let area = Rect::new(0, 0, 42, 30);
        let now = Instant::now();
        let idle_button = sidebar_graph_button_rect(area, false, &app, now);
        assert_eq!(idle_button.y, 2);
        assert_eq!(idle_button.right(), area.right());
        assert_eq!(idle_button.width, 5); // padding + three braille cells + padding
        assert!(!handle_sidebar_graph_click(
            &mut app,
            area,
            false,
            idle_button.x.saturating_sub(1),
            idle_button.y,
            now,
        ));
        assert!(handle_sidebar_graph_click(
            &mut app,
            area,
            false,
            idle_button.x,
            idle_button.y,
            now,
        ));
        assert!(app.header_selected);
        assert!(app.all_drawers_open());

        let active_button = sidebar_graph_button_rect(area, false, &app, now);
        assert_eq!(active_button.right(), area.right());
        // The trailing padding cell at the pane edge is part of the button.
        assert!(active_button.contains((area.right() - 1, active_button.y).into()));
        assert!(handle_sidebar_graph_click(
            &mut app,
            area,
            false,
            area.right() - 1,
            active_button.y,
            now,
        ));
        assert!(!app.any_drawers_open());
    }

    #[test]
    fn sidebar_resize_drag_tracks_both_sidebar_edges() {
        let left = SidebarResizeDrag::new(0, 42, 39);
        assert_eq!(left.desired_width(44, false), (47, 44));

        let mut right = SidebarResizeDrag::new(78, 42, 0);
        let (width, pointer_x) = right.desired_width(5, true);
        assert_eq!((width, pointer_x), (37, 83));
        right.commit(width, pointer_x);
        // The right edge remains fixed as the pane shrinks, so a further
        // one-column pointer move is measured in the pane's new coordinates.
        assert_eq!(right.desired_width(1, true), (36, 84));
    }

    #[test]
    fn missing_agent_signal_is_displayed_as_idle_without_a_fallback_age() {
        let mut process = test_process(Some("/work"), Some("/work"));
        process.process.agent = Some(crate::AgentInfo {
            kind: "codex".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });

        let (_, _, status, _, status_at) = process_badges(&process);

        assert_eq!(status, "idle");
        assert_eq!(status_at, None);
    }

    #[test]
    fn primary_action_jumps_attached_processes_or_opens_a_new_split() {
        let mut process = test_process(Some("/work"), Some("/work"));
        assert_eq!(
            primary_action_for_process(&process),
            PrimaryAction::AttachInNewSplit
        );
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        assert_eq!(primary_action_for_process(&process), PrimaryAction::Jump);
    }

    #[test]
    fn group_headers_are_mouse_targets_but_their_spacing_is_not() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/work"), None)];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Cwd,
            manual: vec![crate::ManualGroup {
                id: "saved".to_string(),
                name: "Saved".to_string(),
                processes: vec!["demo".to_string()],
            }],
            ..Default::default()
        };
        app.rebuild_rows();
        let area = Rect::new(0, 4, 42, 20);
        let screen_row = |index: usize| {
            area.y.saturating_add(1).saturating_add(
                (0..index)
                    .map(|row| app.row_block_line_count(row) as u16)
                    .sum::<u16>(),
            )
        };
        let group_indices: Vec<_> = app
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, SidebarListRow::Group(_)).then_some(index))
            .collect();

        assert_eq!(group_indices.len(), 2);
        for index in group_indices {
            let row = screen_row(index);
            assert_eq!(
                sidebar_mouse_target(&app, area, row),
                Some(SidebarMouseTarget::Group(index))
            );
            assert_eq!(
                sidebar_mouse_target(&app, area, row + 1),
                None,
                "the blank line beneath an expanded group is not clickable"
            );
        }
    }

    #[test]
    fn launch_directory_is_shown_only_after_a_process_changes_cwd() {
        let same = test_process(Some("/work"), Some("/work"));
        assert_eq!(launch_cwd_text(&same), None);

        let moved = test_process(Some("/work/child"), Some("/work"));
        assert_eq!(launch_cwd_text(&moved), Some("/work"));

        let unknown_current = test_process(None, Some("/work"));
        assert_eq!(launch_cwd_text(&unknown_current), None);
    }

    #[test]
    fn attachment_icons_follow_sidebar_side_and_scope() {
        let mut process = test_process(Some("/work"), Some("/work"));
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        let mut context = test_context();
        assert_eq!(
            attachment_scope(&process, &context),
            AttachmentScope::CurrentWindow
        );
        assert_eq!(
            attachment_icon(attachment_scope(&process, &context), false),
            Some("→")
        );
        assert_eq!(
            crate::sidebar_attachment_detail(&process, &context).as_deref(),
            Some("pane 3")
        );
        context.sidebar_on_right = true;
        assert_eq!(
            attachment_icon(attachment_scope(&process, &context), true),
            Some("←")
        );

        process.outer_location.as_mut().unwrap().window_id = "@2".to_string();
        context.sidebar_on_right = false;
        assert_eq!(
            attachment_scope(&process, &context),
            AttachmentScope::OtherWindow
        );
        assert_eq!(
            attachment_icon(attachment_scope(&process, &context), false),
            Some("↗")
        );
        assert_eq!(
            crate::sidebar_attachment_detail(&process, &context).as_deref(),
            Some("window 1")
        );
        context.sidebar_on_right = true;
        assert_eq!(
            attachment_icon(attachment_scope(&process, &context), true),
            Some("↖")
        );

        process.outer_location = None;
        process.process.attached_clients = 2;
        assert_eq!(
            attachment_scope(&process, &context),
            AttachmentScope::OtherSession
        );
        assert_eq!(
            attachment_icon(attachment_scope(&process, &context), true),
            Some("⧉")
        );
        assert_eq!(
            crate::sidebar_attachment_detail(&process, &context).as_deref(),
            Some("2 attached")
        );
    }

    #[test]
    fn first_line_styles_follow_focus_and_attachment_scope() {
        let active_border_color = Color::Rgb(255, 136, 0);
        // The focused process is orange wherever its rows appear, including a
        // copy listed under another window.
        for scope in [AttachmentScope::CurrentWindow, AttachmentScope::OtherWindow] {
            assert_eq!(
                first_line_color(scope, true, active_border_color),
                active_border_color
            );
        }
        // Other processes in the current window are white, like other-session
        // attachments: brighter than other windows, but not orange.
        for scope in [
            AttachmentScope::CurrentWindow,
            AttachmentScope::OtherSession,
        ] {
            assert_eq!(
                first_line_color(scope, false, active_border_color),
                Color::White
            );
        }
        // Attached in another window shares the dimmer unattached gray.
        for scope in [AttachmentScope::OtherWindow, AttachmentScope::Unattached] {
            assert_eq!(
                first_line_color(scope, false, active_border_color),
                super::ATTACHMENT_COLOR
            );
        }
    }
}
