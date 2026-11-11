//! Machine-facing process queries and input: `ls`, `state`, `inspect`, `send`, `wait`.

use super::attach::resolve_attach_target;
use crate::api::{API_VERSION, ProcessInfo, StateSnapshot, build_state_snapshot};
use crate::session::SessionInfo;
use crate::tmux::{list_sessions, tmux_cmd, tmux_failure};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::io::Write;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

/// A condition `pmux wait` can observe without inspecting terminal text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    /// Wait until the pmux process session no longer exists.
    Exited,
    /// Wait until a fresh ai-attn record reports this state for the session.
    AgentState(String),
}

impl WaitCondition {
    pub fn label(&self) -> String {
        match self {
            Self::Exited => "exited".to_string(),
            Self::AgentState(state) => format!("agent:{state}"),
        }
    }
}

/// The observed result of a `pmux wait` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitResult {
    pub api_version: u32,
    pub session: String,
    pub condition: String,
    pub outcome: String,
    /// The requested condition that matched; absent for other outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_condition: Option<String>,
    pub observed_at: i64,
    pub process: Option<ProcessInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
}

impl WaitResult {
    /// For waits that give an unsignalled agent the whole timeout instead
    /// of the launch-based startup budget: an agent that still had not
    /// signalled when time ran out was never confirmed ready.
    fn never_signalled_as_not_ready(&mut self) {
        if self.outcome == "timed_out" && self.process.as_ref().is_some_and(|p| p.startup.is_some())
        {
            self.outcome = "not_ready".to_string();
        }
    }

    /// Capture is best effort: losing the pane must not change the wait outcome.
    pub fn capture_on_block(&mut self, lines: Option<u32>) {
        if self.outcome != "blocked" || lines.is_none() {
            return;
        }
        // Approval hooks can precede the TUI drawing the question by ~2s.
        // Bound the settling delay instead of guessing from screen contents;
        // calls without requested blocked capture still return immediately.
        thread::sleep(Duration::from_millis(2500));
        self.capture_screen(lines);
    }

    /// `send --confirm` also returns the screen when the agent never
    /// signalled: a startup dialog (folder trust, login) is already drawn.
    pub fn capture_on_not_ready(&mut self, lines: Option<u32>) {
        if self.outcome == "not_ready" && lines.is_some() {
            self.capture_screen(lines);
        }
    }

    fn capture_screen(&mut self, lines: Option<u32>) {
        match capture(&self.session, lines, false) {
            Ok(capture)
                if self
                    .process
                    .as_ref()
                    .is_some_and(|p| p.pane_id == capture.pane_id) =>
            {
                self.capture = Some(capture);
            }
            Ok(_) => self.capture_error = Some("process changed before screen capture".into()),
            Err(err) => self.capture_error = Some(err.to_string()),
        }
    }
}

/// One submitted prompt followed by the normal wait result and transcript reply.
#[derive(Debug, Serialize)]
pub struct AskResult {
    #[serde(flatten)]
    pub wait: WaitResult,
    /// What was typed; `null` when ask sent nothing because the agent never
    /// showed its ready text (outcome `not_ready`).
    pub send: Option<SendResult>,
    pub reply: Option<super::ReplyResult>,
}

pub fn ask(
    target: &str,
    input: &str,
    startup_timeout: Duration,
    timeout: Option<Duration>,
) -> Result<AskResult> {
    // Resolve selectors once, before injecting input. Remember the old signal
    // so a previous turn's done cannot complete this turn before work starts.
    let process = match await_ready_text(inspect(target)?, startup_timeout)? {
        Ok(process) => process,
        Err(not_ready) => {
            return Ok(AskResult {
                wait: not_ready,
                send: None,
                reply: None,
            });
        }
    };
    let previous_reply = super::reply(&process.name)?;
    let sent = send(&process.name, input, true)?;
    let waited = wait_impl(
        &sent.session,
        vec![WaitCondition::AgentState("done".into())],
        false,
        startup_timeout,
        timeout,
        process
            .agent
            .clone()
            .filter(|a| a.state.as_deref() == Some("done")),
        Some(&process),
    )?;
    let reply = if waited.outcome == "matched" {
        let mut reply = super::reply(&waited.session)?;
        if reply.available && reply == previous_reply {
            reply.available = false;
            reply.text = None;
            reply.reason = Some("transcript reply is unchanged since before send; retry pmux reply after the transcript is flushed".into());
        }
        Some(reply)
    } else {
        None
    };
    Ok(AskResult {
        wait: waited,
        send: Some(sent),
        reply,
    })
}

/// A prompt sent by `send --confirm`, then the wait for the agent to take it
/// up: `matched` once a fresh `working` (or an already finished `done`)
/// signal follows the send.
#[derive(Debug, Serialize)]
pub struct ConfirmedSend {
    #[serde(flatten)]
    pub wait: WaitResult,
    pub send: SendResult,
}

pub fn send_confirmed(target: &str, input: &str, timeout: Duration) -> Result<ConfirmedSend> {
    // As in `ask`: resolve once, and ignore the signal from before the send
    // so an agent's earlier turn cannot confirm this one.
    let process = inspect(target)?;
    let mut sent = send(&process.name, input, true)?;
    // An agent that has not signalled since launch is usually just at its
    // first prompt (a fresh Codex or Claude emits nothing until a turn
    // starts), so the launch-based startup budget would end the wait before
    // the prompt's own signal arrives. Give it the whole timeout instead,
    // and call it not_ready only if no signal came by then.
    let mut waited = wait_impl(
        &sent.session,
        vec![
            WaitCondition::AgentState("working".into()),
            WaitCondition::AgentState("done".into()),
        ],
        false,
        Duration::MAX,
        Some(timeout),
        process
            .agent
            .clone()
            .filter(|a| matches!(a.state.as_deref(), Some("working" | "done"))),
        Some(&process),
    )?;
    waited.never_signalled_as_not_ready();
    sent.submission_confirmed = waited.outcome == "matched";
    Ok(ConfirmedSend {
        wait: waited,
        send: sent,
    })
}

/// Before ask types anything, waits for an agent whose ready text is
/// configured but not on screen: it is most likely at a startup or approval
/// dialog, where the prompt would answer the dialog instead. Returns the
/// process to send to, or a `not_ready` wait result once `startup_timeout`
/// (measured from launch, as in wait) runs out. Agents that have signalled,
/// or have no ready text configured, pass straight through.
fn await_ready_text(
    mut process: ProcessInfo,
    startup_timeout: Duration,
) -> Result<std::result::Result<ProcessInfo, WaitResult>> {
    loop {
        let Some(startup) = process.startup.as_ref().filter(|s| s.ready_text_missing()) else {
            return Ok(Ok(process));
        };
        let now = crate::util::now_epoch();
        let since_launch =
            Duration::from_secs(now.saturating_sub(startup.started_at).max(0) as u64);
        if since_launch >= startup_timeout {
            return Ok(Err(WaitResult {
                api_version: API_VERSION,
                session: process.name.clone(),
                condition: WaitCondition::AgentState("done".into()).label(),
                outcome: "not_ready".into(),
                matched_condition: None,
                observed_at: now,
                process: Some(process),
                capture: None,
                capture_error: None,
            }));
        }
        thread::sleep(Duration::from_millis(200));
        process = inspect(&process.name)?;
    }
}

/// Confirmation of terminal input injection, not application-level acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SendResult {
    pub api_version: u32,
    pub session: String,
    pub pane_id: String,
    pub bytes_sent: usize,
    /// Legacy name: true means pmux sent Enter, not that the TUI accepted it.
    pub submitted: bool,
    pub enter_sent: bool,
    /// True only after `send --confirm` saw the agent take the prompt up;
    /// terminal injection alone cannot confirm it.
    pub submission_confirmed: bool,
}

/// List all pmux sessions sorted by most-recently-active first.
/// Called by `pmux ls`.
pub fn ls() -> Result<Vec<SessionInfo>> {
    let mut sessions = list_sessions()?;
    // Sort by most-recently-active first, matching the picker order.
    sessions.sort_by(|a, b| {
        b.last_activity_epoch
            .cmp(&a.last_activity_epoch)
            .then(b.created_epoch.cmp(&a.created_epoch))
    });
    Ok(sessions)
}

/// The processes in `sessions` spawned by `of`, or by the caller's own pmux
/// process when `of` is `None`: direct children, or with `all_levels` every
/// process below it. Called by `pmux ls`/`kill` `--children/--descendants`.
pub fn spawned_names(
    sessions: &[SessionInfo],
    of: Option<&str>,
    all_levels: bool,
) -> Result<Vec<String>> {
    let root = match of {
        Some(target) => resolve_attach_target(sessions, target)?.name.clone(),
        None => crate::lineage::caller_process().ok_or_else(|| {
            anyhow!("not running inside a pmux process; name the parent with --of NAME")
        })?,
    };
    Ok(crate::lineage::spawned_under(&root, sessions, all_levels))
}

/// Return a machine-readable snapshot of all live pmux processes.
///
/// This is process-generic: every running pmux session appears, while the
/// optional `agent` field is only populated for recognized AI commands.
pub fn state() -> Result<StateSnapshot> {
    Ok(build_state_snapshot(&ls()?))
}

/// Inspect one running process by name or the `newest`/`oldest` selector.
pub fn inspect(target: &str) -> Result<ProcessInfo> {
    let sessions = ls()?;
    let name = resolve_attach_target(&sessions, target)?.name.as_str();
    let snapshot = build_state_snapshot(&sessions);
    let mut process = snapshot
        .sessions
        .into_iter()
        .find(|session| session.name == name)
        .ok_or_else(|| anyhow!("process '{}' disappeared while inspecting it", name))?;
    if let Some(agent) = process.agent.as_mut() {
        agent.session_id = super::respawn::agent_session_id(
            &process.name,
            &process.pane_id,
            process.created_at,
            &agent.kind,
        );
    }
    Ok(process)
}

/// A read-only snapshot of the active terminal grid, not an agent transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureResult {
    pub api_version: u32,
    pub session: String,
    pub pane_id: String,
    pub text: String,
}

pub fn capture(target: &str, lines: Option<u32>, history: bool) -> Result<CaptureResult> {
    if lines == Some(0) {
        bail!("capture line count must be positive");
    }
    let sessions = list_sessions()?;
    let session = resolve_attach_target(&sessions, target)?;
    let mut command = tmux_cmd();
    // Default capture uses the active screen, including alternate-screen TUIs.
    // Do not join wrapped lines, crop at the cursor, trim blank rows, or use
    // completion-replay cleanup: prompt boxes can extend below the cursor.
    command.args(["capture-pane", "-p", "-t", &session.pane_id]);
    if history || lines.is_some() {
        command.args(["-S", "-"]);
    }
    let output = command
        .output()
        .context("failed to capture pmux process screen")?;
    if !output.status.success() {
        return Err(tmux_failure(
            "failed to capture pmux process screen",
            &output,
        ));
    }
    let text = capture_text(&String::from_utf8_lossy(&output.stdout), lines);
    Ok(CaptureResult {
        api_version: API_VERSION,
        session: session.name.clone(),
        pane_id: session.pane_id.clone(),
        text,
    })
}

fn capture_text(text: &str, lines: Option<u32>) -> String {
    let text = if let Some(limit) = lines {
        // Choose rows from the bottom without stripping blank rows; those are
        // meaningful screen coordinates, especially for an approval dialog.
        let skip = text
            .split_inclusive('\n')
            .count()
            .saturating_sub(limit as usize);
        text.split_inclusive('\n').skip(skip).collect::<String>()
    } else {
        text.to_string()
    };
    // tmux omits attributes unless -e is supplied. Sanitize any remaining
    // terminal controls while retaining the physical rows exactly.
    text.split('\n')
        .map(crate::util::sanitize_terminal_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Send literal input to the active pane of a pmux process.
///
/// Submitted text uses bracketed paste when the target enables it, followed
/// by a separate Enter after a short settling interval for TUI paste handling.
/// Unsubmitted input uses literal keys, including single approval responses.
pub fn send(target: &str, input: &str, submit: bool) -> Result<SendResult> {
    if input.contains('\0') {
        bail!("input contains an unsupported nul byte");
    }

    let sessions = list_sessions()?;
    let session = resolve_attach_target(&sessions, target)?;
    let name = session.name.clone();
    let pane_id = session.pane_id.clone();
    let receipt = crate::input::InputReceipt::before_send(session, &sessions);

    if submit && !input.is_empty() {
        paste_input(&pane_id, input)?;
        // TUI input loops may process paste completion asynchronously. Keep
        // Enter out of the paste burst (including heuristic paste detection).
        thread::sleep(Duration::from_millis(100));
    } else if !input.is_empty() {
        let output = tmux_cmd()
            .arg("send-keys")
            .arg("-t")
            .arg(&pane_id)
            .arg("-l")
            // `--` ends option parsing: without it, text such as `-R` is taken as
            // a send-keys flag and `- fix bug` fails with "invalid flag".
            .arg("--")
            .arg(input)
            .output()
            .context("failed to send literal input to pmux process")?;
        if !output.status.success() {
            return Err(tmux_failure(
                "failed to send input to pmux process",
                &output,
            ));
        }
    }

    if submit {
        let output = tmux_cmd()
            .arg("send-keys")
            .arg("-t")
            .arg(&pane_id)
            .arg("Enter")
            .output()
            .context("failed to send Enter to pmux process")?;
        if !output.status.success() {
            return Err(tmux_failure(
                "failed to send Enter to pmux process",
                &output,
            ));
        }
    }

    if submit || !input.is_empty() {
        receipt.record(&name)?;
    }

    Ok(SendResult {
        api_version: API_VERSION,
        session: name,
        pane_id,
        bytes_sent: input.len(),
        submitted: submit,
        enter_sent: submit,
        submission_confirmed: false,
    })
}

/// Named-key injection has its own result: no implicit Enter or byte-count
/// guess, since terminal encodings depend on the application's keyboard mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeySendResult {
    pub api_version: u32,
    pub session: String,
    pub pane_id: String,
    pub keys: Vec<String>,
    pub repeat: u16,
    pub keys_sent: usize,
    pub submission_confirmed: bool,
}

/// The send-keys argument for a validated key. tmux reads a lone `;` argument
/// as a command separator, so a `;` key (or one with modifiers, `M-;`) must be
/// escaped to reach the pane.
fn tmux_key_arg(key: &str) -> String {
    match key.strip_suffix(';') {
        Some(modifiers) => format!("{modifiers}\\;"),
        None => key.to_string(),
    }
}

fn valid_key(key: &str) -> bool {
    let mut base = key;
    let mut modifiers = Vec::new();
    while base.starts_with("C-") || base.starts_with("M-") || base.starts_with("S-") {
        let modifier = &base[..2];
        if modifiers.contains(&modifier) {
            return false;
        }
        modifiers.push(modifier);
        base = &base[2..];
    }
    matches!(
        base,
        "Escape"
            | "Enter"
            | "BSpace"
            | "Tab"
            | "BTab"
            | "Space"
            | "Up"
            | "Down"
            | "Left"
            | "Right"
            | "Home"
            | "End"
            | "PPage"
            | "NPage"
            | "IC"
            | "DC"
    ) || (base.len() == 1 && base.as_bytes()[0].is_ascii_graphic())
        || base.strip_prefix('F').is_some_and(|number| {
            number
                .parse::<u8>()
                .is_ok_and(|n| (1..=12).contains(&n) && number == n.to_string())
        })
}

pub fn send_keys(target: &str, keys: &[String], repeat: u16) -> Result<KeySendResult> {
    if keys.is_empty()
        || !(1..=1000).contains(&repeat)
        || keys.len().saturating_mul(repeat as usize) > 10000
    {
        bail!("provide named keys with repeat 1..=1000 and at most 10000 total keys");
    }
    for key in keys {
        if !valid_key(key) {
            bail!(
                "unsupported named key {:?}; see the send documentation for allowed keys",
                key
            );
        }
    }
    let sessions = list_sessions()?;
    let session = resolve_attach_target(&sessions, target)?;
    let receipt = crate::input::InputReceipt::before_send(session, &sessions);
    let keys_sent = keys.len() * repeat as usize;
    // Build one argv vector; no shell, tmux command parser, or literal fallback.
    let output = tmux_cmd()
        .args(["send-keys", "-t", &session.pane_id, "--"])
        .args(
            keys.iter()
                .map(|key| tmux_key_arg(key))
                .cycle()
                .take(keys_sent),
        )
        .output()
        .context("failed to send named keys")?;
    if !output.status.success() {
        return Err(tmux_failure("failed to send named keys", &output));
    }
    receipt.record(&session.name)?;
    Ok(KeySendResult {
        api_version: API_VERSION,
        session: session.name.clone(),
        pane_id: session.pane_id.clone(),
        keys: keys.to_vec(),
        repeat,
        keys_sent,
        submission_confirmed: false,
    })
}

/// Use a private named tmux buffer so simultaneous sends never overwrite the
/// user's clipboard or each other's payload. The guard also cleans up failures.
fn paste_input(pane_id: &str, input: &str) -> Result<()> {
    struct Buffer(String);
    impl Drop for Buffer {
        fn drop(&mut self) {
            let _ = tmux_cmd().args(["delete-buffer", "-b", &self.0]).output();
        }
    }
    let buffer = Buffer(format!(
        "pmux-send-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut child = tmux_cmd()
        .args(["load-buffer", "-b", &buffer.0, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to load input into tmux paste buffer")?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("tmux paste stdin unavailable"))?
        .write_all(input.as_bytes());
    let output = child
        .wait_with_output()
        .context("failed to load input into tmux paste buffer")?;
    if !output.status.success() {
        return Err(tmux_failure(
            "failed to load input into tmux paste buffer",
            &output,
        ));
    }
    write_result.context("failed to write tmux paste input")?;
    let output = tmux_cmd()
        .args(["paste-buffer", "-p", "-r", "-b", &buffer.0, "-t", pane_id])
        .output()
        .context("failed to paste input into pmux process")?;
    if !output.status.success() {
        return Err(tmux_failure(
            "failed to paste input into pmux process",
            &output,
        ));
    }
    Ok(())
}

/// Wait for a live process to exit or for an explicit ai-attn agent state.
///
/// An absent timeout waits indefinitely. The function returns an outcome for
/// timeout and early session exit rather than guessing based on pane output.
pub fn wait(
    target: &str,
    conditions: Vec<WaitCondition>,
    ignore_waiting: bool,
    startup_timeout: Duration,
    timeout: Option<Duration>,
) -> Result<WaitResult> {
    wait_impl(
        target,
        conditions,
        ignore_waiting,
        startup_timeout,
        timeout,
        None,
        None,
    )
}

fn wait_impl(
    target: &str,
    conditions: Vec<WaitCondition>,
    ignore_waiting: bool,
    startup_timeout: Duration,
    timeout: Option<Duration>,
    previous_signal: Option<crate::api::AgentInfo>,
    initial_process: Option<&ProcessInfo>,
) -> Result<WaitResult> {
    let mut watch = match initial_process {
        Some(process) => Watch::new(&process.name, &process.pane_id),
        None => {
            let sessions = list_sessions()?;
            let resolved = resolve_attach_target(&sessions, target)?;
            Watch::new(&resolved.name, &resolved.pane_id)
        }
    };
    // Snapshots never carry the conversation id `inspect` adds, so drop it
    // before comparing, or an unchanged signal would look new.
    watch.previous_signal = previous_signal.map(|mut agent| {
        agent.session_id = None;
        agent
    });
    let spec = WaitSpec::new(conditions, ignore_waiting, startup_timeout)?;
    let started = Instant::now();
    loop {
        let snapshot = state()?;
        let timed_out = timeout.is_some_and(|limit| started.elapsed() >= limit);
        if let Some(result) = watch.observe(&spec, &snapshot, timed_out) {
            return Ok(result);
        }
        thread::sleep(poll_interval(timeout, started));
    }
}

fn poll_interval(timeout: Option<Duration>, started: Instant) -> Duration {
    timeout
        .and_then(|limit| limit.checked_sub(started.elapsed()))
        .map(|remaining| remaining.min(Duration::from_millis(200)))
        .unwrap_or_else(|| Duration::from_millis(200))
}

/// What a wait is for, shared by every process it watches.
struct WaitSpec {
    conditions: Vec<WaitCondition>,
    label: String,
    ignore_waiting: bool,
    startup_timeout: Duration,
}

impl WaitSpec {
    fn new(
        conditions: Vec<WaitCondition>,
        ignore_waiting: bool,
        startup_timeout: Duration,
    ) -> Result<Self> {
        if conditions.is_empty() {
            bail!("at least one wait condition is required");
        }
        let label = conditions
            .iter()
            .map(WaitCondition::label)
            .collect::<Vec<_>>()
            .join(",");
        Ok(Self {
            conditions,
            label,
            ignore_waiting,
            startup_timeout,
        })
    }
}

/// One process being waited on. It follows the pane, not the name: a tmux
/// pane id is stable for the pane's lifetime, so a rename mid-wait is not
/// mistaken for the process exiting.
struct Watch {
    name: String,
    pane_id: String,
    /// A signal from before the wait began that must change before it counts.
    previous_signal: Option<crate::api::AgentInfo>,
}

impl Watch {
    fn new(name: &str, pane_id: &str) -> Self {
        Self {
            name: name.to_string(),
            pane_id: pane_id.to_string(),
            previous_signal: None,
        }
    }

    /// This process's outcome in `snapshot`, or `None` to keep waiting.
    fn observe(
        &mut self,
        spec: &WaitSpec,
        snapshot: &StateSnapshot,
        timed_out: bool,
    ) -> Option<WaitResult> {
        let process = snapshot
            .sessions
            .iter()
            .find(|session| session.pane_id == self.pane_id)
            .cloned();
        if let Some(process) = &process {
            self.name.clone_from(&process.name);
        }

        let unchanged_signal = self.previous_signal.is_some()
            && process.as_ref().and_then(|p| p.agent.as_ref()) == self.previous_signal.as_ref();
        if !unchanged_signal {
            self.previous_signal = None;
        }
        let observation = if unchanged_signal {
            None
        } else {
            wait_observation(&spec.conditions, process.as_ref(), spec.ignore_waiting)
        };
        let startup_expired = spec
            .conditions
            .iter()
            .any(|condition| matches!(condition, WaitCondition::AgentState(_)))
            && process
                .as_ref()
                .and_then(|p| p.startup.as_ref())
                // Configured ready text on screen confirms an idle agent is at
                // its prompt even though it has sent no signal yet.
                .filter(|startup| !startup.is_ready())
                .is_some_and(|startup| {
                    Duration::from_secs(
                        snapshot
                            .generated_at
                            .saturating_sub(startup.started_at)
                            .max(0) as u64,
                    ) >= spec.startup_timeout
                });
        // Keep precedence identical for wait and ask: match/block, exit,
        // unconfirmed startup, then timeout.
        let (outcome, matched_condition) = observation.or_else(|| {
            if process.is_none() {
                Some(("exited", None))
            } else if startup_expired {
                Some(("not_ready", None))
            } else if timed_out {
                Some(("timed_out", None))
            } else {
                None
            }
        })?;
        Some(self.result(
            spec,
            snapshot.generated_at,
            outcome,
            matched_condition,
            process,
        ))
    }

    fn result(
        &self,
        spec: &WaitSpec,
        observed_at: i64,
        outcome: &str,
        matched_condition: Option<String>,
        process: Option<ProcessInfo>,
    ) -> WaitResult {
        WaitResult {
            api_version: API_VERSION,
            capture: None,
            capture_error: None,
            session: self.name.clone(),
            condition: spec.label.clone(),
            outcome: outcome.to_string(),
            matched_condition,
            observed_at,
            process,
        }
    }
}

/// How `pmux wait` treats several processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// Return once every process matched, or as soon as one needs attention.
    All,
    /// Return as soon as any process matches or needs attention.
    Any,
}

/// The result of waiting on several processes at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MultiWaitResult {
    pub api_version: u32,
    /// `all` or `any`.
    pub mode: &'static str,
    pub condition: String,
    /// `matched` when the mode was satisfied, else the outcome of
    /// `decided_by`.
    pub outcome: String,
    /// The process whose outcome ended the wait; absent when every process
    /// matched under `all`.
    pub decided_by: Option<String>,
    pub observed_at: i64,
    /// One result per process, in the order named. A process the wait ended
    /// before deciding is `pending`.
    pub results: Vec<WaitResult>,
}

impl MultiWaitResult {
    /// The result that sets the exit status and next-step hint.
    pub fn deciding(&self) -> Option<&WaitResult> {
        let name = self.decided_by.as_deref()?;
        self.results.iter().find(|result| result.session == name)
    }
}

/// Wait on several processes. Under `All` a blocked or exited process (or
/// the timeout) ends the wait at once, since the whole batch cannot finish
/// without attention; under `Any` the first decided process does.
///
/// An agent with no signal yet is not a reason to stop: by the time a batch
/// is waited on its prompts were usually confirmed, and an agent whose hooks
/// are silent would otherwise end the wait for nothing. It is watched until
/// the timeout and reported `not_ready` only if it never signalled.
pub fn wait_many(
    targets: &[String],
    mode: WaitMode,
    conditions: Vec<WaitCondition>,
    ignore_waiting: bool,
    timeout: Option<Duration>,
) -> Result<MultiWaitResult> {
    let spec = WaitSpec::new(conditions, ignore_waiting, Duration::MAX)?;
    let sessions = list_sessions()?;
    let mut watches: Vec<Watch> = Vec::with_capacity(targets.len());
    for target in targets {
        let resolved = resolve_attach_target(&sessions, target)?;
        if !watches
            .iter()
            .any(|watch| watch.pane_id == resolved.pane_id)
        {
            watches.push(Watch::new(&resolved.name, &resolved.pane_id));
        }
    }
    let mut results: Vec<Option<WaitResult>> = vec![None; watches.len()];
    let started = Instant::now();
    loop {
        let snapshot = state()?;
        let timed_out = timeout.is_some_and(|limit| started.elapsed() >= limit);
        let mut decided_by = None;
        for (watch, slot) in watches.iter_mut().zip(results.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            let Some(mut result) = watch.observe(&spec, &snapshot, timed_out) else {
                continue;
            };
            result.never_signalled_as_not_ready();
            let ends_wait = mode == WaitMode::Any || result.outcome != "matched";
            if ends_wait && decided_by.is_none() {
                decided_by = Some(result.session.clone());
            }
            *slot = Some(result);
        }
        let all_matched = results.iter().all(Option::is_some);
        if decided_by.is_some() || all_matched {
            let outcome = match &decided_by {
                Some(name) => results
                    .iter()
                    .flatten()
                    .find(|result| &result.session == name)
                    .map(|result| result.outcome.clone())
                    .unwrap_or_default(),
                None => "matched".to_string(),
            };
            let results = watches
                .iter()
                .zip(results)
                .map(|(watch, result)| {
                    result.unwrap_or_else(|| {
                        let process = snapshot
                            .sessions
                            .iter()
                            .find(|session| session.pane_id == watch.pane_id)
                            .cloned();
                        watch.result(&spec, snapshot.generated_at, "pending", None, process)
                    })
                })
                .collect();
            return Ok(MultiWaitResult {
                api_version: API_VERSION,
                mode: match mode {
                    WaitMode::All => "all",
                    WaitMode::Any => "any",
                },
                condition: spec.label.clone(),
                outcome,
                decided_by,
                observed_at: snapshot.generated_at,
                results,
            });
        }
        thread::sleep(poll_interval(timeout, started));
    }
}

/// Explicit matches take precedence over the implicit input-needed stop.
fn wait_observation(
    conditions: &[WaitCondition],
    process: Option<&ProcessInfo>,
    ignore_waiting: bool,
) -> Option<(&'static str, Option<String>)> {
    let state = process
        .and_then(|p| p.agent.as_ref())
        .and_then(|a| a.state.as_deref())
        .filter(|state| {
            *state != "waiting" || !process.is_some_and(crate::input::waiting_was_answered)
        });
    for condition in conditions {
        let matched = match condition {
            WaitCondition::Exited => process.is_none(),
            WaitCondition::AgentState(expected) => state == Some(expected.as_str()),
        };
        if matched {
            return Some(("matched", Some(condition.label())));
        }
    }
    if !ignore_waiting
        && state == Some("waiting")
        && conditions
            .iter()
            .any(|c| matches!(c, WaitCondition::AgentState(_)))
    {
        return Some(("blocked", None));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_limits_physical_rows_without_dropping_prompt_or_blank_rows() {
        assert_eq!(
            capture_text("history\nbox top\nAllow?\nbox bottom\n\n", Some(4)),
            "box top\nAllow?\nbox bottom\n\n"
        );
        assert_eq!(capture_text("\x1b[31mAllow?\x1b[0m\n", None), "Allow?\n");
    }

    #[test]
    fn capture_failure_preserves_blocked_outcome() {
        let home = tempfile::tempdir().unwrap();
        let mut env = crate::EnvGuard::new();
        env.set("HOME", home.path());
        env.set(
            "PMUX_SOCKET",
            format!("pmux-capture-missing-{}", std::process::id()),
        );
        env.set("TMUX_TMPDIR", home.path());
        let mut result = WaitResult {
            api_version: API_VERSION,
            session: "missing".into(),
            condition: "agent:done".into(),
            outcome: "blocked".into(),
            matched_condition: None,
            observed_at: 0,
            process: None,
            capture: None,
            capture_error: None,
        };
        result.capture_on_block(Some(80));
        assert_eq!(result.outcome, "blocked");
        assert!(result.capture.is_none());
        assert!(result.capture_error.is_some());
    }

    #[test]
    fn semicolon_keys_are_escaped_for_tmux() {
        assert_eq!(tmux_key_arg(";"), "\\;");
        assert_eq!(tmux_key_arg("M-;"), "M-\\;");
        assert_eq!(tmux_key_arg("t"), "t");
        assert_eq!(tmux_key_arg("Enter"), "Enter");
    }

    #[test]
    fn named_keys_reject_unknown_names_and_malformed_modifiers() {
        for key in ["Escape", "C-u", "BSpace", "M-Left", "C-S-Left", "F12", "y"] {
            assert!(valid_key(key), "{key}");
        }
        for key in [
            "Escap",
            "C-",
            "C-C-u",
            "F13",
            "F01",
            "",
            "hello",
            "\x1b",
            "Enter; kill-server",
        ] {
            assert!(!valid_key(key), "{key}");
        }
    }

    #[test]
    fn wait_condition_labels_are_stable_for_json_consumers() {
        assert_eq!(WaitCondition::Exited.label(), "exited");
        assert_eq!(
            WaitCondition::AgentState("waiting".to_string()).label(),
            "agent:waiting"
        );
    }
}
