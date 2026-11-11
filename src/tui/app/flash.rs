//! Which sidebar row's outer pane flashes, and driving that flash.

use super::*;

/// Decides which sidebar row's outer pane should be flashing. The keyboard
/// cursor (while the sidebar is focused) and the mouse pointer each nominate a
/// row when they move onto a different one; the latest nomination flashes once
/// it has rested for [`PANE_FLASH_REST`]. A keyboard nomination keeps flashing
/// until the cursor moves on; a pointer nomination (hover or click) also stops
/// once the pointer has probably left the sidebar.
#[derive(Debug, Default)]
pub(in crate::tui) struct PaneFlashTracker {
    pub(in crate::tui) selected: Option<String>,
    /// The row most recently selected by a click. When the cursor arrives
    /// there it is the pointer's doing, so it must not start a keyboard flash.
    clicked: Option<String>,
    hovered: Option<Hover>,
    /// The current target, when it was nominated, and by what.
    nominee: Option<(String, Instant, FlashSource)>,
}

/// What nominated a flash. A keyboard flash is quieter in the sidebar itself
/// (only the arrow and `pane N` / `window N` blink, since the cursor already
/// marks the row), while a pointer flash lights the whole row it is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum FlashSource {
    Keyboard,
    Pointer,
}

#[derive(Debug)]
struct Hover {
    identity: String,
    last_motion: Instant,
    /// Whether that motion was in the row's first or last column, which a
    /// pointer leaving the sidebar sideways almost always passes through.
    at_edge: bool,
}

impl PaneFlashTracker {
    fn observe_selection(&mut self, identity: Option<&str>, now: Instant) {
        if self.selected.as_deref() == identity {
            return;
        }
        let previous = std::mem::replace(&mut self.selected, identity.map(str::to_string));
        if identity.is_some() && identity == self.clicked.as_deref() {
            return;
        }
        if identity.is_some() {
            self.clicked = None;
        }
        self.nominate(previous.as_deref(), identity, FlashSource::Keyboard, now);
    }

    pub(in crate::tui) fn observe_hover(
        &mut self,
        identity: Option<&str>,
        at_edge: bool,
        now: Instant,
    ) {
        match (&mut self.hovered, identity) {
            (Some(hover), Some(identity)) if hover.identity == identity => {
                hover.last_motion = now;
                hover.at_edge = at_edge;
            }
            _ => {
                let previous = self.hovered.take().map(|hover| hover.identity);
                self.hovered = identity.map(|identity| Hover {
                    identity: identity.to_string(),
                    last_motion: now,
                    at_edge,
                });
                self.nominate(previous.as_deref(), identity, FlashSource::Pointer, now);
            }
        }
    }

    /// A click selects its row, but the flash it starts follows the pointer
    /// like a hover rather than persisting like a keyboard selection.
    pub(in crate::tui) fn observe_click(&mut self, identity: &str, at_edge: bool, now: Instant) {
        self.clicked = Some(identity.to_string());
        self.observe_hover(Some(identity), at_edge, now);
    }

    /// A newly entered row becomes the target; leaving the current target's
    /// row without entering another clears it.
    fn nominate(
        &mut self,
        previous: Option<&str>,
        identity: Option<&str>,
        source: FlashSource,
        now: Instant,
    ) {
        match identity {
            Some(identity) => self.nominee = Some((identity.to_string(), now, source)),
            None if self.nominee.as_ref().map(|(nominee, ..)| nominee.as_str()) == previous => {
                self.nominee = None;
            }
            None => {}
        }
    }

    #[cfg(test)]
    fn target_row(&mut self, now: Instant) -> Option<&str> {
        self.target(now).map(|(identity, _)| identity)
    }

    /// The row that should be flashing now, if any, and what nominated it.
    fn target(&mut self, now: Instant) -> Option<(&str, FlashSource)> {
        // tmux sends the sidebar no event when the pointer moves into another
        // pane, so a pointer nomination lapses once the pointer has been still:
        // quickly if its last motion was at the sidebar's edge (it most likely
        // left), otherwise after a short rest.
        if let Some(hover) = &self.hovered {
            let idle_limit = if hover.at_edge {
                PANE_FLASH_EDGE_IDLE
            } else {
                PANE_FLASH_HOVER_IDLE
            };
            if now.duration_since(hover.last_motion) >= idle_limit {
                if self.nominee.as_ref().map(|(nominee, ..)| nominee) == Some(&hover.identity) {
                    self.nominee = None;
                }
                self.hovered = None;
            }
        }
        let (identity, since, source) = self.nominee.as_ref()?;
        (now.duration_since(*since) >= PANE_FLASH_REST).then_some((identity.as_str(), *source))
    }
}

impl SidebarApp {
    /// Keep the outer pane of whichever row the cursor or pointer rests on
    /// flashing, if that row's process is attached in this outer session.
    pub(in crate::tui) fn update_pane_flash(
        &mut self,
        context: &crate::SidebarContext,
        now: Instant,
    ) {
        let selected = (self.sidebar_focused && !self.header_selected)
            .then(|| self.flash_identity(self.selected))
            .flatten();
        self.pane_flash.observe_selection(selected.as_deref(), now);
        let target = self
            .pane_flash
            .target(now)
            .map(|(identity, source)| (identity.to_string(), source));
        let location = target
            .as_ref()
            .and_then(|(identity, _)| self.row_for_flash_identity(identity))
            .and_then(|row| self.row_location(row))
            .cloned();
        let ((target, source), location) = match (target, location) {
            (Some(target), Some(location)) => (target, location),
            _ => {
                if self.active_flash.take().is_some() {
                    crate::stop_outer_flash();
                }
                return;
            }
        };
        if let Some((identity, _, active_source)) = self.active_flash.as_mut()
            && *identity == target
        {
            // Hovering onto the row the cursor already flashes (or back) only
            // changes how loudly the row blinks; the outer flash keeps going.
            *active_source = source;
            return;
        }
        let started = crate::flash_outer_location(&location, context);
        self.active_flash = Some((target, started, source));
    }

    /// What nominated `row`'s flash, while its pane is flashing and in the
    /// lit phase.
    pub(in crate::tui) fn row_flash_lit(&self, row: usize, now: Instant) -> Option<FlashSource> {
        let (identity, started, source) = self.active_flash.as_ref()?;
        (self.flash_identity(row).as_deref() == Some(identity.as_str())
            && crate::flash_phase_on(*started, now))
        .then_some(*source)
    }

    /// How long the event loop may wait before the flashing row's detail text
    /// next needs to change phase.
    pub(in crate::tui) fn flash_poll_timeout(&self, now: Instant) -> Option<Duration> {
        let (_, started, _) = self.active_flash.as_ref()?;
        let phase = crate::FLASH_PHASE_DURATION.as_millis().max(1);
        let elapsed = now.saturating_duration_since(*started).as_millis();
        let remaining = phase - elapsed % phase;
        Some(Duration::from_millis(remaining as u64 + 1))
    }

    /// The current pane flash, for change detection in the run loop.
    pub(in crate::tui) fn flash_key(&self) -> Option<(String, Instant, FlashSource)> {
        self.active_flash.clone()
    }

    /// A stable identity for a row the flash can target, including the
    /// ungrouped fallback list where rows are not materialized.
    pub(in crate::tui) fn flash_identity(&self, row: usize) -> Option<String> {
        if self.rows.is_empty() {
            return self
                .processes
                .get(row)
                .map(|process| process.process.name.clone());
        }
        match self.rows.get(row)? {
            SidebarListRow::Process { identity, .. } => Some(identity.clone()),
            _ => None,
        }
    }

    fn row_for_flash_identity(&self, identity: &str) -> Option<usize> {
        if self.rows.is_empty() {
            return self
                .processes
                .iter()
                .position(|process| process.process.name == identity);
        }
        self.rows.iter().position(
            |row| matches!(row, SidebarListRow::Process { identity: id, .. } if id == identity),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_flash_targets_the_resting_row_until_the_cursor_moves_on() {
        use super::{PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        tracker.observe_selection(Some("a"), start);
        assert_eq!(tracker.target_row(start), None);
        // Moving on before the rest elapses replaces the nomination.
        let later = start + Duration::from_millis(50);
        tracker.observe_selection(Some("b"), later);
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST), None);
        assert_eq!(tracker.target_row(later + PANE_FLASH_REST), Some("b"));
        // It keeps flashing for as long as the cursor stays.
        tracker.observe_selection(Some("b"), later + PANE_FLASH_REST * 10);
        assert_eq!(tracker.target_row(later + PANE_FLASH_REST * 20), Some("b"));
        // Losing the selection (e.g. the sidebar lost focus) stops it.
        tracker.observe_selection(None, later + PANE_FLASH_REST * 21);
        assert_eq!(tracker.target_row(later + PANE_FLASH_REST * 22), None);
    }

    #[test]
    fn pane_flash_follows_hover_and_lapses_under_a_parked_pointer() {
        use super::{PANE_FLASH_HOVER_IDLE, PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        // Passing over a row without resting on it never flashes it.
        tracker.observe_hover(Some("a"), false, start);
        tracker.observe_hover(None, false, start + Duration::from_millis(50));
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST * 2), None);

        tracker.observe_hover(Some("b"), false, start);
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST), Some("b"));
        // Small movements within the row keep it alive past the idle limit...
        let moved = start + PANE_FLASH_HOVER_IDLE - Duration::from_millis(1);
        tracker.observe_hover(Some("b"), false, moved);
        assert_eq!(tracker.target_row(start + PANE_FLASH_HOVER_IDLE), Some("b"));
        // ...but a pointer left still that long stops flashing.
        assert_eq!(tracker.target_row(moved + PANE_FLASH_HOVER_IDLE), None);
    }

    #[test]
    fn pane_flash_stops_quickly_when_the_pointer_was_last_seen_at_the_edge() {
        use super::{PANE_FLASH_EDGE_IDLE, PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        tracker.observe_hover(Some("row"), false, start);
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST), Some("row"));
        // Sliding sideways out of the sidebar: the last report is an edge cell.
        let exit = start + PANE_FLASH_REST * 2;
        tracker.observe_hover(Some("row"), true, exit);
        assert_eq!(
            tracker.target_row(exit + PANE_FLASH_EDGE_IDLE / 2),
            Some("row")
        );
        assert_eq!(tracker.target_row(exit + PANE_FLASH_EDGE_IDLE), None);
    }

    #[test]
    fn flashes_record_whether_the_keyboard_or_the_pointer_nominated_them() {
        use super::{FlashSource, PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        tracker.observe_selection(Some("row"), start);
        assert_eq!(
            tracker.target(start + PANE_FLASH_REST),
            Some(("row", FlashSource::Keyboard))
        );
        // Hovering the same row upgrades it to a pointer flash...
        tracker.observe_hover(Some("row"), false, start + PANE_FLASH_REST);
        assert_eq!(
            tracker.target(start + PANE_FLASH_REST * 2),
            Some(("row", FlashSource::Pointer))
        );
        // ...and a click is a pointer flash too.
        let mut clicked = PaneFlashTracker::default();
        clicked.observe_click("other", false, start);
        assert_eq!(
            clicked.target(start + PANE_FLASH_REST),
            Some(("other", FlashSource::Pointer))
        );
    }

    #[test]
    fn a_clicked_row_flashes_like_a_hover_not_a_keyboard_selection() {
        use super::{PANE_FLASH_HOVER_IDLE, PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        tracker.observe_click("row", false, start);
        // The click focuses the sidebar, so the cursor then reports that row.
        tracker.observe_selection(Some("row"), start + PANE_FLASH_REST);
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST), Some("row"));
        // Once the pointer has gone, the flash stops even though the sidebar
        // remains focused with its cursor on the clicked row.
        assert_eq!(tracker.target_row(start + PANE_FLASH_HOVER_IDLE), None);
        // Moving the cursor with the keyboard afterwards is a keyboard flash
        // again, which persists.
        let key = start + PANE_FLASH_HOVER_IDLE * 2;
        tracker.observe_selection(Some("next"), key);
        assert_eq!(
            tracker.target_row(key + PANE_FLASH_HOVER_IDLE * 5),
            Some("next")
        );
    }

    #[test]
    fn hover_overrides_the_cursor_and_leaving_it_does_not_resume_the_cursor() {
        use super::{PANE_FLASH_REST, PaneFlashTracker};
        let start = Instant::now();
        let mut tracker = PaneFlashTracker::default();
        tracker.observe_selection(Some("cursor"), start);
        tracker.observe_hover(Some("hover"), false, start + PANE_FLASH_REST);
        assert_eq!(
            tracker.target_row(start + PANE_FLASH_REST * 3),
            Some("hover")
        );
        tracker.observe_hover(None, false, start + PANE_FLASH_REST * 4);
        assert_eq!(tracker.target_row(start + PANE_FLASH_REST * 5), None);
        // Moving the cursor re-nominates it.
        tracker.observe_selection(Some("next"), start + PANE_FLASH_REST * 6);
        assert_eq!(
            tracker.target_row(start + PANE_FLASH_REST * 7),
            Some("next")
        );
    }
}
