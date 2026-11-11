//! Sidebar width: clamping to the window, persisting the user's preferred
//! width, and restoring it after the layout changes.

use super::pane::live_sidebar_host;
use super::*;

fn max_sidebar_width_for_window(window_width: u16) -> Result<u16> {
    let max_width = window_width.saturating_sub(MIN_MAIN_WIDTH.saturating_add(1));
    if max_width == 0 {
        bail!(
            "outer tmux window is too narrow for the pmux sidebar (needs more than {} columns)",
            MIN_MAIN_WIDTH + 1
        );
    }
    Ok(max_width)
}

fn preferred_sidebar_width(remembered: Option<u16>) -> u16 {
    remembered
        .unwrap_or(DEFAULT_SIDEBAR_WIDTH)
        .max(MIN_SIDEBAR_WIDTH)
}

/// The sidebar width to open or restore at: the user's remembered width (or
/// the default), limited only by the window's available space.
pub(super) fn clamp_width_for_window(remembered: Option<u16>, window_width: u16) -> Result<u16> {
    let max_width = max_sidebar_width_for_window(window_width)?;
    let desired_width = preferred_sidebar_width(remembered);
    Ok(desired_width.min(max_width))
}

/// Record the width of a pane only when it is a pmux-managed sidebar.
///
/// This command is called by the outer tmux `after-resize-pane` hook, so an
/// ordinary pane resize can never overwrite the saved sidebar preference. A
/// closing sidebar also calls this as a fallback for mouse drags on tmux
/// versions that miss the hook; a terminal-constrained width is ignored so it
/// cannot overwrite the user's saved preference.
pub fn record_sidebar_width(pane_id: &str, socket: &str) -> Result<()> {
    if pane_id.trim().is_empty() {
        return Ok(());
    }
    let raw = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{@pmux_sidebar}\t#{pane_width}\t#{window_width}",
        ],
        "query outer tmux sidebar width",
    )?;
    let mut fields = raw.split('\t');
    if fields.next().is_none_or(|value| value.trim() != "1") {
        return Ok(());
    }
    let width = fields
        .next()
        .ok_or_else(|| anyhow!("outer tmux did not return a sidebar width"))?
        .parse::<u16>()
        .context("outer tmux returned an invalid sidebar width")?;
    let window_width = fields
        .next()
        .ok_or_else(|| anyhow!("outer tmux did not return a sidebar window width"))?
        .parse::<u16>()
        .context("outer tmux returned an invalid sidebar window width")?;
    let constrained_preference = clamp_width_for_window(read_sidebar_width()?, window_width)?;
    // Window space is the only hard limit; a manual resize is persisted exactly
    // as the user left it.
    let maximum_width = max_sidebar_width_for_window(window_width)?;
    if width > maximum_width {
        write_sidebar_width(maximum_width)?;
        outer_tmux_output(
            socket,
            &[
                "resize-pane",
                "-t",
                pane_id,
                "-x",
                &maximum_width.to_string(),
            ],
            "constrain pmux sidebar to the window width",
        )?;
        return Ok(());
    }
    if width == constrained_preference {
        return Ok(());
    }
    write_sidebar_width(width)
}

/// Restore an open sidebar to its remembered width after
/// another pane closes or the terminal changes size. Tmux otherwise may give
/// released space to the edge sidebar simply because it was its adjacent
/// sibling.
pub fn stabilize_sidebar_width(window_id: &str, socket: &str) -> Result<()> {
    if window_id.trim().is_empty() {
        return Ok(());
    }
    let window = target_window(socket, window_id)?;
    let Some(sidebar) = window_panes(socket, &window.id)?
        .into_iter()
        .find(|pane| pane.sidebar)
    else {
        return Ok(());
    };
    if live_sidebar_host(socket, &sidebar.id)?.is_none() {
        return Ok(());
    }
    let desired_width = clamp_width_for_window(read_sidebar_width()?, window.width)?;
    let current_width = outer_tmux_output(
        socket,
        &["display-message", "-p", "-t", &sidebar.id, "#{pane_width}"],
        "query open pmux sidebar width",
    )?
    .parse::<u16>()
    .context("outer tmux returned an invalid open sidebar width")?;
    if current_width == desired_width {
        return Ok(());
    }
    outer_tmux_output(
        socket,
        &[
            "resize-pane",
            "-t",
            &sidebar.id,
            "-x",
            &desired_width.to_string(),
        ],
        "restore pmux sidebar width after another pane closed",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_has_a_usable_floor_but_respects_window_space() {
        assert_eq!(
            clamp_width_for_window(None, 100).unwrap(),
            DEFAULT_SIDEBAR_WIDTH
        );
        assert_eq!(clamp_width_for_window(None, 20).unwrap(), 9);
        assert_eq!(clamp_width_for_window(Some(12), 100).unwrap(), 30);
        assert_eq!(clamp_width_for_window(Some(12), 35).unwrap(), 24);
        assert_eq!(clamp_width_for_window(Some(55), 100).unwrap(), 55);
    }
}
