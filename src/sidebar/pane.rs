//! Opening, closing, toggling and equalizing the sidebar pane in an outer
//! tmux window, and tracking the pane that opened it.

use super::layout::{
    dynamic_layout_without_sidebars, equalized_layout, opening_width_deltas, parse_width_deltas,
};
use super::width::clamp_width_for_window;
use super::*;

fn configured_open_focus(socket: &str) -> Result<SidebarOpenFocus> {
    let value = outer_tmux_output(
        socket,
        &["show-options", "-gqv", SIDEBAR_OPEN_FOCUS_OPTION],
        "read pmux sidebar open-focus option",
    )?;
    Ok(SidebarOpenFocus::from_tmux_option(&value))
}

fn pane_border_label_visible(socket: &str, pane_id: &str) -> Result<bool> {
    let status = outer_tmux_output(
        socket,
        &[
            "show-options",
            // `-A` includes a value inherited from the global window option;
            // without it tmux prints nothing for the usual `set -g` setup.
            "-wAqv",
            "-t",
            pane_id,
            "pane-border-status",
        ],
        "query outer tmux pane border visibility",
    )?;
    Ok(matches!(status.as_str(), "top" | "bottom"))
}

fn sidebar_command(socket: &str, host_pane: &str, pane_label_visible: bool) -> Result<String> {
    let binary = crate::util::resolve_pmux_binary()?;
    let mut parts = pmux_command_parts(&binary);
    parts.push("sidebar".to_string());
    if pane_label_visible {
        parts.push("--pane-label".to_string());
    }
    parts.extend([
        "--socket".to_string(),
        socket.to_string(),
        "--host-pane".to_string(),
        host_pane.to_string(),
    ]);
    Ok(shell_join(&parts))
}

fn sidebar_origin_pane(socket: &str, sidebar_pane: &str) -> Result<Option<String>> {
    let origin = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            sidebar_pane,
            &format!("#{{{SIDEBAR_HOST_PANE_OPTION}}}"),
        ],
        "query pmux sidebar origin pane",
    )?;
    Ok((!origin.trim().is_empty()).then_some(origin))
}

fn outer_pane_exists(socket: &str, pane_id: &str) -> bool {
    outer_tmux_output(
        socket,
        &["display-message", "-p", "-t", pane_id, "#{pane_id}"],
        "query outer tmux pane",
    )
    .is_ok_and(|id| id == pane_id)
}

/// The pane that opened `sidebar_pane`, re-homed onto a live work pane in the
/// sidebar's window when that original host has since been closed.
///
/// Width calculations resolve their display context through the host pane, so
/// a dead host would otherwise make both the stabilize hook and the closing
/// toggle fail: the sidebar would absorb the closed pane's width and could no
/// longer be toggled shut.
pub(super) fn live_sidebar_host(socket: &str, sidebar_pane: &str) -> Result<Option<String>> {
    let Some(host) = sidebar_origin_pane(socket, sidebar_pane)? else {
        return Ok(None);
    };
    if outer_pane_exists(socket, &host) {
        return Ok(Some(host));
    }
    let raw = outer_tmux_output(
        socket,
        &[
            "list-panes",
            "-t",
            sidebar_pane,
            "-F",
            "#{pane_id}\t#{pane_active}\t#{@pmux_sidebar}",
        ],
        "find a replacement pmux sidebar host pane",
    )?;
    let work_panes: Vec<(&str, bool)> = raw
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().filter(|id| !id.is_empty())?;
            let active = fields.next() == Some("1");
            let sidebar = fields.next().is_some_and(|value| value.trim() == "1");
            (!sidebar).then_some((id, active))
        })
        .collect();
    let Some(replacement) = work_panes
        .iter()
        .find(|(_, active)| *active)
        .or_else(|| work_panes.first())
        .map(|(id, _)| id.to_string())
    else {
        return Ok(None);
    };
    outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            sidebar_pane,
            SIDEBAR_HOST_PANE_OPTION,
            &replacement,
        ],
        "re-home pmux sidebar onto a live host pane",
    )?;
    Ok(Some(replacement))
}

/// The host pane a sidebar should act for: `host_pane` while it is alive,
/// otherwise the live pane its sidebar has been re-homed onto.
///
/// A running sidebar keeps the host it was launched with, and passes it to the
/// commands its menus run and to its own re-exec after an upgrade. Once that
/// pane has closed, resolving through the sidebar pane keeps those working
/// instead of failing to find the outer session.
pub fn resolve_sidebar_host(socket: &str, host_pane: &str, sidebar_pane: Option<&str>) -> String {
    if outer_pane_exists(socket, host_pane) {
        return host_pane.to_string();
    }
    sidebar_pane
        .and_then(|sidebar_pane| live_sidebar_host(socket, sidebar_pane).ok().flatten())
        .unwrap_or_else(|| host_pane.to_string())
}

fn sidebar_previous_pane(socket: &str, sidebar_pane: &str) -> Result<Option<String>> {
    let previous = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            sidebar_pane,
            &format!("#{{{SIDEBAR_PREVIOUS_PANE_OPTION}}}"),
        ],
        "query pmux sidebar previous pane",
    )?;
    Ok((!previous.trim().is_empty()).then_some(previous))
}

fn record_sidebar_previous_pane(
    socket: &str,
    sidebar_pane: &str,
    previous_pane: &str,
) -> Result<()> {
    outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            sidebar_pane,
            SIDEBAR_PREVIOUS_PANE_OPTION,
            previous_pane,
        ],
        "record pmux sidebar previous pane",
    )?;
    Ok(())
}

fn sidebar_width_deltas(socket: &str, sidebar_pane: &str) -> Result<Option<String>> {
    let deltas = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            sidebar_pane,
            &format!("#{{{SIDEBAR_WIDTH_DELTAS_OPTION}}}"),
        ],
        "query pmux sidebar opening width deltas",
    )?;
    Ok((!deltas.trim().is_empty()).then_some(deltas))
}

/// Return the outer window's current layout.
///
/// Tmux does not necessarily restore sibling pane widths when a full-height
/// edge pane is killed, so the close path builds and reapplies a new layout
/// after removing the sidebar leaf.
fn window_layout(socket: &str, target: &str) -> Result<String> {
    let layout = outer_tmux_output(
        socket,
        &["display-message", "-p", "-t", target, "#{window_layout}"],
        "query outer tmux window layout",
    )?;
    if layout.is_empty() {
        bail!("outer tmux did not return a window layout");
    }
    Ok(layout)
}

fn restore_window_layout(socket: &str, window_id: &str, layout: &str) -> Result<()> {
    outer_tmux_output(
        socket,
        &["select-layout", "-t", window_id, layout],
        "restore outer tmux layout after closing pmux sidebar",
    )?;
    Ok(())
}

fn focus_outer_pane_if_present(socket: &str, pane_id: &str) {
    // The remembered origin is advisory: it may have been closed while the
    // sidebar was open. In that case tmux's normal focus fallback is better
    // than leaving the sidebar open.
    let _ = outer_tmux_output(
        socket,
        &["select-pane", "-t", pane_id],
        "restore pmux sidebar origin pane focus",
    );
}

/// Add the pmux sidebar title without replacing the user's normal pane label.
/// `pane-border-format` is a window option, so a per-pane user option gates a
/// single global wrapper and the original format is evaluated as the fallback.
fn ensure_sidebar_pane_border_format(socket: &str) -> Result<()> {
    let current = outer_tmux_output(
        socket,
        &["show-options", "-gqv", "pane-border-format"],
        "read outer tmux pane border format",
    )?;
    if current == SIDEBAR_PANE_BORDER_FORMAT {
        return Ok(());
    }
    if current == LEGACY_SIDEBAR_PANE_BORDER_FORMAT {
        outer_tmux_output(
            socket,
            &[
                "set-option",
                "-g",
                "pane-border-format",
                SIDEBAR_PANE_BORDER_FORMAT,
            ],
            "upgrade pmux sidebar pane border format",
        )?;
        return Ok(());
    }
    if current.contains(SIDEBAR_PANE_BORDER_FALLBACK_OPTION) {
        return Ok(());
    }
    outer_tmux_output(
        socket,
        &[
            "set-option",
            "-g",
            SIDEBAR_PANE_BORDER_FALLBACK_OPTION,
            &current,
        ],
        "preserve outer tmux pane border format",
    )?;
    outer_tmux_output(
        socket,
        &[
            "set-option",
            "-g",
            "pane-border-format",
            SIDEBAR_PANE_BORDER_FORMAT,
        ],
        "install pmux sidebar pane border format",
    )?;
    Ok(())
}

fn open_sidebar(host_pane: &str, socket: &str, focus_sidebar: bool) -> Result<()> {
    let window = target_window(socket, host_pane)?;
    let backend_socket = crate::tmux::pmux_socket();
    let width = clamp_width_for_window(read_sidebar_width()?, window.width)?;
    let side = configured_side(socket)?;
    let pane_label_visible = pane_border_label_visible(socket, host_pane)?;
    let layout_before = window_layout(socket, &window.id).ok();
    let panes_before = window_panes(socket, &window.id).ok();
    let command = sidebar_command(socket, host_pane, pane_label_visible)?;
    let mut args = vec![
        "split-window".to_string(),
        "-d".to_string(),
        "-f".to_string(),
        "-h".to_string(),
    ];
    if side == SidebarSide::Left {
        args.push("-b".to_string());
    }
    args.extend([
        "-l".to_string(),
        width.to_string(),
        "-P".to_string(),
        "-F".to_string(),
        "#{pane_id}".to_string(),
        "-t".to_string(),
        host_pane.to_string(),
        command,
    ]);
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let sidebar_pane = outer_tmux_output(socket, &args_ref, "open pmux sidebar pane")?;
    if sidebar_pane.is_empty() {
        bail!("outer tmux did not return the new pmux sidebar pane id");
    }

    if let Err(err) = outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            &sidebar_pane,
            SIDEBAR_PANE_OPTION,
            "1",
        ],
        "mark pmux sidebar pane",
    ) {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up unmarked pmux sidebar pane",
        );
        return Err(err);
    }
    if let Err(err) = outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            &sidebar_pane,
            SIDEBAR_HOST_PANE_OPTION,
            host_pane,
        ],
        "record pmux sidebar origin pane",
    ) {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up sidebar without origin pane",
        );
        return Err(err);
    }
    if let Err(err) = outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            &sidebar_pane,
            SIDEBAR_BACKEND_SOCKET_OPTION,
            &backend_socket,
        ],
        "record pmux sidebar backend socket",
    ) {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up sidebar without backend socket",
        );
        return Err(err);
    }
    if let Err(err) = record_sidebar_previous_pane(socket, &sidebar_pane, host_pane) {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up sidebar without previous pane",
        );
        return Err(err);
    }
    if let Some(width_deltas) = layout_before.as_deref().and_then(|before| {
        let after = window_layout(socket, &window.id).ok()?;
        let panes_before = panes_before.as_deref()?;
        let panes_after = window_panes(socket, &window.id).ok()?;
        opening_width_deltas(before, &after, panes_before, &panes_after)
    }) && let Err(err) = outer_tmux_output(
        socket,
        &[
            "set-option",
            "-p",
            "-t",
            &sidebar_pane,
            SIDEBAR_WIDTH_DELTAS_OPTION,
            &width_deltas,
        ],
        "record pmux sidebar opening width deltas",
    ) {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up sidebar without opening width deltas",
        );
        return Err(err);
    }
    if focus_sidebar
        && let Err(err) = outer_tmux_output(
            socket,
            &["select-pane", "-t", &sidebar_pane],
            "focus pmux sidebar pane",
        )
    {
        let _ = outer_tmux_output(
            socket,
            &["kill-pane", "-t", &sidebar_pane],
            "clean up unfocusable pmux sidebar pane",
        );
        return Err(err);
    }
    Ok(())
}

/// Open a sidebar in `host_pane`'s window if one is not already there.
pub(super) fn ensure_sidebar(host_pane: &str, socket: &str) -> Result<()> {
    let window = target_window(socket, host_pane)?;
    if window_panes(socket, &window.id)?
        .iter()
        .any(|pane| pane.sidebar)
    {
        return Ok(());
    }
    ensure_sidebar_pane_border_format(socket)?;
    open_sidebar(host_pane, socket, false)
}

/// Equalize outer-tmux panes without changing the window's arrangement:
/// every split divides its space evenly between its direct children, so a
/// left pane beside a stacked pair becomes one half and two quarters. An open
/// pmux sidebar keeps its current width.
pub fn equalize_panes(pane_id: &str, socket: &str) -> Result<()> {
    if pane_id.trim().is_empty() {
        bail!("outer tmux pane id must not be empty");
    }
    let window = target_window(socket, pane_id)?;
    let panes = window_panes(socket, &window.id)?;
    let layout = window_layout(socket, &window.id)?;
    let layout = equalized_layout(&layout, &panes)
        .ok_or_else(|| anyhow!("could not equalize panes within the current layout"))?;
    restore_window_layout(socket, &window.id, &layout.encode_with_checksum())
}

/// Toggle the dedicated pmux sidebar for the outer window containing
/// `host_pane`. The sidebar is always a full-height edge pane. This is a pure
/// toggle: an open sidebar is closed regardless of which pane is focused,
/// otherwise one is opened. Whether opening moves focus into the sidebar or
/// leaves it on the current pane is controlled by `@pmux_sidebar_open_focus`
/// (default: preserve the current pane).
pub fn toggle_sidebar(host_pane: &str, socket: &str) -> Result<()> {
    if host_pane.trim().is_empty() {
        bail!("outer tmux pane id must not be empty");
    }
    let window = target_window(socket, host_pane)?;
    let outer_panes = window_panes(socket, &window.id)?;
    let existing: Vec<_> = outer_panes
        .iter()
        .filter(|pane| pane.sidebar)
        .cloned()
        .collect();
    if !existing.is_empty() {
        // Pure toggle: an open sidebar is closed regardless of which pane is
        // focused. It never jumps into an open-but-unfocused sidebar first.
        let sidebar_is_focused = existing.iter().any(|pane| pane.id == host_pane);
        let origins = existing
            .iter()
            .filter_map(|pane| sidebar_origin_pane(socket, &pane.id).ok().flatten())
            .collect::<Vec<_>>();
        let previous_panes = existing
            .iter()
            .filter_map(|pane| sidebar_previous_pane(socket, &pane.id).ok().flatten())
            .collect::<Vec<_>>();
        let width_deltas = existing
            .iter()
            .find_map(|pane| sidebar_width_deltas(socket, &pane.id).ok().flatten())
            .and_then(|deltas| parse_width_deltas(&deltas));
        let dynamic_layout = window_layout(socket, &window.id).ok().and_then(|layout| {
            dynamic_layout_without_sidebars(&layout, &existing, width_deltas.as_ref())
        });
        // Restore focus before removing the sidebar pane. Closing from the
        // sidebar itself returns to the pane that opened it; closing from any
        // other pane leaves the user exactly where they already are.
        let focus_target = if sidebar_is_focused {
            previous_panes.first().or(origins.first()).cloned()
        } else {
            Some(host_pane.to_string())
        };
        if let Some(target) = focus_target {
            focus_outer_pane_if_present(socket, &target);
        }
        // Killing the sidebar pane gives its renderer no chance to clear a
        // flash it is running.
        clear_stale_flash_options(socket);
        for pane in existing {
            // Mouse drags do not reliably fire `after-resize-pane` in every
            // outer tmux setup, so take one final authoritative measurement
            // before this pane disappears. Best-effort: a failed measurement
            // must never leave the sidebar impossible to close.
            let _ = record_sidebar_width(&pane.id, socket);
            outer_tmux_output(
                socket,
                &["kill-pane", "-t", &pane.id],
                "close pmux sidebar pane",
            )?;
        }
        if let Some(layout) = dynamic_layout {
            restore_window_layout(socket, &window.id, &layout.encode_with_checksum())?;
        }
        return Ok(());
    }
    ensure_sidebar_pane_border_format(socket)?;
    // Default is to leave focus on the current pane; opt in to jumping into the
    // sidebar with `set -g @pmux_sidebar_open_focus sidebar`.
    let focus_sidebar = matches!(configured_open_focus(socket)?, SidebarOpenFocus::Sidebar);
    open_sidebar(host_pane, socket, focus_sidebar)
}

/// Whether every pane left in a window is a pmux sidebar. Only toggled
/// sidebars carry `@pmux_sidebar`, so a pane running `pmux sidebar` by hand
/// never counts.
fn only_sidebars_remain(panes: &[OuterPane]) -> bool {
    !panes.is_empty() && panes.iter().all(|pane| pane.sidebar)
}

/// Close the window holding `sidebar_pane` if nothing but sidebars is left in
/// it, rather than leaving the sidebar stretched across the whole window. The
/// sidebar calls this when it is resized, which is how it learns its last
/// neighbour closed. (tmux's pane-close hooks cannot say which window lost a
/// pane: their `#{window_id}` is the session's current window.) The session's
/// last window is kept, since closing it would end the outer session. Returns
/// whether the window was closed.
pub fn close_window_if_only_sidebar(socket: &str, sidebar_pane: &str) -> Result<bool> {
    let window = target_window(socket, sidebar_pane)?;
    if !only_sidebars_remain(&window_panes(socket, &window.id)?) {
        return Ok(false);
    }
    let windows = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            &window.id,
            "#{session_windows}",
        ],
        "count the outer session's windows",
    )?;
    if windows.trim().parse::<u32>().unwrap_or(1) <= 1 {
        return Ok(false);
    }
    outer_tmux_output(
        socket,
        &["kill-window", "-t", &window.id],
        "close a window left with only the pmux sidebar",
    )?;
    Ok(true)
}

/// Ask any pmux sidebar renderer(s) in `window_id` to re-check outer-tmux focus
/// immediately by sending SIGUSR1 to their pane process. This is the sending
/// side of the sidebar's focus-refresh signal: the `pane-focus-in` hook calls it
/// so foregrounding a window updates its sidebar instantly instead of waiting
/// out the visibility backoff. Best-effort — a sidebar that has gone away is
/// simply skipped, and a window with no sidebar is a no-op.
pub fn nudge_window_sidebars(window_id: &str, socket: &str) -> Result<()> {
    if window_id.trim().is_empty() {
        return Ok(());
    }
    let raw = outer_tmux_output(
        socket,
        &[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{@pmux_sidebar}\t#{pane_pid}",
        ],
        "list outer tmux panes to nudge the sidebar",
    )?;
    for line in raw.lines() {
        let mut fields = line.splitn(2, '\t');
        let is_sidebar = fields.next().is_some_and(|value| value.trim() == "1");
        if !is_sidebar {
            continue;
        }
        // Only a pmux sidebar pane is signalled (its pane command is the sidebar
        // renderer, which installs a SIGUSR1 handler). A parse failure or a pane
        // that has since exited is skipped rather than risking a stray signal.
        let Some(pid) = fields
            .next()
            .and_then(|value| value.trim().parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        // SAFETY: `kill` with a fixed signal number is safe; a pid that has
        // exited returns ESRCH, which we intentionally ignore (best-effort).
        unsafe {
            libc::kill(pid, libc::SIGUSR1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_counts_as_sidebar_only_when_every_pane_is_a_marked_sidebar() {
        let pane = |id: u16, sidebar: bool| OuterPane {
            id: format!("%{id}"),
            sidebar,
            layout_pane_id: id,
        };
        assert!(only_sidebars_remain(&[pane(1, true)]));
        assert!(!only_sidebars_remain(&[pane(1, true), pane(2, false)]));
        // A pane running `pmux sidebar` by hand carries no marker.
        assert!(!only_sidebars_remain(&[pane(2, false)]));
        assert!(!only_sidebars_remain(&[]));
    }
}
