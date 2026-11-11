//! Custom attach/switch picker TUI.
//!
//! The session picker behind `pmux attach` / `pmux a` and the
//! the in-pmux `prefix+s` switch. It renders the same multi-line rows as the
//! sidebar (through [`crate::render`], the single source of truth for row
//! appearance), supports type-to-filter plus arrow-key navigation, and can
//! return either a live process to attach or a past agent session to resume.
//! Rows update live from the same `pmux events` stream the sidebar
//! consumes, and optional Dredge full-text results, agent titles, and prompt
//! counts all load off the UI thread.

use crate::dredge::{SearchHit, SearchUpdate};
use crate::keys::{DefaultKeys, KeyAction, KeyChord, KeyLookup, KeyTables, Keymap, PendingKeys};
use crate::render::{self, RowDecorations};
use crate::session::SessionInfo;
use crate::sidebar::SidebarProcess;
use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const EVENT_POLL: Duration = Duration::from_millis(100);

/// Something a key can do in the picker. The names are the keys of the
/// `[picker.keys]` table, and match the sidebar's where the action is the
/// same, so one `[keys]` entry rebinds both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PickerAction {
    MoveUp,
    MoveDown,
    Open,
    CycleFilter,
    DeleteChar,
    Close,
}

impl KeyAction for PickerAction {
    const ALL: &'static [Self] = &[
        Self::MoveUp,
        Self::MoveDown,
        Self::Open,
        Self::CycleFilter,
        Self::DeleteChar,
        Self::Close,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::MoveUp => "move_up",
            Self::MoveDown => "move_down",
            Self::Open => "open",
            Self::CycleFilter => "cycle_filter",
            Self::DeleteChar => "delete_char",
            Self::Close => "close",
        }
    }
}

/// The picker's default keys. Every printable character is filter input, so
/// navigation uses arrows and fzf-style Ctrl keys.
const PICKER_DEFAULT_KEYS: &DefaultKeys<PickerAction> = &[
    (PickerAction::MoveUp, &["up", "C-p"], &[]),
    (PickerAction::MoveDown, &["down", "C-n"], &[]),
    (PickerAction::Open, &["enter"], &[]),
    (PickerAction::CycleFilter, &["C-a"], &[]),
    (PickerAction::DeleteChar, &["backspace"], &[]),
    (PickerAction::Close, &["esc", "C-c"], &[]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct PickerKeymap(Keymap<PickerAction>);

impl Default for PickerKeymap {
    fn default() -> Self {
        Self::from_tables(&KeyTables::default()).0
    }
}

impl PickerKeymap {
    /// The keymap from `[keys]` and `[picker.keys]` in config.toml, and a
    /// message for each entry that could not be used.
    fn load() -> (Self, Vec<String>) {
        let (tables, mut warnings) = KeyTables::load("picker");
        let (keymap, more) = Self::from_tables(&tables);
        warnings.extend(more);
        (keymap, warnings)
    }

    #[cfg(test)]
    fn from_config_text(text: &str) -> (Self, Vec<String>) {
        let (tables, mut warnings) = KeyTables::from_config_text(text, "picker");
        let (keymap, more) = Self::from_tables(&tables);
        warnings.extend(more);
        (keymap, warnings)
    }

    fn from_tables(tables: &KeyTables) -> (Self, Vec<String>) {
        let (mut keymap, mut warnings) =
            Keymap::build("picker", tables, PICKER_DEFAULT_KEYS, |keys| {
                match keys[0].typed_char() {
                    Some(_) => Err(format!(
                        "`{}` would type into the filter; use a named key or one with Ctrl or Alt",
                        keys[0].label()
                    )),
                    None => Ok(()),
                }
            });
        // The picker takes over the terminal, so it always needs a way out.
        if !keymap.has_binding(PickerAction::Close) {
            warnings.push("picker.keys: close needs a key; keeping Esc".to_string());
            keymap.force_bind(vec![KeyChord::plain(KeyCode::Esc)], PickerAction::Close);
        }
        (Self(keymap), warnings)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PickerItem {
    Process(usize),
    History(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatchDetail {
    source: String,
    snippet: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PickerSelection {
    Attach(String),
    Resume(String),
}

/// Which rows the picker lists; Ctrl-a cycles through them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ActivityFilter {
    /// Live processes, plus past sessions that match the query.
    #[default]
    All,
    /// Only live pmux processes.
    Live,
    /// Only past sessions pmux can resume.
    Past,
}

impl ActivityFilter {
    const ALL: [Self; 3] = [Self::All, Self::Live, Self::Past];

    fn next(self) -> Self {
        match self {
            Self::All => Self::Live,
            Self::Live => Self::Past,
            Self::Past => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Live => "live",
            Self::Past => "past",
        }
    }

    fn keeps(self, item: &PickerItem) -> bool {
        matches!(
            (self, item),
            (Self::All, _)
                | (Self::Live, PickerItem::Process(_))
                | (Self::Past, PickerItem::History(_))
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum DredgeState {
    #[default]
    Indexing,
    Ready,
    Absent,
    Failed(String),
}

struct PickerApp {
    /// Full process list in sidebar order (no outer-tmux location).
    processes: Vec<SidebarProcess>,
    query: String,
    activity: ActivityFilter,
    /// Live processes and Dredge history matches in display-ranked order.
    filtered: Vec<PickerItem>,
    /// Index into `filtered`.
    selected: usize,
    /// Index into `filtered`.
    first_visible: usize,
    matcher: Matcher,
    /// Agent titles and prompt counts, filled in as background loads land.
    decorations: RowDecorations,
    /// Agent processes whose decorations have been requested.
    decorations_requested: HashSet<String>,
    /// pmux's authoritative catalog supplies liveness and safe resume metadata
    /// for Dredge's search-only results.
    history: Vec<crate::HistoryEntry>,
    dredge_state: DredgeState,
    /// The first `[rows]` config problem, shown beside the count.
    config_problem: Option<String>,
    keymap: PickerKeymap,
    pending_keys: PendingKeys,
    dredge_query: Option<String>,
    dredge_hits: Vec<SearchHit>,
    /// Why each currently filtered row matched the active query, keyed by the
    /// same stable identity used to preserve the selection across updates.
    match_details: HashMap<String, MatchDetail>,
    /// Once the user navigates a query's results, background updates preserve
    /// that chosen row. Until then, the cursor follows the best-ranked result.
    selection_navigated: bool,
}

/// One result from a background decoration load.
enum DecorationUpdate {
    #[cfg(test)]
    AgentTitles(HashMap<String, String>),
    HistoryCatalog(Vec<crate::HistoryEntry>),
    PromptLabel {
        name: String,
        label: String,
    },
}

enum PickAction {
    Select(PickerSelection),
    Cancel,
    Continue,
}

impl PickerApp {
    fn new(processes: Vec<SidebarProcess>) -> Self {
        let mut app = Self {
            processes,
            query: String::new(),
            activity: ActivityFilter::default(),
            filtered: Vec::new(),
            selected: 0,
            first_visible: 0,
            matcher: Matcher::new(Config::DEFAULT),
            decorations: RowDecorations::default(),
            decorations_requested: HashSet::new(),
            history: Vec::new(),
            dredge_state: DredgeState::Indexing,
            config_problem: None,
            keymap: PickerKeymap::default(),
            pending_keys: PendingKeys::default(),
            dredge_query: None,
            dredge_hits: Vec::new(),
            match_details: HashMap::new(),
            selection_navigated: false,
        };
        app.recompute_filter();
        app
    }

    #[cfg(test)]
    fn selected_name(&self) -> Option<&str> {
        self.filtered
            .get(self.selected)
            .and_then(|item| match item {
                PickerItem::Process(index) => Some(self.processes[*index].process.name.as_str()),
                PickerItem::History(_) => None,
            })
    }

    fn selected_identity(&self) -> Option<String> {
        self.filtered
            .get(self.selected)
            .map(|item| self.item_identity(item))
    }

    fn selected_selection(&self) -> Option<PickerSelection> {
        match self.filtered.get(self.selected)? {
            PickerItem::Process(index) => Some(PickerSelection::Attach(
                self.processes[*index].process.name.clone(),
            )),
            PickerItem::History(index) => {
                let entry = self.history.get(*index)?;
                Some(PickerSelection::Resume(format!(
                    "{}:{}",
                    entry.agent, entry.session_id
                )))
            }
        }
    }

    fn item_identity(&self, item: &PickerItem) -> String {
        match item {
            PickerItem::Process(index) => {
                format!("process:{}", self.processes[*index].process.name)
            }
            PickerItem::History(index) => {
                let entry = &self.history[*index];
                format!("history:{}:{}", entry.agent, entry.session_id)
            }
        }
    }

    /// Adopt a fresh snapshot while keeping the cursor on the same process by
    /// name where possible (mirrors the sidebar's snapshot handling).
    fn update_processes(&mut self, processes: Vec<SidebarProcess>) {
        let previously_selected = self.selected_identity();
        self.processes = processes;
        self.refilter_after_background_update(previously_selected);
    }

    /// Fold in a finished decoration load. Titles are matched by the filter,
    /// so they re-run it.
    fn apply_decoration(&mut self, update: DecorationUpdate) {
        match update {
            #[cfg(test)]
            DecorationUpdate::AgentTitles(titles) => {
                let previously_selected = self.selected_identity();
                self.decorations.agent_titles.extend(titles);
                self.refilter_after_background_update(previously_selected);
            }
            DecorationUpdate::HistoryCatalog(entries) => {
                let previously_selected = self.selected_identity();
                self.decorations
                    .agent_titles
                    .extend(render::agent_titles(&entries));
                self.history = entries;
                self.refilter_after_background_update(previously_selected);
            }
            DecorationUpdate::PromptLabel { name, label } => {
                self.decorations.prompt_labels.insert(name, label);
            }
        }
    }

    fn apply_dredge_update(&mut self, update: SearchUpdate) {
        match update {
            SearchUpdate::Absent => {
                let previously_selected = self.selected_identity();
                self.dredge_state = DredgeState::Absent;
                self.dredge_query = None;
                self.dredge_hits.clear();
                self.refilter_after_background_update(previously_selected);
            }
            SearchUpdate::Ready => self.dredge_state = DredgeState::Ready,
            SearchUpdate::Failed(message) => {
                let previously_selected = self.selected_identity();
                self.dredge_state = DredgeState::Failed(message);
                self.dredge_query = None;
                self.dredge_hits.clear();
                self.refilter_after_background_update(previously_selected);
            }
            SearchUpdate::Results { query, hits } if query == self.query => {
                let previously_selected = self.selected_identity();
                self.dredge_state = DredgeState::Ready;
                self.dredge_query = Some(query);
                self.dredge_hits = hits;
                self.refilter_after_background_update(previously_selected);
            }
            SearchUpdate::Results { .. } => {}
        }
    }

    /// Agent processes not yet sent to a decoration load, now marked sent.
    fn take_undecorated_agents(&mut self) -> Vec<String> {
        let names: Vec<String> = self
            .processes
            .iter()
            .filter(|process| process.process.agent.is_some())
            .map(|process| process.process.name.clone())
            .filter(|name| !self.decorations_requested.contains(name))
            .collect();
        self.decorations_requested.extend(names.iter().cloned());
        names
    }

    fn refilter_keeping_selection(&mut self, previously_selected: Option<String>) {
        self.recompute_filter();
        if let Some(identity) = previously_selected
            && let Some(position) = self
                .filtered
                .iter()
                .position(|item| self.item_identity(item) == identity)
        {
            self.selected = position;
        }
        self.clamp_selection();
    }

    fn refilter_after_background_update(&mut self, previously_selected: Option<String>) {
        if self.query.is_empty() || self.selection_navigated {
            self.refilter_keeping_selection(previously_selected);
        } else {
            self.recompute_filter();
            self.select_first();
        }
    }

    fn query_changed(&mut self) {
        self.selection_navigated = false;
        self.recompute_filter();
        self.select_first();
    }

    fn cycle_activity(&mut self) {
        let previously_selected = self.selected_identity();
        self.activity = self.activity.next();
        self.refilter_keeping_selection(previously_selected);
    }

    fn select_first(&mut self) {
        self.selected = 0;
        self.first_visible = 0;
    }

    fn recompute_filter(&mut self) {
        self.match_details.clear();
        if self.query.is_empty() && self.activity == ActivityFilter::Past {
            // With no query there is nothing to search, so list every
            // resumable past session, newest first (the catalog's order).
            self.filtered = self
                .history
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.can_resume())
                .map(|(index, _)| PickerItem::History(index))
                .collect();
        } else if self.query.is_empty() {
            self.filtered = (0..self.processes.len()).map(PickerItem::Process).collect();
        } else {
            let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
            let matcher = &mut self.matcher;
            let mut buf = Vec::new();
            let local: Vec<(u32, usize)> = self
                .processes
                .iter()
                .enumerate()
                .filter_map(|(index, process)| {
                    let title = self.decorations.agent_titles.get(&process.process.name);
                    let haystack = row_haystack(process, title.map(String::as_str));
                    let score = pattern.score(Utf32Str::new(&haystack, &mut buf), matcher)?;
                    Some((score, index))
                })
                .collect();
            let local_max = local.iter().map(|(score, _)| *score).max().unwrap_or(1) as f32;
            let mut scored: HashMap<String, (f32, usize, PickerItem)> = HashMap::new();
            for (score, index) in local {
                let item = PickerItem::Process(index);
                let identity = self.item_identity(&item);
                self.match_details.insert(
                    identity.clone(),
                    local_match_detail(
                        &self.processes[index],
                        self.decorations
                            .agent_titles
                            .get(&self.processes[index].process.name)
                            .map(String::as_str),
                        &self.query,
                    ),
                );
                scored.insert(
                    identity,
                    // Dredge's full-transcript relevance should normally beat
                    // a title/name-only fuzzy match, while local rows remain a
                    // useful fallback for shells and unavailable history.
                    (score as f32 / local_max * 0.9, index, item),
                );
            }

            if self.dredge_query.as_deref() == Some(self.query.as_str()) {
                let history_by_key: HashMap<(&str, &str), usize> = self
                    .history
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        ((entry.agent.as_str(), entry.session_id.as_str()), index)
                    })
                    .collect();
                let process_by_name: HashMap<&str, usize> = self
                    .processes
                    .iter()
                    .enumerate()
                    .map(|(index, process)| (process.process.name.as_str(), index))
                    .collect();
                for (order, hit) in self.dredge_hits.iter().enumerate() {
                    let Some(&history_index) =
                        history_by_key.get(&(hit.provider.as_str(), hit.session_id.as_str()))
                    else {
                        continue;
                    };
                    let entry = &self.history[history_index];
                    let item = match entry.pmux_process.as_deref() {
                        Some(name) => match process_by_name.get(name) {
                            Some(index) => PickerItem::Process(*index),
                            None => continue,
                        },
                        None if entry.live => continue,
                        None => PickerItem::History(history_index),
                    };
                    let identity = self.item_identity(&item);
                    let candidate = (hit.score, self.processes.len() + order, item);
                    match scored.get(&identity) {
                        // A previous Dredge duplicate keeps its stronger score;
                        // a local fuzzy score is replaced so transcript search
                        // controls the rank of an agent session it knows about.
                        Some((score, source_order, _))
                            if *source_order >= self.processes.len() && *score >= hit.score => {}
                        _ => {
                            if let (Some(source), Some(snippet)) =
                                (hit.match_source.as_deref(), hit.snippet.as_deref())
                                && !snippet.trim().is_empty()
                            {
                                self.match_details.insert(
                                    identity.clone(),
                                    MatchDetail {
                                        source: display_match_source(source).to_string(),
                                        snippet: snippet.to_string(),
                                    },
                                );
                            }
                            scored.insert(identity, candidate);
                        }
                    }
                }
            }
            let mut ranked: Vec<_> = scored.into_values().collect();
            ranked.sort_by(|left, right| {
                right
                    .0
                    .partial_cmp(&left.0)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| left.1.cmp(&right.1))
            });
            self.filtered = ranked
                .into_iter()
                .map(|(_, _, item)| item)
                .filter(|item| self.activity.keeps(item))
                .collect();
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.first_visible = 0;
            return;
        }
        self.selected = self.selected.min(self.filtered.len() - 1);
        self.first_visible = self.first_visible.min(self.filtered.len() - 1);
    }

    fn move_cursor(&mut self, delta: isize, list_height: u16) {
        self.selection_navigated = true;
        if self.filtered.is_empty() {
            return;
        }
        let maximum = self.filtered.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, maximum) as usize;
        self.ensure_visible(list_height);
    }

    fn ensure_visible(&mut self, list_height: u16) {
        if self.filtered.is_empty() {
            self.first_visible = 0;
            return;
        }
        if self.selected < self.first_visible {
            self.first_visible = self.selected;
        }
        while self.selected >= self.viewport_end(list_height) {
            self.first_visible = (self.first_visible + 1).min(self.selected);
        }
    }

    fn viewport_end(&self, list_height: u16) -> usize {
        render::viewport_end(
            self.filtered.len(),
            self.first_visible,
            list_height,
            |index| self.item_line_count(&self.filtered[index]),
        )
    }

    fn item_line_count(&self, item: &PickerItem) -> usize {
        let base = match item {
            PickerItem::Process(index) => {
                render::process_line_count(&self.processes[*index], false)
            }
            PickerItem::History(_) => render::RESUMABLE_ROW_LINES,
        };
        base + usize::from(self.item_match_detail(item).is_some())
    }

    fn item_match_detail(&self, item: &PickerItem) -> Option<&MatchDetail> {
        self.match_details.get(&self.item_identity(item))
    }
}

/// The text a query is matched against: session name, the agent's title for
/// the conversation (shown in the pill of an auto-named row), current command,
/// and CWD.
fn row_haystack(process: &SidebarProcess, agent_title: Option<&str>) -> String {
    let mut parts = vec![process.process.name.clone()];
    if let Some(title) = agent_title {
        parts.push(title.to_string());
    }
    if !process.process.current_command.trim().is_empty() {
        parts.push(process.process.current_command.clone());
    }
    if let Some(cwd) = crate::sidebar_process_cwd(process) {
        parts.push(crate::display_path(cwd));
    }
    parts.join(" ")
}

fn local_match_detail(
    process: &SidebarProcess,
    agent_title: Option<&str>,
    query: &str,
) -> MatchDetail {
    let name = process.process.name.clone();
    let command = process.process.current_command.trim().to_string();
    let cwd = crate::sidebar_process_cwd(process).map(crate::display_path);
    let mut fields = vec![("process name", name)];
    if let Some(title) = agent_title {
        fields.push(("title", title.to_string()));
    }
    if !command.is_empty() {
        fields.push(("command", command));
    }
    if let Some(cwd) = cwd {
        fields.push(("directory", cwd));
    }

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut best: Option<(u32, &str, &str)> = None;
    for (source, value) in &fields {
        let Some(score) = pattern.score(Utf32Str::new(value, &mut buf), &mut matcher) else {
            continue;
        };
        if best.is_none_or(|(best_score, _, _)| score > best_score) {
            best = Some((score, source, value));
        }
    }
    let (_, source, snippet) = best.unwrap_or_else(|| {
        let (_, value) = &fields[0];
        (0, "process metadata", value.as_str())
    });
    MatchDetail {
        source: source.to_string(),
        snippet: snippet.to_string(),
    }
}

fn display_match_source(source: &str) -> &str {
    match source {
        "conversation" => "transcript",
        "first_prompt" => "first prompt",
        other => other,
    }
}

fn render_match_detail(detail: &MatchDetail, width: u16) -> Line<'static> {
    let prefix = format!("  matched {} · ", detail.source);
    let snippet = crate::truncate_col(
        &detail.snippet,
        usize::from(width).saturating_sub(render::text_width(&prefix)),
    );
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::DarkGray)),
        Span::styled(snippet, Style::default().fg(Color::Gray)),
    ])
}

/// Load agent titles (from the history catalog) and the prompt counts of the
/// agent processes `names` on a worker thread, sending each as it lands. A
/// failed or crashed load leaves those rows undecorated.
fn spawn_decoration_load(names: Vec<String>, tx: Sender<DecorationUpdate>) {
    render::spawn_worker(
        "pmux-picker-decorations",
        move || {
            let query = crate::HistoryQuery {
                include_live: true,
                ..crate::HistoryQuery::default()
            };
            if let Ok(entries) = crate::history(&query)
                && tx.send(DecorationUpdate::HistoryCatalog(entries)).is_err()
            {
                return;
            }
            for name in names {
                if let crate::DrawerState::Ready(data) = crate::load_activity(&name) {
                    let label = render::prompt_label(&data);
                    if tx
                        .send(DecorationUpdate::PromptLabel { name, label })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        },
        || {},
    );
}

/// Run the picker and return the chosen live or historical session, or an error
/// whose message is `selection cancelled` when the user backs out (the
/// `attach` command maps that to a no-op).
pub(crate) fn pick_session_tui(sessions: &[SessionInfo]) -> Result<PickerSelection> {
    let (row_layout, row_problems) = render::load_row_layout(render::RowSurface::Picker);
    render::set_row_layout(row_layout);
    let snapshot = crate::api::build_state_snapshot(sessions);
    let mut app = PickerApp::new(crate::picker_processes(&snapshot));
    let (keymap, mut config_problems) = PickerKeymap::load();
    app.keymap = keymap;
    config_problems.extend(row_problems);
    app.config_problem = config_problems
        .first()
        .map(|first| match config_problems.len() {
            1 => first.clone(),
            count => format!("{first} (+{} more)", count - 1),
        });
    // Live updates from the same event stream the sidebar consumes. A failure to
    // start it is non-fatal: the picker still works from the seeded snapshot.
    let (mut child, receiver) = match render::spawn_event_stream() {
        Ok((child, receiver)) => (Some(child), Some(receiver)),
        Err(_) => (None, None),
    };
    let (decoration_tx, decoration_rx) = mpsc::channel();
    // Load the authoritative history catalog even when there are no live agent
    // rows. Dredge only searches; pmux owns liveness and resume safety.
    spawn_decoration_load(app.take_undecorated_agents(), decoration_tx.clone());
    let (dredge_query_tx, dredge_query_rx) = mpsc::channel();
    let (dredge_update_tx, dredge_update_rx) = mpsc::channel();
    crate::dredge::spawn_search_worker(dredge_query_rx, dredge_update_tx);
    let result = render::with_terminal_opts(render::MouseMode::Scroll, |terminal| {
        run_picker(
            terminal,
            &mut app,
            receiver.as_ref(),
            &decoration_tx,
            &decoration_rx,
            &dredge_query_tx,
            &dredge_update_rx,
        )
    });
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn run_picker(
    terminal: &mut render::AppTerminal,
    app: &mut PickerApp,
    snapshots: Option<&Receiver<crate::StateSnapshot>>,
    decoration_tx: &Sender<DecorationUpdate>,
    decoration_rx: &Receiver<DecorationUpdate>,
    dredge_query_tx: &Sender<String>,
    dredge_update_rx: &Receiver<SearchUpdate>,
) -> Result<PickerSelection> {
    loop {
        if let Some(receiver) = snapshots {
            let mut latest = None;
            while let Ok(snapshot) = receiver.try_recv() {
                latest = Some(snapshot);
            }
            if let Some(snapshot) = latest {
                app.update_processes(crate::picker_processes(&snapshot));
            }
        }
        let undecorated = app.take_undecorated_agents();
        if !undecorated.is_empty() {
            spawn_decoration_load(undecorated, decoration_tx.clone());
        }
        while let Ok(update) = decoration_rx.try_recv() {
            app.apply_decoration(update);
        }
        while let Ok(update) = dredge_update_rx.try_recv() {
            app.apply_dredge_update(update);
        }
        let list_height = picker_list_height(terminal.size()?.height);
        // Background snapshots, history, and search results can insert rows
        // above the current selection. Keep that stable selection on-screen.
        app.ensure_visible(list_height);
        terminal.draw(|frame| draw_picker(frame, app))?;
        if !event::poll(EVENT_POLL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let old_query = app.query.clone();
                match handle_picker_key(app, key, list_height) {
                    PickAction::Select(selection) => return Ok(selection),
                    PickAction::Cancel => bail!("selection cancelled"),
                    PickAction::Continue => {}
                }
                if app.query != old_query {
                    app.dredge_query = None;
                    app.dredge_hits.clear();
                    if matches!(app.dredge_state, DredgeState::Ready | DredgeState::Indexing) {
                        let _ = dredge_query_tx.send(app.query.clone());
                    }
                }
            }
            // The scroll wheel moves the selection, like the arrow keys, so
            // Enter still attaches to whatever the wheel landed on.
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => app.move_cursor(-1, list_height),
                MouseEventKind::ScrollDown => app.move_cursor(1, list_height),
                _ => {}
            },
            _ => {}
        }
    }
}

/// Rows above the list: the prompt/query row and the filter hint row.
const PICKER_HEADER_ROWS: u16 = 2;

/// Rows available to the list below the prompt and filter hint rows.
fn picker_list_height(total_height: u16) -> u16 {
    total_height.saturating_sub(PICKER_HEADER_ROWS)
}

fn handle_picker_key(app: &mut PickerApp, key: KeyEvent, list_height: u16) -> PickAction {
    let chord = KeyChord::from_event(key);
    let keymap = &app.keymap.0;
    let action = match app
        .pending_keys
        .resolve(chord, |pressed| keymap.lookup(pressed))
    {
        KeyLookup::Action(action) => action,
        KeyLookup::Pending => return PickAction::Continue,
        // Every printable character (including j/k) is filter input.
        KeyLookup::Unbound => {
            if let Some(character) = chord.typed_char() {
                app.query.push(character);
                app.query_changed();
            }
            return PickAction::Continue;
        }
    };
    match action {
        PickerAction::Close => PickAction::Cancel,
        PickerAction::Open => match app.selected_selection() {
            Some(selection) => PickAction::Select(selection),
            None => PickAction::Continue,
        },
        PickerAction::MoveUp => {
            app.move_cursor(-1, list_height);
            PickAction::Continue
        }
        PickerAction::MoveDown => {
            app.move_cursor(1, list_height);
            PickAction::Continue
        }
        PickerAction::CycleFilter => {
            app.cycle_activity();
            PickAction::Continue
        }
        PickerAction::DeleteChar => {
            app.query.pop();
            app.query_changed();
            PickAction::Continue
        }
    }
}

fn draw_picker(frame: &mut ratatui::Frame, app: &mut PickerApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(area);
    draw_prompt(frame, chunks[0], app);
    draw_filter_hint(frame, chunks[1], app);
    draw_picker_list(frame, chunks[2], app);
}

/// The `cycle_filter` hint under the prompt (Ctrl-a by default), with the
/// current filter highlighted.
fn draw_filter_hint(frame: &mut ratatui::Frame, area: Rect, app: &PickerApp) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    if let Some(key) = app.keymap.0.labels(PickerAction::CycleFilter).first() {
        let key = key
            .replace("^M-", "ctrl-alt-")
            .replace('^', "ctrl-")
            .replace("M-", "alt-");
        spans.push(Span::styled(key, Style::default().fg(Color::Cyan)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("show: ", dim));
    for (index, filter) in ActivityFilter::ALL.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        if filter == app.activity {
            spans.push(Span::styled(
                format!("[{}]", filter.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(filter.label(), dim));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_prompt(frame: &mut ratatui::Frame, area: Rect, app: &PickerApp) {
    let prefix = "Attach or resume: ";
    let backend = match &app.dredge_state {
        DredgeState::Indexing => " · indexing history".to_string(),
        DredgeState::Ready => " · history".to_string(),
        DredgeState::Absent => String::new(),
        DredgeState::Failed(message) => format!(" · {}", crate::truncate_col(message, 30)),
    };
    let backend = match &app.config_problem {
        Some(problem) => format!("{backend} · {}", crate::truncate_col(problem, 40)),
        None => backend,
    };
    let count = if app.query.is_empty() && app.activity == ActivityFilter::Past {
        format!("{} past{backend}", app.filtered.len())
    } else if app.query.is_empty() {
        format!("{} live{backend}", app.filtered.len())
    } else {
        format!("{} matches{backend}", app.filtered.len())
    };
    let width = usize::from(area.width);
    let left_width = render::text_width(prefix) + render::text_width(&app.query) + 1;
    let count_width = render::text_width(&count);
    let mut spans = vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.query.clone()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ];
    if width > left_width + count_width + 1 {
        spans.push(Span::raw(" ".repeat(width - left_width - count_width - 1)));
        spans.push(Span::styled(count, Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_picker_list(frame: &mut ratatui::Frame, area: Rect, app: &PickerApp) {
    if app.filtered.is_empty() {
        let message = match (app.query.is_empty(), app.activity) {
            (true, ActivityFilter::Past) => "No resumable past sessions.",
            (true, _) => "No running pmux processes.",
            (false, ActivityFilter::All) => "No matching processes or resumable sessions.",
            (false, ActivityFilter::Live) => "No matching live processes.",
            (false, ActivityFilter::Past) => "No matching resumable sessions.",
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let end = render::viewport_end(
        app.filtered.len(),
        app.first_visible,
        area.height,
        |index| app.item_line_count(&app.filtered[index]),
    );
    let top_more = app.first_visible;
    let bottom_more = app.filtered.len().saturating_sub(end);
    let mut y = area.y;
    if top_more > 0 && y < area.bottom() {
        render::render_centered_line(
            frame,
            area,
            y,
            Line::from(Span::styled(
                format!("↑ ({top_more} more)"),
                Style::default().fg(Color::DarkGray),
            )),
        );
    }
    y = y.saturating_add(1);
    let content_bottom = if bottom_more > 0 {
        area.bottom().saturating_sub(1)
    } else {
        area.bottom()
    };
    let pill_colors = render::assign_pill_colors(&app.processes);
    let tag_width = render::agent_tag_column_width(app.processes.iter());

    for (offset, item) in app.filtered[app.first_visible..end].iter().enumerate() {
        if y >= content_bottom {
            break;
        }
        let index = app.first_visible + offset;
        let highlight = (index == app.selected).then_some(render::CURSOR_HIGHLIGHT_COLOR);
        let mut lines = match item {
            PickerItem::Process(process_index) => {
                let process = &app.processes[*process_index];
                let name = &process.process.name;
                let params = render::RowRenderParams {
                    width: area.width,
                    highlight_color: highlight,
                    drawer_outline: false,
                    pill_text_color: render::ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
                    active_pane_border_color: render::ACTIVE_PANE_BORDER_FALLBACK_COLOR,
                    agent_tag_column_width: tag_width,
                    context: None,
                    pill_colors: Some(&pill_colors),
                    location: None,
                    focused_process: false,
                    flash_lit: false,
                    attachment_flash_lit: false,
                    prompt_label: app.decorations.prompt_labels.get(name).map(String::as_str),
                    agent_title: app.decorations.agent_titles.get(name).map(String::as_str),
                    parent_note: None,
                };
                render::render_process_block(process, &params, Vec::new())
            }
            PickerItem::History(history_index) => app
                .history
                .get(*history_index)
                .map(|entry| {
                    render::render_resumable_row(
                        entry,
                        None,
                        area.width,
                        highlight,
                        render::ACTIVE_PANE_BACKGROUND_FALLBACK_COLOR,
                    )
                })
                .unwrap_or_default(),
        };
        if let Some(detail) = app.item_match_detail(item) {
            lines.push(render_match_detail(detail, area.width));
        }
        for line in lines {
            if y >= content_bottom {
                break;
            }
            render::render_line(frame, area, y, line, highlight);
            y = y.saturating_add(1);
        }
        if index + 1 < end {
            y = y.saturating_add(1);
        }
    }
    if bottom_more > 0 && area.height > 0 {
        render::render_centered_line(
            frame,
            area,
            area.bottom() - 1,
            Line::from(Span::styled(
                format!("↓ ({bottom_more} more)"),
                Style::default().fg(Color::DarkGray),
            )),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_state_snapshot;
    use crate::session::SessionInfo;
    use crate::util::now_epoch;
    use crossterm::event::KeyModifiers;

    fn press(app: &mut PickerApp, text: &str) -> Vec<bool> {
        crate::keys::parse_binding(text)
            .unwrap()
            .into_iter()
            .map(|chord| {
                let event = chord_event(chord);
                matches!(handle_picker_key(app, event, 10), PickAction::Cancel)
            })
            .collect()
    }

    /// The key event that produces `chord`.
    fn chord_event(chord: KeyChord) -> KeyEvent {
        let label = chord.label();
        let (ctrl, rest) = match label.strip_prefix('^') {
            Some(rest) => (true, rest),
            None => (false, label.as_str()),
        };
        let code = match rest {
            "Esc" => KeyCode::Esc,
            "Enter" => KeyCode::Enter,
            "Tab" => KeyCode::Tab,
            "BS" => KeyCode::Backspace,
            "↑" => KeyCode::Up,
            "↓" => KeyCode::Down,
            other => KeyCode::Char(other.chars().next().unwrap()),
        };
        let modifiers = if ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(code, modifiers)
    }

    fn lookup(keymap: &PickerKeymap, text: &str) -> KeyLookup<PickerAction> {
        keymap.0.lookup(&crate::keys::parse_binding(text).unwrap())
    }

    #[test]
    fn picker_defaults_keep_every_printable_character_for_the_filter() {
        let keymap = PickerKeymap::default();
        assert_eq!(
            lookup(&keymap, "C-n"),
            KeyLookup::Action(PickerAction::MoveDown)
        );
        assert_eq!(
            lookup(&keymap, "up"),
            KeyLookup::Action(PickerAction::MoveUp)
        );
        assert_eq!(
            lookup(&keymap, "enter"),
            KeyLookup::Action(PickerAction::Open)
        );
        assert_eq!(
            lookup(&keymap, "C-c"),
            KeyLookup::Action(PickerAction::Close)
        );
        assert_eq!(lookup(&keymap, "j"), KeyLookup::Unbound);
    }

    #[test]
    fn picker_keys_rebind_and_refuse_keys_that_type() {
        let (keymap, warnings) = PickerKeymap::from_config_text(
            "[picker.keys]\nmove_down = [\"C-j\", \"j\"]\nopen = \"tab\"\n",
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("would type into the filter"));
        assert_eq!(
            lookup(&keymap, "C-j"),
            KeyLookup::Action(PickerAction::MoveDown)
        );
        assert_eq!(lookup(&keymap, "C-n"), KeyLookup::Unbound);
        assert_eq!(lookup(&keymap, "j"), KeyLookup::Unbound);
        assert_eq!(
            lookup(&keymap, "tab"),
            KeyLookup::Action(PickerAction::Open)
        );
        assert_eq!(lookup(&keymap, "enter"), KeyLookup::Unbound);
    }

    #[test]
    fn shared_keys_apply_where_they_can_and_the_picker_table_wins() {
        let (keymap, warnings) = PickerKeymap::from_config_text(
            r#"
            [keys]
            move_down = ["j", "C-j"]   # j is the sidebar's; the picker takes C-j
            move_up = "k"              # nothing usable: the picker keeps its defaults
            fold_section = "za"        # a sidebar action
            open = "C-o"
            [picker.keys]
            open = "C-y"
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "C-j"),
            KeyLookup::Action(PickerAction::MoveDown)
        );
        assert_eq!(lookup(&keymap, "down"), KeyLookup::Unbound);
        assert_eq!(
            lookup(&keymap, "up"),
            KeyLookup::Action(PickerAction::MoveUp)
        );
        assert_eq!(
            lookup(&keymap, "C-y"),
            KeyLookup::Action(PickerAction::Open)
        );
        assert_eq!(lookup(&keymap, "C-o"), KeyLookup::Unbound);

        let (_, warnings) = PickerKeymap::from_config_text("[keys]\njump = \"C-j\"\n");
        assert_eq!(warnings, ["keys: unknown action `jump`"]);
    }

    #[test]
    fn the_picker_always_keeps_a_way_out() {
        let (keymap, warnings) = PickerKeymap::from_config_text("[picker.keys]\nclose = []\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "esc"),
            KeyLookup::Action(PickerAction::Close)
        );
        assert_eq!(lookup(&keymap, "C-c"), KeyLookup::Unbound);
    }

    #[test]
    fn rebound_keys_drive_the_picker_and_unbound_characters_filter() {
        let mut app = app_with(&[session("api", "bash", "/tmp")]);
        app.keymap = PickerKeymap::from_config_text(
            "[picker.keys]\nclose = \"C-x C-c\"\ndelete_char = \"C-h\"\n",
        )
        .0;
        assert_eq!(press(&mut app, "esc"), [false], "esc is no longer bound");
        press(&mut app, "ab");
        assert_eq!(app.query, "ab");
        press(&mut app, "C-h");
        assert_eq!(app.query, "a");
        assert_eq!(press(&mut app, "C-x C-c"), [false, true]);
    }

    #[test]
    fn the_customization_docs_picker_key_examples_apply_cleanly() {
        let doc = include_str!("../docs/sidebar-customization.md");
        let blocks: Vec<&str> = doc
            .split("```toml\n")
            .skip(1)
            .filter_map(|part| part.split_once("```").map(|(block, _)| block))
            .filter(|block| block.contains("[picker.keys]") || block.contains("[keys]"))
            .collect();
        assert!(!blocks.is_empty());
        for block in blocks {
            let (_, warnings) = PickerKeymap::from_config_text(block);
            assert!(warnings.is_empty(), "{block}: {warnings:?}");
        }
    }

    fn session(name: &str, command: &str, cwd: &str) -> SessionInfo {
        let now = now_epoch();
        SessionInfo {
            name: name.to_string(),
            pane_id: "%1".to_string(),
            created_epoch: now,
            last_activity_epoch: now,
            current_command: command.to_string(),
            start_command: None,
            current_path: Some(cwd.to_string()),
            launch_command: None,
            launch_cwd: Some(cwd.to_string()),
            attached_clients: 0,
        }
    }

    fn app_with(sessions: &[SessionInfo]) -> PickerApp {
        let snapshot = build_state_snapshot(sessions);
        PickerApp::new(crate::picker_processes(&snapshot))
    }

    fn history_entry(
        agent: &str,
        id: &str,
        title: &str,
        live_process: Option<&str>,
    ) -> crate::HistoryEntry {
        crate::HistoryEntry {
            agent: agent.to_string(),
            session_id: id.to_string(),
            title: Some(title.to_string()),
            title_source: Some(crate::TitleSource::Prompt),
            first_prompt: Some(title.to_string()),
            cwd: Some("/tmp/project".to_string()),
            cwd_exists: true,
            git_branch: None,
            started_epoch: Some(now_epoch() - 120),
            last_activity_epoch: now_epoch() - 60,
            transcript: format!("/tmp/{id}.jsonl"),
            live: live_process.is_some(),
            pmux_process: live_process.map(str::to_string),
            resume_command: Some(vec![
                agent.to_string(),
                "resume".to_string(),
                id.to_string(),
            ]),
        }
    }

    #[test]
    fn picker_draws_wide_names_at_every_tiny_size_without_panicking() {
        let names = [
            "日本語のプロセス名前がとても長い",
            "🦀🦀🦀-rust",
            "👩\u{200d}👩\u{200d}👧\u{200d}👦-family",
            "e\u{301}\u{301}-combining",
            "x",
        ];
        let sessions: Vec<SessionInfo> = names
            .iter()
            .map(|name| session(name, "claude", &format!("/srv/項目/{name}")))
            .collect();
        let mut app = app_with(&sessions);
        for width in 0..24u16 {
            for height in 0..12u16 {
                // Alternate between the full list, a wide query that matches
                // some rows, and one that matches none.
                app.query =
                    ["", "項目🦀", "👩\u{200d}💻"][usize::from(width + height) % 3].to_string();
                app.recompute_filter();
                app.move_cursor(
                    isize::from(u8::try_from(height % 4).unwrap()) - 1,
                    picker_list_height(height),
                );
                let backend = ratatui::backend::TestBackend::new(width, height);
                let mut terminal = ratatui::Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| draw_picker(frame, &mut app))
                    .unwrap_or_else(|err| panic!("{width}x{height}: {err}"));
            }
        }
    }

    fn agent(process: &mut SidebarProcess) {
        process.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: None,
            updated_at: None,
            source: None,
            reason: None,
            session_id: None,
        });
    }

    fn drawn_picker(app: &mut PickerApp, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_picker(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn rows_show_the_agent_title_and_prompt_count_like_the_sidebar() {
        let mut app = app_with(&[session("1772262306-9rtuxxxx", "claude", "/srv/web")]);
        agent(&mut app.processes[0]);
        app.apply_decoration(DecorationUpdate::AgentTitles(HashMap::from([(
            "1772262306-9rtuxxxx".to_string(),
            "Fix the login flow".to_string(),
        )])));
        app.apply_decoration(DecorationUpdate::PromptLabel {
            name: "1772262306-9rtuxxxx".to_string(),
            label: "12p".to_string(),
        });
        let screen = drawn_picker(&mut app, 60, 8).join("\n");
        assert!(screen.contains("Fix the login flow"), "{screen}");
        assert!(screen.contains("/srv/web · 12p"), "{screen}");
    }

    #[test]
    fn query_matches_the_agent_title() {
        let mut app = app_with(&[
            session("1772262306-9rtuxxxx", "claude", "/srv/a"),
            session("1772262307-abcdxxxx", "claude", "/srv/b"),
        ]);
        app.apply_decoration(DecorationUpdate::AgentTitles(HashMap::from([(
            "1772262307-abcdxxxx".to_string(),
            "Refactor billing".to_string(),
        )])));
        app.query = "billing".to_string();
        app.recompute_filter();
        assert_eq!(app.selected_name(), Some("1772262307-abcdxxxx"));
    }

    #[test]
    fn titles_landing_keep_the_cursor_on_the_same_process() {
        let mut app = app_with(&[
            session("alpha", "zsh", "/tmp/a"),
            session("beta", "zsh", "/tmp/b"),
        ]);
        app.move_cursor(1, 40);
        let selected = app.selected_name().map(str::to_string);
        app.apply_decoration(DecorationUpdate::AgentTitles(HashMap::new()));
        assert_eq!(app.selected_name().map(str::to_string), selected);
    }

    #[test]
    fn each_agent_process_is_requested_for_decoration_once() {
        let mut app = app_with(&[
            session("agent", "claude", "/tmp/a"),
            session("shell", "zsh", "/tmp/b"),
        ]);
        let index = app
            .processes
            .iter()
            .position(|process| process.process.name == "agent")
            .unwrap();
        agent(&mut app.processes[index]);
        assert_eq!(app.take_undecorated_agents(), vec!["agent".to_string()]);
        assert!(app.take_undecorated_agents().is_empty());
    }

    #[test]
    fn more_below_indicator_is_pinned_to_the_last_line() {
        let sessions: Vec<SessionInfo> = (0..6)
            .map(|index| session(&format!("p{index}"), "zsh", "/tmp"))
            .collect();
        let mut app = app_with(&sessions);
        let screen = drawn_picker(&mut app, 40, 12);
        assert!(screen[11].contains("more)"), "{screen:#?}");
    }

    #[test]
    fn empty_query_keeps_all_in_sidebar_order() {
        let app = app_with(&[
            session("alpha", "zsh", "/tmp/a"),
            session("beta", "codex", "/tmp/b"),
        ]);
        assert_eq!(app.filtered.len(), 2);
        // Same order as picker_processes (sidebar order) with an empty query.
        assert_eq!(
            app.filtered,
            vec![PickerItem::Process(0), PickerItem::Process(1)]
        );
    }

    #[test]
    fn fuzzy_query_filters_and_ranks() {
        let mut app = app_with(&[
            session("web-server", "node", "/srv/web"),
            session("db", "psql", "/srv/db"),
        ]);
        app.query = "web".to_string();
        app.recompute_filter();
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected_name(), Some("web-server"));
    }

    #[test]
    fn query_matches_command_and_cwd() {
        let mut app = app_with(&[
            session("one", "vim", "/home/user/project"),
            session("two", "cargo", "/tmp"),
        ]);
        app.query = "cargo".to_string();
        app.recompute_filter();
        assert_eq!(app.selected_name(), Some("two"));

        app.query = "project".to_string();
        app.recompute_filter();
        assert_eq!(app.selected_name(), Some("one"));
    }

    #[test]
    fn dredge_results_mix_past_sessions_with_live_processes_without_duplicates() {
        let mut app = app_with(&[session("transcript-only-live", "claude", "/tmp/project")]);
        app.apply_decoration(DecorationUpdate::HistoryCatalog(vec![
            history_entry(
                "claude",
                "live-id",
                "Live conversation",
                Some("transcript-only-live"),
            ),
            history_entry("codex", "past-id", "Historical conversation", None),
        ]));
        app.query = "transcript-only".to_string();
        app.query_changed();
        assert_eq!(app.filtered, vec![PickerItem::Process(0)]);

        app.apply_dredge_update(SearchUpdate::Results {
            query: "transcript-only".to_string(),
            hits: vec![
                SearchHit {
                    provider: "codex".to_string(),
                    session_id: "past-id".to_string(),
                    score: 0.95,
                    match_source: Some("conversation".to_string()),
                    snippet: Some("…the transcript-only integration details…".to_string()),
                },
                SearchHit {
                    provider: "claude".to_string(),
                    session_id: "live-id".to_string(),
                    score: 0.8,
                    match_source: Some("first_prompt".to_string()),
                    snippet: Some("transcript-only live prompt".to_string()),
                },
                // Repeating the live identity must not create a second row.
                SearchHit {
                    provider: "claude".to_string(),
                    session_id: "live-id".to_string(),
                    score: 0.7,
                    match_source: Some("conversation".to_string()),
                    snippet: Some("weaker duplicate".to_string()),
                },
            ],
        });

        assert_eq!(
            app.filtered,
            vec![PickerItem::History(1), PickerItem::Process(0)]
        );
        // With no deliberate navigation, the cursor follows the new best hit.
        assert_eq!(
            app.selected_selection(),
            Some(PickerSelection::Resume("codex:past-id".to_string()))
        );
        app.move_cursor(1, 40);
        assert_eq!(
            app.selected_selection(),
            Some(PickerSelection::Attach("transcript-only-live".to_string()))
        );

        let screen = drawn_picker(&mut app, 70, 16).join("\n");
        assert!(
            screen.contains("↻ \u{e0b6}Historical conversation\u{e0b4}"),
            "{screen}"
        );
        assert!(
            screen.contains("matched transcript · …the transcript-only integration details…"),
            "{screen}"
        );
        assert!(
            screen.contains("matched first prompt · transcript-only live prompt"),
            "{screen}"
        );
    }

    #[test]
    fn query_edits_select_the_best_match_even_after_navigating_the_live_list() {
        let mut app = app_with(&[
            session("alpha", "zsh", "/tmp/a"),
            session("beta", "zsh", "/tmp/b"),
            session("gamma", "zsh", "/tmp/c"),
        ]);
        app.move_cursor(2, 40);
        assert_eq!(app.selected, 2);

        let action = handle_picker_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            40,
        );
        assert!(matches!(action, PickAction::Continue));
        assert_eq!(app.selected, 0);
        assert_eq!(app.first_visible, 0);
        assert!(!app.selection_navigated);
    }

    #[test]
    fn dredge_results_preserve_a_row_chosen_after_the_query_changed() {
        let mut app = app_with(&[
            session("match-alpha", "zsh", "/tmp/a"),
            session("match-beta", "zsh", "/tmp/b"),
        ]);
        app.apply_decoration(DecorationUpdate::HistoryCatalog(vec![history_entry(
            "codex", "past-id", "Past", None,
        )]));
        app.query = "match".to_string();
        app.query_changed();
        app.move_cursor(1, 40);
        let chosen = app.selected_selection();

        app.apply_dredge_update(SearchUpdate::Results {
            query: "match".to_string(),
            hits: vec![SearchHit {
                provider: "codex".to_string(),
                session_id: "past-id".to_string(),
                score: 1.0,
                match_source: None,
                snippet: None,
            }],
        });

        assert_eq!(app.filtered.first(), Some(&PickerItem::History(0)));
        assert_eq!(app.selected_selection(), chosen);
        assert!(app.selection_navigated);
    }

    #[test]
    fn stale_and_failed_dredge_queries_never_leave_history_rows_behind() {
        let mut app = app_with(&[session("live", "zsh", "/tmp")]);
        app.apply_decoration(DecorationUpdate::HistoryCatalog(vec![history_entry(
            "codex", "past-id", "Past", None,
        )]));
        app.query = "current".to_string();
        app.recompute_filter();
        app.apply_dredge_update(SearchUpdate::Results {
            query: "stale".to_string(),
            hits: vec![SearchHit {
                provider: "codex".to_string(),
                session_id: "past-id".to_string(),
                score: 1.0,
                match_source: None,
                snippet: None,
            }],
        });
        assert!(app.filtered.is_empty());

        app.apply_dredge_update(SearchUpdate::Results {
            query: "current".to_string(),
            hits: vec![SearchHit {
                provider: "codex".to_string(),
                session_id: "past-id".to_string(),
                score: 1.0,
                match_source: None,
                snippet: None,
            }],
        });
        assert_eq!(app.filtered, vec![PickerItem::History(0)]);

        app.apply_dredge_update(SearchUpdate::Failed("Dredge failed".to_string()));
        assert!(app.filtered.is_empty());
        assert!(matches!(app.dredge_state, DredgeState::Failed(_)));
    }

    fn ctrl_a(app: &mut PickerApp) {
        handle_picker_key(
            app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            40,
        );
    }

    fn app_with_live_and_past() -> PickerApp {
        let mut app = app_with(&[
            session("alpha", "claude", "/tmp/project"),
            session("beta", "zsh", "/tmp/other"),
        ]);
        app.apply_decoration(DecorationUpdate::HistoryCatalog(vec![
            history_entry("claude", "live-id", "Live conversation", Some("alpha")),
            history_entry("codex", "past-id", "Historical conversation", None),
            history_entry("claude", "older-id", "Older conversation", None),
        ]));
        app
    }

    #[test]
    fn ctrl_a_cycles_all_live_and_past_without_touching_the_query() {
        let mut app = app_with_live_and_past();
        assert_eq!(app.activity, ActivityFilter::All);
        assert_eq!(
            app.filtered,
            vec![PickerItem::Process(0), PickerItem::Process(1)]
        );

        ctrl_a(&mut app);
        assert_eq!(app.activity, ActivityFilter::Live);
        assert_eq!(
            app.filtered,
            vec![PickerItem::Process(0), PickerItem::Process(1)]
        );

        ctrl_a(&mut app);
        assert_eq!(app.activity, ActivityFilter::Past);
        // Every resumable past session, in catalog (newest-first) order; the
        // live one is a process, not a past session.
        assert_eq!(
            app.filtered,
            vec![PickerItem::History(1), PickerItem::History(2)]
        );
        assert_eq!(
            app.selected_selection(),
            Some(PickerSelection::Resume("codex:past-id".to_string()))
        );

        ctrl_a(&mut app);
        assert_eq!(app.activity, ActivityFilter::All);
        assert!(app.query.is_empty());
    }

    #[test]
    fn activity_filter_narrows_query_results_to_live_or_past_rows() {
        let mut app = app_with_live_and_past();
        app.query = "conversation".to_string();
        app.query_changed();
        app.apply_dredge_update(SearchUpdate::Results {
            query: "conversation".to_string(),
            hits: vec![
                SearchHit {
                    provider: "codex".to_string(),
                    session_id: "past-id".to_string(),
                    score: 0.9,
                    match_source: None,
                    snippet: None,
                },
                SearchHit {
                    provider: "claude".to_string(),
                    session_id: "live-id".to_string(),
                    score: 0.8,
                    match_source: None,
                    snippet: None,
                },
            ],
        });
        assert_eq!(
            app.filtered,
            vec![PickerItem::History(1), PickerItem::Process(0)]
        );

        ctrl_a(&mut app);
        assert_eq!(app.filtered, vec![PickerItem::Process(0)]);
        ctrl_a(&mut app);
        assert_eq!(app.filtered, vec![PickerItem::History(1)]);
    }

    #[test]
    fn filter_hint_sits_under_the_prompt_and_marks_the_current_filter() {
        let mut app = app_with_live_and_past();
        let screen = drawn_picker(&mut app, 70, 12);
        assert!(screen[0].starts_with("Attach or resume:"), "{screen:?}");
        assert!(
            screen[1].starts_with("ctrl-a show: [all] · live · past"),
            "{screen:?}"
        );
        ctrl_a(&mut app);
        ctrl_a(&mut app);
        let screen = drawn_picker(&mut app, 70, 12);
        assert!(
            screen[1].starts_with("ctrl-a show: all · live · [past]"),
            "{screen:?}"
        );
        assert!(screen[0].contains("2 past"), "{screen:?}");
    }

    #[test]
    fn past_filter_with_no_resumable_sessions_says_so() {
        let mut app = app_with(&[session("alpha", "zsh", "/tmp/project")]);
        ctrl_a(&mut app);
        ctrl_a(&mut app);
        assert!(app.filtered.is_empty());
        let screen = drawn_picker(&mut app, 70, 12).join("\n");
        assert!(screen.contains("No resumable past sessions."), "{screen}");
    }

    #[test]
    fn non_matching_query_empties_and_clamps() {
        let mut app = app_with(&[session("only", "zsh", "/tmp")]);
        app.query = "zzzznomatch".to_string();
        app.recompute_filter();
        assert!(app.filtered.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_name(), None);
    }

    #[test]
    fn cursor_moves_and_clamps_over_filtered_list() {
        let mut app = app_with(&[
            session("a", "zsh", "/tmp/a"),
            session("b", "zsh", "/tmp/b"),
            session("c", "zsh", "/tmp/c"),
        ]);
        app.move_cursor(1, 40);
        assert_eq!(app.selected, 1);
        app.move_cursor(50, 40);
        assert_eq!(app.selected, 2);
        app.move_cursor(-50, 40);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn refilter_clamps_selection() {
        let mut app = app_with(&[
            session("keep", "zsh", "/tmp/a"),
            session("drop-one", "zsh", "/tmp/b"),
            session("drop-two", "zsh", "/tmp/c"),
        ]);
        app.move_cursor(2, 40);
        assert_eq!(app.selected, 2);
        app.query = "keep".to_string();
        app.recompute_filter();
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected, 0);
    }
}
