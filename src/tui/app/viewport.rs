//! Cursor movement, scrolling, and keeping rows visible in the viewport.

use super::*;

impl SidebarApp {
    pub(in crate::tui) fn move_cursor(&mut self, delta: isize, list_height: u16) {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        if length == 0 {
            self.header_selected = true;
            return;
        }
        let current = if self.header_selected {
            -1
        } else {
            self.selected as isize
        };
        let maximum = length.saturating_sub(1) as isize;
        let mut target = (current + delta).clamp(-1, maximum);
        if target < 0 {
            self.header_selected = true;
            return;
        }
        let direction = if delta < 0 { -1 } else { 1 };
        while !self.rows.is_empty()
            && target >= 0
            && target <= maximum
            && !self.rows[target as usize].selectable()
        {
            target += direction;
        }
        if target < 0 {
            self.header_selected = true;
            return;
        }
        target = target.min(maximum);
        self.header_selected = false;
        self.selected = target as usize;
        self.ensure_selected_item_visible(list_height);
    }

    /// Vim's `gg` targets the first process, not the independently selectable
    /// graph control in the header. Reset both viewport coordinates so even a
    /// partially scrolled open drawer cannot leave the first row clipped.
    pub(in crate::tui) fn move_to_first_process(&mut self) {
        self.first_visible = 0;
        self.first_visible_line = 0;
        self.header_selected = self.processes.is_empty() && self.rows.is_empty();
        self.selected = 0;
    }

    pub(super) fn ensure_visible(&mut self, list_height: u16) {
        if self.rows.is_empty() && self.processes.is_empty() {
            self.first_visible = 0;
            self.first_visible_line = 0;
            return;
        }
        if self.selected <= self.first_visible {
            self.first_visible = self.selected;
            self.first_visible_line = 0;
        }
        while self.selected >= self.visible_end(self.first_visible, list_height) {
            self.first_visible = (self.first_visible + 1).min(self.selected);
            self.first_visible_line = 0;
        }
    }

    /// Reveal the complete selected item after explicit navigation. Drawer
    /// toggles use [`Self::ensure_visible`] instead so opening a drawer can keep
    /// the process header anchored at its existing screen position.
    pub(in crate::tui) fn ensure_selected_item_visible(&mut self, list_height: u16) {
        self.ensure_visible(list_height);
        // With nothing listed there is no selected item to measure.
        if self.rows.is_empty() && self.processes.is_empty() {
            return;
        }
        let viewport_start = self.viewport_start_line();
        let selected_end = self.row_start_line(self.selected) + self.row_line_count(self.selected);
        let available = list_content_height(list_height);
        if selected_end > viewport_start + available {
            // Exact line scrolling ensures the drawer's bottom outline, rather
            // than only its process header, lands inside the viewport.
            self.set_viewport_absolute_line(selected_end.saturating_sub(available));
        }
    }

    /// Followed outer-pane changes receive a little extra space beneath the
    /// selected process. A normal cursor move stays as compact as possible;
    /// this margin is only for the automatic jump/follow behavior so the full
    /// drawer is not visually pressed against the footer or clipped by chrome.
    pub(super) fn ensure_followed_process_visible(&mut self, list_height: u16) {
        let selected_start = self.row_start_line(self.selected);
        let selected_end = selected_start + self.row_line_count(self.selected);
        let viewport_start = self.viewport_start_line();
        let available = list_content_height(list_height);
        // The margin belongs beneath a row revealed at the bottom. A row
        // revealed upward lands at the top; scrolling on past it would push
        // the newly followed process straight back off screen.
        let revealed_downward = selected_start >= viewport_start
            && selected_end > viewport_start.saturating_add(available);

        self.ensure_selected_item_visible(list_height);
        if revealed_downward {
            self.set_viewport_absolute_line(
                self.viewport_start_line()
                    .saturating_add(FOLLOWED_PROCESS_BOTTOM_MARGIN_LINES),
            );
        }
    }

    /// Lines a row occupies in the list, including its details drawer when open.
    pub(in crate::tui) fn row_line_count(&self, index: usize) -> usize {
        if self.rows.is_empty() {
            return process_line_count(&self.processes[index], false)
                + self.drawer_line_count(index);
        }
        match &self.rows[index] {
            SidebarListRow::Group(_) | SidebarListRow::GroupTypeSeparator => 1,
            SidebarListRow::Resumable { entry_index, .. } => {
                render::RESUMABLE_ROW_LINES + self.past_drawer_line_count(*entry_index)
            }
            SidebarListRow::Process {
                process_index,
                tree,
                parent_note,
                ..
            } => {
                process_line_count(&self.processes[*process_index], parent_note.is_some())
                    + self.drawer_line_count_indented(*process_index, tree.width())
            }
        }
    }

    pub(in crate::tui) fn row_block_line_count(&self, index: usize) -> usize {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        let spacing = if self.rows.is_empty() {
            index + 1 < length
        } else {
            match self.rows.get(index) {
                Some(SidebarListRow::Process { .. } | SidebarListRow::Resumable { .. }) => {
                    index + 1 < length
                }
                Some(SidebarListRow::Group(group)) => !group.collapsed,
                _ => false,
            }
        };
        self.row_line_count(index) + usize::from(spacing)
    }

    pub(super) fn normalize_viewport_line(&mut self) {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        if length == 0 {
            self.first_visible = 0;
            self.first_visible_line = 0;
            return;
        }
        self.first_visible = self.first_visible.min(length - 1);
        self.first_visible_line = self.first_visible_line.min(
            self.row_block_line_count(self.first_visible)
                .saturating_sub(1),
        );
    }

    pub(super) fn row_start_line(&self, index: usize) -> usize {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        (0..index.min(length))
            .map(|row| self.row_block_line_count(row))
            .sum()
    }

    pub(in crate::tui) fn selected_viewport_line(&self) -> usize {
        self.row_start_line(self.selected)
            .saturating_sub(self.viewport_start_line())
    }

    pub(super) fn viewport_start_line(&self) -> usize {
        self.row_start_line(self.first_visible) + self.first_visible_line
    }

    fn set_viewport_absolute_line(&mut self, mut line: usize) {
        self.first_visible = 0;
        self.first_visible_line = 0;
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        for index in 0..length {
            let height = self.row_block_line_count(index);
            if line < height {
                self.first_visible = index;
                self.first_visible_line = line;
                return;
            }
            line = line.saturating_sub(height);
        }
        if length > 0 {
            self.first_visible = length - 1;
        }
    }

    pub(super) fn anchor_selected_at(&mut self, viewport_line: usize) {
        let selected_start = self.row_start_line(self.selected);
        self.set_viewport_absolute_line(selected_start.saturating_sub(viewport_line));
    }

    pub(super) fn selected_is_visible(&self, list_height: u16) -> bool {
        if self.header_selected {
            return false;
        }
        let selected_start = self.row_start_line(self.selected);
        let viewport_start = self.viewport_start_line();
        selected_start >= viewport_start
            && selected_start - viewport_start < list_content_height(list_height)
    }

    /// Drawer-aware viewport end (mirrors the free [`visible_end`] but counts the
    /// expanded row's drawer against the height budget).
    pub(in crate::tui) fn visible_end(&self, start: usize, list_height: u16) -> usize {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        // Use the same per-row block heights the renderer and scroll math use:
        // folded group headers and the separator carry no blank line after them.
        render::viewport_end_for_blocks(
            length,
            start,
            usize::from(start == self.first_visible) * self.first_visible_line,
            list_height,
            |index| self.row_block_line_count(index),
            |index| {
                if self.rows.is_empty() {
                    process_line_count(&self.processes[index], false)
                } else {
                    match &self.rows[index] {
                        SidebarListRow::Process {
                            process_index,
                            parent_note,
                            ..
                        } => process_line_count(
                            &self.processes[*process_index],
                            parent_note.is_some(),
                        ),
                        SidebarListRow::Resumable { .. } => render::RESUMABLE_ROW_LINES,
                        _ => 1,
                    }
                }
            },
        )
    }

    /// Ctrl-U/Ctrl-D move by half of the *rendered process entries*, not half
    /// of the terminal's text lines. Most process entries span multiple lines,
    /// so using line count here would skip an entire list on a tall sidebar.
    pub(in crate::tui) fn half_visible_process_step(&self, list_height: u16) -> usize {
        let visible = self
            .visible_end(self.first_visible, list_height)
            .saturating_sub(self.first_visible);
        half_visible_process_count(visible)
    }

    /// Scroll list entries without changing the tracked selection. This is
    /// used while the sidebar is unfocused, where selection follows the outer
    /// tmux pane and may intentionally be outside the viewport.
    pub(in crate::tui) fn scroll_viewport(&mut self, delta: isize) {
        let length = if self.rows.is_empty() {
            self.processes.len()
        } else {
            self.rows.len()
        };
        if length == 0 {
            self.first_visible = 0;
            self.first_visible_line = 0;
            return;
        }
        let maximum = length.saturating_sub(1) as isize;
        self.first_visible = (self.first_visible as isize + delta).clamp(0, maximum) as usize;
        self.first_visible_line = 0;
    }

    pub(in crate::tui) fn cursor_is_visible_for(&self, index: usize) -> bool {
        if self.header_selected || index != self.selected {
            return false;
        }
        if self.sidebar_focused {
            return true;
        }
        let Some(active_pane) = self.active_outer_pane.as_deref() else {
            return false;
        };
        if self.rows.is_empty() {
            return self.processes.get(index).is_some_and(|process| {
                process
                    .attachments()
                    .iter()
                    .any(|location| location.pane_id == active_pane)
            });
        }
        match self.rows.get(index) {
            Some(SidebarListRow::Process {
                location: Some(location),
                ..
            }) => location.pane_id == active_pane,
            Some(SidebarListRow::Process { process_index, .. }) => self.processes[*process_index]
                .attachments()
                .iter()
                .any(|location| location.pane_id == active_pane),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    #[test]
    fn gg_selects_and_fully_reveals_the_first_process_not_the_graph_header() {
        let mut app = SidebarApp::new(None);
        app.processes = (0..3)
            .map(|index| {
                let mut process = test_process(Some("/work"), None);
                process.process.name = format!("process-{index}");
                process
            })
            .collect();
        app.header_selected = true;
        app.selected = 2;
        app.first_visible = 1;
        app.first_visible_line = 3;

        app.move_to_first_process();

        assert!(!app.header_selected, "gg must not target the graph control");
        assert_eq!(app.selected, 0);
        assert_eq!(app.first_visible, 0);
        assert_eq!(app.first_visible_line, 0);
    }

    #[test]
    fn half_page_uses_rendered_process_count_not_terminal_lines() {
        let mut app = SidebarApp::new(None);
        app.processes = (0..27)
            .map(|index| {
                let mut process = test_process(Some("/work"), None);
                process.process.name = format!("process-{index}");
                process
            })
            .collect();
        // Find a terminal height where the renderer fits exactly 24 process
        // entries, matching the real-world sidebar example.
        let list_height = (3..=200)
            .find(|height| app.visible_end(0, *height) == 24)
            .expect("a height should fit exactly 24 test processes");

        assert_eq!(app.half_visible_process_step(list_height), 12);
        app.move_cursor(
            app.half_visible_process_step(list_height) as isize,
            list_height,
        );
        assert_eq!(app.selected, 12);
    }

    #[test]
    fn empty_sidebar_cursor_is_safe() {
        let mut app = SidebarApp::new(None);
        app.move_cursor(1, 8);
        assert_eq!(app.selected, 0);
        assert_eq!(app.first_visible, 0);
        assert!(app.header_selected);
    }

    #[test]
    fn revealing_the_selection_of_an_empty_list_does_not_panic() {
        // A sidebar opened with no pmux processes and no resumable sessions
        // (e.g. a fresh install) restores its selection on startup.
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.rebuild_rows();
        assert!(app.rows.is_empty() && app.processes.is_empty());
        app.ensure_selected_item_visible(20);
        assert_eq!((app.first_visible, app.first_visible_line), (0, 0));
    }
}
