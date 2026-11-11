//! Searching the sidebar: `/` filters the list down to the processes and
//! past sessions that match a query, keeping the tree and groups around them.
//!
//! The query uses the attach picker's matcher (fuzzy, case insensitive, fzf
//! syntax such as `'exact` and `^prefix`). Each word of it must match within
//! one of a process's name, its agent's conversation title, the agent and its
//! state, the running command, or the directory; `!word` leaves out what
//! matches it. A past session's fields are those `pmux history --query`
//! searches. A process that does not match stays
//! listed while one drawn under it does, so a match keeps its place in the
//! tree. Groups are unfolded while a query is active, and a group with
//! nothing left in it is left out.

use super::*;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// The sidebar's query and whether it is being typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tui) struct SidebarSearch {
    pub(in crate::tui) query: String,
    /// Keys go to the query, not the keymap, while this is set.
    pub(in crate::tui) editing: bool,
    /// Processes and past sessions the query matched at the last rebuild.
    pub(in crate::tui) matches: usize,
}

impl SidebarSearch {
    /// Whether a query is filtering the list.
    pub(in crate::tui) fn active(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// A compiled query, matched against one haystack at a time.
pub(super) struct SearchMatcher {
    pattern: Pattern,
    matcher: Matcher,
}

impl SearchMatcher {
    fn new(query: &str) -> Self {
        Self {
            pattern: Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Whether every word of the query matches within one of `fields`, and
    /// no field matches a negated (`!word`) one. Matching field by field
    /// keeps a fuzzy word from being spelled out across several of them.
    fn matches(&mut self, fields: &[String]) -> bool {
        let haystacks: Vec<Vec<char>> =
            fields.iter().map(|field| field.chars().collect()).collect();
        let matcher = &mut self.matcher;
        self.pattern.atoms.iter().all(|atom| {
            let mut field_matches = haystacks
                .iter()
                .map(|chars| atom.score(Utf32Str::Unicode(chars), matcher).is_some());
            if atom.negative {
                field_matches.all(|matched| matched)
            } else {
                field_matches.any(|matched| matched)
            }
        })
    }
}

impl SidebarApp {
    /// The active query's matcher, or `None` when nothing is filtering.
    pub(super) fn search_matcher(&self) -> Option<SearchMatcher> {
        self.search
            .active()
            .then(|| SearchMatcher::new(self.search.query.trim()))
    }

    /// Which processes the query matches, by index into `processes`.
    pub(super) fn process_matches(&self, matcher: &mut SearchMatcher) -> Vec<bool> {
        (0..self.processes.len())
            .map(|index| matcher.matches(&self.process_fields(index)))
            .collect()
    }

    /// Whether the query matches past session `entry`.
    pub(super) fn history_matches(
        matcher: &mut SearchMatcher,
        entry: &crate::HistoryEntry,
    ) -> bool {
        let fields: Vec<String> = crate::history_search::fields(entry)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        matcher.matches(&fields)
    }

    /// The fields a query is matched against for process `index`.
    fn process_fields(&self, index: usize) -> Vec<String> {
        let process = &self.processes[index];
        let info = &process.process;
        let mut parts = vec![info.name.clone()];
        if let Some(title) = self.agent_title(index) {
            parts.push(title.to_string());
        }
        if let Some(agent) = &info.agent {
            parts.push(agent.kind.clone());
            if let Some(state) = &agent.state {
                parts.push(state.clone());
            }
        }
        if !info.current_command.trim().is_empty() {
            parts.push(info.current_command.clone());
        }
        if let Some(cwd) = crate::sidebar_process_cwd(process) {
            parts.push(crate::display_path(cwd));
        }
        parts
    }

    /// Start typing a query. A query already filtering the list is kept, so
    /// `/` edits it.
    pub(in crate::tui) fn start_search(&mut self) {
        self.search.editing = true;
    }

    /// Drop the query and show the whole list again, keeping the selection.
    pub(in crate::tui) fn clear_search(&mut self, list_height: u16) {
        self.search = SidebarSearch::default();
        self.rebuild_rows();
        self.ensure_selected_item_visible(list_height);
    }

    /// Re-filter after the query changed and select the first match.
    pub(in crate::tui) fn search_changed(&mut self, list_height: u16) {
        self.rebuild_rows();
        self.select_first_match();
        self.ensure_selected_item_visible(list_height);
    }

    /// Select the first row the query matches itself, not one kept only
    /// because something under it matches.
    fn select_first_match(&mut self) {
        self.header_selected = false;
        let Some(mut matcher) = self.search_matcher() else {
            return;
        };
        let matched = self.process_matches(&mut matcher);
        let first = self.rows.iter().position(|row| match row {
            SidebarListRow::Process { process_index, .. } => matched[*process_index],
            SidebarListRow::Resumable { .. } => true,
            _ => false,
        });
        if let Some(first) = first.or_else(|| self.rows.iter().position(SidebarListRow::selectable))
        {
            self.selected = first;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    fn process(name: &str, cwd: &str, created_at: i64) -> crate::SidebarProcess {
        let mut process = test_process(Some(cwd), None);
        process.process.name = name.to_string();
        process.process.pane_id = format!("%{created_at}");
        process.process.created_at = created_at;
        process
    }

    fn app(processes: Vec<crate::SidebarProcess>) -> SidebarApp {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.processes = processes;
        app.rebuild_rows();
        app
    }

    fn listed(app: &SidebarApp) -> Vec<&str> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Process { process_index, .. } => {
                    Some(app.processes[*process_index].process.name.as_str())
                }
                _ => None,
            })
            .collect()
    }

    fn search(app: &mut SidebarApp, query: &str) {
        app.search.query = query.to_string();
        app.search_changed(30);
    }

    #[test]
    fn a_query_lists_only_matching_processes_and_selects_the_first() {
        let mut app = app(vec![
            process("api-server", "/work/api", 3),
            process("web-build", "/work/web", 2),
            process("api-tests", "/work/api", 1),
        ]);
        search(&mut app, "api");
        assert_eq!(listed(&app), ["api-server", "api-tests"]);
        assert_eq!(app.search.matches, 2);
        assert_eq!(app.selected_process_name(), Some("api-server"));

        // Directories match too.
        search(&mut app, "work/web");
        assert_eq!(listed(&app), ["web-build"]);

        search(&mut app, "nothing-like-this");
        assert!(listed(&app).is_empty());
        assert_eq!(app.search.matches, 0);

        app.clear_search(30);
        assert_eq!(listed(&app).len(), 3);
    }

    #[test]
    fn a_matching_child_keeps_its_parent_listed_above_it() {
        let parent = process("lead", "/work", 3);
        let mut child = process("helper", "/work", 2);
        child.process.spawned_by = Some(crate::SpawnedBy::local("lead", 3));
        let mut app = app(vec![parent, child, process("other", "/elsewhere", 1)]);
        search(&mut app, "helper");
        assert_eq!(listed(&app), ["lead", "helper"]);
        // The match is selected, not the parent kept for context.
        assert_eq!(app.selected_process_name(), Some("helper"));
        assert_eq!(app.search.matches, 1);
    }

    #[test]
    fn folded_groups_unfold_while_searching_and_empty_groups_are_left_out() {
        let mut app = app(vec![
            process("api-server", "/work/api", 2),
            process("web-build", "/work/web", 1),
        ]);
        app.grouping.automatic = crate::AutomaticGrouping::Cwd;
        app.rebuild_rows();
        let groups: Vec<String> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Group(group) => Some(group.identity.clone()),
                _ => None,
            })
            .collect();
        app.grouping.collapsed.extend(groups);
        app.rebuild_rows();
        assert!(listed(&app).is_empty());

        search(&mut app, "web");
        assert_eq!(listed(&app), ["web-build"]);
        let headers = app
            .rows
            .iter()
            .filter(|row| matches!(row, SidebarListRow::Group(_)))
            .count();
        assert_eq!(headers, 1);

        app.clear_search(30);
        assert!(
            listed(&app).is_empty(),
            "folds come back with the full list"
        );
    }
}
