//! Process drawers: which are open, the activity cache behind them, and
//! building their lines.

use super::*;

/// Identifies one process's cached activity. The name alone is not enough: a
/// process that exits and is replaced by a new one with the same name must
/// not show the old one's activity, so its pane and start time are included.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::tui) struct DrawerKey {
    name: String,
    pane_id: String,
    created_at: i64,
}

impl DrawerKey {
    pub(in crate::tui) fn of(process: &crate::SidebarProcess) -> Self {
        Self {
            name: process.process.name.clone(),
            pane_id: process.process.pane_id.clone(),
            created_at: process.process.created_at,
        }
    }
}

impl SidebarApp {
    pub(in crate::tui) fn any_drawers_open(&self) -> bool {
        !self.open_drawers.is_empty() || !self.open_past_drawers.is_empty()
    }

    pub(super) fn all_drawers_open(&self) -> bool {
        !self.processes.is_empty()
            && self
                .processes
                .iter()
                .all(|process| self.open_drawers.contains(&process.process.name))
    }

    pub(super) fn drawer_is_open(&self, index: usize) -> bool {
        self.processes
            .get(index)
            .is_some_and(|process| self.open_drawers.contains(&process.process.name))
    }

    /// How many extra lines the drawer adds under row `index` (0 when drawers are
    /// closed). These constants must match
    /// [`Self::build_drawer_lines_for`].
    pub(in crate::tui) fn drawer_line_count(&self, index: usize) -> usize {
        self.drawer_line_count_indented(index, 0)
    }

    /// [`Self::drawer_line_count`] for a row `indent` columns in, under its
    /// parent, whose drawer wraps its prompts that much narrower.
    pub(in crate::tui) fn drawer_line_count_indented(&self, index: usize, indent: usize) -> usize {
        if !self.drawer_is_open(index) {
            return 0;
        }
        let Some(process) = self.processes.get(index) else {
            return 0;
        };
        match self.drawer_cache.get(&DrawerKey::of(process)) {
            Some(crate::DrawerState::Ready(data)) => {
                let width = self.drawer_width.saturating_sub(indent as u16);
                DRAWER_READY_FIXED_LINES + drawer_prompt_lines(data, width).len()
            }
            Some(crate::DrawerState::Unavailable) => DRAWER_UNAVAIL_LINES,
            // Loading, or not yet requested.
            _ => DRAWER_LOADING_LINES,
        }
    }

    /// The open drawer's state for past session `entry_index`, or `None`
    /// while its drawer is closed. A drawer not yet loaded is `Loading`.
    fn past_drawer_state(&self, entry_index: usize) -> Option<&crate::DrawerState> {
        let target = resumable_target(self.history.get(entry_index)?);
        if !self.open_past_drawers.contains(&target) {
            return None;
        }
        Some(
            self.past_drawer_cache
                .get(&target)
                .unwrap_or(&crate::DrawerState::Loading),
        )
    }

    /// Lines the drawer adds under past session `entry_index`, as
    /// [`Self::drawer_line_count`] does for a process.
    pub(in crate::tui) fn past_drawer_line_count(&self, entry_index: usize) -> usize {
        match self.past_drawer_state(entry_index) {
            None => 0,
            Some(crate::DrawerState::Ready(data)) => {
                DRAWER_READY_FIXED_LINES + drawer_prompt_lines(data, self.drawer_width).len()
            }
            Some(crate::DrawerState::Unavailable) => DRAWER_UNAVAIL_LINES,
            Some(_) => DRAWER_LOADING_LINES,
        }
    }

    /// The drawer lines under past session `entry_index`; their count must
    /// match [`Self::past_drawer_line_count`].
    pub(in crate::tui) fn build_past_drawer_lines(
        &self,
        entry_index: usize,
        width: u16,
    ) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);
        match self.past_drawer_state(entry_index) {
            None => Vec::new(),
            Some(crate::DrawerState::Ready(data)) => self.ready_drawer_lines(data, width),
            Some(crate::DrawerState::Unavailable) => {
                let inner = usize::from(width).saturating_sub(2);
                let detail = self.history[entry_index]
                    .started_epoch
                    .map(|started| format!("started {} ago", crate::relative_age(started)))
                    .unwrap_or_default();
                vec![
                    drawer_content_line("· no activity in its transcript", dim),
                    drawer_content_line(crate::truncate_col(&detail, inner), dim),
                    drawer_bottom_line(width),
                ]
            }
            Some(_) => vec![
                drawer_content_line("loading activity…", dim),
                drawer_bottom_line(width),
            ],
        }
    }

    /// Parse the transcript of every open past-session drawer not yet
    /// loaded, each on a background thread.
    fn ensure_past_drawer_fetch(&mut self) {
        let pending: Vec<(String, PathBuf, String)> = self
            .history
            .iter()
            .filter(|entry| !entry.live)
            .map(|entry| (resumable_target(entry), entry))
            .filter(|(target, _)| {
                self.open_past_drawers.contains(target)
                    && !self.past_drawer_cache.contains_key(target)
            })
            .map(|(target, entry)| {
                (
                    target,
                    PathBuf::from(&entry.transcript),
                    entry.agent.clone(),
                )
            })
            .collect();
        for (target, path, agent) in pending {
            self.past_drawer_cache
                .insert(target.clone(), crate::DrawerState::Loading);
            let tx = self.past_drawer_tx.clone();
            let panic_tx = self.past_drawer_tx.clone();
            let panic_target = target.clone();
            render::spawn_worker(
                "pmux-past-drawer",
                move || {
                    let _ = tx.send((target, crate::load_transcript_activity(&path, &agent)));
                },
                move || {
                    let _ = panic_tx.send((panic_target, crate::DrawerState::Unavailable));
                },
            );
        }
    }

    /// Parse the transcripts of the past sessions on screen, one after another
    /// on a background thread, so their rows can show a prompt count. A past
    /// transcript no longer changes, so each is parsed once; the results share
    /// the cache behind past-session drawers.
    pub(in crate::tui) fn ensure_past_prompt_counts(&mut self, list_height: u16) {
        if !self.window_active || self.past_count_running.load(Ordering::SeqCst) {
            return;
        }
        let end = self
            .visible_end(self.first_visible, list_height)
            .min(self.rows.len());
        let pending: Vec<(String, PathBuf, String)> = self
            .rows
            .get(self.first_visible..end)
            .unwrap_or_default()
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Resumable { entry_index, .. } => self.history.get(*entry_index),
                _ => None,
            })
            .filter(|entry| !entry.live)
            .map(|entry| (resumable_target(entry), entry))
            .filter(|(target, _)| !self.past_drawer_cache.contains_key(target))
            .map(|(target, entry)| {
                (
                    target,
                    PathBuf::from(&entry.transcript),
                    entry.agent.clone(),
                )
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        for (target, _, _) in &pending {
            self.past_drawer_cache
                .insert(target.clone(), crate::DrawerState::Loading);
        }
        self.past_count_running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.past_count_running);
        let panic_running = Arc::clone(&self.past_count_running);
        let tx = self.past_drawer_tx.clone();
        render::spawn_worker(
            "pmux-past-prompts",
            move || {
                for (target, path, agent) in pending {
                    let state = crate::load_transcript_activity(&path, &agent);
                    if tx.send((target, state)).is_err() {
                        break;
                    }
                }
                running.store(false, Ordering::SeqCst);
            },
            move || panic_running.store(false, Ordering::SeqCst),
        );
    }

    /// The prompt count shown on past session `entry_index`'s row, once parsed.
    pub(in crate::tui) fn past_prompt_label(&self, entry_index: usize) -> Option<String> {
        let target = resumable_target(self.history.get(entry_index)?);
        match self.past_drawer_cache.get(&target)? {
            crate::DrawerState::Ready(data) => Some(render::prompt_label(data)),
            _ => None,
        }
    }

    /// Update width-dependent drawer heights while keeping the selected row at
    /// the same screen position during a sidebar resize.
    pub(in crate::tui) fn update_drawer_width(&mut self, width: u16, list_height: u16) {
        if self.drawer_width == width {
            return;
        }
        let anchor = (self.any_drawers_open() && self.selected_is_visible(list_height))
            .then(|| self.selected_viewport_line());
        self.drawer_width = width;
        if let Some(viewport_line) = anchor {
            self.anchor_selected_at(viewport_line);
        }
    }

    /// Toggle only the selected process drawer while anchoring its process row
    /// at the same viewport line.
    /// Toggle the selected row: a group header folds or unfolds (returning
    /// the effect that persists it), a process or past session opens or
    /// closes its drawer.
    pub(in crate::tui) fn toggle_selected_drawer(&mut self, list_height: u16) -> Vec<Effect> {
        if self.header_selected {
            return Vec::new();
        }
        if let Some(group) = self.selected_group().cloned() {
            let collapsed = !group.collapsed;
            let change = if group.kind == SidebarGroupKind::Resumable {
                crate::GroupingChange::ResumableExpanded(!collapsed)
            } else {
                crate::GroupingChange::Fold {
                    identity: group.identity.clone(),
                    collapsed,
                }
            };
            change.apply(&mut self.grouping);
            self.rebuild_rows();
            self.selected = self
                .rows
                .iter()
                .position(|row| row.identity() == Some(group.identity.as_str()))
                .unwrap_or(self.selected.min(self.rows.len().saturating_sub(1)));
            self.ensure_visible(list_height);
            return vec![Effect::SaveGrouping(change)];
        }
        let anchor = self
            .selected_is_visible(list_height)
            .then(|| self.selected_viewport_line());
        self.drawer_axis_hint_until = None;
        if let Some(entry) = self.selected_resumable() {
            let target = resumable_target(entry);
            if !self.open_past_drawers.remove(&target) {
                self.open_past_drawers.insert(target);
            }
            self.ensure_past_drawer_fetch();
            if let Some(viewport_line) = anchor {
                self.anchor_selected_at(viewport_line);
            } else {
                self.ensure_visible(list_height);
            }
            return Vec::new();
        }
        let Some(name) = self.selected_process_name().map(str::to_string) else {
            return Vec::new();
        };
        if !self.open_drawers.remove(&name) {
            self.open_drawers.insert(name);
        }
        self.ensure_drawer_fetch();
        if let Some(viewport_line) = anchor {
            self.anchor_selected_at(viewport_line);
        } else {
            self.ensure_visible(list_height);
        }
        Vec::new()
    }

    /// Fold or unfold the section holding the selected row: a group header's
    /// own group, or the group a process or past session is listed under.
    /// The cursor lands on that group's header. Ungrouped processes have no
    /// section, so this does nothing there.
    pub(in crate::tui) fn toggle_selected_section(&mut self, list_height: u16) -> Vec<Effect> {
        if self.header_selected || self.rows.is_empty() {
            return Vec::new();
        }
        let selected = self.selected.min(self.rows.len() - 1);
        let header = self.rows[..=selected].iter().rposition(|row| {
            matches!(
                row,
                SidebarListRow::Group(_) | SidebarListRow::GroupTypeSeparator
            )
        });
        match header {
            Some(index) if matches!(self.rows[index], SidebarListRow::Group(_)) => {
                self.selected = index;
                self.toggle_selected_drawer(list_height)
            }
            _ => Vec::new(),
        }
    }

    /// Toggle every process drawer from the header graph control. A partially
    /// open list expands to all; only an already-all-open list collapses all.
    pub(in crate::tui) fn toggle_all_drawers(&mut self) {
        self.drawer_axis_hint_until = None;
        if self.all_drawers_open() {
            self.open_drawers.clear();
            self.open_past_drawers.clear();
        } else {
            self.open_drawers = self
                .processes
                .iter()
                .map(|process| process.process.name.clone())
                .collect();
            self.ensure_drawer_fetch();
        }
        self.normalize_viewport_line();
    }

    /// Spawn background parses for open processes without a cached result.
    /// Idempotent per name (a `Loading` marker is inserted immediately so later
    /// ticks skip re-spawning).
    pub(in crate::tui) fn ensure_drawer_fetch(&mut self) {
        let pending: Vec<DrawerKey> = self
            .processes
            .iter()
            .filter(|entry| self.open_drawers.contains(&entry.process.name))
            .map(DrawerKey::of)
            .filter(|key| !self.drawer_cache.contains_key(key))
            .collect();
        for key in pending {
            self.drawer_cache
                .insert(key.clone(), crate::DrawerState::Loading);
            let tx = self.drawer_tx.clone();
            let panic_tx = self.drawer_tx.clone();
            let panic_key = key.clone();
            render::spawn_worker(
                "pmux-drawer",
                move || {
                    let state = crate::load_activity(&key.name);
                    let _ = tx.send((key, state));
                },
                move || {
                    let _ = panic_tx.send((panic_key, crate::DrawerState::Unavailable));
                },
            );
        }
    }

    /// Re-parse every agent process's activity on one background thread, so
    /// each row can show its prompt count without opening its drawer. Runs
    /// immediately for an agent with nothing cached yet, otherwise every
    /// [`ACTIVITY_REFRESH`] while the window is visible. Results land in the
    /// drawer cache, which also keeps open drawers current.
    pub(in crate::tui) fn ensure_activity_refresh(&mut self, now: Instant) {
        if !self.window_active || self.activity_refresh_running.load(Ordering::SeqCst) {
            return;
        }
        let keys: Vec<DrawerKey> = self
            .processes
            .iter()
            .filter(|entry| entry.process.agent.is_some())
            .map(DrawerKey::of)
            .collect();
        if keys.is_empty() {
            return;
        }
        let uncached = keys.iter().any(|key| !self.drawer_cache.contains_key(key));
        let due = self
            .activity_refreshed_at
            .is_none_or(|at| now.duration_since(at) >= ACTIVITY_REFRESH);
        if !uncached && !due {
            return;
        }
        self.activity_refreshed_at = Some(now);
        self.activity_refresh_running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.activity_refresh_running);
        let panic_running = Arc::clone(&self.activity_refresh_running);
        let tx = self.drawer_tx.clone();
        render::spawn_worker(
            "pmux-activity",
            move || {
                for key in keys {
                    let state = crate::load_activity(&key.name);
                    if tx.send((key, state)).is_err() {
                        break;
                    }
                }
                running.store(false, Ordering::SeqCst);
            },
            // Let the next interval retry rather than wedging the refresh.
            move || panic_running.store(false, Ordering::SeqCst),
        );
    }

    /// Cache `state` as process `index`'s activity, as a finished fetch would.
    #[cfg(test)]
    pub(in crate::tui) fn cache_activity(&mut self, index: usize, state: crate::DrawerState) {
        let key = DrawerKey::of(&self.processes[index]);
        self.drawer_cache.insert(key, state);
    }

    /// The prompt count shown on an agent row's second line, once parsed.
    pub(in crate::tui) fn prompt_label(&self, process_index: usize) -> Option<String> {
        let process = self.processes.get(process_index)?;
        process.process.agent.as_ref()?;
        match self.drawer_cache.get(&DrawerKey::of(process))? {
            crate::DrawerState::Ready(data) => Some(render::prompt_label(data)),
            _ => None,
        }
    }

    /// Fold finished drawer fetches into the cache. Returns whether any arrived.
    pub(in crate::tui) fn drain_drawer_results(&mut self, list_height: u16) -> bool {
        let anchor = (self.any_drawers_open() && self.selected_is_visible(list_height))
            .then(|| self.selected_viewport_line());
        let mut changed = false;
        while let Ok((target, state)) = self.past_drawer_rx.try_recv() {
            // Dropped by a history reload meanwhile: leave it out.
            if let Some(cached) = self.past_drawer_cache.get_mut(&target) {
                *cached = state;
                changed = true;
            }
        }
        while let Ok((key, state)) = self.drawer_rx.try_recv() {
            // A fetch that outlived its process must not resurrect its entry.
            if self
                .processes
                .iter()
                .any(|process| DrawerKey::of(process) == key)
            {
                self.drawer_cache.insert(key, state);
                changed = true;
            }
        }
        if changed && let Some(viewport_line) = anchor {
            self.anchor_selected_at(viewport_line);
        }
        changed
    }

    /// Build the styled drawer lines for process row `index` (empty when drawers
    /// are closed). Line counts here must match [`Self::drawer_line_count`].
    pub(in crate::tui) fn build_drawer_lines_for(
        &self,
        index: usize,
        width: u16,
    ) -> Vec<Line<'static>> {
        if !self.drawer_is_open(index) {
            return Vec::new();
        }
        let inner = usize::from(width).saturating_sub(2);
        let dim = Style::default().fg(Color::DarkGray);
        let loading = || {
            vec![
                drawer_content_line("loading activity…", dim),
                drawer_bottom_line(width),
            ]
        };
        let Some(process) = self.processes.get(index) else {
            return loading();
        };
        match self.drawer_cache.get(&DrawerKey::of(process)) {
            Some(crate::DrawerState::Ready(data)) => self.ready_drawer_lines(data, width),
            Some(crate::DrawerState::Unavailable) => {
                let age = crate::relative_age(process.process.created_at);
                let mut detail = format!("{age} old");
                if process.process.attached_clients > 0 {
                    detail.push_str(&format!(" · {} attached", process.process.attached_clients));
                }
                if let Some(cwd) = crate::sidebar_process_cwd(process) {
                    detail.push_str(&format!(" · {}", crate::display_path(cwd)));
                }
                vec![
                    drawer_content_line("· no agent activity data", dim),
                    drawer_content_line(crate::truncate_col(&detail, inner), dim),
                    drawer_bottom_line(width),
                ]
            }
            // Loading, or not yet requested.
            _ => loading(),
        }
    }

    fn ready_drawer_lines(&self, data: &crate::ActivityData, width: u16) -> Vec<Line<'static>> {
        let inner = usize::from(width).saturating_sub(2);
        let metric_color = drawer_metric_color(self.drawer_metric);
        let prompt_lines = drawer_prompt_lines(data, width);
        let mut lines = Vec::with_capacity(DRAWER_READY_FIXED_LINES + prompt_lines.len());
        lines.push(drawer_stats_line(data));

        let graph_width = inner.max(1);
        let mode = crate::AXIS_PRESETS[self.drawer_axis].1;
        let series = data.series(self.drawer_metric);
        let buckets = if self.drawer_metric.peaks() {
            crate::warp_bucket_peaks(series, graph_width * 2, mode)
        } else {
            crate::warp_buckets(series, graph_width * 2, mode)
        };
        for row in crate::braille_rows(&buckets, graph_width, DRAWER_GRAPH_ROWS, true) {
            lines.push(drawer_content_line(row, Style::default().fg(metric_color)));
        }

        for prompt_line in prompt_lines {
            lines.push(drawer_content_line(
                prompt_line,
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(drawer_bottom_line(width));
        lines
    }

    pub(in crate::tui) fn drawer_header_label(&self, now: Instant) -> Option<&'static str> {
        self.any_drawers_open().then(|| {
            if self.drawer_axis_hint_until.is_some_and(|until| now < until) {
                crate::AXIS_PRESETS[self.drawer_axis].0
            } else {
                self.drawer_metric.label()
            }
        })
    }

    pub(in crate::tui) fn cycle_drawer_metric(&mut self) {
        self.cycle_drawer_metric_at(Instant::now());
    }

    /// [`Self::cycle_drawer_metric`] at `now`, which starts the header's
    /// note of the new graph.
    pub(in crate::tui) fn cycle_drawer_metric_at(&mut self, now: Instant) {
        self.drawer_metric = self.drawer_metric.next();
        self.drawer_axis_hint_until = None;
        self.show_header_notice(HeaderNotice::Graph, now);
    }

    pub(in crate::tui) fn cycle_drawer_axis(&mut self, now: Instant) {
        self.drawer_axis = (self.drawer_axis + 1) % crate::AXIS_PRESETS.len();
        self.drawer_axis_hint_until = Some(now + DRAWER_AXIS_HINT_DURATION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    fn drawer_key(app: &SidebarApp, name: &str) -> DrawerKey {
        DrawerKey::of(
            app.processes
                .iter()
                .find(|process| process.process.name == name)
                .expect("a process with that name"),
        )
    }

    fn cache_drawer(app: &mut SidebarApp, name: &str, state: crate::DrawerState) {
        let key = drawer_key(app, name);
        app.drawer_cache.insert(key, state);
    }

    fn cache_drawer_for(
        app: &mut SidebarApp,
        process: &crate::SidebarProcess,
        state: crate::DrawerState,
    ) {
        app.drawer_cache.insert(DrawerKey::of(process), state);
    }

    #[test]
    fn drawer_cache_is_pruned_to_live_processes_and_not_inherited_by_a_reused_name() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        let mut app = SidebarApp::new(None);
        let mut gone = test_process(Some("/w"), None);
        gone.process.name = "gone".to_string();
        let demo = test_process(Some("/w"), None);
        app.apply_processes(vec![demo.clone(), gone.clone()], 20);
        cache_drawer(&mut app, "demo", ready_activity());
        cache_drawer(&mut app, "gone", ready_activity());

        app.apply_processes(vec![demo.clone()], 20);
        assert_eq!(app.drawer_cache.len(), 1, "an exited process is dropped");

        // A new process reusing the name starts without the old activity.
        let mut reborn = demo.clone();
        reborn.process.created_at += 60;
        reborn.process.pane_id = "%77".to_string();
        app.apply_processes(vec![reborn], 20);
        assert!(app.drawer_cache.is_empty());
        open_test_drawers(&mut app, &["demo"]);
        assert_eq!(app.drawer_line_count(0), DRAWER_LOADING_LINES);

        // A fetch that finishes after its process was replaced is discarded.
        app.drawer_tx
            .send((DrawerKey::of(&demo), ready_activity()))
            .unwrap();
        assert!(!app.drain_drawer_results(20));
        assert!(app.drawer_cache.is_empty());
    }

    #[test]
    fn drawer_line_count_tracks_open_state_and_cache() {
        let mut app = SidebarApp::new(None);
        let mut second = test_process(Some("/w2"), None);
        second.process.name = "second".to_string();
        app.processes = vec![test_process(Some("/w"), None), second];
        // Closed: no extra lines.
        assert_eq!(app.drawer_line_count(0), 0);
        open_test_drawers(&mut app, &["demo"]);
        // Only explicitly opened rows contribute a drawer.
        assert_eq!(app.drawer_line_count(0), DRAWER_LOADING_LINES);
        assert_eq!(app.drawer_line_count(1), 0);
        cache_drawer(&mut app, "demo", crate::DrawerState::Unavailable);
        assert_eq!(app.drawer_line_count(0), DRAWER_UNAVAIL_LINES);
        cache_drawer(&mut app, "demo", ready_activity());
        assert_eq!(app.drawer_line_count(0), DRAWER_READY_FIXED_LINES + 1);
        // Out-of-range row has no drawer.
        assert_eq!(app.drawer_line_count(2), 0);
    }

    #[test]
    fn agent_rows_show_their_prompt_count_once_activity_is_parsed() {
        let mut app = SidebarApp::new(None);
        let mut agent = test_process(Some("/w"), None);
        agent.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        let mut plain = test_process(Some("/w"), None);
        plain.process.name = "plain".to_string();
        app.processes = vec![agent, plain];
        assert_eq!(app.prompt_label(0), None, "nothing until it is parsed");
        cache_drawer(&mut app, "demo", ready_activity());
        assert_eq!(app.prompt_label(0).as_deref(), Some("3p"));
        cache_drawer(&mut app, "plain", ready_activity());
        assert_eq!(app.prompt_label(1), None, "non-agents never show one");
    }

    #[test]
    fn activity_refresh_starts_for_uncached_agents_only_while_visible() {
        let mut app = SidebarApp::new(None);
        let mut agent = test_process(Some("/w"), None);
        agent.process.name = "pmux-test-no-such-agent-session".to_string();
        agent.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
        app.processes = vec![agent];
        let now = Instant::now();
        app.window_active = false;
        app.ensure_activity_refresh(now);
        assert_eq!(
            app.activity_refreshed_at, None,
            "hidden windows do not parse"
        );
        app.window_active = true;
        app.ensure_activity_refresh(now);
        assert_eq!(app.activity_refreshed_at, Some(now));
    }

    #[test]
    fn build_drawer_lines_ready_height_matches_count() {
        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/w"), None)];
        open_test_drawers(&mut app, &["demo"]);
        cache_drawer(&mut app, "demo", ready_activity());
        let lines = app.build_drawer_lines_for(0, 40);
        assert_eq!(lines.len(), DRAWER_READY_FIXED_LINES + 1);
        assert_eq!(lines.len(), app.drawer_line_count(0));
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // Stats line carries terse numbers (the graph metric lives in the
        // header); summary is present; the middle rows are braille.
        // The prompt count moved to the process row's second line.
        assert!(text[0].contains("46k in / 12k out · +10/-4"));
        assert!(text[0].starts_with("│   46k in"));
        assert!(!text[0].contains("3p"));
        assert!(!text[0].contains("idle"));
        assert!(!text[0].contains("m:"));
        for (content, color) in [
            ("+10", Color::Green),
            ("-4", Color::LightRed),
            ("46k in", Color::LightMagenta),
            ("12k out", Color::Yellow),
        ] {
            assert!(
                lines[0]
                    .spans
                    .iter()
                    .any(|span| span.content == content && span.style.fg == Some(color)),
                "missing styled metric {content:?}"
            );
        }
        assert_eq!(
            lines[0]
                .spans
                .iter()
                .filter(|span| span.content == " · ")
                .count(),
            1
        );
        assert!(text.iter().any(|l| l.contains("do the thing")));
        assert!(
            text[1]
                .chars()
                .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
        );
        assert!(
            text[..text.len() - 1]
                .iter()
                .all(|line| line.starts_with('│'))
        );
        assert!(text.last().is_some_and(|line| line.starts_with('└')));
        assert!(!text.last().is_some_and(|line| line.ends_with('┘')));
        assert_eq!(app.drawer_header_label(Instant::now()), Some("↓"));
    }

    #[test]
    fn latest_prompt_wraps_to_three_lines_and_then_ellipsizes() {
        let lines = wrap_text_lines(
            "one two three four five six seven eight nine ten eleven twelve",
            14,
            3,
        );
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| text_width(line) <= 14));
        assert!(lines[2].ends_with('…'));

        let mut app = SidebarApp::new(None);
        app.processes = vec![test_process(Some("/w"), None)];
        open_test_drawers(&mut app, &["demo"]);
        app.drawer_width = 16;
        let mut activity = ready_activity();
        let crate::DrawerState::Ready(data) = &mut activity else {
            unreachable!()
        };
        data.summary =
            Some("one two three four five six seven eight nine ten eleven twelve".to_string());
        data.recent_prompts = vec![data.summary.clone().unwrap()];
        cache_drawer(&mut app, "demo", activity);

        let drawer = app.build_drawer_lines_for(0, 16);
        assert_eq!(drawer.len(), DRAWER_READY_FIXED_LINES + 3);
        assert_eq!(drawer.len(), app.drawer_line_count(0));
        let prompt_text: Vec<String> = drawer[1 + super::DRAWER_GRAPH_ROWS..drawer.len() - 1]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(prompt_text.len(), 3);
        assert!(prompt_text[2].ends_with('…'));
    }

    #[test]
    fn enter_on_process_toggles_only_that_drawer_and_marks_loading() {
        let mut app = SidebarApp::new(None);
        let mut second = test_process(Some("/other"), None);
        second.process.name = "other".to_string();
        app.processes = vec![test_process(Some("/w"), None), second];
        app.toggle_selected_drawer(20);
        assert!(app.drawer_is_open(0));
        assert!(!app.drawer_is_open(1));
        // A fetch was kicked off only for the opened process.
        assert!(matches!(
            app.drawer_cache.get(&drawer_key(&app, "demo")),
            Some(crate::DrawerState::Loading)
        ));
        assert!(!app.drawer_cache.contains_key(&drawer_key(&app, "other")));
        app.move_cursor(1, 20);
        assert!(app.drawer_is_open(0));
        assert!(!app.drawer_is_open(1));
        app.toggle_selected_drawer(20);
        assert!(app.drawer_is_open(0));
        assert!(app.drawer_is_open(1));
    }

    #[test]
    fn header_graph_target_toggles_all_drawers_without_following_navigation() {
        let mut app = SidebarApp::new(None);
        app.processes = (0..3)
            .map(|index| {
                let mut process = test_process(Some("/work"), None);
                process.process.name = format!("process-{index}");
                process
            })
            .collect();

        app.move_cursor(-1, 20);
        assert!(app.header_selected);
        assert_eq!(app.selected_name(), None);
        app.toggle_all_drawers();
        assert!(app.all_drawers_open());

        app.move_cursor(1, 20);
        assert!(!app.header_selected);
        assert_eq!(app.selected, 0);
        app.toggle_selected_drawer(20);
        assert!(!app.drawer_is_open(0));
        assert!(app.drawer_is_open(1));
        app.move_cursor(1, 20);
        assert!(!app.drawer_is_open(0));
        assert!(app.drawer_is_open(1));

        app.header_selected = true;
        app.toggle_all_drawers();
        assert!(app.all_drawers_open(), "partial state expands to all");
        app.toggle_all_drawers();
        assert!(!app.any_drawers_open());
    }

    #[test]
    fn toggling_and_loading_drawers_preserves_the_selected_screen_row() {
        let mut app = SidebarApp::new(None);
        for index in 0..8 {
            let mut process = test_process(Some("/w"), None);
            process.process.name = format!("process-{index}");
            cache_drawer_for(&mut app, &process, crate::DrawerState::Unavailable);
            app.processes.push(process);
        }
        app.first_visible = 1;
        app.selected = 4;
        open_test_drawers(&mut app, &["process-1"]);
        let selected_line = app.selected_viewport_line();

        app.toggle_selected_drawer(20);

        assert!(app.drawer_is_open(1));
        assert!(app.drawer_is_open(4));
        assert_eq!(app.selected_viewport_line(), selected_line);
        app.ensure_visible(20);
        assert_eq!(app.selected_viewport_line(), selected_line);

        // A completed activity load above the cursor changes that drawer's
        // height, but the selected process stays anchored on the same line.
        app.drawer_tx
            .send((drawer_key(&app, "process-1"), ready_activity()))
            .unwrap();
        app.drain_drawer_results(20);
        assert_eq!(app.selected_viewport_line(), selected_line);
        assert!(app.first_visible_line > 0);
        app.ensure_visible(20);
        assert_eq!(app.selected_viewport_line(), selected_line);

        app.toggle_selected_drawer(20);
        assert!(app.drawer_is_open(1));
        assert!(!app.drawer_is_open(4));
        assert_eq!(app.selected_viewport_line(), selected_line);
    }

    #[test]
    fn moving_down_reveals_the_selected_drawers_bottom_outline() {
        let mut app = SidebarApp::new(None);
        for index in 0..4 {
            let mut process = test_process(Some("/work"), None);
            process.process.name = format!("process-{index}");
            cache_drawer_for(&mut app, &process, ready_activity());
            app.processes.push(process);
        }
        open_test_drawers(&mut app, &["process-1"]);

        // Eight content lines cannot fit the preceding process plus the complete
        // selected drawer. Navigation must reveal its bottom border.
        app.move_cursor(1, 10);

        let viewport_start = app.viewport_start_line();
        let selected_bottom = app.row_start_line(app.selected) + app.row_line_count(app.selected);
        assert!(selected_bottom <= viewport_start + list_content_height(10));
        assert!(viewport_start > 0);
    }

    #[test]
    fn following_an_outer_pane_reveals_the_complete_selected_drawer() {
        let mut app = SidebarApp::new(Some("%sidebar".to_string()));
        app.active_outer_pane = Some("%host".to_string());
        for index in 0..4 {
            let mut process = test_process(Some("/work"), Some("/work"));
            process.process.name = format!("process-{index}");
            process.outer_location = (index == 1).then(|| crate::OuterPaneLocation {
                pane_id: "%host".to_string(),
                pane_index: "1".to_string(),
                window_id: "@1".to_string(),
                window_index: "1".to_string(),
                window_name: "work".to_string(),
            });
            cache_drawer_for(&mut app, &process, ready_activity());
            app.processes.push(process);
        }
        open_test_drawers(&mut app, &["process-1"]);

        assert!(app.follow_active_outer_process());
        let available = list_content_height(10);
        let selected_bottom = app.row_start_line(app.selected) + app.row_line_count(app.selected);
        let minimum_viewport_start = selected_bottom.saturating_sub(available);
        app.ensure_followed_process_visible(10);

        assert!(
            selected_bottom <= app.viewport_start_line() + list_content_height(10),
            "the selected drawer bottom must remain on screen after a pane jump"
        );
        assert_eq!(
            app.viewport_start_line(),
            minimum_viewport_start + FOLLOWED_PROCESS_BOTTOM_MARGIN_LINES,
            "following a process leaves three readable rows beneath its drawer"
        );
        assert!(app.first_visible_line > 0);
    }

    #[test]
    fn past_sessions_on_screen_show_their_prompt_count_once_parsed() {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.grouping.resumable_expanded = true;
        let transcript = tempfile::NamedTempFile::new().unwrap();
        let prompt = |text: &str| {
            serde_json::json!({
                "timestamp": "2026-10-01T10:00:00Z",
                "message": {"role": "user", "content": text}
            })
            .to_string()
        };
        std::fs::write(
            transcript.path(),
            format!("{}\n{}\n", prompt("fix the bug"), prompt("add a test")),
        )
        .unwrap();
        let mut entry = test_history_entry("past", "/work", 10);
        entry.transcript = transcript.path().display().to_string();
        app.set_history(vec![entry]);
        assert_eq!(app.past_prompt_label(0), None, "nothing until it is parsed");

        app.window_active = false;
        app.ensure_past_prompt_counts(20);
        assert!(
            app.past_drawer_cache.is_empty(),
            "hidden windows do not parse"
        );

        app.window_active = true;
        app.ensure_past_prompt_counts(20);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.drain_drawer_results(20) {
            assert!(
                Instant::now() < deadline,
                "the transcript parse never arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.past_prompt_label(0).as_deref(), Some("2p"));
        // Parsed for its count, the drawer is still closed.
        assert!(!app.any_drawers_open());
    }

    #[test]
    fn a_past_session_opens_a_drawer_from_its_transcript_like_a_process() {
        let mut app = SidebarApp::new(None);
        app.grouping = crate::SidebarGroupingState::default();
        app.grouping.resumable_expanded = true;
        let transcript = tempfile::NamedTempFile::new().unwrap();
        let mut entry = test_history_entry("past", "/work", 10);
        entry.transcript = transcript.path().display().to_string();
        app.set_history(vec![entry]);
        let row = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarListRow::Resumable { .. }))
            .unwrap();
        app.selected = row;
        assert_eq!(app.row_line_count(row), render::RESUMABLE_ROW_LINES);

        assert!(app.toggle_selected_drawer(20).is_empty());
        assert!(app.any_drawers_open());
        assert_eq!(
            app.row_line_count(row),
            render::RESUMABLE_ROW_LINES + DRAWER_LOADING_LINES
        );
        // The background parse lands: an empty transcript still parses.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.drain_drawer_results(20) {
            assert!(
                Instant::now() < deadline,
                "the transcript parse never arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let lines = app.build_past_drawer_lines(0, 40);
        assert_eq!(lines.len(), app.past_drawer_line_count(0));
        assert!(lines.len() >= DRAWER_READY_FIXED_LINES);

        // Resumed elsewhere (gone from the past sessions): the drawer goes.
        app.set_history(Vec::new());
        assert!(!app.any_drawers_open());
        assert!(app.past_drawer_cache.is_empty());
    }
}
