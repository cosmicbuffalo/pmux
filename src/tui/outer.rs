//! Reading the outer tmux server's focus, colors, and key prefix.

use super::*;

/// Query the sidebar window's outer-tmux focus in a single `list-panes` call.
/// Returns whether the window is active (visible) and, if so, which pane holds
/// focus — both driven off the same output so the poll cadence can adapt to
/// visibility without a second round-trip.
pub(super) fn outer_focus(context: &crate::SidebarContext) -> Result<OuterFocus> {
    let output = crate::util::output_with_timeout(
        crate::tmux::tmux_command().args([
            "-S",
            &context.socket,
            "list-panes",
            "-t",
            &context.host_window_id,
            "-F",
            "#{window_active}\t#{pane_id}\t#{pane_active}",
        ]),
        crate::SIDEBAR_QUERY_TIMEOUT,
    )
    .context("query outer tmux sidebar focus")?;
    if !output.status.success() {
        bail!("outer tmux could not query sidebar focus");
    }
    Ok(parse_outer_focus(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse the `#{window_active}\t#{pane_id}\t#{pane_active}` rows emitted by
/// [`outer_focus`]. Split out so the visibility/active-pane logic is unit
/// testable without an outer tmux server.
fn parse_outer_focus(raw: &str) -> OuterFocus {
    // Every pane in a window reports the same window_active flag, so the first
    // row is sufficient to know whether the window is visible.
    let window_active = raw
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .is_some_and(|field| field == "1");
    let active_pane = raw.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let window_active = matches!(fields.next(), Some("1"));
        let pane_id = fields.next()?;
        let pane_active = matches!(fields.next(), Some("1"));
        (window_active && pane_active).then(|| pane_id.to_string())
    });
    OuterFocus {
        window_active,
        active_pane,
    }
}

/// Read the outer server's active-pane border color once when the sidebar
/// launches. A hard-coded orange keeps the sidebar coherent if the option is
/// absent or uses a terminal-specific value such as `terminal`.
pub(super) fn active_pane_border_color(context: &crate::SidebarContext) -> Color {
    let output = crate::tmux::tmux_command()
        .args([
            "-S",
            &context.socket,
            "show-options",
            "-gqv",
            "pane-active-border-style",
        ])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| tmux_style_foreground(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(ACTIVE_PANE_BORDER_FALLBACK_COLOR)
}

/// Whether the outer tmux tells the sidebar about focus changes: pmux's
/// `pane-focus-in` hook (which runs `pmux on-focus`) is installed, and
/// `focus-events` is on, without which tmux never fires it.
pub(super) fn focus_hook_fires(context: &crate::SidebarContext) -> bool {
    let query = |args: &[&str]| {
        crate::tmux::tmux_command()
            .args(["-S", &context.socket])
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default()
    };
    query(&["show-options", "-gqv", "focus-events"]).trim() == "on"
        && query(&["show-hooks", "-g", "pane-focus-in"]).contains(" on-focus ")
}

/// Read the outer window's active and inactive pane backgrounds so the text
/// inside a session-name pill visually cuts through to the surrounding pane.
pub(super) fn pane_background_colors(context: &crate::SidebarContext) -> (Color, Color) {
    (
        outer_style_background_color(
            context,
            "window-active-style",
            ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
        ),
        outer_style_background_color(
            context,
            "window-style",
            INACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
        ),
    )
}

fn outer_style_background_color(
    context: &crate::SidebarContext,
    option: &str,
    fallback: Color,
) -> Color {
    let output = crate::tmux::tmux_command()
        .args(["-S", &context.socket, "show-options", "-gqv", option])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| tmux_style_background(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(fallback)
}

fn tmux_style_foreground(style: &str) -> Option<Color> {
    style
        .split(',')
        .find_map(|part| part.trim().trim_start_matches("#[").strip_prefix("fg="))
        .and_then(parse_tmux_color)
}

pub(super) fn tmux_style_background(style: &str) -> Option<Color> {
    style
        .split(',')
        .find_map(|part| part.trim().trim_start_matches("#[").strip_prefix("bg="))
        .and_then(parse_tmux_color)
}

fn parse_tmux_color(color: &str) -> Option<Color> {
    let color = color.trim().to_ascii_lowercase();
    if let Some(index) = color
        .strip_prefix("colour")
        .or_else(|| color.strip_prefix("color"))
        .and_then(|index| index.parse::<u8>().ok())
    {
        return Some(Color::Indexed(index));
    }
    if let Some(hex) = color.strip_prefix('#')
        && hex.len() == 6
    {
        let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(red, green, blue));
    }
    match color.as_str() {
        "black" => Some(Color::Black),
        "red" | "brightred" => Some(Color::Red),
        "green" | "brightgreen" => Some(Color::Green),
        "yellow" | "brightyellow" => Some(Color::Yellow),
        "blue" | "brightblue" => Some(Color::Blue),
        "magenta" | "purple" | "brightmagenta" => Some(Color::Magenta),
        "cyan" | "brightcyan" => Some(Color::Cyan),
        "white" | "brightwhite" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "brightblack" | "darkgray" | "darkgrey" => Some(Color::DarkGray),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::style::Color;

    #[test]
    fn parse_outer_focus_reports_active_window_and_pane() {
        // Window is active; the focused, non-sidebar pane is reported.
        let focus = parse_outer_focus("1\t%10\t0\n1\t%11\t1\n");
        assert!(focus.window_active);
        assert_eq!(focus.active_pane.as_deref(), Some("%11"));
    }

    #[test]
    fn parse_outer_focus_reports_inactive_window_without_pane() {
        // Background window: no active pane is surfaced, so the poll loop backs
        // off and the highlight is left untouched until the window returns.
        let focus = parse_outer_focus("0\t%10\t1\n0\t%11\t0\n");
        assert!(!focus.window_active);
        assert_eq!(focus.active_pane, None);
        // Empty output (window gone) is treated as inactive, never a panic.
        let empty = parse_outer_focus("");
        assert!(!empty.window_active);
        assert_eq!(empty.active_pane, None);
    }

    #[test]
    fn outer_active_border_color_supports_tmux_color_syntax() {
        assert_eq!(
            tmux_style_foreground("bg=default,fg=#ff8800"),
            Some(Color::Rgb(255, 136, 0))
        );
        assert_eq!(parse_tmux_color("colour214"), Some(Color::Indexed(214)));
        assert_eq!(parse_tmux_color("brightblue"), Some(Color::Blue));
        assert_eq!(tmux_style_foreground("bg=default"), None);
        assert_eq!(
            tmux_style_background("fg=white,bg=colour234"),
            Some(Color::Indexed(234))
        );
    }
}
