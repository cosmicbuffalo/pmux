//! Sidebar input handling: key and mouse events become state changes on
//! `SidebarApp` plus a list of [`Effect`]s for the run loop to perform.
//!
//! Nothing here talks to tmux or spawns processes, so keyboard and mouse
//! behavior can be tested directly; `event_loop::run_effects` performs the
//! effects.

use super::*;
use crossterm::event::MouseEvent;

/// A side effect requested by an input event. Everything else an input does
/// is a direct change to [`SidebarApp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Effect {
    /// Ask the outer tmux server to close this sidebar pane.
    CloseSidebar,
    /// Jump to, attach, or resume the selected row (Tab, Enter on a past
    /// session).
    RunPrimaryAction,
    /// Run a double-click action that was held until tmux forwarded its own
    /// DoubleClick event.
    RunDoubleClickAction,
    /// A fresh double-click on `row_identity`. Whether it acts now depends on
    /// the outer click override, which is a tmux query.
    DoubleClick { row_identity: String, at: Instant },
    /// Open the per-process actions menu.
    OpenActions(PopupTrigger),
    /// Advance and persist the automatic grouping mode.
    CycleAutomaticGrouping,
    /// Start a drag-resize from the handle at `column`.
    BeginResize { column: u16 },
    /// Continue the active drag-resize with the pointer at `column`.
    ResizeDrag { column: u16 },
    /// Persist a grouping edit already applied to the in-memory state.
    SaveGrouping(crate::GroupingChange),
    /// Nothing to perform, but the display changed (from [`SidebarApp::tick`]).
    Redraw,
}

/// The terminal geometry and clock an input is interpreted against.
#[derive(Debug, Clone, Copy)]
pub(super) struct InputEnv {
    /// The whole sidebar pane.
    pub(super) area: Rect,
    /// Rows available to the process list.
    pub(super) list_height: u16,
    pub(super) pane_label_visible: bool,
    /// Which outer edge the sidebar is on, which decides the arrow that
    /// opens the context menu and where the resize edge is.
    pub(super) sidebar_on_right: bool,
    pub(super) now: Instant,
}

impl InputEnv {
    pub(super) fn new(
        width: u16,
        height: u16,
        pane_label_visible: bool,
        sidebar_on_right: bool,
        now: Instant,
    ) -> Self {
        Self {
            area: Rect::new(0, 0, width, height),
            list_height: height.saturating_sub(sidebar_header_height(pane_label_visible) + 2),
            pane_label_visible,
            sidebar_on_right,
            now,
        }
    }
}

pub(super) fn handle_key(app: &mut SidebarApp, key: KeyEvent, env: &InputEnv) -> Vec<Effect> {
    let list_height = env.list_height;
    app.pending_mouse_menu = None;
    if app.help_open {
        handle_help_key(app, key);
        return Vec::new();
    }
    if app.search.editing {
        return handle_search_key(app, key, list_height);
    }
    // With a query filtering the list, Esc clears it before it can close
    // the sidebar.
    if key.code == KeyCode::Esc && key.modifiers.is_empty() && app.search.active() {
        app.pending_keys.clear();
        app.clear_search(list_height);
        return Vec::new();
    }
    let Some(action) = app.resolve_key(key, env.sidebar_on_right) else {
        return Vec::new();
    };
    match action {
        // The outer tmux server closes the pane: it keeps running after this
        // pane is killed, so it can restore the dynamic layout.
        SidebarAction::Close => return vec![Effect::CloseSidebar],
        SidebarAction::Help => {
            app.help_open = true;
            app.help_scroll = 0;
        }
        SidebarAction::MoveUp => app.move_cursor(-1, list_height),
        SidebarAction::MoveDown => app.move_cursor(1, list_height),
        SidebarAction::HalfPageUp => app.move_cursor(
            -(app.half_visible_process_step(list_height) as isize),
            list_height,
        ),
        SidebarAction::HalfPageDown => app.move_cursor(
            app.half_visible_process_step(list_height) as isize,
            list_height,
        ),
        SidebarAction::PageUp => app.move_cursor(-(page_step(list_height) as isize), list_height),
        SidebarAction::PageDown => app.move_cursor(page_step(list_height) as isize, list_height),
        SidebarAction::First => app.move_to_first_process(),
        SidebarAction::Last if !app.rows.is_empty() => select_last_row(app, list_height),
        SidebarAction::Last => {}
        SidebarAction::SelectHeader => app.header_selected = true,
        SidebarAction::Toggle => {
            if app.header_selected {
                app.toggle_all_drawers();
            } else {
                return app.toggle_selected_drawer(list_height);
            }
        }
        SidebarAction::FoldSection => return app.toggle_selected_section(list_height),
        SidebarAction::Open => return vec![Effect::RunPrimaryAction],
        // Drawer-only: cycle the timeline metric / x-axis (btop-style).
        SidebarAction::CycleMetric if app.any_drawers_open() => app.cycle_drawer_metric(),
        SidebarAction::CycleScale if app.any_drawers_open() => app.cycle_drawer_axis(env.now),
        SidebarAction::CycleMetric | SidebarAction::CycleScale => {}
        SidebarAction::CycleGrouping => return vec![Effect::CycleAutomaticGrouping],
        SidebarAction::Menu => return vec![Effect::OpenActions(PopupTrigger::Keyboard)],
        SidebarAction::Search => app.start_search(),
    }
    Vec::new()
}

/// Typing a query. Text keys edit it and the list follows each change;
/// Enter keeps the filter and goes back to the keymap, Tab also opens the
/// selection, and Esc drops the query. The arrows and Ctrl-n/Ctrl-p move the
/// selection without leaving the query.
fn handle_search_key(app: &mut SidebarApp, key: KeyEvent, list_height: u16) -> Vec<Effect> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc => app.clear_search(list_height),
        KeyCode::Char('c' | 'g') if control => app.clear_search(list_height),
        KeyCode::Enter => {
            app.search.editing = false;
            if !app.search.active() {
                app.clear_search(list_height);
            }
        }
        KeyCode::Tab => {
            app.search.editing = false;
            if app.rows.iter().any(SidebarListRow::selectable) {
                return vec![Effect::RunPrimaryAction];
            }
        }
        KeyCode::Up => app.move_cursor(-1, list_height),
        KeyCode::Down => app.move_cursor(1, list_height),
        KeyCode::Char('p') if control => app.move_cursor(-1, list_height),
        KeyCode::Char('n') if control => app.move_cursor(1, list_height),
        KeyCode::Backspace => {
            if app.search.query.pop().is_some() {
                app.search_changed(list_height);
            } else {
                // Backspace on an empty query leaves search, as in Vim.
                app.clear_search(list_height);
            }
        }
        KeyCode::Char('u') if control => {
            app.search.query.clear();
            app.search_changed(list_height);
        }
        KeyCode::Char('w') if control => {
            let query = &mut app.search.query;
            let trimmed = query.trim_end().len();
            query.truncate(trimmed);
            let word_start = query.rfind(char::is_whitespace).map_or(0, |at| at + 1);
            query.truncate(word_start);
            app.search_changed(list_height);
        }
        KeyCode::Char(character) if !control && !alt => {
            app.search.query.push(character);
            app.search_changed(list_height);
        }
        _ => {}
    }
    Vec::new()
}

/// The keymap help is modal: its keys close or scroll it, and nothing reaches
/// the list behind it. Esc always closes it, whatever the keymap says.
fn handle_help_key(app: &mut SidebarApp, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.pending_keys.clear();
        app.help_open = false;
        return;
    }
    let page = usize::from(app.help_page.max(1));
    // The help is not beside the list, so the side never matters here.
    match app.resolve_key(key, false) {
        Some(SidebarAction::Help | SidebarAction::Close) => app.help_open = false,
        Some(SidebarAction::MoveUp) => app.help_scroll = app.help_scroll.saturating_sub(1),
        Some(SidebarAction::MoveDown) => app.help_scroll = app.help_scroll.saturating_add(1),
        Some(SidebarAction::HalfPageUp) => {
            app.help_scroll = app.help_scroll.saturating_sub(page.div_ceil(2));
        }
        Some(SidebarAction::HalfPageDown) => {
            app.help_scroll = app.help_scroll.saturating_add(page.div_ceil(2));
        }
        Some(SidebarAction::PageUp) => app.help_scroll = app.help_scroll.saturating_sub(page),
        Some(SidebarAction::PageDown) => app.help_scroll = app.help_scroll.saturating_add(page),
        Some(SidebarAction::First | SidebarAction::SelectHeader) => app.help_scroll = 0,
        // Drawing clamps this to the last page.
        Some(SidebarAction::Last) => app.help_scroll = usize::MAX,
        _ => {}
    }
}

fn select_last_row(app: &mut SidebarApp, list_height: u16) {
    app.header_selected = false;
    app.selected = app
        .rows
        .iter()
        .rposition(SidebarListRow::selectable)
        .unwrap_or(0);
    app.ensure_selected_item_visible(list_height);
}

pub(super) fn handle_mouse(app: &mut SidebarApp, mouse: MouseEvent, env: &InputEnv) -> Vec<Effect> {
    if app.help_open {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => app.help_open = false,
            MouseEventKind::ScrollUp => app.help_scroll = app.help_scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => app.help_scroll = app.help_scroll.saturating_add(3),
            _ => {}
        }
        return Vec::new();
    }
    let list_height = env.list_height;
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) if app.resize_drag.is_some() => {
            app.pending_mouse_menu = None;
            return vec![Effect::ResizeDrag {
                column: mouse.column,
            }];
        }
        MouseEventKind::Up(MouseButton::Left) if app.resize_drag.is_some() => {
            app.resize_drag = None;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(pending) = app.pending_mouse_menu.as_mut() {
                pending.mark_released(env.now);
            }
        }
        MouseEventKind::Moved => {
            let over_handle =
                sidebar_resize_handle_rect(env.area).contains((mouse.column, mouse.row).into());
            // Refresh the linger clock while over the handle; clear at once
            // when the pointer moves onto any other cell we're told about.
            // (Leaving the pane entirely sends no event; the linger expiry
            // in the run loop handles that.)
            app.handle_hover_seen = over_handle.then_some(env.now);
            let list_area = sidebar_list_area(env.area, env.pane_label_visible);
            let hovered = match sidebar_mouse_target(app, list_area, mouse.row) {
                Some(SidebarMouseTarget::Process(row)) => app.flash_identity(row),
                _ => None,
            };
            app.pane_flash.observe_hover(
                hovered.as_deref(),
                pointer_at_side_edge(env.area, mouse.column),
                env.now,
            );
        }
        MouseEventKind::ScrollUp if app.sidebar_focused => {
            app.pending_mouse_menu = None;
            app.move_cursor(-3, list_height)
        }
        MouseEventKind::ScrollDown if app.sidebar_focused => {
            app.pending_mouse_menu = None;
            app.move_cursor(3, list_height)
        }
        MouseEventKind::ScrollUp => {
            app.pending_mouse_menu = None;
            app.scroll_viewport(-3);
        }
        MouseEventKind::ScrollDown => {
            app.pending_mouse_menu = None;
            app.scroll_viewport(3);
        }
        MouseEventKind::Down(MouseButton::Left) => return left_press(app, mouse, env),
        _ => {}
    }
    Vec::new()
}

fn left_press(app: &mut SidebarApp, mouse: MouseEvent, env: &InputEnv) -> Vec<Effect> {
    let list_height = env.list_height;
    // A release can occur over the neighboring pane after a fast drag, in
    // which case tmux does not forward it back here. Every new press starts
    // a fresh gesture.
    app.resize_drag = None;
    app.pending_mouse_menu = None;
    if handle_sidebar_graph_click(
        app,
        env.area,
        env.pane_label_visible,
        mouse.column,
        mouse.row,
        env.now,
    ) {
        app.handle_hover_seen = None;
        return Vec::new();
    }
    if sidebar_resize_hotspot(env.area, mouse.column, mouse.row, env.sidebar_on_right) {
        return vec![Effect::BeginResize {
            column: mouse.column,
        }];
    }
    let list_area = sidebar_list_area(env.area, env.pane_label_visible);
    match sidebar_mouse_target(app, list_area, mouse.row) {
        Some(SidebarMouseTarget::Group(index)) => {
            app.header_selected = false;
            app.selected = index;
            return app.toggle_selected_drawer(list_height);
        }
        Some(SidebarMouseTarget::Process(index)) => {
            let was_selected = !app.header_selected && index == app.selected;
            app.header_selected = false;
            app.selected = index;
            if let Some(identity) = app.flash_identity(index) {
                app.pane_flash.observe_click(
                    &identity,
                    pointer_at_side_edge(list_area, mouse.column),
                    env.now,
                );
            }
            app.ensure_selected_item_visible(list_height);
            let row_identity = app
                .selected_row_identity()
                .expect("a clicked sidebar row is selectable")
                .to_string();
            if let Some(pending) = app.take_pending_double_click(&row_identity) {
                // tmux's delayed DoubleClick event. Without pmux's click
                // override its binding has just re-selected this sidebar, so
                // act now and our focus sticks; with it, the action already
                // ran and this press is swallowed.
                if !pending.acted {
                    return vec![Effect::RunDoubleClickAction];
                }
                return Vec::new();
            }
            match app.register_row_click(&row_identity, env.now) {
                RowClick::Double => {
                    // The first press may have scheduled the menu; the
                    // double-click replaces it.
                    app.pending_mouse_menu = None;
                    return vec![Effect::DoubleClick {
                        row_identity,
                        at: env.now,
                    }];
                }
                RowClick::Repeat => {}
                RowClick::Single => {
                    if opens_context_menu_on_left_click(was_selected) {
                        app.pending_mouse_menu = Some(PendingMouseMenu::new(row_identity, env.now));
                    }
                }
            }
        }
        None => {}
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;
    use crossterm::event::KeyModifiers;

    // Three running processes, no grouping, and the cursor on the first.
    fn app_with_processes() -> SidebarApp {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.processes = ["alpha", "beta", "gamma"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut process = test_process(Some("/work"), None);
                process.process.name = (*name).to_string();
                process.process.pane_id = format!("%{}", i + 10);
                process.process.created_at = 10 - i as i64;
                process
            })
            .collect();
        app.rebuild_rows();
        app.sidebar_focused = true;
        app.header_selected = false;
        app.selected = first_process_row(&app);
        app
    }

    fn type_keys(app: &mut SidebarApp, text: &str, env: &InputEnv) {
        for character in text.chars() {
            handle_key(app, key(KeyCode::Char(character)), env);
        }
    }

    fn listed(app: &SidebarApp) -> Vec<&str> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Process { process_index, .. } => {
                    Some(app.processes[*process_index].process.name.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn slash_filters_the_list_as_you_type_and_esc_clears_it() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        type_keys(&mut app, "/gam", &env);
        assert!(app.search.editing);
        assert_eq!(listed(&app), ["gamma"]);
        assert_eq!(app.selected_process_name(), Some("gamma"));
        // While typing, `q` is text, not close.
        assert!(handle_key(&mut app, key(KeyCode::Char('q')), &env).is_empty());
        assert!(listed(&app).is_empty());
        handle_key(&mut app, key(KeyCode::Backspace), &env);
        assert_eq!(listed(&app), ["gamma"]);

        // Enter keeps the filter and gives keys back to the keymap.
        handle_key(&mut app, key(KeyCode::Enter), &env);
        assert!(!app.search.editing);
        assert_eq!(listed(&app), ["gamma"]);
        // The first Esc clears the filter, keeping the selection; the next
        // closes the sidebar.
        assert!(handle_key(&mut app, key(KeyCode::Esc), &env).is_empty());
        assert_eq!(listed(&app), ["alpha", "beta", "gamma"]);
        assert_eq!(app.selected_process_name(), Some("gamma"));
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Esc), &env),
            vec![Effect::CloseSidebar]
        );
    }

    #[test]
    fn tab_while_searching_opens_the_selected_match() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        type_keys(&mut app, "/a", &env);
        // alpha, beta and gamma all match; Down moves within the matches.
        handle_key(&mut app, key(KeyCode::Down), &env);
        assert_eq!(app.selected_process_name(), Some("beta"));
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Tab), &env),
            vec![Effect::RunPrimaryAction]
        );
        assert!(!app.search.editing);
        assert!(app.search.active());
    }

    #[test]
    fn search_line_edits_and_esc_while_typing_drops_the_query() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        type_keys(&mut app, "/be ta", &env);
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl('w'), &env);
        assert_eq!(app.search.query, "be ");
        handle_key(&mut app, ctrl('u'), &env);
        assert_eq!(app.search.query, "");
        assert_eq!(listed(&app).len(), 3);
        type_keys(&mut app, "beta", &env);
        handle_key(&mut app, key(KeyCode::Esc), &env);
        assert_eq!(app.search, SidebarSearch::default());
        assert_eq!(listed(&app).len(), 3);
    }

    // A 40x30 sidebar at a fixed instant.
    fn env(now: Instant) -> InputEnv {
        InputEnv::new(40, 30, false, false, now)
    }

    fn first_process_row(app: &SidebarApp) -> usize {
        app.rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Process { .. }))
            .expect("a process row")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // The screen row a click must land on to hit list row `index`, found
    // with the same hit-testing the handler uses.
    fn screen_row_of(app: &SidebarApp, env: &InputEnv, index: usize) -> u16 {
        let list_area = sidebar_list_area(env.area, env.pane_label_visible);
        (list_area.y..list_area.y + list_area.height)
            .find(|row| {
                sidebar_mouse_target(app, list_area, *row)
                    == Some(SidebarMouseTarget::Process(index))
            })
            .expect("row is on screen")
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn left_press_at(row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), 10, row)
    }

    #[test]
    fn j_and_k_move_the_selection_without_effects() {
        let mut app = app_with_processes();
        let start = app.selected;
        let env = env(Instant::now());
        assert!(handle_key(&mut app, key(KeyCode::Char('j')), &env).is_empty());
        assert_ne!(app.selected, start);
        assert!(handle_key(&mut app, key(KeyCode::Char('k')), &env).is_empty());
        assert_eq!(app.selected, start);
    }

    #[test]
    fn q_and_esc_ask_the_outer_server_to_close_the_sidebar() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            assert_eq!(
                handle_key(&mut app, key(code), &env),
                [Effect::CloseSidebar]
            );
        }
    }

    #[test]
    fn tab_runs_the_primary_action_and_any_key_cancels_a_pending_menu() {
        let mut app = app_with_processes();
        let now = Instant::now();
        app.pending_mouse_menu = Some(PendingMouseMenu::new("x".to_string(), now));
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Tab), &env(now)),
            [Effect::RunPrimaryAction]
        );
        assert!(app.pending_mouse_menu.is_none());
    }

    #[test]
    fn a_cycles_grouping_through_an_effect() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('a')), &env),
            [Effect::CycleAutomaticGrouping]
        );
    }

    #[test]
    fn shift_g_and_gg_jump_to_the_ends() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert!(handle_key(&mut app, shift_g, &env).is_empty());
        let last = app
            .rows
            .iter()
            .rposition(SidebarListRow::selectable)
            .unwrap();
        assert_eq!(app.selected, last);
        handle_key(&mut app, key(KeyCode::Char('g')), &env);
        assert_eq!(app.pending_keys.len(), 1);
        handle_key(&mut app, key(KeyCode::Char('g')), &env);
        assert_eq!(app.selected, first_process_row(&app));
    }

    #[test]
    fn open_help_swallows_keys_and_q_or_a_click_closes_only_the_help() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        let start = app.selected;
        app.help_open = true;
        assert!(handle_key(&mut app, key(KeyCode::Char('j')), &env).is_empty());
        assert!(app.help_open);
        assert_eq!(app.selected, start);
        // `q` dismisses the help instead of closing the sidebar.
        assert!(handle_key(&mut app, key(KeyCode::Char('q')), &env).is_empty());
        assert!(!app.help_open);

        app.help_open = true;
        assert!(handle_mouse(&mut app, left_press_at(5), &env).is_empty());
        assert!(!app.help_open);
    }

    #[test]
    fn a_quick_second_click_on_a_row_is_a_double_click_not_a_menu() {
        let mut app = app_with_processes();
        let target = first_process_row(&app) + 1;
        let start = Instant::now();
        let row = screen_row_of(&app, &env(start), target);

        // The first press on an unselected row only selects it.
        assert!(handle_mouse(&mut app, left_press_at(row), &env(start)).is_empty());
        assert_eq!(app.selected, target);
        assert!(app.pending_mouse_menu.is_none());

        // A quick second press is a double-click: no menu, and one effect
        // carrying the row and time (the executor decides whether to act now).
        let later = start + Duration::from_millis(100);
        let effects = handle_mouse(&mut app, left_press_at(row), &env(later));
        let identity = app.selected_row_identity().unwrap().to_string();
        assert_eq!(
            effects,
            [Effect::DoubleClick {
                row_identity: identity,
                at: later,
            }]
        );
        assert!(app.pending_mouse_menu.is_none());
    }

    #[test]
    fn a_slow_second_click_on_the_selected_row_schedules_the_menu() {
        let mut app = app_with_processes();
        let target = first_process_row(&app);
        let start = Instant::now();
        let row = screen_row_of(&app, &env(start), target);
        handle_mouse(&mut app, left_press_at(row), &env(start));
        let later = start + Duration::from_secs(2);
        assert!(handle_mouse(&mut app, left_press_at(row), &env(later)).is_empty());
        assert!(app.pending_mouse_menu.is_some());
    }

    #[test]
    fn tmuxs_forwarded_double_click_runs_a_held_action_or_is_swallowed() {
        let now = Instant::now();
        for acted in [false, true] {
            let mut app = app_with_processes();
            let index = first_process_row(&app);
            let row = screen_row_of(&app, &env(now), index);
            let identity = app.selected_row_identity().unwrap().to_string();
            app.pending_double_click = Some(PendingDoubleClick::new(identity, now, acted));
            let effects = handle_mouse(&mut app, left_press_at(row), &env(now));
            let expected: &[Effect] = if acted {
                &[]
            } else {
                &[Effect::RunDoubleClickAction]
            };
            assert_eq!(effects, expected, "acted = {acted}");
            assert!(app.pending_double_click.is_none());
        }
    }

    #[test]
    fn dragging_resizes_only_during_an_active_resize() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 30, 1);
        assert!(handle_mouse(&mut app, drag, &env).is_empty());
        app.resize_drag = Some(SidebarResizeDrag::new(0, 40, 39));
        assert_eq!(
            handle_mouse(&mut app, drag, &env),
            [Effect::ResizeDrag { column: 30 }]
        );
    }

    #[test]
    fn the_inward_arrow_and_resize_edge_follow_the_sidebar_side() {
        let now = Instant::now();
        let left = InputEnv::new(40, 30, false, false, now);
        let right = InputEnv::new(40, 30, false, true, now);
        let mut app = app_with_processes();
        let menu = [Effect::OpenActions(PopupTrigger::Keyboard)];
        assert_eq!(handle_key(&mut app, key(KeyCode::Right), &left), menu);
        assert!(handle_key(&mut app, key(KeyCode::Left), &left).is_empty());
        assert_eq!(handle_key(&mut app, key(KeyCode::Left), &right), menu);
        // The draggable edge is the one facing the main panes.
        let press = |column| mouse(MouseEventKind::Down(MouseButton::Left), column, 12);
        assert_eq!(
            handle_mouse(&mut app, press(39), &left),
            [Effect::BeginResize { column: 39 }]
        );
        assert_eq!(
            handle_mouse(&mut app, press(0), &right),
            [Effect::BeginResize { column: 0 }]
        );
    }

    #[test]
    fn a_scheduled_menu_opens_from_tick_once_due_and_only_for_its_row() {
        let mut app = app_with_processes();
        let start = Instant::now();
        let identity = app.selected_row_identity().unwrap().to_string();
        let mut pending = PendingMouseMenu::new(identity, start);
        pending.mark_released(start);
        let due = pending.ready_at();
        app.pending_mouse_menu = Some(pending.clone());
        assert!(app.tick(start).is_empty());
        assert!(app.pending_mouse_menu.is_some());
        assert_eq!(app.tick(due), [Effect::OpenActions(PopupTrigger::Mouse)]);
        assert!(app.pending_mouse_menu.is_none());

        // The cursor moved away meanwhile: the menu is dropped, not opened.
        app.pending_mouse_menu = Some(pending);
        app.move_cursor(1, 20);
        assert!(app.tick(due).is_empty());
        assert!(app.pending_mouse_menu.is_none());
    }

    #[test]
    fn a_held_double_click_acts_from_tick_after_the_settle_window() {
        let start = Instant::now();
        for acted in [false, true] {
            let mut app = app_with_processes();
            let identity = app.selected_row_identity().unwrap().to_string();
            let pending = PendingDoubleClick::new(identity, start, acted);
            let due = pending.ready_at();
            app.pending_double_click = Some(pending);
            assert!(app.tick(start).is_empty());
            let expected: &[Effect] = if acted {
                &[]
            } else {
                &[Effect::RunDoubleClickAction]
            };
            assert_eq!(app.tick(due), expected, "acted = {acted}");
            assert!(app.pending_double_click.is_none());
        }
    }

    #[test]
    fn the_handle_hover_lapses_from_tick_after_the_linger() {
        let mut app = app_with_processes();
        let start = Instant::now();
        app.handle_hover_seen = Some(start);
        assert!(app.tick(start + HANDLE_HOVER_LINGER).is_empty());
        assert_eq!(
            app.tick(start + HANDLE_HOVER_LINGER + Duration::from_millis(1)),
            [Effect::Redraw]
        );
        assert!(app.handle_hover_seen.is_none());
    }

    #[test]
    fn enter_on_a_group_folds_it_in_memory_and_asks_to_persist_the_fold() {
        let mut app = app_with_processes();
        app.grouping.automatic = crate::AutomaticGrouping::Cwd;
        app.rebuild_rows();
        let group = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Group(_)))
            .unwrap();
        app.selected = group;
        let identity = app.rows[group].identity().unwrap().to_string();
        let effects = handle_key(&mut app, key(KeyCode::Enter), &env(Instant::now()));
        assert_eq!(
            effects,
            [Effect::SaveGrouping(crate::GroupingChange::Fold {
                identity: identity.clone(),
                collapsed: true,
            })]
        );
        assert!(app.grouping.collapsed.contains(&identity));
        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row, SidebarListRow::Process { .. }))
        );
    }

    #[test]
    fn a_cycled_grouping_resets_the_viewport_and_persists_its_new_order() {
        let mut app = app_with_processes();
        app.first_visible = 2;
        let effects = app.grouping_cycled(Ok(crate::AutomaticGrouping::Cwd));
        assert_eq!(app.grouping.automatic, crate::AutomaticGrouping::Cwd);
        assert_eq!(app.first_visible, 0);
        assert_eq!(app.message, None, "the header shows the new mode");
        assert!(app.grouping_highlighted(Instant::now()));
        assert!(matches!(
            effects.as_slice(),
            [Effect::SaveGrouping(crate::GroupingChange::AutomaticOrder { mode, .. })]
                if mode == "cwd"
        ));

        let effects = app.grouping_cycled(Err(anyhow!("locked")));
        assert!(effects.is_empty());
        assert_eq!(app.grouping.automatic, crate::AutomaticGrouping::Cwd);
        assert_eq!(
            app.message.as_deref(),
            Some("Could not change grouping: locked")
        );
    }

    #[test]
    fn a_resize_drag_asks_for_a_new_width_only_when_the_pointer_changes_it() {
        let mut app = app_with_processes();
        assert_eq!(app.resize_drag_request(30, false), None, "no drag active");
        app.begin_resize(Ok(SidebarResizeDrag::new(0, 40, 39)));
        // Pointer back where it started: nothing to resize.
        assert_eq!(app.resize_drag_request(39, false), None);
        let (width, pointer_x) = app.resize_drag_request(44, false).unwrap();
        assert_eq!(width, 45);
        // tmux applied a narrower width than asked for.
        app.finish_resize_step(Ok(43), pointer_x);
        assert_eq!(app.resize_drag.unwrap().width, 43);
        app.finish_resize_step(Err(anyhow!("gone")), pointer_x);
        assert_eq!(
            app.message.as_deref(),
            Some("Could not resize sidebar: gone")
        );

        app.resize_drag = None;
        app.begin_resize(Err(anyhow!("no pane")));
        assert!(app.resize_drag.is_none());
        assert_eq!(
            app.message.as_deref(),
            Some("Could not start sidebar resize: no pane")
        );
    }

    #[test]
    fn scrolling_moves_the_cursor_when_focused_and_never_otherwise() {
        let env = env(Instant::now());
        let scroll = mouse(MouseEventKind::ScrollDown, 5, 5);
        let mut app = app_with_processes();
        let start = app.selected;
        handle_mouse(&mut app, scroll, &env);
        assert_ne!(app.selected, start);

        let mut app = app_with_processes();
        app.sidebar_focused = false;
        let start = app.selected;
        handle_mouse(&mut app, scroll, &env);
        assert_eq!(app.selected, start);
    }

    #[test]
    fn z_folds_the_section_of_the_selected_row_from_any_row_in_it() {
        let mut app = app_with_processes();
        app.grouping.automatic = crate::AutomaticGrouping::Cwd;
        app.rebuild_rows();
        let group = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Group(_)))
            .unwrap();
        let identity = app.rows[group].identity().unwrap().to_string();
        app.selected = first_process_row(&app) + 1;
        let effects = handle_key(&mut app, key(KeyCode::Char('z')), &env(Instant::now()));
        assert_eq!(
            effects,
            [Effect::SaveGrouping(crate::GroupingChange::Fold {
                identity: identity.clone(),
                collapsed: true,
            })]
        );
        assert_eq!(app.selected, group, "the cursor lands on the folded header");
        // On the header itself, `z` unfolds it again.
        handle_key(&mut app, key(KeyCode::Char('z')), &env(Instant::now()));
        assert!(!app.grouping.collapsed.contains(&identity));
    }

    #[test]
    fn z_does_nothing_for_an_ungrouped_process() {
        let mut app = app_with_processes();
        let rows = app.rows.clone();
        assert!(handle_key(&mut app, key(KeyCode::Char('z')), &env(Instant::now())).is_empty());
        assert_eq!(app.rows, rows);
    }

    #[test]
    fn the_open_help_scrolls_with_the_navigation_keys_and_closes_on_esc() {
        let mut app = app_with_processes();
        let env = env(Instant::now());
        handle_key(&mut app, key(KeyCode::Char('?')), &env);
        assert!(app.help_open);
        app.help_page = 10;
        handle_key(&mut app, key(KeyCode::Char('j')), &env);
        handle_key(&mut app, key(KeyCode::Char('j')), &env);
        assert_eq!(app.help_scroll, 2);
        handle_key(&mut app, key(KeyCode::Char('k')), &env);
        assert_eq!(app.help_scroll, 1);
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_d, &env);
        assert_eq!(app.help_scroll, 6);
        handle_key(&mut app, key(KeyCode::Char('g')), &env);
        handle_key(&mut app, key(KeyCode::Char('g')), &env);
        assert_eq!(app.help_scroll, 0);
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 5, 5), &env);
        assert_eq!(app.help_scroll, 3);
        assert_eq!(app.selected, first_process_row(&app), "the list stays put");
        handle_key(&mut app, key(KeyCode::Esc), &env);
        assert!(!app.help_open);
    }

    #[test]
    fn a_rebound_key_runs_its_new_action() {
        let mut app = app_with_processes();
        app.keymap =
            SidebarKeymap::from_config_text("[sidebar.keys]\nclose = \"Q\"\nmove_down = \"n\"\n").0;
        let env = env(Instant::now());
        assert!(handle_key(&mut app, key(KeyCode::Char('q')), &env).is_empty());
        let start = app.selected;
        handle_key(&mut app, key(KeyCode::Char('n')), &env);
        assert_ne!(app.selected, start);
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('Q')), &env),
            [Effect::CloseSidebar]
        );
    }

    #[test]
    fn enter_and_tab_mean_the_same_on_a_past_session_as_on_a_process() {
        let mut app = app_with_processes();
        app.grouping.resumable_expanded = true;
        app.set_history(vec![test_history_entry("past", "/work", 10)]);
        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Resumable { .. }))
            .unwrap();
        let env = env(Instant::now());
        // Enter opens its drawer rather than resuming it...
        assert!(handle_key(&mut app, key(KeyCode::Enter), &env).is_empty());
        assert!(!app.open_past_drawers.is_empty());
        assert!(handle_key(&mut app, key(KeyCode::Enter), &env).is_empty());
        assert!(app.open_past_drawers.is_empty());
        // ...and Tab resumes it, as Tab opens a process.
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Tab), &env),
            [Effect::RunPrimaryAction]
        );
    }
}
