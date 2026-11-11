//! Shared presentation for the sidebar and the attach picker.
//!
//! This module is the **single source of truth** for how a pmux process is
//! drawn as a multi-line row. Both the outer-tmux sidebar (`tui/view.rs`) and
//! the attach/switch picker (`attach_picker.rs`) draw every process through
//! [`render_process_block`] (the row plus any open drawer beneath it), with the
//! same agent titles and prompt counts ([`RowDecorations`]), so a visual change
//! to a row — the pill, status line, outline, or drawer edge — is made once
//! here and appears in both surfaces. Each surface keeps its own list loop so
//! it can interleave the rows unique to that view, while sharing the viewport
//! math, `↑/↓ more` indicators, and spacing defined here.
//!
//! The renderer is context-optional. The sidebar passes a [`SidebarContext`]
//! (it runs inside the outer tmux server and knows where each process is
//! displayed), which enables the attachment arrow/detail. The picker has no
//! outer-tmux context, so it passes `None`: the outer-location bits are skipped
//! and the attachment indicator falls back to a client count.

mod row;
mod row_config;
mod tree;

pub use row::{RowLayout, RowOutline, render_row, row_layout, set_row_layout};
pub use row_config::{RowSurface, configured_preset, load_row_layout, with_preset};
pub use tree::{RowTree, tree_block, tree_spacer};

use crate::sidebar::{OuterPaneLocation, SidebarContext, SidebarProcess};
use anyhow::{Context, Result};
use crossterm::{
    event, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

pub type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub const CURSOR_BAR: &str = "▊";
/// Tile `glyph` (which may be a multi-cell pattern) to exactly `columns` display
/// columns, anchored to the RIGHT: the last cell is always the pattern's final
/// glyph, so a complete dash chunk lands on the right edge (flush with the solid
/// line) and any partial chunk is truncated on the LEFT, nearest the
/// variable-width name/cwd. `glyph` must be composed of single-column chars —
/// every row fill is — which keeps the phase math in simple column units.
pub fn fill_to_width(glyph: &str, columns: usize) -> String {
    let chars: Vec<char> = glyph.chars().collect();
    debug_assert!(
        chars
            .iter()
            .all(|&c| crate::display_width(c.encode_utf8(&mut [0u8; 4])) == 1),
        "fill glyph must be composed of single-column characters"
    );
    if chars.is_empty() || columns == 0 {
        return String::new();
    }
    let len = chars.len();
    // Phase the tiling so column `columns - 1` lands on the pattern's last glyph.
    let offset = (len - columns % len) % len;
    (0..columns).map(|j| chars[(j + offset) % len]).collect()
}
/// Subtle neutral charcoal: enough contrast to track the selected item without
/// turning the full drawer into a conspicuous blue block.
pub const CURSOR_HIGHLIGHT_COLOR: Color = Color::Rgb(62, 62, 62);
pub const UNFOCUSED_CURSOR_HIGHLIGHT_COLOR: Color = Color::Rgb(62, 62, 62);
pub const ATTACHMENT_COLOR: Color = Color::Rgb(180, 180, 180);
/// The sidebar drag handle's resting color, also used for the lit phase of
/// a pane flash so the row and its outer pane/window share one accent.
pub const DRAG_HANDLE_COLOR: Color = Color::Cyan;
/// Quiet outline color for processes not attached in the current outer window.
/// The current-window outline keeps the outer pane's active border color.
pub const MUTED_OUTLINE_COLOR: Color = Color::DarkGray;
pub const ACTIVE_PANE_BORDER_FALLBACK_COLOR: Color = Color::Rgb(255, 136, 0);
pub const ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR: Color = Color::Indexed(234);
pub const INACTIVE_PANE_BACKGROUND_FALLBACK_COLOR: Color = Color::Indexed(236);
/// Curated tag palette, drawn from the xterm-256 color cube so the exact color
/// renders on any 256-color terminal (24-bit truecolor is often quantized away
/// through nested tmux/screen, which would collapse distinct RGB pills into a
/// handful of look-alikes). 24 hues, spread around the wheel with some
/// brightness variety, chosen to stay easily distinguishable from one another.
pub const SESSION_NAME_PILL_COLORS: [Color; 24] = [
    Color::Indexed(210), // red
    Color::Indexed(174), // dusty rose
    Color::Indexed(209), // coral
    Color::Indexed(215), // orange
    Color::Indexed(173), // terracotta
    Color::Indexed(222), // amber
    Color::Indexed(186), // yellow
    Color::Indexed(143), // olive
    Color::Indexed(150), // lime
    Color::Indexed(114), // green
    Color::Indexed(71),  // forest
    Color::Indexed(79),  // teal
    Color::Indexed(80),  // cyan
    Color::Indexed(73),  // steel teal
    Color::Indexed(117), // sky
    Color::Indexed(74),  // steel blue
    Color::Indexed(111), // blue
    Color::Indexed(105), // indigo
    Color::Indexed(141), // violet
    Color::Indexed(176), // orchid
    Color::Indexed(139), // mauve
    Color::Indexed(213), // magenta
    Color::Indexed(212), // pink
    Color::Indexed(218), // rose
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentScope {
    CurrentWindow,
    OtherWindow,
    OtherSession,
    Unattached,
}

pub fn attachment_scope(process: &SidebarProcess, context: &SidebarContext) -> AttachmentScope {
    attachment_scope_at(process, process.outer_location.as_ref(), context)
}

/// [`attachment_scope`] for one specific attachment of a process.
pub fn attachment_scope_at(
    process: &SidebarProcess,
    location: Option<&OuterPaneLocation>,
    context: &SidebarContext,
) -> AttachmentScope {
    match location {
        Some(location) if location.window_id == context.host_window_id => {
            AttachmentScope::CurrentWindow
        }
        Some(_) => AttachmentScope::OtherWindow,
        None if process.process.attached_clients > 0 => AttachmentScope::OtherSession,
        None => AttachmentScope::Unattached,
    }
}

/// Attachment scope derivable without an outer-tmux context (picker mode).
/// Mirrors the `None`-location arms of [`attachment_scope`].
fn attachment_scope_contextless(process: &SidebarProcess) -> AttachmentScope {
    if process.process.attached_clients > 0 {
        AttachmentScope::OtherSession
    } else {
        AttachmentScope::Unattached
    }
}

pub fn attachment_icon(scope: AttachmentScope, sidebar_on_right: bool) -> Option<&'static str> {
    match (scope, sidebar_on_right) {
        (AttachmentScope::CurrentWindow, false) => Some("→"),
        (AttachmentScope::CurrentWindow, true) => Some("←"),
        (AttachmentScope::OtherWindow, false) => Some("↗"),
        (AttachmentScope::OtherWindow, true) => Some("↖"),
        (AttachmentScope::OtherSession, _) => Some("⧉"),
        (AttachmentScope::Unattached, _) => Some("◯"),
    }
}

/// `focused` marks the process attached in the focused outer pane: it, and
/// every copy of it shown for other windows, takes the active border color.
pub fn first_line_color(
    scope: AttachmentScope,
    focused: bool,
    active_pane_border_color: Color,
) -> Color {
    if focused {
        return active_pane_border_color;
    }
    match scope {
        // Attached alongside the focused process, or in another tmux session:
        // bright white, one step down from the focused process's border color.
        AttachmentScope::CurrentWindow | AttachmentScope::OtherSession => Color::White,
        // Attached elsewhere in this session, or not attached at all: the dimmer
        // gray, one step down from white.
        AttachmentScope::OtherWindow | AttachmentScope::Unattached => ATTACHMENT_COLOR,
    }
}

pub fn outline_color(focused: bool, active_pane_border_color: Color) -> Color {
    if focused {
        active_pane_border_color
    } else {
        MUTED_OUTLINE_COLOR
    }
}

pub fn launch_cwd_text(process: &SidebarProcess) -> Option<&str> {
    let current = process
        .process
        .current_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())?;
    let launch = process
        .process
        .launch_cwd
        .as_deref()
        .filter(|path| !path.trim().is_empty())?;
    (launch != current).then_some(launch)
}

/// Lines in one [`render_process_row`], `noted` when it names its parent.
pub fn process_line_count(process: &SidebarProcess, noted: bool) -> usize {
    row_layout().line_count(process, noted)
}

/// Lines in one [`render_resumable_row`].
pub const RESUMABLE_ROW_LINES: usize = 2;

/// Build the lines for a past agent session in the sidebar's Resumable group
/// and the picker: its title in a pill like a live row's, then the agent,
/// last activity, and directory. The pill is italic unless the session was
/// named explicitly, as a live row's derived title is, and gray when the
/// directory is gone, since the session cannot be resumed there.
/// `pill_text_color` is the pane background the pill's text is drawn in.
pub fn render_resumable_row(
    entry: &crate::HistoryEntry,
    prompt_label: Option<&str>,
    width: u16,
    highlight_color: Option<Color>,
    pill_text_color: Color,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let outline = content_style(Style::default().fg(Color::DarkGray), highlight_color);
    let pill_color = if entry.cwd_exists {
        // Keyed by the pmux process when known, so a session keeps the color
        // it had while live.
        session_name_pill_color(entry.pmux_process.as_deref().unwrap_or(&entry.session_id))
    } else {
        Color::DarkGray
    };
    let mut pill_style = session_name_pill_style(pill_color, pill_text_color);
    if entry.title_source != Some(crate::history::TitleSource::Custom) {
        pill_style = pill_style.add_modifier(Modifier::ITALIC);
    }
    // The outline corner, the `↻ `, and the pill's two caps.
    let title = crate::truncate_col(
        entry.title.as_deref().unwrap_or(&entry.session_id),
        width.saturating_sub(7),
    );
    let first = Line::from(vec![
        Span::styled(format!("{} ", row_layout().outline.top), outline),
        Span::styled(
            "↻ ",
            content_style(Style::default().fg(Color::Gray), highlight_color),
        ),
        Span::styled(
            "\u{e0b6}",
            content_style(Style::default().fg(pill_color), highlight_color),
        ),
        Span::styled(title, pill_style),
        Span::styled(
            "\u{e0b4}",
            content_style(Style::default().fg(pill_color), highlight_color),
        ),
    ]);

    let age = format!(" · {} ago", crate::relative_age(entry.last_activity_epoch));
    // The prompt count comes before the CWD, which truncates first.
    let prompts = prompt_label
        .map(|label| format!(" · {label}"))
        .unwrap_or_default();
    let cwd = match entry.cwd.as_deref() {
        Some(cwd) if entry.cwd_exists => format!(" · {}", crate::display_path(cwd)),
        Some(cwd) => format!(" · {} (gone)", crate::display_path(cwd)),
        None => String::new(),
    };
    let used = 2 + text_width(&entry.agent) + text_width(&age) + text_width(&prompts);
    let cwd = crate::truncate_col(&cwd, width.saturating_sub(used + 1));
    let dim = content_style(Style::default().fg(Color::DarkGray), highlight_color);
    let second = Line::from(vec![
        Span::styled(format!("{} ", row_layout().outline.bottom), outline),
        Span::styled(
            entry.agent.clone(),
            content_style(
                Style::default().fg(agent_kind_color(&entry.agent)),
                highlight_color,
            ),
        ),
        Span::styled(age, dim),
        Span::styled(prompts, dim),
        Span::styled(cwd, dim),
    ]);
    vec![first, second]
}

pub fn list_content_height(list_height: u16) -> usize {
    // One line at each edge is reserved for the `more` indicators.
    list_height.saturating_sub(2).max(3) as usize
}

/// Index one past the last process that fits, starting from `start`. A blank
/// separator after every visible record except the final one is part of the
/// height budget so it is a stable part of the viewport rather than an
/// accidental overflow. `line_count(i)` returns the row height of entry `i`
/// (without its separator).
pub fn viewport_end(
    len: usize,
    start: usize,
    list_height: u16,
    line_count: impl Fn(usize) -> usize,
) -> usize {
    viewport_end_with_offset(len, start, 0, list_height, line_count)
}

/// Offset-aware form of [`viewport_end`]. `first_line_offset` lines are already
/// scrolled off the top of the first entry's row+separator block.
pub fn viewport_end_with_offset(
    len: usize,
    start: usize,
    first_line_offset: usize,
    list_height: u16,
    line_count: impl Fn(usize) -> usize,
) -> usize {
    viewport_end_with_offset_and_minimum(
        len,
        start,
        first_line_offset,
        list_height,
        &line_count,
        &line_count,
    )
}

/// Offset-aware viewport calculation that may include a partially visible last
/// entry once `minimum_visible(index)` lines still fit. This lets a process row
/// remain visible while its drawer continues below the viewport.
pub fn viewport_end_with_offset_and_minimum(
    len: usize,
    start: usize,
    first_line_offset: usize,
    list_height: u16,
    line_count: impl Fn(usize) -> usize,
    minimum_visible: impl Fn(usize) -> usize,
) -> usize {
    // Every entry but the last is followed by one blank separator line.
    viewport_end_for_blocks(
        len,
        start,
        first_line_offset,
        list_height,
        |index| line_count(index) + usize::from(index + 1 < len),
        minimum_visible,
    )
}

/// [`viewport_end_with_offset_and_minimum`] for lists whose separators vary:
/// `block_height(index)` is the entry's full height *including* any blank
/// line rendered after it. The sidebar needs this because a folded group
/// header has no separator, so assuming one per entry would end the viewport
/// early and leave unused lines below the `↓ (N more)` indicator.
pub fn viewport_end_for_blocks(
    len: usize,
    start: usize,
    first_line_offset: usize,
    list_height: u16,
    block_height: impl Fn(usize) -> usize,
    minimum_visible: impl Fn(usize) -> usize,
) -> usize {
    let mut used = 0;
    let available = list_content_height(list_height);
    let mut index = start.min(len);
    while index < len {
        let mut height = block_height(index);
        if index == start {
            height = height.saturating_sub(first_line_offset);
        }
        if index > start && used + minimum_visible(index) > available {
            break;
        }
        used += height;
        index += 1;
    }
    index
}

pub fn content_style(style: Style, highlight_color: Option<Color>) -> Style {
    highlight_color.map_or(style, |color| style.bg(color))
}

pub fn selection_background(highlight_color: Option<Color>) -> Style {
    highlight_color.map_or_else(Style::default, |color| Style::default().bg(color))
}

pub fn session_name_pill_style(pill_color: Color, pane_background_color: Color) -> Style {
    Style::default()
        .fg(pane_background_color)
        .bg(pill_color)
        .add_modifier(Modifier::BOLD)
}

/// Pick a stable, readable pill color directly from the canonical pmux
/// process name. This avoids assigning colors based on list order, which can
/// change whenever a session emits a new signal.
/// The palette slot a name hashes to (its *preferred* color). This is the stable
/// per-name starting point; [`assign_pill_colors`] probes onward from here when
/// an older process already holds that slot.
fn session_name_pill_index(process_name: &str) -> usize {
    let hash = process_name
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    hash as usize % SESSION_NAME_PILL_COLORS.len()
}

/// A name's preferred pill color, ignoring collisions. Used as the fallback when
/// no assignment map is supplied (e.g. unit tests); live rendering goes through
/// [`assign_pill_colors`] so visible tags don't share a color.
pub fn session_name_pill_color(process_name: &str) -> Color {
    SESSION_NAME_PILL_COLORS[session_name_pill_index(process_name)]
}

/// Assign every process a pill color so that, as long as there are no more
/// distinct names than palette colors, each visible tag is a different color.
///
/// A process prefers its name's hashed slot and, when an *older* process already
/// took that slot, probes to the next free color (wrapping). Assignment runs
/// oldest-first (by `created_at`, then name for a stable tiebreak), so **oldest
/// wins**: creating a new process never recolors an existing tag — the newcomer
/// yields to whatever is already there. (Removing a process can still shift a
/// newer one whose preferred slot it was occupying.) With more distinct names
/// than colors, the overflow falls back to the hashed slot and may repeat.
///
/// Callers pass the **full** process set (not a filtered/visible view) so the
/// sidebar and the picker agree on a given name's color; the result is keyed by
/// process name.
pub fn assign_pill_colors(processes: &[SidebarProcess]) -> HashMap<String, Color> {
    let n = SESSION_NAME_PILL_COLORS.len();
    let mut order: Vec<&SidebarProcess> = processes.iter().collect();
    order.sort_by(|a, b| {
        a.process
            .created_at
            .cmp(&b.process.created_at)
            .then_with(|| a.process.name.cmp(&b.process.name))
    });
    let mut used = vec![false; n];
    let mut colors: HashMap<String, Color> = HashMap::new();
    for process in order {
        let name = &process.process.name;
        if colors.contains_key(name) {
            continue; // same name already placed — one color per name
        }
        let base = session_name_pill_index(name);
        let idx = (0..n)
            .map(|offset| (base + offset) % n)
            .find(|&i| !used[i])
            .unwrap_or(base);
        used[idx] = true;
        colors.insert(name.clone(), SESSION_NAME_PILL_COLORS[idx]);
    }
    colors
}

pub fn text_width(text: &str) -> usize {
    crate::display_width(text)
}

/// Separates the CWD from an agent's prompt count on a row's second line.
const PROMPT_SEPARATOR: &str = " · ";

/// Compact human count for drawer stats and row prompt counts (e.g. `1.8M`,
/// `548k`, `640`).
pub fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// The prompt count shown after an agent row's CWD (e.g. `12p`).
pub fn prompt_label(data: &crate::ActivityData) -> String {
    format!("{}p", fmt_count(u64::from(data.prompts)))
}

/// Each live process's agent conversation title, keyed by process name, from
/// a history catalog loaded with `include_live`.
pub fn agent_titles(entries: &[crate::HistoryEntry]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| Some((entry.pmux_process.clone()?, entry.title.clone()?)))
        .collect()
}

/// Per-process row text loaded off the UI thread (agent titles from the
/// history catalog, prompt counts from parsed transcripts), keyed by process
/// name. Empty until a load lands; rows simply omit what is missing.
#[derive(Default)]
pub struct RowDecorations {
    pub agent_titles: HashMap<String, String>,
    pub prompt_labels: HashMap<String, String>,
}

/// Widest agent/command tag a row reserves room for. Longer command names are
/// truncated so one of them cannot push every row's CWD across the sidebar.
pub const MAX_AGENT_TAG_WIDTH: usize = 12;

pub fn agent_tag_column_width<'a>(
    processes: impl IntoIterator<Item = &'a SidebarProcess>,
) -> usize {
    processes
        .into_iter()
        .map(crate::sidebar_agent_tag)
        .map(text_width)
        .max()
        .unwrap_or_default()
        .min(MAX_AGENT_TAG_WIDTH)
}

/// The tag color identifying an agent kind (`claude`, `codex`, …).
pub fn agent_kind_color(kind: &str) -> Color {
    use crate::agents::Agent;
    // Matching on the enum makes the compiler point here when an agent is
    // added; unknown names (other ai-attn producers) keep the fallback.
    match Agent::from_name(kind) {
        Some(Agent::Claude) => Color::Rgb(214, 130, 85),
        Some(Agent::Codex) => Color::Rgb(62, 166, 130),
        Some(Agent::Gemini) => Color::Rgb(96, 124, 230),
        Some(Agent::Cortex) => Color::Rgb(41, 181, 232),
        Some(Agent::Muse | Agent::OpenCode | Agent::Pi) | None => Color::Magenta,
    }
}

pub fn process_badges(process: &SidebarProcess) -> (&str, Color, String, Style, Option<i64>) {
    match process.process.agent.as_ref() {
        Some(agent) => {
            let tag_color = agent_kind_color(&agent.kind);
            let (state, state_style) = attention_style(agent.state.as_deref());
            (
                agent.kind.as_str(),
                tag_color,
                state,
                state_style,
                agent.updated_at,
            )
        }
        None => (
            "process",
            Color::DarkGray,
            "running".to_string(),
            Style::default().fg(Color::Gray),
            Some(process.process.last_activity_at),
        ),
    }
}

pub fn attention_style(state: Option<&str>) -> (String, Style) {
    match state.unwrap_or("idle") {
        "working" | "busy" => ("working".to_string(), Style::default().fg(Color::Cyan)),
        "waiting" | "needs_input" | "attention" => (
            state.unwrap_or("waiting").to_string(),
            Style::default().fg(Color::Yellow),
        ),
        "done" | "complete" => (
            state.unwrap_or("done").to_string(),
            Style::default().fg(Color::Green),
        ),
        "error" | "failed" | "stopped" => (
            state.unwrap_or("error").to_string(),
            Style::default().fg(Color::Red),
        ),
        other => (other.to_string(), Style::default().fg(Color::DarkGray)),
    }
}

/// The attachment detail text pinned to the right of a process's status row —
/// `pane N` / `window N` when the outer location is known (sidebar), or the
/// client count (`N attached`) when it is not (picker).
fn attachment_detail(
    process: &SidebarProcess,
    location: Option<&OuterPaneLocation>,
    context: Option<&SidebarContext>,
) -> Option<String> {
    match context {
        Some(context) => crate::sidebar_attachment_detail_at(process, location, context),
        None => (process.process.attached_clients > 0)
            .then(|| format!("{} attached", process.process.attached_clients)),
    }
}

/// Per-row styling inputs that vary between callers and between rows.
pub struct RowRenderParams<'a> {
    pub width: u16,
    pub highlight_color: Option<Color>,
    /// Whether the process header continues into an open details drawer.
    pub drawer_outline: bool,
    pub pill_text_color: Color,
    pub active_pane_border_color: Color,
    pub agent_tag_column_width: usize,
    /// `Some` for the sidebar (enables the outer attachment arrow/detail),
    /// `None` for the picker (client-count indicator only).
    pub context: Option<&'a SidebarContext>,
    /// Per-name pill colors from [`assign_pill_colors`]. When `None`, or missing
    /// this name, the row falls back to the name's preferred (hashed) color.
    pub pill_colors: Option<&'a HashMap<String, Color>>,
    /// The attachment this row represents when a process is attached in
    /// several windows; `None` uses the process's primary location.
    pub location: Option<&'a OuterPaneLocation>,
    /// Whether this process is attached in the focused outer pane.
    pub focused_process: bool,
    /// Lit phase of a flash on this row's pane: the row's text, `pane N` /
    /// `window N` detail and outline blink in step with the outer flash.
    pub flash_lit: bool,
    /// Lit phase of a quieter flash (from the keyboard cursor): only the
    /// attachment arrow and `pane N` / `window N` detail blink.
    pub attachment_flash_lit: bool,
    /// The agent's prompt count (e.g. `12p`), shown after the CWD once its
    /// activity has been parsed; `None` for non-agents or until it loads.
    pub prompt_label: Option<&'a str>,
    /// The agent's own title for the conversation in this process. Shown in
    /// the name pill when the process has no user-chosen name.
    pub agent_title: Option<&'a str>,
    /// The process that spawned this one (`lead`, `lead on macbook`), for a
    /// row not drawn under it, shown as `from lead`.
    pub parent_note: Option<&'a str>,
}

/// Build the 2–3 styled lines for one process row. Pure and unit-testable; the
/// selection background is applied by the caller via [`render_line`], not baked
/// into the returned spans, so the pill keeps its own fill over a selected row.
pub fn render_process_row(
    process: &SidebarProcess,
    params: &RowRenderParams<'_>,
) -> Vec<Line<'static>> {
    render_row(process, params, row_layout())
}

/// One process row followed by its open details drawer, as both the sidebar
/// and the picker draw it. `drawer_lines` (empty for a folded row) take the
/// row's outline: dashed while unattached, and the row's edge color.
pub fn render_process_block(
    process: &SidebarProcess,
    params: &RowRenderParams<'_>,
    drawer_lines: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let mut lines = render_process_row(process, params);
    let scope = match params.context {
        Some(context) => attachment_scope(process, context),
        None => attachment_scope_contextless(process),
    };
    let edge_color = if params.flash_lit {
        DRAG_HANDLE_COLOR
    } else {
        outline_color(params.focused_process, params.active_pane_border_color)
    };
    let detached = scope == AttachmentScope::Unattached;
    // A dashed rule matches the folded row's endpoint and dash phase: in the
    // sidebar the top row reserves its final cell after the attachment glyph,
    // so the drawer does too.
    let trailing_padding = usize::from(detached && params.context.is_some());
    for mut line in drawer_lines {
        outline_drawer_line(&mut line, &row_layout().outline, detached, trailing_padding);
        if let Some(first) = line.spans.first_mut() {
            first.style = first.style.fg(edge_color);
        }
        lines.push(line);
    }
    lines
}

/// Redraw the outline of a drawer line, built with `│` for its edge and `└─…`
/// for its bottom, in `outline`'s glyphs. A bottom rule leaves its last
/// `trailing_padding` cells blank.
pub fn outline_drawer_line(
    line: &mut Line<'static>,
    outline: &RowOutline,
    detached: bool,
    trailing_padding: usize,
) {
    let Some(first) = line.spans.first_mut() else {
        return;
    };
    if let Some(rest) = first.content.strip_prefix('│') {
        first.content = format!("{}{rest}", outline.side(detached)).into();
    } else if first.content.starts_with('└') {
        let rule_width = text_width(first.content.as_ref()).saturating_sub(1);
        first.content = format!(
            "{}{}{}",
            outline.bottom,
            fill_to_width(
                outline.rule(detached),
                rule_width.saturating_sub(trailing_padding)
            ),
            " ".repeat(trailing_padding),
        )
        .into();
    }
}

pub fn render_line(
    frame: &mut ratatui::Frame,
    area: Rect,
    y: u16,
    line: Line<'_>,
    highlight_color: Option<Color>,
) {
    if y >= area.bottom() {
        return;
    }
    frame.render_widget(
        Paragraph::new(line).style(selection_background(highlight_color)),
        Rect::new(area.x, y, area.width, 1),
    );
}

pub fn render_centered_line(frame: &mut ratatui::Frame, area: Rect, y: u16, line: Line<'_>) {
    if y >= area.bottom() {
        return;
    }
    frame.render_widget(
        Paragraph::new(line).alignment(Alignment::Center),
        Rect::new(area.x, y, area.width, 1),
    );
}

/// How the terminal should track the mouse for the duration of
/// [`with_terminal_opts`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Wheel + button reporting only (SGR), *without* motion tracking. Safe
    /// inside a `tmux display-popup`: because no per-move sequences are ever
    /// emitted, none can leak into the popup or the pane beneath it. Used by the
    /// attach picker, which only needs the scroll wheel.
    Scroll,
    /// Full capture: buttons, drag, and motion. The sidebar needs this for
    /// drag-to-resize and for hover feedback on the resize handle.
    Full,
    /// No mouse reporting at all; the terminal keeps its own selection.
    None,
}

/// Everything [`with_terminal`] can set up besides raw mode and the alternate
/// screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TerminalOptions {
    pub mouse: MouseMode,
    /// Deliver pastes as one `Event::Paste` instead of a burst of key events.
    /// Without it, bracketed paste is turned off for the TUI's lifetime.
    pub bracketed_paste: bool,
}

// Scroll-wheel reporting without motion tracking: normal button tracking
// (`?1000`) plus SGR extended coordinates (`?1006`). Deliberately omits the
// any-motion mode (`?1003`) that crossterm's `EnableMouseCapture` also sets and
// that would flood a `display-popup` with per-move sequences.
const MOUSE_SCROLL_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const MOUSE_SCROLL_OFF: &[u8] = b"\x1b[?1006l\x1b[?1000l";

// Whether a pmux TUI currently owns the terminal (raw mode, alternate screen,
// mouse reporting). The panic hook and `TerminalGuard` restore only while set.
static TUI_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Whether leaving the TUI turns bracketed paste back on (the default for a
// TUI that disabled it) or off (for one that enabled it for its own use).
static PASTE_ON_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

// Best-effort reset of everything `with_terminal_opts` can turn on, for paths
// where the normal restore never runs. Every step is harmless if unneeded.
fn restore_terminal_best_effort() {
    use std::io::Write;
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = stdout.write_all(MOUSE_SCROLL_OFF);
    let _ = execute!(stdout, event::DisableMouseCapture);
    let _ = if PASTE_ON_EXIT.load(std::sync::atomic::Ordering::SeqCst) {
        execute!(stdout, event::EnableBracketedPaste)
    } else {
        execute!(stdout, event::DisableBracketedPaste)
    };
    let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
}

// The thread that owns the terminal while `TUI_ACTIVE` is set. Only a panic
// on that thread unwinds out of the TUI; a worker thread's panic must leave
// raw mode and the alternate screen alone while the UI keeps drawing.
static UI_THREAD: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);
// The most recent worker-thread panic seen while a TUI was active, for the UI
// to show as a status message (see [`take_worker_panic`]).
static WORKER_PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn lock_ignoring_poison<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn on_ui_thread() -> bool {
    *lock_ignoring_poison(&UI_THREAD) == Some(thread::current().id())
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let text = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    let thread = thread::current();
    let name = thread.name().unwrap_or("worker");
    match info.location() {
        Some(location) => format!("{name}: {text} ({}:{})", location.file(), location.line()),
        None => format!("{name}: {text}"),
    }
}

/// The most recent worker-thread panic recorded while a TUI owned the
/// terminal, cleared by this call. The UI shows it instead of letting the
/// default hook print into the alternate screen.
pub fn take_worker_panic() -> Option<String> {
    lock_ignoring_poison(&WORKER_PANIC).take()
}

// The default panic hook prints before unwinding, i.e. into the alternate
// screen, where the message vanishes and the shell is left in raw mode. Run
// the restore first so the user gets a usable terminal and sees the panic.
// A panic on any other thread while the TUI is up is recorded for the UI to
// report instead: restoring there would drop raw mode under a still-running
// UI, and printing would scribble over its screen.
fn install_panic_restore_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) && !on_ui_thread() {
                *lock_ignoring_poison(&WORKER_PANIC) = Some(panic_message(info));
                return;
            }
            if TUI_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
                restore_terminal_best_effort();
            }
            previous(info);
        }));
    });
}

/// Run `work` on a named background thread, catching a panic so the owner
/// learns the work will never finish: `on_panic` runs instead, e.g. to send a
/// fallback result or clear a "loading" flag. The panic hook has already
/// recorded the message for [`take_worker_panic`].
pub fn spawn_worker(
    name: &str,
    work: impl FnOnce() + Send + 'static,
    on_panic: impl FnOnce() + Send + 'static,
) {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
                on_panic();
            }
        })
        // Exactly what `thread::spawn` does when the OS refuses a thread.
        .expect("failed to spawn pmux TUI worker thread");
}

// Restores the terminal if `run` unwinds (or the process is otherwise leaving
// the TUI without the normal restore having cleared `TUI_ACTIVE`).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if TUI_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            restore_terminal_best_effort();
        }
    }
}

/// Enter raw mode + alt screen (tracking the mouse per `mouse`) for the duration
/// of `run`, restoring the terminal afterward on success, cancel, or error.
pub fn with_terminal_opts<T>(
    mouse: MouseMode,
    run: impl FnOnce(&mut AppTerminal) -> Result<T>,
) -> Result<T> {
    with_terminal(
        TerminalOptions {
            mouse,
            bracketed_paste: false,
        },
        run,
    )
}

/// [`with_terminal_opts`] with every option spelled out.
pub fn with_terminal<T>(
    options: TerminalOptions,
    run: impl FnOnce(&mut AppTerminal) -> Result<T>,
) -> Result<T> {
    use std::io::Write;
    install_panic_restore_hook();
    enable_raw_mode().context("enable raw mode for pmux TUI")?;
    let mut stdout = io::stdout();
    let enter = (|| -> io::Result<()> {
        execute!(stdout, EnterAlternateScreen)?;
        match options.mouse {
            MouseMode::Full => execute!(stdout, event::EnableMouseCapture)?,
            MouseMode::Scroll => stdout.write_all(MOUSE_SCROLL_ON)?,
            MouseMode::None => {}
        }
        if options.bracketed_paste {
            execute!(stdout, event::EnableBracketedPaste)?;
        } else {
            execute!(stdout, event::DisableBracketedPaste)?;
        }
        stdout.flush()
    })();
    if let Err(err) = enter {
        let _ = disable_raw_mode();
        return Err(err).context("enter pmux TUI screen");
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend).context("initialize pmux TUI") {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = disable_raw_mode();
            return Err(err);
        }
    };
    *lock_ignoring_poison(&UI_THREAD) = Some(thread::current().id());
    PASTE_ON_EXIT.store(
        !options.bracketed_paste,
        std::sync::atomic::Ordering::SeqCst,
    );
    TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    let guard = TerminalGuard;
    let result = run(&mut terminal);
    // Normal path: restore below and report its errors; the guard stands down.
    TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    drop(guard);
    let restore = (|| -> Result<()> {
        disable_raw_mode().context("disable raw mode for pmux TUI")?;
        let backend = terminal.backend_mut();
        match options.mouse {
            MouseMode::Full => execute!(backend, event::DisableMouseCapture)
                .context("restore terminal after pmux TUI")?,
            MouseMode::Scroll => backend
                .write_all(MOUSE_SCROLL_OFF)
                .context("restore terminal after pmux TUI")?,
            MouseMode::None => {}
        }
        if options.bracketed_paste {
            execute!(backend, event::DisableBracketedPaste)
        } else {
            execute!(backend, event::EnableBracketedPaste)
        }
        .context("restore terminal after pmux TUI")?;
        execute!(backend, LeaveAlternateScreen).context("restore terminal after pmux TUI")?;
        terminal.show_cursor().context("restore terminal cursor")?;
        Ok(())
    })();
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) | (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(restore_err)) => {
            Err(err.context(format!("also failed to restore terminal: {restore_err}")))
        }
    }
}

#[derive(Debug, Deserialize)]
struct StateEvent {
    state: crate::StateSnapshot,
}

/// Spawn `pmux events` and stream each state snapshot over a channel.
/// Shared by the sidebar and the live-updating picker. The returned child must
/// be killed by the caller when the TUI exits.
pub fn spawn_event_stream() -> Result<(Child, Receiver<crate::StateSnapshot>)> {
    let binary = crate::resolve_pmux_binary()?;
    let mut child = Command::new(binary)
        .args(["events", "--interval", "500ms"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start pmux events for TUI")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("pmux events did not expose stdout"))?;
    let (sender, receiver) = mpsc::channel();
    // A crash drops the sender, which the receiver sees as a closed stream.
    spawn_worker(
        "pmux-events",
        move || {
            use std::io::BufRead;
            let reader = io::BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(event) = serde_json::from_str::<StateEvent>(&line) else {
                    continue;
                };
                if sender.send(event.state).is_err() {
                    break;
                }
            }
        },
        || {},
    );
    Ok((child, receiver))
}

/// First delay before respawning an exited `pmux events` child;
/// doubled after each failed attempt up to [`EVENT_STREAM_MAX_BACKOFF`].
const EVENT_STREAM_MIN_BACKOFF: Duration = Duration::from_millis(500);
const EVENT_STREAM_MAX_BACKOFF: Duration = Duration::from_secs(10);

type EventStreamSpawner =
    Box<dyn FnMut() -> Result<(Option<Child>, Receiver<crate::StateSnapshot>)>>;

/// A `pmux events` feed that survives its child exiting: the child is
/// respawned with exponential backoff, and [`Self::is_down`] reports the gap
/// so the UI can say it is reconnecting instead of silently going stale.
pub struct EventStream {
    spawn: EventStreamSpawner,
    child: Option<Child>,
    receiver: Option<Receiver<crate::StateSnapshot>>,
    /// Consecutive spawns that ended without delivering a snapshot.
    failures: u32,
    /// Whether the current child has delivered a snapshot yet.
    delivered: bool,
    retry_at: Option<Instant>,
}

impl EventStream {
    /// Start following `pmux events`. A failure to spawn is not fatal: the
    /// stream starts down and retries on the usual backoff.
    pub fn start(now: Instant) -> Self {
        Self::with_spawner(
            Box::new(|| spawn_event_stream().map(|(child, receiver)| (Some(child), receiver))),
            now,
        )
    }

    fn with_spawner(spawn: EventStreamSpawner, now: Instant) -> Self {
        let mut stream = Self {
            spawn,
            child: None,
            receiver: None,
            failures: 0,
            delivered: false,
            retry_at: None,
        };
        stream.respawn(now);
        stream
    }

    /// Whether the feed is currently disconnected and waiting to respawn.
    pub fn is_down(&self) -> bool {
        self.receiver.is_none()
    }

    /// The newest snapshot received since the last call, if any, respawning a
    /// dead child once its backoff has elapsed. Older queued snapshots are
    /// skipped: only the latest state matters to a redraw.
    pub fn poll_latest(&mut self, now: Instant) -> Option<crate::StateSnapshot> {
        if self.receiver.is_none() && self.retry_at.is_none_or(|at| now >= at) {
            self.respawn(now);
        }
        let receiver = self.receiver.as_ref()?;
        let mut latest = None;
        loop {
            match receiver.try_recv() {
                Ok(snapshot) => latest = Some(snapshot),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected(now);
                    break;
                }
            }
        }
        if latest.is_some() {
            self.delivered = true;
            self.failures = 0;
        }
        latest
    }

    fn respawn(&mut self, now: Instant) {
        self.stop_child();
        self.delivered = false;
        match (self.spawn)() {
            Ok((child, receiver)) => {
                self.child = child;
                self.receiver = Some(receiver);
                self.retry_at = None;
            }
            Err(_) => self.schedule_retry(now),
        }
    }

    fn disconnected(&mut self, now: Instant) {
        self.receiver = None;
        self.stop_child();
        self.schedule_retry(now);
    }

    fn schedule_retry(&mut self, now: Instant) {
        // A child that worked for a while before exiting retries promptly; one
        // that keeps dying before its first snapshot backs off.
        if !self.delivered {
            self.failures = self.failures.saturating_add(1);
        }
        let backoff = EVENT_STREAM_MIN_BACKOFF
            .saturating_mul(1 << self.failures.saturating_sub(1).min(8))
            .min(EVENT_STREAM_MAX_BACKOFF);
        self.retry_at = Some(now + backoff);
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop_child();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AgentInfo, ProcessInfo};
    use crate::util::now_epoch;
    use ratatui::backend::TestBackend;

    fn process(name: &str, current: &str, launch: &str, attached: i64) -> SidebarProcess {
        let now = now_epoch();
        SidebarProcess {
            process: ProcessInfo {
                name: name.to_string(),
                pane_id: "%1".to_string(),
                lifecycle: "running".to_string(),
                created_at: now,
                last_activity_at: now,
                attached_clients: attached,
                current_command: "zsh".to_string(),
                start_command: None,
                current_path: Some(current.to_string()),
                launch_command: None,
                launch_cwd: Some(launch.to_string()),
                agent: None,
                startup: None,
                auto: None,
                spawned_by: None,
            },
            outer_location: None,
            outer_locations: Vec::new(),
        }
    }

    fn params<'a>() -> RowRenderParams<'a> {
        RowRenderParams {
            width: 80,
            highlight_color: None,
            drawer_outline: false,
            pill_text_color: Color::Black,
            active_pane_border_color: Color::White,
            agent_tag_column_width: 7,
            context: None,
            pill_colors: None,
            location: None,
            focused_process: false,
            flash_lit: false,
            attachment_flash_lit: false,
            prompt_label: None,
            agent_title: None,
            parent_note: None,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn empty_snapshot() -> crate::StateSnapshot {
        crate::StateSnapshot {
            api_version: crate::API_VERSION,
            generated_at: 0,
            sessions: Vec::new(),
        }
    }

    /// An event stream whose "children" are plain channels handed to the
    /// test, plus a count of spawns.
    fn fake_stream(
        now: Instant,
    ) -> (
        EventStream,
        std::rc::Rc<std::cell::RefCell<Vec<mpsc::Sender<crate::StateSnapshot>>>>,
    ) {
        let senders = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let handed = std::rc::Rc::clone(&senders);
        let stream = EventStream::with_spawner(
            Box::new(move || {
                let (tx, rx) = mpsc::channel();
                handed.borrow_mut().push(tx);
                Ok((None, rx))
            }),
            now,
        );
        (stream, senders)
    }

    #[test]
    fn event_stream_respawns_a_dead_child_with_backoff() {
        let start = Instant::now();
        let (mut stream, senders) = fake_stream(start);
        assert!(!stream.is_down());
        senders.borrow()[0].send(empty_snapshot()).unwrap();
        senders.borrow()[0].send(empty_snapshot()).unwrap();
        assert!(stream.poll_latest(start).is_some());
        assert!(stream.poll_latest(start).is_none());

        // The child exits: the stream reports it and waits before respawning.
        senders.borrow_mut().clear();
        assert!(stream.poll_latest(start).is_none());
        assert!(stream.is_down());
        assert!(
            stream
                .poll_latest(start + Duration::from_millis(100))
                .is_none()
        );
        assert_eq!(senders.borrow().len(), 0);
        let first_retry = start + EVENT_STREAM_MIN_BACKOFF;
        assert!(stream.poll_latest(first_retry).is_none());
        assert!(!stream.is_down());
        assert_eq!(senders.borrow().len(), 1);

        // Replacements that keep dying before delivering anything back off.
        senders.borrow_mut().clear();
        stream.poll_latest(first_retry);
        assert!(stream.is_down());
        let second_retry = first_retry + EVENT_STREAM_MIN_BACKOFF;
        stream.poll_latest(second_retry);
        assert!(!stream.is_down());
        senders.borrow_mut().clear();
        stream.poll_latest(second_retry);
        stream.poll_latest(second_retry + EVENT_STREAM_MIN_BACKOFF);
        assert!(stream.is_down(), "a repeat failure waits twice as long");
        let third_retry = second_retry + EVENT_STREAM_MIN_BACKOFF * 2;
        stream.poll_latest(third_retry);
        assert!(!stream.is_down());
        // A delivering child resets the backoff.
        senders.borrow()[0].send(empty_snapshot()).unwrap();
        assert!(stream.poll_latest(third_retry).is_some());
        assert_eq!(stream.failures, 0);
    }

    #[test]
    fn event_stream_retries_a_failed_spawn() {
        let start = Instant::now();
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = std::rc::Rc::clone(&attempts);
        let mut stream = EventStream::with_spawner(
            Box::new(move || {
                counted.set(counted.get() + 1);
                anyhow::bail!("no binary")
            }),
            start,
        );
        assert!(stream.is_down());
        stream.poll_latest(start);
        assert_eq!(attempts.get(), 1);
        stream.poll_latest(start + EVENT_STREAM_MIN_BACKOFF);
        assert_eq!(attempts.get(), 2);
        // Capped exponential backoff.
        stream.poll_latest(start + EVENT_STREAM_MIN_BACKOFF * 2);
        assert_eq!(attempts.get(), 2);
        stream.poll_latest(start + EVENT_STREAM_MIN_BACKOFF * 3);
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn a_panicking_worker_reports_instead_of_unwinding_the_process() {
        let (tx, rx) = mpsc::channel();
        spawn_worker(
            "pmux-test-worker",
            || panic!("simulated worker failure"),
            move || tx.send("recovered").unwrap(),
        );
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok("recovered")
        );
    }

    #[test]
    fn auto_named_process_shows_its_agent_title_in_an_italic_pill() {
        let titled = RowRenderParams {
            agent_title: Some("Fix the relay"),
            ..params()
        };
        let auto = process("1790869355-vurcl60h", "/srv", "/srv", 0);
        let rows = render_process_row(&auto, &titled);
        let title_span = rows[0]
            .spans
            .iter()
            .find(|span| span.content == "Fix the relay")
            .expect("title pill");
        assert!(title_span.style.add_modifier.contains(Modifier::ITALIC));

        // A user-chosen name always wins over the agent's title.
        let named = process("web", "/srv", "/srv", 0);
        let text = line_text(&render_process_row(&named, &titled)[0]);
        assert!(text.contains("web"));
        assert!(!text.contains("Fix the relay"));
    }

    #[test]
    fn resumable_row_shows_title_agent_age_and_directory() {
        let entry = crate::HistoryEntry {
            agent: "codex".to_string(),
            session_id: "01a0".to_string(),
            title: Some("Sidebar groups".to_string()),
            title_source: None,
            first_prompt: None,
            cwd: Some("/srv/app".to_string()),
            cwd_exists: true,
            git_branch: None,
            started_epoch: None,
            last_activity_epoch: now_epoch() - 7200,
            transcript: String::new(),
            live: false,
            pmux_process: None,
            resume_command: None,
        };
        let lines = render_resumable_row(&entry, None, 60, None, Color::Black);
        assert_eq!(lines.len(), RESUMABLE_ROW_LINES);
        assert_eq!(line_text(&lines[0]), "┌ ↻ \u{e0b6}Sidebar groups\u{e0b4}");
        // The title sits in an italic pill, as a live row's derived title does.
        let pill = &lines[0].spans[3].style;
        assert_eq!(pill.bg, Some(session_name_pill_color("01a0")));
        assert_eq!(pill.fg, Some(Color::Black));
        assert!(pill.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(line_text(&lines[1]), "└ codex · 2h ago · /srv/app");
        let lines = render_resumable_row(&entry, Some("12p"), 60, None, Color::Black);
        assert_eq!(line_text(&lines[1]), "└ codex · 2h ago · 12p · /srv/app");

        let gone = crate::HistoryEntry {
            cwd_exists: false,
            ..entry
        };
        let lines = render_resumable_row(&gone, None, 60, None, Color::Black);
        assert!(line_text(&lines[1]).ends_with("/srv/app (gone)"));
        assert_eq!(lines[0].spans[3].style.bg, Some(Color::DarkGray));
    }

    #[test]
    fn row_has_name_cwd_and_status_lines_without_a_distinct_launch_cwd() {
        let rows = render_process_row(&process("web", "/srv", "/srv", 0), &params());
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.len(),
            process_line_count(&process("web", "/srv", "/srv", 0), false)
        );
        // The name keeps the top line; the CWD sits beneath it, aligned with
        // the name pill, and the status line comes last.
        assert!(line_text(&rows[0]).contains("web"));
        assert!(!line_text(&rows[0]).contains("/srv"));
        // The second line leads with the agent/process tag (padded to the
        // shared tag column) before the CWD; the status line no longer repeats it.
        assert_eq!(
            line_text(&rows[1]),
            format!("{}   process /srv", RowOutline::default().side_detached)
        );
        assert!(line_text(&rows[2]).starts_with("└── running "));
    }

    #[test]
    fn the_prompt_count_follows_the_cwd_before_the_attachment_detail() {
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let mut attached = process("web", "/srv", "/srv", 1);
        attached.outer_location = Some(OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "main".to_string(),
        });
        let mut render_params = params();
        render_params.width = 42;
        render_params.context = Some(&context);
        render_params.prompt_label = Some("12p");
        let second = line_text(&render_process_row(&attached, &render_params)[1]);
        assert!(second.contains("/srv · 12p"), "{second:?}");
        assert!(second.ends_with("pane 3 "), "{second:?}");
        assert_eq!(text_width(&second), 42);
    }

    #[test]
    fn a_keyboard_flash_lights_only_the_arrow_and_attachment_detail() {
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let mut attached = process("web", "/srv", "/srv", 1);
        attached.outer_location = Some(OuterPaneLocation {
            pane_id: "%2".to_string(),
            pane_index: "3".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "main".to_string(),
        });
        let mut render_params = params();
        render_params.width = 42;
        render_params.context = Some(&context);
        let fg_of = |rows: &[Line<'static>], line: usize, text: &str| {
            rows[line]
                .spans
                .iter()
                .find(|span| span.content.as_ref() == text)
                .and_then(|span| span.style.fg)
        };

        render_params.attachment_flash_lit = true;
        let rows = render_process_row(&attached, &render_params);
        assert_eq!(fg_of(&rows, 0, "→"), Some(DRAG_HANDLE_COLOR));
        assert_eq!(fg_of(&rows, 1, "pane 3"), Some(DRAG_HANDLE_COLOR));
        assert_ne!(fg_of(&rows, 1, "/srv"), Some(DRAG_HANDLE_COLOR));
        assert_ne!(
            rows[0].spans[0].style.fg,
            Some(DRAG_HANDLE_COLOR),
            "outline"
        );

        // A pointer flash lights the whole row, outline and CWD included.
        render_params.attachment_flash_lit = false;
        render_params.flash_lit = true;
        let rows = render_process_row(&attached, &render_params);
        assert_eq!(fg_of(&rows, 1, "/srv"), Some(DRAG_HANDLE_COLOR));
        assert_eq!(rows[0].spans[0].style.fg, Some(DRAG_HANDLE_COLOR));
    }

    #[test]
    fn a_long_name_is_truncated_before_the_attachment_arrow() {
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let mut render_params = params();
        render_params.width = 42;
        render_params.context = Some(&context);
        let long = process("sidebar-width-cap-validation-session", "/srv", "/srv", 0);
        let rows = render_process_row(&long, &render_params);
        let top = line_text(&rows[0]);
        assert_eq!(text_width(&top), 42);
        assert!(top.ends_with(" ◯ "), "{top:?}");
        assert!(top.contains('…'), "{top:?}");
    }

    #[test]
    fn row_adds_a_launched_in_line_when_cwd_differs() {
        let rows = render_process_row(&process("web", "/srv/now", "/srv/then", 0), &params());
        assert_eq!(rows.len(), 4);
        assert!(line_text(&rows[0]).starts_with('┌'));
        assert!(line_text(&rows[1]).contains("/srv/now"));
        assert!(line_text(&rows[2]).starts_with(&RowOutline::default().side_detached));
        assert!(line_text(&rows[3]).starts_with('└'));
        assert!(line_text(&rows[3]).contains("Launched in:"));
        assert!(line_text(&rows[3]).ends_with('─'));
        assert_eq!(text_width(&line_text(&rows[3])), 80);
    }

    #[test]
    fn dashed_fill_is_right_anchored_to_a_full_chunk() {
        // Whatever the width parity, the fill spans exactly the requested
        // columns and ends on the pattern's final glyph (a complete dash at the
        // right edge); the partial chunk, if any, is on the left.
        for cols in [10usize, 11, 12, 13] {
            let s = fill_to_width(&RowOutline::default().rule_detached, cols);
            assert_eq!(text_width(&s), cols, "cols={cols}");
            assert!(s.ends_with('─'), "cols={cols}: {s:?}");
        }
        assert_eq!(fill_to_width(&RowOutline::default().rule, 4), "────");
    }

    #[test]
    fn pill_colors_are_distinct_until_the_palette_is_exhausted() {
        // Distinct names, fewer than the palette size => all distinct colors.
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let procs: Vec<SidebarProcess> = names.iter().map(|n| process(n, "/x", "/x", 0)).collect();
        let map = assign_pill_colors(&procs);
        let distinct: std::collections::HashSet<_> = map.values().collect();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn pill_color_assignment_is_oldest_wins() {
        // Find two names that hash to the same palette slot.
        let (mut a, mut b) = (String::new(), String::new());
        'search: for i in 0..2000u32 {
            for j in (i + 1)..2000u32 {
                let (x, y) = (format!("s{i}"), format!("s{j}"));
                if session_name_pill_index(&x) == session_name_pill_index(&y) {
                    a = x;
                    b = y;
                    break 'search;
                }
            }
        }
        assert!(!a.is_empty(), "expected to find a colliding name pair");
        let base = SESSION_NAME_PILL_COLORS[session_name_pill_index(&a)];
        let mk = |name: &str, ts: i64| {
            let mut p = process(name, "/x", "/x", 0);
            p.process.created_at = ts;
            p
        };

        // `a` is older; slice order is intentionally newest-first to prove the
        // assignment sorts by created_at, not slice position.
        let map = assign_pill_colors(&[mk(&b, 200), mk(&a, 100)]);
        assert_eq!(map[&a], base, "older process keeps its hashed color");
        assert_ne!(map[&b], base, "newer process yields to a different color");

        // Adding the newcomer must not recolor the older process.
        let solo = assign_pill_colors(&[mk(&a, 100)]);
        assert_eq!(solo[&a], map[&a]);
    }

    #[test]
    fn closed_rows_use_outline_corners_and_a_right_anchored_bottom_rule() {
        let lines = render_process_row(&process("web", "/srv", "/srv", 0), &params());
        assert_eq!(lines[0].spans[0].content.as_ref(), "┌");
        assert_eq!(
            lines[1].spans[0].content.as_ref(),
            RowOutline::default().side_detached
        );
        assert_eq!(lines[2].spans[0].content.as_ref(), "└");
        assert!(line_text(&lines[2]).starts_with("└── running "));
        assert!(line_text(&lines[2]).ends_with('─'));
        assert_eq!(text_width(&line_text(&lines[2])), 80);
    }

    #[test]
    fn sidebar_dashed_borders_share_a_column_phase_and_final_glyph() {
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let mut render_params = params();
        render_params.width = 60;
        render_params.agent_tag_column_width = text_width("claude");
        render_params.context = Some(&context);
        let mut claude = process("1772262306-9rtuxxxx", "/srv/web", "/srv/web", 0);
        claude.process.created_at = now_epoch() - 34 * 24 * 60 * 60;
        claude.process.last_activity_at = claude.process.created_at;
        claude.process.agent = Some(AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        let lines = render_process_row(&claude, &render_params);
        let top: Vec<char> = line_text(&lines[0]).chars().collect();
        let bottom_text = line_text(&lines[2]);
        let bottom: Vec<char> = bottom_text.chars().collect();

        assert_eq!(top.len(), 60);
        assert_eq!(bottom.len(), 60);
        assert_eq!(&top[57..], &[' ', '◯', ' ']);
        assert_eq!(&bottom[58..], &['─', ' ']);
        assert!(line_text(&lines[1]).contains("claude"));
        // At most a one-cell gap plus the dash pattern's leading half-chunk
        // separates the status from the right-anchored bottom rule.
        assert!(
            bottom_text.starts_with("└── idle · 34d old "),
            "{bottom_text:?}"
        );
        assert!(!bottom_text.contains("34d old   "), "{bottom_text:?}");
        // The bottom rule ends on the top row's attachment-glyph column. Its
        // two-cell dash cadence therefore agrees with the top rule everywhere
        // the two trailing fills overlap.
        assert_eq!(&top[48..=56], &bottom[48..=56]);
    }

    #[test]
    fn drawer_outline_continues_through_the_process_header() {
        let mut outlined = params();
        outlined.drawer_outline = true;
        let lines = render_process_row(&process("web", "/srv", "/srv", 0), &outlined);
        assert!(line_text(&lines[0]).starts_with("┌ ●"));
        assert!(line_text(&lines[1]).starts_with("╎ "));
        assert_eq!(lines[0].spans[0].style.fg, Some(MUTED_OUTLINE_COLOR));
        assert_eq!(lines[1].spans[0].style.fg, Some(MUTED_OUTLINE_COLOR));

        let attached = render_process_row(&process("web", "/srv", "/srv", 1), &outlined);
        assert!(line_text(&attached[1]).starts_with("│ "));
    }

    #[test]
    fn only_the_focused_process_outline_keeps_the_active_border_color() {
        let orange = Color::Rgb(255, 136, 0);
        assert_eq!(outline_color(true, orange), orange);
        assert_eq!(outline_color(false, orange), MUTED_OUTLINE_COLOR);
    }

    #[test]
    fn selected_drawer_keeps_its_outline_color_and_highlight_to_the_bottom() {
        let process = process("web", "/srv", "/srv", 0);
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let rows = [&process];
        let pill_colors = assign_pill_colors(std::slice::from_ref(&process));
        let backend = TestBackend::new(16, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let highlight_color = Some(CURSOR_HIGHLIGHT_COLOR);
                let params = RowRenderParams {
                    width: 16,
                    highlight_color,
                    drawer_outline: true,
                    pill_text_color: Color::Black,
                    active_pane_border_color: Color::White,
                    agent_tag_column_width: agent_tag_column_width(rows.iter().copied()),
                    context: Some(&context),
                    pill_colors: Some(&pill_colors),
                    location: None,
                    focused_process: false,
                    flash_lit: false,
                    attachment_flash_lit: false,
                    prompt_label: None,
                    agent_title: None,
                    parent_note: None,
                };
                let drawer = vec![
                    Line::from("│ details"),
                    Line::from(format!("└{}", "─".repeat(15))),
                ];
                for (offset, line) in render_process_block(&process, &params, drawer)
                    .into_iter()
                    .enumerate()
                {
                    render_line(
                        frame,
                        Rect::new(0, 0, 16, 8),
                        1 + u16::try_from(offset).unwrap(),
                        line,
                        highlight_color,
                    );
                }
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for (y, edge) in [(1, "┌"), (2, "╎"), (3, "╎"), (4, "╎"), (5, "└")] {
            assert_eq!(buffer.cell((0, y)).unwrap().symbol(), edge);
            assert_eq!(buffer.cell((0, y)).unwrap().fg, MUTED_OUTLINE_COLOR);
            assert_eq!(buffer.cell((15, y)).unwrap().bg, CURSOR_HIGHLIGHT_COLOR);
        }
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "●");
        assert_eq!(buffer.cell((1, 4)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((1, 5)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((2, 5)).unwrap().symbol(), "─");
        assert_eq!(buffer.cell((14, 5)).unwrap().symbol(), "─");
        assert_eq!(buffer.cell((15, 5)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((2, 5)).unwrap().fg, MUTED_OUTLINE_COLOR);
    }

    #[test]
    fn picker_mode_shows_client_count_and_no_attachment_arrow() {
        let rows = render_process_row(&process("web", "/srv", "/srv", 2), &params());
        // Client count is right-pinned on the second line, after the CWD.
        assert!(
            rows[1]
                .spans
                .iter()
                .any(|span| span.content.as_ref() == "2 attached")
        );
        assert!(line_text(&rows[1]).ends_with("2 attached "));
        assert_eq!(text_width(&line_text(&rows[1])), 80);
        // No outer-tmux attachment arrow anywhere without a context.
        for line in &rows {
            let text = line_text(line);
            for arrow in ["→", "←", "↗", "↖", "⧉"] {
                assert!(
                    !text.contains(arrow),
                    "unexpected arrow {arrow:?} in {text:?}"
                );
            }
        }
    }

    #[test]
    fn agent_row_uses_the_agent_kind_as_its_tag() {
        let mut agent_process = process("api", "/srv", "/srv", 0);
        agent_process.process.agent = Some(AgentInfo {
            kind: "codex".to_string(),
            state: Some("working".to_string()),
            updated_at: Some(now_epoch()),
            source: Some("ai-attn".to_string()),
            reason: None,
            session_id: None,
        });
        let rows = render_process_row(&agent_process, &params());
        assert!(line_text(&rows[1]).contains("codex"));
        assert!(line_text(&rows[2]).contains("working"));
        assert!(!line_text(&rows[2]).contains("codex"));
    }

    /// One golden case: a label, the process, and how to draw it.
    struct GoldenCase {
        label: &'static str,
        process: SidebarProcess,
        width: u16,
        context: Option<SidebarContext>,
        location: Option<OuterPaneLocation>,
        prompt_label: Option<&'static str>,
        agent_title: Option<&'static str>,
        drawer_outline: bool,
        highlight: bool,
        focused: bool,
        flash_lit: bool,
        attachment_flash_lit: bool,
    }

    fn golden_context(sidebar_on_right: bool) -> SidebarContext {
        SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right,
        }
    }

    fn golden_location(window_id: &str) -> OuterPaneLocation {
        OuterPaneLocation {
            pane_id: "%9".to_string(),
            pane_index: "2".to_string(),
            window_id: window_id.to_string(),
            window_index: "3".to_string(),
            window_name: "api".to_string(),
        }
    }

    /// A process 90 minutes old whose agent (if any) changed state 20
    /// minutes ago: ages that read the same on any run.
    fn golden_process(
        name: &str,
        agent: Option<(&str, &str)>,
        launch: Option<&str>,
    ) -> SidebarProcess {
        let now = now_epoch();
        let mut process = process(
            name,
            "/home/demo/code/api",
            launch.unwrap_or("/home/demo/code/api"),
            0,
        );
        process.process.created_at = now - 90 * 60;
        process.process.last_activity_at = now - 20 * 60;
        process.process.agent = agent.map(|(kind, state)| AgentInfo {
            kind: kind.to_string(),
            state: Some(state.to_string()),
            updated_at: Some(now - 20 * 60),
            source: Some("ai-attn".to_string()),
            reason: None,
            session_id: None,
        });
        process
    }

    fn golden_cases() -> Vec<GoldenCase> {
        let base = |label, process| GoldenCase {
            label,
            process,
            width: 42,
            context: Some(golden_context(false)),
            location: None,
            prompt_label: None,
            agent_title: None,
            drawer_outline: false,
            highlight: false,
            focused: false,
            flash_lit: false,
            attachment_flash_lit: false,
        };
        let auto = "1785337162-9vl2hy3f";
        let attached = |mut case: GoldenCase, window: &str| {
            case.process.process.attached_clients = 1;
            case.process.outer_location = Some(golden_location(window));
            case.process.outer_locations = vec![golden_location(window)];
            case
        };
        let mut cases = vec![
            base("plain unattached", golden_process("dev-server", None, None)),
            attached(
                base(
                    "plain in this window",
                    golden_process("dev-server", None, None),
                ),
                "@1",
            ),
            attached(
                base(
                    "plain in another window",
                    golden_process("dev-server", None, None),
                ),
                "@7",
            ),
            {
                let mut case = base(
                    "plain in another session",
                    golden_process("dev-server", None, None),
                );
                case.process.process.attached_clients = 2;
                case
            },
            {
                let mut case = attached(
                    base(
                        "claude working, prompts",
                        golden_process("loader", Some(("claude", "working")), None),
                    ),
                    "@1",
                );
                case.prompt_label = Some("12p");
                case
            },
            {
                let mut case = base(
                    "codex waiting, auto-named with title",
                    golden_process(auto, Some(("codex", "waiting")), None),
                );
                case.agent_title = Some("Add rate limiting to the upload endpoint");
                case.prompt_label = Some("3p");
                case
            },
            base(
                "auto-named without title",
                golden_process(auto, Some(("claude", "done")), None),
            ),
            base(
                "moved since launch",
                golden_process("web", None, Some("/home/demo/code/web")),
            ),
            {
                let mut case = base(
                    "moved since launch, drawer open",
                    golden_process("web", None, Some("/home/demo/code/web")),
                );
                case.drawer_outline = true;
                case
            },
            {
                let mut case = attached(
                    base(
                        "drawer open in this window",
                        golden_process("loader", Some(("claude", "working")), None),
                    ),
                    "@1",
                );
                case.drawer_outline = true;
                case
            },
            {
                let mut case = attached(
                    base(
                        "selected and focused",
                        golden_process("loader", Some(("claude", "working")), None),
                    ),
                    "@1",
                );
                case.highlight = true;
                case.focused = true;
                case
            },
            {
                let mut case = attached(
                    base("pointer flash", golden_process("loader", None, None)),
                    "@7",
                );
                case.flash_lit = true;
                case
            },
            {
                let mut case = attached(
                    base("keyboard flash", golden_process("loader", None, None)),
                    "@7",
                );
                case.attachment_flash_lit = true;
                case
            },
            {
                let mut case = attached(
                    base("sidebar on the right", golden_process("loader", None, None)),
                    "@7",
                );
                case.context = Some(golden_context(true));
                case
            },
            {
                let mut case = attached(
                    base(
                        "second attachment row",
                        golden_process("loader", None, None),
                    ),
                    "@1",
                );
                case.location = Some(golden_location("@7"));
                case
            },
            {
                let mut case = base(
                    "picker, unattached",
                    golden_process("dev-server", None, None),
                );
                case.context = None;
                case.width = 80;
                case
            },
            {
                let mut case = base(
                    "picker, attached twice",
                    golden_process("loader", Some(("claude", "working")), None),
                );
                case.context = None;
                case.width = 80;
                case.process.process.attached_clients = 2;
                case.prompt_label = Some("7p");
                case
            },
            {
                let mut case = base(
                    "very long name",
                    golden_process("an-extremely-long-process-name-for-truncation", None, None),
                );
                case.width = 30;
                case
            },
        ];
        for width in [24, 16, 10] {
            let mut case = attached(
                base(
                    "narrow",
                    golden_process(
                        "loader-refactor",
                        Some(("claude", "working")),
                        Some("/home/demo/elsewhere"),
                    ),
                ),
                "@7",
            );
            case.width = width;
            case.prompt_label = Some("12p");
            cases.push(case);
        }
        cases
    }

    /// One row of a golden family: the process, its place in the tree, and
    /// the parent it names.
    type FamilyRow = (SidebarProcess, RowTree, Option<&'static str>);

    /// The family drawn with its second row under the cursor.
    const SELECTED_CHILD_FAMILY: &str = "selected child";
    /// The family drawn with its parent under the cursor.
    const SELECTED_PARENT_FAMILY: &str = "selected parent";

    /// Families as the sidebar lists them, each drawn with every preset.
    fn golden_families() -> Vec<(&'static str, Vec<FamilyRow>)> {
        let child = |depth: usize, guides: &[bool], last: bool, has_children: bool| RowTree {
            depth,
            guides: guides.to_vec(),
            last,
            has_children,
        };
        let root = |has_children: bool| RowTree {
            has_children,
            ..RowTree::default()
        };
        let mut lead = golden_process("lead", Some(("claude", "working")), None);
        lead.process.attached_clients = 1;
        lead.outer_location = Some(golden_location("@1"));
        lead.outer_locations = vec![golden_location("@1")];
        let helper = || golden_process("helper", Some(("codex", "done")), None);
        let tests = || golden_process("tests", None, None);
        vec![
            (
                "family",
                vec![
                    (lead.clone(), root(true), None),
                    (helper(), child(1, &[], false, false), None),
                    (tests(), child(1, &[], true, false), None),
                ],
            ),
            (
                "nested grandchild",
                vec![
                    (lead.clone(), root(true), None),
                    (helper(), child(1, &[], false, true), None),
                    (
                        golden_process("helper-lint", Some(("claude", "working")), None),
                        child(2, &[true], true, false),
                        None,
                    ),
                    (tests(), child(1, &[], true, false), None),
                ],
            ),
            (
                "remote parent",
                vec![(
                    golden_process("sidebar lineage", Some(("claude", "working")), None),
                    root(false),
                    Some("lineage ios on macbook"),
                )],
            ),
            (
                "unnamed parent",
                vec![
                    (
                        golden_process("1774574118-qz0tmz4y", Some(("claude", "idle")), None),
                        root(true),
                        None,
                    ),
                    (
                        golden_process("ts-docs", Some(("claude", "idle")), None),
                        child(1, &[], true, false),
                        None,
                    ),
                ],
            ),
            (
                SELECTED_PARENT_FAMILY,
                vec![
                    (lead.clone(), root(true), None),
                    (tests(), child(1, &[], true, false), None),
                ],
            ),
            (
                SELECTED_CHILD_FAMILY,
                vec![
                    (lead.clone(), root(true), None),
                    (helper(), child(1, &[], false, false), None),
                    (tests(), child(1, &[], true, false), None),
                ],
            ),
            (
                "waiting child promoted to the top",
                vec![
                    (
                        golden_process("helper", Some(("codex", "waiting")), None),
                        root(false),
                        Some("lead"),
                    ),
                    (lead, root(true), None),
                    (tests(), child(1, &[], true, false), None),
                ],
            ),
        ]
    }

    /// `rows` as the sidebar draws them at `width`: each row narrowed by
    /// its place in the tree, joined by spacers carrying the spine.
    fn golden_family_lines(
        label: &str,
        rows: &[FamilyRow],
        layout: &RowLayout,
        width: u16,
    ) -> Vec<Line<'static>> {
        let context = golden_context(false);
        let pill_colors = HashMap::new();
        let mut lines = Vec::new();
        for (index, (process, tree, note)) in rows.iter().enumerate() {
            if index > 0 {
                lines.push(tree_spacer(Some(tree), &layout.outline));
            }
            let selected = (label == SELECTED_CHILD_FAMILY && index == 1)
                || (label == SELECTED_PARENT_FAMILY && index == 0);
            let params = RowRenderParams {
                width: width - tree.width() as u16,
                highlight_color: selected.then_some(CURSOR_HIGHLIGHT_COLOR),
                drawer_outline: false,
                pill_text_color: Color::Black,
                active_pane_border_color: Color::Indexed(208),
                agent_tag_column_width: 6,
                context: Some(&context),
                pill_colors: Some(&pill_colors),
                location: None,
                focused_process: false,
                flash_lit: false,
                attachment_flash_lit: false,
                prompt_label: None,
                agent_title: None,
                parent_note: *note,
            };
            lines.extend(tree_block(
                render_row(process, &params, layout),
                tree,
                &layout.outline,
            ));
        }
        lines
    }

    /// Each line as runs of one style, `[text]` then the non-default style
    /// parts. Adjacent spans of one style merge, so how a renderer splits
    /// its spans does not matter, only what reaches the screen.
    fn golden_line(line: &Line<'_>) -> String {
        let mut runs: Vec<(String, Style)> = Vec::new();
        for span in &line.spans {
            if span.content.is_empty() {
                continue;
            }
            match runs.last_mut() {
                Some((text, style)) if *style == span.style => text.push_str(&span.content),
                _ => runs.push((span.content.to_string(), span.style)),
            }
        }
        runs.iter()
            .map(|(content, style)| {
                let style = *style;
                let mut parts = Vec::new();
                if let Some(fg) = style.fg {
                    parts.push(format!("fg={fg}"));
                }
                if let Some(bg) = style.bg {
                    parts.push(format!("bg={bg}"));
                }
                if !style.add_modifier.is_empty() {
                    parts.push(format!("+{:?}", style.add_modifier));
                }
                if parts.is_empty() {
                    format!("[{content}]")
                } else {
                    format!("[{content}]{{{}}}", parts.join(","))
                }
            })
            .collect()
    }

    /// Every row variant pmux draws, recorded span for span. A change to
    /// the file is a visible change to rows: review it, then regenerate with
    /// `PMUX_UPDATE_GOLDEN=1 cargo test golden`.
    #[test]
    fn process_rows_match_the_golden_file() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/testdata/process_rows.golden");
        let mut out = String::new();
        for case in golden_cases() {
            let pill_colors = HashMap::new();
            let params = RowRenderParams {
                width: case.width,
                highlight_color: case.highlight.then_some(CURSOR_HIGHLIGHT_COLOR),
                drawer_outline: case.drawer_outline,
                pill_text_color: Color::Black,
                active_pane_border_color: Color::Indexed(208),
                agent_tag_column_width: 6,
                context: case.context.as_ref(),
                pill_colors: Some(&pill_colors),
                location: case.location.as_ref(),
                focused_process: case.focused,
                flash_lit: case.flash_lit,
                attachment_flash_lit: case.attachment_flash_lit,
                prompt_label: case.prompt_label,
                agent_title: case.agent_title,
                parent_note: None,
            };
            out.push_str(&format!("## {} (width {})\n", case.label, case.width));
            for line in render_process_row(&case.process, &params) {
                out.push_str(&line_text(&line));
                out.push('\n');
            }
            for line in render_process_row(&case.process, &params) {
                out.push_str(&golden_line(&line));
                out.push('\n');
            }
            out.push('\n');
        }
        for (label, rows) in golden_families() {
            for (preset, _) in RowLayout::PRESETS {
                let layout = RowLayout::preset(preset).unwrap();
                let lines = golden_family_lines(label, &rows, &layout, 42);
                out.push_str(&format!("## {label}, {preset} preset (width 42)\n"));
                for line in &lines {
                    out.push_str(&line_text(line));
                    out.push('\n');
                }
                for line in &lines {
                    out.push_str(&golden_line(line));
                    out.push('\n');
                }
                out.push('\n');
            }
        }
        if std::env::var_os("PMUX_UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &out).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        if expected != out {
            let first = expected
                .lines()
                .zip(out.lines())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| expected.lines().count().min(out.lines().count()));
            panic!(
                "process rows differ from {} at line {}:\n  expected: {:?}\n  actual:   {:?}\n\
                 Review the change, then regenerate with PMUX_UPDATE_GOLDEN=1.",
                path.display(),
                first + 1,
                expected.lines().nth(first),
                out.lines().nth(first),
            );
        }
    }
}
