//! The floating prompt behind pmux's popups: one line of input, or a yes/no
//! question, with Cancel and action buttons. Renaming (`prefix ,` and the
//! sidebar), starting a process (`prefix c`), and kill confirmations all use
//! it, and `pmux prompt` opens it for any command, so every pmux prompt looks
//! and behaves the same.

use super::*;

/// Size of the tmux popup a prompt runs in, border included. Bindings that
/// open a prompt use these (see `Pmux.prompt_popup` in tmux/pmux.rb).
pub(crate) const PROMPT_POPUP_WIDTH: &str = "60";
pub(crate) const PROMPT_POPUP_HEIGHT: &str = "9";

#[derive(Debug)]
pub(crate) struct Prompt {
    title: String,
    action: String,
    /// The text being edited; `None` asks a yes/no question instead.
    input: Option<String>,
    detail: Option<String>,
    cursor: usize,
    danger: bool,
    focus_buttons: bool,
    /// 0 is Cancel, 1 the action.
    selected_button: usize,
    message: Option<String>,
}

pub(crate) enum PromptEvent {
    Stay,
    Cancel,
    Submit,
}

impl Prompt {
    /// A prompt for one line of text, starting with `value`.
    pub(crate) fn text(title: impl Into<String>, action: impl Into<String>, value: &str) -> Self {
        Self {
            title: title.into(),
            action: action.into(),
            input: Some(value.to_string()),
            detail: None,
            cursor: value.len(),
            danger: false,
            focus_buttons: false,
            selected_button: 1,
            message: None,
        }
    }

    /// A yes/no question. Cancel starts selected, so a stray Enter is harmless.
    pub(crate) fn confirm(
        title: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            action: action.into(),
            input: None,
            detail: Some(detail.into()),
            cursor: 0,
            danger: false,
            focus_buttons: true,
            selected_button: 0,
            message: None,
        }
    }

    /// Show the title and action in red, for actions that destroy something.
    pub(crate) fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub(crate) fn value(&self) -> &str {
        self.input.as_deref().unwrap_or("")
    }

    /// Keep the prompt open with `message` shown in place of the key hints.
    pub(crate) fn fail(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PromptEvent {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return PromptEvent::Cancel;
        }
        let editing = self.input.is_some() && !self.focus_buttons;
        match key.code {
            KeyCode::Esc => return PromptEvent::Cancel,
            _ if editing => return self.edit(key.code, control),
            KeyCode::Char('y') if self.input.is_none() => return PromptEvent::Submit,
            KeyCode::Char('n' | 'q') if self.input.is_none() => return PromptEvent::Cancel,
            KeyCode::Up if self.input.is_some() => self.focus_buttons = false,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.selected_button = 1 - self.selected_button;
            }
            KeyCode::Enter if self.selected_button == 0 => return PromptEvent::Cancel,
            KeyCode::Enter => return PromptEvent::Submit,
            _ => {}
        }
        PromptEvent::Stay
    }

    fn edit(&mut self, code: KeyCode, control: bool) -> PromptEvent {
        let Some(input) = self.input.as_mut() else {
            return PromptEvent::Stay;
        };
        let previous = |input: &str, cursor: usize| {
            input[..cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index)
        };
        let next = |input: &str, cursor: usize| {
            input[cursor..]
                .chars()
                .next()
                .map_or(cursor, |character| cursor + character.len_utf8())
        };
        match code {
            KeyCode::Enter => return PromptEvent::Submit,
            KeyCode::Tab | KeyCode::Down => self.focus_buttons = true,
            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::Char('e') if control => self.cursor = input.len(),
            KeyCode::Char('u') if control => {
                input.drain(..self.cursor);
                self.cursor = 0;
                self.message = None;
            }
            KeyCode::Char(_) if control => {}
            KeyCode::Left => self.cursor = previous(input, self.cursor),
            KeyCode::Right => self.cursor = next(input, self.cursor),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = input.len(),
            KeyCode::Backspace => {
                let start = previous(input, self.cursor);
                input.drain(start..self.cursor);
                self.cursor = start;
                self.message = None;
            }
            KeyCode::Delete => {
                let end = next(input, self.cursor);
                input.drain(self.cursor..end);
                self.message = None;
            }
            KeyCode::Char(character) => {
                input.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                self.message = None;
            }
            _ => {}
        }
        PromptEvent::Stay
    }

    pub(crate) fn draw(&self, frame: &mut ratatui::Frame) {
        // The tmux popup supplies the dialog frame; a second border drawn here
        // would nest inside it. In a larger popup (the sidebar's action menu)
        // the prompt keeps its own size, centered.
        let full = frame.area();
        frame.render_widget(Clear, full);
        let inner_width = PROMPT_POPUP_WIDTH.parse::<u16>().unwrap_or(60) - 2;
        let inner_height = PROMPT_POPUP_HEIGHT.parse::<u16>().unwrap_or(9) - 2;
        let width = full.width.min(inner_width);
        let height = full.height.min(inner_height);
        let area = Rect::new(
            full.x + (full.width - width) / 2,
            full.y + (full.height - height) / 2,
            width,
            height,
        );
        let content = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        if content.width == 0 || content.height == 0 {
            return;
        }
        let row = |y: u16| Rect::new(content.x, content.y.saturating_add(y), content.width, 1);
        let accent = if self.danger {
            Color::Red
        } else {
            Color::White
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::truncate_col(&self.title, content.width as usize),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            row(0),
        );

        let body = Rect::new(
            content.x,
            content.y.saturating_add(2),
            content.width,
            3.min(content.height.saturating_sub(2)),
        );
        match &self.input {
            Some(input) => self.draw_input(frame, input, body),
            None => frame.render_widget(
                Paragraph::new(self.detail.clone().unwrap_or_default())
                    .style(Style::default().fg(Color::Gray))
                    .alignment(Alignment::Center)
                    .wrap(ratatui::widgets::Wrap { trim: true }),
                body,
            ),
        }

        let buttons = self.focus_buttons;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Cancel ",
                    prompt_button_style(buttons && self.selected_button == 0, false, false),
                ),
                Span::raw("  "),
                Span::styled(
                    format!(" {} ", self.action),
                    prompt_button_style(buttons && self.selected_button == 1, true, self.danger),
                ),
            ]))
            .alignment(Alignment::Center),
            Rect::new(content.x, body.bottom(), content.width, 1),
        );

        // Only one line fits below the buttons, so an error replaces the key
        // hints there; drawing both made a rejected submit look like Enter did
        // nothing.
        let action = self.action.to_lowercase();
        let hints = match (&self.input, buttons) {
            (None, _) => format!("←/→ select  •  y {action}  •  n/Esc cancel"),
            (Some(_), true) => "←/→ select  •  ↑ edit  •  Enter choose  •  Esc cancel".to_string(),
            (Some(_), false) => {
                format!("←/→ edit  •  Tab buttons  •  Enter {action}  •  Esc cancel")
            }
        };
        let (status, style) = match &self.message {
            Some(message) => (message.as_str(), Style::default().fg(Color::Red)),
            None => (hints.as_str(), Style::default().fg(Color::DarkGray)),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::truncate_col(status, content.width as usize),
                style,
            ))
            .alignment(Alignment::Center),
            row(content.height.saturating_sub(1)),
        );
    }

    fn draw_input(&self, frame: &mut ratatui::Frame, input: &str, area: Rect) {
        let style = Style::default().fg(if self.focus_buttons {
            Color::Gray
        } else {
            Color::White
        });
        // Inside the box's borders, one column of padding each side and one
        // for the cursor.
        let room = (area.width as usize).saturating_sub(5);
        let (before, after) = visible_input(input, self.cursor.min(input.len()), room);
        let line = Line::from(vec![
            Span::styled(" ", style),
            Span::styled(before, style),
            Span::styled(if self.focus_buttons { "" } else { "▌" }, style),
            Span::styled(after, style),
            Span::styled(" ", style),
        ]);
        frame.render_widget(
            Paragraph::new(line).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
    }
}

/// The part of `input` that fits in `room` columns, split at the cursor. Text
/// longer than the box scrolls to keep the cursor in view.
fn visible_input(input: &str, cursor: usize, room: usize) -> (&str, &str) {
    let (before, after) = input.split_at(cursor);
    if crate::display_width(input) <= room {
        return (before, after);
    }
    let mut start = before.len();
    let mut used = 0;
    for (index, character) in before.char_indices().rev() {
        let width = crate::display_width(character.encode_utf8(&mut [0; 4]));
        if used + width > room {
            break;
        }
        used += width;
        start = index;
    }
    let mut end = 0;
    for (index, character) in after.char_indices() {
        let width = crate::display_width(character.encode_utf8(&mut [0; 4]));
        if used + width > room {
            break;
        }
        used += width;
        end = index + character.len_utf8();
    }
    (&before[start..], &after[..end])
}

// Fixed xterm-256 shades rather than the theme's 16 colors: a theme may make
// "blue" light (Catppuccin does), which turns white button labels unreadable.
// Themes leave indexes 16-255 alone.
const BUTTON_BLUE: Color = Color::Indexed(25);
const BUTTON_RED: Color = Color::Indexed(124);
const BUTTON_GRAY: Color = Color::Indexed(240);

fn prompt_button_style(selected: bool, primary: bool, danger: bool) -> Style {
    let fill = match (primary, danger) {
        (true, true) => BUTTON_RED,
        (true, false) => BUTTON_BLUE,
        (false, _) => BUTTON_GRAY,
    };
    Style::default()
        .fg(Color::White)
        .bg(fill)
        .add_modifier(if selected {
            Modifier::BOLD | Modifier::UNDERLINED
        } else {
            Modifier::empty()
        })
}

/// Show `prompt` in this terminal until it is cancelled or `submit` accepts
/// its value. `submit` returns an error message to keep the prompt open and
/// show it.
pub(crate) fn run_prompt(
    mut prompt: Prompt,
    mut submit: impl FnMut(&str) -> std::result::Result<(), String>,
) -> Result<()> {
    render::with_terminal_opts(render::MouseMode::Full, |terminal| {
        loop {
            terminal.draw(|frame| prompt.draw(frame))?;
            if !event::poll(EVENT_POLL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match prompt.handle_key(key) {
                PromptEvent::Stay => {}
                PromptEvent::Cancel => return Ok(()),
                PromptEvent::Submit => match submit(prompt.value()) {
                    Ok(()) => return Ok(()),
                    Err(message) => prompt.fail(message),
                },
            }
        }
    })
}

/// `pmux prompt`: ask in a floating prompt, then run `command` with the answer.
/// An argument that is exactly `{}` is replaced by the answer, which is
/// otherwise appended; with `split`, the answer is split into words as a shell
/// would and each becomes an argument. A confirmation passes no answer. When
/// the command fails, its last line of error output is shown in the prompt.
pub(crate) fn run_prompt_command(prompt: Prompt, split: bool, command: &[String]) -> Result<()> {
    let confirm = prompt.input.is_none();
    run_prompt(prompt, |value| {
        let answer = if confirm {
            Vec::new()
        } else if split {
            shlex::split(value).ok_or_else(|| "Unmatched quote in the command.".to_string())?
        } else {
            vec![value.to_string()]
        };
        let argv = command_with_answer(command, answer);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| "No command to run.".to_string())?;
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|err| format!("Could not run {program}: {err}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| format!("{program} failed ({})", output.status)))
    })
}

fn command_with_answer(command: &[String], answer: Vec<String>) -> Vec<String> {
    if command.iter().any(|arg| arg == "{}") {
        command
            .iter()
            .flat_map(|arg| {
                if arg == "{}" {
                    answer.clone()
                } else {
                    vec![arg.clone()]
                }
            })
            .collect()
    } else {
        command.iter().cloned().chain(answer).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn rendered_rows(prompt: &Prompt) -> Vec<String> {
        let width: u16 = PROMPT_POPUP_WIDTH.parse::<u16>().unwrap() - 2;
        let height: u16 = PROMPT_POPUP_HEIGHT.parse::<u16>().unwrap() - 2;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| prompt.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn text_prompt_edits_at_the_cursor() {
        let mut prompt = Prompt::text("Rename", "Rename", "abcd");
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Char('X')));
        assert_eq!(prompt.value(), "abXcd");
        prompt.handle_key(key(KeyCode::Delete));
        assert_eq!(prompt.value(), "abXd");
        prompt.handle_key(key(KeyCode::Home));
        prompt.handle_key(key(KeyCode::Char('é')));
        assert_eq!(prompt.value(), "éabXd");
        prompt.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        prompt.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(prompt.value(), "");
    }

    #[test]
    fn text_prompt_submits_on_enter_and_cancels_on_escape() {
        let mut prompt = Prompt::text("New process", "Run", "codex");
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            PromptEvent::Submit
        ));
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Esc)),
            PromptEvent::Cancel
        ));
        // Typing y or n into a text prompt is input, not an answer.
        prompt.handle_key(key(KeyCode::Char('n')));
        assert_eq!(prompt.value(), "codexn");
        // The Cancel button cancels.
        prompt.handle_key(key(KeyCode::Tab));
        prompt.handle_key(key(KeyCode::Left));
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            PromptEvent::Cancel
        ));
    }

    #[test]
    fn confirm_prompt_needs_y_or_the_action_button() {
        let mut prompt = Prompt::confirm("Kill?", "Kill", "Ends it.").danger();
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            PromptEvent::Cancel
        ));
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Char('n'))),
            PromptEvent::Cancel
        ));
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Char('y'))),
            PromptEvent::Submit
        ));
        prompt.handle_key(key(KeyCode::Right));
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            PromptEvent::Submit
        ));
    }

    #[test]
    fn errors_replace_the_hints_until_the_input_changes() {
        let mut prompt = Prompt::text("Rename", "Rename", "-name");
        assert!(
            rendered_rows(&prompt)
                .iter()
                .any(|row| row.contains("Tab buttons"))
        );
        prompt.fail("Rename failed: bad name");
        let rows = rendered_rows(&prompt);
        assert!(
            rows.iter()
                .any(|row| row.contains("Rename failed: bad name"))
        );
        assert!(!rows.iter().any(|row| row.contains("Tab buttons")));
        prompt.handle_key(key(KeyCode::Backspace));
        assert!(
            rendered_rows(&prompt)
                .iter()
                .any(|row| row.contains("Tab buttons"))
        );
    }

    #[test]
    fn long_input_scrolls_to_keep_the_cursor_visible() {
        let input = "x".repeat(30) + "END";
        assert_eq!(visible_input(&input, input.len(), 10), ("xxxxxxxEND", ""));
        assert_eq!(visible_input(&input, 0, 10), ("", "xxxxxxxxxx"));
        assert_eq!(visible_input("short", 2, 10), ("sh", "ort"));
        let mut prompt = Prompt::text("New process", "Run", &"word ".repeat(20));
        prompt.handle_key(key(KeyCode::Char('Z')));
        assert!(
            rendered_rows(&prompt)
                .iter()
                .any(|row| row.contains("word Z▌"))
        );
    }

    #[test]
    fn answers_fill_braces_or_follow_the_command() {
        let command = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_with_answer(&command(&["new", "%1"]), command(&["codex", "--full-auto"])),
            command(&["new", "%1", "codex", "--full-auto"])
        );
        assert_eq!(
            command_with_answer(&command(&["rename", "{}", "now"]), command(&["a b"])),
            command(&["rename", "a b", "now"])
        );
    }

    #[test]
    fn buttons_use_fixed_blue_red_and_gray_fills() {
        assert_eq!(
            prompt_button_style(false, true, false).bg,
            Some(BUTTON_BLUE)
        );
        assert_eq!(prompt_button_style(false, true, true).bg, Some(BUTTON_RED));
        assert_eq!(
            prompt_button_style(false, false, false).bg,
            Some(BUTTON_GRAY)
        );
        assert_eq!(
            prompt_button_style(true, true, false).fg,
            Some(Color::White)
        );
    }
}
