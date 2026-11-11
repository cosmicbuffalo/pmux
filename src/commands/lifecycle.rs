//! Starting, killing, renaming, and restarting pmux processes.

use super::attach::{
    attach_by_name_inner, attach_target, ordered_creation_sessions, resolve_attach_target,
    try_ensure_relay,
};
use crate::config::{ensure_pmux_config, server_conf_path};
use crate::history::{self, HistoryEntry, HistoryQuery};
use crate::lineage::{ParentSpec, SpawnedBy};
use crate::session::SessionInfo;
use crate::state::{
    lock_state, pmux_history_index_file, read_live_session_agent_identity, remove_session_metadata,
    write_session_agent_identity, write_session_launch_command, write_session_launch_cwd,
};
use crate::tmux::{
    detect_current_pmux_session, list_sessions, output_indicates_missing_server,
    pmux_server_running, sync_environment_to_server, tmux_cmd, tmux_failure,
};
use crate::util::{SessionName, exact_session_target, shell_join};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartResult {
    SoftReloaded,
    SoftNoServer,
    HardRestarted,
}

/// Files used to hand a completed attached process's terminal scrollback back
/// to the `pmux run` invocation that owns the outer terminal.
#[derive(Debug)]
struct CompletionCapture {
    output_path: PathBuf,
    status_path: PathBuf,
}

#[derive(Debug)]
struct CompletedProcess {
    output: Vec<u8>,
    exit_code: i32,
}

impl CompletionCapture {
    fn for_session(session: &str) -> Result<Self> {
        let dir = crate::state::ensure_pmux_state_dir()?.join("completed-output");
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create completion directory {}", dir.display()))?;
        Ok(Self {
            output_path: dir.join(format!("{session}.output")),
            status_path: dir.join(format!("{session}.status")),
        })
    }

    fn remove_stale(&self) {
        for path in [&self.output_path, &self.status_path] {
            if let Err(err) = fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "pmux: failed to remove stale completion data {}: {err}",
                    path.display()
                );
            }
        }
    }

    fn take(&self) -> Result<Option<CompletedProcess>> {
        let raw_status = match fs::read_to_string(&self.status_path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read completed process status {}",
                        self.status_path.display()
                    )
                });
            }
        };

        let exit_code = raw_status.trim().parse::<i32>().with_context(|| {
            format!(
                "invalid completed process status in {}",
                self.status_path.display()
            )
        })?;
        let output = match fs::read(&self.output_path) {
            Ok(output) => output,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read completed process output {}",
                        self.output_path.display()
                    )
                });
            }
        };
        self.remove_stale();
        Ok(Some(CompletedProcess { output, exit_code }))
    }
}

// This runs as the initial command in an attached pmux session. It waits for
// the requested command to exit, captures the rendered tmux scrollback while
// the pane still exists, and then exits with the child's original status.
//
// The parent `pmux run` process is passed in explicitly. If the user detaches
// before the child exits, that parent is gone and the supervisor skips capture:
// there is no outer terminal left to replay it into.
const COMPLETION_CAPTURE_SUPERVISOR: &str = r#"capture_path=$1
status_path=$2
caller_pid=$3
shift 3
"$@"
exit_status=$?
if kill -0 "$caller_pid" 2>/dev/null; then
  umask 077
  capture_tmp="${capture_path}.tmp.$$"
  if tmux capture-pane -p -e -S - -t "$TMUX_PANE" > "$capture_tmp"; then
    mv -f "$capture_tmp" "$capture_path"
  else
    rm -f "$capture_tmp"
  fi
  status_tmp="${status_path}.tmp.$$"
  if printf '%s\n' "$exit_status" > "$status_tmp"; then
    mv -f "$status_tmp" "$status_path"
  else
    rm -f "$status_tmp"
  fi
fi
exit "$exit_status""#;

// Keep this in sync with templates/status.conf. Attached `pmux run` commands
// begin under the completion-capture supervisor, so pane_start_command alone
// is no longer a reliable label. Applying this native format after a spawn or
// reload also repairs long-lived servers which cached an older status.conf.
const SESSION_LABEL_FORMAT: &str = "#{?#{||:#{m/r:^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]]$,#{session_name}},#{m/r:^.*-[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[[:alnum:]][[:alnum:]][[:alnum:]][[:alnum:]]$,#{session_name}}},#{?@pmux_launch_command,#{b:#{s| .*||:@pmux_launch_command}},#{?pane_start_command,#{b:#{s| .*||:pane_start_command}},#{?pane_current_command,#{b:#{s| .*||:pane_current_command}},#{session_name}}}},#{session_name}}";

/// Spawn a new pmux process and immediately attach to it.
/// Called by `pmux run <cmd>` when no `--cwd` override is given.
pub fn run(
    command: String,
    args: Vec<String>,
    session_name: Option<String>,
    parent: &ParentSpec,
) -> Result<()> {
    let cwd = env::current_dir().context("failed to get current working directory")?;
    run_in_cwd(command, args, session_name, &cwd, parent)
}

/// Like [`run`], but starts the command from an explicit working directory.
///
/// This is used by `pmux run --cwd`; unlike detached spawning it retains the
/// completed pane's scrollback and replays it after the inner client returns.
pub fn run_in_cwd(
    command: String,
    args: Vec<String>,
    session_name: Option<String>,
    cwd: &Path,
    parent: &ParentSpec,
) -> Result<()> {
    run_in_cwd_then(command, args, session_name, cwd, parent, |_| Ok(()))
}

/// [`run_in_cwd`], calling `on_spawned` with the session name once the process
/// exists and before the client attaches.
fn run_in_cwd_then(
    command: String,
    args: Vec<String>,
    session_name: Option<String>,
    cwd: &Path,
    parent: &ParentSpec,
    on_spawned: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let session = resolved_session_name(session_name)?;
    let completion = CompletionCapture::for_session(&session)?;
    completion.remove_stale();

    if let Err(err) = spawn_named(command, args, &session, cwd, parent, Some(&completion)) {
        completion.remove_stale();
        return Err(err);
    }
    on_spawned(&session)?;

    // A tmux client prints "no sessions" when the supervised command closes
    // the final session. Completion data tells us that this is expected, so
    // keep that transient client diagnostic out of the caller's scrollback.
    let attach_result = attach_by_name_inner(&session, true);
    let completed = completion.take()?;

    if let Some(completed) = completed {
        replay_completed_output(&completed.output)?;
        if completed.exit_code != 0 {
            bail!(
                "pmux process '{}' exited with status {}",
                session,
                completed.exit_code
            );
        }
        return Ok(());
    }

    attach_result
}

/// Create a new detached tmux session on the pmux socket and return its name.
/// Called by `pmux run -d` for detached spawning.
pub fn spawn(
    command: String,
    args: Vec<String>,
    session_name: Option<String>,
    cwd: &Path,
    parent: &ParentSpec,
) -> Result<String> {
    let session = resolved_session_name(session_name)?;
    spawn_named(command, args, &session, cwd, parent, None)
}

/// List past agent sessions from Claude Code and Codex history, newest first.
/// Sessions running right now (in pmux or elsewhere) are excluded unless
/// `query.include_live` is set.
pub fn history(query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
    let sources = history::Sources::from_env();
    let mut live = history::LiveSessions::default();
    if pmux_server_running().unwrap_or(false) {
        for session in list_sessions().unwrap_or_default() {
            if let Ok(Some(identity)) =
                read_live_session_agent_identity(&session.name, session.created_epoch)
            {
                live.insert_pmux(&identity.agent, &identity.session_id, &session.name);
            }
        }
    }
    live.detect_external(&sources);
    let cache = pmux_history_index_file().ok();
    let metas = history::scan(&sources, cache.as_deref());
    Ok(history::build_history(
        metas,
        &history::codex_thread_names(&sources),
        &live,
        query,
    ))
}

/// Relaunch a past agent session (see [`history`]) in a new pmux process,
/// from the directory it originally ran in. Attaches unless `detach`, in
/// which case the new process name is returned.
pub fn resume(
    target: &str,
    session_name: Option<String>,
    detach: bool,
    parent: &ParentSpec,
) -> Result<Option<String>> {
    let entries = history(&HistoryQuery {
        include_live: true,
        ..HistoryQuery::default()
    })?;
    let entry = history::find_entry(&entries, target)?;
    let label = format!("{}:{}", entry.agent, entry.session_id);
    if entry.live {
        match &entry.pmux_process {
            Some(process) => bail!(
                "{label} is already running in pmux process '{process}' (pmux attach {process})"
            ),
            None => bail!(
                "{label} is already running outside pmux; resuming it again would put two writers on one transcript"
            ),
        }
    }
    if entry.agent == "codex"
        && !history::codex_live_detection_available(&history::Sources::from_env())
    {
        eprintln!(
            "pmux: warning: cannot check whether {label} is already running outside pmux \
             (no /proc and no lsof); make sure it is not open elsewhere"
        );
    }
    let argv = entry
        .resume_command
        .clone()
        .ok_or_else(|| anyhow!("pmux does not know how to resume {} sessions", entry.agent))?;
    let cwd = entry
        .cwd
        .as_deref()
        .ok_or_else(|| anyhow!("{label} has no recorded working directory"))?;
    if !entry.cwd_exists {
        bail!("cannot resume {label}: its directory {cwd} no longer exists");
    }

    // Record the identity up front rather than waiting for the agent's first
    // ai-attn signal, so the session counts as live in pmux immediately.
    let (agent, session_id) = (entry.agent.clone(), entry.session_id.clone());
    let record = move |session: &str| {
        // Stamp the new session's creation epoch: identity readers ignore
        // records whose epoch does not match the live session.
        if let Ok(Some(epoch)) = crate::tmux::session_created_epoch(session) {
            let _ = write_session_agent_identity(session, &agent, &session_id, epoch);
        }
        Ok(())
    };
    let mut argv = argv.into_iter();
    let command = argv.next().context("empty resume command")?;
    let args: Vec<String> = argv.collect();
    let cwd = Path::new(cwd);
    if detach {
        let session = spawn(command, args, session_name, cwd, parent)?;
        record(&session)?;
        Ok(Some(session))
    } else {
        run_in_cwd_then(command, args, session_name, cwd, parent, record)?;
        Ok(None)
    }
}

/// The parent of one process, as `pmux parent` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentReport {
    pub process: String,
    /// The parent clients see: a local one only while it is alive.
    pub reported: Option<SpawnedBy>,
    /// What is on record, alive or not.
    pub recorded: Option<SpawnedBy>,
}

/// Show `target`'s parent, after first applying `change` if given:
/// `ParentSpec::None` clears it, `Local`/`Remote` set it (`Auto` is ignored).
/// Called by `pmux parent`.
pub fn parent(target: &str, change: Option<ParentSpec>) -> Result<ParentReport> {
    let (name, created) = {
        let sessions = list_sessions()?;
        let child = resolve_attach_target(&sessions, target)?;
        (child.name.clone(), child.created_epoch)
    };
    if let Some(change) = change.filter(|change| *change != ParentSpec::Auto) {
        crate::state::with_state_lock(|| {
            let sessions = list_sessions()?;
            if !sessions
                .iter()
                .any(|s| s.name == name && s.created_epoch == created)
            {
                bail!("process '{name}' exited");
            }
            if change == ParentSpec::None {
                return crate::lineage::clear_parent(&name);
            }
            if change == ParentSpec::Local(name.clone()) {
                bail!("a process cannot be its own parent");
            }
            let parent = crate::lineage::resolve(&change)?.context("no parent given")?;
            crate::lineage::check_no_cycle(&name, &parent, &sessions)?;
            crate::lineage::write_parent(&name, created, &parent)
        })?;
        crate::restore::snapshot_best_effort();
    }
    let sessions = list_sessions()?;
    Ok(ParentReport {
        reported: crate::lineage::live_parents(&sessions).remove(&name),
        recorded: crate::lineage::recorded_parent(&name, created),
        process: name,
    })
}

fn resolved_session_name(session_name: Option<String>) -> Result<SessionName> {
    match session_name {
        Some(name) => SessionName::from_user_input(&name),
        None => Ok(SessionName::generate()),
    }
}

fn spawn_named(
    command: String,
    args: Vec<String>,
    session: &str,
    cwd: &Path,
    parent: &ParentSpec,
    completion: Option<&CompletionCapture>,
) -> Result<String> {
    ensure_pmux_config()?;
    crate::tmux::ensure_tmux_supported()?;
    // Before creating anything: an explicit parent that does not exist fails
    // the spawn, while an automatic lookup that fails records nothing.
    let parent = crate::lineage::resolve(parent)?;

    let cwd_str = cwd.display().to_string();

    let mut parts = Vec::with_capacity(2 + args.len());
    parts.push(command);
    parts.extend(args);
    crate::agents::apply_launch_rewrites(&mut parts);
    let launch_command = shell_join(&parts);
    let shell_cmd = match completion {
        Some(completion) => completion_supervisor_command(&parts, completion)?,
        None => launch_command.clone(),
    };

    let mut generation = crate::startup::AgentStart::new(0, crate::AiAttnSnapshot::load());
    sync_environment_to_server();

    // Create the session and record its metadata under the state lock, so a
    // concurrent metadata cleanup (which lists sessions under the same lock)
    // cannot delete the new session's files in between.
    let state_lock = lock_state()?;
    // `-s` and `-c` are format-expanded by tmux; escape them so a `#(...)` in
    // a name or directory is taken literally instead of being executed.
    let status = tmux_cmd()
        .env("PATH", crate::tmux::guarded_path_env())
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(crate::tmux::tmux_format_escape(session))
        .arg("-c")
        .arg(crate::tmux::tmux_format_escape_path(cwd))
        .arg(&shell_cmd)
        .status()
        .context("failed to execute tmux new-session")?;

    if !status.success() {
        bail!("failed to create pmux process '{session}'");
    }

    // tmux just created this name, so anything already recorded under it
    // belongs to an earlier session (kept across a crash or reboot for
    // restore). Drop it so the new process cannot inherit that session's
    // agent identity or host pane.
    let metadata_result = remove_session_metadata(session)
        .and_then(|()| write_session_launch_command(session, &launch_command))
        .and_then(|()| write_session_launch_cwd(session, &cwd_str))
        .and_then(|()| {
            let epoch =
                crate::tmux::session_created_epoch(session)?.context("new session disappeared")?;
            generation.created_epoch = epoch;
            crate::startup::write_start(session, &generation)?;
            match &parent {
                Some(parent) => crate::lineage::write_parent(session, epoch, parent),
                None => Ok(()),
            }
        });
    if let Err(metadata_err) = metadata_result {
        let rollback_result = rollback_spawn(session);
        return match rollback_result {
            Ok(()) => Err(metadata_err).with_context(|| {
                format!(
                    "failed to persist metadata for pmux process '{session}'; creation rolled back"
                )
            }),
            Err(rollback_err) => Err(anyhow!(
                "failed to persist metadata for pmux process '{session}': {metadata_err:#}; \
                 additionally failed to roll back the tmux session: {rollback_err:#}"
            )),
        };
    }
    drop(state_lock);
    set_session_launch_command_option(session, &launch_command);
    ensure_native_session_label_format_best_effort();

    crate::restore::install_snapshot_hook_best_effort();
    try_ensure_relay();
    // Keep the cross-restart snapshot fresh as sessions come and go.
    crate::restore::snapshot_best_effort();

    Ok(session.to_string())
}

// The completion supervisor becomes tmux's pane_start_command/current_command,
// so retain the user's real launch command as pane-local tmux metadata for
// native status formats.
fn set_session_launch_command_option(session: &str, launch_command: &str) {
    // Exact session target: a bare name like "3.3.4 review" parses as window.pane.
    let output = match tmux_cmd()
        .args(["set-option", "-p", "-t"])
        .arg(exact_session_target(session))
        .args(["@pmux_launch_command", launch_command])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!(
                "pmux: failed to record launch command for status display in '{}': {err}",
                session
            );
            return;
        }
    };
    if !output.status.success() && !output_indicates_missing_server(&output.stderr) {
        eprintln!(
            "pmux: failed to record launch command for status display in '{}': {}",
            session,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}

fn hydrate_session_launch_command_options() -> Result<()> {
    for session in list_sessions()? {
        if let Some(launch_command) = session.launch_command {
            set_session_launch_command_option(&session.name, &launch_command);
        }
    }
    Ok(())
}

fn ensure_native_session_label_format_best_effort() {
    let output = match tmux_cmd()
        .args([
            "set-option",
            "-g",
            "@pmux_session_label_format",
            SESSION_LABEL_FORMAT,
        ])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!("pmux: failed to install the native session label format: {err}");
            return;
        }
    };
    if !output.status.success() && !output_indicates_missing_server(&output.stderr) {
        eprintln!(
            "pmux: failed to install the native session label format: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}

fn completion_supervisor_command(
    parts: &[String],
    completion: &CompletionCapture,
) -> Result<String> {
    let mut supervisor_parts = Vec::with_capacity(parts.len() + 7);
    supervisor_parts.push("sh".to_string());
    supervisor_parts.push("-c".to_string());
    supervisor_parts.push(COMPLETION_CAPTURE_SUPERVISOR.to_string());
    // `$0` is not included in the script's `"$@"` command invocation.
    supervisor_parts.push("pmux-completion-capture".to_string());
    supervisor_parts.push(completion.output_path.display().to_string());
    supervisor_parts.push(completion.status_path.display().to_string());
    supervisor_parts.push(std::process::id().to_string());
    supervisor_parts.extend_from_slice(parts);
    Ok(shell_join(&supervisor_parts))
}

fn replay_completed_output(output: &[u8]) -> Result<()> {
    let output = trim_blank_terminal_rows(output);
    if output.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(output)
        .context("failed to replay completed pmux process output")?;
    stdout
        .flush()
        .context("failed to flush completed pmux process output")?;
    Ok(())
}

/// `tmux capture-pane` includes unused rows above and below the live viewport.
/// Remove only outer lines that render as blank before replaying completed
/// output into the caller's terminal. ANSI styling sequences on otherwise
/// blank lines do not make those lines meaningful output.
fn trim_blank_terminal_rows(output: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut first_content_start = None;
    let mut last_content_end = 0;
    while start < output.len() {
        let line_end = output[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(output.len(), |offset| start + offset + 1);
        if !terminal_line_is_blank(&output[start..line_end]) {
            first_content_start.get_or_insert(start);
            last_content_end = line_end;
        }
        start = line_end;
    }
    first_content_start.map_or(&[], |start| &output[start..last_content_end])
}

fn terminal_line_is_blank(line: &[u8]) -> bool {
    let mut index = 0;
    while index < line.len() {
        match line[index] {
            b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            b'\x1b' if line.get(index + 1) == Some(&b'[') => {
                // Skip one CSI sequence (including SGR color/style resets).
                index += 2;
                while let Some(byte) = line.get(index) {
                    index += 1;
                    if (0x40..=0x7e).contains(byte) {
                        break;
                    }
                }
            }
            b'\x1b' if line.get(index + 1) == Some(&b']') => {
                // Skip an OSC sequence, terminated by BEL or ST (ESC \\).
                index += 2;
                while let Some(byte) = line.get(index) {
                    if *byte == 0x07 {
                        index += 1;
                        break;
                    }
                    if *byte == b'\x1b' && line.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Kill a pmux-managed session by name or selector (newest/oldest).
/// Called by `pmux kill <target>`.
pub fn kill(target: &str) -> Result<()> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        bail!("no running pmux processes found");
    }
    let session = resolve_attach_target(&sessions, target)?;
    kill_session(&session.name)?;
    // Refresh the cross-restart snapshot so a killed session isn't resurrected.
    crate::restore::snapshot_best_effort();
    Ok(())
}

/// Kill the processes the caller's own pmux process spawned: its direct
/// children, or with `all_levels` every process below it. Returns the names
/// killed. Called by `pmux kill --children/--descendants`.
pub fn kill_spawned(all_levels: bool) -> Result<Vec<String>> {
    let targets = crate::spawned_names(&list_sessions()?, None, all_levels)?;
    let mut killed = Vec::with_capacity(targets.len());
    let mut failure = None;
    for name in targets {
        match kill_session(&name) {
            Ok(()) => killed.push(name),
            Err(err) if failure.is_none() => failure = Some(err),
            Err(_) => {}
        }
    }
    if !killed.is_empty() {
        crate::restore::snapshot_best_effort();
    }
    match failure {
        Some(err) => Err(err.context(format!("killed {} process(es) first", killed.len()))),
        None => Ok(killed),
    }
}

fn kill_session(name: &str) -> Result<()> {
    let status = tmux_cmd()
        .arg("kill-session")
        .arg("-t")
        .arg(exact_session_target(name))
        .status()
        .context("failed to execute tmux kill-session")?;
    if !status.success() {
        bail!("failed to kill pmux process '{}'", name);
    }
    Ok(())
}

/// Kill the current session and switch the client to the next available process.
/// Called by the `k` tmux keybinding inside pmux.
pub fn kill_current_and_next(current: Option<String>, client: Option<String>) -> Result<()> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        bail!("no running pmux processes found");
    }

    let current_name = current
        .or_else(|| detect_current_pmux_session(&sessions).ok())
        .ok_or_else(|| anyhow!("could not determine current pmux process"))?;
    // The binding passes the name it saw when the key was pressed; that
    // session may already have exited. Never act on a name that is gone.
    if !sessions.iter().any(|s| s.name == current_name) {
        bail!("pmux process '{current_name}' is no longer running");
    }
    let replacement = replacement_after_kill(&sessions, &current_name);

    if let Some(ref next_name) = replacement {
        attach_target(next_name, client.as_deref(), Some(current_name.as_str()))?;
    }

    let status = tmux_cmd()
        .arg("kill-session")
        .arg("-t")
        .arg(exact_session_target(&current_name))
        .status()
        .context("failed to execute tmux kill-session")?;
    if !status.success() {
        bail!("failed to kill pmux process '{}'", current_name);
    }

    crate::restore::snapshot_best_effort();
    Ok(())
}

/// Rename a tmux session and migrate its state files. Called by `pmux rename-session`.
pub fn rename_session(old_name: &str, new_name: &str) -> Result<()> {
    let new_name = SessionName::from_user_input(new_name)?;
    let new_name = new_name.as_str();
    if old_name == new_name {
        return Ok(());
    }

    // Between rename-session and the metadata migration the new name is live
    // but has no files yet; the state lock keeps metadata cleanup out.
    let state_lock = lock_state()?;
    if list_sessions()?
        .iter()
        .any(|session| session.name == new_name)
    {
        bail!("a pmux process named '{new_name}' already exists");
    }
    // No live session uses the new name, so any files recorded under it are
    // leftovers from a dead session and must not block the rename.
    remove_session_metadata(new_name)?;
    let created_epoch = crate::tmux::session_created_epoch(old_name)?;
    rename_session_transaction(
        old_name,
        new_name,
        || rename_tmux_session(old_name, new_name),
        || rename_tmux_session(new_name, old_name),
    )?;
    // Children name their parent; follow it to the new name. Best effort:
    // the rename itself already happened.
    if let Some(created_epoch) = created_epoch
        && let Err(err) =
            crate::lineage::rename_parent_references(old_name, new_name, created_epoch)
    {
        eprintln!("pmux: failed to update processes spawned by '{old_name}': {err:#}");
    }
    drop(state_lock);
    // Manual sidebar membership follows the process identity through a rename.
    // Group metadata is advisory, so a persistence failure should not undo an
    // otherwise successful tmux/state-file transaction.
    let _ = crate::groups::rename_process_in_manual_groups(old_name, new_name);
    crate::agent_rename::propagate_rename_best_effort(new_name);
    crate::restore::snapshot_best_effort();

    Ok(())
}

fn rename_tmux_session(old_name: &str, new_name: &str) -> Result<()> {
    let status = tmux_cmd()
        .arg("rename-session")
        .arg("-t")
        .arg(exact_session_target(old_name))
        // The new name is format-expanded by tmux; the `-t` target is not.
        .arg(crate::tmux::tmux_format_escape(new_name))
        .status()
        .context("failed to execute tmux rename-session")?;
    if !status.success() {
        bail!("failed to rename session '{}' to '{}'", old_name, new_name);
    }
    Ok(())
}

fn rename_session_transaction<F, R>(
    old_name: &str,
    new_name: &str,
    rename_tmux: F,
    rollback_tmux: R,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
    R: FnOnce() -> Result<()>,
{
    crate::state::validate_session_state_migration(old_name, new_name)?;
    rename_tmux()?;
    if let Err(migration_err) = crate::state::migrate_session_state(old_name, new_name) {
        return match rollback_tmux() {
            Ok(()) => Err(migration_err),
            Err(rollback_err) => Err(anyhow!(
                "{migration_err:#}; additionally failed to restore the tmux session name: {rollback_err:#}"
            )),
        };
    }
    Ok(())
}

/// Restart pmux: soft reload reloads tmux config; hard restart kills the server.
/// Called by `pmux restart [--hard]`.
pub fn restart(hard: bool) -> Result<RestartResult> {
    // A relay survives binary replacement and calls list_sessions(), which
    // regenerates config. Replace it first so an old relay cannot race this
    // migration with stale embedded templates.
    crate::relay::restart_ai_attn_relay_if_running()?;
    // A tmux.rb user's tmux.conf.rb is the real config: compile it, and say
    // so if that fails, before anything is reloaded from it.
    crate::config::compile_tmux_conf_rb()?;
    ensure_pmux_config()?;
    if hard {
        let output = tmux_cmd()
            .arg("kill-server")
            .output()
            .context("failed to execute tmux kill-server")?;
        if !output.status.success() && !output_indicates_missing_server(&output.stderr) {
            return Err(tmux_failure("failed to stop pmux tmux server", &output));
        }
        return Ok(RestartResult::HardRestarted);
    }

    if !pmux_server_running()? {
        return Ok(RestartResult::SoftNoServer);
    }

    hydrate_session_launch_command_options()?;
    crate::tmux::publish_pmux_bin_best_effort();
    crate::tmux::guard_server_path_best_effort();
    let conf = server_conf_path()?;
    let status = tmux_cmd()
        .arg("source-file")
        .arg(&conf)
        .status()
        .context("failed to execute tmux source-file")?;
    if !status.success() {
        bail!("failed to reload pmux tmux config");
    }
    ensure_native_session_label_format_best_effort();
    Ok(RestartResult::SoftReloaded)
}

fn rollback_spawn(session: &str) -> Result<()> {
    let output = tmux_cmd()
        .arg("kill-session")
        .arg("-t")
        .arg(exact_session_target(session))
        .output()
        .context("failed to execute tmux kill-session during spawn rollback")?;
    let metadata_result = remove_session_metadata(session);
    if !output.status.success() {
        return Err(tmux_failure(
            "failed to kill partially-created pmux process",
            &output,
        ));
    }
    metadata_result
}

// Find the next session to switch to after killing the current one (creation order).
fn replacement_after_kill(sessions: &[SessionInfo], current_name: &str) -> Option<String> {
    let ordered = ordered_creation_sessions(sessions.to_vec());
    if ordered.len() <= 1 {
        return None;
    }

    let idx = ordered.iter().position(|s| s.name == current_name)?;
    let len = ordered.len();
    let next_idx = (idx + 1) % len;
    Some(ordered[next_idx].name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionInfo;
    use crate::state::{read_session_launch_command, write_session_launch_command};

    use crate::commands::test_support::with_temp_home;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn rename_transaction_keeps_metadata_when_tmux_rename_fails() {
        with_temp_home(|_| {
            write_session_launch_command("old", "codex").unwrap();

            let result = rename_session_transaction(
                "old",
                "new",
                || Err(anyhow::anyhow!("simulated tmux failure")),
                || panic!("rollback must not run when the initial rename fails"),
            );

            assert!(result.is_err());
            assert_eq!(
                read_session_launch_command("old").unwrap().as_deref(),
                Some("codex")
            );
            assert!(read_session_launch_command("new").unwrap().is_none());
        });
    }

    #[test]
    fn rename_transaction_rolls_back_tmux_after_metadata_failure() {
        with_temp_home(|_| {
            write_session_launch_command("old", "codex").unwrap();
            let rollback_called = std::cell::Cell::new(false);

            let result = rename_session_transaction(
                "old",
                "new",
                || {
                    // Simulate a destination appearing after preflight but before
                    // metadata migration.
                    crate::state::write_session_launch_cwd("new", "/stale")?;
                    Ok(())
                },
                || {
                    rollback_called.set(true);
                    Ok(())
                },
            );

            assert!(result.is_err());
            assert!(rollback_called.get());
            assert_eq!(
                read_session_launch_command("old").unwrap().as_deref(),
                Some("codex")
            );
            assert!(read_session_launch_command("new").unwrap().is_none());
        });
    }

    #[test]
    fn completion_capture_is_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let completion = CompletionCapture {
            output_path: dir.path().join("output"),
            status_path: dir.path().join("status"),
        };
        fs::write(&completion.output_path, b"captured output\n").unwrap();
        fs::write(&completion.status_path, "7\n").unwrap();

        let result = completion.take().unwrap().expect("completed process");
        assert_eq!(result.output, b"captured output\n");
        assert_eq!(result.exit_code, 7);
        assert!(!completion.output_path.exists());
        assert!(!completion.status_path.exists());
        assert!(completion.take().unwrap().is_none());
    }

    #[test]
    fn completion_supervisor_preserves_command_arguments() {
        let completion = CompletionCapture {
            output_path: PathBuf::from("/tmp/pmux completion/output"),
            status_path: PathBuf::from("/tmp/pmux completion/status"),
        };
        let command = completion_supervisor_command(
            &[
                "claude".to_string(),
                "--name=review".to_string(),
                "fix the tests".to_string(),
            ],
            &completion,
        )
        .unwrap();

        assert!(command.contains("pmux-completion-capture"));
        assert!(command.contains("capture-pane -p -e -S -"));
        assert!(command.contains("--name=review"));
        assert!(command.contains("fix the tests"));
    }

    // Helper to build a minimal SessionInfo with an attached client for tests.
    fn sess(name: &str, created: i64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pane_id: format!("%{}", name),
            created_epoch: created,
            last_activity_epoch: created,
            current_command: "sh".to_string(),
            start_command: None,
            current_path: None,
            launch_command: None,
            launch_cwd: None,
            attached_clients: 1,
        }
    }

    // Tests that replacement picks the next session in creation order.
    #[test]
    fn replacement_after_kill_uses_next_creation_order() {
        let sessions = vec![sess("alpha", 10), sess("beta", 20), sess("gamma", 30)];
        assert_eq!(
            replacement_after_kill(&sessions, "beta").as_deref(),
            Some("gamma")
        );
    }

    // Tests that replacement wraps around from the last to the first session.
    #[test]
    fn replacement_after_kill_wraps_around() {
        let sessions = vec![sess("alpha", 10), sess("beta", 20), sess("gamma", 30)];
        assert_eq!(
            replacement_after_kill(&sessions, "gamma").as_deref(),
            Some("alpha")
        );
    }

    // Tests that replacement returns None when only one session exists.
    #[test]
    fn replacement_after_kill_none_for_last_session() {
        let sessions = vec![sess("solo", 10)];
        assert!(replacement_after_kill(&sessions, "solo").is_none());
    }

    #[test]
    fn completion_replay_discards_blank_viewport_rows() {
        let captured = b"\r\n   \n\x1b[0m\nclaude 1.2.3\n\n\x1b[0m\n";
        assert_eq!(super::trim_blank_terminal_rows(captured), b"claude 1.2.3\n");
    }

    #[test]
    fn completion_replay_preserves_nonblank_ansi_output() {
        let captured = b"\x1b[32mready\x1b[0m\n\n";
        assert_eq!(
            super::trim_blank_terminal_rows(captured),
            b"\x1b[32mready\x1b[0m\n"
        );
    }
}
