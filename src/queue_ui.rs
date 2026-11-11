//! Prefix popup for collecting and scheduling one combined prompt.
use crate::queue::{self, Batch, Edit, Status};
use crate::render;
use crate::util::now_epoch;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

type Screen = render::AppTerminal;

/// Shared palette, matching the sidebar: cyan accents, dark-gray chrome, red errors.
const ACCENT: Color = Color::Cyan;
const CHROME: Color = Color::DarkGray;
const KEY: Color = Color::Yellow;
const BODY: Color = Color::Gray;

fn panel<'a>(title: impl Into<String>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CHROME))
        .title(Span::styled(
            title.into(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}
fn status_color(status: Status) -> Color {
    match status {
        Status::Draft => Color::Gray,
        Status::Scheduled => Color::Green,
        Status::Paused => Color::Yellow,
        Status::Sending => Color::Cyan,
        Status::Sent => Color::Blue,
        Status::Held => Color::Magenta,
        Status::Uncertain => Color::Red,
        Status::Cancelled => Color::DarkGray,
    }
}
/// Render `key label · key label ...` with the key glyphs highlighted.
fn hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(CHROME)));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(KEY).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(BODY)));
    }
    Line::from(spans)
}

#[derive(Default)]
struct Editor {
    chars: Vec<char>,
    cursor: usize,
}
impl Editor {
    fn new(text: &str) -> Self {
        let chars: Vec<_> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn insert(&mut self, text: &str) {
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            if !c.is_control() || c == '\n' || c == '\t' {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
        }
    }
    fn start(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
    }
    fn end(&self) -> usize {
        self.chars[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.chars.len(), |i| self.cursor + i)
    }
    fn vertical(&mut self, down: bool) {
        let start = self.start();
        let column = self.cursor - start;
        if down {
            let next = self.end() + 1;
            if next > self.chars.len() {
                return;
            }
            let end = self.chars[next..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(self.chars.len(), |i| next + i);
            self.cursor = (next + column).min(end);
        } else if start > 0 {
            let end = start - 1;
            let previous = self.chars[..end]
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |i| i + 1);
            self.cursor = (previous + column).min(end);
        }
    }
}
fn editor(screen: &mut Screen, title: &str, initial: &str) -> Result<Option<String>> {
    let mut editor = Editor::new(initial);
    loop {
        screen.draw(|frame| {
            let [body, help] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(frame.area());
            let block = panel(title);
            let inner = block.inner(body);
            let row = editor.chars[..editor.cursor]
                .iter()
                .filter(|c| **c == '\n')
                .count();
            let prefix: String = editor.chars[editor.start()..editor.cursor].iter().collect();
            let column = UnicodeWidthStr::width(prefix.replace('\t', "    ").as_str());
            let top = row.saturating_sub(inner.height.saturating_sub(1) as usize);
            let left = column.saturating_sub(inner.width.saturating_sub(1) as usize);
            frame.render_widget(
                Paragraph::new(editor.text().replace('\t', "    "))
                    .style(Style::default().fg(Color::White))
                    .block(block)
                    .scroll((
                        top.min(u16::MAX as usize) as u16,
                        left.min(u16::MAX as usize) as u16,
                    )),
                body,
            );
            if inner.width > 0 && inner.height > 0 {
                frame.set_cursor_position((
                    inner.x + (column - left) as u16,
                    inner.y + (row - top) as u16,
                ));
            }
            let keys = hints(&[
                ("Esc", "Save & close"),
                ("Ctrl-c", "Discard"),
                ("Enter", "Newline"),
                ("\u{2191}\u{2193}\u{2190}\u{2192}", "Move"),
            ]);
            let note = Line::from(Span::styled(
                "Nothing is sent until you pick a time with t.",
                Style::default().fg(CHROME),
            ));
            frame.render_widget(Paragraph::new(vec![keys, note]), help);
        })?;
        match event::read()? {
            Event::Paste(text) => editor.insert(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                // Esc backs out keeping the text; only Ctrl-c discards. Ctrl-s is
                // unusable here because it is the outer tmux prefix.
                KeyCode::Esc => return Ok(Some(editor.text())),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Enter => editor.insert("\n"),
                KeyCode::Backspace if editor.cursor > 0 => {
                    editor.cursor -= 1;
                    editor.chars.remove(editor.cursor);
                }
                KeyCode::Delete if editor.cursor < editor.chars.len() => {
                    editor.chars.remove(editor.cursor);
                }
                KeyCode::Left => editor.cursor = editor.cursor.saturating_sub(1),
                KeyCode::Right => editor.cursor = (editor.cursor + 1).min(editor.chars.len()),
                KeyCode::Home => editor.cursor = editor.start(),
                KeyCode::End => editor.cursor = editor.end(),
                KeyCode::Up => editor.vertical(false),
                KeyCode::Down => editor.vertical(true),
                KeyCode::Tab => editor.insert("    "),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    editor.insert(&c.to_string())
                }
                _ => {}
            },
            _ => {}
        }
    }
}
/// Human-readable gap between now and the chosen time.
fn relative(delta: i64) -> String {
    if delta <= 0 {
        return "already in the past".into();
    }
    let (days, hours, minutes) = (delta / 86400, delta % 86400 / 3600, delta % 3600 / 60);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 || parts.is_empty() {
        parts.push(format!("{minutes}m"));
    }
    format!("in {}", parts.join(" "))
}
/// Arrow-key date/time picker: left/right choose a field, up/down adjust it.
/// Opens on the batch's current time, or on the next whole minute.
fn picker(screen: &mut Screen, initial: Option<i64>) -> Result<Option<i64>> {
    let mut fields = match initial {
        Some(at) => queue::picker_fields(at)?,
        None => queue::picker_start(now_epoch())?,
    };
    let mut slot = queue::HOUR;
    let mut warning = String::new();
    let separators = ["-", "-", "   ", ":"];
    loop {
        let resolved = queue::fields_epoch(fields).map_err(|error| error.to_string());
        screen.draw(|frame| {
            let [body, help] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(frame.area());
            let block = panel(" Send after (host local time) ");
            let inner = block.inner(body);
            let segments = [
                format!("{:04}", fields[queue::YEAR]),
                format!("{:02}", fields[queue::MONTH]),
                format!("{:02}", fields[queue::DAY]),
                format!("{:02}", fields[queue::HOUR]),
                format!("{:02}", fields[queue::MINUTE]),
            ];
            let mut spans = Vec::new();
            let mut columns = [0usize; 5];
            let mut column = 0usize;
            for (i, segment) in segments.iter().enumerate() {
                if i > 0 {
                    let separator = separators[i - 1];
                    spans.push(Span::styled(separator, Style::default().fg(CHROME)));
                    column += separator.chars().count();
                }
                columns[i] = column;
                spans.push(Span::styled(
                    segment.clone(),
                    if i == slot {
                        Style::default()
                            .bg(Color::Rgb(35, 53, 78))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                ));
                column += segment.chars().count();
            }
            // All three rows are padded to one width so centering keeps them aligned.
            let width = column;
            let centre = columns[slot] + segments[slot].chars().count() / 2;
            let marker = |glyph: &str| {
                Line::from(Span::styled(
                    format!(
                        "{}{glyph}{}",
                        " ".repeat(centre),
                        " ".repeat(width.saturating_sub(centre + 1))
                    ),
                    Style::default().fg(ACCENT),
                ))
            };
            let mut lines = vec![
                marker("\u{25b2}"),
                Line::from(spans),
                marker("\u{25bc}"),
                Line::default(),
            ];
            match &resolved {
                Ok(at) => {
                    let delta = at - now_epoch();
                    lines.push(Line::from(Span::styled(
                        queue::format_weekday(*at),
                        Style::default().fg(BODY),
                    )));
                    lines.push(Line::from(Span::styled(
                        relative(delta),
                        if delta <= 0 {
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Green)
                        },
                    )));
                }
                Err(error) => lines.push(Line::from(Span::styled(
                    error.clone(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ))),
            }
            if !warning.is_empty() {
                lines.push(Line::from(Span::styled(
                    warning.clone(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
            }
            let padding = (inner.height as usize).saturating_sub(lines.len()) / 2;
            let mut padded = vec![Line::default(); padding];
            padded.extend(lines);
            frame.render_widget(
                Paragraph::new(padded)
                    .alignment(Alignment::Center)
                    .block(block),
                body,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    hints(&[
                        ("\u{2190}\u{2192}", "Field"),
                        ("\u{2191}\u{2193}", "Adjust"),
                        ("Enter", "Schedule"),
                        ("Esc/Ctrl-c", "Back"),
                    ]),
                    Line::from(Span::styled(
                        format!("Editing {}", queue::SLOTS[slot]),
                        Style::default().fg(CHROME),
                    )),
                ]),
                help,
            );
        })?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        warning.clear();
        match key.code {
            KeyCode::Left => slot = slot.saturating_sub(1),
            KeyCode::Right => slot = (slot + 1).min(queue::SLOTS.len() - 1),
            KeyCode::Up => fields = queue::shift_fields(fields, slot, 1),
            KeyCode::Down => fields = queue::shift_fields(fields, slot, -1),
            KeyCode::Enter => match &resolved {
                Ok(at) if *at > now_epoch() => return Ok(Some(*at)),
                Ok(_) => warning = "Pick a future time.".into(),
                Err(error) => warning = error.clone(),
            },
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(None);
            }
            _ => {}
        }
    }
}
fn confirm(screen: &mut Screen, prompt: &str) -> Result<bool> {
    loop {
        screen.draw(|frame| {
            // A Span cannot hold newlines, so keep the prompt's own line breaks.
            let mut lines: Vec<Line> = prompt
                .lines()
                .map(|line| {
                    Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(Color::White),
                    ))
                })
                .collect();
            lines.push(Line::default());
            lines.push(hints(&[("Enter", "Confirm"), ("Esc", "Cancel")]));
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Message queue ")),
                frame.area(),
            )
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => return Ok(true),
                KeyCode::Esc => return Ok(false),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false);
                }
                _ => {}
            }
        }
    }
}
fn active(batches: &[Batch]) -> Option<&Batch> {
    batches.iter().rev().find(|b| b.status.editable())
}

pub fn run(selector: &str) -> Result<()> {
    let target = queue::target(selector)?;
    queue::ensure_worker()?;
    // Pastes arrive whole in the editor; the mouse is left to the terminal.
    let options = render::TerminalOptions {
        mouse: render::MouseMode::None,
        bracketed_paste: true,
    };
    render::with_terminal(options, |screen| run_loop(screen, &target))
}

fn run_loop(screen: &mut Screen, target: &queue::Target) -> Result<()> {
    let mut selected = 0usize;
    let mut message = String::new();
    let mut failed = false;
    let mut unsaved = String::new();
    loop {
        let batches = queue::list(Some(target))?;
        let batch = active(&batches);
        let messages = batch.map(|b| b.messages.as_slice()).unwrap_or_default();
        selected = selected.min(messages.len().saturating_sub(1));
        let title = format!(" Message queue · {} ", target.session);
        let bold = |color: Color| Style::default().fg(color).add_modifier(Modifier::BOLD);
        let body = Style::default().fg(BODY);
        let schedule: Vec<Line> = match batch {
            Some(b) => vec![
                Line::from(vec![
                    Span::styled(b.status.label(), bold(status_color(b.status))),
                    Span::styled(format!(" · {} message(s)", b.messages.len()), body),
                ]),
                match b.after {
                    Some(at) => Line::from(vec![
                        Span::styled("sends ", body),
                        Span::styled(queue::format_time(at), bold(Color::White)),
                    ]),
                    None => Line::from(vec![
                        Span::styled("Choose a time with ", body),
                        Span::styled("t", bold(KEY)),
                        Span::styled(" to arm this batch", body),
                    ]),
                },
            ],
            None => vec![
                Line::from(Span::styled(
                    "No pending batch",
                    Style::default().fg(CHROME),
                )),
                Line::from(vec![
                    Span::styled("a", bold(KEY)),
                    Span::styled(" to add a message", body),
                ]),
            ],
        };
        let last = batches.iter().rev().find(|b| !b.status.editable());
        let detail = batch
            .and_then(|b| b.detail.clone())
            .or_else(|| {
                last.map(|b| {
                    format!(
                        "Previous batch: {}{}",
                        b.status.label(),
                        b.detail
                            .as_ref()
                            .map(|x| format!(" · {x}"))
                            .unwrap_or_default()
                    )
                })
            })
            .unwrap_or_default();
        screen.draw(|frame| {
            let [header, list, preview, footer] = Layout::vertical([Constraint::Length(4), Constraint::Percentage(30), Constraint::Min(3), Constraint::Length(5)]).areas(frame.area());
            frame.render_widget(Paragraph::new(schedule).block(panel(title)), header);
            let items: Vec<_> = messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{}. ", i + 1), Style::default().fg(CHROME)),
                        Span::styled(
                            m.lines().next().unwrap_or_default().to_string(),
                            Style::default().fg(Color::White),
                        ),
                    ]))
                })
                .collect();
            let mut selection = ListState::default().with_selected(if items.is_empty() { None } else { Some(selected) });
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(Style::default().bg(Color::Rgb(35, 53, 78)).fg(Color::White).add_modifier(Modifier::BOLD))
                    .highlight_symbol(" ")
                    .block(panel(" Instructions · combined into one prompt ")),
                list,
                &mut selection,
            );
            let selected_text = match messages.get(selected) {
                Some(text) => Paragraph::new(text.as_str()).style(Style::default().fg(Color::White)),
                None => Paragraph::new("Add messages, then choose a time with t. Closing this window keeps the schedule running.").style(Style::default().fg(CHROME)),
            };
            frame.render_widget(selected_text.wrap(Wrap { trim: false }).block(panel(" Selected instruction ")), preview);
            let mut lines = vec![
                hints(&[("a", "Add"), ("e", "Edit"), ("d", "Delete"), ("\u{2191}\u{2193}", "Select"), ("J/K", "Reorder")]),
                hints(&[("t", "Time"), ("Space", "Pause/resume"), ("c", "Cancel batch"), ("q/Esc", "Close")]),
            ];
            let feedback = if failed { bold(Color::Red) } else { Style::default().fg(Color::Green) };
            // Spans cannot hold newlines; multi-line errors/details need one Line each.
            lines.extend(message.lines().map(|line| Line::from(Span::styled(line.to_string(), feedback))));
            lines.extend(detail.lines().map(|line| Line::from(Span::styled(line.to_string(), Style::default().fg(CHROME)))));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), footer);
        })?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if failed {
            message.clear();
            failed = false;
        }
        let expected = batch
            .map(|b| queue::Revision::Batch(b.id.as_str(), b.revision))
            .unwrap_or(queue::Revision::Empty);
        let action: Result<()> = (|| {
            match key.code {
                KeyCode::Char('a') => match editor(screen, " Add instruction ", &unsaved)? {
                    Some(text) if !text.trim().is_empty() => {
                        unsaved.clone_from(&text);
                        queue::edit(target, expected, Edit::Add(text))?;
                        unsaved.clear();
                        selected = messages.len();
                        message = "Message saved.".into();
                    }
                    Some(_) => message = "Nothing to save.".into(),
                    None => {
                        unsaved.clear();
                        message = "Discarded.".into();
                    }
                },
                KeyCode::Char('e') if !messages.is_empty() => {
                    match editor(screen, " Edit instruction ", &messages[selected])? {
                        Some(text) if !text.trim().is_empty() => {
                            unsaved.clone_from(&text);
                            queue::edit(target, expected, Edit::Replace(selected, text))?;
                            unsaved.clear();
                            message = "Message saved.".into();
                        }
                        Some(_) => message = "Left unchanged · d deletes an instruction.".into(),
                        None => message = "Discarded.".into(),
                    }
                }
                KeyCode::Char('d') if !messages.is_empty() => {
                    if confirm(screen, &format!("Delete instruction {}?", selected + 1))? {
                        queue::edit(target, expected, Edit::Remove(selected))?;
                        message = "Message deleted.".into();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(messages.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('J') if selected + 1 < messages.len() => {
                    queue::edit(target, expected, Edit::Move(selected, selected + 1))?;
                    selected += 1;
                }
                KeyCode::Char('K') if selected > 0 => {
                    queue::edit(target, expected, Edit::Move(selected, selected - 1))?;
                    selected -= 1;
                }
                KeyCode::Char('t') if !messages.is_empty() => {
                    // The picker shows the resolved zone live, so it needs no review step.
                    if let Some(at) = picker(screen, batch.and_then(|b| b.after))? {
                        queue::edit(target, expected, Edit::Schedule(at))?;
                        message = "Batch scheduled. You can close this window.".into();
                    }
                }
                KeyCode::Char(' ') if batch.is_some() => {
                    queue::edit(
                        target,
                        expected,
                        if batch.unwrap().status == Status::Scheduled {
                            Edit::Pause
                        } else {
                            Edit::Resume
                        },
                    )?;
                    message.clear();
                }
                KeyCode::Char('c')
                    if batch.is_some()
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && confirm(
                            screen,
                            "Cancel this batch? Its messages will remain in CLI history.",
                        )? =>
                {
                    queue::edit(target, expected, Edit::Cancel)?;
                    message = "Batch cancelled.".into();
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = action {
            message = error.to_string();
            failed = true;
            if !unsaved.is_empty() {
                message.push_str(" · a to recover unsaved text");
            }
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_multiline_editing_and_paste() {
        let mut e = Editor::new("αβ\nhello");
        e.vertical(false);
        assert_eq!(e.cursor, 2);
        e.insert("🦀\r\nthird\u{1b}");
        assert_eq!(e.text(), "αβ🦀\nthird\nhello");
        e.vertical(true);
        assert_eq!(e.cursor, e.chars.len());
    }
}
