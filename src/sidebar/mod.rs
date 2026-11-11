//! Outer-tmux sidebar management.
//!
//! pmux itself always talks to its dedicated `-L pmux` tmux server. The
//! sidebar is intentionally different: it is a normal pane in the user's
//! outer server, addressed by the explicit socket path supplied by tmux.
//!
//! Layout: this file holds the shared types, option names and outer-tmux
//! command helpers, and re-exports the public surface. `pane` opens, toggles
//! and closes the sidebar pane, `width` keeps its width, `layout` is the pure
//! tmux layout math, `locations` finds where processes are attached, `launch`
//! opens processes in new splits and windows, `flash` flashes outer panes,
//! and `init` is the `pmux tmux-init` integration script. Each submodule
//! starts with `use super::*`.

use crate::api::{ProcessInfo, StateSnapshot};
use crate::state::{
    read_session_host_pane, read_session_host_socket, read_sidebar_width, write_sidebar_width,
};
use crate::tmux::current_tmux_socket;
use crate::util::is_auto_generated_name;
use crate::util::shell_join;
use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::env;
use std::process::Command;

mod flash;
mod init;
mod launch;
mod layout;
mod locations;
mod pane;
mod width;

pub use flash::{
    FLASH_PHASE_DURATION, clear_stale_flash_options, flash_outer_location, flash_phase_on,
    stop_outer_flash,
};
pub use init::{apply_tmux_init, sidebar_double_click_installed, tmux_init_script};
pub use launch::{
    attach_group_in_new_outer_window, attach_in_new_outer_split, attach_in_new_outer_window,
    resume_in_new_outer_split, resume_in_new_outer_window,
};
#[cfg(test)]
pub use locations::sidebar_attachment_detail;
pub use locations::{
    display_session_name, jump_to_outer_pane, picker_processes, sidebar_agent_tag,
    sidebar_attachment_detail_at, sidebar_process_cwd, sidebar_processes,
};
pub use pane::{
    close_window_if_only_sidebar, equalize_panes, nudge_window_sidebars, resolve_sidebar_host,
    toggle_sidebar,
};
pub use width::{record_sidebar_width, stabilize_sidebar_width};

/// Width used until a user manually resizes their first sidebar.
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 42;

/// Smallest useful sidebar width on a window that has room for it. A narrow
/// terminal may force a smaller temporary width, but that width is never
/// allowed to become the user's long-term preference.
const MIN_SIDEBAR_WIDTH: u16 = 30;

/// Keep enough horizontal room for the panes the sidebar sits alongside.
const MIN_MAIN_WIDTH: u16 = 10;
const SIDEBAR_PANE_OPTION: &str = "@pmux_sidebar";
const SIDEBAR_HOST_PANE_OPTION: &str = "@pmux_sidebar_host_pane";
const SIDEBAR_BACKEND_SOCKET_OPTION: &str = "@pmux_sidebar_backend_socket";
const SIDEBAR_PREVIOUS_PANE_OPTION: &str = "@pmux_sidebar_previous_pane";
const SIDEBAR_WIDTH_DELTAS_OPTION: &str = "@pmux_sidebar_width_deltas";
const SIDEBAR_SIDE_OPTION: &str = "@pmux_sidebar_side";
const SIDEBAR_OPEN_FOCUS_OPTION: &str = "@pmux_sidebar_open_focus";
const SIDEBAR_PANE_BORDER_FALLBACK_OPTION: &str = "@pmux_sidebar_pane_border_fallback";
const SIDEBAR_PANE_BORDER_FORMAT: &str = "#{?@pmux_sidebar,#[fg=cyan] pmux processes #[default],#{E:@pmux_sidebar_pane_border_fallback}}";
const LEGACY_SIDEBAR_PANE_BORDER_FORMAT: &str =
    "#{?@pmux_sidebar, pmux processes ,#{E:@pmux_sidebar_pane_border_fallback}}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarSide {
    Left,
    Right,
}

impl SidebarSide {
    fn from_tmux_option(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "right" => Self::Right,
            _ => Self::Left,
        }
    }
}

/// Where focus lands when `prefix + Ctrl-p` opens the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarOpenFocus {
    /// Move focus into the newly opened sidebar.
    Sidebar,
    /// Leave focus on whatever pane the user was already on (the default).
    Preserve,
}

impl SidebarOpenFocus {
    fn from_tmux_option(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            // Accept a few natural spellings for "jump into the sidebar".
            "sidebar" | "jump" | "focus" => Self::Sidebar,
            // Anything else (including unset/empty) preserves the current pane.
            _ => Self::Preserve,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OuterWindow {
    id: String,
    width: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OuterPane {
    id: String,
    sidebar: bool,
    layout_pane_id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SplitTargetPane {
    id: String,
    left: u16,
    width: u16,
    top: u16,
    sidebar: bool,
}

/// The outer tmux context from which a sidebar was opened.
///
/// `host_pane` deliberately refers to the pane that opened the sidebar, not
/// the sidebar pane itself. This lets actions switch the pmux client that is
/// already visible in that outer pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarContext {
    pub socket: String,
    pub host_pane: String,
    pub outer_session_id: String,
    pub host_window_id: String,
    pub sidebar_on_right: bool,
}

/// A process's location in the current outer tmux session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterPaneLocation {
    pub pane_id: String,
    pub pane_index: String,
    pub window_id: String,
    pub window_index: String,
    pub window_name: String,
}

/// A stable process snapshot enriched with its location in this outer tmux
/// session, if pmux knows one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarProcess {
    pub process: ProcessInfo,
    /// The primary attachment: the one in the sidebar's own window when there
    /// is one, otherwise the most recently focused. Jumps target this pane.
    pub outer_location: Option<OuterPaneLocation>,
    /// Every pane in this outer session the process is attached in, ordered by
    /// window then pane. Empty when only the primary location is known.
    pub outer_locations: Vec<OuterPaneLocation>,
}

impl SidebarProcess {
    /// All known attachments, falling back to the primary location alone.
    pub fn attachments(&self) -> Vec<&OuterPaneLocation> {
        if self.outer_locations.is_empty() {
            self.outer_location.iter().collect()
        } else {
            self.outer_locations.iter().collect()
        }
    }
}

fn outer_tmux_cmd(socket: &str) -> Result<Command> {
    if socket.trim().is_empty() {
        bail!("outer tmux socket path must not be empty");
    }
    let mut cmd = crate::tmux::tmux_command();
    cmd.arg("-S").arg(socket);
    Ok(cmd)
}

fn outer_tmux_output(socket: &str, args: &[&str], context: &str) -> Result<String> {
    let output = outer_tmux_cmd(socket)?
        .args(args)
        .output()
        .with_context(|| format!("failed to {context}"))?;
    outer_tmux_stdout(output, context)
}

/// How long a background sidebar query (process locations, focus) may wait
/// on a tmux server before giving up until its next refresh.
pub const SIDEBAR_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// [`outer_tmux_output`] for the sidebar's background refreshes, bounded by
/// [`SIDEBAR_QUERY_TIMEOUT`] so a hung outer server cannot wedge them.
fn outer_tmux_query(socket: &str, args: &[&str], context: &str) -> Result<String> {
    let output =
        crate::util::output_with_timeout(outer_tmux_cmd(socket)?.args(args), SIDEBAR_QUERY_TIMEOUT)
            .with_context(|| format!("failed to {context}"))?;
    outer_tmux_stdout(output, context)
}

fn outer_tmux_stdout(output: std::process::Output, context: &str) -> Result<String> {
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.is_empty() {
            bail!("{context}: tmux exited with {}", output.status);
        }
        bail!("{context}: {detail}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn target_window(socket: &str, pane_id: &str) -> Result<OuterWindow> {
    let raw = outer_tmux_output(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{window_id}\t#{window_width}",
        ],
        "query outer tmux window",
    )?;
    let mut fields = raw.split('\t');
    let id = fields
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("outer tmux did not return a window id"))?;
    let width = fields
        .next()
        .ok_or_else(|| anyhow!("outer tmux did not return a window width"))?
        .parse::<u16>()
        .context("outer tmux returned an invalid window width")?;
    Ok(OuterWindow {
        id: id.to_string(),
        width,
    })
}

fn window_panes(socket: &str, window_id: &str) -> Result<Vec<OuterPane>> {
    let raw = outer_tmux_output(
        socket,
        &[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{pane_id}\t#{@pmux_sidebar}",
        ],
        "list outer tmux panes",
    )?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.splitn(2, '\t');
            let id = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("outer tmux returned a pane without an id"))?;
            let sidebar = fields.next().is_some_and(|value| value.trim() == "1");
            let layout_pane_id = id
                .strip_prefix('%')
                .ok_or_else(|| anyhow!("outer tmux returned a pane with an invalid id"))?
                .parse::<u16>()
                .context("outer tmux returned an invalid pane id")?;
            Ok(OuterPane {
                id: id.to_string(),
                sidebar,
                layout_pane_id,
            })
        })
        .collect()
}

fn configured_side(socket: &str) -> Result<SidebarSide> {
    let value = outer_tmux_output(
        socket,
        &["show-options", "-gqv", SIDEBAR_SIDE_OPTION],
        "read pmux sidebar side option",
    )?;
    Ok(SidebarSide::from_tmux_option(&value))
}

/// Build a command that uses this same pmux binary and backend socket.
fn pmux_command_parts(binary: &std::path::Path) -> Vec<String> {
    let mut parts = Vec::new();
    // A custom backend socket is principally used for isolated development
    // servers, but outer panes do not inherit it automatically. Carry it
    // explicitly so sidebar-created panes use the same pmux server as the UI.
    if let Ok(pmux_socket) = env::var(crate::tmux::PMUX_SOCKET_ENV)
        && !pmux_socket.trim().is_empty()
    {
        parts.push("env".to_string());
        parts.push(format!("{}={}", crate::tmux::PMUX_SOCKET_ENV, pmux_socket));
    }
    parts.push(binary.display().to_string());
    parts
}

/// Build a sidebar context from explicit outer-tmux values, or from the
/// current terminal when the renderer is launched directly.
pub fn sidebar_context(socket: Option<&str>, host_pane: Option<&str>) -> Result<SidebarContext> {
    let socket = socket
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .or_else(current_tmux_socket)
        .ok_or_else(|| anyhow!("pmux sidebar must run inside an outer tmux server"))?;
    let host_pane = host_pane
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("TMUX_PANE")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| anyhow!("pmux sidebar needs the outer tmux host pane"))?;
    let outer_context = outer_tmux_output(
        &socket,
        &[
            "display-message",
            "-p",
            "-t",
            &host_pane,
            "#{session_id}\t#{window_id}",
        ],
        "query outer tmux session",
    )?;
    let (outer_session_id, host_window_id) = outer_context
        .split_once('\t')
        .filter(|(session_id, window_id)| !session_id.is_empty() && !window_id.is_empty())
        .ok_or_else(|| anyhow!("outer tmux did not return a session and window id"))?;
    let sidebar_on_right = configured_side(&socket)? == SidebarSide::Right;
    Ok(SidebarContext {
        socket,
        host_pane,
        outer_session_id: outer_session_id.to_string(),
        host_window_id: host_window_id.to_string(),
        sidebar_on_right,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_defaults_to_left_and_accepts_right_option() {
        assert_eq!(SidebarSide::from_tmux_option(""), SidebarSide::Left);
        assert_eq!(SidebarSide::from_tmux_option("LEFT"), SidebarSide::Left);
        assert_eq!(SidebarSide::from_tmux_option("right"), SidebarSide::Right);
        assert_eq!(
            SidebarSide::from_tmux_option("unexpected"),
            SidebarSide::Left
        );
    }

    #[test]
    fn sidebar_open_focus_defaults_to_preserve() {
        // Unset/empty and unrecognized values preserve the current pane.
        assert_eq!(
            SidebarOpenFocus::from_tmux_option(""),
            SidebarOpenFocus::Preserve
        );
        assert_eq!(
            SidebarOpenFocus::from_tmux_option("preserve"),
            SidebarOpenFocus::Preserve
        );
        assert_eq!(
            SidebarOpenFocus::from_tmux_option("nonsense"),
            SidebarOpenFocus::Preserve
        );
        // Opt in to jumping into the sidebar with a few natural spellings.
        for value in ["sidebar", "Sidebar", "JUMP", "focus"] {
            assert_eq!(
                SidebarOpenFocus::from_tmux_option(value),
                SidebarOpenFocus::Sidebar,
                "value {value:?} should focus the sidebar"
            );
        }
    }

    #[test]
    fn sidebar_border_label_uses_a_fixed_cyan_style() {
        assert!(SIDEBAR_PANE_BORDER_FORMAT.contains("#[fg=cyan] pmux processes #[default]"));
        assert_ne!(
            SIDEBAR_PANE_BORDER_FORMAT,
            LEGACY_SIDEBAR_PANE_BORDER_FORMAT
        );
    }
}
