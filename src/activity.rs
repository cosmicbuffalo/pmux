//! Agent-transcript activity extraction for the sidebar details drawer.
//!
//! Given a pmux process name, this locates the underlying agent transcript
//! (Claude `~/.claude/projects/*/<id>.jsonl`, Codex
//! `~/.codex/sessions/**/rollout-*-<id>.jsonl`) via the native `session_id`
//! pmux already persists in `agent_sessions/<name>`, parses it, and produces a
//! compact [`ActivityData`] — the per-event churn/token series plus summary
//! stats that the drawer renders as a btop-style braille timeline.
//!
//! Everything here is pure/offline and intended to run on a background thread
//! (see the sidebar's fetch path); it never touches tmux or the render loop.

use crate::agents::{Agent, locate_transcript};
use crate::state::read_session_agent_identity;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

/// Which quantity the drawer timeline plots. Cycled live with a keybind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerMetric {
    /// Lines added + deleted (code churn).
    Churn,
    /// Model output tokens generated: the default.
    OutputTokens,
    /// Input tokens each turn read, cached context included: in effect the
    /// context size, so its graph plots each slice's largest turn.
    InputTokens,
}

impl DrawerMetric {
    pub fn next(self) -> Self {
        match self {
            Self::OutputTokens => Self::InputTokens,
            Self::InputTokens => Self::Churn,
            Self::Churn => Self::OutputTokens,
        }
    }

    /// The header's short label: `↓` for output, as agents show it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Churn => "+/-",
            Self::OutputTokens => "↓",
            Self::InputTokens => "↑",
        }
    }

    /// What the graph shows, in words, as the header names it after a change.
    pub fn description(self) -> &'static str {
        match self {
            Self::Churn => "lines changed",
            Self::OutputTokens => "output tokens",
            Self::InputTokens => "input tokens",
        }
    }

    /// Whether a graph slice shows its largest event rather than their sum.
    pub fn peaks(self) -> bool {
        self == Self::InputTokens
    }
}

/// How wall-clock time is mapped onto the graph's x-axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AxisMode {
    /// One bucket per event, ignoring time — maximum intra-burst detail.
    EventOrdered,
    /// Power-law gap warp: virtual time advances by `gap.powf(alpha)`.
    /// `alpha == 1.0` is true wall-clock; smaller compresses idle gaps and
    /// expands active bursts.
    Warp(f64),
}

/// The axis presets the `x` key cycles through. Index 2 (`Warp(0.5)`) is the
/// default — chosen on real sessions to compress idle without crushing bursts.
pub const AXIS_PRESETS: &[(&str, AxisMode)] = &[
    ("events", AxisMode::EventOrdered),
    ("α.25", AxisMode::Warp(0.25)),
    ("α.5", AxisMode::Warp(0.5)),
    ("α.75", AxisMode::Warp(0.75)),
    ("clock", AxisMode::Warp(1.0)),
];

/// The default axis-preset index (`Warp(0.5)`).
pub const DEFAULT_AXIS_INDEX: usize = 2;

/// Parsed per-process activity, ready to render. Series are `(epoch, weight)`
/// in file order; summary fields are totals over the whole transcript.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityData {
    pub churn: Vec<(i64, f64)>,
    pub out_tokens: Vec<(i64, f64)>,
    pub in_tokens: Vec<(i64, f64)>,
    pub prompts: u32,
    pub total_in: u64,
    pub total_out: u64,
    pub added: u64,
    pub deleted: u64,
    pub tools: u32,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Most recent genuine user prompt found in the transcript.
    pub summary: Option<String>,
    /// Up to the three most recent genuine user prompts, oldest first.
    pub recent_prompts: Vec<String>,
}

impl ActivityData {
    /// The `(epoch, weight)` series for `metric`.
    pub fn series(&self, metric: DrawerMetric) -> &[(i64, f64)] {
        match metric {
            DrawerMetric::Churn => &self.churn,
            DrawerMetric::OutputTokens => &self.out_tokens,
            DrawerMetric::InputTokens => &self.in_tokens,
        }
    }
}

/// The drawer's per-process state, cached and updated from the fetch thread.
#[derive(Debug, Clone)]
pub enum DrawerState {
    /// Fetch in progress on a background thread.
    Loading,
    /// Parsed activity is available.
    Ready(Box<ActivityData>),
    /// No agent transcript for this process (non-agent, or file gone).
    Unavailable,
}

/// Resolve a process to its parsed activity. Intended to run on a background
/// thread. Returns [`DrawerState::Unavailable`] for non-agent processes or when
/// the transcript cannot be found/parsed.
///
/// Transcripts are append-only logs that can grow to hundreds of megabytes,
/// and the sidebar asks again every few seconds, so parses are cached per file
/// and continue from where the last one stopped (see [`parse_transcript_cached`]).
pub fn load_activity(process_name: &str) -> DrawerState {
    let Ok(Some(identity)) = read_session_agent_identity(process_name) else {
        return DrawerState::Unavailable;
    };
    let Some(path) = locate_transcript(&identity.agent, &identity.session_id) else {
        return DrawerState::Unavailable;
    };
    match parse_transcript_cached(&path, &identity.agent) {
        Some(data) => DrawerState::Ready(Box::new(data)),
        None => DrawerState::Unavailable,
    }
}

/// The activity in a past session's transcript, for its drawer.
pub fn load_transcript_activity(path: &Path, agent: &str) -> DrawerState {
    match parse_transcript_cached(path, agent) {
        Some(data) => DrawerState::Ready(Box::new(data)),
        None => DrawerState::Unavailable,
    }
}

/// The agents whose transcripts can be parsed.
fn transcript_kind(agent: &str) -> Option<Agent> {
    match Agent::from_name(agent) {
        Some(kind @ (Agent::Claude | Agent::Codex | Agent::Cortex)) => Some(kind),
        _ => None,
    }
}

/// Parse a transcript into [`ActivityData`]. `None` if the file cannot be
/// opened or `agent` has no transcript parser (Claude, Codex and Cortex do); a
/// readable-but-empty file yields empty (but non-`None`) data.
#[cfg(test)]
pub fn parse_transcript(path: &Path, agent: &str) -> Option<ActivityData> {
    let kind = transcript_kind(agent)?;
    let file = File::open(path).ok()?;
    let mut data = ActivityData::default();
    ingest_lines(&mut BufReader::new(file), kind, &mut data);
    Some(data)
}

/// What identifies a transcript's contents cheaply: a change in any of these
/// means the file must be read again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }

    /// Whether `self` is the same file as `earlier`, only longer: everything
    /// already parsed is still there, so parsing can resume at the old offset.
    fn extends(&self, earlier: &Self) -> bool {
        #[cfg(unix)]
        if (self.device, self.inode) != (earlier.device, earlier.inode) {
            return false;
        }
        self.len > earlier.len
    }
}

#[derive(Debug)]
struct CachedTranscript {
    kind: Agent,
    stamp: FileStamp,
    /// Bytes already folded into `data`: always the end of a complete record.
    offset: u64,
    data: ActivityData,
    last_used: u64,
}

/// Parsed transcripts by path. Shared by every fetch thread in the process.
static TRANSCRIPTS: LazyLock<Mutex<TranscriptCache>> =
    LazyLock::new(|| Mutex::new(TranscriptCache::default()));

/// More transcripts than any sidebar shows at once; the least recently used
/// beyond this are forgotten.
const TRANSCRIPT_CACHE_CAPACITY: usize = 64;

#[derive(Debug, Default)]
struct TranscriptCache {
    entries: HashMap<PathBuf, CachedTranscript>,
    clock: u64,
}

impl TranscriptCache {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn store(&mut self, path: &Path, entry: CachedTranscript) {
        self.entries.insert(path.to_path_buf(), entry);
        while self.entries.len() > TRANSCRIPT_CACHE_CAPACITY {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

fn lock_transcripts() -> std::sync::MutexGuard<'static, TranscriptCache> {
    TRANSCRIPTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`parse_transcript`], reusing the previous parse of `path`: an unchanged
/// file costs one `stat`, and a grown one only parses the appended records. A
/// file that shrank, was rewritten, or was replaced (rotation) is parsed
/// again from the start.
pub fn parse_transcript_cached(path: &Path, agent: &str) -> Option<ActivityData> {
    let kind = transcript_kind(agent)?;
    let mut file = File::open(path).ok()?;
    let stamp = FileStamp::of(&file.metadata().ok()?);
    // The lock is held only to look up and store, never while parsing.
    let previous = {
        let mut cache = lock_transcripts();
        let now = cache.tick();
        match cache.entries.get_mut(path) {
            Some(entry) if entry.kind == kind && entry.stamp == stamp => {
                entry.last_used = now;
                return Some(entry.data.clone());
            }
            Some(entry) if entry.kind == kind && stamp.extends(&entry.stamp) => {
                Some((entry.offset, entry.data.clone()))
            }
            _ => None,
        }
    };
    let (start, mut data) = previous.unwrap_or_default();
    if file.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let consumed = ingest_lines(&mut BufReader::new(file), kind, &mut data);
    let mut cache = lock_transcripts();
    let last_used = cache.tick();
    cache.store(
        path,
        CachedTranscript {
            kind,
            stamp,
            offset: start + consumed,
            data: data.clone(),
            last_used,
        },
    );
    Some(data)
}

/// Fold every complete JSONL record from `reader` into `data`, returning how
/// many bytes were consumed. Invalid UTF-8 is replaced rather than ending the
/// parse, and a malformed line is skipped. A trailing record that is still
/// being written (no newline yet, and not valid JSON) is left unconsumed so a
/// later incremental parse picks it up whole.
fn ingest_lines(reader: &mut impl BufRead, kind: Agent, data: &mut ActivityData) -> u64 {
    let mut consumed = 0u64;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read as u64,
        };
        let complete = buffer.last() == Some(&b'\n');
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim();
        if line.is_empty() {
            if complete {
                consumed += read;
            }
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => {
                match kind {
                    Agent::Claude => ingest_claude_line(data, &value),
                    Agent::Cortex => ingest_cortex_line(data, &value),
                    _ => ingest_codex_line(data, &value),
                }
                consumed += read;
            }
            Err(_) if complete => consumed += read,
            Err(_) => break,
        }
    }
    consumed
}

fn note_time(data: &mut ActivityData, t: Option<i64>) {
    if let Some(t) = t {
        data.first = Some(data.first.map_or(t, |f| f.min(t)));
        data.last = Some(data.last.map_or(t, |l| l.max(t)));
    }
}

/// Cortex stores messages directly, with tool payloads nested under tool_use.
/// User messages may also have a second copy carrying server_message_id.
fn ingest_cortex_line(data: &mut ActivityData, v: &serde_json::Value) {
    if v.get("server_message_id").is_some() {
        return;
    }
    let t = v
        .get("created_on")
        .and_then(|t| t.as_i64())
        .map(|t| t / 1000)
        .or_else(|| {
            v.get("user_sent_time")
                .and_then(|t| t.as_str())
                .and_then(iso_to_epoch)
        });
    let blocks = v
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let content: Vec<_> = blocks
        .into_iter()
        // Cortex prepends harness reminders to its first user message. Only
        // marked prompt blocks belong in the sidebar summary.
        .filter(|block| v["role"] != "user" || block["is_user_prompt"] == true)
        .map(|block| {
            if block["type"] == "tool_use" {
                let mut tool = block
                    .get("tool_use")
                    .cloned()
                    .unwrap_or_else(|| block.clone());
                tool["type"] = "tool_use".into();
                // Cortex uses lowercase tool names for the Claude edit/write schema.
                match tool["name"].as_str() {
                    Some("edit") => tool["name"] = "Edit".into(),
                    Some("write") => tool["name"] = "Write".into(),
                    _ => {}
                }
                tool
            } else {
                block
            }
        })
        .collect();
    // Require the explicit prompt marker when present: tool results and harness
    // reminder messages are user-role records too.
    let prompt = v["role"] == "user"
        && content
            .iter()
            .any(|b| b["type"] == "text" && b["is_user_prompt"] == true);
    let wrapped = serde_json::json!({
        "isSidechain": v["role"] == "user" && !prompt,
        "message": {"role": v["role"], "content": content}
    });
    let churn_start = data.churn.len();
    ingest_claude_line(data, &wrapped);
    for event in &mut data.churn[churn_start..] {
        event.0 = t.unwrap_or(0);
    }
    note_time(data, t);
}

fn ingest_claude_line(data: &mut ActivityData, v: &serde_json::Value) {
    let t = v
        .get("timestamp")
        .and_then(|s| s.as_str())
        .and_then(iso_to_epoch);
    note_time(data, t);
    let Some(message) = v.get("message") else {
        return;
    };
    let role = message.get("role").and_then(|r| r.as_str());

    if let Some(usage) = message.get("usage") {
        let input = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .into_iter()
        .filter_map(|key| usage.get(key).and_then(|n| n.as_u64()))
        .sum::<u64>();
        data.total_in = data.total_in.saturating_add(input);
        if input > 0 {
            data.in_tokens.push((t.unwrap_or(0), input as f64));
        }

        let out = usage
            .get("output_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        if out > 0 {
            data.out_tokens.push((t.unwrap_or(0), out as f64));
            data.total_out += out;
        }
    }

    let content = message.get("content");
    if claude_is_prompt(v) {
        data.prompts += 1;
        if let Some(prompt) = prompt_text(content) {
            note_prompt(data, prompt);
        }
    }

    if role == Some("assistant")
        && let Some(serde_json::Value::Array(blocks)) = content
    {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            data.tools += 1;
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block.get("input");
            let (add, del) = claude_tool_churn(name, input);
            if add + del > 0 {
                data.added += add;
                data.deleted += del;
                data.churn.push((t.unwrap_or(0), (add + del) as f64));
            }
        }
    }
}

/// A genuine Claude user prompt: a non-sidechain user message whose content is
/// a non-blank string, or a content list carrying a text block with no
/// tool_result block (which would be a tool echo, not a prompt).
pub(crate) fn claude_is_prompt(v: &serde_json::Value) -> bool {
    let Some(message) = v.get("message") else {
        return false;
    };
    if message.get("role").and_then(|r| r.as_str()) != Some("user")
        || v.get("isSidechain")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
    {
        return false;
    }
    match message.get("content") {
        Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
        Some(serde_json::Value::Array(blocks)) => {
            let has_text = blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"));
            let has_tool_result = blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
            has_text && !has_tool_result
        }
        _ => false,
    }
}

fn claude_tool_churn(name: &str, input: Option<&serde_json::Value>) -> (u64, u64) {
    let lines = |v: Option<&serde_json::Value>, key: &str| -> u64 {
        v.and_then(|i| i.get(key))
            .and_then(|s| s.as_str())
            .map(|s| s.lines().count().max(1) as u64)
            .unwrap_or(0)
    };
    match name {
        "Edit" => (lines(input, "new_string"), lines(input, "old_string")),
        "Write" => (lines(input, "content"), 0),
        "MultiEdit" => {
            let mut add = 0;
            let mut del = 0;
            if let Some(edits) = input
                .and_then(|i| i.get("edits"))
                .and_then(|e| e.as_array())
            {
                for edit in edits {
                    add += lines(Some(edit), "new_string");
                    del += lines(Some(edit), "old_string");
                }
            }
            (add, del)
        }
        _ => (0, 0),
    }
}

pub(crate) fn prompt_text(content: Option<&serde_json::Value>) -> Option<String> {
    let text = match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .map(str::to_string)?,
        _ => return None,
    };
    clean_summary(text)
}

fn ingest_codex_line(data: &mut ActivityData, v: &serde_json::Value) {
    let t = v
        .get("timestamp")
        .and_then(|s| s.as_str())
        .and_then(iso_to_epoch);
    note_time(data, t);
    let Some(payload) = v.get("payload") else {
        return;
    };
    let kind = v.get("type").and_then(|t| t.as_str());
    let ptype = payload.get("type").and_then(|t| t.as_str());

    if kind == Some("event_msg") && ptype == Some("token_count") {
        let usage = payload.get("info").and_then(|i| i.get("last_token_usage"));
        let input = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        data.total_in = data.total_in.saturating_add(input);
        if input > 0 {
            data.in_tokens.push((t.unwrap_or(0), input as f64));
        }

        let out = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        if out > 0 {
            data.out_tokens.push((t.unwrap_or(0), out as f64));
            data.total_out += out;
        }
    }

    if kind == Some("response_item") {
        match ptype {
            Some("message") => {
                if payload.get("role").and_then(|r| r.as_str()) == Some("user") {
                    data.prompts += 1;
                    if let Some(prompt) = codex_message_text(payload) {
                        note_prompt(data, prompt);
                    }
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                data.tools += 1;
                // Codex edits ride inside the `exec` tool's `apply_patch`
                // heredoc; the body is under `arguments` or `input`.
                let body = payload
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .or_else(|| payload.get("input").and_then(|i| i.as_str()));
                if let Some(body) = body
                    && body.contains("apply_patch")
                {
                    let (add, del) = count_patch(body);
                    if add + del > 0 {
                        data.added += add;
                        data.deleted += del;
                        data.churn.push((t.unwrap_or(0), (add + del) as f64));
                    }
                }
            }
            _ => {}
        }
    }
}

fn note_prompt(data: &mut ActivityData, prompt: String) {
    data.summary = Some(prompt.clone());
    data.recent_prompts.push(prompt);
    if data.recent_prompts.len() > 3 {
        data.recent_prompts.remove(0);
    }
}

pub(crate) fn codex_message_text(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .find_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::to_string)?,
        _ => return None,
    };
    clean_summary(text)
}

/// Collapse whitespace and reject blank or injected system/tool blocks (the
/// caret-wrapped `<environment_context>` / `<local-command-caveat>` / tool
/// echoes that aren't the user's actual prompt), so the drawer summary keeps
/// the most recent real prose prompt. Terminal escape sequences and other
/// control characters are removed first: a transcript is untrusted text that
/// ends up drawn in the sidebar and in tmux menus.
fn clean_summary(text: String) -> Option<String> {
    let text = strip_terminal_controls(&text);
    let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Codex injects the repo's AGENTS.md as the first user message.
    if trimmed.is_empty() || trimmed.starts_with('<') || trimmed.starts_with("# AGENTS.md") {
        return None;
    }
    Some(trimmed)
}

/// Remove ANSI escape sequences (CSI, OSC, and other `ESC`-introduced ones),
/// C0/C1 control characters, and bidirectional overrides, turning each into a
/// space so words either side stay apart. Zero-width joiners stay: emoji
/// sequences need them.
fn strip_terminal_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                match chars.next() {
                    // CSI: parameters and intermediates up to a final byte.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC, DCS, and friends: a string ended by BEL or ST.
                    Some(']' | 'P' | 'X' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    // Two-character escapes such as `ESC c`.
                    _ => {}
                }
                out.push(' ');
            }
            // A C1 CSI introducer behaves like `ESC [`.
            '\u{9b}' => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
                out.push(' ');
            }
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Count added/deleted lines in an `apply_patch` body. The patch text may embed
/// literal `\n` (it rides inside a JS string) or real newlines, so normalize
/// the former first, then count leading `+`/`-` lines (ignoring the `+++`/`---`
/// / `*** ` headers).
fn count_patch(body: &str) -> (u64, u64) {
    let normalized = body.replace("\\n", "\n");
    let mut add = 0;
    let mut del = 0;
    for line in normalized.split('\n') {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("*** ") {
            continue;
        }
        if line.starts_with('+') {
            add += 1;
        } else if line.starts_with('-') {
            del += 1;
        }
    }
    (add, del)
}

/// Parse an RFC3339 / ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`)
/// to a Unix epoch in seconds. Deliberately dependency-free (the crate carries
/// no `chrono`/`time`); fractional seconds are ignored. Returns `None` on any
/// shape it does not recognize.
pub fn iso_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;

    // Split the time from any trailing zone designator.
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(idx) = rest.rfind(['+', '-']) {
        // Guard against a bare time with no offset (idx would be absent).
        let (t, off) = rest.split_at(idx);
        (t, parse_offset(off).unwrap_or(0))
    } else {
        (rest, 0)
    };
    // Drop fractional seconds.
    let time = time.split('.').next().unwrap_or(time);
    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let minute: i64 = tparts.next()?.parse().ok()?;
    let second: i64 = tparts.next().unwrap_or("0").parse().ok()?;

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs)
}

fn parse_offset(off: &str) -> Option<i64> {
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let body = &off[1..];
    let (h, m) = body.split_once(':').unwrap_or((body, "0"));
    Some(sign * (h.parse::<i64>().ok()? * 3_600 + m.parse::<i64>().ok()? * 60))
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Bucket a `(epoch, weight)` series into `n` samples along the chosen axis.
/// Weights within a bucket are summed; the caller normalizes for display.
pub fn warp_buckets(events: &[(i64, f64)], n: usize, mode: AxisMode) -> Vec<f64> {
    warp_buckets_with(events, n, mode, |sum, w| sum + w)
}

/// [`warp_buckets`] keeping each bucket's largest weight instead of the sum,
/// for a level such as context size rather than an amount of work.
pub fn warp_bucket_peaks(events: &[(i64, f64)], n: usize, mode: AxisMode) -> Vec<f64> {
    warp_buckets_with(events, n, mode, f64::max)
}

fn warp_buckets_with(
    events: &[(i64, f64)],
    n: usize,
    mode: AxisMode,
    combine: fn(f64, f64) -> f64,
) -> Vec<f64> {
    let mut buckets = vec![0.0; n.max(1)];
    if events.is_empty() {
        return buckets;
    }
    let mut evs: Vec<(i64, f64)> = events.iter().copied().filter(|(t, _)| *t > 0).collect();
    evs.sort_by_key(|(t, _)| *t);
    if evs.is_empty() {
        return buckets;
    }
    match mode {
        AxisMode::EventOrdered => {
            let len = evs.len();
            for (i, (_, w)) in evs.iter().enumerate() {
                let j = (i * n / len).min(n - 1);
                buckets[j] = combine(buckets[j], *w);
            }
        }
        AxisMode::Warp(alpha) => {
            // Advance a virtual clock by gap^alpha between consecutive events,
            // then bucket linearly over that warped span.
            let mut virt = Vec::with_capacity(evs.len());
            let mut vt = 0.0f64;
            let mut prev = evs[0].0;
            for (t, w) in &evs {
                let gap = (t - prev).max(0) as f64;
                if gap > 0.0 {
                    vt += gap.powf(alpha);
                }
                prev = *t;
                virt.push((vt, *w));
            }
            let span = if vt > 0.0 { vt } else { 1.0 };
            for (v, w) in virt {
                let j = ((v / span) * n as f64) as usize;
                let j = j.min(n - 1);
                buckets[j] = combine(buckets[j], w);
            }
        }
    }
    buckets
}

// Braille dot bit tables: left column top→bottom (dots 1,2,3,7), right column
// top→bottom (dots 4,5,6,8). Base code point is U+2800.
const BRAILLE_LEFT: [u32; 4] = [0x01, 0x02, 0x04, 0x40];
const BRAILLE_RIGHT: [u32; 4] = [0x08, 0x10, 0x20, 0x80];

/// Render `values` as a `rows`-tall braille area graph, `width` chars wide (two
/// samples per char). Per-series max-normalized; with `baseline` on, every
/// column keeps at least the bottom dot so the graph reads as one connected
/// line. Returns `rows` strings, top to bottom.
pub fn braille_rows(values: &[f64], width: usize, rows: usize, baseline: bool) -> Vec<String> {
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let dots = (rows * 4) as f64;
    let samples = resample(values, width * 2);
    let max = samples.iter().cloned().fold(0.0f64, f64::max);
    let norm = if max > 0.0 { max } else { 1.0 };
    let filled: Vec<u32> = samples
        .iter()
        .map(|&v| {
            let mut f = (v / norm * dots).round() as i64;
            if f == 0 && v > 0.0 {
                f = 1;
            }
            if baseline {
                f = f.max(1);
            }
            f.clamp(0, dots as i64) as u32
        })
        .collect();
    let total = (rows * 4) as u32;
    (0..rows)
        .map(|cr| {
            (0..width)
                .map(|cc| {
                    let mut ch = 0x2800u32;
                    for (bits, si) in [(BRAILLE_LEFT, 2 * cc), (BRAILLE_RIGHT, 2 * cc + 1)] {
                        let f = filled.get(si).copied().unwrap_or(0);
                        for (k, bit) in bits.iter().enumerate() {
                            if (cr as u32) * 4 + k as u32 >= total - f {
                                ch |= bit;
                            }
                        }
                    }
                    char::from_u32(ch).unwrap_or(' ')
                })
                .collect::<String>()
        })
        .collect()
}

/// Average `values` into exactly `n` bins (nearest-count binning).
fn resample(values: &[f64], n: usize) -> Vec<f64> {
    if values.is_empty() || n == 0 {
        return vec![0.0; n];
    }
    if values.len() == n {
        return values.to_vec();
    }
    let mut sum = vec![0.0; n];
    let mut cnt = vec![0u32; n];
    for (i, &v) in values.iter().enumerate() {
        let j = (i * n / values.len()).min(n - 1);
        sum[j] += v;
        cnt[j] += 1;
    }
    (0..n)
        .map(|j| {
            if cnt[j] > 0 {
                sum[j] / cnt[j] as f64
            } else {
                0.0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_epoch_parses_utc_z() {
        // 2021-01-01T00:00:00Z == 1609459200
        assert_eq!(iso_to_epoch("2021-01-01T00:00:00Z"), Some(1_609_459_200));
        // Fractional seconds are ignored.
        assert_eq!(
            iso_to_epoch("2021-01-01T00:00:00.512Z"),
            Some(1_609_459_200)
        );
        // Epoch itself.
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn iso_to_epoch_applies_offset() {
        // 2021-01-01T01:00:00+01:00 is the same instant as 00:00:00Z.
        assert_eq!(
            iso_to_epoch("2021-01-01T01:00:00+01:00"),
            Some(1_609_459_200)
        );
    }

    #[test]
    fn iso_to_epoch_rejects_garbage() {
        assert_eq!(iso_to_epoch("not-a-date"), None);
        assert_eq!(iso_to_epoch(""), None);
    }

    fn claude_prompt_line(text: &str) -> String {
        let mut line = serde_json::json!({
            "timestamp": "2021-01-01T00:00:00Z",
            "message": {"role": "user", "content": text}
        })
        .to_string();
        line.push('\n');
        line
    }

    fn claude_output_line(tokens: u64) -> String {
        let mut line = serde_json::json!({
            "timestamp": "2021-01-01T00:01:00Z",
            "message": {"role": "assistant", "usage": {"output_tokens": tokens}, "content": []}
        })
        .to_string();
        line.push('\n');
        line
    }

    fn append(path: &Path, text: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap()
            .write_all(text)
            .unwrap();
    }

    #[test]
    fn cached_parse_continues_from_the_last_record_and_matches_a_full_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, claude_prompt_line("first").as_bytes());
        append(&path, claude_output_line(10).as_bytes());
        let first = parse_transcript_cached(&path, "claude").unwrap();
        assert_eq!(first.prompts, 1);
        assert_eq!(first, parse_transcript(&path, "claude").unwrap());

        // A record still being written is not counted, and not lost: it is
        // parsed whole once its newline lands.
        let second = claude_prompt_line("second");
        let (head, tail) = second.split_at(second.len() / 2);
        append(&path, head.as_bytes());
        assert_eq!(parse_transcript_cached(&path, "claude").unwrap().prompts, 1);
        append(&path, tail.as_bytes());
        append(&path, claude_output_line(5).as_bytes());
        let grown = parse_transcript_cached(&path, "claude").unwrap();
        assert_eq!(grown.prompts, 2);
        assert_eq!(grown.total_out, 15);
        assert_eq!(grown.summary.as_deref(), Some("second"));
        assert_eq!(grown, parse_transcript(&path, "claude").unwrap());

        // Unchanged: served from the cache.
        assert_eq!(parse_transcript_cached(&path, "claude").unwrap(), grown);
    }

    #[test]
    fn cached_parse_starts_over_after_truncation_or_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, claude_prompt_line("one").as_bytes());
        append(&path, claude_prompt_line("two").as_bytes());
        assert_eq!(parse_transcript_cached(&path, "claude").unwrap().prompts, 2);

        std::fs::write(&path, claude_prompt_line("fresh")).unwrap();
        let truncated = parse_transcript_cached(&path, "claude").unwrap();
        assert_eq!(truncated.prompts, 1);
        assert_eq!(truncated.summary.as_deref(), Some("fresh"));

        // Rotated: a different, longer file now at the same path.
        let replacement = dir.path().join("replacement.jsonl");
        for text in ["a", "b", "c"] {
            append(&replacement, claude_prompt_line(text).as_bytes());
        }
        std::fs::rename(&replacement, &path).unwrap();
        let rotated = parse_transcript_cached(&path, "claude").unwrap();
        assert_eq!(rotated.prompts, 3);
        assert_eq!(rotated.recent_prompts, ["a", "b", "c"]);
    }

    #[test]
    fn invalid_utf8_does_not_end_the_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, claude_prompt_line("before").as_bytes());
        append(&path, b"{\"message\": \"\xff\xfe broken\"}\n");
        append(&path, claude_prompt_line("after").as_bytes());
        let data = parse_transcript(&path, "claude").unwrap();
        assert_eq!(data.prompts, 2);
        assert_eq!(data.summary.as_deref(), Some("after"));
    }

    #[test]
    fn summaries_drop_terminal_escapes_and_control_characters() {
        assert_eq!(
            clean_summary("\u{1b}[31mred\u{1b}[0m text\u{7}".to_string()).as_deref(),
            Some("red text")
        );
        assert_eq!(
            clean_summary(
                "title \u{1b}]0;pwned\u{7}set \u{1b}]8;;http://x\u{1b}\\link".to_string()
            )
            .as_deref(),
            Some("title set link")
        );
        assert_eq!(
            clean_summary("a\u{202e}b\u{9b}2Jc\rd".to_string()).as_deref(),
            Some("a b c d")
        );
        // Emoji ZWJ sequences survive.
        let family = "👩\u{200d}👩\u{200d}👧";
        assert_eq!(clean_summary(family.to_string()).as_deref(), Some(family));
        assert_eq!(clean_summary("\u{1b}[2J".to_string()), None);
    }

    #[test]
    fn count_patch_handles_escaped_and_real_newlines() {
        let escaped = "*** Begin Patch\\n*** Update File: x.rs\\n+added line\\n-removed line\\n";
        assert_eq!(count_patch(escaped), (1, 1));
        let real = "*** Begin Patch\n+a\n+b\n-c\n";
        assert_eq!(count_patch(real), (2, 1));
    }

    #[test]
    fn claude_churn_counts_edit_and_write() {
        assert_eq!(
            claude_tool_churn(
                "Edit",
                Some(&serde_json::json!({"old_string": "a\nb", "new_string": "a\nb\nc"}))
            ),
            (3, 2)
        );
        assert_eq!(
            claude_tool_churn("Write", Some(&serde_json::json!({"content": "one\ntwo"}))),
            (2, 0)
        );
    }

    #[test]
    fn ingest_claude_counts_prompt_tokens_and_churn() {
        let mut data = ActivityData::default();
        let user = serde_json::json!({
            "timestamp": "2021-01-01T00:00:00Z",
            "message": {"role": "user", "content": "please refactor this"}
        });
        let assistant = serde_json::json!({
            "timestamp": "2021-01-01T00:01:00Z",
            "message": {
                "role": "assistant",
                "usage": {
                    "input_tokens": 100,
                    "cache_creation_input_tokens": 20,
                    "cache_read_input_tokens": 300,
                    "output_tokens": 40
                },
                "content": [{"type": "tool_use", "name": "Write", "input": {"content": "a\nb\nc"}}]
            }
        });
        let latest_user = serde_json::json!({
            "timestamp": "2021-01-01T00:02:00Z",
            "message": {"role": "user", "content": "now update the tests"}
        });
        ingest_claude_line(&mut data, &user);
        ingest_claude_line(&mut data, &assistant);
        ingest_claude_line(&mut data, &latest_user);
        assert_eq!(data.prompts, 2);
        assert_eq!(data.total_in, 420);
        assert_eq!(data.in_tokens.len(), 1);
        assert_eq!(data.total_out, 40);
        assert_eq!(data.added, 3);
        assert_eq!(data.summary.as_deref(), Some("now update the tests"));
        assert_eq!(
            data.recent_prompts,
            ["please refactor this", "now update the tests"]
        );
        assert_eq!(data.out_tokens.len(), 1);
        assert_eq!(data.churn.len(), 1);
    }

    #[test]
    fn codex_summary_tracks_the_latest_real_user_prompt() {
        let mut data = ActivityData::default();
        for (timestamp, text) in [
            ("2021-01-01T00:00:00Z", "first request"),
            (
                "2021-01-01T00:01:00Z",
                "<environment_context>injected context</environment_context>",
            ),
            ("2021-01-01T00:02:00Z", "latest request with details"),
        ] {
            ingest_codex_line(
                &mut data,
                &serde_json::json!({
                    "timestamp": timestamp,
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": text}]
                    }
                }),
            );
        }

        assert_eq!(data.prompts, 3);
        assert_eq!(data.summary.as_deref(), Some("latest request with details"));
        assert_eq!(
            data.recent_prompts,
            ["first request", "latest request with details"]
        );
    }

    #[test]
    fn ingest_codex_counts_input_and_output_tokens() {
        let mut data = ActivityData::default();
        ingest_codex_line(
            &mut data,
            &serde_json::json!({
                "timestamp": "2021-01-01T00:03:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1_250,
                            "output_tokens": 375
                        }
                    }
                }
            }),
        );

        assert_eq!(data.total_in, 1_250);
        assert_eq!(data.total_out, 375);
        assert_eq!(data.in_tokens, [(1_609_459_380, 1_250.0)]);
    }

    #[test]
    fn peak_buckets_keep_each_slice_largest_event_instead_of_the_sum() {
        let events = [(1, 100.0), (2, 300.0), (3, 200.0), (4, 50.0)];
        assert_eq!(
            warp_buckets(&events, 2, AxisMode::EventOrdered),
            [400.0, 250.0]
        );
        assert_eq!(
            warp_bucket_peaks(&events, 2, AxisMode::EventOrdered),
            [300.0, 200.0]
        );
    }

    #[test]
    fn the_graph_cycles_output_input_then_churn_tokens_first() {
        let metric = DrawerMetric::OutputTokens;
        assert_eq!(metric.label(), "↓");
        assert_eq!(metric.next(), DrawerMetric::InputTokens);
        assert_eq!(metric.next().label(), "↑");
        assert!(metric.next().peaks());
        assert_eq!(metric.next().next(), DrawerMetric::Churn);
        assert_eq!(metric.next().next().next(), metric);
    }

    #[test]
    fn warp_event_ordered_spreads_across_all_buckets() {
        let events: Vec<(i64, f64)> = (0..8).map(|i| (1000 + i, 1.0)).collect();
        let buckets = warp_buckets(&events, 4, AxisMode::EventOrdered);
        assert_eq!(buckets.len(), 4);
        assert!(buckets.iter().all(|&b| b > 0.0));
    }

    #[test]
    fn warp_clock_puts_isolated_late_event_in_last_bucket() {
        // Cluster early, one event far later → wall-clock pushes it to the end.
        let mut events: Vec<(i64, f64)> = (0..4).map(|i| (i, 1.0)).collect();
        events.push((1_000_000, 5.0));
        let buckets = warp_buckets(&events, 10, AxisMode::Warp(1.0));
        assert!(buckets[9] > 0.0);
        assert_eq!(buckets[1..9].iter().sum::<f64>(), 0.0);
    }

    #[test]
    fn braille_baseline_fills_bottom_row() {
        // All-zero series with baseline → bottom row is all bottom-dot glyphs,
        // upper rows blank.
        let rows = braille_rows(&[0.0; 8], 4, 3, true);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "\u{2800}".repeat(4)); // top row blank
        assert!(rows[2].chars().all(|c| c == '\u{28c0}')); // dots 7+8
    }

    #[test]
    fn braille_no_baseline_is_blank_for_zero() {
        let rows = braille_rows(&[0.0; 8], 4, 3, false);
        assert!(rows.iter().all(|r| r.chars().all(|c| c == '\u{2800}')));
    }

    #[test]
    fn braille_peak_reaches_top_row() {
        let mut vals = vec![0.0; 8];
        vals[7] = 100.0; // one tall column
        let rows = braille_rows(&vals, 4, 3, true);
        // The last char of the top row should carry dots for that column.
        assert_ne!(rows[0].chars().last().unwrap(), '\u{2800}');
    }

    #[test]
    fn cortex_activity_uses_native_prompts_nested_tools_and_timestamps() {
        let mut data = ActivityData::default();
        let prompt = serde_json::json!({"role":"user", "user_sent_time":"2026-10-30T18:00:00Z",
            "content":[{"type":"text","text":"<system-reminder>Injected harness context</system-reminder>", "is_user_prompt":false},{"type":"text","text":"Fix the relay", "is_user_prompt":true}]});
        ingest_cortex_line(&mut data, &prompt);
        let mut duplicate = prompt.clone();
        duplicate["server_message_id"] = "copy".into();
        ingest_cortex_line(&mut data, &duplicate);
        ingest_cortex_line(
            &mut data,
            &serde_json::json!({"role":"user", "content":[
            {"type":"tool_result", "tool_result":{"content":[{"type":"text","text":"output"}]}}]}),
        );
        ingest_cortex_line(
            &mut data,
            &serde_json::json!({"role":"assistant", "created_on":1793383201000_i64,
            "content":[{"type":"tool_use", "tool_use":{"name":"edit", "input":{"old_string":"old", "new_string":"new\nline"}}}]}),
        );
        assert_eq!(data.prompts, 1);
        assert_eq!(data.summary.as_deref(), Some("Fix the relay"));
        assert_eq!(data.tools, 1);
        assert_eq!((data.added, data.deleted), (2, 1));
        assert_eq!(data.churn, vec![(1793383201, 3.0)]);
        assert_eq!(data.total_out, 0); // Cortex messages do not expose token usage.
    }
}
