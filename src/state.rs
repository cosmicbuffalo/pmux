//! Persistent state management for pmux.
//!
//! State files live in `~/.local/state/pmux/` and track launch commands,
//! working directories, host pane mappings, and attachment history. A
//! non-default `PMUX_SOCKET` keeps its own state under `sockets/<hex>/`.
//!
//! **Concurrency:** many pmux processes touch this state at once (CLI
//! commands, tmux hooks, the relay daemon, sidebar renderers). Every write
//! replaces its file atomically ([`write_state_file`]), so readers see either
//! the old or the new contents and never a truncated file. Read-modify-write
//! updates and check-then-act sequences run under [`with_state_lock`], an
//! exclusive per-backend `flock`.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::SessionInfo;

// Return `~/.local/state/pmux`, for per-user preferences that are not tied
// to one backend's sessions.
fn pmux_shared_state_dir() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("pmux"))
}

// Return the pmux state directory for the active backend socket.
//
// The default `pmux` socket uses `~/.local/state/pmux` directly. Any other
// socket (`PMUX_SOCKET=pmux-test`) gets `~/.local/state/pmux/sockets/<hex>`:
// session-keyed state, stale-metadata cleanup, the restore snapshot, and the
// relay PID must never act on another backend's sessions. The hex encoding
// matches the queue store and keeps arbitrary socket names path-safe.
fn pmux_state_dir() -> Result<PathBuf> {
    let base = pmux_shared_state_dir()?;
    let socket = crate::tmux::pmux_socket();
    if socket == crate::tmux::PMUX_SOCKET {
        return Ok(base);
    }
    let encoded: String = socket.bytes().map(|b| format!("{b:02x}")).collect();
    Ok(base.join("sockets").join(encoded))
}

/// Return the ai-attn state directory, respecting `AI_ATTN_STATE_DIR`.
pub(crate) fn ai_attn_state_dir() -> PathBuf {
    if let Ok(dir) = env::var("AI_ATTN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("ai-attn")
}

// Create a state directory and its parents if they don't exist, mode 0700.
fn ensure_state_dir(dir: &std::path::Path) -> Result<()> {
    crate::util::create_private_dir_all(dir).context("failed to create state directory")
}

// Replace `path` atomically: write a temp file in the same directory, then
// rename it over the target. Temp files are dot-prefixed (`.tmpXXXXXX`), which
// stale-metadata cleanup skips, and session names never contain dots.
pub(crate) fn write_state_file(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    ensure_state_dir(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to create a temp file in {}", dir.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Run `f` while holding an exclusive lock on this backend's state.
///
/// Use it for read-modify-write updates and check-then-act sequences that
/// would otherwise lose updates or race across pmux processes. The lock is a
/// `flock` on a dedicated file, released when `f` returns (or the process
/// dies). It is not reentrant: never call this from inside `f`.
pub(crate) fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let guard = lock_state()?;
    let result = f();
    drop(guard);
    result
}

/// Holds the state lock until dropped. See [`with_state_lock`].
pub(crate) struct StateLockGuard {
    // Closing the fd releases the flock.
    _file: File,
}

/// Take the state lock for a scope that is awkward to express as a closure.
/// Same rules as [`with_state_lock`]: not reentrant.
pub(crate) fn lock_state() -> Result<StateLockGuard> {
    let dir = ensure_pmux_state_dir()?;
    let path = dir.join(".state.lock");
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: `file` owns a live fd for the whole lock lifetime; dropping it
    // closes the fd, which releases the flock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", path.display()));
    }
    Ok(StateLockGuard { _file: file })
}

// Return the directory for per-session launch command files.
fn pmux_launch_cmds_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("launch_cmds"))
}

// Return the directory for per-session launch CWD files.
fn pmux_launch_cwds_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("launch_cwds"))
}

// Return the directory for per-session parent (spawned-by) records.
pub(crate) fn pmux_parents_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("parents"))
}

// Return the directory for per-session host pane mapping files.
fn pmux_host_panes_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("host_panes"))
}

/// A stamp `pmux status-labels` rewrites whenever it changes a process's
/// label, so every sidebar (in any outer tmux) re-reads agent titles at once
/// instead of on its next periodic history refresh.
pub(crate) fn pmux_labels_changed_path() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("labels_changed"))
}

/// Rewrite the labels-changed stamp with a fresh value. Best-effort: a
/// sidebar that misses it still refreshes periodically.
pub(crate) fn touch_labels_changed() {
    let Ok(path) = pmux_labels_changed_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && crate::util::create_private_dir_all(parent).is_err()
    {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let _ = crate::util::write_private_file_atomic(&path, format!("{stamp}\n").as_bytes());
}

/// The labels-changed stamp's current value, if it has ever been written.
pub(crate) fn read_labels_changed() -> Option<String> {
    fs::read_to_string(pmux_labels_changed_path().ok()?).ok()
}

// Return the directory for durable per-session agent resume identities.
fn pmux_agent_sessions_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("agent_sessions"))
}

// Return the path to the cached agent session history index.
pub(crate) fn pmux_history_index_file() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("history-index.json"))
}

// Return the path to the relay daemon PID file.
pub(crate) fn pmux_relay_pid_file() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("ai_attn_relay.pid"))
}

// Return the path to the cross-restart session snapshot file.
pub(crate) fn restore_snapshot_path() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("restore.json"))
}

// Ensure the pmux state directory exists, returning its path.
pub(crate) fn ensure_pmux_state_dir() -> Result<PathBuf> {
    let dir = pmux_state_dir()?;
    ensure_state_dir(&dir)?;
    Ok(dir)
}

/// Return the directory for short-lived sidebar context-menu requests.
///
/// It is shared across backends: a menu item runs from the outer tmux
/// server's `run-shell`, whose environment need not carry the sidebar's
/// `PMUX_SOCKET`.
pub(crate) fn sidebar_menu_dir() -> Result<PathBuf> {
    Ok(pmux_shared_state_dir()?.join("sidebar-menus"))
}

// Return the directory for per-pane last-process tracking files.
fn pmux_pane_last_processes_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("pane_last_processes"))
}

// Return the path to the attach history file.
fn pmux_attach_history_file() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("attach_history"))
}

// Return the file containing the user's most recently chosen sidebar width.
fn pmux_sidebar_width_file() -> Result<PathBuf> {
    Ok(pmux_shared_state_dir()?.join("sidebar_width"))
}

const MAX_ATTACH_HISTORY: usize = 16;

/// Read the remembered outer-tmux sidebar width, if one has been set.
pub(crate) fn read_sidebar_width() -> Result<Option<u16>> {
    let Some(raw) = read_optional_trimmed(&pmux_sidebar_width_file()?)? else {
        return Ok(None);
    };
    match raw.parse::<u16>() {
        Ok(width) if width > 0 => Ok(Some(width)),
        _ => Ok(None),
    }
}

/// Persist a manually resized outer-tmux sidebar width.
pub(crate) fn write_sidebar_width(width: u16) -> Result<()> {
    if width == 0 {
        anyhow::bail!("sidebar width must be greater than zero");
    }
    write_state_file(&pmux_sidebar_width_file()?, &format!("{width}\n"))
        .context("failed to save sidebar width")
}

// Persist the launch command for a session to disk. Called during spawn.
pub(crate) fn write_session_launch_command(session_name: &str, command: &str) -> Result<()> {
    write_state_file(
        &pmux_launch_cmds_dir()?.join(session_name),
        &format!("{command}\n"),
    )
}

/// Read a file, returning None if it doesn't exist or is empty.
fn read_optional_trimmed(path: &std::path::Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(data) => {
            let trimmed = data.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {}", path.display(), e)),
    }
}

// Read the persisted launch command for a session, if it exists.
pub(crate) fn read_session_launch_command(session_name: &str) -> Result<Option<String>> {
    read_optional_trimmed(&pmux_launch_cmds_dir()?.join(session_name))
}

// Persist the launch working directory for a session to disk. Called during spawn.
pub(crate) fn write_session_launch_cwd(session_name: &str, cwd: &str) -> Result<()> {
    write_state_file(
        &pmux_launch_cwds_dir()?.join(session_name),
        &format!("{cwd}\n"),
    )
}

// Read the persisted launch CWD for a session, if it exists.
pub(crate) fn read_session_launch_cwd(session_name: &str) -> Result<Option<String>> {
    read_optional_trimmed(&pmux_launch_cwds_dir()?.join(session_name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAgentIdentity {
    pub agent: String,
    pub session_id: String,
    pub created_epoch: i64,
}

pub(crate) fn pmux_last_inputs_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("last_inputs"))
}

pub(crate) fn pmux_agent_starts_dir() -> Result<PathBuf> {
    Ok(pmux_state_dir()?.join("agent_starts"))
}

/// Persist an agent's native resume id as soon as pmux observes it. Unlike the
/// ai-attn signal record, this metadata lives until the pmux session is killed.
pub(crate) fn write_session_agent_identity(
    session_name: &str,
    agent: &str,
    session_id: &str,
    created_epoch: i64,
) -> Result<()> {
    write_state_file(
        &pmux_agent_sessions_dir()?.join(session_name),
        &format!("{agent}\t{session_id}\t{created_epoch}\n"),
    )
}

/// Read the agent identity recorded for the session that currently runs
/// under `session_name`. An identity left behind by an earlier session with
/// the same name (after a crash, reboot or `restart --hard`) is ignored: its
/// creation epoch does not match the live session's.
pub(crate) fn read_session_agent_identity(
    session_name: &str,
) -> Result<Option<SessionAgentIdentity>> {
    let Some(created_epoch) = crate::tmux::session_created_epoch(session_name)? else {
        return Ok(None);
    };
    read_live_session_agent_identity(session_name, created_epoch)
}

/// Like [`read_session_agent_identity`] for a caller that already knows the
/// live session's creation epoch.
pub(crate) fn read_live_session_agent_identity(
    session_name: &str,
    created_epoch: i64,
) -> Result<Option<SessionAgentIdentity>> {
    Ok(read_stored_session_agent_identity(session_name)?
        .filter(|identity| identity.created_epoch == created_epoch))
}

/// Read the identity file as stored, whichever session wrote it. Only for
/// callers that check `created_epoch` themselves.
pub(crate) fn read_stored_session_agent_identity(
    session_name: &str,
) -> Result<Option<SessionAgentIdentity>> {
    let Some(line) = read_optional_trimmed(&pmux_agent_sessions_dir()?.join(session_name))? else {
        return Ok(None);
    };
    let mut fields = line.splitn(3, '\t');
    let (Some(agent), Some(session_id), Some(created_epoch)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return Ok(None);
    };
    let Ok(created_epoch) = created_epoch.parse::<i64>() else {
        return Ok(None);
    };
    if agent.is_empty() || session_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(SessionAgentIdentity {
        agent: agent.to_string(),
        session_id: session_id.to_string(),
        created_epoch,
    }))
}

// Build the source/destination pairs for all per-session metadata files.
fn session_state_paths(old_name: &str, new_name: &str) -> Result<Vec<(PathBuf, PathBuf)>> {
    let dirs = [
        pmux_launch_cmds_dir()?,
        pmux_launch_cwds_dir()?,
        pmux_host_panes_dir()?,
        pmux_agent_sessions_dir()?,
        pmux_agent_starts_dir()?,
        pmux_last_inputs_dir()?,
        pmux_parents_dir()?,
    ];
    Ok(dirs
        .iter()
        .map(|dir| (dir.join(old_name), dir.join(new_name)))
        .collect())
}

pub(crate) fn validate_session_state_migration(old_name: &str, new_name: &str) -> Result<()> {
    let paths = session_state_paths(old_name, new_name)?;
    for (_, new_path) in &paths {
        if new_path.exists() {
            anyhow::bail!(
                "cannot migrate session state: {} already exists",
                new_path.display()
            );
        }
    }
    Ok(())
}

/// Migrate all state files from an old session name to a new one after a rename.
/// Any partial filesystem work is rolled back before returning an error.
pub(crate) fn migrate_session_state(old_name: &str, new_name: &str) -> Result<()> {
    validate_session_state_migration(old_name, new_name)?;
    let moves = session_state_paths(old_name, new_name)?
        .into_iter()
        .filter(|(old_path, _)| old_path.exists());

    let mut completed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (old_path, new_path) in moves {
        if let Err(err) = fs::rename(&old_path, &new_path) {
            let mut rollback_errors = Vec::new();
            for (completed_old, completed_new) in completed.iter().rev() {
                if let Err(rollback_err) = fs::rename(completed_new, completed_old) {
                    rollback_errors.push(format!(
                        "{} to {}: {}",
                        completed_new.display(),
                        completed_old.display(),
                        rollback_err
                    ));
                }
            }
            let rollback_detail = if rollback_errors.is_empty() {
                String::new()
            } else {
                format!("; rollback also failed: {}", rollback_errors.join(", "))
            };
            return Err(err).with_context(|| {
                format!(
                    "failed to migrate session state from {} to {}{}",
                    old_path.display(),
                    new_path.display(),
                    rollback_detail
                )
            });
        }
        completed.push((old_path, new_path));
    }
    Ok(())
}

/// Remove all per-session metadata. Used when a newly-created tmux session
/// must be rolled back before it is exposed to the caller.
pub(crate) fn remove_session_metadata(session_name: &str) -> Result<()> {
    for dir in [
        pmux_launch_cmds_dir()?,
        pmux_launch_cwds_dir()?,
        pmux_host_panes_dir()?,
        pmux_agent_sessions_dir()?,
        pmux_agent_starts_dir()?,
        pmux_last_inputs_dir()?,
        pmux_parents_dir()?,
    ] {
        let path = dir.join(session_name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(err) => {
                return Err(err).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

/// Remove metadata for sessions that are no longer running, listing them
/// under the state lock. A list taken before the lock may miss a session
/// that a concurrent `pmux run -d` or rename has since created, and cleanup
/// would delete that live session's metadata. Spawn and rename hold the
/// same lock while they create a name and write its metadata.
pub(crate) fn cleanup_stale_session_metadata_now() -> Result<()> {
    with_state_lock(|| {
        let sessions = crate::tmux::list_sessions()?;
        // A missing server lists as zero sessions; keep metadata for restore.
        if sessions.is_empty() && !crate::tmux::pmux_server_running()? {
            return Ok(());
        }
        cleanup_stale_session_metadata(&sessions)
    })
}

// Remove state files for sessions that are no longer running. The caller
// must hold the state lock and have listed `live_sessions` under it.
pub(crate) fn cleanup_stale_session_metadata(live_sessions: &[SessionInfo]) -> Result<()> {
    let live: HashSet<&str> = live_sessions.iter().map(|s| s.name.as_str()).collect();
    remove_stale_files_in_dir(pmux_launch_cmds_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_launch_cwds_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_host_panes_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_agent_sessions_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_agent_starts_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_last_inputs_dir()?, &live)?;
    remove_stale_files_in_dir(pmux_parents_dir()?, &live)?;
    Ok(())
}

// Delete files in a directory whose names don't match any live session.
fn remove_stale_files_in_dir(dir: PathBuf, live: &HashSet<&str>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("failed to iterate {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
            continue;
        };
        // Skip in-flight atomic writes (see `write_state_file`).
        if name.starts_with('.') {
            continue;
        }
        if !live.contains(name) {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

// Return the most recently attached session name from history or legacy state.
pub(crate) fn read_last_attached() -> Result<Option<String>> {
    if let Some(name) = read_attach_history()?.into_iter().next() {
        return Ok(Some(name));
    }
    read_optional_trimmed(&pmux_state_dir()?.join("last_attached"))
}

// Read the ordered attach history (most recent first, deduplicated, max 16 entries).
pub(crate) fn read_attach_history() -> Result<Vec<String>> {
    let path = pmux_attach_history_file()?;
    match fs::read_to_string(&path) {
        Ok(data) => {
            let mut seen = HashSet::new();
            let mut names = Vec::new();
            for line in data.lines().map(str::trim).filter(|line| !line.is_empty()) {
                if seen.insert(line.to_string()) {
                    names.push(line.to_string());
                    if names.len() >= MAX_ATTACH_HISTORY {
                        break;
                    }
                }
            }
            Ok(names)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {}", path.display(), e)),
    }
}

// Record an attach transition: target becomes most recent, previous is second.
// Called by attach_by_name and attach_for_client after a successful switch.
// The read-modify-write runs under the state lock so concurrent attaches from
// several terminals cannot drop each other's entries.
pub(crate) fn record_attached_transition(target: &str, previous: Option<&str>) -> Result<()> {
    with_state_lock(|| record_attached_transition_locked(target, previous))
}

fn record_attached_transition_locked(target: &str, previous: Option<&str>) -> Result<()> {
    let mut new_history = Vec::with_capacity(MAX_ATTACH_HISTORY);
    new_history.push(target.to_string());

    if let Some(prev) = previous.filter(|prev| *prev != target) {
        new_history.push(prev.to_string());
    }

    for name in read_attach_history()? {
        if new_history.iter().any(|existing| existing == &name) {
            continue;
        }
        new_history.push(name);
        if new_history.len() >= MAX_ATTACH_HISTORY {
            break;
        }
    }

    write_state_file(
        &pmux_attach_history_file()?,
        &format!("{}\n", new_history.join("\n")),
    )
}

// Convert a pane ID to a filename for per-pane state files.
fn pane_state_file_name(pane_id: &str) -> String {
    pane_id.to_string()
}

// Record which session was last shown in a specific outer tmux pane.
pub(crate) fn write_pane_last_process(pane_id: &str, session_name: &str) -> Result<()> {
    write_state_file(
        &pmux_pane_last_processes_dir()?.join(pane_state_file_name(pane_id)),
        &format!("{session_name}\n"),
    )
}

// Read which session was last shown in a specific outer tmux pane.
pub(crate) fn read_pane_last_process(pane_id: &str) -> Result<Option<String>> {
    read_optional_trimmed(&pmux_pane_last_processes_dir()?.join(pane_state_file_name(pane_id)))
}

// Record which outer tmux pane (and the tmux server socket that owns it) is
// hosting a given pmux session. Stored as "<pane_id>\t<socket>" on one line;
// the socket lets the ai-attn relay scope its record to the correct outer
// server so the pane ID isn't matched against an unrelated pane on another
// server. The socket is optional — legacy files holding just a pane ID still
// read back correctly (socket = None).
pub(crate) fn write_session_host_pane(
    session_name: &str,
    pane_id: &str,
    socket: Option<&str>,
) -> Result<()> {
    let line = match socket {
        Some(sock) if !sock.is_empty() => format!("{pane_id}\t{sock}\n"),
        _ => format!("{pane_id}\n"),
    };
    write_state_file(&pmux_host_panes_dir()?.join(session_name), &line)
}

// Read which outer tmux pane is hosting a given pmux session (first field).
pub(crate) fn read_session_host_pane(session_name: &str) -> Result<Option<String>> {
    Ok(
        read_optional_trimmed(&pmux_host_panes_dir()?.join(session_name))?
            .map(|line| line.split('\t').next().unwrap_or("").trim().to_string())
            .filter(|pane| !pane.is_empty()),
    )
}

// Read the tmux server socket that owns this session's host pane, if recorded.
pub(crate) fn read_session_host_socket(session_name: &str) -> Result<Option<String>> {
    Ok(
        read_optional_trimmed(&pmux_host_panes_dir()?.join(session_name))?
            .and_then(|line| line.split('\t').nth(1).map(|s| s.trim().to_string()))
            .filter(|sock| !sock.is_empty()),
    )
}

/// Every tmux server socket recorded as hosting a pmux process, once each.
pub(crate) fn known_host_sockets() -> Result<Vec<String>> {
    let mut sockets = std::collections::BTreeSet::new();
    let entries = match fs::read_dir(pmux_host_panes_dir()?) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && let Some(socket) = read_session_host_socket(name)?
        {
            sockets.insert(socket);
        }
    }
    Ok(sockets.into_iter().collect())
}

// Remove the host-pane mapping for a session (e.g. when it's detached).
pub(crate) fn clear_session_host_pane(session_name: &str) -> Result<()> {
    let file = pmux_host_panes_dir()?.join(session_name);
    match fs::remove_file(&file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "failed to remove {}: {}",
            file.display(),
            e
        )),
    }
}

// How long a freshly written host-pane mapping survives a detached session.
// `pmux attach` records the mapping just before its tmux attach-session
// connects, so a relay sync in between sees no attached clients.
const HOST_PANE_ATTACH_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

// Clear the host-pane mapping of a session with no attached clients, unless it
// was written within the attach grace period.
pub(crate) fn clear_detached_session_host_pane(session_name: &str) -> Result<()> {
    let file = pmux_host_panes_dir()?.join(session_name);
    let recent = fs::metadata(&file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < HOST_PANE_ATTACH_GRACE);
    if recent {
        return Ok(());
    }
    clear_session_host_pane(session_name)
}

// Write the PID of the ai-attn relay daemon to the state directory.
pub(crate) fn write_ai_attn_relay_pid(pid: u32) -> Result<()> {
    write_state_file(&pmux_relay_pid_file()?, &format!("{pid}\n"))
}

// Read the PID of the ai-attn relay daemon, if the PID file exists.
pub(crate) fn read_ai_attn_relay_pid() -> Result<Option<u32>> {
    read_optional_trimmed(&pmux_relay_pid_file()?)
        .map(|opt| opt.and_then(|s| s.parse::<u32>().ok()))
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_stale_session_metadata, read_attach_history, read_last_attached,
        read_pane_last_process, read_session_launch_command, read_session_launch_cwd,
        record_attached_transition, write_pane_last_process, write_session_launch_command,
        write_session_launch_cwd,
    };
    use crate::session::SessionInfo;
    // Set up a temporary HOME directory for isolated state tests.
    fn with_temp_home<F: FnOnce()>(f: F) {
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());
        f();
    }

    // T2: state read/write cycle

    // Tests that writing and reading a launch command round-trips correctly.
    #[test]
    fn test_write_read_launch_command_roundtrip() {
        with_temp_home(|| {
            write_session_launch_command("my-session", "codex --model o4").unwrap();
            let cmd = read_session_launch_command("my-session").unwrap();
            assert_eq!(cmd.as_deref(), Some("codex --model o4"));
        });
    }

    // Tests that writing and reading a launch CWD round-trips correctly.
    #[test]
    fn test_write_read_launch_cwd_roundtrip() {
        with_temp_home(|| {
            write_session_launch_cwd("my-session", "/home/user/project").unwrap();
            let cwd = read_session_launch_cwd("my-session").unwrap();
            assert_eq!(cwd.as_deref(), Some("/home/user/project"));
        });
    }

    // Tests that reading a missing session's launch command returns None.
    #[test]
    fn test_read_launch_command_missing_returns_none() {
        with_temp_home(|| {
            let cmd = read_session_launch_command("nonexistent-session").unwrap();
            assert!(cmd.is_none());
        });
    }

    // Tests that recording and reading last-attached round-trips correctly.
    #[test]
    fn test_write_read_last_attached_roundtrip() {
        with_temp_home(|| {
            record_attached_transition("codex-session", None).unwrap();
            let name = read_last_attached().unwrap();
            assert_eq!(name.as_deref(), Some("codex-session"));
        });
    }

    // Tests that attach transitions promote the target and keep previous second.
    #[test]
    fn test_record_attached_transition_promotes_previous_target() {
        with_temp_home(|| {
            record_attached_transition("beta", Some("alpha")).unwrap();
            record_attached_transition("alpha", Some("beta")).unwrap();
            let history = read_attach_history().unwrap();
            assert_eq!(history, vec!["alpha", "beta"]);
            assert_eq!(read_last_attached().unwrap().as_deref(), Some("alpha"));
        });
    }

    // Tests that attach history takes priority over the legacy last_attached file.
    #[test]
    fn test_read_last_attached_prefers_attach_history() {
        with_temp_home(|| {
            // Write a legacy last_attached file directly.
            let dir = super::pmux_state_dir().unwrap();
            super::ensure_state_dir(&dir).unwrap();
            std::fs::write(dir.join("last_attached"), "stale\n").unwrap();
            record_attached_transition("fresh", None).unwrap();
            assert_eq!(read_last_attached().unwrap().as_deref(), Some("fresh"));
        });
    }

    // Tests that writing and reading per-pane last process round-trips correctly.
    #[test]
    fn test_write_read_pane_last_process_roundtrip() {
        with_temp_home(|| {
            write_pane_last_process("%12", "alpha").unwrap();
            assert_eq!(
                read_pane_last_process("%12").unwrap().as_deref(),
                Some("alpha")
            );
        });
    }

    #[test]
    fn test_sidebar_width_round_trip_and_invalid_value() {
        use super::{pmux_state_dir, read_sidebar_width, write_sidebar_width};
        with_temp_home(|| {
            assert_eq!(read_sidebar_width().unwrap(), None);
            write_sidebar_width(51).unwrap();
            assert_eq!(read_sidebar_width().unwrap(), Some(51));
            std::fs::write(pmux_state_dir().unwrap().join("sidebar_width"), "invalid\n").unwrap();
            assert_eq!(read_sidebar_width().unwrap(), None);
            assert!(write_sidebar_width(0).is_err());
        });
    }

    // Spawn and rename hold a scoped guard; it must exclude closure users.
    #[test]
    fn test_state_lock_guard_excludes_with_state_lock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        with_temp_home(|| {
            let guard = super::lock_state().unwrap();
            let ran = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&ran);
            let waiter = std::thread::spawn(move || {
                super::with_state_lock(|| {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(!ran.load(Ordering::SeqCst), "lock was not exclusive");
            drop(guard);
            waiter.join().unwrap().unwrap();
            assert!(ran.load(Ordering::SeqCst));
        });
    }

    // Regression: attach-history updates are a read-modify-write. Without the
    // state lock, concurrent attaches overwrote each other's entries. flock
    // locks belong to each open file, so threads exercise the same exclusion
    // that separate pmux processes get.
    #[test]
    fn test_concurrent_attach_transitions_lose_no_entries() {
        with_temp_home(|| {
            let threads: Vec<_> = (0..12)
                .map(|i| {
                    std::thread::spawn(move || record_attached_transition(&format!("s{i}"), None))
                })
                .collect();
            for thread in threads {
                thread.join().unwrap().unwrap();
            }
            let mut history = read_attach_history().unwrap();
            history.sort();
            let mut expected: Vec<String> = (0..12).map(|i| format!("s{i}")).collect();
            expected.sort();
            assert_eq!(history, expected);
        });
    }

    // Atomic writes must not leave temp files behind, and cleanup must never
    // delete an in-flight one (dot-prefixed) as if it were stale metadata.
    #[test]
    fn test_atomic_state_writes_leave_no_temp_files_and_survive_cleanup() {
        with_temp_home(|| {
            write_session_launch_command("live", "claude").unwrap();
            write_session_launch_command("live", "codex").unwrap();
            let dir = super::pmux_launch_cmds_dir().unwrap();
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, vec!["live".to_string()]);
            assert_eq!(
                read_session_launch_command("live").unwrap().as_deref(),
                Some("codex")
            );

            let in_flight = dir.join(".tmpABC123");
            std::fs::write(&in_flight, "partial").unwrap();
            cleanup_stale_session_metadata(&[]).unwrap();
            assert!(in_flight.exists());
            assert!(read_session_launch_command("live").unwrap().is_none());
        });
    }

    // Tests that cleanup removes metadata for dead sessions but keeps live ones.
    #[test]
    fn test_cleanup_removes_stale_metadata() {
        with_temp_home(|| {
            // Write metadata for two sessions.
            write_session_launch_command("alive", "claude").unwrap();
            write_session_launch_command("dead", "codex").unwrap();

            // Only "alive" is in the live sessions list.
            let live = vec![SessionInfo {
                name: "alive".to_string(),
                pane_id: "%0".to_string(),
                created_epoch: 1,
                last_activity_epoch: 1,
                current_command: "claude".to_string(),
                start_command: None,
                current_path: None,
                launch_command: None,
                launch_cwd: None,
                attached_clients: 0,
            }];
            cleanup_stale_session_metadata(&live).unwrap();

            assert!(read_session_launch_command("alive").unwrap().is_some());
            assert!(read_session_launch_command("dead").unwrap().is_none());
        });
    }

    // Regression: cleanup on a throwaway PMUX_SOCKET (whose session list is
    // empty) must not delete the default backend's session metadata.
    #[test]
    fn test_non_default_socket_state_is_isolated() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        env.remove("PMUX_SOCKET");
        write_session_launch_command("live-work", "claude").unwrap();

        env.set("PMUX_SOCKET", "pmux-test");
        assert!(read_session_launch_command("live-work").unwrap().is_none());
        write_session_launch_command("sandbox", "bash").unwrap();
        cleanup_stale_session_metadata(&[]).unwrap();
        assert!(read_session_launch_command("sandbox").unwrap().is_none());

        env.remove("PMUX_SOCKET");
        assert_eq!(
            read_session_launch_command("live-work").unwrap().as_deref(),
            Some("claude")
        );
    }

    // Tests that writing and reading a host pane round-trips correctly.
    #[test]
    fn test_write_read_session_host_pane_roundtrip() {
        use super::{read_session_host_pane, read_session_host_socket, write_session_host_pane};
        with_temp_home(|| {
            // Without a socket: pane reads back, socket is None.
            write_session_host_pane("my-session", "%42", None).unwrap();
            assert_eq!(
                read_session_host_pane("my-session").unwrap().as_deref(),
                Some("%42")
            );
            assert_eq!(read_session_host_socket("my-session").unwrap(), None);

            // With a socket: both pane and socket round-trip.
            write_session_host_pane("sock-session", "%7", Some("/tmp/tmux-1000/homelab")).unwrap();
            assert_eq!(
                read_session_host_pane("sock-session").unwrap().as_deref(),
                Some("%7")
            );
            assert_eq!(
                read_session_host_socket("sock-session").unwrap().as_deref(),
                Some("/tmp/tmux-1000/homelab")
            );
        });
    }

    // Tests that a legacy host-pane file (bare pane ID, no tab/socket) still
    // reads back the pane with no socket — backward compatibility.
    #[test]
    fn test_read_legacy_host_pane_without_socket() {
        use super::{pmux_host_panes_dir, read_session_host_pane, read_session_host_socket};
        with_temp_home(|| {
            let dir = pmux_host_panes_dir().unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("legacy"), "%99\n").unwrap();
            assert_eq!(
                read_session_host_pane("legacy").unwrap().as_deref(),
                Some("%99")
            );
            assert_eq!(read_session_host_socket("legacy").unwrap(), None);
        });
    }

    // Tests that clearing a host pane is idempotent (Ok for nonexistent, removes existing).
    #[test]
    fn test_clear_session_host_pane_idempotent() {
        use super::{clear_session_host_pane, read_session_host_pane, write_session_host_pane};
        with_temp_home(|| {
            // Clearing a nonexistent pane mapping should be Ok.
            assert!(clear_session_host_pane("nonexistent").is_ok());

            // Write then clear should remove it.
            write_session_host_pane("my-session", "%10", None).unwrap();
            assert!(read_session_host_pane("my-session").unwrap().is_some());
            clear_session_host_pane("my-session").unwrap();
            assert!(read_session_host_pane("my-session").unwrap().is_none());
        });
    }

    // A mapping written just before attach-session connects survives a relay
    // sync that still sees the session detached; an old one is cleared.
    #[test]
    fn test_clear_detached_session_host_pane_keeps_fresh_mapping() {
        use super::{
            clear_detached_session_host_pane, pmux_host_panes_dir, read_session_host_pane,
            write_session_host_pane,
        };
        with_temp_home(|| {
            write_session_host_pane("attaching", "%3", None).unwrap();
            clear_detached_session_host_pane("attaching").unwrap();
            assert_eq!(
                read_session_host_pane("attaching").unwrap().as_deref(),
                Some("%3")
            );

            let file = pmux_host_panes_dir().unwrap().join("attaching");
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(old)
                .unwrap();
            clear_detached_session_host_pane("attaching").unwrap();
            assert!(read_session_host_pane("attaching").unwrap().is_none());
            assert!(clear_detached_session_host_pane("never-written").is_ok());
        });
    }

    // Each touch leaves a new value, which is what sidebars watch for.
    #[test]
    fn labels_changed_stamp_gets_a_new_value_on_every_touch() {
        use super::{read_labels_changed, touch_labels_changed};
        with_temp_home(|| {
            assert_eq!(read_labels_changed(), None);
            touch_labels_changed();
            let first = read_labels_changed().expect("stamp written");
            std::thread::sleep(std::time::Duration::from_millis(2));
            touch_labels_changed();
            let second = read_labels_changed().expect("stamp rewritten");
            assert_ne!(first, second);
        });
    }

    // Identity readers must ignore records left by an earlier session.
    #[test]
    fn test_live_agent_identity_requires_matching_creation_epoch() {
        use super::{
            read_live_session_agent_identity, read_stored_session_agent_identity,
            write_session_agent_identity,
        };
        with_temp_home(|| {
            write_session_agent_identity("work", "codex", "abc", 1000).unwrap();
            let live = read_live_session_agent_identity("work", 1000).unwrap();
            assert_eq!(
                live.map(|identity| identity.session_id).as_deref(),
                Some("abc")
            );
            // A later session reusing the name must not inherit it, and the
            // old epoch-0 wildcard no longer matches anything.
            assert!(
                read_live_session_agent_identity("work", 2000)
                    .unwrap()
                    .is_none()
            );
            write_session_agent_identity("work", "codex", "abc", 0).unwrap();
            assert!(
                read_live_session_agent_identity("work", 2000)
                    .unwrap()
                    .is_none()
            );
            assert!(
                read_stored_session_agent_identity("work")
                    .unwrap()
                    .is_some()
            );
        });
    }

    // Tests that writing and reading a relay PID round-trips correctly.
    #[test]
    fn test_write_read_relay_pid_roundtrip() {
        use super::{read_ai_attn_relay_pid, write_ai_attn_relay_pid};
        with_temp_home(|| {
            write_ai_attn_relay_pid(12345).unwrap();
            let pid = read_ai_attn_relay_pid().unwrap();
            assert_eq!(pid, Some(12345));
        });
    }

    // Tests that migrate_session_state moves files from old name to new name.
    #[test]
    fn test_migrate_session_state_moves_files() {
        use super::{migrate_session_state, read_session_launch_command, read_session_launch_cwd};
        with_temp_home(|| {
            write_session_launch_command("old", "claude --model sonnet").unwrap();
            write_session_launch_cwd("old", "/home/user/project").unwrap();
            migrate_session_state("old", "new").unwrap();

            // New name should have the data.
            assert_eq!(
                read_session_launch_command("new").unwrap().as_deref(),
                Some("claude --model sonnet")
            );
            assert_eq!(
                read_session_launch_cwd("new").unwrap().as_deref(),
                Some("/home/user/project")
            );
            // Old name should be gone.
            assert!(read_session_launch_command("old").unwrap().is_none());
            assert!(read_session_launch_cwd("old").unwrap().is_none());
        });
    }

    // Tests that migrating nonexistent session names is a no-op (Ok).
    #[test]
    fn test_migrate_session_state_noop_when_no_files() {
        use super::migrate_session_state;
        with_temp_home(|| {
            assert!(migrate_session_state("nonexistent-old", "nonexistent-new").is_ok());
        });
    }

    #[test]
    fn test_migrate_session_state_does_not_partially_move_on_conflict() {
        use super::{migrate_session_state, read_session_launch_command, read_session_launch_cwd};
        with_temp_home(|| {
            write_session_launch_command("old", "codex").unwrap();
            write_session_launch_cwd("old", "/work/old").unwrap();
            write_session_launch_cwd("new", "/work/stale").unwrap();

            assert!(migrate_session_state("old", "new").is_err());
            assert_eq!(
                read_session_launch_command("old").unwrap().as_deref(),
                Some("codex")
            );
            assert_eq!(
                read_session_launch_cwd("old").unwrap().as_deref(),
                Some("/work/old")
            );
            assert!(read_session_launch_command("new").unwrap().is_none());
            assert_eq!(
                read_session_launch_cwd("new").unwrap().as_deref(),
                Some("/work/stale")
            );
        });
    }
}
