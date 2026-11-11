//! The sidebar's run loop: input, navigation, mouse, resize, and re-exec.

use super::*;

/// Run the interactive sidebar. The explicit values are supplied by the outer
/// tmux binding; direct invocation falls back to the current tmux environment.
pub fn run_sidebar(
    socket: Option<&str>,
    host_pane: Option<&str>,
    has_pane_label: bool,
) -> Result<()> {
    let reload_state = SidebarReloadState::from_environment();
    // After an upgrade the sidebar re-execs with the host it was first opened
    // from, which may have closed since; resolve it through our own pane.
    let resolved_host = match (socket, host_pane) {
        (Some(socket), Some(host_pane)) => Some(crate::resolve_sidebar_host(
            socket,
            host_pane,
            std::env::var("TMUX_PANE").ok().as_deref(),
        )),
        _ => host_pane.map(str::to_string),
    };
    let host_pane = resolved_host.as_deref();
    let context = crate::sidebar_context(socket, host_pane)?;
    // Install the focus-refresh handler before anything else so the window in
    // which a stray early SIGUSR1 (from the focus-hook) could otherwise
    // terminate the process is as small as possible.
    install_focus_refresh_handler();
    // A sidebar killed mid-flash (e.g. its pane closed directly) cannot clean
    // up after itself; clear anything it left lit.
    crate::clear_stale_flash_options(&context.socket);
    let sidebar_pane = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane| !pane.trim().is_empty());
    // The creator evaluates the outer pane-border option before launching the
    // child, so this stays correct even on tmux versions that do not expose a
    // fully initialized pane context during process startup.
    let pane_label_visible = has_pane_label;
    // Failure to read the executable is deliberately non-fatal: a directly
    // invoked sidebar should remain usable even in an unusual PATH-only setup;
    // it merely opts out of automatic reload for that run.
    let mut binary_watcher = SidebarBinaryWatcher::for_running_binary(Instant::now());
    let mut events = render::EventStream::start(Instant::now());
    let result = render::with_terminal_opts(render::MouseMode::Full, |terminal| {
        run_sidebar_loop(
            terminal,
            &context,
            &mut events,
            sidebar_pane,
            pane_label_visible,
            binary_watcher.as_mut(),
            reload_state,
        )
    });
    drop(events);
    // Also covers a reload: `exec` would otherwise leave the flash lit with no
    // thread left to clear it.
    crate::stop_outer_flash();
    match result? {
        // `with_terminal_opts` has restored raw mode, mouse tracking, and the
        // alternate screen before this point. Exec only now so a replaced
        // binary never inherits a half-initialized terminal.
        SidebarLoopExit::Reload(state) => reexec_sidebar(socket, host_pane, has_pane_label, &state),
    }
}

fn run_sidebar_loop(
    terminal: &mut AppTerminal,
    context: &crate::SidebarContext,
    events: &mut render::EventStream,
    sidebar_pane: Option<String>,
    pane_label_visible: bool,
    mut binary_watcher: Option<&mut SidebarBinaryWatcher>,
    reload_state: SidebarReloadState,
) -> Result<SidebarLoopExit> {
    let close_sidebar_pane = sidebar_pane.clone();
    let mut app = SidebarApp::new(sidebar_pane);
    watch_pmux_config_changed(app.pmux_config_changed.clone());
    let (keymap, mut config_warnings) = SidebarKeymap::load();
    app.keymap = keymap;
    let (row_layout, row_warnings) = render::load_row_layout(render::RowSurface::Sidebar);
    render::set_row_layout(row_layout);
    config_warnings.extend(row_warnings);
    if let Some(first) = config_warnings.first() {
        app.message = Some(match config_warnings.len() {
            1 => first.clone(),
            count => format!("{first} (+{} more)", count - 1),
        });
    }
    let mut pending_reload_state = Some(reload_state);
    app.toggle_hint = sidebar_toggle_hint(context);
    app.active_pane_border_color = active_pane_border_color(context);
    app.focus_hook_fires = focus_hook_fires(context);
    (
        app.active_pane_background_color,
        app.inactive_pane_background_color,
    ) = pane_background_colors(context);
    // Outer focus and process locations are read on background threads (see
    // `poller`); this thread only applies what they observe. The first
    // location refresh reads the state itself rather than waiting for the
    // event stream's first snapshot.
    let mut poller = SidebarPoller::spawn(context);
    poller.request_locations(None);
    let opened_at = Instant::now();
    let mut processes_loaded = false;
    // Redraw only when something on screen may have changed (or the clock
    // moved ages along), and not at all while the window is hidden: building
    // every row, drawer, and activity graph every 100ms was the sidebar's main
    // steady CPU cost. Becoming visible changes `window_active`, which marks
    // the frame dirty.
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    loop {
        if binary_watcher
            .as_deref_mut()
            .is_some_and(|watcher| watcher.reload_ready(Instant::now()))
        {
            return Ok(SidebarLoopExit::Reload(SidebarReloadState::capture(&app)));
        }
        let list_height = terminal
            .size()?
            .height
            .saturating_sub(sidebar_header_height(pane_label_visible) + 2);
        // #3: a SIGUSR1 from the focus-hook forces the next poll to run now,
        // bypassing the (possibly backed-off) interval so foregrounding a window
        // updates its sidebar instantly instead of after the backoff. It also
        // expires the location timer so locations refresh on arrival too.
        if take_focus_refresh_request() {
            app.force_focus_refresh = true;
            app.last_location_refresh = Instant::now()
                .checked_sub(LOCATION_POLL_INACTIVE)
                .unwrap_or_else(Instant::now);
        }
        let now = Instant::now();
        // A focus query still running (say, against a hung outer server) is
        // not queued behind; the next one starts once it has finished.
        if app.focus_refresh_due(now) && !poller.focus.busy() {
            poller.focus.request(());
            app.mark_focus_requested(now);
        }
        // Keep only the latest event when a busy server produces several
        // snapshots before the next paint. A dead `events` child is
        // respawned with backoff; the footer says so meanwhile.
        let latest = events.poll_latest(now);
        if app.event_stream_down != events.is_down() {
            app.event_stream_down = events.is_down();
            dirty = true;
        }
        if let Some(snapshot) = latest {
            let snapshot = Arc::new(snapshot);
            app.last_snapshot = Some(Arc::clone(&snapshot));
            poller.request_locations(Some(snapshot));
            app.last_location_refresh = now;
        } else if app.last_location_refresh.elapsed() >= app.location_poll_interval()
            && !poller.locations.busy()
        {
            // Pane/window movement updates host mappings independently of the
            // process state event stream, so refresh locations periodically —
            // rarely while backgrounded, since the result is off screen. The
            // processes themselves come from the stream's last snapshot; only
            // while the stream is down is the state read afresh.
            let snapshot = (!events.is_down())
                .then(|| app.last_snapshot.clone())
                .flatten();
            poller.request_locations(snapshot);
            app.last_location_refresh = now;
        }
        if let Some(Ok(focus)) = poller.focus.try_take() {
            let before_focus = app.view_state();
            app.apply_focus(focus, list_height);
            dirty |= app.view_state() != before_focus;
        }
        if let Some(Ok(locations)) = poller.locations.try_take() {
            // A snapshot the worker read itself is older than any the stream
            // has delivered meanwhile; adopt it only when there is no other.
            if let Some(fetched) = locations.fetched
                && (app.last_snapshot.is_none() || events.is_down())
            {
                app.last_snapshot = Some(fetched);
            }
            app.apply_processes(locations.processes, list_height);
            if let Some(state) = pending_reload_state.take() {
                state.restore(&mut app, list_height);
            }
            processes_loaded = true;
            dirty = true;
        }

        // Fold in completed drawer fetches, then start any missing fetches for
        // the independently opened rows.
        dirty |= app.drain_drawer_results(list_height);
        app.ensure_drawer_fetch();
        app.ensure_activity_refresh(Instant::now());
        app.ensure_past_prompt_counts(list_height);
        // Likewise for the agent history behind the Resumable group and titles.
        dirty |= app.drain_history_results();
        app.check_labels_stamp(now);
        app.ensure_history_fetch();
        // A crashed worker has already been contained (see
        // `render::spawn_worker`); say so rather than failing silently.
        if let Some(panic) = render::take_worker_panic() {
            app.message = Some(format!("Background task failed: {panic}"));
            dirty = true;
        }

        // Hover expiry, a scheduled mouse menu, and a held double-click.
        let effects = app.tick(Instant::now());
        dirty |= !effects.is_empty();
        run_effects(
            effects,
            &mut app,
            context,
            pane_label_visible,
            close_sidebar_pane.as_deref(),
        )?;
        let flash_before = app.flash_key();
        app.update_pane_flash(context, Instant::now());
        // An active flash alternates phases over time; the poll below wakes at
        // each phase boundary, so redraw on every such wake.
        dirty |= app.flash_key() != flash_before || app.flash_key().is_some();

        let now = Instant::now();
        if app.window_active && last_draw.is_none_or(|at| now.duration_since(at) >= CLOCK_REDRAW) {
            // Relative ages and briefly shown hints are time-driven.
            dirty = true;
        }
        // Hold the very first frame briefly for the first process list, so
        // opening the sidebar does not flash its empty state.
        let first_frame_ready =
            processes_loaded || now.duration_since(opened_at) >= FIRST_FRAME_WAIT;
        if dirty && app.window_active && first_frame_ready {
            terminal.draw(|frame| draw_sidebar(frame, &mut app, context, pane_label_visible))?;
            dirty = false;
            last_draw = Some(now);
        }
        let mut poll = app
            .flash_poll_timeout(Instant::now())
            .map_or(EVENT_POLL, |timeout| timeout.min(EVENT_POLL));
        if poller.busy() || !first_frame_ready {
            // Apply a background result soon after it lands rather than on
            // the next input or the idle tick.
            poll = poll.min(POLLER_RESULT_POLL);
        }
        if !event::poll(poll)? {
            continue;
        }
        let event = event::read()?;
        // Any input (keys, mouse, resize, paste) can change what is shown.
        dirty = true;
        // A resize is how the sidebar learns its last neighbour closed: tmux
        // hands it the whole window. A window left with only the sidebar then
        // closes (unless it is the session's last window), and tmux ends this
        // process along with its pane.
        if matches!(event, Event::Resize(..))
            && let Some(sidebar_pane) = app.sidebar_pane.as_deref()
            && crate::close_window_if_only_sidebar(&context.socket, sidebar_pane).unwrap_or(false)
        {
            continue;
        }
        let size = terminal.size()?;
        let env = InputEnv::new(
            size.width,
            size.height,
            pane_label_visible,
            context.sidebar_on_right,
            Instant::now(),
        );
        let effects = match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(&mut app, key, &env),
            Event::Mouse(mouse) => handle_mouse(&mut app, mouse, &env),
            _ => Vec::new(),
        };
        run_effects(
            effects,
            &mut app,
            context,
            pane_label_visible,
            close_sidebar_pane.as_deref(),
        )?;
    }
}

/// Perform the side effects input handling asked for (see `input::Effect`).
/// Only closing the sidebar can fail the loop; other failures surface as the
/// sidebar's status message, as they always have. The state changes around
/// each effect live in `SidebarApp`; this only does the I/O between them.
fn run_effects(
    effects: Vec<Effect>,
    app: &mut SidebarApp,
    context: &crate::SidebarContext,
    pane_label_visible: bool,
    close_sidebar_pane: Option<&str>,
) -> Result<()> {
    let mut queue = std::collections::VecDeque::from(effects);
    while let Some(effect) = queue.pop_front() {
        match effect {
            Effect::CloseSidebar => request_sidebar_close(context, close_sidebar_pane)?,
            Effect::RunPrimaryAction => run_primary_action_for_selected(app, context),
            Effect::RunDoubleClickAction => run_double_click_action(app, context),
            Effect::DoubleClick { row_identity, at } => {
                // With pmux's outer click override tmux will not steal focus
                // back, so act at once; otherwise wait for tmux's own
                // DoubleClick event (see `PendingDoubleClick`).
                let act_now = crate::sidebar_double_click_installed(context);
                if act_now {
                    run_double_click_action(app, context);
                }
                app.pending_double_click = Some(PendingDoubleClick::new(row_identity, at, act_now));
            }
            Effect::OpenActions(trigger) => {
                open_actions_for_selected(app, context, pane_label_visible, trigger)
            }
            Effect::CycleAutomaticGrouping => {
                queue.extend(app.grouping_cycled(crate::cycle_automatic_grouping()));
            }
            Effect::BeginResize { column } => {
                app.begin_resize(begin_sidebar_resize(app, context, column));
            }
            Effect::ResizeDrag { column } => {
                if let Some((width, pointer_x)) =
                    app.resize_drag_request(column, context.sidebar_on_right)
                {
                    let result = resize_sidebar_pane(app, context, width);
                    app.finish_resize_step(result, pointer_x);
                }
            }
            Effect::SaveGrouping(change) => {
                let result = crate::save_grouping_change(&change);
                match (result, &change) {
                    (Ok(_), _) => {}
                    // The derived order is re-derived on every rebuild.
                    (Err(_), crate::GroupingChange::AutomaticOrder { .. }) => {}
                    (Err(err), _) => {
                        app.message = Some(format!("Could not save group fold state: {err}"));
                    }
                }
            }
            Effect::Redraw => {}
        }
    }
    Ok(())
}

/// Re-enter the sidebar using the freshly installed executable while retaining
/// this exact outer tmux pane. Unix `exec` replaces the current process rather
/// than opening another pane or accumulating a process tree.
#[cfg(unix)]
fn reexec_sidebar(
    socket: Option<&str>,
    host_pane: Option<&str>,
    has_pane_label: bool,
    state: &SidebarReloadState,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let binary = crate::resolve_pmux_binary()?;
    let mut command = Command::new(binary);
    state.apply_to_command(&mut command);
    command.arg("sidebar");
    if let Some(socket) = socket {
        command.args(["--socket", socket]);
    }
    if let Some(host_pane) = host_pane {
        command.args(["--host-pane", host_pane]);
    }
    if has_pane_label {
        command.arg("--pane-label");
    }
    Err(command.exec()).context("re-exec freshly installed pmux sidebar")
}

// pmux supports Unix platforms, where the branch above is used. Keeping a
// fallback makes this module compile for documentation or cross-platform
// checks too; the child inherits the same terminal and replaces the old loop.
#[cfg(not(unix))]
fn reexec_sidebar(
    socket: Option<&str>,
    host_pane: Option<&str>,
    has_pane_label: bool,
    state: &SidebarReloadState,
) -> Result<()> {
    let binary = crate::resolve_pmux_binary()?;
    let mut command = Command::new(binary);
    state.apply_to_command(&mut command);
    command.arg("sidebar");
    if let Some(socket) = socket {
        command.args(["--socket", socket]);
    }
    if let Some(host_pane) = host_pane {
        command.args(["--host-pane", host_pane]);
    }
    if has_pane_label {
        command.arg("--pane-label");
    }
    command
        .spawn()
        .context("start freshly installed pmux sidebar")?;
    Ok(())
}

fn request_sidebar_close(
    context: &crate::SidebarContext,
    sidebar_pane: Option<&str>,
) -> Result<()> {
    let sidebar_pane = sidebar_pane.unwrap_or(&context.host_pane);
    let binary = crate::resolve_pmux_binary()?;
    let command = shell_join(&[
        binary.display().to_string(),
        "toggle-sidebar".to_string(),
        sidebar_pane.to_string(),
        "--socket".to_string(),
        context.socket.clone(),
    ])?;
    // run-shell format-expands its command; keep a `#` in a path literal.
    let command = crate::tmux::tmux_format_escape(&command);
    let status = crate::tmux::tmux_command()
        .args(["-S", &context.socket, "run-shell", "-b", &command])
        .status()
        .context("failed to schedule pmux sidebar close")?;
    if !status.success() {
        bail!("failed to schedule pmux sidebar close");
    }
    Ok(())
}

fn begin_sidebar_resize(
    app: &SidebarApp,
    context: &crate::SidebarContext,
    pointer_column: u16,
) -> Result<SidebarResizeDrag> {
    let sidebar_pane = app
        .sidebar_pane
        .as_deref()
        .ok_or_else(|| anyhow!("sidebar pane id is unavailable"))?;
    let output = crate::tmux::tmux_command()
        .args([
            "-S",
            &context.socket,
            "display-message",
            "-p",
            "-t",
            sidebar_pane,
            "#{pane_left}\t#{pane_width}",
        ])
        .output()
        .context("query pmux sidebar geometry")?;
    if !output.status.success() {
        bail!("outer tmux could not query the sidebar geometry");
    }
    let geometry = String::from_utf8_lossy(&output.stdout);
    let mut fields = geometry.trim().split('\t');
    let pane_left = fields
        .next()
        .ok_or_else(|| anyhow!("outer tmux omitted the sidebar position"))?
        .parse::<u16>()
        .context("outer tmux returned an invalid sidebar position")?;
    let width = fields
        .next()
        .ok_or_else(|| anyhow!("outer tmux omitted the sidebar width"))?
        .parse::<u16>()
        .context("outer tmux returned an invalid sidebar width")?;
    Ok(SidebarResizeDrag::new(pane_left, width, pointer_column))
}

fn resize_sidebar_pane(
    app: &SidebarApp,
    context: &crate::SidebarContext,
    width: u16,
) -> Result<u16> {
    let sidebar_pane = app
        .sidebar_pane
        .as_deref()
        .ok_or_else(|| anyhow!("sidebar pane id is unavailable"))?;
    let width = width.to_string();
    let output = crate::tmux::tmux_command()
        .args([
            "-S",
            &context.socket,
            "resize-pane",
            "-t",
            sidebar_pane,
            "-x",
            &width,
            ";",
            "display-message",
            "-p",
            "-t",
            sidebar_pane,
            "#{pane_width}",
        ])
        .output()
        .context("resize pmux sidebar pane")?;
    if !output.status.success() {
        bail!("outer tmux could not resize the sidebar");
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u16>()
        .context("outer tmux returned an invalid resized sidebar width")
}

pub(super) fn page_step(list_height: u16) -> usize {
    (list_content_height(list_height) / 3).max(1)
}

pub(super) fn half_visible_process_count(visible: usize) -> usize {
    visible.div_ceil(2).max(1)
}

pub(super) fn sidebar_list_area(area: Rect, pane_label_visible: bool) -> Rect {
    let header_height = sidebar_header_height(pane_label_visible);
    Rect::new(
        area.x,
        area.y.saturating_add(header_height),
        area.width,
        area.height.saturating_sub(header_height + 2),
    )
}

pub(super) fn sidebar_resize_handle_rect(area: Rect) -> Rect {
    let width = area.width.min(SIDEBAR_RESIZE_HANDLE_WIDTH);
    // Inset from the right edge by the gap, as long as the pane is wide enough to
    // spare it, so a bare cell sits between the handle and the boundary.
    let gap = SIDEBAR_RESIZE_HANDLE_EDGE_GAP.min(area.width.saturating_sub(width));
    Rect::new(
        area.x
            .saturating_add(area.width.saturating_sub(width + gap)),
        area.y,
        width,
        u16::from(area.height > 0),
    )
}

/// Whether a pointer column is in the pane's first or last column. A pointer
/// leaving the sidebar sideways (into the neighboring pane, or off the
/// terminal) crosses one of these cells, and tmux reports nothing afterwards.
pub(super) fn pointer_at_side_edge(area: Rect, column: u16) -> bool {
    column <= area.x || column.saturating_add(1) >= area.x.saturating_add(area.width)
}

pub(super) fn sidebar_resize_hotspot(
    area: Rect,
    column: u16,
    row: u16,
    sidebar_on_right: bool,
) -> bool {
    let in_handle = sidebar_resize_handle_rect(area).contains((column, row).into());
    let in_edge = if sidebar_on_right {
        column < area.x.saturating_add(SIDEBAR_RESIZE_EDGE_HIT_WIDTH)
    } else {
        column
            >= area
                .x
                .saturating_add(area.width.saturating_sub(SIDEBAR_RESIZE_EDGE_HIT_WIDTH))
    };
    in_handle || (row >= area.y && row < area.y.saturating_add(area.height) && in_edge)
}

fn process_at_mouse_row(app: &SidebarApp, area: Rect, row: u16) -> Option<usize> {
    let end = app.visible_end(app.first_visible, area.height);
    let content_top = i32::from(area.y.saturating_add(1));
    let first_line = i32::try_from(app.first_visible_line).unwrap_or(i32::MAX);
    let mut y = content_top.saturating_sub(first_line);
    let pointer_y = i32::from(row);
    for index in app.first_visible..end {
        // Includes the open drawer's extra lines, so a click below an expanded
        // row still resolves to the row above rather than drifting.
        let row_height = i32::try_from(app.row_line_count(index)).unwrap_or(i32::MAX);
        if pointer_y >= y.max(content_top) && pointer_y < y.saturating_add(row_height) {
            return app
                .rows
                .get(index)
                .is_none_or(SidebarListRow::selectable)
                .then_some(index);
        }
        y = y.saturating_add(row_height);
        if app.row_block_line_count(index) > app.row_line_count(index) {
            y = y.saturating_add(1);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarMouseTarget {
    Group(usize),
    Process(usize),
}

pub(super) fn sidebar_mouse_target(
    app: &SidebarApp,
    area: Rect,
    row: u16,
) -> Option<SidebarMouseTarget> {
    let index = process_at_mouse_row(app, area, row)?;
    match app.rows.get(index) {
        Some(SidebarListRow::Group(_)) => Some(SidebarMouseTarget::Group(index)),
        // Resumable rows select and open their menu exactly like processes.
        Some(SidebarListRow::Process { .. } | SidebarListRow::Resumable { .. }) | None => {
            Some(SidebarMouseTarget::Process(index))
        }
        Some(SidebarListRow::GroupTypeSeparator) => None,
    }
}

fn open_actions_for_selected(
    app: &mut SidebarApp,
    context: &crate::SidebarContext,
    pane_label_visible: bool,
    trigger: PopupTrigger,
) {
    if app.header_selected {
        return;
    }
    let row = selected_process_row(app, pane_label_visible);
    let result = if let Some(group) = app.selected_group() {
        Some(open_group_actions_menu(group, row, context, trigger))
    } else if let Some(entry) = app.selected_resumable() {
        Some(open_resumable_actions_menu(entry, row, context, trigger))
    } else {
        app.selected_process_index()
            .and_then(|index| app.processes.get(index))
            .map(|process| open_session_actions_menu(process, row, context, trigger))
    };
    match result {
        Some(Ok(())) => {}
        Some(Err(err)) => app.message = Some(format!("Could not open actions: {err}")),
        None => {}
    }
}

fn run_primary_action_for_selected(app: &mut SidebarApp, context: &crate::SidebarContext) {
    if app.header_selected {
        return;
    }
    if let Some(entry) = app.selected_resumable() {
        if let Err(err) = resume_in_split(entry, context) {
            app.message = Some(format!("Could not resume session: {err}"));
        }
        return;
    }
    let result = app
        .selected_process_index()
        .and_then(|index| app.processes.get(index))
        .map(|process| run_primary_action(process, context));
    if let Some(Err(err)) = result {
        app.message = Some(format!("Could not open process: {err}"));
    }
}

/// Resume a past session beside the rightmost pane, from its own directory.
fn resume_in_split(entry: &crate::HistoryEntry, context: &crate::SidebarContext) -> Result<()> {
    let cwd = resumable_cwd(entry)?;
    crate::resume_in_new_outer_split(&resumable_target(entry), cwd, context)
}

/// What double-clicking a process row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DoubleClickAction {
    /// Focus the outer pane it is attached in, in any window of this session.
    Jump,
    /// Not attached in this outer session: open it in a new outer window.
    AttachInNewWindow,
}

pub(super) fn double_click_action_for_process(
    process: &crate::SidebarProcess,
) -> DoubleClickAction {
    if process.outer_location.is_some() {
        DoubleClickAction::Jump
    } else {
        DoubleClickAction::AttachInNewWindow
    }
}

/// Double-click on the selected row: jump to a process wherever it is
/// attached in this outer session, else open it in a new window. A past
/// session is resumed in a new window.
fn run_double_click_action(app: &mut SidebarApp, context: &crate::SidebarContext) {
    if app.header_selected {
        return;
    }
    let result = if let Some(entry) = app.selected_resumable() {
        resumable_cwd(entry)
            .and_then(|cwd| {
                crate::resume_in_new_outer_window(&resumable_target(entry), cwd, context)
            })
            .map_err(|err| format!("Could not resume session: {err}"))
    } else if let Some(process) = app
        .selected_process_index()
        .and_then(|index| app.processes.get(index))
    {
        let name = &process.process.name;
        match double_click_action_for_process(process) {
            DoubleClickAction::Jump => crate::jump_to_outer_pane(name, context),
            DoubleClickAction::AttachInNewWindow => {
                crate::attach_in_new_outer_window(name, context)
            }
        }
        .map_err(|err| format!("Could not open process: {err}"))
    } else {
        Ok(())
    };
    if let Err(message) = result {
        app.message = Some(message);
    }
}

/// The directory a past session must be resumed from, or why it cannot be.
pub(super) fn resumable_cwd(entry: &crate::HistoryEntry) -> Result<&str> {
    match entry.cwd.as_deref() {
        Some(cwd) if entry.cwd_exists => Ok(cwd),
        Some(cwd) => bail!("its directory {cwd} no longer exists"),
        None => bail!("it has no recorded directory"),
    }
}

pub(super) fn primary_action_for_process(process: &crate::SidebarProcess) -> PrimaryAction {
    if process.outer_location.is_some() {
        PrimaryAction::Jump
    } else {
        PrimaryAction::AttachInNewSplit
    }
}

fn run_primary_action(
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> Result<()> {
    match primary_action_for_process(process) {
        PrimaryAction::Jump => crate::jump_to_outer_pane(&process.process.name, context),
        PrimaryAction::AttachInNewSplit => {
            crate::attach_in_new_outer_split(&process.process.name, context)
        }
    }
}

/// The arrow pointing back into the sidebar dismisses its context menu.
pub(super) fn closes_context_menu(key: KeyCode, sidebar_on_right: bool) -> bool {
    matches!(
        (key, sidebar_on_right),
        (KeyCode::Left, false) | (KeyCode::Right, true)
    )
}

pub(super) fn opens_context_menu_on_left_click(was_selected: bool) -> bool {
    was_selected
}

pub(super) fn sidebar_header_height(pane_label_visible: bool) -> u16 {
    if pane_label_visible { 3 } else { 4 }
}

/// Check pmux's tmux server for the foreign-config flag every couple of
/// seconds on a background thread, so a slow tmux never stalls drawing.
fn watch_pmux_config_changed(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("pmux-config-watch".to_string())
        .spawn(move || {
            loop {
                let changed = crate::tmux::tmux_cmd()
                    .args(["show-options", "-gqv", "@pmux_foreign_config"])
                    .output()
                    .is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1");
                flag.store(changed, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;
    use crossterm::event::KeyCode;
    use ratatui::layout::Rect;

    #[test]
    fn pointer_side_edges_are_the_first_and_last_columns() {
        let area = Rect::new(0, 0, 42, 30);
        assert!(pointer_at_side_edge(area, 0));
        assert!(pointer_at_side_edge(area, 41));
        assert!(!pointer_at_side_edge(area, 1));
        assert!(!pointer_at_side_edge(area, 40));
    }

    #[test]
    fn viewport_reserves_space_for_more_indicators() {
        assert_eq!(list_content_height(2), 3);
        assert_eq!(list_content_height(8), 6);
        assert_eq!(page_step(8), 2);
        assert_eq!(half_visible_process_count(24), 12);
        assert_eq!(half_visible_process_count(6), 3);
    }

    #[test]
    fn sidebar_header_uses_pane_label_without_duplication() {
        assert_eq!(sidebar_header_height(false), 4);
        assert_eq!(sidebar_header_height(true), 3);
    }

    #[test]
    fn outward_arrow_closes_the_context_menu() {
        assert!(closes_context_menu(KeyCode::Left, false));
        assert!(!closes_context_menu(KeyCode::Right, false));
        assert!(closes_context_menu(KeyCode::Right, true));
        assert!(!closes_context_menu(KeyCode::Left, true));
    }

    #[test]
    fn sidebar_second_left_click_opens_the_context_menu() {
        assert!(!opens_context_menu_on_left_click(false));
        assert!(opens_context_menu_on_left_click(true));
    }

    #[test]
    fn sidebar_resize_handle_is_inset_from_the_top_right() {
        let area = Rect::new(0, 0, 42, 30);
        // Handle glyph sits one column in from the right edge (columns 38-40),
        // leaving column 41 as a bare in-pane cell.
        let handle = sidebar_resize_handle_rect(area);
        assert_eq!(handle, Rect::new(38, 0, 3, 1));
        // The gap cell is NOT part of the handle, so hovering it drops the
        // highlight, but it is still within the resize hotspot (draggable edge).
        assert!(!handle.contains((41, 0).into()));
        assert!(sidebar_resize_hotspot(area, 41, 0, false));
        assert!(sidebar_resize_hotspot(area, 39, 0, false));
        assert!(!sidebar_resize_hotspot(area, 37, 0, false));
    }

    #[test]
    fn sidebar_resize_edge_has_a_two_column_safety_zone() {
        let area = Rect::new(0, 0, 42, 30);
        assert!(sidebar_resize_hotspot(area, 40, 12, false));
        assert!(sidebar_resize_hotspot(area, 41, 12, false));
        assert!(!sidebar_resize_hotspot(area, 39, 12, false));
        assert!(sidebar_resize_hotspot(area, 0, 12, true));
        assert!(sidebar_resize_hotspot(area, 1, 12, true));
        assert!(!sidebar_resize_hotspot(area, 2, 12, true));
    }

    #[test]
    fn process_mouse_rows_select_content_but_not_indicators_or_spacing() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![
            test_process(Some("/one"), Some("/one")),
            test_process(Some("/two"), Some("/two")),
        ];
        let area = Rect::new(0, 4, 42, 12);

        assert_eq!(process_at_mouse_row(&app, area, 4), None);
        assert_eq!(process_at_mouse_row(&app, area, 5), Some(0));
        assert_eq!(process_at_mouse_row(&app, area, 6), Some(0));
        assert_eq!(process_at_mouse_row(&app, area, 7), Some(0));
        assert_eq!(process_at_mouse_row(&app, area, 8), None);
        assert_eq!(process_at_mouse_row(&app, area, 9), Some(1));
    }
}
