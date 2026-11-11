//! Durable, socket-scoped batches. The store lock serializes UI edits and delivery;
//! the worker lock elects one scheduler. A persisted Sending record is never retried.
pub use crate::queue_time::{
    DAY, HOUR, MINUTE, MONTH, SLOTS, YEAR, fields_epoch, format_time, format_weekday, parse_time,
    picker_fields, picker_start, shift_fields,
};
use crate::{
    commands::resolve_attach_target,
    tmux::{list_sessions, pmux_socket, tmux_cmd},
    util::now_epoch,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub session: String,
    pub pane: String,
    token: String,
    pid: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Draft,
    Scheduled,
    Paused,
    Sending,
    Sent,
    Held,
    Uncertain,
    Cancelled,
}
impl Status {
    pub fn editable(self) -> bool {
        matches!(
            self,
            Self::Draft | Self::Scheduled | Self::Paused | Self::Held
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Scheduled => "scheduled",
            Self::Paused => "paused",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Held => "held",
            Self::Uncertain => "uncertain",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    pub target: Target,
    pub messages: Vec<String>,
    pub after: Option<i64>,
    pub status: Status,
    pub detail: Option<String>,
    pub sent_at: Option<i64>,
}
#[derive(Default, Serialize, Deserialize)]
struct Store {
    batches: Vec<Batch>,
}

fn directory() -> Result<PathBuf> {
    let socket = pmux_socket()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let dir = PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
        .join(".local/state/pmux/queues")
        .join(socket);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}
fn lock(name: &str, nonblocking: bool) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory()?.join(name))?;
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    // SAFETY: file owns a live fd for the entire lock lifetime. Closing releases flock.
    if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
        let error = std::io::Error::last_os_error();
        if nonblocking && error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(file))
}
fn read_store() -> Result<Store> {
    match fs::read(directory()?.join("batches.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("cannot read queue store; existing data was preserved"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.into()),
    }
}
fn save(store: &Store) -> Result<()> {
    let dir = directory()?;
    let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer_pretty(&mut temp, store)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join("batches.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
fn tmux_output(args: &[&str]) -> Result<String> {
    let output = tmux_cmd().args(args).output()?;
    if !output.status.success() {
        bail!("tmux: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}
fn inspect_pane(pane: &str) -> Result<(String, String, String)> {
    let output = tmux_output(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{@pmux_queue_identity}\t#{pane_pid}\t#{pane_dead}",
    ])?;
    let fields: Vec<_> = output.split('\t').collect();
    if fields.len() != 3 {
        bail!("cannot identify target process");
    }
    Ok((fields[0].into(), fields[1].into(), fields[2].into()))
}
fn target_valid(target: &Target) -> bool {
    inspect_pane(&target.pane)
        .is_ok_and(|(token, pid, dead)| token == target.token && pid == target.pid && dead == "0")
}
/// Resolve once, including a random pane identity that cannot survive a server restart.
pub fn target(selector: &str) -> Result<Target> {
    let _lock = lock("store.lock", false)?;
    let sessions = list_sessions()?;
    let session = if selector.starts_with('%') {
        sessions
            .iter()
            .find(|s| s.pane_id == selector)
            .ok_or_else(|| anyhow!("process pane no longer exists"))?
    } else {
        resolve_attach_target(&sessions, selector)?
    };
    let (mut token, pid, dead) = inspect_pane(&session.pane_id)?;
    if dead != "0" {
        bail!("target process has exited");
    }
    if token.is_empty() {
        token = format!("{:032x}", rand::random::<u128>());
        tmux_output(&[
            "set-option",
            "-p",
            "-t",
            &session.pane_id,
            "@pmux_queue_identity",
            &token,
        ])?;
    }
    Ok(Target {
        session: session.name.clone(),
        pane: session.pane_id.clone(),
        token,
        pid,
    })
}
fn same_target(a: &Target, b: &Target) -> bool {
    a.pane == b.pane && a.token == b.token && a.pid == b.pid
}
pub fn list(target: Option<&Target>) -> Result<Vec<Batch>> {
    let _lock = lock("store.lock", false)?;
    Ok(read_store()?
        .batches
        .into_iter()
        .filter(|b| target.is_none_or(|t| same_target(t, &b.target)))
        .collect())
}

pub enum Edit {
    Add(String),
    Replace(usize, String),
    Remove(usize),
    Move(usize, usize),
    Schedule(i64),
    Pause,
    Resume,
    Cancel,
}
fn validate_message(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("message cannot be empty");
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        bail!("messages may contain newlines and tabs, but no other control characters");
    }
    if text.len() > 1_000_000 {
        bail!("message exceeds 1 MB");
    }
    Ok(())
}
fn apply(batch: &mut Batch, edit: Edit) -> Result<()> {
    let n = batch.messages.len();
    match edit {
        Edit::Add(text) => {
            validate_message(&text)?;
            batch.messages.push(text);
        }
        Edit::Replace(i, text) => {
            validate_message(&text)?;
            *batch
                .messages
                .get_mut(i)
                .ok_or_else(|| anyhow!("message index out of range"))? = text;
        }
        Edit::Remove(i) => {
            if i >= n {
                bail!("message index out of range");
            }
            batch.messages.remove(i);
        }
        Edit::Move(i, to) => {
            if i >= n || to >= n {
                bail!("message index out of range");
            }
            let text = batch.messages.remove(i);
            batch.messages.insert(to, text);
        }
        Edit::Schedule(at) => {
            if at <= now_epoch() {
                bail!("schedule must be in the future");
            }
            if n == 0 {
                bail!("add a message before scheduling");
            }
            batch.after = Some(at);
            batch.status = Status::Scheduled;
            batch.detail = None;
        }
        Edit::Pause => {
            batch.status = Status::Paused;
        }
        Edit::Resume => {
            if n == 0 {
                bail!("add a message before resuming");
            }
            if batch.after.is_none_or(|at| at <= now_epoch()) {
                bail!("choose a new future time before resuming");
            }
            batch.status = Status::Scheduled;
            batch.detail = None;
        }
        Edit::Cancel => {
            batch.status = Status::Cancelled;
        }
    }
    if batch.messages.is_empty() && batch.status != Status::Cancelled {
        batch.status = Status::Draft;
        batch.after = None;
    }
    Ok(())
}
/// Optimistic edit guard for a popup that can remain open while another client edits.
#[derive(Clone, Copy)]
pub enum Revision<'a> {
    Any,
    Empty,
    Batch(&'a str, u64),
}

/// A revision guard prevents an editor open during delivery from changing a subsequent batch.
pub fn edit(target: &Target, expected: Revision<'_>, change: Edit) -> Result<Batch> {
    edit_many(target, expected, vec![change])
}
pub fn edit_many(target: &Target, expected: Revision<'_>, changes: Vec<Edit>) -> Result<Batch> {
    let result = {
        let _lock = lock("store.lock", false)?;
        if !target_valid(target) {
            bail!("target process changed or exited; queued messages were preserved");
        }
        let mut store = read_store()?;
        let index = store
            .batches
            .iter()
            .rposition(|b| same_target(&b.target, target) && b.status.editable());
        let matches = match expected {
            Revision::Any => true,
            Revision::Empty => index.is_none(),
            Revision::Batch(id, revision) => index.is_some_and(|i| {
                store.batches[i].id == id && store.batches[i].revision == revision
            }),
        };
        if !matches {
            bail!("batch changed while editing; reopen the queue");
        }
        let index = match index {
            Some(i) => i,
            None => {
                if !matches!(changes.first(), Some(Edit::Add(_))) {
                    bail!("no editable batch; add a message first");
                }
                store.batches.push(Batch {
                    revision: 0,
                    id: format!("{:032x}", rand::random::<u128>()),
                    target: target.clone(),
                    messages: vec![],
                    after: None,
                    status: Status::Draft,
                    detail: None,
                    sent_at: None,
                });
                store.batches.len() - 1
            }
        };
        let batch = &mut store.batches[index];
        batch.target.session.clone_from(&target.session);
        for change in changes {
            apply(batch, change)?;
        }
        batch.revision += 1;
        let result = batch.clone();
        save(&store)?;
        update_badge(&result);
        result
    };
    ensure_worker().context("queue saved, but scheduler could not start; run pmux queue worker")?;
    Ok(result)
}
/// Cancel history by ID, even when the original process no longer exists.
pub fn cancel(id: &str) -> Result<()> {
    let _lock = lock("store.lock", false)?;
    let mut store = read_store()?;
    let batch = store
        .batches
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| anyhow!("unknown batch ID"))?;
    if !batch.status.editable() {
        bail!("batch is already {}", batch.status.label());
    }
    batch.status = Status::Cancelled;
    let batch = batch.clone();
    save(&store)?;
    update_badge(&batch);
    Ok(())
}

pub fn combined(messages: &[String]) -> String {
    if messages.len() == 1 {
        return messages[0].clone();
    }
    let n = messages.len();
    let mut text = format!(
        "The user queued the following {n} instructions for this time.\nTreat each numbered section as a separate user instruction, in the order listed."
    );
    for (i, message) in messages.iter().enumerate() {
        text.push_str(&format!(
            "\n\n--- Instruction {} of {n} ---\n{message}",
            i + 1
        ));
    }
    text
}
fn deliver(batch: &Batch) -> Result<()> {
    // Use a private named tmux buffer and bracketed paste. Newlines remain part
    // of one prompt in supporting CLIs; prompt text never goes through a shell.
    let buffer = format!("pmux-queue-{}", batch.id);
    let mut child = tmux_cmd()
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let write_result = child
        .stdin
        .take()
        .context("missing buffer input")?
        .write_all(combined(&batch.messages).as_bytes());
    let output = child.wait_with_output()?;
    let result = (|| {
        write_result?;
        if !output.status.success() {
            bail!("failed to load prompt buffer");
        }
        if !target_valid(&batch.target) {
            bail!("target changed before paste");
        }
        tmux_output(&[
            "paste-buffer",
            "-p",
            "-r",
            "-b",
            &buffer,
            "-t",
            &batch.target.pane,
        ])?;
        // Let the CLI consume the paste before its submit key arrives.
        thread::sleep(Duration::from_millis(150));
        if !target_valid(&batch.target) {
            bail!("target changed before submission");
        }
        tmux_output(&["send-keys", "-t", &batch.target.pane, "Enter"])?;
        Ok(())
    })();
    let _ = tmux_output(&["delete-buffer", "-b", &buffer]);
    result
}
fn update_badge(batch: &Batch) {
    if !target_valid(&batch.target) {
        return;
    }
    let badge = if batch.status.editable() {
        format!(
            "Q:{} {}{}",
            batch.messages.len(),
            batch.status.label(),
            batch
                .after
                .map(|at| format!(
                    " {}",
                    format_time(at).split_whitespace().nth(1).unwrap_or("")
                ))
                .unwrap_or_default()
        )
    } else if matches!(batch.status, Status::Uncertain | Status::Sending) {
        format!("Q:{}", batch.status.label())
    } else {
        String::new()
    };
    let _ = tmux_output(&[
        "set-option",
        "-p",
        "-t",
        &batch.target.pane,
        "@pmux_queue_status",
        &badge,
    ]);
}
/// Sent and cancelled batches kept as history; older ones are pruned.
const HISTORY_LIMIT: usize = 200;

fn is_history(batch: &Batch) -> bool {
    matches!(batch.status, Status::Sent | Status::Cancelled)
}

// Drop the oldest history beyond HISTORY_LIMIT. Held, uncertain, and
// editable batches are never pruned: they may hold messages to recover.
fn prune_history(store: &mut Store) -> bool {
    let mut excess = store
        .batches
        .iter()
        .filter(|batch| is_history(batch))
        .count()
        .saturating_sub(HISTORY_LIMIT);
    if excess == 0 {
        return false;
    }
    store.batches.retain(|batch| {
        if excess > 0 && is_history(batch) {
            excess -= 1;
            false
        } else {
            true
        }
    });
    true
}

// Whether the scheduler still has anything to deliver.
fn has_pending(store: &Store) -> bool {
    store
        .batches
        .iter()
        .any(|batch| matches!(batch.status, Status::Scheduled | Status::Sending))
}

/// Deliver due batches. Returns whether anything remains scheduled.
fn tick() -> Result<bool> {
    let _lock = lock("store.lock", false)?;
    let mut store = read_store()?;
    for i in 0..store.batches.len() {
        if store.batches[i].status == Status::Sending {
            store.batches[i].status = Status::Uncertain;
            store.batches[i].detail = Some("Scheduler stopped during delivery. Inspect the terminal before creating another batch; automatic retry is disabled.".into());
            save(&store)?;
            update_badge(&store.batches[i]);
        }
        if store.batches[i].status != Status::Scheduled
            || store.batches[i].after.is_none_or(|at| at > now_epoch())
        {
            continue;
        }
        if !target_valid(&store.batches[i].target) {
            store.batches[i].status = Status::Held;
            store.batches[i].detail = Some("Original process is unavailable or was replaced. Copy messages into the intended process's queue and choose a new time.".into());
            save(&store)?;
            update_badge(&store.batches[i]);
            continue;
        }
        store.batches[i].status = Status::Sending;
        save(&store)?; // durable claim BEFORE any terminal input
        match deliver(&store.batches[i]) {
            Ok(()) => {
                store.batches[i].status = Status::Sent;
                store.batches[i].sent_at = Some(now_epoch());
            }
            Err(e) => {
                store.batches[i].status = Status::Uncertain;
                store.batches[i].detail = Some(format!(
                    "Delivery may be partial; inspect the terminal before retrying: {e}"
                ));
            }
        }
        save(&store)?;
        update_badge(&store.batches[i]);
    }
    if prune_history(&mut store) {
        save(&store)?;
    }
    Ok(has_pending(&store))
}
/// Detached scheduler, elected by an OS lock that also releases after a crash.
pub fn worker() -> Result<()> {
    let Some(mut worker_lock) = lock("worker.lock", true)? else {
        return Ok(());
    };
    worker_lock.set_len(0)?;
    writeln!(worker_lock, "{}", std::process::id())?;
    worker_lock.sync_all()?;
    let mut retry = Duration::from_secs(1);
    loop {
        match tick() {
            Ok(true) => retry = Duration::from_secs(1),
            Ok(false) => {
                // Every queue edit calls ensure_worker after saving. Re-check
                // and give up the election lock while holding the store lock,
                // so an edit racing this exit sees either a live worker or a
                // free lock it can claim, never a worker about to vanish.
                let _store = lock("store.lock", false)?;
                if !has_pending(&read_store()?) {
                    drop(worker_lock);
                    return Ok(());
                }
            }
            // A corrupt store or failed save must not stop the scheduler for
            // good; log it (stderr is worker.log) and retry with backoff.
            Err(error) => {
                eprintln!("pmux queue worker: {error:#}");
                thread::sleep(retry);
                retry = (retry * 2).min(Duration::from_secs(60));
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}
pub fn ensure_worker() -> Result<()> {
    let Some(probe) = lock("worker.lock", true)? else {
        return Ok(());
    };
    drop(probe);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(directory()?.join("worker.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["queue", "worker"])
        .env("PMUX_SOCKET", pmux_socket())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and the closure does no allocation.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Reconnects to an existing queue scheduler when a user attaches after a worker crash.
pub(crate) fn ensure_existing_worker() -> Result<()> {
    if directory()?.join("batches.json").exists() {
        ensure_worker()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch() -> Batch {
        Batch {
            id: "test".into(),
            revision: 0,
            target: Target {
                session: "agent".into(),
                pane: "%1".into(),
                token: "original".into(),
                pid: "123".into(),
            },
            messages: vec![],
            after: None,
            status: Status::Draft,
            detail: None,
            sent_at: None,
        }
    }
    fn with_status(id: &str, status: Status) -> Batch {
        Batch {
            id: id.into(),
            status,
            ..batch()
        }
    }
    #[test]
    fn history_is_pruned_oldest_first_and_recoverable_batches_are_kept() {
        let mut batches = vec![with_status("held", Status::Held)];
        batches
            .extend((0..HISTORY_LIMIT + 3).map(|i| with_status(&format!("sent{i}"), Status::Sent)));
        batches.push(with_status("uncertain", Status::Uncertain));
        let mut store = Store { batches };
        assert!(prune_history(&mut store));
        let ids: Vec<&str> = store.batches.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids.len(), HISTORY_LIMIT + 2);
        assert_eq!(ids[0], "held");
        assert_eq!(ids[1], "sent3");
        assert_eq!(ids.last(), Some(&"uncertain"));
        assert!(!prune_history(&mut store));
    }
    #[test]
    fn only_scheduled_or_sending_batches_keep_the_worker_alive() {
        let idle = Store {
            batches: vec![
                with_status("a", Status::Sent),
                with_status("b", Status::Held),
                with_status("c", Status::Paused),
            ],
        };
        assert!(!has_pending(&idle));
        let busy = Store {
            batches: vec![with_status("a", Status::Scheduled)],
        };
        assert!(has_pending(&busy));
    }
    #[test]
    fn batching_preserves_verbatim_text_and_order() {
        let one = "  Markdown\n\n```sh\n$HOME `echo hi`\n```\n".to_string();
        assert_eq!(combined(std::slice::from_ref(&one)), one);
        let prompt = combined(&[one.clone(), "次の命令".into()]);
        assert!(prompt.contains(&format!("--- Instruction 1 of 2 ---\n{one}")));
        assert!(prompt.ends_with("--- Instruction 2 of 2 ---\n次の命令"));
    }
    #[test]
    fn batch_lifecycle_and_editing() {
        let mut b = batch();
        assert!(apply(&mut b, Edit::Schedule(now_epoch() + 60)).is_err());
        apply(&mut b, Edit::Add("first".into())).unwrap();
        apply(&mut b, Edit::Add("second".into())).unwrap();
        apply(&mut b, Edit::Move(1, 0)).unwrap();
        apply(&mut b, Edit::Replace(1, "updated".into())).unwrap();
        assert_eq!(b.messages, ["second", "updated"]);
        apply(&mut b, Edit::Schedule(now_epoch() + 60)).unwrap();
        apply(&mut b, Edit::Pause).unwrap();
        assert_eq!(b.status, Status::Paused);
        apply(&mut b, Edit::Resume).unwrap();
        assert_eq!(b.status, Status::Scheduled);
        b.after = Some(now_epoch() - 1);
        assert!(apply(&mut b, Edit::Resume).is_err());
        apply(&mut b, Edit::Remove(1)).unwrap();
        apply(&mut b, Edit::Remove(0)).unwrap();
        assert_eq!(b.status, Status::Draft);
        assert_eq!(b.after, None);
        for text in ["", "  ", "escape\x1b[31m", "nul\0", "\x03"] {
            assert!(apply(&mut b, Edit::Add(text.into())).is_err());
        }
    }
    #[test]
    fn concurrent_transactions_preserve_messages_and_private_atomic_store() {
        use std::os::unix::fs::PermissionsExt;
        let mut env = crate::EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        env.set("HOME", temp.path());
        save(&Store {
            batches: vec![batch()],
        })
        .unwrap();
        let threads: Vec<_> = (0..12)
            .map(|i| {
                thread::spawn(move || {
                    let _lock = lock("store.lock", false).unwrap();
                    let mut store = read_store().unwrap();
                    store.batches[0].messages.push(i.to_string());
                    save(&store).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(read_store().unwrap().batches[0].messages.len(), 12);
        let dir = directory().unwrap();
        assert_eq!(
            fs::metadata(dir.join("batches.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(dir.join("batches.json"), "broken data").unwrap();
        assert!(read_store().is_err());
        assert_eq!(
            fs::read_to_string(dir.join("batches.json")).unwrap(),
            "broken data"
        );
    }
}
