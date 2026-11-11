//! Flashing an attached process's outer pane (and window tab) so the user can
//! spot it.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Set to `on`/`off` on a pane while the sidebar flashes it, and unset after.
/// A pane-border-format can test it with `#{==:#{@pmux_flash_pane},on}`.
const FLASH_PANE_OPTION: &str = "@pmux_flash_pane";
/// The window-status-format counterpart of [`FLASH_PANE_OPTION`]. It is a
/// separate name because pane options inherit window options, and a window
/// flash must not light up every pane border in that window.
const FLASH_WINDOW_OPTION: &str = "@pmux_flash_window";
/// Length of each on/off phase of a flash. Public so the sidebar can blink
/// its own row text in step via [`flash_phase_on`].
pub const FLASH_PHASE_DURATION: Duration = Duration::from_millis(500);
/// The pane (and optional window) currently being flashed, with the flash's
/// generation. The lock is held across each option write, so once
/// [`stop_outer_flash`] has cleared a target no in-flight phase can set it
/// again.
static FLASH_TARGET: Mutex<Option<FlashTarget>> = Mutex::new(None);
static FLASH_GENERATION: AtomicU64 = AtomicU64::new(0);

struct FlashTarget {
    generation: u64,
    socket: String,
    pane_id: String,
    window_id: Option<String>,
}

/// Flash the outer pane holding an attached process, plus its window's status
/// tab when that window is not the one showing the sidebar, until
/// [`stop_outer_flash`] is called or another flash replaces it.
///
/// The flash only toggles the [`FLASH_PANE_OPTION`] / [`FLASH_WINDOW_OPTION`]
/// user options (every option change redraws tmux clients); the user's
/// border and status formats decide what a flash looks like. Phases are timed
/// from the returned start instant, so callers can blink in step with
/// [`flash_phase_on`].
pub fn flash_outer_location(location: &OuterPaneLocation, context: &SidebarContext) -> Instant {
    stop_outer_flash();
    let started = Instant::now();
    let generation = FLASH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let window_id =
        (location.window_id != context.host_window_id).then(|| location.window_id.clone());
    *lock_flash_target() = Some(FlashTarget {
        generation,
        socket: context.socket.clone(),
        pane_id: location.pane_id.clone(),
        window_id,
    });
    thread::spawn(move || {
        let mut phase: u32 = 0;
        loop {
            {
                let target = lock_flash_target();
                let Some(target) = target.as_ref().filter(|t| t.generation == generation) else {
                    return;
                };
                let value = if phase.is_multiple_of(2) { "on" } else { "off" };
                set_flash_options(
                    &target.socket,
                    &target.pane_id,
                    target.window_id.as_deref(),
                    Some(value),
                );
            }
            phase = phase.wrapping_add(1);
            let next = started + FLASH_PHASE_DURATION * phase;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    started
}

/// Whether a flash that began at `started` is in its lit phase at `now`.
pub fn flash_phase_on(started: Instant, now: Instant) -> bool {
    let phase = now.saturating_duration_since(started).as_millis()
        / FLASH_PHASE_DURATION.as_millis().max(1);
    phase.is_multiple_of(2)
}

/// Stop the current flash, if any, and clear its options immediately.
pub fn stop_outer_flash() {
    let mut target = lock_flash_target();
    if let Some(target) = target.take() {
        set_flash_options(
            &target.socket,
            &target.pane_id,
            target.window_id.as_deref(),
            None,
        );
    }
}

fn lock_flash_target() -> std::sync::MutexGuard<'static, Option<FlashTarget>> {
    FLASH_TARGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Unset any flash options left behind on this outer server, e.g. by a
/// sidebar that was killed mid-flash. Best-effort.
pub fn clear_stale_flash_options(socket: &str) {
    let mut args: Vec<String> = Vec::new();
    for (scope, list, option) in [
        ("-p", "list-panes", FLASH_PANE_OPTION),
        ("-w", "list-windows", FLASH_WINDOW_OPTION),
    ] {
        let Ok(raw) = outer_tmux_output(
            socket,
            &[
                list,
                "-a",
                "-F",
                &format!(
                    "#{{{option}}}\t#{{{}}}",
                    if list == "list-panes" {
                        "pane_id"
                    } else {
                        "window_id"
                    }
                ),
            ],
            "list stale pmux flash options",
        ) else {
            continue;
        };
        for target in raw.lines().filter_map(|line| {
            let (value, target) = line.split_once('\t')?;
            (!value.is_empty()).then_some(target)
        }) {
            if !args.is_empty() {
                args.push(";".to_string());
            }
            args.extend(["set-option", scope, "-u", "-t", target, option].map(str::to_string));
        }
    }
    if !args.is_empty() {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = outer_tmux_output(socket, &args, "clear stale pmux flash options");
    }
}

/// Set (or with `None`, unset) the flash options in one tmux invocation.
/// Best-effort: the target may have closed mid-flash.
fn set_flash_options(socket: &str, pane_id: &str, window_id: Option<&str>, value: Option<&str>) {
    let mut args = vec!["set-option", "-p"];
    if value.is_none() {
        args.push("-u");
    }
    args.extend(["-t", pane_id, FLASH_PANE_OPTION]);
    args.extend(value);
    if let Some(window_id) = window_id {
        args.extend([";", "set-option", "-w"]);
        if value.is_none() {
            args.push("-u");
        }
        args.extend(["-t", window_id, FLASH_WINDOW_OPTION]);
        args.extend(value);
    }
    let _ = outer_tmux_output(socket, &args, "flash outer tmux pane");
}
