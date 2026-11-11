//! Sidebar rows: grouping processes, the resumable-history group, and
//! building the flat row list the view draws.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum SidebarGroupKind {
    Automatic,
    Manual,
    /// Past agent sessions that are not running and can be resumed.
    Resumable,
}

/// Row identity of the Resumable group header.
pub(in crate::tui) const RESUMABLE_GROUP_IDENTITY: &str = "resumable";
/// How often the agent history catalog is re-read while nothing else
/// prompts a refresh (a process starting or exiting does, immediately).
const HISTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// How often the sidebar checks whether `pmux status-labels` changed a label.
/// Reading a one-line file this often costs next to nothing.
const LABELS_STAMP_POLL: Duration = Duration::from_millis(250);

const GROUP_FOLDED_ICON: &str = "▶";
const GROUP_EXPANDED_ICON: &str = "▼";
const GROUP_HEADER_BACKGROUND_FALLBACK: Color = Color::Rgb(42, 49, 66);

pub(in crate::tui) fn group_header_background(base: Color, selected: bool) -> Color {
    if !selected {
        return base;
    }
    match base {
        Color::Rgb(red, green, blue) => Color::Rgb(
            red.saturating_add(16),
            green.saturating_add(16),
            blue.saturating_add(16),
        ),
        _ => base,
    }
}

pub(super) fn pmux_process_chip_background() -> Color {
    let Some(home) = std::env::var_os("HOME") else {
        return GROUP_HEADER_BACKGROUND_FALLBACK;
    };
    let path = PathBuf::from(home).join(".config/pmux/theme.conf");
    fs::read_to_string(path)
        .ok()
        .and_then(|theme| {
            theme.lines().find_map(|line| {
                line.contains("@pmux_process_chip_style")
                    .then(|| line.find("#[").map(|start| &line[start..]))
                    .flatten()
                    .and_then(tmux_style_background)
            })
        })
        .unwrap_or(GROUP_HEADER_BACKGROUND_FALLBACK)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct SidebarGroupRow {
    pub(in crate::tui) identity: String,
    pub(in crate::tui) key: String,
    pub(in crate::tui) label: String,
    pub(in crate::tui) kind: SidebarGroupKind,
    pub(in crate::tui) members: Vec<String>,
    pub(super) collapsed: bool,
}

pub(in crate::tui) fn group_header_line(group: &SidebarGroupRow, width: u16) -> Line<'static> {
    let fold = if group.collapsed {
        GROUP_FOLDED_ICON
    } else {
        GROUP_EXPANDED_ICON
    };
    let kind = match group.kind {
        SidebarGroupKind::Automatic => "auto",
        SidebarGroupKind::Manual => "manual",
        SidebarGroupKind::Resumable => "past",
    };
    let color = match group.kind {
        SidebarGroupKind::Automatic => Color::Cyan,
        SidebarGroupKind::Manual => Color::Magenta,
        SidebarGroupKind::Resumable => Color::Green,
    };
    let prefix = format!(" {fold} ");
    let metadata = format!("({}) · {kind} ", group.members.len());
    let label = crate::truncate_col(
        &group.label,
        usize::from(width).saturating_sub(text_width(&prefix) + text_width(&metadata) + 1),
    );
    let gap = usize::from(width)
        .saturating_sub(text_width(&prefix) + text_width(&label) + text_width(&metadata));
    Line::from(vec![
        Span::styled(
            prefix,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(metadata, Style::default().fg(Color::Gray)),
    ])
}

/// The `pmux resume` target naming a history entry unambiguously.
pub(in crate::tui) fn resumable_target(entry: &crate::HistoryEntry) -> String {
    format!("{}:{}", entry.agent, entry.session_id)
}

/// An automatic group's label and its `(process index, attachment)` members.
type AutomaticGroup = (String, Vec<(usize, Option<crate::OuterPaneLocation>)>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum SidebarListRow {
    Group(SidebarGroupRow),
    Process {
        process_index: usize,
        identity: String,
        /// The attachment this row stands for when window grouping shows a
        /// process once per window it is attached in; `None` elsewhere.
        location: Option<crate::OuterPaneLocation>,
        /// Where the row sits under the process that spawned it.
        tree: render::RowTree,
        /// The process that spawned this one, named on a row not drawn
        /// under it (`lead`, `lead on macbook`).
        parent_note: Option<String>,
    },
    /// A past agent session, indexing [`SidebarApp::history`].
    Resumable {
        entry_index: usize,
        identity: String,
    },
    GroupTypeSeparator,
}

impl SidebarListRow {
    pub(in crate::tui) fn identity(&self) -> Option<&str> {
        match self {
            Self::Group(group) => Some(&group.identity),
            Self::Process { identity, .. } | Self::Resumable { identity, .. } => Some(identity),
            Self::GroupTypeSeparator => None,
        }
    }

    pub(in crate::tui) fn selectable(&self) -> bool {
        !matches!(self, Self::GroupTypeSeparator)
    }
}

impl SidebarApp {
    pub(in crate::tui) fn selected_group(&self) -> Option<&SidebarGroupRow> {
        match self.rows.get(self.selected) {
            Some(SidebarListRow::Group(group)) => Some(group),
            _ => None,
        }
    }

    fn group_process_indices(&self, members: &[String]) -> Vec<usize> {
        self.processes
            .iter()
            .enumerate()
            .filter(|(_, process)| members.iter().any(|name| name == &process.process.name))
            .map(|(index, _)| index)
            .collect()
    }

    /// The automatic groups a process belongs to, as `(key, label, location)`.
    /// Window grouping lists a process under every window it is attached in,
    /// each entry carrying that window's attachment; other modes yield one.
    pub(super) fn automatic_group_entries(
        &self,
        process: &crate::SidebarProcess,
    ) -> Vec<(String, String, Option<crate::OuterPaneLocation>)> {
        if self.grouping.automatic == crate::AutomaticGrouping::Window {
            let mut windows = HashSet::new();
            let entries: Vec<_> = process
                .attachments()
                .into_iter()
                .filter(|location| windows.insert(location.window_id.clone()))
                .map(|location| {
                    (
                        location.window_id.clone(),
                        format!(
                            "window {} · {}",
                            location.window_index, location.window_name
                        ),
                        Some(location.clone()),
                    )
                })
                .collect();
            if !entries.is_empty() {
                return entries;
            }
        }
        let (key, label) = self.automatic_group_parts(process);
        vec![(key, label, None)]
    }

    fn automatic_group_parts(&self, process: &crate::SidebarProcess) -> (String, String) {
        match self.grouping.automatic {
            crate::AutomaticGrouping::Ungrouped => (String::new(), String::new()),
            crate::AutomaticGrouping::Window => {
                if process.process.attached_clients > 0 {
                    (
                        "other-session".to_string(),
                        "Other tmux session".to_string(),
                    )
                } else {
                    ("unattached".to_string(), "Unattached".to_string())
                }
            }
            crate::AutomaticGrouping::Cwd => {
                let cwd = crate::sidebar_process_cwd(process).unwrap_or("unknown CWD");
                (cwd.to_string(), crate::display_path(cwd))
            }
            crate::AutomaticGrouping::Agent => {
                let agent = process
                    .process
                    .agent
                    .as_ref()
                    .map(|agent| agent.kind.as_str())
                    .unwrap_or("other");
                (agent.to_string(), agent.to_string())
            }
        }
    }

    /// Note when each process changed course: an agent when its attention
    /// state changes, so the list moves it then and not on every signal
    /// while it works. A process seen for the first time counts from its
    /// start, which keeps a freshly opened sidebar in start order.
    pub(in crate::tui) fn settle_courses(&mut self) {
        let mut courses = HashMap::with_capacity(self.processes.len());
        for process in &self.processes {
            let process = &process.process;
            let state = process.agent.as_ref().and_then(|agent| agent.state.clone());
            let course = match self.courses.remove(&process.pane_id) {
                Some(previous) if previous.state == state => previous,
                Some(_) => Course {
                    since: process
                        .agent
                        .as_ref()
                        .and_then(|agent| agent.updated_at)
                        .unwrap_or_else(crate::util::now_epoch),
                    state,
                },
                None => Course {
                    state,
                    since: process.created_at,
                },
            };
            courses.insert(process.pane_id.clone(), course);
        }
        self.courses = courses;
    }

    /// When `process` last changed course; see [`Self::settle_courses`].
    pub(super) fn course_since(&self, process: &crate::ProcessInfo) -> i64 {
        self.courses
            .get(&process.pane_id)
            .map_or(process.created_at, |course| course.since)
    }

    /// The list order: agents waiting on the user first, then those at
    /// work, then the rest, each by when it last changed course, newest
    /// first; then newest started, then name. Families are ordered as one
    /// entry by [`Self::family_order`].
    pub(super) fn list_order(&self, left: usize, right: usize) -> std::cmp::Ordering {
        let (left, right) = (
            &self.processes[left].process,
            &self.processes[right].process,
        );
        attention_tier(left)
            .cmp(&attention_tier(right))
            .then_with(|| self.course_since(right).cmp(&self.course_since(left)))
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.name.cmp(&right.name))
    }

    fn default_automatic_group_order(
        &self,
        groups: &HashMap<String, AutomaticGroup>,
    ) -> Vec<String> {
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        match self.grouping.automatic {
            crate::AutomaticGrouping::Window => keys.sort_by(|left, right| {
                let sort_key = |key: &str| {
                    let class = match key {
                        "unattached" => 2,
                        "other-session" => 1,
                        _ => 0,
                    };
                    let window_index = groups
                        .get(key)
                        .and_then(|(_, members)| {
                            members.iter().find_map(|(_, location)| {
                                location
                                    .as_ref()
                                    .and_then(|location| location.window_index.parse::<u64>().ok())
                            })
                        })
                        .unwrap_or(u64::MAX);
                    (class, window_index)
                };
                sort_key(left)
                    .cmp(&sort_key(right))
                    .then_with(|| left.cmp(right))
            }),
            crate::AutomaticGrouping::Cwd | crate::AutomaticGrouping::Agent => {
                keys.sort_by(|left, right| {
                    let newest_start = |key: &str| {
                        groups
                            .get(key)
                            .into_iter()
                            .flat_map(|(_, members)| members)
                            .map(|(index, _)| self.processes[*index].process.created_at)
                            .max()
                            .unwrap_or(i64::MIN)
                    };
                    newest_start(right)
                        .cmp(&newest_start(left))
                        .then_with(|| left.cmp(right))
                });
            }
            crate::AutomaticGrouping::Ungrouped => {}
        }
        keys
    }

    pub(in crate::tui) fn rebuild_rows(&mut self) {
        let previous_identity = self
            .rows
            .get(self.selected)
            .and_then(SidebarListRow::identity)
            .map(str::to_string);
        let previous_process = self.selected_process_name().map(str::to_string);
        let mut rows = Vec::new();
        let mut lineage = self.lineage();
        // A query filters every section and unfolds the groups it leaves.
        let mut matcher = self.search_matcher();
        let searching = matcher.is_some();
        let matched = matcher
            .as_mut()
            .map(|matcher| self.process_matches(matcher));
        if let Some(matched) = &matched {
            lineage.retain_matches(matched);
        }

        if self.grouping.automatic == crate::AutomaticGrouping::Ungrouped {
            let mut roots: Vec<usize> = lineage.roots().collect();
            roots.sort_by(|left, right| self.family_order(&lineage, *left, *right));
            for root in roots {
                rows.extend(self.family_rows(&lineage, root, "auto:ungrouped", None));
            }
        } else {
            // A family is listed in its root's groups.
            let mut groups: HashMap<String, AutomaticGroup> = HashMap::new();
            for index in lineage.roots() {
                let process = &self.processes[index];
                for (key, label, location) in self.automatic_group_entries(process) {
                    groups
                        .entry(key)
                        .or_insert_with(|| (label, Vec::new()))
                        .1
                        .push((index, location));
                }
            }
            for (_, members) in groups.values_mut() {
                members.sort_by(|(left, _), (right, _)| self.family_order(&lineage, *left, *right));
            }
            let mode_key = self.grouping.automatic.state_key().to_string();
            let default_order = self.default_automatic_group_order(&groups);
            let custom_order = self.grouping.automatic_custom_order.contains(&mode_key);
            let order = self.grouping.automatic_order.entry(mode_key).or_default();
            if custom_order {
                order.retain(|key| groups.contains_key(key));
                for key in default_order {
                    if !order.contains(&key) {
                        order.push(key);
                    }
                }
            } else {
                *order = default_order;
            }
            for key in order.clone() {
                let Some((label, entries)) = groups.remove(&key) else {
                    continue;
                };
                let identity = format!("auto:{}:{}", self.grouping.automatic.state_key(), key);
                let collapsed = !searching && self.grouping.collapsed.contains(&identity);
                let members = entries
                    .iter()
                    .flat_map(|(index, _)| lineage.family(*index))
                    .map(|index| self.processes[index].process.name.clone())
                    .collect();
                rows.push(SidebarListRow::Group(SidebarGroupRow {
                    identity: identity.clone(),
                    key: key.clone(),
                    label,
                    kind: SidebarGroupKind::Automatic,
                    members,
                    collapsed,
                }));
                if !collapsed {
                    for (root, location) in entries {
                        rows.extend(self.family_rows(&lineage, root, &identity, location));
                    }
                }
            }
        }

        let manual_groups: Vec<_> = self
            .grouping
            .manual
            .iter()
            .map(|group| {
                let mut indices = self.group_process_indices(&group.processes);
                if let Some(matched) = &matched {
                    indices.retain(|index| matched[*index]);
                }
                (group, indices)
            })
            .filter(|(_, indices)| !searching || !indices.is_empty())
            .collect();
        if !manual_groups.is_empty() {
            rows.push(SidebarListRow::GroupTypeSeparator);
        }
        for (group, indices) in manual_groups {
            let identity = format!("manual:{}", group.id);
            let collapsed = !searching && self.grouping.collapsed.contains(&identity);
            let live_members = indices
                .iter()
                .map(|index| self.processes[*index].process.name.clone())
                .collect();
            rows.push(SidebarListRow::Group(SidebarGroupRow {
                identity: identity.clone(),
                key: group.id.clone(),
                label: group.name.clone(),
                kind: SidebarGroupKind::Manual,
                members: live_members,
                collapsed,
            }));
            if !collapsed {
                rows.extend(indices.into_iter().map(|process_index| {
                    SidebarListRow::Process {
                        process_index,
                        identity: format!(
                            "{identity}:process:{}",
                            self.processes[process_index].process.name
                        ),
                        location: None,
                        // A manual group lists its members flat, each
                        // naming the process that spawned it.
                        tree: render::RowTree::default(),
                        parent_note: self.processes[process_index]
                            .process
                            .spawned_by
                            .as_ref()
                            .map(crate::SpawnedBy::describe),
                    }
                }));
            }
        }

        let mut resumable = self.resumable_entry_indices();
        if let Some(matcher) = matcher.as_mut() {
            resumable.retain(|index| Self::history_matches(matcher, &self.history[*index]));
        }
        self.search.matches = matched.iter().flatten().filter(|matched| **matched).count()
            + if searching { resumable.len() } else { 0 };
        if !resumable.is_empty() {
            let collapsed = !searching && !self.grouping.resumable_expanded;
            rows.push(SidebarListRow::Group(SidebarGroupRow {
                identity: RESUMABLE_GROUP_IDENTITY.to_string(),
                key: RESUMABLE_GROUP_IDENTITY.to_string(),
                label: "Resumable".to_string(),
                kind: SidebarGroupKind::Resumable,
                members: resumable
                    .iter()
                    .map(|index| resumable_target(&self.history[*index]))
                    .collect(),
                collapsed,
            }));
            if !collapsed {
                rows.extend(
                    resumable
                        .into_iter()
                        .map(|entry_index| SidebarListRow::Resumable {
                            entry_index,
                            identity: format!(
                                "{RESUMABLE_GROUP_IDENTITY}:{}",
                                resumable_target(&self.history[entry_index])
                            ),
                        }),
                );
            }
        }

        self.rows = rows;
        self.selected = previous_identity
            .as_deref()
            .and_then(|identity| {
                self.rows
                    .iter()
                    .position(|row| row.identity() == Some(identity))
            })
            .or_else(|| {
                previous_process.as_deref().and_then(|name| {
                    self.rows.iter().position(|row| match row {
                        SidebarListRow::Process { process_index, .. } => {
                            self.processes[*process_index].process.name == name
                        }
                        _ => false,
                    })
                })
            })
            .unwrap_or_else(|| {
                self.rows
                    .iter()
                    .position(SidebarListRow::selectable)
                    .unwrap_or(0)
            });
    }

    /// Apply a grouping edit to the in-memory state and persist it on top of
    /// the state on disk now, keeping concurrent edits by other sidebars and
    /// `sidebar-action`s (the in-memory copy is reloaded on every snapshot).
    pub(in crate::tui) fn save_grouping(&mut self, change: crate::GroupingChange) -> Result<()> {
        change.apply(&mut self.grouping);
        crate::save_grouping_change(&change).map(drop)
    }

    /// [`Self::rebuild_rows`], returning the change that persists the
    /// automatic group order it derived for the current mode, if that order
    /// changed.
    pub(in crate::tui) fn rebuild_rows_tracking_automatic_order(
        &mut self,
    ) -> Option<crate::GroupingChange> {
        let mode = self.grouping.automatic.state_key().to_string();
        let previous = self.grouping.automatic_order.get(&mode).cloned();
        self.rebuild_rows();
        let order = self.grouping.automatic_order.get(&mode).cloned()?;
        (Some(&order) != previous.as_ref()).then_some(crate::GroupingChange::AutomaticOrder {
            mode,
            previous,
            order,
        })
    }

    /// The past agent session under the cursor, if the cursor is on one.
    pub(in crate::tui) fn selected_resumable(&self) -> Option<&crate::HistoryEntry> {
        if self.header_selected {
            return None;
        }
        match self.rows.get(self.selected) {
            Some(SidebarListRow::Resumable { entry_index, .. }) => self.history.get(*entry_index),
            _ => None,
        }
    }

    /// The agent's own title for the conversation in process `index`.
    pub(in crate::tui) fn agent_title(&self, index: usize) -> Option<&str> {
        self.processes
            .get(index)
            .and_then(|process| self.agent_titles.get(&process.process.name))
            .map(String::as_str)
    }

    /// History entries listed under Resumable, newest first: every session
    /// that is not running, from every project.
    pub(in crate::tui) fn resumable_entry_indices(&self) -> Vec<usize> {
        self.history
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.live)
            .map(|(index, _)| index)
            .collect()
    }

    /// Re-read the labels-changed stamp at most every [`LABELS_STAMP_POLL`],
    /// and request a history reload when `pmux status-labels` has rewritten
    /// it since: a label changed, usually because an agent's title arrived.
    pub(in crate::tui) fn check_labels_stamp(&mut self, now: Instant) {
        if self
            .labels_stamp_checked
            .is_some_and(|checked| now.saturating_duration_since(checked) < LABELS_STAMP_POLL)
        {
            return;
        }
        let first_check = self.labels_stamp_checked.is_none();
        self.labels_stamp_checked = Some(now);
        self.observe_labels_stamp(crate::state::read_labels_changed(), first_check);
    }

    /// Record the stamp's value. Any change after the first reading requests
    /// a reload; the first reading only sets the baseline, since the initial
    /// history load is already due then.
    pub(in crate::tui) fn observe_labels_stamp(
        &mut self,
        stamp: Option<String>,
        first_check: bool,
    ) {
        if !first_check && stamp != self.labels_stamp {
            self.history_reload_requested = true;
        }
        self.labels_stamp = stamp;
    }

    /// Whether the history catalog should be reloaded now, given the sorted
    /// names of the live processes. See [`Self::ensure_history_fetch`].
    pub(in crate::tui) fn history_fetch_due(&self, names: &[String], now: Instant) -> bool {
        match self.history_loaded_at {
            None => true,
            Some(loaded) => {
                self.history_reload_requested
                    || names != self.history_process_names.as_slice()
                    || (self.window_active
                        && now.saturating_duration_since(loaded) >= HISTORY_REFRESH_INTERVAL)
            }
        }
    }

    /// Start a background reload of the agent history catalog when it is due:
    /// on first use, when the set of live processes changed, when a label
    /// changed (see [`Self::check_labels_stamp`]), or periodically while this
    /// window is visible. At most one load runs at a time; a reload requested
    /// during a load runs after it.
    pub(in crate::tui) fn ensure_history_fetch(&mut self) {
        if self.history_loading {
            return;
        }
        let mut names: Vec<String> = self
            .processes
            .iter()
            .map(|process| process.process.name.clone())
            .collect();
        names.sort();
        if !self.history_fetch_due(&names, Instant::now()) {
            return;
        }
        self.history_reload_requested = false;
        self.history_loading = true;
        self.history_process_names = names;
        let tx = self.history_tx.clone();
        let panic_tx = self.history_tx.clone();
        render::spawn_worker(
            "pmux-history",
            move || {
                let query = crate::HistoryQuery {
                    include_live: true,
                    ..crate::HistoryQuery::default()
                };
                let _ = tx.send(crate::history(&query).map_err(|err| err.to_string()));
            },
            // A failed load keeps the previous catalog and allows a retry.
            move || {
                let _ = panic_tx.send(Err("the history reader crashed".to_string()));
            },
        );
    }

    /// Fold a finished history load into the Resumable group and the process
    /// titles. A failed load keeps the previous catalog. Returns whether one
    /// arrived.
    pub(in crate::tui) fn drain_history_results(&mut self) -> bool {
        let Ok(result) = self.history_rx.try_recv() else {
            return false;
        };
        self.history_loading = false;
        self.history_loaded_at = Some(Instant::now());
        match result {
            Ok(entries) => self.set_history(entries),
            Err(err) => self.message = Some(format!("Could not read agent history: {err}")),
        }
        true
    }

    pub(in crate::tui) fn set_history(&mut self, entries: Vec<crate::HistoryEntry>) {
        self.agent_titles = render::agent_titles(&entries);
        self.history = entries;
        // A past session resumed since, or gone from the catalog, loses its
        // drawer.
        let past: HashSet<String> = self
            .history
            .iter()
            .filter(|entry| !entry.live)
            .map(resumable_target)
            .collect();
        self.open_past_drawers
            .retain(|target| past.contains(target));
        self.past_drawer_cache
            .retain(|target, _| past.contains(target));
        self.rebuild_rows();
        self.normalize_viewport_line();
    }

    /// Whether the header still names a grouping mode just switched to.
    #[cfg(test)]
    pub(in crate::tui) fn grouping_highlighted(&self, now: Instant) -> bool {
        self.header_notice(now) == Some(HeaderNotice::Grouping)
    }

    /// What the header names after a change, while it still shows.
    pub(in crate::tui) fn header_notice(&self, now: Instant) -> Option<HeaderNotice> {
        self.header_notice
            .filter(|(_, until)| now < *until)
            .map(|(notice, _)| notice)
    }

    /// Name `notice` in the header for a moment.
    pub(in crate::tui) fn show_header_notice(&mut self, notice: HeaderNotice, now: Instant) {
        self.header_notice = Some((notice, now + HEADER_NOTICE_DURATION));
    }

    /// Show the automatic grouping mode `cycle_automatic_grouping` switched
    /// to, or why it could not. Returns the effect persisting the group order
    /// the new mode derives, if any.
    pub(in crate::tui) fn grouping_cycled(
        &mut self,
        result: Result<crate::AutomaticGrouping>,
    ) -> Vec<Effect> {
        self.grouping_cycled_at(result, Instant::now())
    }

    /// [`Self::grouping_cycled`] at `now`, which starts the header's
    /// highlight of the new mode.
    pub(in crate::tui) fn grouping_cycled_at(
        &mut self,
        result: Result<crate::AutomaticGrouping>,
        now: Instant,
    ) -> Vec<Effect> {
        let mode = match result {
            Ok(mode) => mode,
            Err(err) => {
                self.message = Some(format!("Could not change grouping: {err}"));
                return Vec::new();
            }
        };
        self.grouping.automatic = mode;
        let order_change = self.rebuild_rows_tracking_automatic_order();
        self.first_visible = 0;
        self.first_visible_line = 0;
        // The header names the new mode for a moment.
        self.show_header_notice(HeaderNotice::Grouping, now);
        order_change.map(Effect::SaveGrouping).into_iter().collect()
    }
}

/// What a process was last doing, and since when (epoch seconds).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tui) struct Course {
    state: Option<String>,
    since: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    fn agent(
        name: &str,
        pane: &str,
        created_at: i64,
        state: &str,
        updated_at: i64,
    ) -> crate::SidebarProcess {
        let mut process = test_process(Some("/work"), None);
        process.process.name = name.to_string();
        process.process.pane_id = pane.to_string();
        process.process.created_at = created_at;
        process.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: Some(state.to_string()),
            updated_at: Some(updated_at),
            source: None,
            reason: None,
            session_id: None,
        });
        process
    }

    /// What `apply_processes` does to the order, without reading the
    /// user's saved grouping.
    fn show(app: &mut SidebarApp, processes: Vec<crate::SidebarProcess>) {
        app.processes = processes;
        app.settle_courses();
        app.rebuild_rows();
    }

    fn process_order(app: &SidebarApp) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Process { process_index, .. } => {
                    Some(app.processes[*process_index].process.name.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn working_agents_list_first_and_move_only_when_their_state_changes() {
        let mut app = SidebarApp::new(None);
        app.grouping.automatic = crate::AutomaticGrouping::Ungrouped;
        show(
            &mut app,
            vec![
                agent("old-work", "%1", 10, "working", 100),
                agent("new-done", "%2", 30, "done", 100),
                agent("new-work", "%3", 20, "working", 100),
            ],
        );
        // Working first; at first sight each group is in start order.
        assert_eq!(process_order(&app), ["new-work", "old-work", "new-done"]);

        // Signals while working move nothing.
        show(
            &mut app,
            vec![
                agent("old-work", "%1", 10, "working", 500),
                agent("new-done", "%2", 30, "done", 100),
                agent("new-work", "%3", 20, "working", 400),
            ],
        );
        assert_eq!(process_order(&app), ["new-work", "old-work", "new-done"]);

        // Finishing drops an agent to the rest, newest change first; going
        // back to work puts it at the top.
        show(
            &mut app,
            vec![
                agent("old-work", "%1", 10, "done", 600),
                agent("new-done", "%2", 30, "working", 700),
                agent("new-work", "%3", 20, "working", 650),
            ],
        );
        assert_eq!(process_order(&app), ["new-done", "new-work", "old-work"]);
    }

    #[test]
    fn a_changed_labels_stamp_reloads_history_at_once_even_while_hidden() {
        let mut app = SidebarApp::new(None);
        let names = vec!["proc".to_string()];
        let now = Instant::now();
        app.history_loaded_at = Some(now);
        app.history_process_names = names.clone();
        app.window_active = false;

        // The first reading only sets the baseline.
        app.observe_labels_stamp(Some("1\n".into()), true);
        assert!(!app.history_fetch_due(&names, now));
        // The same value again changes nothing.
        app.observe_labels_stamp(Some("1\n".into()), false);
        assert!(!app.history_fetch_due(&names, now));
        // A new value requests a reload, though the window is hidden and the
        // periodic refresh is far off.
        app.observe_labels_stamp(Some("2\n".into()), false);
        assert!(app.history_fetch_due(&names, now));
    }

    #[test]
    fn a_stamp_appearing_for_the_first_time_after_startup_reloads_history() {
        let mut app = SidebarApp::new(None);
        let names: Vec<String> = Vec::new();
        let now = Instant::now();
        app.history_loaded_at = Some(now);
        app.observe_labels_stamp(None, true);
        assert!(!app.history_fetch_due(&names, now));
        app.observe_labels_stamp(Some("1\n".into()), false);
        assert!(app.history_fetch_due(&names, now));
    }

    #[test]
    fn the_stamp_is_read_at_most_every_poll_interval() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        let mut app = SidebarApp::new(None);
        let start = Instant::now();
        app.check_labels_stamp(start);
        assert_eq!(app.labels_stamp_checked, Some(start));
        app.check_labels_stamp(start + LABELS_STAMP_POLL / 2);
        assert_eq!(app.labels_stamp_checked, Some(start));
        app.check_labels_stamp(start + LABELS_STAMP_POLL);
        assert_eq!(app.labels_stamp_checked, Some(start + LABELS_STAMP_POLL));
    }

    #[test]
    fn periodic_history_refresh_still_needs_a_visible_window() {
        let mut app = SidebarApp::new(None);
        let names: Vec<String> = Vec::new();
        let loaded = Instant::now();
        app.history_loaded_at = Some(loaded);
        let later = loaded + HISTORY_REFRESH_INTERVAL;
        app.window_active = false;
        assert!(!app.history_fetch_due(&names, later));
        app.window_active = true;
        assert!(app.history_fetch_due(&names, later));
    }

    fn resumable_app(history: Vec<crate::HistoryEntry>) -> SidebarApp {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.processes = vec![test_process(Some("/work"), None)];
        app.set_history(history);
        app
    }

    fn resumable_ids(app: &SidebarApp) -> Vec<&str> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Resumable { entry_index, .. } => {
                    Some(app.history[*entry_index].session_id.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn folded_group_headers_fill_the_viewport_without_phantom_spacers() {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState {
            manual: (0..30)
                .map(|index| crate::ManualGroup {
                    id: format!("g{index}"),
                    name: format!("group {index}"),
                    processes: Vec::new(),
                })
                .collect(),
            collapsed: (0..30).map(|index| format!("manual:g{index}")).collect(),
            ..Default::default()
        };
        app.rebuild_rows();
        // The separator plus 30 folded headers, each exactly one line tall.
        assert_eq!(app.rows.len(), 31);
        assert!((0..31).all(|index| app.row_block_line_count(index) == 1));
        // 12 rows leave 10 content lines between the `more` indicators, so 10
        // one-line rows fit. Counting a separator after each folded header
        // stopped at 5 and left the rest of the sidebar blank.
        assert_eq!(app.visible_end(0, 12), 10);
    }

    #[test]
    fn resumable_group_comes_last_folded_and_excludes_live_sessions() {
        let mut live = test_history_entry("live", "/work", 30);
        live.live = true;
        live.pmux_process = Some("demo".to_string());
        let mut app = resumable_app(vec![
            live,
            test_history_entry("newer", "/a", 20),
            test_history_entry("older", "/b", 10),
        ]);

        let Some(SidebarListRow::Group(group)) = app.rows.last() else {
            panic!(
                "expected the Resumable header last, got {:?}",
                app.rows.last()
            );
        };
        assert_eq!(group.kind, SidebarGroupKind::Resumable);
        assert_eq!(group.identity, RESUMABLE_GROUP_IDENTITY);
        assert!(group.collapsed, "Resumable starts folded");
        assert_eq!(group.members, ["claude:newer", "claude:older"]);
        assert!(resumable_ids(&app).is_empty());

        app.grouping.resumable_expanded = true;
        app.rebuild_rows();
        assert_eq!(resumable_ids(&app), ["newer", "older"]);
        assert_eq!(
            app.rows.last().and_then(SidebarListRow::identity),
            Some("resumable:claude:older")
        );
    }

    #[test]
    fn no_resumable_sessions_means_no_resumable_group() {
        let app = resumable_app(Vec::new());
        assert!(app.rows.iter().all(|row| !matches!(
            row,
            SidebarListRow::Group(SidebarGroupRow {
                kind: SidebarGroupKind::Resumable,
                ..
            })
        )));
    }

    #[test]
    fn resumable_group_lists_every_project_without_a_cap() {
        let history: Vec<_> = (0..30)
            .map(|index| {
                test_history_entry(
                    &format!("s{index:02}"),
                    &format!("/proj{index}"),
                    100 - index,
                )
            })
            .collect();
        let mut app = resumable_app(history);
        app.grouping.resumable_expanded = true;
        let location = crate::OuterPaneLocation {
            pane_id: "%5".to_string(),
            pane_index: "1".to_string(),
            window_id: "@1".to_string(),
            window_index: "1".to_string(),
            window_name: "main".to_string(),
        };
        // A focused process in one project does not narrow the list.
        app.processes[0].process.current_path = Some("/proj3".to_string());
        app.processes[0].outer_location = Some(location.clone());
        app.processes[0].outer_locations = vec![location];
        app.focused_outer_pane = Some("%5".to_string());
        app.rebuild_rows();

        assert_eq!(resumable_ids(&app).len(), 30);
        assert_eq!(resumable_ids(&app).first(), Some(&"s00"));
        let Some(SidebarListRow::Group(group)) = app
            .rows
            .iter()
            .find(|row| row.identity() == Some(RESUMABLE_GROUP_IDENTITY))
        else {
            panic!("expected the Resumable header");
        };
        assert_eq!(group.label, "Resumable");
        assert_eq!(group.members.len(), 30);
    }

    #[test]
    fn resumable_rows_select_as_sessions_not_processes() {
        let mut app = resumable_app(vec![test_history_entry("past", "/a", 20)]);
        app.grouping.resumable_expanded = true;
        app.rebuild_rows();
        let row = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Resumable { .. }))
            .unwrap();
        app.selected = row;
        assert_eq!(
            app.selected_resumable().map(|e| e.session_id.as_str()),
            Some("past")
        );
        assert_eq!(app.selected_process_index(), None);
        assert_eq!(app.row_line_count(row), render::RESUMABLE_ROW_LINES);

        // Selection follows the session across a catalog refresh.
        let mut newer = test_history_entry("newer", "/a", 30);
        newer.title = Some("fresh".to_string());
        app.set_history(vec![newer, test_history_entry("past", "/a", 20)]);
        assert_eq!(
            app.selected_resumable().map(|e| e.session_id.as_str()),
            Some("past")
        );
    }

    #[test]
    fn agent_titles_come_from_live_history_entries() {
        let mut live = test_history_entry("live", "/work", 30);
        live.live = true;
        live.pmux_process = Some("demo".to_string());
        live.title = Some("Fix the relay".to_string());
        let mut app = resumable_app(vec![live]);
        assert_eq!(app.agent_title(0), Some("Fix the relay"));

        app.set_history(Vec::new());
        assert_eq!(app.agent_title(0), None);
    }

    #[test]
    fn a_process_attached_in_two_windows_appears_under_both() {
        let location = |window: &str, pane: &str, pane_index: &str| crate::OuterPaneLocation {
            pane_id: pane.to_string(),
            pane_index: pane_index.to_string(),
            window_id: format!("@{window}"),
            window_index: window.to_string(),
            window_name: format!("w{window}"),
        };
        let context = test_context();
        let mut process = test_process(Some("/work"), None);
        process.process.attached_clients = 2;
        process.outer_locations = vec![location("1", "%10", "2"), location("9", "%90", "1")];
        process.outer_location = Some(process.outer_locations[0].clone());
        let mut app = SidebarApp::new(None);
        app.processes = vec![process];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Window,
            ..Default::default()
        };
        app.rebuild_rows();

        let process_rows: Vec<usize> = app
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, SidebarListRow::Process { .. }))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(process_rows.len(), 2);
        // Each copy shows its own attachment: the pane here, the other window.
        let details: Vec<_> = process_rows
            .iter()
            .map(|row| {
                crate::sidebar_attachment_detail_at(
                    &app.processes[0],
                    app.row_location(*row),
                    &context,
                )
            })
            .collect();
        assert_eq!(
            details,
            [Some("pane 2".to_string()), Some("window 9".to_string())]
        );
        let identities: std::collections::HashSet<_> = process_rows
            .iter()
            .map(|row| app.flash_identity(*row))
            .collect();
        assert_eq!(identities.len(), 2, "each copy is its own flash target");

        // Focusing the window-9 pane selects that copy, and only it carries
        // the unfocused cursor highlight.
        app.active_outer_pane = Some("%90".to_string());
        app.focused_outer_pane = app.active_outer_pane.clone();
        app.follow_active_outer_process();
        assert_eq!(app.selected, process_rows[1]);
        assert!(!app.cursor_is_visible_for(process_rows[0]));
        assert!(app.cursor_is_visible_for(process_rows[1]));
        assert!(app.process_is_focused(0), "both copies render orange");

        app.focused_outer_pane = Some("%elsewhere".to_string());
        assert!(!app.process_is_focused(0));
    }

    #[test]
    fn automatic_groups_stay_above_manual_groups_and_members_are_duplicated() {
        let mut app = SidebarApp::new(None);
        let mut first = test_process(Some("/work/one"), None);
        first.process.name = "one".to_string();
        first.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        let mut second = test_process(Some("/work/two"), None);
        second.process.name = "two".to_string();
        second.process.agent = Some(crate::AgentInfo {
            kind: "codex".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        app.processes = vec![first, second];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Agent,
            manual: vec![crate::ManualGroup {
                id: "favorites".to_string(),
                name: "Favorites".to_string(),
                processes: vec!["one".to_string()],
            }],
            ..Default::default()
        };
        app.rebuild_rows();

        let separator = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::GroupTypeSeparator))
            .unwrap();
        assert!(app.rows[..separator].iter().any(|row| {
            matches!(row, SidebarListRow::Group(group) if group.kind == SidebarGroupKind::Automatic)
        }));
        assert!(app.rows[separator + 1..].iter().any(|row| {
            matches!(row, SidebarListRow::Group(group) if group.kind == SidebarGroupKind::Manual)
        }));
        assert_eq!(
            app.rows
                .iter()
                .filter(|row| matches!(
                    row,
                    SidebarListRow::Process {
                        process_index: 0,
                        ..
                    }
                ))
                .count(),
            2,
            "manual membership must not remove the automatic occurrence"
        );
    }

    #[test]
    fn manual_groups_remain_visible_in_ungrouped_mode() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/work/one"), None)];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Ungrouped,
            manual: vec![crate::ManualGroup {
                id: "saved".to_string(),
                name: "Saved".to_string(),
                processes: vec!["demo".to_string()],
            }],
            ..Default::default()
        };
        app.rebuild_rows();
        assert!(app.rows.iter().any(|row| {
            matches!(row, SidebarListRow::Group(group) if group.kind == SidebarGroupKind::Manual)
        }));
        assert_eq!(
            app.rows
                .iter()
                .filter(|row| matches!(row, SidebarListRow::Process { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn automatic_group_defaults_follow_window_index_and_recent_session_start() {
        let window_process = |name: &str, created_at: i64, window_index: Option<&str>| {
            let mut process = test_process(Some("/work"), None);
            process.process.name = name.to_string();
            process.process.created_at = created_at;
            process.outer_location = window_index.map(|index| crate::OuterPaneLocation {
                pane_id: format!("%{index}"),
                pane_index: "0".to_string(),
                window_id: format!("@{index}"),
                window_index: index.to_string(),
                window_name: format!("window-{index}"),
            });
            process
        };
        let mut other_session = window_process("other", 300, None);
        other_session.process.attached_clients = 1;
        let mut app = SidebarApp::new(None);
        app.processes = vec![
            window_process("unattached", 400, None),
            window_process("ten", 200, Some("10")),
            other_session,
            window_process("two", 100, Some("2")),
        ];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Window,
            ..Default::default()
        };
        app.rebuild_rows();
        let labels: Vec<_> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Group(group) => Some(group.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            [
                "window 2 · window-2",
                "window 10 · window-10",
                "Other tmux session",
                "Unattached"
            ]
        );

        let mut older_claude = test_process(Some("/old"), None);
        older_claude.process.name = "older-claude".to_string();
        older_claude.process.created_at = 10;
        older_claude.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        let mut recent_codex = test_process(Some("/new"), None);
        recent_codex.process.name = "recent-codex".to_string();
        recent_codex.process.created_at = 50;
        recent_codex.process.agent = Some(crate::AgentInfo {
            kind: "codex".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        app.processes = vec![older_claude, recent_codex];
        app.grouping = crate::SidebarGroupingState {
            automatic: crate::AutomaticGrouping::Agent,
            ..Default::default()
        };
        app.rebuild_rows();
        let keys: Vec<_> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Group(group) => Some(group.key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, ["codex", "claude"]);

        app.grouping
            .automatic_custom_order
            .insert("agent".to_string());
        app.grouping.automatic_order.insert(
            "agent".to_string(),
            vec!["claude".to_string(), "codex".to_string()],
        );
        app.rebuild_rows();
        let keys: Vec<_> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Group(group) => Some(group.key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, ["claude", "codex"]);
    }

    #[test]
    fn group_headers_use_large_fold_icons_and_the_process_chip_background() {
        assert_eq!(GROUP_FOLDED_ICON, "▶");
        assert_eq!(GROUP_EXPANDED_ICON, "▼");
        let chip = Color::Rgb(42, 49, 66);
        assert_eq!(group_header_background(chip, false), chip);
        assert_ne!(
            chip,
            group_header_background(chip, true),
            "selection should remain visible without changing the base fill"
        );
        assert_eq!(
            tmux_style_background("#[bg=#2a3142,fg=#8aadf4,bold]"),
            Some(chip)
        );
    }

    #[test]
    fn group_header_metadata_is_pinned_right_and_open_groups_add_a_blank_line() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/work/one"), None)];
        app.grouping = crate::SidebarGroupingState {
            manual: vec![crate::ManualGroup {
                id: "saved".to_string(),
                name: "A long group name".to_string(),
                processes: vec!["demo".to_string()],
            }],
            ..Default::default()
        };
        app.rebuild_rows();
        let header_index = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Group(_)))
            .unwrap();
        let SidebarListRow::Group(group) = &app.rows[header_index] else {
            unreachable!()
        };
        let line = group_header_line(group, 42);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text_width(&text), 42);
        assert!(text.ends_with("(1) · manual "));
        assert_eq!(app.row_block_line_count(header_index), 2);

        app.grouping.collapsed.insert("manual:saved".to_string());
        app.rebuild_rows();
        let header_index = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Group(_)))
            .unwrap();
        assert_eq!(app.row_block_line_count(header_index), 1);
    }
}
