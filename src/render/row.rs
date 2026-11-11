//! Process rows as lines of tokens.
//!
//! A [`RowLayout`] says which values ([`RowToken`]s) go on which line of a
//! process row, left-aligned or pinned right. [`render_row`] draws it in
//! pmux's frame: a box outline (`┌`, `│`, `└`, dashed while unattached), a
//! status dot in the first line's gutter, a rule from the first line's
//! content to its right edge, and a closing rule under the last line unless
//! a drawer continues the box.
//!
//! Each token draws itself only when it has a value. A line is left out only
//! when the process itself rules out every token on it (no launch directory
//! to show, say), so a row keeps its height while values such as its prompt
//! count load. When a line is too narrow, its flexible token (the title, the
//! CWD, the launch directory) shrinks to [`MIN_FLEXIBLE_WIDTH`]; then the
//! pinned-right tokens go, then tokens from the right end of the line; and
//! finally the line is cut short, so nothing runs past the row's edge.

use super::*;

/// A process's age (`3h old`) is left off its status line until it ran this
/// long before its state last changed, so a fresh process shows only its
/// state and how long ago it changed.
const LIFETIME_HIDDEN_FOR_SECS: i64 = 10 * 60;

/// One value a process row can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowToken {
    /// The colored status dot. First on a line, it sits in the gutter.
    StateDot,
    /// The user's name for the process, else its agent's conversation title
    /// (italic), in a colored pill. Flexible.
    Title,
    /// The agent kind, or `process`, padded to a column shared by every row.
    Agent,
    /// The working directory, `~`-shortened. Flexible.
    Cwd,
    /// The agent's prompt count, such as `12p`.
    Prompts,
    /// Where the process is attached: `pane 2`, `window 3`, `2 attached`.
    Location,
    /// The arrow toward where it is attached (the sidebar only).
    Arrow,
    /// The agent state, or `running`.
    Status,
    /// How long ago the agent state changed: `20m ago`.
    Age,
    /// How long the process has run: `1h old`; left out when it would say
    /// the same as [`RowToken::Age`].
    Lifetime,
    /// `Launched in: …` when the process has moved since. Flexible.
    LaunchedIn,
    /// `from lead`, the process that spawned this one, when the row is not
    /// drawn under it: its parent is on another machine (`from lead on
    /// macbook`), or it waits on the user and is listed at the top. Flexible.
    Parent,
}

impl RowToken {
    /// Every token, as config names them.
    pub const ALL: [(RowToken, &'static str); 12] = [
        (RowToken::StateDot, "state_dot"),
        (RowToken::Title, "title"),
        (RowToken::Agent, "agent"),
        (RowToken::Cwd, "cwd"),
        (RowToken::Prompts, "prompts"),
        (RowToken::Location, "location"),
        (RowToken::Arrow, "arrow"),
        (RowToken::Status, "status"),
        (RowToken::Age, "age"),
        (RowToken::Lifetime, "lifetime"),
        (RowToken::LaunchedIn, "launched_in"),
        (RowToken::Parent, "parent"),
    ];

    /// The token config calls `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, known)| *known == name)
            .map(|(token, _)| *token)
    }

    /// Whether `process` can have a value for this token, `noted` when its
    /// row names its parent. Most values come and go as the row's state
    /// changes (a prompt count once parsed, an age once signalled); only
    /// these depend on the process and where its row is listed.
    fn may_show(self, process: &SidebarProcess, noted: bool) -> bool {
        match self {
            RowToken::Cwd => crate::sidebar_process_cwd(process).is_some(),
            RowToken::LaunchedIn => launch_cwd_text(process).is_some(),
            RowToken::Parent => noted,
            _ => true,
        }
    }
}

/// What a color can be set for: a token, or the ` · ` between two tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorTarget {
    Token(RowToken),
    Separator,
}

/// One line of a row: tokens from the left, and tokens pinned right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLine {
    pub left: Vec<RowToken>,
    pub right: Vec<RowToken>,
    /// Columns between the outline and the content.
    pub indent: usize,
}

impl RowLine {
    pub fn new(left: &[RowToken], right: &[RowToken]) -> Self {
        // `Launched in: …` starts right after the closing rule's first cell:
        // `└─ Launched in: …`.
        let indent = if left.first() == Some(&RowToken::LaunchedIn) {
            2
        } else {
            3
        };
        Self {
            left: left.to_vec(),
            right: right.to_vec(),
            indent,
        }
    }

    /// Whether this line is drawn for `process`; see [`RowToken::may_show`].
    fn shown_for(&self, process: &SidebarProcess, noted: bool) -> bool {
        self.left
            .iter()
            .chain(&self.right)
            .any(|token| token.may_show(process, noted))
    }
}

/// The glyphs of a row's box: its corners and edge, and the rule drawn from
/// the first and last lines' content to the right edge. Each corner and edge
/// is one column wide; a rule is a pattern of one-column glyphs, tiled. A
/// process attached nowhere draws its edge and rules detached (dashed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowOutline {
    pub top: String,
    pub bottom: String,
    pub side: String,
    pub side_detached: String,
    pub rule: String,
    pub rule_detached: String,
}

impl RowOutline {
    /// The outline styles config can name.
    pub const STYLES: &'static [&'static str] = &["box", "rounded", "heavy", "ascii", "none"];

    /// The outline style config calls `name`.
    pub fn style(name: &str) -> Option<Self> {
        let glyphs = match name {
            "box" => ["┌", "└", "│", "╎", "─", " ─"],
            "rounded" => ["╭", "╰", "│", "╎", "─", " ─"],
            "heavy" => ["┏", "┗", "┃", "╏", "━", " ━"],
            "ascii" => ["+", "+", "|", ":", "-", " -"],
            "none" => [" ", " ", " ", " ", " ", " "],
            _ => return None,
        };
        let [top, bottom, side, side_detached, rule, rule_detached] = glyphs.map(String::from);
        Some(Self {
            top,
            bottom,
            side,
            side_detached,
            rule,
            rule_detached,
        })
    }

    /// The edge between the corners.
    pub fn side(&self, detached: bool) -> &str {
        if detached {
            &self.side_detached
        } else {
            &self.side
        }
    }

    /// The rule after the first and last lines' content.
    pub fn rule(&self, detached: bool) -> &str {
        if detached {
            &self.rule_detached
        } else {
            &self.rule
        }
    }
}

impl Default for RowOutline {
    fn default() -> Self {
        Self::style("box").expect("the box style exists")
    }
}

/// Which tokens go on which line of a process row, any colors that replace
/// their own, and the glyphs of its box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLayout {
    pub lines: Vec<RowLine>,
    pub colors: HashMap<ColorTarget, Color>,
    pub outline: RowOutline,
    /// Whether the title sits in a colored pill. Without one it is bold text
    /// in the row's own color.
    pub pills: bool,
}

impl RowLayout {
    /// pmux's own rows: the name, then the agent and where it runs, then its
    /// state and ages, then where it was launched when that differs, then
    /// the process that spawned it when the row is not under it.
    pub fn pmux() -> Self {
        use RowToken::*;
        Self {
            lines: vec![
                RowLine::new(&[StateDot, Title], &[Arrow]),
                RowLine::new(&[Agent, Cwd, Prompts], &[Location]),
                RowLine::new(&[Status, Age, Lifetime], &[]),
                RowLine::new(&[LaunchedIn], &[]),
                RowLine::new(&[Parent], &[]),
            ],
            colors: HashMap::new(),
            outline: RowOutline::default(),
            pills: true,
        }
    }

    /// The presets config can name, each with what it shows.
    pub const PRESETS: &'static [(&'static str, &'static str)] = &[
        (
            "pmux",
            "four lines in a box: name, agent and directory, state and ages, launch directory",
        ),
        (
            "compact",
            "two lines in a box: name, then agent, state, and where it is attached",
        ),
        (
            "herdr",
            "two lines with no box or pills, like herdr's sidebar: name and window, then agent and state",
        ),
    ];

    /// The preset config calls `name`.
    pub fn preset(name: &str) -> Option<Self> {
        use RowToken::*;
        match name {
            "pmux" => Some(Self::pmux()),
            "compact" => Some(Self {
                lines: vec![
                    RowLine::new(&[StateDot, Title], &[Arrow]),
                    RowLine::new(&[Agent, Status, Age], &[Location]),
                    RowLine::new(&[Parent], &[]),
                ],
                ..Self::pmux()
            }),
            "herdr" => Some(Self {
                lines: vec![
                    RowLine::new(&[StateDot, Title], &[Location]),
                    RowLine::new(&[Agent, Status, Age], &[]),
                    RowLine::new(&[Parent], &[]),
                ],
                outline: RowOutline::style("none")?,
                pills: false,
                ..Self::pmux()
            }),
            _ => None,
        }
    }

    /// How many lines [`render_row`] draws for `process`, `noted` when its
    /// row names its parent ([`RowRenderParams::parent_note`]).
    pub fn line_count(&self, process: &SidebarProcess, noted: bool) -> usize {
        self.lines
            .iter()
            .filter(|line| line.shown_for(process, noted))
            .count()
    }
}

static LAYOUT: std::sync::OnceLock<RowLayout> = std::sync::OnceLock::new();

/// The layout process rows are drawn with: pmux's own unless
/// [`set_row_layout`] chose another.
pub fn row_layout() -> &'static RowLayout {
    LAYOUT.get_or_init(RowLayout::pmux)
}

/// Draw process rows with `layout` from now on. The sidebar and the picker
/// each run in their own process and call this once, before drawing; a later
/// call has no effect.
pub fn set_row_layout(layout: RowLayout) {
    let _ = LAYOUT.set(layout);
}

/// The narrowest a flexible token shrinks to before whole tokens are dropped
/// to make room.
pub const MIN_FLEXIBLE_WIDTH: usize = 8;

/// A token's drawn form.
#[derive(Debug, Clone)]
struct Piece {
    token: RowToken,
    /// Text that never shrinks, before `text` (`Launched in: `).
    prefix: &'static str,
    text: String,
    style: Style,
    /// The pill around a title: its color, drawn as rounded caps.
    pill: Option<Color>,
}

impl Piece {
    fn new(token: RowToken, text: impl Into<String>, style: Style) -> Self {
        Self {
            token,
            prefix: "",
            text: text.into(),
            style,
            pill: None,
        }
    }

    fn flexible(&self) -> bool {
        matches!(
            self.token,
            RowToken::Title | RowToken::Cwd | RowToken::LaunchedIn | RowToken::Parent
        )
    }

    /// Columns besides the shrinkable text.
    fn fixed_width(&self) -> usize {
        text_width(self.prefix) + if self.pill.is_some() { 2 } else { 0 }
    }

    fn width(&self) -> usize {
        self.fixed_width() + text_width(&self.text)
    }

    fn spans(&self, highlight: Option<Color>) -> Vec<Span<'static>> {
        match self.pill {
            Some(color) => vec![
                Span::styled("\u{e0b6}", Style::default().fg(color)),
                Span::styled(self.text.clone(), self.style),
                Span::styled("\u{e0b4}", Style::default().fg(color)),
            ],
            None => vec![Span::styled(
                format!("{}{}", self.prefix, self.text),
                content_style(self.style, highlight),
            )],
        }
    }
}

/// What a row shows and how it is colored, gathered once per row.
struct RowValues<'a> {
    params: &'a RowRenderParams<'a>,
    scope: AttachmentScope,
    sidebar: bool,
    outline_style: Style,
    outline: &'a RowOutline,
    pieces: Vec<Piece>,
    separators: Separators,
    /// Some line of the layout shows the directory.
    cwd_shown: bool,
}

impl<'a> RowValues<'a> {
    fn new(
        process: &'a SidebarProcess,
        params: &'a RowRenderParams<'a>,
        layout: &'a RowLayout,
    ) -> Self {
        let highlight = params.highlight_color;
        let (agent_tag, agent_tag_color, status, status_style, status_at) = process_badges(process);
        let location = params.location.or(process.outer_location.as_ref());
        // Colors describe the process as a whole; the arrow and detail
        // describe this row's attachment, which differ when it is attached
        // in two windows.
        let scope = match params.context {
            Some(context) => attachment_scope(process, context),
            None => attachment_scope_contextless(process),
        };
        let row_scope = match params.context {
            Some(context) => attachment_scope_at(process, location, context),
            None => scope,
        };
        let (first_line_color, outline_color) = if params.flash_lit {
            (DRAG_HANDLE_COLOR, DRAG_HANDLE_COLOR)
        } else {
            (
                first_line_color(
                    scope,
                    params.focused_process,
                    params.active_pane_border_color,
                ),
                outline_color(params.focused_process, params.active_pane_border_color),
            )
        };
        // The arrow and `pane N` / `window N` detail also blink alone for a
        // keyboard-cursor flash, which leaves the rest of the row steady.
        let attachment_color = if params.attachment_flash_lit {
            DRAG_HANDLE_COLOR
        } else {
            first_line_color
        };
        let tag_column = params.agent_tag_column_width.max(1);

        let mut pieces = vec![
            Piece::new(RowToken::StateDot, "●", status_style),
            Piece::new(
                RowToken::Agent,
                crate::truncate_col(agent_tag, tag_column),
                Style::default().fg(agent_tag_color),
            ),
        ];
        // A user-chosen name wins; an auto-named process borrows its agent's
        // conversation title, drawn italic to mark it as derived.
        let user_name = crate::display_session_name(&process.process.name);
        if let Some(name) = user_name.or(params.agent_title) {
            let (pill, style) = if layout.pills {
                let color = params
                    .pill_colors
                    .and_then(|colors| colors.get(&process.process.name).copied())
                    .unwrap_or_else(|| session_name_pill_color(&process.process.name));
                (
                    Some(color),
                    session_name_pill_style(color, params.pill_text_color),
                )
            } else {
                (
                    None,
                    Style::default()
                        .fg(first_line_color)
                        .add_modifier(Modifier::BOLD),
                )
            };
            let style = if user_name.is_none() {
                style.add_modifier(Modifier::ITALIC)
            } else {
                style
            };
            pieces.push(Piece {
                pill,
                ..Piece::new(RowToken::Title, name, style)
            });
        }
        if let Some(cwd) = crate::sidebar_process_cwd(process) {
            pieces.push(Piece::new(
                RowToken::Cwd,
                crate::display_path(cwd),
                Style::default()
                    .fg(first_line_color)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(label) = params.prompt_label {
            pieces.push(Piece::new(
                RowToken::Prompts,
                label,
                Style::default().fg(Color::Cyan),
            ));
        }
        if let Some(detail) = attachment_detail(process, location, params.context) {
            pieces.push(Piece::new(
                RowToken::Location,
                detail,
                Style::default().fg(attachment_color),
            ));
        }
        if let Some(icon) = params
            .context
            .and_then(|context| attachment_icon(row_scope, context.sidebar_on_right))
        {
            pieces.push(Piece::new(
                RowToken::Arrow,
                icon,
                Style::default().fg(attachment_color),
            ));
        }
        let pane_age = crate::relative_age(process.process.created_at);
        let signal_age = status_at.map(crate::relative_age);
        pieces.push(Piece::new(RowToken::Status, status, status_style));
        if let Some(age) = signal_age.as_deref() {
            pieces.push(Piece::new(
                RowToken::Age,
                format!("{age} ago"),
                status_style,
            ));
        }
        // How long the process ran before its state last changed (or before
        // now, without a state time). A state change within the first
        // minutes says enough about its age on its own.
        let ran_before_status =
            status_at.unwrap_or_else(crate::util::now_epoch) - process.process.created_at;
        if ran_before_status > LIFETIME_HIDDEN_FOR_SECS
            && signal_age.as_deref() != Some(pane_age.as_str())
        {
            pieces.push(Piece::new(
                RowToken::Lifetime,
                format!("{pane_age} old"),
                status_style,
            ));
        }
        if let Some(launch) = launch_cwd_text(process) {
            pieces.push(Piece {
                prefix: "Launched in: ",
                ..Piece::new(
                    RowToken::LaunchedIn,
                    crate::display_path(launch),
                    Style::default().fg(Color::DarkGray),
                )
            });
        }
        if let Some(parent) = params.parent_note {
            pieces.push(Piece {
                prefix: "from ",
                ..Piece::new(
                    RowToken::Parent,
                    parent,
                    Style::default().fg(Color::DarkGray),
                )
            });
        }

        for piece in &mut pieces {
            // A flash recolors the row, which a configured color must not
            // hide.
            let flashing = params.flash_lit
                || (params.attachment_flash_lit
                    && matches!(piece.token, RowToken::Location | RowToken::Arrow));
            let color = layout.colors.get(&ColorTarget::Token(piece.token));
            let Some(&color) = color.filter(|_| !flashing) else {
                continue;
            };
            if piece.pill.is_some() {
                piece.pill = Some(color);
                piece.style = piece.style.bg(color);
            } else {
                piece.style = piece.style.fg(color);
            }
        }

        let outline_style = content_style(Style::default().fg(outline_color), highlight);
        Self {
            params,
            scope,
            sidebar: params.context.is_some(),
            outline_style,
            outline: &layout.outline,
            pieces,
            cwd_shown: layout.lines.iter().any(|line| {
                line.left
                    .iter()
                    .chain(&line.right)
                    .any(|t| *t == RowToken::Cwd)
            }),
            separators: Separators {
                tag_column,
                color: layout
                    .colors
                    .get(&ColorTarget::Separator)
                    .copied()
                    .unwrap_or(Color::DarkGray),
            },
        }
    }

    fn piece(&self, token: RowToken) -> Option<&Piece> {
        self.pieces.iter().find(|piece| piece.token == token)
    }

    /// The pieces for `tokens` this process has. A row with no name or
    /// agent title shows its directory in the title's place, unless the
    /// layout shows the directory elsewhere, so it is never a bare dot.
    fn present(&self, tokens: &[RowToken]) -> Vec<Piece> {
        tokens
            .iter()
            .filter_map(|token| {
                self.piece(*token)
                    .or_else(|| {
                        (*token == RowToken::Title && !self.cwd_shown)
                            .then(|| self.piece(RowToken::Cwd))
                            .flatten()
                    })
                    .cloned()
            })
            .collect()
    }
}

/// How left-side pieces are joined.
#[derive(Debug, Clone, Copy)]
struct Separators {
    /// The agent column's width, shared by every row.
    tag_column: usize,
    /// The ` · ` color between pieces of different styles.
    color: Color,
}

impl Separators {
    /// The text between two left-side pieces: the agent column's padding,
    /// else ` · `, dim unless both pieces share a style (`working · 20m ago`).
    fn between(self, before: &Piece, after: &Piece) -> (String, Option<Style>) {
        if before.token == RowToken::Agent {
            let padding = self.tag_column.saturating_sub(text_width(&before.text));
            return (" ".repeat(padding + 1), None);
        }
        let style = if before.style == after.style && before.pill.is_none() && after.pill.is_none()
        {
            before.style
        } else {
            Style::default().fg(self.color)
        };
        (PROMPT_SEPARATOR.to_string(), Some(style))
    }

    /// The width of `pieces` joined.
    fn total(self, pieces: &[Piece]) -> usize {
        let separators: usize = pieces
            .windows(2)
            .map(|pair| text_width(&self.between(&pair[0], &pair[1]).0))
            .sum();
        pieces.iter().map(Piece::width).sum::<usize>() + separators
    }
}

/// Draw `process` with `layout`: the lines [`render_process_row`] returns.
pub fn render_row(
    process: &SidebarProcess,
    params: &RowRenderParams<'_>,
    layout: &RowLayout,
) -> Vec<Line<'static>> {
    let values = RowValues::new(process, params, layout);
    let noted = params.parent_note.is_some();
    let lines: Vec<(&RowLine, Vec<Piece>, Vec<Piece>)> = layout
        .lines
        .iter()
        .filter(|line| line.shown_for(process, noted))
        .map(|line| {
            (
                line,
                values.present(&line.left),
                values.present(&line.right),
            )
        })
        .collect();
    let count = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(index, (line, left, right))| {
            let position = LinePosition {
                top: index == 0,
                bottom: index + 1 == count && !params.drawer_outline,
            };
            Line::from(draw_line(&values, line, left, right, position))
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct LinePosition {
    top: bool,
    /// The last line of a box no drawer continues: it carries the closing
    /// rule.
    bottom: bool,
}

fn draw_line(
    values: &RowValues<'_>,
    line: &RowLine,
    mut left: Vec<Piece>,
    right: Vec<Piece>,
    position: LinePosition,
) -> Vec<Span<'static>> {
    let width = usize::from(values.params.width);
    let highlight = values.params.highlight_color;
    let ruled = position.top || position.bottom;
    let bar_style = values.outline_style;

    // The outline and gutter.
    let outline = values.outline;
    let detached = values.scope == AttachmentScope::Unattached;
    let edge = if position.top {
        &outline.top
    } else if position.bottom {
        &outline.bottom
    } else {
        outline.side(detached)
    };
    let mut spans = vec![Span::styled(edge.to_string(), values.outline_style)];
    let dot_in_gutter = left
        .first()
        .is_some_and(|piece| piece.token == RowToken::StateDot);
    if dot_in_gutter {
        let dot = left.remove(0);
        spans.push(Span::raw(" "));
        spans.extend(dot.spans(highlight));
        spans.push(Span::raw(" ".repeat(line.indent.saturating_sub(2))));
    } else if position.bottom && !position.top {
        // A solid continuation of the closing rule up to the content.
        spans.push(Span::styled(
            fill_to_width(&outline.rule, line.indent.saturating_sub(1)),
            bar_style,
        ));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw(" ".repeat(line.indent)));
    }
    let gutter = 1 + line.indent;
    let pad = usize::from(values.sidebar);
    let right_width = |right: &[Piece]| {
        right.iter().map(Piece::width).sum::<usize>() + right.len().saturating_sub(1)
    };
    // Room for the left pieces: what the right side and rules leave.
    let room = |left: &[Piece], right: &[Piece]| {
        let reserve = if ruled {
            // A gap before the rule, the rule itself (at least one cell on the
            // closing line), the right pieces after a gap, and the sidebar's
            // trailing cell.
            let gap = usize::from(!left.is_empty());
            let min_rule = usize::from(position.bottom);
            let right_block = if right.is_empty() {
                0
            } else {
                1 + right_width(right)
            };
            gap + min_rule
                + right_block
                + if right.is_empty() && !position.bottom {
                    0
                } else {
                    pad
                }
        } else if right.is_empty() {
            // The sidebar's trailing cell, which every line leaves clear.
            pad
        } else {
            // At least one space, the right pieces, and a trailing cell.
            right_width(right) + 2
        };
        width.saturating_sub(gutter + reserve)
    };
    let (left, right) = fit_line(left, right, room, values.separators);
    let right_width = right_width(&right);
    let mut used = gutter;
    let mut previous: Option<&Piece> = None;
    for piece in &left {
        if let Some(before) = previous {
            let (text, style) = values.separators.between(before, piece);
            used += text_width(&text);
            spans.push(match style {
                Some(style) => Span::styled(text, content_style(style, highlight)),
                None => Span::raw(text),
            });
        }
        used += piece.width();
        spans.extend(piece.spans(highlight));
        previous = Some(piece);
    }

    let right_spans = |spans: &mut Vec<Span<'static>>| {
        for (index, piece) in right.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            spans.extend(piece.spans(highlight));
        }
    };
    if ruled {
        let gap = usize::from(!left.is_empty());
        let right_block = if right.is_empty() { 0 } else { 1 + right_width };
        let trailing = if right.is_empty() && !position.bottom {
            0
        } else {
            pad
        };
        let rule = width.saturating_sub(used + gap + right_block + trailing);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(
            fill_to_width(outline.rule(detached), rule),
            bar_style,
        ));
        if !right.is_empty() {
            spans.push(Span::raw(" "));
            right_spans(&mut spans);
        }
        spans.push(Span::raw(" ".repeat(trailing)));
    } else if !right.is_empty() {
        let gap = width.saturating_sub(used + right_width + 1);
        spans.push(Span::raw(" ".repeat(gap)));
        right_spans(&mut spans);
        spans.push(Span::raw(" "));
    }
    spans
}

/// Fit a line's pieces into the room `room(left, right)` leaves for the
/// left ones: shrink the flexible piece to [`MIN_FLEXIBLE_WIDTH`], then drop
/// pinned-right pieces from the end, then left pieces from the end (keeping
/// the first), and finally [`fit`] what is left.
fn fit_line(
    mut left: Vec<Piece>,
    mut right: Vec<Piece>,
    room: impl Fn(&[Piece], &[Piece]) -> usize,
    separators: Separators,
) -> (Vec<Piece>, Vec<Piece>) {
    loop {
        if let Some(fitted) = shrink_to_fit(&left, room(&left, &right), separators) {
            return (fitted, right);
        }
        if right.pop().is_none() && (left.len() <= 1 || left.pop().is_none()) {
            break;
        }
    }
    let room = room(&left, &right);
    (fit(left, room, separators), right)
}

/// `pieces` within `room`, shrinking the flexible piece no narrower than
/// [`MIN_FLEXIBLE_WIDTH`]; `None` when that is not enough.
fn shrink_to_fit(pieces: &[Piece], room: usize, separators: Separators) -> Option<Vec<Piece>> {
    if separators.total(pieces) <= room {
        return Some(pieces.to_vec());
    }
    let index = pieces.iter().position(Piece::flexible)?;
    let mut fitted = pieces.to_vec();
    fitted[index].text.clear();
    let budget = room.checked_sub(separators.total(&fitted))?;
    if budget < MIN_FLEXIBLE_WIDTH {
        return None;
    }
    fitted[index].text = crate::truncate_col(&pieces[index].text, budget);
    Some(fitted)
}

/// Fit `pieces` into `room` columns: shrink the flexible piece (dropping it
/// when nothing of it fits), then cut the line short if it is still too
/// long.
fn fit(mut pieces: Vec<Piece>, room: usize, separators: Separators) -> Vec<Piece> {
    let total = |pieces: &[Piece]| separators.total(pieces);
    if total(&pieces) <= room {
        return pieces;
    }
    if let Some(index) = pieces.iter().position(Piece::flexible) {
        let others = {
            let mut without = pieces.clone();
            without[index].text.clear();
            total(&without)
        };
        let budget = room.saturating_sub(others);
        let piece = &mut pieces[index];
        if budget == 0 && piece.prefix.is_empty() {
            pieces.remove(index);
        } else {
            piece.text = crate::truncate_col(&piece.text, budget);
        }
    }
    if total(&pieces) <= room {
        return pieces;
    }
    // Still too wide: keep whole pieces while they fit, and end on the
    // first that does not, cut short.
    let mut kept: Vec<Piece> = Vec::new();
    for mut piece in pieces {
        let mut candidate = kept.clone();
        candidate.push(piece.clone());
        if total(&candidate) <= room {
            kept = candidate;
            continue;
        }
        let used = total(&kept)
            + kept
                .last()
                .map_or(0, |last| text_width(&separators.between(last, &piece).0));
        // The cut may reach into a fixed prefix (`Launched in: `).
        let left = room.saturating_sub(used + piece.fixed_width() - text_width(piece.prefix));
        // A lone `…` after a separator says nothing; only the first piece
        // shrinks that far.
        if left > 1 || (left == 1 && kept.is_empty()) {
            piece.text = crate::truncate_col(&format!("{}{}", piece.prefix, piece.text), left);
            piece.prefix = "";
            kept.push(piece);
        }
        break;
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AgentInfo, ProcessInfo};
    use crate::util::now_epoch;

    fn agent_process(updated_ago: Option<i64>, created_ago: i64) -> SidebarProcess {
        let now = now_epoch();
        SidebarProcess {
            process: ProcessInfo {
                name: "loader".to_string(),
                pane_id: "%1".to_string(),
                lifecycle: "running".to_string(),
                created_at: now - created_ago,
                last_activity_at: now,
                attached_clients: 0,
                current_command: "claude".to_string(),
                start_command: None,
                current_path: Some("/srv".to_string()),
                launch_command: None,
                launch_cwd: Some("/srv".to_string()),
                agent: Some(AgentInfo {
                    kind: "claude".to_string(),
                    state: Some("waiting".to_string()),
                    updated_at: updated_ago.map(|ago| now - ago),
                    source: Some("ai-attn".to_string()),
                    reason: None,
                    session_id: None,
                }),
                startup: None,
                auto: None,
                spawned_by: None,
            },
            outer_location: None,
            outer_locations: Vec::new(),
        }
    }

    fn params(width: u16) -> RowRenderParams<'static> {
        RowRenderParams {
            width,
            highlight_color: None,
            drawer_outline: false,
            pill_text_color: Color::Black,
            active_pane_border_color: Color::White,
            agent_tag_column_width: 6,
            context: None,
            pill_colors: None,
            location: None,
            focused_process: false,
            flash_lit: false,
            attachment_flash_lit: false,
            prompt_label: Some("12p"),
            agent_title: None,
            parent_note: None,
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn only_layouts_with_pills_draw_the_title_in_a_colored_pill() {
        let process = agent_process(Some(180), 3 * 3600);
        let title_line = |layout: &RowLayout| render_row(&process, &params(60), layout).remove(0);

        let pmux = title_line(&RowLayout::pmux());
        assert!(
            text(&pmux).contains("\u{e0b6}loader\u{e0b4}"),
            "{}",
            text(&pmux)
        );
        assert!(pmux.spans.iter().any(|span| span.style.bg.is_some()));

        let herdr = RowLayout::preset("herdr").unwrap();
        assert!(!herdr.pills);
        let line = title_line(&herdr);
        assert!(text(&line).contains(" loader "), "{}", text(&line));
        assert!(!text(&line).contains('\u{e0b6}'), "{}", text(&line));
        let title = line
            .spans
            .iter()
            .find(|span| span.content.contains("loader"))
            .unwrap();
        assert_eq!(title.style.bg, None);
        assert!(title.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_lifetime_shows_only_for_a_process_that_ran_ten_minutes_before_its_state() {
        let status =
            |process: &SidebarProcess| text(&render_row(process, &params(60), row_layout())[2]);
        assert!(status(&agent_process(Some(180), 200)).contains("waiting · 3m ago "));
        // Started 14 minutes ago, waiting since 3 minutes ago: 11 minutes in.
        assert!(status(&agent_process(Some(180), 14 * 60)).contains("waiting · 3m ago · 14m old"));
        // Started 12 minutes ago, waiting since 3 minutes ago: 9 minutes in.
        assert!(!status(&agent_process(Some(180), 12 * 60)).contains(" old"));
        assert!(status(&agent_process(Some(180), 3 * 3600)).contains("waiting · 3m ago · 3h old"));
        assert!(status(&agent_process(None, 3 * 3600)).contains("waiting · 3h old"));
        assert!(!status(&agent_process(None, 5 * 60)).contains(" old"));
    }

    #[test]
    fn no_line_runs_past_the_row_at_any_width() {
        let mut moved = agent_process(Some(180), 3 * 3600);
        moved.process.launch_cwd = Some("/somewhere/else/entirely".to_string());
        moved.process.name = "a-rather-long-process-name".to_string();
        let context = SidebarContext {
            socket: "/tmp/outer".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        for sidebar in [false, true] {
            for width in 6..=60 {
                let mut params = params(width);
                params.context = sidebar.then_some(&context);
                for line in render_row(&moved, &params, row_layout()) {
                    let line = text(&line);
                    assert!(
                        text_width(&line) <= usize::from(width),
                        "width {width}, sidebar {sidebar}: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_rows_height_follows_its_layout_and_not_values_still_loading() {
        use RowToken::*;
        let mut process = agent_process(Some(180), 3 * 3600);
        assert_eq!(RowLayout::pmux().line_count(&process, false), 3);
        process.process.launch_cwd = Some("/elsewhere".to_string());
        assert_eq!(RowLayout::pmux().line_count(&process, false), 4);

        let layout = RowLayout {
            lines: vec![
                RowLine::new(&[StateDot, Title], &[]),
                RowLine::new(&[Prompts], &[]),
            ],
            colors: HashMap::new(),
            outline: RowOutline::default(),
            pills: true,
        };
        let mut params = params(40);
        params.prompt_label = None;
        // The prompt count has not loaded, but its line keeps its place.
        assert_eq!(layout.line_count(&process, false), 2);
        assert_eq!(render_row(&process, &params, &layout).len(), 2);
    }

    #[test]
    fn colors_from_the_layout_replace_a_tokens_own() {
        let mut layout = RowLayout::pmux();
        layout
            .colors
            .insert(ColorTarget::Token(RowToken::Prompts), Color::Indexed(117));
        layout.colors.insert(ColorTarget::Separator, Color::Red);
        layout
            .colors
            .insert(ColorTarget::Token(RowToken::Title), Color::Magenta);
        let lines = render_row(&agent_process(Some(180), 3 * 3600), &params(60), &layout);
        let span = |line: usize, content: &str| {
            lines[line]
                .spans
                .iter()
                .find(|span| span.content == content)
                .unwrap_or_else(|| panic!("no {content:?} in {:?}", text(&lines[line])))
                .style
        };
        assert_eq!(span(1, "12p").fg, Some(Color::Indexed(117)));
        assert_eq!(span(1, PROMPT_SEPARATOR).fg, Some(Color::Red));
        assert_eq!(span(0, "loader").bg, Some(Color::Magenta));
        assert_eq!(span(0, "\u{e0b6}").fg, Some(Color::Magenta));

        // A flash still shows through.
        let mut flashing = params(60);
        flashing.flash_lit = true;
        let lines = render_row(&agent_process(Some(180), 3 * 3600), &flashing, &layout);
        let prompts = lines[1].spans.iter().find(|span| span.content == "12p");
        assert_ne!(prompts.unwrap().style.fg, Some(Color::Indexed(117)));
    }

    #[test]
    fn a_narrow_line_drops_pinned_tokens_then_tokens_from_its_right_end() {
        use RowToken::*;
        let layout = RowLayout {
            lines: vec![RowLine::new(&[Status, Age, Lifetime], &[Prompts])],
            colors: HashMap::new(),
            outline: RowOutline::default(),
            pills: true,
        };
        let process = agent_process(Some(180), 3 * 3600);
        let line = |width| text(&render_row(&process, &params(width), &layout)[0]);
        assert!(line(40).starts_with("┌   waiting · 3m ago · 3h old "));
        assert!(line(40).trim_end().ends_with(" 12p"));
        // `12p`, pinned right, goes first, then `3h old`, then `3m ago`.
        assert!(line(34).starts_with("┌   waiting · 3m ago · 3h old "));
        assert!(!line(34).contains("12p"));
        assert!(line(30).starts_with("┌   waiting · 3m ago ─"));
        assert!(line(18).starts_with("┌   waiting  ─"));
    }

    #[test]
    fn the_outline_glyphs_come_from_the_layout() {
        let mut layout = RowLayout::pmux();
        layout.outline = RowOutline::style("ascii").unwrap();
        // Attached nowhere, so its edge and rules are the detached ones.
        let lines = render_row(&agent_process(Some(180), 3 * 3600), &params(40), &layout);
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert!(texts[0].starts_with("+ ● "), "{texts:?}");
        assert!(texts[0].trim_end().ends_with(" -"), "{texts:?}");
        assert!(texts[1].starts_with(":   "), "{texts:?}");
        assert!(texts[2].starts_with("+-- "), "{texts:?}");
        assert!(!texts.concat().contains(['┌', '└', '│', '╎', '─']));

        let mut drawer = Line::from(vec![Span::raw("└──────")]);
        outline_drawer_line(&mut drawer, &layout.outline, true, 1);
        assert_eq!(text(&drawer), "+- - - ");
        let mut drawer = Line::from(vec![Span::raw("│ details")]);
        outline_drawer_line(&mut drawer, &layout.outline, false, 0);
        assert_eq!(text(&drawer), "| details");
    }
}
