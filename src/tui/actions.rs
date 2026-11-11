//! The per-process actions TUI (menu and kill confirmation).

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuAction {
    Jump,
    Attach,
    AttachInNewWindow,
    Rename,
    Kill,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionMode {
    Menu,
    Rename,
    ConfirmKill,
}

#[derive(Debug)]
struct ActionApp {
    process: crate::SidebarProcess,
    context: crate::SidebarContext,
    mode: ActionMode,
    pub(super) selected: usize,
    rename: Prompt,
    kill: Prompt,
    pub(super) message: Option<String>,
    opened_at: Instant,
    opened_with_mouse: bool,
    cursor_visible: bool,
}

/// Run the context-menu popup opened from the sidebar.
pub fn run_session_actions(
    socket: &str,
    host_pane: &str,
    session_name: &str,
    mode: Option<&str>,
    mouse_opened: bool,
) -> Result<()> {
    let context = crate::sidebar_context(Some(socket), Some(host_pane))?;
    let snapshot = crate::state()?;
    let process = crate::sidebar_processes(&snapshot, &context)
        .into_iter()
        .find(|entry| entry.process.name == session_name)
        .ok_or_else(|| anyhow!("pmux process '{session_name}' no longer exists"))?;
    let mode = action_mode_from_name(mode)?;
    render::with_terminal_opts(render::MouseMode::Full, |terminal| {
        let mut app = ActionApp {
            process,
            context,
            mode,
            selected: 0,
            rename: rename_prompt(session_name),
            kill: kill_prompt(session_name),
            message: None,
            opened_at: Instant::now(),
            opened_with_mouse: mouse_opened,
            cursor_visible: !mouse_opened,
        };
        run_actions_loop(terminal, &mut app)
    })
}

/// Execute one action chosen from the native tmux context menu.
pub fn run_sidebar_action(
    socket: &str,
    host_pane: &str,
    sidebar_pane: &str,
    session_name: &str,
    action: &str,
    value: Option<&str>,
    kind: Option<&str>,
) -> Result<()> {
    let host_pane = &crate::resolve_sidebar_host(socket, host_pane, Some(sidebar_pane));
    match action {
        "rename" | "kill" => {
            open_session_action_form(socket, host_pane, sidebar_pane, session_name, action)
        }
        "jump" | "attach" | "attach-window" => {
            let context = crate::sidebar_context(Some(socket), Some(host_pane))?;
            let snapshot = crate::state()?;
            let process = crate::sidebar_processes(&snapshot, &context)
                .into_iter()
                .find(|entry| entry.process.name == session_name)
                .ok_or_else(|| anyhow!("pmux process '{session_name}' no longer exists"))?;
            match action {
                "jump" => crate::jump_to_outer_pane(&process.process.name, &context),
                "attach" => crate::attach_in_new_outer_split(&process.process.name, &context),
                "attach-window" => {
                    crate::attach_in_new_outer_window(&process.process.name, &context)
                }
                _ => unreachable!(),
            }
        }
        "group-create" => {
            crate::create_manual_group(
                value.context("missing new group name")?,
                Some(session_name),
            )?;
            Ok(())
        }
        "group-add" => crate::add_process_to_manual_group(
            value.context("missing manual group id")?,
            session_name,
        ),
        "group-dissolve" => crate::dissolve_manual_group(session_name),
        "group-up" | "group-down" => {
            let delta = if action == "group-up" { -1 } else { 1 };
            match kind.unwrap_or("manual") {
                "manual" => crate::reorder_manual_group(session_name, delta),
                automatic if automatic.starts_with("auto:") => {
                    let mode = match automatic.trim_start_matches("auto:") {
                        "window" => crate::AutomaticGrouping::Window,
                        "cwd" => crate::AutomaticGrouping::Cwd,
                        "agent" => crate::AutomaticGrouping::Agent,
                        _ => crate::AutomaticGrouping::Ungrouped,
                    };
                    crate::reorder_automatic_group(mode, session_name, delta)
                }
                other => bail!("unknown sidebar group kind '{other}'"),
            }
        }
        "group-attach-window" | "group-kill" => {
            let names: Vec<String> =
                serde_json::from_str(value.context("missing group process list")?)
                    .context("invalid group process list")?;
            if action == "group-attach-window" {
                let context = crate::sidebar_context(Some(socket), Some(host_pane))?;
                crate::attach_group_in_new_outer_window(&names, &context)
            } else {
                for name in names {
                    crate::kill(&name)?;
                }
                Ok(())
            }
        }
        "resume" | "resume-window" => {
            // `session_name` is the `agent:id` target; `pmux resume` itself
            // re-checks that the session is not already running.
            let context = crate::sidebar_context(Some(socket), Some(host_pane))?;
            let cwd = value.context("missing session directory")?;
            if action == "resume" {
                crate::resume_in_new_outer_split(session_name, cwd, &context)
            } else {
                crate::resume_in_new_outer_window(session_name, cwd, &context)
            }
        }
        "cancel" => Ok(()),
        _ => bail!("unknown pmux sidebar action '{action}'"),
    }
}

fn action_mode_from_name(mode: Option<&str>) -> Result<ActionMode> {
    match mode {
        None | Some("menu") => Ok(ActionMode::Menu),
        Some("rename") => Ok(ActionMode::Rename),
        Some("kill") => Ok(ActionMode::ConfirmKill),
        Some(other) => bail!("unknown pmux sidebar action form '{other}'"),
    }
}

fn open_session_action_form(
    socket: &str,
    host_pane: &str,
    sidebar_pane: &str,
    session_name: &str,
    mode: &str,
) -> Result<()> {
    let binary = crate::resolve_pmux_binary()?;
    let command = shell_join(&[
        binary.display().to_string(),
        "session-actions".to_string(),
        session_name.to_string(),
        "--socket".to_string(),
        socket.to_string(),
        "--host-pane".to_string(),
        host_pane.to_string(),
        "--mode".to_string(),
        mode.to_string(),
    ])?;
    let (width, height) = (PROMPT_POPUP_WIDTH, PROMPT_POPUP_HEIGHT);
    let mut args = vec![
        "-S",
        socket,
        "display-popup",
        // The prompt uses tmux's visible popup border as its one dialog frame.
        "-E",
        "-t",
        sidebar_pane,
        "-w",
        width,
        "-h",
        height,
        &command,
    ];
    args.retain(|argument| !argument.is_empty());
    let status = crate::tmux::tmux_command()
        .args(args)
        .status()
        .context("open pmux process action form")?;
    if status.success() {
        Ok(())
    } else {
        bail!("outer tmux could not open the process action form")
    }
}

fn attached_in_current_window(
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> bool {
    process
        .outer_location
        .as_ref()
        .is_some_and(|location| location.window_id == context.host_window_id)
}

fn actions_for_process(
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> Vec<MenuAction> {
    let mut actions = Vec::new();
    if process.outer_location.is_some() {
        actions.push(MenuAction::Jump);
    }
    if !attached_in_current_window(process, context) {
        actions.push(MenuAction::Attach);
    }
    // Opening the process in a fresh window is always available, even for a
    // process already attached somewhere else.
    actions.push(MenuAction::AttachInNewWindow);
    actions.extend([MenuAction::Rename, MenuAction::Kill, MenuAction::Cancel]);
    actions
}

fn actions_for(app: &ActionApp) -> Vec<MenuAction> {
    actions_for_process(&app.process, &app.context)
}

fn action_labels(process: &crate::SidebarProcess, context: &crate::SidebarContext) -> Vec<String> {
    actions_for_process(process, context)
        .into_iter()
        .map(|action| action_label(action, process, context))
        .collect()
}

/// Native menus have no per-item layout primitives, so pad the text labels to
/// one column and put each action glyph in a shared right-hand column.
pub(super) fn native_action_menu_items(
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> Vec<(MenuAction, String)> {
    let actions = actions_for_process(process, context);
    let labels: Vec<_> = actions
        .iter()
        .map(|action| action_label(*action, process, context))
        .collect();
    let widest_label = labels
        .iter()
        .map(|label| text_width(label))
        .max()
        .unwrap_or(0);

    actions
        .into_iter()
        .zip(labels)
        .map(|(action, label)| {
            let padding = " ".repeat(widest_label.saturating_sub(text_width(&label)));
            let label = match action_glyph(action, process, context) {
                Some(glyph) => format!("{label}{padding} {glyph}"),
                None => label,
            };
            (action, label)
        })
        .collect()
}

fn action_label(
    action: MenuAction,
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> String {
    match action {
        MenuAction::Jump => match process.outer_location.as_ref() {
            Some(location) if location.window_id == context.host_window_id => {
                format!("Jump to pane {}", location.pane_index)
            }
            Some(location) => format!("Jump to window {}", location.window_index),
            None => "Jump to attached process".to_string(),
        },
        MenuAction::Attach => "Attach beside rightmost pane".to_string(),
        MenuAction::AttachInNewWindow => "Attach in new window".to_string(),
        MenuAction::Rename => "Rename session".to_string(),
        MenuAction::Kill => "Kill session…".to_string(),
        MenuAction::Cancel => "Cancel".to_string(),
    }
}

fn action_glyph(
    action: MenuAction,
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
) -> Option<&'static str> {
    match action {
        MenuAction::Jump => match attachment_scope(process, context) {
            AttachmentScope::CurrentWindow => {
                if context.sidebar_on_right {
                    Some("←")
                } else {
                    Some("→")
                }
            }
            AttachmentScope::OtherWindow => {
                if context.sidebar_on_right {
                    Some("↖")
                } else {
                    Some("↗")
                }
            }
            AttachmentScope::OtherSession => Some("⧉"),
            AttachmentScope::Unattached => None,
        },
        MenuAction::Attach => Some("⇥"),
        MenuAction::AttachInNewWindow => Some("❐"),
        MenuAction::Rename => Some("✎"),
        MenuAction::Kill => Some("×"),
        MenuAction::Cancel => None,
    }
}

/// tmux menus only handle Enter, Up, Down, and q themselves. Give the
/// cancellation row the outward-arrow shortcut so it returns to the sidebar
/// with no side effect.
pub(super) fn native_menu_shortcut(action: MenuAction, sidebar_on_right: bool) -> &'static str {
    if action == MenuAction::Cancel {
        if sidebar_on_right { "Right" } else { "Left" }
    } else {
        ""
    }
}

/// The `sidebar-action` name for a native process menu item.
pub(super) fn native_menu_action_name(action: MenuAction) -> &'static str {
    match action {
        MenuAction::Jump => "jump",
        MenuAction::Attach => "attach",
        MenuAction::AttachInNewWindow => "attach-window",
        MenuAction::Rename => "rename",
        MenuAction::Kill => "kill",
        MenuAction::Cancel => "cancel",
    }
}

fn run_actions_loop(terminal: &mut AppTerminal, app: &mut ActionApp) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_actions(frame, app))?;
        if !event::poll(EVENT_POLL)? {
            continue;
        }
        match app.mode {
            ActionMode::Menu => match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let actions = actions_for(app);
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                        key if closes_context_menu(key, app.context.sidebar_on_right) => {
                            return Ok(());
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.cursor_visible = true;
                            app.selected = app.selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.cursor_visible = true;
                            app.selected = (app.selected + 1).min(actions.len().saturating_sub(1));
                        }
                        KeyCode::Enter if ignore_action_launch_enter(app.opened_at, key.code) => {}
                        KeyCode::Enter => {
                            app.cursor_visible = true;
                            if run_menu_action(app, actions[app.selected])? {
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if ignore_action_launch_mouse(app) {
                        continue;
                    }
                    let actions = actions_for(app);
                    if let Some(index) = action_at_mouse_row(mouse.row, actions.len()) {
                        app.selected = index;
                        if run_menu_action(app, actions[index])? {
                            return Ok(());
                        }
                    } else {
                        // Treat a click outside an action row as a dismissal.
                        // This covers popup padding and border clicks delivered
                        // to the action TUI itself.
                        return Ok(());
                    }
                }
                _ => {}
            },
            ActionMode::Rename => {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.rename.handle_key(key) {
                    PromptEvent::Stay => {}
                    PromptEvent::Cancel => return Ok(()),
                    PromptEvent::Submit => {
                        match submit_rename(&app.process.process.name, app.rename.value()) {
                            Ok(()) => return Ok(()),
                            Err(message) => app.rename.fail(message),
                        }
                    }
                }
            }
            ActionMode::ConfirmKill => {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.kill.handle_key(key) {
                    PromptEvent::Stay => {}
                    PromptEvent::Cancel => return Ok(()),
                    PromptEvent::Submit => {
                        crate::kill(&app.process.process.name)?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn action_at_mouse_row(row: u16, action_count: usize) -> Option<usize> {
    row.checked_sub(1)
        .map(usize::from)
        .filter(|index| *index < action_count)
}

/// Returns true when the action should close the menu.
fn run_menu_action(app: &mut ActionApp, action: MenuAction) -> Result<bool> {
    match action {
        MenuAction::Jump => {
            crate::jump_to_outer_pane(&app.process.process.name, &app.context)?;
            Ok(true)
        }
        MenuAction::Attach => {
            crate::attach_in_new_outer_split(&app.process.process.name, &app.context)?;
            Ok(true)
        }
        MenuAction::AttachInNewWindow => {
            crate::attach_in_new_outer_window(&app.process.process.name, &app.context)?;
            Ok(true)
        }
        MenuAction::Rename => {
            app.mode = ActionMode::Rename;
            app.message = None;
            Ok(false)
        }
        MenuAction::Kill => {
            app.mode = ActionMode::ConfirmKill;
            app.kill = kill_prompt(&app.process.process.name);
            app.message = None;
            Ok(false)
        }
        MenuAction::Cancel => Ok(true),
    }
}

fn ignore_action_launch_enter(opened_at: Instant, key: KeyCode) -> bool {
    key == KeyCode::Enter && opened_at.elapsed() < ACTION_LAUNCH_GUARD
}

fn ignore_action_launch_mouse(app: &ActionApp) -> bool {
    app.opened_with_mouse && app.opened_at.elapsed() < ACTION_LAUNCH_GUARD
}

fn draw_actions(frame: &mut ratatui::Frame, app: &ActionApp) {
    match app.mode {
        ActionMode::Menu => draw_action_menu(frame, app),
        ActionMode::Rename => app.rename.draw(frame),
        ActionMode::ConfirmKill => app.kill.draw(frame),
    }
}

fn draw_action_menu(frame: &mut ratatui::Frame, app: &ActionApp) {
    let area = frame.area();
    let labels = action_labels(&app.process, &app.context);
    let mut lines = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let selected = app.cursor_visible && index == app.selected;
        let marker = if selected { CURSOR_BAR } else { " " };
        let style = if selected {
            Style::default()
                .bg(Color::Rgb(35, 53, 78))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {label}{ACTION_MENU_TRAILING_PADDING}"),
            style,
        )));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn kill_prompt(session_name: &str) -> Prompt {
    let title = crate::display_session_name(session_name)
        .map(|name| format!("Kill [{name}]?"))
        .unwrap_or_else(|| "Kill process?".to_string());
    Prompt::confirm(
        title,
        "Kill",
        "This terminates the managed process immediately.",
    )
    .danger()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;
    use crossterm::event::KeyCode;

    use std::time::{Duration, Instant};

    #[test]
    fn action_menu_mouse_rows_exclude_its_border() {
        assert_eq!(action_at_mouse_row(0, 4), None);
        assert_eq!(action_at_mouse_row(1, 4), Some(0));
        assert_eq!(action_at_mouse_row(4, 4), Some(3));
        assert_eq!(action_at_mouse_row(5, 4), None);
    }

    #[test]
    fn native_menu_shortcuts_map_the_outward_arrow_to_cancel() {
        assert_eq!(native_menu_shortcut(MenuAction::Jump, false), "");
        assert_eq!(native_menu_shortcut(MenuAction::Cancel, false), "Left");
        assert_eq!(native_menu_shortcut(MenuAction::Cancel, true), "Right");
    }

    #[test]
    fn current_window_attachment_is_not_offered_again() {
        let mut process = test_process(Some("/work"), Some("/work"));
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        let context = test_context();
        // The in-window split is not offered again, but "attach in new window"
        // is always available.
        let actions = actions_for_process(&process, &context);
        assert_eq!(
            actions,
            vec![
                MenuAction::Jump,
                MenuAction::AttachInNewWindow,
                MenuAction::Rename,
                MenuAction::Kill,
                MenuAction::Cancel,
            ]
        );
        assert!(!actions.contains(&MenuAction::Attach));
    }

    #[test]
    fn attach_menu_action_describes_the_rightmost_pane_split() {
        let process = test_process(Some("/work"), Some("/work"));
        assert!(
            action_labels(&process, &test_context())
                .iter()
                .any(|label| label == "Attach beside rightmost pane")
        );
    }

    #[test]
    fn attach_in_new_window_is_offered_for_every_process() {
        let context = test_context();
        // Unattached process: new-window action present.
        let unattached = test_process(Some("/work"), Some("/work"));
        assert!(
            actions_for_process(&unattached, &context).contains(&MenuAction::AttachInNewWindow)
        );
        assert!(
            action_labels(&unattached, &context)
                .iter()
                .any(|label| label == "Attach in new window")
        );
        // Process already attached in the current window: still offered.
        let mut attached_here = test_process(Some("/work"), Some("/work"));
        attached_here.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        assert!(
            actions_for_process(&attached_here, &context).contains(&MenuAction::AttachInNewWindow)
        );
    }

    #[test]
    fn native_action_menu_glyphs_describe_each_action() {
        let mut process = test_process(Some("/work"), Some("/work"));
        let mut context = test_context();
        process.outer_location = Some(crate::OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "work".to_string(),
        });
        let items = native_action_menu_items(&process, &context);
        assert!(items.iter().any(|(_, label)| label.ends_with('→')));
        assert!(items.iter().any(|(_, label)| label.ends_with('✎')));
        assert!(items.iter().any(|(_, label)| label.ends_with('×')));
        assert_eq!(action_glyph(MenuAction::Cancel, &process, &context), None);

        context.host_window_id = "@2".to_string();
        assert_eq!(
            action_glyph(MenuAction::Jump, &process, &context),
            Some("↗")
        );
    }

    #[test]
    fn action_popup_ignores_only_the_enter_that_opened_it() {
        assert!(ignore_action_launch_enter(Instant::now(), KeyCode::Enter));
        assert!(!ignore_action_launch_enter(Instant::now(), KeyCode::Down));
        assert!(!ignore_action_launch_enter(
            Instant::now() - Duration::from_secs(1),
            KeyCode::Enter
        ));
    }
}
