//! Sidebar drawing: header, process list, drawers, help, and summary lines.

use super::*;
use std::borrow::Cow;

pub(super) fn drawer_content_line(content: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(format!(" {}", content.into()), style),
    ])
}

pub(super) fn drawer_metric_color(metric: crate::DrawerMetric) -> Color {
    match metric {
        crate::DrawerMetric::Churn => Color::Green,
        // As the drawer's `in` and `out` totals are colored.
        crate::DrawerMetric::OutputTokens => Color::Yellow,
        crate::DrawerMetric::InputTokens => Color::LightMagenta,
    }
}

pub(super) fn drawer_stats_line(data: &crate::ActivityData) -> Line<'static> {
    let separator = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::styled("│", separator),
        // Match the three-cell inset used before the process row's agent/status
        // text, so drawer metrics sit on the same visual column.
        Span::raw("   "),
        // The prompt count lives on the process row's second line instead.
        Span::styled(
            format!("{} in", fmt_count(data.total_in)),
            Style::default().fg(Color::LightMagenta),
        ),
        Span::styled(" / ", separator),
        Span::styled(
            format!("{} out", fmt_count(data.total_out)),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(" · ", separator),
        Span::styled(
            format!("+{}", fmt_count(data.added)),
            Style::default().fg(Color::Green),
        ),
        Span::styled("/", separator),
        Span::styled(
            format!("-{}", fmt_count(data.deleted)),
            Style::default().fg(Color::LightRed),
        ),
    ])
}

pub(super) fn drawer_prompt_lines(data: &crate::ActivityData, width: u16) -> Vec<String> {
    let inner = usize::from(width).saturating_sub(2);
    let prompt_width = inner.saturating_sub(2).max(1);
    let prompts: Vec<&str> = if data.recent_prompts.is_empty() {
        vec![data.summary.as_deref().unwrap_or("(no user prompt)")]
    } else {
        data.recent_prompts.iter().map(String::as_str).collect()
    };

    // Allocate lines newest-first so the latest prompt always gets all the
    // room it can use. Older prompts fill only the remaining space, but the
    // completed blocks are reversed back into chronological display order.
    let mut remaining = DRAWER_PROMPT_MAX_LINES;
    let mut blocks = Vec::new();
    for prompt in prompts.iter().rev() {
        if remaining == 0 {
            break;
        }
        let mut lines = wrap_text_lines(prompt, prompt_width, remaining);
        if let Some(first) = lines.first_mut() {
            first.insert_str(0, "↑ ");
        }
        for continuation in lines.iter_mut().skip(1) {
            continuation.insert_str(0, "  ");
        }
        remaining = remaining.saturating_sub(lines.len());
        blocks.push(lines);
    }
    blocks.reverse();
    blocks.into_iter().flatten().collect()
}

/// Word-wrap terminal text to at most `max_lines`. The final line receives an
/// ellipsis when more content remains, while earlier lines wrap without losing
/// text. Long unbroken words are split at terminal-column boundaries.
pub(super) fn wrap_text_lines(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    if width == 0 {
        return vec![String::new()];
    }

    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::with_capacity(max_lines);
    let mut remaining = normalized.as_str();
    while !remaining.is_empty() && lines.len() < max_lines {
        if text_width(remaining) <= width {
            lines.push(remaining.to_string());
            break;
        }
        if lines.len() + 1 == max_lines {
            lines.push(crate::truncate_col(remaining, width));
            break;
        }

        let mut used = 0;
        let mut boundary = 0;
        let mut last_space = None;
        for (index, ch) in remaining.char_indices() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + char_width > width {
                break;
            }
            used += char_width;
            boundary = index + ch.len_utf8();
            if ch.is_whitespace() {
                last_space = Some(index);
            }
        }

        if boundary == 0 {
            // A single wide glyph cannot fit (only possible at extremely small
            // widths); consume it so wrapping always makes progress.
            boundary = remaining
                .char_indices()
                .nth(1)
                .map_or(remaining.len(), |(index, _)| index);
        }
        let split = last_space.filter(|&index| index > 0).unwrap_or(boundary);
        lines.push(remaining[..split].trim_end().to_string());
        remaining = remaining[split..].trim_start();
    }
    lines
}

pub(super) fn drawer_bottom_line(width: u16) -> Line<'static> {
    let width = usize::from(width);
    let border = if width == 0 {
        String::new()
    } else {
        format!("└{}", "─".repeat(width.saturating_sub(1)))
    };
    Line::from(Span::styled(border, Style::default().fg(Color::DarkGray)))
}

/// The selected entry's first line, relative to the sidebar pane. The process
/// list always reserves its first row for the top `more` indicator, keeping
/// the menu aligned with the row the user selected whether or not it is shown.
pub(super) fn selected_process_row(app: &SidebarApp, pane_label_visible: bool) -> u16 {
    let header_height = sidebar_header_height(pane_label_visible);
    let list_indicator_height = 1;
    let viewport_line = app.selected_viewport_line().min(usize::from(u16::MAX)) as u16;
    header_height
        .saturating_add(list_indicator_height)
        .saturating_add(viewport_line)
}

/// Index one past the last process that fits from `start`, ignoring any open
/// drawer. The live sidebar uses the drawer-aware [`SidebarApp::visible_end`];
/// this free form (no `self`) backs the viewport unit tests.
#[cfg(test)]
pub(super) fn visible_end(
    processes: &[crate::SidebarProcess],
    start: usize,
    list_height: u16,
) -> usize {
    render::viewport_end(processes.len(), start, list_height, |index| {
        process_line_count(&processes[index], false)
    })
}

pub(super) fn draw_sidebar(
    frame: &mut ratatui::Frame,
    app: &mut SidebarApp,
    context: &crate::SidebarContext,
    pane_label_visible: bool,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(sidebar_header_height(pane_label_visible)),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
    let mut header_lines = Vec::new();
    if !pane_label_visible {
        header_lines.push(Line::from(Span::styled(
            " pmux processes",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    }
    let now = Instant::now();
    // On the handle's row, the resumable count must also leave the handle
    // and a cell before it clear.
    let whole_room = if pane_label_visible {
        usize::from(sidebar_resize_handle_rect(area).x.saturating_sub(area.x)).saturating_sub(1)
    } else {
        usize::from(chunks[0].width)
    };
    header_lines.push(sidebar_counts_line(app, chunks[0].width, whole_room));
    header_lines.push(sidebar_summary_line(app, chunks[0].width, now));
    let header = Paragraph::new(header_lines).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);
    // White while hovered or actively dragging; cyan at rest.
    let handle_color = if app.handle_hover_seen.is_some() || app.resize_drag.is_some() {
        Color::White
    } else {
        DRAG_HANDLE_COLOR
    };
    let handle = Paragraph::new(Line::from(Span::styled(
        SIDEBAR_RESIZE_HANDLE_TEXT,
        Style::default()
            .fg(handle_color)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Right);
    frame.render_widget(handle, sidebar_resize_handle_rect(area));

    app.update_drawer_width(chunks[1].width, chunks[1].height);
    draw_process_list(frame, chunks[1], app, context);
    let (footer_text, footer_style) = sidebar_footer(app);
    let footer = Paragraph::new(Line::from(Span::styled(
        crate::truncate_col(&footer_text, chunks[2].width.saturating_sub(1) as usize),
        footer_style,
    )))
    .alignment(Alignment::Left);
    frame.render_widget(footer, chunks[2]);

    if app.help_open {
        draw_sidebar_help(frame, app, context.sidebar_on_right);
    }
}

/// The search line drawn in the list's top row: yellow while the query is
/// being typed, cyan while it filters the list, `None` without a query.
pub(super) fn sidebar_search_line(app: &SidebarApp) -> Option<(String, Style)> {
    if app.search.editing {
        Some((
            format!(" /{}▏ {}", app.search.query, search_match_text(app)),
            Style::default().fg(Color::Yellow),
        ))
    } else if app.search.active() {
        Some((
            format!(
                " /{} {} • esc:clear",
                app.search.query,
                search_match_text(app)
            ),
            Style::default().fg(Color::Cyan),
        ))
    } else {
        None
    }
}

/// The search line fitted to `width` columns, with `↑ N` at its right end
/// when `hidden_above` rows are scrolled out of view above it.
pub(super) fn fitted_search_line(text: &str, hidden_above: usize, width: usize) -> String {
    if hidden_above == 0 {
        return crate::truncate_col(text, width.saturating_sub(1));
    }
    let more = format!("↑ {hidden_above} ");
    let room = width.saturating_sub(text_width(&more) + 1);
    let text = crate::truncate_col(text, room);
    let pad = width.saturating_sub(text_width(&text) + text_width(&more));
    format!("{text}{}{more}", " ".repeat(pad))
}

/// The footer's text and style: an error message wins, then a changed pmux
/// config, then the live-feed reconnect status, then the usual toggle hint.
pub(super) fn sidebar_footer(app: &SidebarApp) -> (Cow<'_, str>, Style) {
    if let Some(message) = app.message.as_deref() {
        (message.into(), Style::default().fg(Color::Red))
    } else if app
        .pmux_config_changed
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        (
            SIDEBAR_CONFIG_CHANGED_STATUS.into(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.event_stream_down {
        (
            SIDEBAR_RECONNECTING_STATUS.into(),
            Style::default().fg(Color::Yellow),
        )
    } else {
        (
            app.toggle_hint.as_str().into(),
            Style::default().fg(Color::DarkGray),
        )
    }
}

/// `· 3 matches` for the search line, or nothing before a query.
fn search_match_text(app: &SidebarApp) -> String {
    match (app.search.active(), app.search.matches) {
        (false, _) => String::new(),
        (true, 0) => "· no matches".to_string(),
        (true, 1) => "· 1 match".to_string(),
        (true, count) => format!("· {count} matches"),
    }
}

/// The widest keys the keymap help lines its descriptions up after.
const HELP_KEY_COLUMN_MAX: usize = 12;

/// One row of the keymap help: the keys, then what they do.
type HelpEntry = (String, &'static str);

/// The keymap help's sections, with each action's keys read from the keymap
/// so a rebound key shows up as bound. Actions left without a key are
/// skipped.
fn sidebar_help_sections(
    keymap: &SidebarKeymap,
    sidebar_on_right: bool,
) -> Vec<(&'static str, Vec<HelpEntry>)> {
    use SidebarAction::*;
    let key = |action| keymap.label(action, sidebar_on_right);
    // Two actions on one row, such as up and down.
    let pair = |first, second| match (key(first), key(second)) {
        (Some(first), Some(second)) => Some(format!("{first} · {second}")),
        (first, second) => first.or(second),
    };
    let with = |keys: Option<String>, extra: &str| {
        Some(keys.map_or_else(|| extra.to_string(), |keys| format!("{keys}/{extra}")))
    };
    let section = |title, entries: Vec<(Option<String>, &'static str)>| {
        let entries: Vec<HelpEntry> = entries
            .into_iter()
            .filter_map(|(keys, description)| Some((keys?, description)))
            .collect();
        (title, entries)
    };
    vec![
        section(
            "Navigation",
            vec![
                (pair(MoveUp, MoveDown), "move selection"),
                (pair(HalfPageUp, HalfPageDown), "move half a visible page"),
                (pair(First, Last), "first / last item"),
            ],
        ),
        section(
            "Actions",
            vec![
                (key(Open), "jump to it, or open/resume it beside"),
                (key(Toggle), "open/close its drawer, or fold a group"),
                (with(key(Menu), "click×2"), "actions menu"),
                (key(Search), "filter the list; Esc clears it"),
            ],
        ),
        section(
            "Grouping",
            vec![
                (key(CycleGrouping), "cycle ungrouped / window / CWD / agent"),
                (key(FoldSection), "fold or unfold the current section"),
            ],
        ),
        section(
            "Drawer graph",
            vec![
                (
                    with(key(Toggle), "click ⣠⣾⣦"),
                    "toggle every process drawer",
                ),
                (key(CycleMetric), "cycle graph metric"),
                (key(CycleScale), "cycle timeline scale"),
            ],
        ),
        section(
            "Mouse & sidebar",
            vec![
                (
                    Some("Double-click".to_string()),
                    "jump to it, or open/resume it in a new window",
                ),
                (Some("Wheel".to_string()), "scroll list"),
                (Some("Drag edge".to_string()), "resize sidebar"),
                (key(Close), "close help or sidebar"),
                (key(Help), "toggle this help"),
            ],
        ),
    ]
}

/// The keymap help laid out for `width` columns: a blank row between
/// sections, and descriptions that wrap under their own column rather than
/// running off the edge. A key too wide for the key column gets a row of its
/// own, with its description below.
fn sidebar_help_lines(
    sections: &[(&'static str, Vec<HelpEntry>)],
    width: usize,
) -> Vec<Line<'static>> {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let description_style = Style::default().fg(Color::Gray);
    // Sized for the usual short keys; the odd long one (`Enter/Tab on ↻`)
    // takes a row of its own rather than squeezing every description.
    let widest_key = sections
        .iter()
        .flat_map(|(_, entries)| entries)
        .map(|(keys, _)| text_width(keys))
        .filter(|&width| width <= HELP_KEY_COLUMN_MAX)
        .max()
        .unwrap_or(0);
    // The keys and a one-column gap, after one column of indent.
    let key_column = (widest_key + 1).min(width.saturating_sub(1) / 2).max(1);
    let description_width = width.saturating_sub(1 + key_column).max(1);
    let indent = " ".repeat(1 + key_column);

    let mut lines = Vec::new();
    for (index, (title, entries)) in sections.iter().enumerate() {
        if entries.is_empty() {
            continue;
        }
        if index > 0 && !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        for (keys, description) in entries {
            // Never more rows than characters.
            let wrapped = wrap_text_lines(description, description_width, description.len());
            let mut wrapped = wrapped.into_iter();
            if text_width(keys) < key_column {
                let padding = key_column - text_width(keys);
                lines.push(Line::from(vec![
                    Span::styled(format!(" {keys}{}", " ".repeat(padding)), key_style),
                    Span::styled(wrapped.next().unwrap_or_default(), description_style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    crate::truncate_col(&format!(" {keys}"), width),
                    key_style,
                )));
            }
            lines.extend(wrapped.map(|text| {
                Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(text, description_style),
                ])
            }));
        }
    }
    lines
}

/// Draw the keymap help over the list. It is as tall as its content where
/// that fits, and scrolls inside the frame where it does not (a small
/// window), with the border saying which way there is more.
fn draw_sidebar_help(frame: &mut ratatui::Frame, app: &mut SidebarApp, sidebar_on_right: bool) {
    let sections = sidebar_help_sections(&app.keymap, sidebar_on_right);
    let width = modal_area(frame.area(), 96, 1).width;
    let lines = sidebar_help_lines(&sections, usize::from(width.saturating_sub(2)));
    let wanted = u16::try_from(lines.len().saturating_add(2)).unwrap_or(u16::MAX);
    let area = modal_area(frame.area(), 96, wanted);
    let page = area.height.saturating_sub(2);
    let max_scroll = lines.len().saturating_sub(usize::from(page));
    app.help_scroll = app.help_scroll.min(max_scroll);
    app.help_page = page;
    let more = match (app.help_scroll > 0, app.help_scroll < max_scroll) {
        (true, true) => " ↑↓ more ",
        (true, false) => " ↑ more ",
        (false, true) => " ↓ more ",
        (false, false) => "",
    };
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(app.help_scroll)
        .take(usize::from(page))
        .collect();
    let mut block = Block::default()
        .title(" Keymap ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    if !more.is_empty() {
        block = block.title_bottom(Line::from(more).alignment(Alignment::Right));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(visible).block(block), area);
}

/// Read the outer tmux prefix key (e.g. `C-s`) so the footer hint can show the
/// user's actual binding instead of a generic "prefix".
fn outer_prefix_key(socket: &str) -> Option<String> {
    let output = crate::tmux::tmux_command()
        .args(["-S", socket, "show-options", "-gv", "prefix"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "none" {
        None
    } else {
        Some(value)
    }
}

/// Render a tmux key name in the compact caret form used in the hint bar:
/// `C-s` becomes `^s`, other keys are left as-is.
fn format_tmux_key(key: &str) -> String {
    let key = key.trim();
    match key.strip_prefix("C-") {
        Some(rest) => format!("^{rest}"),
        None => key.to_string(),
    }
}

pub(super) fn sidebar_toggle_hint(context: &crate::SidebarContext) -> String {
    sidebar_toggle_hint_text(outer_prefix_key(&context.socket).as_deref())
}

/// Build the footer hint, e.g. `^s+^p:open/close • q:close • ?:help`. When the outer
/// prefix cannot be read it falls back to the literal word `prefix`.
pub(super) fn sidebar_toggle_hint_text(prefix_key: Option<&str>) -> String {
    let prefix = prefix_key.map_or_else(|| "prefix".to_string(), format_tmux_key);
    format!(" {prefix}+^p:open/close • q:close • ?:help")
}

/// The header's ` 3 attached` and `, 1 detached` and their colors: the
/// detached count is as dim as a detached process's outline.
fn sidebar_count_parts(app: &SidebarApp) -> [(String, Color); 2] {
    let attached = app
        .processes
        .iter()
        .filter(|process| process.process.attached_clients > 0)
        .count();
    let detached = app.processes.len().saturating_sub(attached);
    [
        (format!(" {attached} attached"), Color::Gray),
        (
            format!(", {detached} detached"),
            render::MUTED_OUTLINE_COLOR,
        ),
    ]
}

/// The header's attached and detached counts, the last part to fit cut
/// short in a narrow sidebar, then the resumable count when all of it fits
/// in `whole_room` columns.
fn sidebar_counts_line(app: &SidebarApp, width: u16, whole_room: usize) -> Line<'static> {
    let mut room = usize::from(width);
    let mut spans = Vec::new();
    let mut parts = sidebar_count_parts(app).to_vec();
    let resumable = app.resumable_entry_indices().len();
    if resumable > 0 {
        let whole = format!(", {resumable} resumable");
        let used: usize = parts.iter().map(|(text, _)| text_width(text)).sum();
        if used + text_width(&whole) <= whole_room {
            parts.push((whole, render::MUTED_OUTLINE_COLOR));
        }
    }
    for (text, color) in parts {
        if room == 0 {
            break;
        }
        let text = crate::truncate_col(&text, room);
        room -= text_width(&text);
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    Line::from(spans)
}

/// The grouping mode in words, as the header shows it.
fn grouping_text(mode: crate::AutomaticGrouping) -> String {
    match mode {
        crate::AutomaticGrouping::Ungrouped => " Not grouped".to_string(),
        mode => format!(" Grouping by {}", mode.label()),
    }
}

/// What the header's second line says: for a moment after the grouping
/// mode or the graph changes, the new one; otherwise nothing.
fn header_notice_text(app: &SidebarApp, now: Instant) -> String {
    match app.header_notice(now) {
        Some(HeaderNotice::Grouping) => grouping_text(app.grouping.automatic),
        Some(HeaderNotice::Graph) => format!(" Graph: {}", app.drawer_metric.description()),
        None => String::new(),
    }
}

fn sidebar_graph_indicator(app: &SidebarApp, now: Instant) -> String {
    app.drawer_header_label(now).map_or_else(
        || DRAWER_GRAPH_INDICATOR.to_string(),
        |label| format!("{DRAWER_GRAPH_INDICATOR}{label}"),
    )
}

fn sidebar_graph_button_width(app: &SidebarApp, now: Instant) -> usize {
    // One clickable/highlighted padding cell on each side of the indicator.
    text_width(&sidebar_graph_indicator(app, now)) + 2
}

pub(super) fn sidebar_graph_button_rect(
    area: Rect,
    pane_label_visible: bool,
    app: &SidebarApp,
    now: Instant,
) -> Rect {
    let width = sidebar_graph_button_width(app, now).min(usize::from(area.width)) as u16;
    let row_offset = if pane_label_visible { 1 } else { 2 };
    Rect::new(
        area.right().saturating_sub(width),
        area.y.saturating_add(row_offset),
        width,
        1,
    )
}

pub(super) fn handle_sidebar_graph_click(
    app: &mut SidebarApp,
    area: Rect,
    pane_label_visible: bool,
    column: u16,
    row: u16,
    now: Instant,
) -> bool {
    if !sidebar_graph_button_rect(area, pane_label_visible, app, now).contains((column, row).into())
    {
        return false;
    }
    app.header_selected = true;
    app.toggle_all_drawers();
    true
}

fn sidebar_summary_line(app: &SidebarApp, width: u16, now: Instant) -> Line<'static> {
    let width = usize::from(width);
    let label = app.drawer_header_label(now);
    let indicator = sidebar_graph_indicator(app, now);
    let indicator_width = text_width(&indicator);
    let button_width = indicator_width + 2;
    let indicator_color = if app.any_drawers_open() {
        drawer_metric_color(app.drawer_metric)
    } else {
        Color::DarkGray
    };
    let indicator_style = render::content_style(
        Style::default().fg(indicator_color),
        (app.header_selected && app.sidebar_focused).then_some(render::CURSOR_HIGHLIGHT_COLOR),
    );
    if button_width >= width {
        return Line::from(Span::styled(
            crate::truncate_col(&format!(" {indicator} "), width),
            indicator_style,
        ));
    }

    let notice = crate::truncate_col(&header_notice_text(app, now), width - button_width);
    let room = width - button_width - text_width(&notice);
    let mut spans = vec![Span::styled(
        notice,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend([
        Span::raw(" ".repeat(room)),
        Span::styled(" ", indicator_style),
        Span::styled(DRAWER_GRAPH_INDICATOR, indicator_style),
        Span::styled(label.unwrap_or_default(), indicator_style),
        Span::styled(" ", indicator_style),
    ]);
    Line::from(spans)
}

fn draw_process_list(
    frame: &mut ratatui::Frame,
    area: Rect,
    app: &SidebarApp,
    context: &crate::SidebarContext,
) {
    let search_line = sidebar_search_line(app);
    if let Some((text, style)) = &search_line {
        // The search line takes the top row, which otherwise holds `↑ (N more)`.
        let text = fitted_search_line(text, app.first_visible, usize::from(area.width));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, *style))),
            Rect::new(area.x, area.y, area.width, 1.min(area.height)),
        );
        if app.rows.is_empty() {
            render::render_centered_line(
                frame,
                area,
                area.y.saturating_add(2),
                Line::from(Span::styled(
                    "No matches.",
                    Style::default().fg(Color::DarkGray),
                )),
            );
            return;
        }
    }
    if app.rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No running pmux processes.")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let pill_colors = render::assign_pill_colors(&app.processes);
    let process_refs: Vec<_> = app.processes.iter().collect();
    let tag_width = render::agent_tag_column_width(process_refs.iter().copied());
    let end = app.visible_end(app.first_visible, area.height);
    let mut y = area.y;
    if search_line.is_none() && app.first_visible > 0 && y < area.bottom() {
        render::render_centered_line(
            frame,
            area,
            y,
            Line::from(Span::styled(
                format!("↑ ({} more)", app.first_visible),
                Style::default().fg(Color::DarkGray),
            )),
        );
    }
    y = y.saturating_add(1);
    let mut lines_to_skip = app.first_visible_line;
    // `↓ (N more)` is pinned to the last line, as `↑` is to the first, so
    // space a row cannot fill stays above it and the list's bottom edge
    // never moves. Rows stop short of that line.
    let bottom_more = app.rows.len().saturating_sub(end);
    let content_bottom = if bottom_more > 0 {
        area.bottom().saturating_sub(1)
    } else {
        area.bottom()
    };

    for index in app.first_visible..end {
        if y >= content_bottom {
            break;
        }
        let selected = app.cursor_is_visible_for(index);
        let highlight = selected.then_some(if app.sidebar_focused {
            render::CURSOR_HIGHLIGHT_COLOR
        } else {
            render::UNFOCUSED_CURSOR_HIGHLIGHT_COLOR
        });
        let row_background = match &app.rows[index] {
            SidebarListRow::Group(_) => Some(group_header_background(
                app.group_header_background_color,
                selected,
            )),
            SidebarListRow::Process { .. } | SidebarListRow::Resumable { .. } => highlight,
            SidebarListRow::GroupTypeSeparator => None,
        };
        let mut rendered: Vec<Line<'static>> = match &app.rows[index] {
            SidebarListRow::Group(group) => vec![group_header_line(group, area.width)],
            SidebarListRow::Resumable { entry_index, .. } => {
                let mut lines = app
                    .history
                    .get(*entry_index)
                    .map(|entry| {
                        let prompts = app.past_prompt_label(*entry_index);
                        render::render_resumable_row(
                            entry,
                            prompts.as_deref(),
                            area.width,
                            highlight,
                            if app.sidebar_focused {
                                app.active_pane_background_color
                            } else {
                                app.inactive_pane_background_color
                            },
                        )
                    })
                    .unwrap_or_default();
                let mut drawer = app.build_past_drawer_lines(*entry_index, area.width);
                let outline = &render::row_layout().outline;
                // An open drawer continues the row's outline down to its own
                // bottom corner.
                if !drawer.is_empty()
                    && let Some(corner) = lines.last_mut().and_then(|line| line.spans.first_mut())
                {
                    corner.content = format!("{} ", outline.side).into();
                }
                for line in &mut drawer {
                    render::outline_drawer_line(line, outline, false, 0);
                }
                lines.extend(drawer);
                lines
            }
            SidebarListRow::GroupTypeSeparator => vec![Line::from(vec![
                Span::styled(" ── ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "manual groups",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    fill_to_width("─", usize::from(area.width).saturating_sub(19)),
                    Style::default().fg(Color::DarkGray),
                ),
            ])],
            SidebarListRow::Process {
                process_index,
                tree,
                parent_note,
                ..
            } => {
                let process = &app.processes[*process_index];
                // A child is drawn narrower, behind the tree joining it to
                // its parent.
                let width = area.width.saturating_sub(tree.width() as u16);
                let drawer_height = app.drawer_line_count_indented(*process_index, tree.width());
                let focused_process = app.process_is_focused(*process_index);
                let flash = app.row_flash_lit(index, Instant::now());
                let flash_lit = flash == Some(FlashSource::Pointer);
                let attachment_flash_lit = flash == Some(FlashSource::Keyboard);
                let prompt_label = app.prompt_label(*process_index);
                let params = RowRenderParams {
                    width,
                    highlight_color: highlight,
                    drawer_outline: drawer_height > 0,
                    pill_text_color: if app.sidebar_focused {
                        app.active_pane_background_color
                    } else {
                        app.inactive_pane_background_color
                    },
                    active_pane_border_color: app.active_pane_border_color,
                    agent_tag_column_width: tag_width,
                    context: Some(context),
                    pill_colors: Some(&pill_colors),
                    location: app.row_location(index),
                    prompt_label: prompt_label.as_deref(),
                    focused_process,
                    flash_lit,
                    attachment_flash_lit,
                    agent_title: app.agent_title(*process_index),
                    parent_note: parent_note.as_deref(),
                };
                render::tree_block(
                    render::render_process_block(
                        process,
                        &params,
                        app.build_drawer_lines_for(*process_index, width),
                    ),
                    tree,
                    &render::row_layout().outline,
                )
            }
        };
        for line in rendered.drain(..) {
            if lines_to_skip > 0 {
                lines_to_skip -= 1;
            } else if y < content_bottom {
                render::render_line(frame, area, y, line, row_background);
                y = y.saturating_add(1);
            }
        }
        // No separator after the last visible row: nothing below needs one.
        let spacing = app.row_block_line_count(index) > app.row_line_count(index);
        if spacing && index + 1 < end {
            if lines_to_skip > 0 {
                lines_to_skip -= 1;
            } else {
                // The tree's spine carries on across the gap to a child.
                let next = match app.rows.get(index + 1) {
                    Some(SidebarListRow::Process { tree, .. }) => Some(tree),
                    _ => None,
                };
                let spacer = render::tree_spacer(next, &render::row_layout().outline);
                if y < content_bottom {
                    render::render_line(frame, area, y, spacer, None);
                }
                y = y.saturating_add(1);
            }
        }
    }
    if bottom_more > 0 && area.height > 0 {
        render::render_centered_line(
            frame,
            area,
            area.bottom() - 1,
            Line::from(Span::styled(
                format!("↓ ({bottom_more} more)"),
                Style::default().fg(Color::DarkGray),
            )),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    use ratatui::style::Color;

    use std::time::Instant;

    fn rendered_list(app: &SidebarApp, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_process_list(frame, frame.area(), app, &test_context()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_child_is_drawn_under_its_parent_joined_by_the_spine() {
        let mut lead = test_process(Some("/work"), None);
        lead.process.name = "lead".to_string();
        lead.process.created_at = 10;
        let mut helper = test_process(Some("/work"), None);
        helper.process.name = "helper".to_string();
        helper.process.pane_id = "%3".to_string();
        helper.process.created_at = 20;
        helper.process.spawned_by = Some(crate::SpawnedBy::local("lead", 10));
        let mut app = SidebarApp::new(None);
        app.grouping.automatic = crate::AutomaticGrouping::Ungrouped;
        app.processes = vec![lead, helper];
        app.rebuild_rows();

        // The first line is kept for the `↑ (N more)` indicator.
        let lines = rendered_list(&app, 42, 9);
        assert!(lines[1].starts_with("┌ ● \u{e0b6}lead"), "{lines:#?}");
        assert!(lines[3].starts_with("└─┬ running"), "{lines:#?}");
        assert_eq!(lines[4], "  │", "{lines:#?}");
        assert!(lines[5].starts_with("  ├ ● \u{e0b6}helper"), "{lines:#?}");
        assert!(lines[6].starts_with("  ╎   process"), "{lines:#?}");
        assert!(lines[7].starts_with("  └── running"), "{lines:#?}");
        assert!(lines.iter().all(|line| text_width(line) <= 42));
    }

    #[test]
    fn hidden_rows_below_always_show_the_more_indicator() {
        // Mixes of folded one-line headers and expanded groups land the last
        // visible row on every possible line, including the last content line
        // whose trailing separator would otherwise push the indicator out.
        for count in 4..=12 {
            for folded in 0..count {
                for height in 10..=24u16 {
                    let mut app = SidebarApp::new(None);
                    app.processes = (0..count)
                        .map(|index| {
                            let mut process =
                                test_process(Some(&format!("/work/d{index:02}")), None);
                            process.process.name = format!("p{index:02}");
                            process.process.created_at = 100 - index as i64;
                            process
                        })
                        .collect();
                    app.grouping = crate::SidebarGroupingState {
                        automatic: crate::AutomaticGrouping::Cwd,
                        collapsed: (0..folded)
                            .map(|index| format!("auto:cwd:/work/d{index:02}"))
                            .collect(),
                        ..Default::default()
                    };
                    app.rebuild_rows();
                    let end = app.visible_end(0, height);
                    if end >= app.rows.len() {
                        continue;
                    }
                    let lines = rendered_list(&app, 48, height);
                    let Some(indicator) = lines.iter().position(|line| line.contains('↓')) else {
                        panic!(
                            "{count} processes, {folded} folded, height {height}: \
                             rows hidden below but no indicator:\n{}",
                            lines.join("\n")
                        );
                    };
                    // The indicator is pinned to the last line, so the list's
                    // bottom edge never moves.
                    assert_eq!(
                        indicator,
                        usize::from(height) - 1,
                        "{count} processes, {folded} folded, height {height}: \
                         indicator not on the last line:\n{}",
                        lines.join("\n")
                    );
                    // Blank lines above it must be too few for the next hidden
                    // row (plus the separator before it); otherwise that row
                    // should have been shown.
                    let last_content = lines[..indicator]
                        .iter()
                        .rposition(|line| !line.is_empty())
                        .unwrap_or(0);
                    let unused = indicator - last_content - 1;
                    let separator = app.row_block_line_count(end - 1) - app.row_line_count(end - 1);
                    let next_minimum = match &app.rows[end] {
                        SidebarListRow::Process { process_index, .. } => {
                            process_line_count(&app.processes[*process_index], false)
                        }
                        _ => 1,
                    };
                    assert!(
                        unused < separator + next_minimum,
                        "{count} processes, {folded} folded, height {height}: \
                         {unused} unused lines above the indicator:\n{}",
                        lines.join("\n")
                    );
                }
            }
        }
    }

    #[test]
    fn older_prompts_fill_only_space_left_by_latest_prompt() {
        let crate::DrawerState::Ready(mut data) = ready_activity() else {
            unreachable!()
        };
        data.recent_prompts = vec![
            "oldest".to_string(),
            "previous".to_string(),
            "latest".to_string(),
        ];
        assert_eq!(
            drawer_prompt_lines(&data, 40),
            ["↑ oldest", "↑ previous", "↑ latest"]
        );

        data.recent_prompts = vec![
            "older must not displace the latest".to_string(),
            "one two three four five six seven eight nine ten".to_string(),
        ];
        let latest_fills_all = drawer_prompt_lines(&data, 16);
        assert_eq!(latest_fills_all.len(), 3);
        assert_eq!(
            latest_fills_all
                .iter()
                .filter(|line| line.starts_with("↑ "))
                .count(),
            1
        );
        assert!(latest_fills_all[0].starts_with("↑ one"));
        assert!(latest_fills_all[2].ends_with('…'));

        data.recent_prompts = vec![
            "previous prompt takes whatever room remains".to_string(),
            "latest".to_string(),
        ];
        let previous_fills_rest = drawer_prompt_lines(&data, 16);
        assert_eq!(previous_fills_rest.len(), 3);
        assert_eq!(
            previous_fills_rest.last().map(String::as_str),
            Some("↑ latest")
        );
        assert_eq!(
            previous_fills_rest
                .iter()
                .filter(|line| line.starts_with("↑ "))
                .count(),
            2
        );
    }

    #[test]
    fn sidebar_header_summarizes_attached_and_detached_processes() {
        let mut attached = test_process(Some("/attached"), None);
        attached.process.attached_clients = 3;
        let detached = test_process(Some("/detached"), None);

        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        assert_eq!(sidebar_count_parts(&app)[0].0, " 0 attached");
        app.processes = vec![attached, detached];
        let parts = sidebar_count_parts(&app);
        assert_eq!(parts[0], (" 1 attached".to_string(), Color::Gray));
        assert_eq!(
            parts[1],
            (", 1 detached".to_string(), render::MUTED_OUTLINE_COLOR),
            "the detached count is as dim as a detached outline"
        );
    }

    #[test]
    fn the_header_names_a_new_grouping_or_graph_for_a_moment() {
        assert_eq!(
            grouping_text(crate::AutomaticGrouping::Ungrouped),
            " Not grouped"
        );
        assert_eq!(
            grouping_text(crate::AutomaticGrouping::Cwd),
            " Grouping by CWD"
        );
        let text = |app: &SidebarApp, at: Instant| -> String {
            sidebar_summary_line(app, 42, at)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let mut app = SidebarApp::new(None);
        let now = Instant::now();
        assert!(text(&app, now).starts_with("   "), "nothing named at rest");
        app.grouping_cycled_at(Ok(crate::AutomaticGrouping::Window), now);
        assert!(text(&app, now).starts_with(" Grouping by window"));
        let later = now + HEADER_NOTICE_DURATION;
        assert!(!text(&app, later).contains("Grouping"));
        // The tick that sees it lapse asks for the redraw that clears it.
        assert_eq!(app.tick(later), [Effect::Redraw]);

        app.cycle_drawer_metric_at(later);
        assert!(text(&app, later).starts_with(" Graph: input tokens"));
        app.cycle_drawer_metric_at(later);
        assert!(text(&app, later).starts_with(" Graph: lines changed"));
        app.cycle_drawer_metric_at(later);
        assert!(text(&app, later).starts_with(" Graph: output tokens"));
        assert!(!text(&app, later + HEADER_NOTICE_DURATION).contains("Graph"));
    }

    #[test]
    fn the_counts_line_adds_resumable_sessions_only_when_all_of_it_fits() {
        let text = |line: Line<'static>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/w"), None)];
        assert_eq!(
            text(sidebar_counts_line(&app, 42, 42)),
            " 0 attached, 1 detached"
        );
        let mut past = crate::HistoryEntry {
            agent: "claude".to_string(),
            session_id: "a".to_string(),
            title: None,
            title_source: None,
            first_prompt: None,
            cwd: None,
            cwd_exists: true,
            git_branch: None,
            started_epoch: None,
            last_activity_epoch: 0,
            transcript: String::new(),
            live: false,
            pmux_process: None,
            resume_command: None,
        };
        app.history = vec![past.clone(), past.clone()];
        past.live = true;
        app.history.push(past);
        assert_eq!(
            text(sidebar_counts_line(&app, 42, 42)),
            " 0 attached, 1 detached, 2 resumable"
        );
        // Left out whole rather than cut short.
        assert_eq!(
            text(sidebar_counts_line(&app, 42, 30)),
            " 0 attached, 1 detached"
        );
    }

    #[test]
    fn open_drawer_header_pins_metric_and_briefly_shows_axis_changes() {
        let mut attached = test_process(Some("/attached"), None);
        attached.process.attached_clients = 3;
        let detached = test_process(Some("/detached"), None);
        let mut app = SidebarApp::new(None);
        app.processes = vec![attached, detached];
        let now = Instant::now();
        app.header_selected = true;
        app.sidebar_focused = true;

        let idle_line = sidebar_summary_line(&app, 42, now);
        let idle_text = idle_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(idle_text.trim_end().ends_with(DRAWER_GRAPH_INDICATOR));
        assert!(idle_text.ends_with(' '));
        assert!(!idle_text.contains("+/-"));
        assert_eq!(
            idle_line.spans.last().and_then(|span| span.style.fg),
            Some(Color::DarkGray)
        );
        assert_eq!(
            idle_line.spans.last().and_then(|span| span.style.bg),
            Some(CURSOR_HIGHLIGHT_COLOR)
        );
        assert!(
            idle_line.spans[idle_line.spans.len() - 4..]
                .iter()
                .all(|span| span.style.bg == Some(CURSOR_HIGHLIGHT_COLOR))
        );

        open_test_drawers(&mut app, &["demo"]);

        let metric_line = sidebar_summary_line(&app, 42, now);
        let metric_text = metric_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(metric_text.trim_end().ends_with("⣠⣾⣦↓"));
        assert!(metric_text.ends_with(' '));
        assert_eq!(text_width(&metric_text), 42);
        assert!(
            metric_line.spans[metric_line.spans.len() - 4..]
                .iter()
                .all(|span| span.style.fg == Some(Color::Yellow))
        );

        app.cycle_drawer_axis(now);
        let axis_label = crate::AXIS_PRESETS[app.drawer_axis].0;
        let axis_text = sidebar_summary_line(&app, 42, now)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(axis_text.trim_end().ends_with(&format!("⣠⣾⣦{axis_label}")));

        let restored = sidebar_summary_line(&app, 42, now + super::DRAWER_AXIS_HINT_DURATION)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(restored.trim_end().ends_with("⣠⣾⣦↓"));

        app.cycle_drawer_metric();
        let tokens_line = sidebar_summary_line(&app, 42, now);
        let tokens = tokens_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(tokens.trim_end().ends_with("⣠⣾⣦↑"));
        assert!(
            tokens_line.spans[tokens_line.spans.len() - 4..]
                .iter()
                .all(|span| span.style.fg == Some(Color::LightMagenta))
        );
    }

    #[test]
    fn sidebar_help_contains_vim_navigation_bindings() {
        let sections = sidebar_help_sections(&SidebarKeymap::default(), false);
        let text = sidebar_help_lines(&sections, 90)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("k/↑ · j/↓"));
        assert!(text.contains("^u · ^d"));
        assert!(text.contains("gg · G"));
        assert!(text.contains("open/close its drawer"));
        assert!(
            !text.contains("↻"),
            "Tab and Enter work alike on past sessions"
        );
        assert!(text.contains("toggle every process drawer"));
        assert!(text.contains("cycle graph metric"));
        assert!(text.contains("cycle timeline scale"));
        assert!(text.contains("fold or unfold the current section"));
        assert!(text.contains("→/click×2"));
        // Paging and Home/End still work but are left to the Vim keys.
        for hidden in ["PgUp", "PgDn", "Home", "End", "fold/unfold"] {
            assert!(!text.contains(hidden), "{hidden}");
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn sidebar_help_wraps_descriptions_under_their_column_with_gaps_between_sections() {
        let sections = sidebar_help_sections(&SidebarKeymap::default(), false);
        let width = 36;
        let lines: Vec<String> = sidebar_help_lines(&sections, width)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            lines.iter().all(|line| text_width(line) <= width),
            "{lines:#?}"
        );
        // A blank row before every heading but the first.
        let headings: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| ["Navigation", "Actions", "Grouping"].contains(&line.trim()))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(headings[0], 0);
        assert!(
            headings[1..]
                .iter()
                .all(|&index| lines[index - 1].is_empty())
        );
        // A long description continues on the next row, in its own column.
        let start = lines
            .iter()
            .position(|line| line.contains("Double-click"))
            .unwrap();
        let column = lines[start].find("jump").unwrap();
        let continuation = &lines[start + 1];
        assert!(continuation[..column].trim().is_empty(), "{continuation:?}");
        assert!(!continuation.trim().is_empty());
    }

    #[test]
    fn sidebar_help_scrolls_inside_a_small_window() {
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = SidebarApp::new(None);
        app.help_open = true;
        app.help_scroll = usize::MAX;
        terminal
            .draw(|frame| draw_sidebar_help(frame, &mut app, false))
            .unwrap();
        assert_eq!(app.help_page, 8);
        let sections = sidebar_help_sections(&app.keymap, false);
        let inner_width = modal_area(Rect::new(0, 0, 40, 10), 96, 1).width - 2;
        let total = sidebar_help_lines(&sections, usize::from(inner_width)).len();
        assert_eq!(app.help_scroll, total - 8, "clamped to the last page");
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("↑ more"));
        assert!(screen.contains("toggle this help"));
    }

    #[test]
    fn tmux_keys_render_in_caret_form() {
        assert_eq!(format_tmux_key("C-s"), "^s");
        assert_eq!(format_tmux_key("C-b"), "^b");
        assert_eq!(format_tmux_key(" C-a "), "^a");
        // Non-control keys pass through unchanged.
        assert_eq!(format_tmux_key("M-a"), "M-a");
        assert_eq!(format_tmux_key("F1"), "F1");
    }

    /// Names and paths mixing double-width CJK, emoji, ZWJ sequences, and
    /// combining marks, which are the usual way width arithmetic goes wrong.
    pub(in crate::tui) const WIDE_NAMES: &[&str] = &[
        "日本語のプロセス名前がとても長い",
        "🦀🦀🦀-rust",
        "👩\u{200d}👩\u{200d}👧\u{200d}👦-family",
        "e\u{301}\u{301}-combining",
        "x",
        "1790869355-vurcl60h",
    ];

    fn wide_sidebar_app(grouping: crate::AutomaticGrouping, drawers: bool) -> SidebarApp {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        app.grouping = crate::SidebarGroupingState {
            automatic: grouping,
            manual: vec![crate::ManualGroup {
                id: "g".to_string(),
                name: "手動グループ 👩\u{200d}💻".to_string(),
                processes: vec![WIDE_NAMES[0].to_string(), WIDE_NAMES[2].to_string()],
            }],
            resumable_expanded: true,
            ..Default::default()
        };
        app.processes = WIDE_NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let cwd = format!("/srv/項目/{name}");
                let mut process = test_process(Some(&cwd), Some("/srv/起動"));
                process.process.name = (*name).to_string();
                process.process.pane_id = format!("%{index}");
                process.process.created_at = 100 + index as i64;
                process.process.attached_clients = (index % 3) as i64;
                if index % 2 == 0 {
                    process.process.agent = Some(crate::AgentInfo {
                        kind: "claude".to_string(),
                        state: None,
                        updated_at: None,
                        source: None,
                        reason: None,
                        session_id: None,
                    });
                }
                if index == 1 {
                    process.outer_location = Some(crate::OuterPaneLocation {
                        pane_id: "%host".to_string(),
                        pane_index: "1".to_string(),
                        window_id: "@1".to_string(),
                        window_index: "1".to_string(),
                        window_name: "作業 🛠".to_string(),
                    });
                }
                process
            })
            .collect();
        let mut history = test_history_entry("past", "/srv/過去", 50);
        history.title = Some("過去のセッション 👨\u{200d}👩\u{200d}👧".to_string());
        app.set_history(vec![history]);
        if drawers {
            app.open_drawers = WIDE_NAMES.iter().map(|name| name.to_string()).collect();
            for index in 0..app.processes.len() {
                let state = match index % 3 {
                    0 => {
                        let mut activity = ready_activity();
                        if let crate::DrawerState::Ready(data) = &mut activity {
                            data.summary = Some("全角の要約 🦀 を表示する".repeat(4));
                            data.recent_prompts = vec![
                                "👩\u{200d}👩\u{200d}👧 prompt".to_string(),
                                "長い指示".repeat(10),
                            ];
                        }
                        activity
                    }
                    1 => crate::DrawerState::Unavailable,
                    _ => crate::DrawerState::Loading,
                };
                app.cache_activity(index, state);
            }
        }
        app.toggle_hint = " ^b+^p:開く/閉じる 🦀".to_string();
        app
    }

    #[test]
    fn sidebar_draws_wide_names_at_every_tiny_size_without_panicking() {
        let context = test_context();
        for (grouping, drawers) in [
            (crate::AutomaticGrouping::Ungrouped, true),
            (crate::AutomaticGrouping::Cwd, false),
            (crate::AutomaticGrouping::Window, true),
        ] {
            let mut app = wide_sidebar_app(grouping, drawers);
            for width in 0..24u16 {
                for height in 0..12u16 {
                    // Vary the overlays and the scroll position with the size
                    // so every combination meets many sizes, and a clipped
                    // first block is drawn too.
                    app.help_open = (width + height) % 2 == 0;
                    app.message = (height % 3 == 0).then(|| "エラー: 🦀".to_string());
                    app.selected = usize::from(width + height) % app.rows.len().max(1);
                    app.ensure_selected_item_visible(height);
                    let pane_label = width % 2 == 0;
                    let backend = ratatui::backend::TestBackend::new(width, height);
                    let mut terminal = ratatui::Terminal::new(backend).unwrap();
                    terminal
                        .draw(|frame| draw_sidebar(frame, &mut app, &context, pane_label))
                        .unwrap_or_else(|err| panic!("{width}x{height}: {err}"));
                }
            }
        }
    }

    #[test]
    fn the_search_line_shows_the_query_being_typed_then_the_active_filter() {
        let line = |app: &SidebarApp| sidebar_search_line(app).map(|(text, _)| text);
        let mut app = SidebarApp::new(None);
        app.toggle_hint = "hint".to_string();
        assert_eq!(line(&app), None);
        app.search.editing = true;
        assert_eq!(line(&app).as_deref(), Some(" /▏ "));
        app.search.query = "api".to_string();
        app.search.matches = 2;
        app.message = Some("boom".to_string());
        assert_eq!(line(&app).as_deref(), Some(" /api▏ · 2 matches"));
        // The footer keeps its messages while searching.
        assert_eq!(sidebar_footer(&app).0, "boom");
        app.search.editing = false;
        assert_eq!(line(&app).as_deref(), Some(" /api · 2 matches • esc:clear"));
        app.message = None;
        assert_eq!(sidebar_footer(&app).0, "hint");
        app.search.matches = 0;
        assert_eq!(
            line(&app).as_deref(),
            Some(" /api · no matches • esc:clear")
        );
    }

    #[test]
    fn the_search_line_ends_with_the_rows_scrolled_above_it() {
        assert_eq!(fitted_search_line(" /api", 0, 20), " /api");
        assert_eq!(fitted_search_line(" /api", 3, 14), " /api     ↑ 3 ");
        let fitted = fitted_search_line(" /a-long-query · 9 matches", 12, 20);
        assert!(fitted.ends_with("↑ 12 "), "{fitted:?}");
        assert_eq!(text_width(&fitted), 20);
    }

    #[test]
    fn footer_shows_reconnect_status_below_messages_and_above_the_hint() {
        let mut app = SidebarApp::new(None);
        app.toggle_hint = "hint".to_string();
        assert_eq!(sidebar_footer(&app).0, "hint");
        app.event_stream_down = true;
        assert_eq!(sidebar_footer(&app).0, SIDEBAR_RECONNECTING_STATUS);
        app.message = Some("boom".to_string());
        assert_eq!(sidebar_footer(&app).0, "boom");
    }

    #[test]
    fn toggle_hint_uses_the_live_prefix_and_falls_back() {
        assert_eq!(
            sidebar_toggle_hint_text(Some("C-s")),
            " ^s+^p:open/close • q:close • ?:help"
        );
        assert_eq!(
            sidebar_toggle_hint_text(Some("C-b")),
            " ^b+^p:open/close • q:close • ?:help"
        );
        // No readable prefix → literal "prefix".
        assert_eq!(
            sidebar_toggle_hint_text(None),
            " prefix+^p:open/close • q:close • ?:help"
        );
    }
}
