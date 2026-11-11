//! Interactive Rust terminal interfaces used by the outer-tmux sidebar.
//!
//! The sidebar consumes `pmux events`, just like third-party
//! consumers do. It deliberately layers display and interaction on that
//! stable JSON contract instead of reading tmux state through a private path.
//!
//! Layout: `app` holds sidebar state, `event_loop` drives it, `view` draws
//! it, and `outer` reads the outer tmux server's focus and styling. `menus`,
//! `actions`, and `rename` are the per-process menus and forms. The
//! submodules share one namespace: this file holds the common imports and
//! constants, each submodule starts with `use super::*`, and items used by a
//! sibling are `pub(super)`.

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant, SystemTime},
};
use unicode_width::UnicodeWidthChar;

// The sidebar's row and list rendering lives in `crate::render`, the single
// source of truth shared with the attach picker. These names are re-imported so
// this module keeps referring to them unqualified; the module itself is reached
// as `render::` for the list/terminal/event-stream entry points. Changing a row
// or the list's appearance is done in `render.rs` and takes effect in both the
// sidebar and the picker.
use crate::render::{
    self, ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR, ACTIVE_PANE_BORDER_FALLBACK_COLOR,
    AttachmentScope, CURSOR_BAR, DRAG_HANDLE_COLOR, INACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
    RowRenderParams, attachment_scope, fill_to_width, fmt_count, list_content_height,
    process_line_count, text_width,
};
// Referenced only by this module's tests; gating to test builds keeps the
// non-test binary free of unused-import warnings.
#[cfg(test)]
use crate::render::{
    ATTACHMENT_COLOR, CURSOR_HIGHLIGHT_COLOR, UNFOCUSED_CURSOR_HIGHLIGHT_COLOR,
    agent_tag_column_width, attachment_icon, content_style, first_line_color, launch_cwd_text,
    process_badges, session_name_pill_color, session_name_pill_style,
};

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const EVENT_POLL: Duration = Duration::from_millis(100);
/// The input poll used while a background focus or location read is running,
/// so its result is applied within a few milliseconds of arriving.
const POLLER_RESULT_POLL: Duration = Duration::from_millis(5);
/// How long a newly opened sidebar waits for its first process list before
/// drawing anyway.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(500);
/// While visible, redraw at least this often so relative ages ("5s ago") and
/// briefly shown hints stay current even when nothing else changes.
const CLOCK_REDRAW: Duration = Duration::from_secs(1);
/// How often a running sidebar checks whether the executable that launched it
/// has been replaced. This makes `make install` / an upgrade automatically
/// refresh an already-open sidebar without polling the filesystem every frame.
const SIDEBAR_BINARY_POLL: Duration = Duration::from_millis(250);
/// Wait until the newly installed executable has remained unchanged for this
/// long before replacing ourselves. `install` can briefly expose a partially
/// written destination, so immediately exec'ing on the first metadata change
/// would be racy.
const SIDEBAR_BINARY_RELOAD_DEBOUNCE: Duration = Duration::from_millis(400);
/// When following a process after an outer-pane jump, leave a few rows below
/// its drawer rather than packing the bottom outline against the list edge.
/// Besides making the transition easier to read, this accounts for terminal
/// chrome that can visually crowd the list edge on some tmux configurations.
const FOLLOWED_PROCESS_BOTTOM_MARGIN_LINES: usize = 3;
/// How long the resize-handle hover highlight lingers after the last mouse-motion
/// event over it. The one-column edge gap ([`SIDEBAR_RESIZE_HANDLE_EDGE_GAP`])
/// clears the highlight promptly when the pointer slides off sideways; this is
/// the backstop for the paths that send no exit event at all — leaving straight
/// off the top edge, or a flick fast enough to skip the gap cell. tmux forwards
/// motion only while the pointer is over our pane, so "left" and "stopped moving"
/// look identical; we treat a lapse this long as the pointer having gone.
const HANDLE_HOVER_LINGER: Duration = Duration::from_millis(350);
/// How often the sidebar re-queries outer-tmux focus while its window is the
/// active (visible) one — frequent enough that the active-process highlight
/// tracks pane switches without visible lag.
const FOCUS_POLL_ACTIVE: Duration = Duration::from_millis(200);
/// How often it re-queries while its window is in the background. Its output is
/// not visible there, so this only needs to catch up shortly after the window
/// returns to the foreground; the focus-hook signal (see `request_focus_refresh`)
/// makes that catch-up instant when it is wired up. This backoff is what keeps
/// several open-but-unwatched sidebars from hammering the outer tmux server.
const FOCUS_POLL_INACTIVE: Duration = Duration::from_secs(2);
/// How often it re-queries while visible when the outer tmux has
/// `focus-events` on. Its `pane-focus-in` hook then signals every focus change
/// as it happens, so the poll is only a safety net, and five tmux clients a
/// second per visible sidebar would be wasted on a busy server.
const FOCUS_POLL_HOOKED: Duration = Duration::from_secs(1);
/// How often the process *location* refresh (which also queries the outer tmux
/// server) runs while the window is visible vs. backgrounded. Locations are only
/// meaningful when the sidebar is on screen, so a hidden window refreshes them
/// rarely — just enough to be roughly current if it returns without a focus
/// signal. Returning to the foreground resets the timer so locations refresh
/// promptly on arrival.
const LOCATION_POLL_ACTIVE: Duration = Duration::from_secs(1);
const LOCATION_POLL_INACTIVE: Duration = Duration::from_secs(5);
const SIDEBAR_RESIZE_HANDLE_TEXT: &str = "◂⠿▸";
const SIDEBAR_RESIZE_HANDLE_WIDTH: u16 = 3;
/// Blank columns kept between the handle glyph and the pane's right edge. This
/// bare cell is still inside our pane, so sliding the pointer off the handle
/// *sideways* crosses it and fires a motion event — letting us drop the hover
/// highlight before the pointer leaves the pane (tmux sends nothing once it has).
const SIDEBAR_RESIZE_HANDLE_EDGE_GAP: u16 = 1;
const SIDEBAR_RESIZE_EDGE_HIT_WIDTH: u16 = 2;
const ACTION_MENU_TRAILING_PADDING: &str = " ";
const NATIVE_ACTION_MENU_SELECTED_STYLE: &str = "bg=#23354e,fg=white";
const ACTION_LAUNCH_GUARD: Duration = Duration::from_millis(250);
/// A physical click reports press then release. Opening tmux's native menu on
/// the press lets that release immediately dismiss it, so wait until the
/// release has reached the sidebar first. Some trackpad taps only produce a
/// press in terminal mouse mode; the fallback keeps those working too.
const MOUSE_MENU_RELEASE_DELAY: Duration = Duration::from_millis(30);
const MOUSE_MENU_TAP_FALLBACK: Duration = Duration::from_millis(400);
/// Two clicks on one row within this window are a double-click (jump to or
/// open the process). It is longer than tmux's own fixed 300ms double-click
/// window, so a press tmux forwards for its double-click still counts, and a
/// second click's context menu waits this long so a double-click can cancel it.
const MOUSE_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
/// tmux reports its own DoubleClick event only after its 300ms triple-click
/// timer, and the default `DoubleClick1Pane` binding begins with
/// `select-pane -t =`, re-selecting the sidebar under the pointer. A
/// double-click's jump therefore waits for that event (forwarded to the
/// sidebar right after the `select-pane`), or this long when no forwarded
/// event arrives, so its own pane focus is the last word.
const MOUSE_DOUBLE_CLICK_SETTLE: Duration = Duration::from_millis(450);
/// How long the keyboard cursor or mouse pointer must rest on an attached
/// process before its outer pane flashes, so scrolling past rows (or holding
/// `j`) does not flash every pane along the way.
const PANE_FLASH_REST: Duration = Duration::from_millis(150);
/// A pointer (hover or click) flash stops once the pointer has been still this
/// long. tmux sends the sidebar no event when the pointer moves into another
/// pane, so "left" and "stopped moving" look identical; this is the backstop
/// for exits that skip the edge cells, such as a fast flick.
const PANE_FLASH_HOVER_IDLE: Duration = Duration::from_secs(3);
/// The much shorter lapse when the pointer's last motion was in a row's first
/// or last column: leaving the sidebar sideways crosses those cells, so a
/// pointer last seen there has almost certainly gone (compare
/// [`HANDLE_HOVER_LINGER`]).
const PANE_FLASH_EDGE_IDLE: Duration = Duration::from_millis(400);
const SIDEBAR_RELOAD_SELECTED_ENV: &str = "PMUX_SIDEBAR_RELOAD_SELECTED";
const SIDEBAR_RELOAD_DRAWERS_ENV: &str = "PMUX_SIDEBAR_RELOAD_DRAWERS";
const SIDEBAR_RELOAD_OPEN_DRAWERS_ENV: &str = "PMUX_SIDEBAR_RELOAD_OPEN_DRAWERS";
const SIDEBAR_RELOAD_HEADER_SELECTED_ENV: &str = "PMUX_SIDEBAR_RELOAD_HEADER_SELECTED";
/// Footer status while the `pmux events` feed is being respawned.
const SIDEBAR_RECONNECTING_STATUS: &str = " ⟳ reconnecting to pmux…";
/// Shown while a config from outside pmux has changed pmux's tmux server.
const SIDEBAR_CONFIG_CHANGED_STATUS: &str = " pmux config changed: run pmux restart --repair";

/// Drawer heights per cache state — must match [`SidebarApp::build_drawer_lines_for`]
/// so the viewport budget and the actual render agree.
const DRAWER_LOADING_LINES: usize = 2;
const DRAWER_UNAVAIL_LINES: usize = 3;
/// Braille timeline height, in braille cell rows.
const DRAWER_GRAPH_ROWS: usize = 3;
/// How often the sidebar re-parses every agent's activity while its window is
/// visible, keeping row prompt counts (and open drawers) current.
const ACTIVITY_REFRESH: Duration = Duration::from_secs(15);
/// Stats + graph + bottom border; wrapped prompt rows are added dynamically.
const DRAWER_READY_FIXED_LINES: usize = 1 + DRAWER_GRAPH_ROWS + 1;
const DRAWER_PROMPT_MAX_LINES: usize = 3;
const DRAWER_AXIS_HINT_DURATION: Duration = Duration::from_millis(1_500);
/// How long the header names a grouping mode or graph just switched to.
const HEADER_NOTICE_DURATION: Duration = Duration::from_millis(1_500);
const DRAWER_GRAPH_INDICATOR: &str = "⣠⣾⣦";

mod actions;
mod app;
mod event_loop;
mod input;
mod keymap;
mod menus;
mod outer;
mod poller;
mod prompt;
mod rename;
#[cfg(test)]
mod test_support;
mod view;

use actions::*;
use app::*;
use event_loop::*;
use input::*;
pub(crate) use keymap::SidebarAction;
use keymap::*;
use menus::*;
use outer::*;
use poller::*;
use prompt::*;
use rename::*;
use view::*;

// Entry points the CLI dispatches to.
pub(crate) use actions::run_session_actions;
pub(crate) use event_loop::run_sidebar;
pub(crate) use menus::run_sidebar_menu_item;
pub(crate) use prompt::{PROMPT_POPUP_HEIGHT, PROMPT_POPUP_WIDTH, Prompt, run_prompt_command};
pub(crate) use rename::run_rename_session_ui;

fn modal_area(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(30)
        .min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
