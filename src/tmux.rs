use crate::config::{ensure_pmux_config, server_conf_path};
use crate::session::{SessionInfo, session_from_columns};
use crate::state::{read_session_launch_command, read_session_launch_cwd};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Default tmux socket name for the pmux backend server.
pub const PMUX_SOCKET: &str = "pmux";

/// Environment variable that overrides the pmux tmux socket name.
pub const PMUX_SOCKET_ENV: &str = "PMUX_SOCKET";

/// Environment variable through which the backend's hooks and key bindings
/// find pmux. Generated templates run `"${PMUX_BIN:-pmux}"`, so they work
/// wherever pmux is installed (`cargo install`, Homebrew, `PMUX_BIN_DIR`).
const PMUX_BIN_ENV: &str = "PMUX_BIN";

// The running binary, as an absolute path tmux jobs can execute.
fn pmux_bin_value() -> Option<String> {
    crate::util::resolve_pmux_binary()
        .ok()
        .filter(|path| path.is_absolute())
        .and_then(|path| path.to_str().map(str::to_string))
}

// `session_activity` changes for client focus and attach operations. Use the
// pane's output-activity timestamp so sorting reflects process work instead.
//
// tmux prints `pane_current_path` raw, and a directory name may contain tabs
// or newlines. The path therefore comes last (so `splitn` keeps its tabs) and
// every record ends with a `\tpmux:eor` marker before tmux's newline (so a
// newline inside the path does not split the record). tmux escapes control
// characters in the format string itself, so the marker must be printable.
// `pane_start_command` is already quoted and escaped by tmux.
const SESSION_LIST_FORMAT: &str = "#{session_name}\t#{session_created}\t#{pane_activity}\t#{pane_current_command}\t#{pane_start_command}\t#{session_attached}\t#{pane_id}\t#{pane_active}\t#{pane_current_path}\tpmux:eor";

// Terminates each SESSION_LIST_FORMAT record.
const SESSION_RECORD_END: &str = "\tpmux:eor\n";

/// The tmux socket name pmux uses for its backend server.
///
/// Overridable via the `PMUX_SOCKET` environment variable so tests and manual
/// smoke runs can target a throwaway socket (e.g. `pmux-test`) that never
/// touches a user's live `pmux` server. Empty/whitespace values are ignored
/// and fall back to [`PMUX_SOCKET`].
pub(crate) fn pmux_socket() -> String {
    match env::var(PMUX_SOCKET_ENV) {
        Ok(val) if !val.trim().is_empty() => val.trim().to_string(),
        _ => PMUX_SOCKET.to_string(),
    }
}

/// Escape a literal value for a tmux argument that tmux expands as a format.
///
/// tmux runs some arguments through its format expander before using them,
/// even when they were passed as a separate argv entry or single-quoted in a
/// command string. Expansion turns `#{...}` into variables and runs `#(...)`
/// as a shell command, so an unescaped directory such as
/// `/tmp/x#(touch /tmp/pwned)` executes code. Doubling every `#` makes tmux
/// read the value back literally: `##` expands to a single `#`, so a directory
/// that really contains `#` still works.
///
/// Arguments verified to expand on tmux 3.5a (escape user data in these):
/// - `new-session -s`, `-n`, `-c`; `new-window -n`, `-c`; `split-window -c`
/// - the new name given to `rename-session`
/// - `run-shell` shell commands
/// - `display-popup -d` and `display-message` formats
/// - `display-menu` item labels and commands, `confirm-before` prompts and
///   commands, so a `run-shell` inside a menu item or `confirm-before` is
///   expanded once per layer and must be escaped once per layer
///
/// Arguments that are NOT expanded (and so must NOT be escaped, or the `##`
/// would reach the program): `set-option`/`set-environment` values,
/// `new-session -e`, `send-keys`, `set-buffer`, targets given with `-t`, and
/// the shell command of `new-session`, `new-window`, `split-window` and
/// `display-popup -E`.
pub(crate) fn tmux_format_escape(value: &str) -> String {
    value.replace('#', "##")
}

/// [`tmux_format_escape`] for a path, preserving non-UTF-8 bytes.
pub(crate) fn tmux_format_escape_path(path: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut escaped = Vec::with_capacity(path.as_os_str().len());
    for &byte in path.as_os_str().as_bytes() {
        if byte == b'#' {
            escaped.push(b'#');
        }
        escaped.push(byte);
    }
    std::ffi::OsString::from_vec(escaped)
}

/// A `tmux` client command that always writes its output as UTF-8.
///
/// Without a UTF-8 locale (no `LANG`: boot services, cron, `env -i`, some
/// SSH sessions) tmux replaces tabs and other non-printable characters in
/// `-F` output with `_`, so every tab-separated listing pmux parses would come
/// back empty and pmux would report no processes. `-u` tells tmux to assume
/// UTF-8 regardless of the environment. Every tmux invocation goes through
/// here.
pub(crate) fn tmux_command() -> Command {
    let mut cmd = Command::new("tmux");
    cmd.arg("-u");
    cmd
}

// Build a `tmux` Command pre-configured with the pmux socket and config file.
// All tmux interactions in pmux go through this helper.
pub(crate) fn tmux_cmd() -> Command {
    let mut cmd = crate::tmux::tmux_command();
    let explicit_socket = env::var(PMUX_SOCKET_ENV).is_ok_and(|value| !value.trim().is_empty());
    cmd.arg("-L").arg(pmux_socket());
    // A server started by this command inherits PMUX_BIN in its global
    // environment; sync_environment_to_server refreshes running servers.
    if let Some(bin) = pmux_bin_value() {
        cmd.env(PMUX_BIN_ENV, bin);
    }
    if explicit_socket {
        // An inherited TMUX value can make tmux reconnect to the caller's
        // current server and ignore a test/development socket override.
        cmd.env_remove("TMUX");
    }
    if let Ok(conf) = server_conf_path() {
        cmd.arg("-f").arg(conf);
    }
    cmd
}

const TMUX_REQUIREMENT: &str = "pmux requires tmux 3.3a or newer";

/// Set to `1` to skip the `tmux -V` version check (for unusual builds).
const SKIP_TMUX_CHECK_ENV: &str = "PMUX_SKIP_TMUX_CHECK";

fn tmux_missing_error() -> anyhow::Error {
    anyhow!("tmux not found on PATH; {TMUX_REQUIREMENT}")
}

fn is_not_found(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::NotFound
}

/// Replace the bare "No such file or directory" a failed tmux spawn produces
/// with an actionable message when tmux itself is missing. Other errors,
/// including unrelated missing files, pass through unchanged.
pub(crate) fn explain_tmux_failure(err: anyhow::Error) -> anyhow::Error {
    explain_tmux_failure_with(err, || {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .is_err_and(|spawn_err| is_not_found(&spawn_err))
    })
}

// `tmux_missing` is injected so tests need not empty PATH, which would break
// every other test spawning a command at the same time.
fn explain_tmux_failure_with(
    err: anyhow::Error,
    tmux_missing: impl FnOnce() -> bool,
) -> anyhow::Error {
    let has_not_found = err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(is_not_found)
    });
    if has_not_found && tmux_missing() {
        return tmux_missing_error();
    }
    err
}

/// tmux 3.4 alone stores `$` in option values as `\$`, and prints it that way
/// from `show-options` and `show-environment` too (3.3a and 3.5 keep `$`;
/// the environment a pane receives is right on every version). Undo that
/// when reading user text back from a 3.4 server. 3.4 leaves backslashes
/// alone otherwise, so only a literally typed `\$` is ambiguous.
pub(crate) fn undo_tmux_3_4_dollar_escape(value: &str, version: Option<(u32, u32, u32)>) -> String {
    if matches!(version, Some((3, 4, _))) {
        value.replace("\\$", "$")
    } else {
        value.to_string()
    }
}

// A tmux version as (major, minor, letter), where "3.3a" is (3, 3, 1) and a
// plain "3.3" or a pre-release like "3.5-rc" has letter 0.
pub(crate) fn parse_tmux_version(output: &str) -> Option<(u32, u32, u32)> {
    let version = output.trim().strip_prefix("tmux ").unwrap_or(output.trim());
    // Development builds report "next-3.6"; skip any such prefix.
    let start = version.find(|c: char| c.is_ascii_digit())?;
    let version = &version[start..];
    let (major, rest) = version.split_once('.')?;
    let major = major.parse().ok()?;
    let minor_len = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let minor = rest[..minor_len].parse().ok()?;
    let letter = rest[minor_len..]
        .chars()
        .next()
        .filter(char::is_ascii_lowercase)
        .map_or(0, |c| c as u32 - 'a' as u32 + 1);
    Some((major, minor, letter))
}

fn check_tmux_version() -> Result<(), String> {
    let output = match crate::tmux::tmux_command().arg("-V").output() {
        Ok(output) => output,
        Err(err) if is_not_found(&err) => return Err(tmux_missing_error().to_string()),
        Err(err) => return Err(format!("failed to run tmux -V: {err}")),
    };
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match parse_tmux_version(&reported) {
        Some(version) if version < (3, 3, 1) => Err(format!(
            "{TMUX_REQUIREMENT}, but found {reported}; upgrade tmux or set \
             {SKIP_TMUX_CHECK_ENV}=1 to skip this check"
        )),
        // An unrecognized version string (custom builds) is not an error.
        _ => Ok(()),
    }
}

/// Verify once per process that tmux is installed and new enough. Called
/// right before pmux may start its tmux server.
pub(crate) fn ensure_tmux_supported() -> Result<()> {
    if env::var(SKIP_TMUX_CHECK_ENV).is_ok_and(|value| value.trim() == "1") {
        return Ok(());
    }
    static CHECK: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    CHECK
        .get_or_init(check_tmux_version)
        .clone()
        .map_err(|message| anyhow!(message))
}

/// Environment variables that should NOT be forwarded to the pmux tmux
/// server because they are tmux-internal or shell-session-specific.
const ENV_SYNC_SKIP: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "TERM_SESSION_ID",
    "SHLVL",
    "OLDPWD",
    "_",
    // The caller's working directory and terminal size describe that one
    // shell, not the sessions created later on the shared server.
    "PWD",
    "COLUMNS",
    "LINES",
    // Agent harnesses (Claude Code, etc.) set NO_COLOR=1 for their own child
    // processes. Syncing it to the server's global environment makes every
    // pane created afterwards inherit it, so agent TUIs render without color
    // until the variable is unset by hand.
    "NO_COLOR",
    // Loader and shell-startup hooks. The server's global environment is
    // inherited by every later session, so syncing these would let any one
    // caller (or one project's direnv) plant code that runs everywhere.
    "BASH_ENV",
    "ENV",
    "PROMPT_COMMAND",
    "PS4",
    "SHELLOPTS",
    "BASHOPTS",
    "IFS",
    // Codex sign-in flows may set this to a temporary credential directory.
    // Propagating it globally breaks unrelated panes after the directory is removed.
    "CODEX_HOME",
];

/// Variables an agent harness sets for its own child processes. When pmux
/// runs from inside an agent (Claude Code running `pmux run`, a Codex tool
/// call), these describe that one agent process: its config directory, session
/// id, messaging socket and token, effort level. Synced to the server's global
/// environment, every pane created afterwards would believe it is a child of
/// that agent, load its config directory (and so skip the user's own hooks),
/// and talk to its socket. User-set agent configuration such as
/// `CLAUDE_CODE_USE_BEDROCK` or `CODEX_API_KEY` is not listed and still syncs:
/// pmux sessions start under a non-interactive shell, so the server's
/// environment is the only way that configuration reaches them.
const HARNESS_ENV: &[&str] = &[
    // Claude Code
    "CLAUDECODE",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_ENV_FILE",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "AI_AGENT",
    // Claude Code pins corepack off for the commands it runs.
    "COREPACK_ENABLE_AUTO_PIN",
    // Codex
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_TOOL_CALL_ID",
];

/// Prefixes of harness variables that are skipped as a whole family. Cortex
/// Code sets many `CORTEX_*` variables for its children (session, thread, and
/// task ids, client surface, sandbox and agent flags). Leaked into the
/// server's global environment, `CORTEX_SESSION_ID` makes ai-attn's hooks
/// label every later agent, even a plain `claude`, as `cortex`, so pmux's
/// relay and sidebar no longer match its attention state.
const HARNESS_ENV_PREFIXES: &[&str] = &["CORTEX_"];

/// True for a variable an agent harness set for its own children (see
/// [`HARNESS_ENV`]). `GIT_EDITOR=true` is how Claude Code stops git from
/// opening an editor in its tool calls; inherited by a pane, it makes
/// `git commit` without `-m` abort. A real editor in `GIT_EDITOR` still syncs.
fn is_harness_env(key: &str, value: &str) -> bool {
    HARNESS_ENV.contains(&key)
        || HARNESS_ENV_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        || (key == "GIT_EDITOR" && value.trim() == "true")
}

/// Harness variables already in the server's global environment, from
/// `tmux show-environment -g` output, so a sync can remove what an older pmux
/// leaked there. Lines are `KEY=VALUE`, or `-KEY` for an unset marker.
fn stale_harness_keys(show_environment: &str) -> Vec<String> {
    show_environment
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, value)| is_valid_env_key(key) && is_harness_env(key, value))
        .map(|(key, _)| key.to_string())
        .collect()
}

/// Prefixes of dynamic-loader variables (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
/// `DYLD_INSERT_LIBRARIES`, ...) that are never forwarded to the server.
const ENV_SYNC_SKIP_PREFIXES: &[&str] = &["LD_", "DYLD_"];

/// True for a portable shell identifier (`^[A-Za-z_][A-Za-z0-9_]*$`). Keys are
/// written unquoted into a sourced tmux script, so anything else (spaces,
/// `;`, quotes, newlines) could be parsed as extra tmux commands.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Returns true if the given environment variable should be synced to the
/// pmux tmux server.
fn should_sync_env(key: &str) -> bool {
    is_valid_env_key(key)
        && !ENV_SYNC_SKIP.contains(&key)
        && !ENV_SYNC_SKIP_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// `path` with pmux's tmux guard directory first, so a bare `tmux` run by a
/// process in a pmux pane reaches the guard before the real tmux. The guard
/// directory appears once, whatever the caller's PATH already held.
pub(crate) fn guard_path(path: &str) -> String {
    let Ok(guard) = crate::config::tmux_guard_dir() else {
        return path.to_string();
    };
    let guard = guard.display().to_string();
    std::iter::once(guard.as_str())
        .chain(
            path.split(':')
                .filter(|dir| *dir != guard && !dir.is_empty()),
        )
        .collect::<Vec<_>>()
        .join(":")
}

/// The caller's PATH with the guard first, for a command that may start
/// pmux's server: a new server takes its environment from that command.
pub(crate) fn guarded_path_env() -> String {
    guard_path(&env::var("PATH").unwrap_or_default())
}

/// Put the tmux guard first on the running server's own PATH, leaving the
/// rest of its environment alone, so processes started from then on get it.
pub(crate) fn guard_server_path_best_effort() {
    let Ok(output) = tmux_cmd().args(["show-environment", "-g", "PATH"]).output() else {
        return;
    };
    let current = String::from_utf8_lossy(&output.stdout);
    let Some(path) = current.trim().strip_prefix("PATH=") else {
        return;
    };
    let guarded = guard_path(path);
    if guarded != path {
        let _ = tmux_cmd()
            .args(["set-environment", "-g", "PATH", &guarded])
            .output();
    }
}

/// Collect the environment variables that should be synced to the tmux server.
/// Returns a vec of (key, value) pairs filtered through the skip list.
fn collect_sync_env() -> Vec<(String, String)> {
    // An inherited PMUX_BIN may name an older install; always send our own.
    // `vars_os` (unlike `vars`) does not panic on non-UTF-8 entries; those
    // are skipped rather than lossily rewritten.
    let mut vars: Vec<_> = env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, v)| should_sync_env(k) && k != PMUX_BIN_ENV && !is_harness_env(k, v))
        .map(|(k, v)| {
            let v = if k == "PATH" { guard_path(&v) } else { v };
            (k, v)
        })
        .collect();
    if let Some(bin) = pmux_bin_value() {
        vars.push((PMUX_BIN_ENV.to_string(), bin));
    }
    vars
}

/// Quote `value` as one word of tmux's command language (config files,
/// `source-file`, hook and binding command strings) so tmux's parser hands it
/// back unchanged: a double-quoted word with `\`, `"` and `$` (the only
/// characters tmux treats specially inside double quotes) backslash-escaped.
///
/// This removes exactly one layer of tmux quoting. Format expansion (see
/// [`tmux_format_escape`]) and any shell quoting are separate layers.
pub(crate) fn tmux_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' || ch == '$' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Render the tmux script that `sync_environment_to_server` sources: one
/// `set-environment -g` command per line. Keys that are not plain
/// identifiers are dropped here too, so no key can ever add a command.
fn render_env_sync_script(vars: &[(String, String)]) -> String {
    let mut script = String::new();
    for (key, value) in vars {
        if !is_valid_env_key(key) {
            continue;
        }
        script.push_str("set-environment -g ");
        script.push_str(key);
        script.push(' ');
        script.push_str(&tmux_quote(value));
        script.push('\n');
    }
    script
}

/// Render `set-environment -gu` lines removing `keys` from the server's
/// global environment, with the same key check as [`render_env_sync_script`].
fn render_env_unset_script(keys: &[String]) -> String {
    let mut script = String::new();
    for key in keys.iter().filter(|key| is_valid_env_key(key)) {
        script.push_str("set-environment -gu ");
        script.push_str(key);
        script.push('\n');
    }
    script
}

/// Point an already-running server's hooks and bindings at this binary.
pub(crate) fn publish_pmux_bin_best_effort() {
    if let Some(bin) = pmux_bin_value() {
        let _ = tmux_cmd()
            .args(["set-environment", "-g", PMUX_BIN_ENV, &bin])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

// Forward filtered environment variables to the pmux tmux server via source-file.
// Called before spawning new sessions to propagate API keys, PATH, etc.
pub(crate) fn sync_environment_to_server() {
    // A new tmux server inherits this process's environment directly. Trying
    // to source an env file before it exists only emits a misleading warning.
    if !pmux_server_running().unwrap_or(false) {
        return;
    }
    let vars = collect_sync_env();
    // Remove agent-harness variables an older pmux leaked into the server, so
    // panes created from now on stop inheriting them.
    let stale = tmux_cmd()
        .args(["show-environment", "-g"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| stale_harness_keys(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    if vars.is_empty() && stale.is_empty() {
        return;
    }

    let mut tmpfile = match tempfile::NamedTempFile::new() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("pmux: failed to create temp file for env sync: {e}");
            return;
        }
    };

    let script = render_env_unset_script(&stale) + &render_env_sync_script(&vars);
    if let Err(e) = tmpfile.write_all(script.as_bytes()) {
        eprintln!("pmux: failed to write env sync file: {e}");
        return;
    }

    let path = tmpfile.path().to_path_buf();
    let status = tmux_cmd()
        .args(["source-file", &path.display().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status {
        Ok(s) if !s.success() => {
            eprintln!("pmux: tmux source-file failed during env sync");
        }
        Err(e) => {
            eprintln!("pmux: failed to run tmux source-file for env sync: {e}");
        }
        _ => {}
    }
}

// Check whether the pmux tmux server is currently running.
pub(crate) fn pmux_server_running() -> Result<bool> {
    let output = tmux_cmd()
        .arg("list-sessions")
        .stdout(Stdio::null())
        .output()
        .context("failed to query pmux tmux server")?;
    if output.status.success() {
        return Ok(true);
    }
    if output_indicates_missing_server(&output.stderr) {
        return Ok(false);
    }
    Err(tmux_failure("failed to query pmux tmux server", &output))
}

pub(crate) fn output_indicates_missing_server(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("no server running")
        || (stderr.contains("error connecting to") && stderr.contains("no such file or directory"))
        // `list-panes -a` reports this when the final session has exited and
        // tmux has no server target left to inspect.
        || stderr.contains("no current target")
}

pub(crate) fn tmux_failure(context: &str, output: &std::process::Output) -> anyhow::Error {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        anyhow!("{context}: tmux exited with {}", output.status)
    } else {
        anyhow!("{context}: {detail}")
    }
}

/// List all sessions on the pmux tmux server, enriched with persisted launch metadata.
/// Deduplicates panes to return one entry per session.
pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    ensure_pmux_config()?;
    list_sessions_with_command(tmux_cmd())
}

fn list_sessions_with_command(mut command: Command) -> Result<Vec<SessionInfo>> {
    let out = command
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg(SESSION_LIST_FORMAT)
        .output()
        .context("failed to run tmux list-panes")?;

    if !out.status.success() {
        if output_indicates_missing_server(&out.stderr) {
            return Ok(Vec::new());
        }
        return Err(tmux_failure("failed to list pmux processes", &out));
    }

    let raw = String::from_utf8_lossy(&out.stdout);
    let mut sessions = parse_session_records(&raw)?;
    for session in &mut sessions {
        session.launch_command = read_session_launch_command(&session.name)?;
        session.launch_cwd = read_session_launch_cwd(&session.name)?;
    }
    // Note: cleanup_stale_session_metadata is intentionally NOT called here.
    // It runs at explicit lifecycle points (spawn, kill) to avoid redundant
    // filesystem walks on every list_sessions() call.
    Ok(sessions)
}

// Parse SESSION_LIST_FORMAT output. list-panes -a emits one record per pane;
// order them by session name with each session's active pane first.
fn parse_session_records(raw: &str) -> Result<Vec<SessionInfo>> {
    let raw = raw.strip_suffix(SESSION_RECORD_END).unwrap_or(raw);
    let mut records: Vec<(&str, bool, Vec<&str>)> = Vec::new();
    for record in raw.split(SESSION_RECORD_END) {
        let cols: Vec<&str> = record.splitn(9, '\t').collect();
        if cols.len() != 9 {
            continue;
        }
        let active = cols[7] == "1";
        // Reorder into session_from_columns' column layout.
        let ordered = vec![
            cols[0], cols[1], cols[2], cols[3], cols[4], cols[8], cols[5], cols[6],
        ];
        records.push((cols[0], active, ordered));
    }
    records.sort_by(|a, b| a.0.cmp(b.0).then_with(|| b.1.cmp(&a.1)));
    records
        .into_iter()
        .map(|(name, _, cols)| session_from_columns(name, &cols))
        .collect()
}

/// Return the creation epoch of the live session named exactly `name`, or
/// `None` when no such session (or no server) exists.
pub(crate) fn session_created_epoch(name: &str) -> Result<Option<i64>> {
    // display-message prints an empty line (and succeeds) for a missing
    // target, so match the name exactly in a session listing instead.
    let out = tmux_cmd()
        .args(["list-sessions", "-F", "#{session_created}\t#{session_name}"])
        .output()
        .context("failed to query pmux session creation time")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let (epoch, session) = line.split_once('\t')?;
            (session == name).then(|| epoch.parse().ok()).flatten()
        }))
}

/// Return the pmux session of the tmux client running this process (for
/// example a `display-popup` opened from a pmux binding).
pub fn current_session_name() -> Result<String> {
    detect_current_pmux_session(&list_sessions()?)
}

/// Resolve the session that owns a pmux pane, e.g. `%12`.
pub(crate) fn session_name_for_pane(pane: &str) -> Result<String> {
    let out = tmux_cmd()
        .args(["display-message", "-p", "-t", pane, "#{session_name}"])
        .output()
        .context("failed to query pmux pane")?;
    if !out.status.success() {
        bail!("no pmux pane {pane}");
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

// Query the pmux tmux server for the session name of the current client.
// Returns an error if not inside a pmux-managed session.
pub(crate) fn detect_current_pmux_session(known: &[SessionInfo]) -> Result<String> {
    let out = tmux_cmd()
        .arg("display-message")
        .arg("-p")
        .arg("#{session_name}")
        .output()
        .context("failed to query current pmux session")?;
    if !out.status.success() {
        bail!("not running inside a tmux client");
    }

    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if known.iter().any(|s| s.name == name) {
        Ok(name)
    } else {
        bail!("current tmux session is not a pmux-managed process")
    }
}

// Check whether the TMUX env var points to the pmux socket.
// Returns None if TMUX is not set.
fn tmux_socket_is_pmux() -> Option<bool> {
    let tmux = env::var_os("TMUX")?;
    let tmux = tmux.to_string_lossy();
    let socket_path = tmux.split(',').next()?;
    let socket = pmux_socket();
    Some(Path::new(socket_path).file_name().and_then(|v| v.to_str()) == Some(socket.as_str()))
}

// Returns true if the current process is running inside a pmux tmux client.
pub(crate) fn inside_pmux_client() -> bool {
    tmux_socket_is_pmux() == Some(true)
}

// Returns true if running inside a non-pmux (outer) tmux session.
pub(crate) fn inside_outer_tmux() -> bool {
    tmux_socket_is_pmux() == Some(false)
}

// Return the tmux server socket path from $TMUX (its first comma-separated
// field), or None when not running inside tmux. Used to stamp ai-attn relay
// records with the outer server that owns the host pane so its pane ID isn't
// matched against an unrelated pane on a different server.
pub(crate) fn current_tmux_socket() -> Option<String> {
    let tmux = env::var_os("TMUX")?;
    let socket_path = tmux.to_string_lossy().split(',').next()?.to_string();
    if socket_path.is_empty() {
        None
    } else {
        Some(socket_path)
    }
}

/// A throwaway tmux server on a private socket for tests that must observe
/// real tmux parsing and format expansion. It never touches the user's
/// servers and is killed on drop.
#[cfg(test)]
pub(crate) struct TestTmuxServer {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) socket: std::path::PathBuf,
}

#[cfg(test)]
impl TestTmuxServer {
    /// Start a detached server, or `None` when tmux is not installed.
    pub(crate) fn start() -> Option<Self> {
        crate::tmux::tmux_command().arg("-V").output().ok()?;
        let dir = tempfile::Builder::new()
            .prefix("pmux-t-")
            .tempdir_in("/tmp")
            .ok()?;
        let socket = dir.path().join("sock");
        let server = Self { dir, socket };
        let output = server.run(&["new-session", "-d", "-s", "base", "-x", "120", "-y", "40"]);
        assert!(output.status.success(), "start throwaway tmux server");
        Some(server)
    }

    pub(crate) fn command(&self) -> Command {
        let mut cmd = crate::tmux::tmux_command();
        cmd.arg("-S")
            .arg(&self.socket)
            .arg("-f")
            .arg("/dev/null")
            .env_remove("TMUX");
        cmd
    }

    pub(crate) fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().expect("run tmux")
    }

    /// Feed tmux command-language text to the server as `source-file -`.
    pub(crate) fn source(&self, text: &str) -> std::process::Output {
        let mut child = self
            .command()
            .args(["source-file", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tmux source-file");
        child
            .stdin
            .take()
            .expect("source-file stdin")
            .write_all(text.as_bytes())
            .expect("write source-file input");
        child.wait_with_output().expect("wait for source-file")
    }

    /// Write an executable script into a directory whose name contains every
    /// character with meaning to tmux or the shell, logging its arguments one
    /// per line. Returns the script path and its log path.
    pub(crate) fn hostile_binary(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = self.dir.path().join("args.log");
        let marker = self.dir.path().join("pwned");
        let bin_dir = self.dir.path().join(format!(
            "we'ird \"dir\" $HOME `id` \\ #{{host}} #(touch {})",
            marker.display()
        ));
        std::fs::create_dir_all(&bin_dir).expect("create hostile dir");
        let bin = bin_dir.join("pmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> '{}'\n",
                log.display()
            ),
        )
        .expect("write fake pmux");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("make fake pmux executable");
        (bin, log)
    }

    /// The marker file a `#(...)` in [`Self::hostile_binary`]'s path creates
    /// if tmux ever expands it.
    pub(crate) fn hostile_marker(&self) -> std::path::PathBuf {
        self.dir.path().join("pwned")
    }

    /// Wait up to three seconds for `path` to contain `needle`.
    pub(crate) fn wait_for(&self, path: &Path, needle: &str) -> String {
        for _ in 0..60 {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if content.contains(needle) {
                return content;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::read_to_string(path).unwrap_or_default()
    }
}

#[cfg(test)]
impl Drop for TestTmuxServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

#[cfg(test)]
mod tests {

    /// Version of the tmux binary the real-server tests run.
    fn test_tmux_version() -> Option<(u32, u32, u32)> {
        let output = crate::tmux::tmux_command().arg("-V").output().ok()?;
        super::parse_tmux_version(&String::from_utf8_lossy(&output.stdout))
    }

    #[test]
    fn dollar_escape_is_undone_only_for_tmux_3_4() {
        use super::undo_tmux_3_4_dollar_escape;
        assert_eq!(
            undo_tmux_3_4_dollar_escape("a\\$HOME", Some((3, 4, 0))),
            "a$HOME"
        );
        assert_eq!(
            undo_tmux_3_4_dollar_escape("a\\$HOME", Some((3, 4, 1))),
            "a$HOME"
        );
        assert_eq!(
            undo_tmux_3_4_dollar_escape("a\\$HOME", Some((3, 5, 1))),
            "a\\$HOME"
        );
        assert_eq!(undo_tmux_3_4_dollar_escape("a\\$HOME", None), "a\\$HOME");
        assert_eq!(undo_tmux_3_4_dollar_escape("a\\b", Some((3, 4, 0))), "a\\b");
    }
    #[test]
    fn format_escape_doubles_every_hash() {
        use super::tmux_format_escape;
        assert_eq!(tmux_format_escape("plain/dir"), "plain/dir");
        assert_eq!(
            tmux_format_escape("/tmp/x#(touch /tmp/pwned)"),
            "/tmp/x##(touch /tmp/pwned)"
        );
        assert_eq!(tmux_format_escape("#{session_name}"), "##{session_name}");
        assert_eq!(tmux_format_escape("a##b"), "a####b");
        assert_eq!(tmux_format_escape(""), "");
    }

    #[test]
    fn escaped_values_reach_real_tmux_literally() {
        use super::{TestTmuxServer, tmux_format_escape, tmux_format_escape_path, tmux_quote};
        let Some(server) = TestTmuxServer::start() else {
            return;
        };
        let marker = server.hostile_marker();
        let dir = server
            .dir
            .path()
            .join(format!("a#b #{{host}} #(touch {})", marker.display()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = "n#{host}#(true)";
        let created = server
            .command()
            .args(["new-session", "-d", "-s"])
            .arg(tmux_format_escape(name))
            .arg("-c")
            .arg(tmux_format_escape_path(&dir))
            .output()
            .unwrap();
        assert!(created.status.success());
        let listed = server.run(&["list-sessions", "-F", "#{session_name}"]);
        assert!(String::from_utf8_lossy(&listed.stdout).contains(name));
        let path = server.run(&["display-message", "-p", "-t", "$1", "#{pane_current_path}"]);
        // tmux reports the resolved path (macOS's /tmp is /private/tmp).
        assert_eq!(
            String::from_utf8_lossy(&path.stdout).trim(),
            std::fs::canonicalize(&dir).unwrap().display().to_string()
        );

        let value = "it's \"$HOME\" \\ #x";
        let set = server.source(&format!(
            "set-option -g @pmux_quote_test {}",
            tmux_quote(value)
        ));
        assert!(set.status.success());
        let shown = server.run(&["show-option", "-gqv", "@pmux_quote_test"]);
        assert_eq!(
            super::undo_tmux_3_4_dollar_escape(
                String::from_utf8_lossy(&shown.stdout).trim_end(),
                test_tmux_version()
            ),
            value
        );

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!marker.exists(), "tmux expanded an escaped #(...)");
    }

    #[test]
    fn format_escape_path_keeps_non_utf8_bytes() {
        use super::tmux_format_escape_path;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let raw = std::ffi::OsString::from_vec(b"/tmp/\xff#(x)".to_vec());
        let escaped = tmux_format_escape_path(std::path::Path::new(&raw));
        assert_eq!(escaped.as_bytes(), b"/tmp/\xff##(x)");
    }

    #[test]
    fn session_listing_uses_pane_activity_not_client_focus_activity() {
        assert!(super::SESSION_LIST_FORMAT.contains("#{pane_activity}"));
        assert!(!super::SESSION_LIST_FORMAT.contains("#{session_activity}"));
    }

    #[test]
    fn tmux_version_parsing_accepts_suffixes_and_dev_builds() {
        use super::parse_tmux_version;
        assert_eq!(parse_tmux_version("tmux 3.3a"), Some((3, 3, 1)));
        assert_eq!(parse_tmux_version("tmux 3.4a\n"), Some((3, 4, 1)));
        assert_eq!(parse_tmux_version("tmux 3.5-rc"), Some((3, 5, 0)));
        assert_eq!(parse_tmux_version("tmux next-3.6"), Some((3, 6, 0)));
        assert_eq!(parse_tmux_version("tmux 3.3"), Some((3, 3, 0)));
        assert_eq!(parse_tmux_version("tmux 2.9a"), Some((2, 9, 1)));
        assert_eq!(parse_tmux_version("tmux 10.0"), Some((10, 0, 0)));
        assert_eq!(parse_tmux_version("tmux master"), None);
        assert!(parse_tmux_version("tmux 3.3a").unwrap() >= (3, 3, 1));
        assert!(parse_tmux_version("tmux 3.3").unwrap() < (3, 3, 1));
        assert!(parse_tmux_version("tmux 3.2a").unwrap() < (3, 3, 1));
    }

    #[test]
    fn missing_tmux_spawn_errors_are_explained() {
        use super::explain_tmux_failure_with;
        let not_found = || {
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
                .context("failed to run tmux list-panes")
        };
        assert_eq!(
            explain_tmux_failure_with(not_found(), || true).to_string(),
            "tmux not found on PATH; pmux requires tmux 3.3a or newer"
        );
        // A missing file other than tmux itself is left alone.
        assert_eq!(
            explain_tmux_failure_with(not_found(), || false).to_string(),
            "failed to run tmux list-panes"
        );
        // So are unrelated errors.
        let other = explain_tmux_failure_with(anyhow::anyhow!("tmux exploded"), || true);
        assert_eq!(other.to_string(), "tmux exploded");
    }

    // Regression: a tab or newline in a pane's cwd used to break the column
    // split, so the whole session vanished from listings.
    #[test]
    fn session_records_survive_tabs_and_newlines_in_cwd() {
        let raw = "odd\t100\t150\tsleep\t\"sleep 100\"\t0\t%1\t1\t/tmp/a\tb\nc\tpmux:eor\n\
                   plain\t200\t250\tzsh\tzsh\t1\t%2\t1\t/home/user\tpmux:eor\n";
        let sessions = super::parse_session_records(raw).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "odd");
        assert_eq!(sessions[0].current_path.as_deref(), Some("/tmp/a\tb\nc"));
        assert_eq!(sessions[0].start_command.as_deref(), Some("\"sleep 100\""));
        assert_eq!(sessions[0].pane_id, "%1");
        assert_eq!(sessions[1].name, "plain");
        assert_eq!(sessions[1].attached_clients, 1);
        assert_eq!(sessions[1].current_path.as_deref(), Some("/home/user"));
    }

    #[test]
    fn session_records_put_the_active_pane_first() {
        let raw = "s\t1\t1\tvim\tvim\t0\t%3\t0\t/a\tpmux:eor\ns\t1\t1\tzsh\tzsh\t0\t%4\t1\t/b\tpmux:eor\n";
        let sessions = super::parse_session_records(raw).unwrap();
        assert_eq!(sessions[0].pane_id, "%4");
        assert_eq!(sessions[1].pane_id, "%3");
        assert!(super::parse_session_records("").unwrap().is_empty());
    }

    #[test]
    fn test_explicit_socket_override_clears_inherited_tmux() {
        use super::{PMUX_SOCKET_ENV, tmux_cmd};
        let mut env = crate::EnvGuard::new();
        env.set(PMUX_SOCKET_ENV, "pmux-isolated-test");
        env.set("TMUX", "/tmp/tmux-1000/pmux,1,0");
        let command = tmux_cmd();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "TMUX" && value.is_none())
        );
    }

    #[test]
    fn test_missing_server_errors_are_distinguished_from_real_failures() {
        use super::output_indicates_missing_server;
        assert!(output_indicates_missing_server(
            b"no server running on /tmp/tmux-1000/pmux"
        ));
        assert!(output_indicates_missing_server(
            b"error connecting to /tmp/tmux-1000/pmux (No such file or directory)"
        ));
        assert!(output_indicates_missing_server(b"no current target"));
        assert!(!output_indicates_missing_server(
            b"error connecting to /tmp/tmux-1000/pmux (Operation not permitted)"
        ));
        assert!(!output_indicates_missing_server(b"tmux exploded"));
    }

    // --- pmux_socket ---

    // Tests that PMUX_SOCKET overrides the default socket, and blank/unset
    // values fall back to the "pmux" default.
    #[test]
    fn test_pmux_socket_env_override() {
        use super::{PMUX_SOCKET, PMUX_SOCKET_ENV, pmux_socket};
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Save the prior value so running the whole suite with PMUX_SOCKET
        // exported (to shield the live socket) is not disturbed by this test.
        let prior = std::env::var(PMUX_SOCKET_ENV).ok();

        #[allow(deprecated)]
        unsafe {
            std::env::set_var(PMUX_SOCKET_ENV, "pmux-unit-test");
        }
        assert_eq!(pmux_socket(), "pmux-unit-test");

        // Blank values fall back to the default.
        #[allow(deprecated)]
        unsafe {
            std::env::set_var(PMUX_SOCKET_ENV, "  ");
        }
        assert_eq!(pmux_socket(), PMUX_SOCKET);

        // Restore the prior value (or clear if there was none).
        #[allow(deprecated)]
        unsafe {
            match prior {
                Some(val) => std::env::set_var(PMUX_SOCKET_ENV, val),
                None => std::env::remove_var(PMUX_SOCKET_ENV),
            }
        }
    }

    // --- sync_environment_to_server ---

    // Tests that tmux-internal env vars are excluded from sync.
    #[test]
    fn test_should_sync_env_skips_tmux_internal_vars() {
        use super::should_sync_env;
        assert!(!should_sync_env("TMUX"));
        assert!(!should_sync_env("TMUX_PANE"));
        assert!(!should_sync_env("TERM_SESSION_ID"));
        assert!(!should_sync_env("SHLVL"));
        assert!(!should_sync_env("OLDPWD"));
        assert!(!should_sync_env("_"));
    }

    // Tests that NO_COLOR is not leaked into the server's global environment,
    // which would strip color from every pane created afterwards.
    #[test]
    fn test_should_sync_env_skips_no_color() {
        use super::should_sync_env;
        assert!(!should_sync_env("NO_COLOR"));
        // Neighbouring color vars are not tmux-internal and still sync.
        assert!(should_sync_env("CLICOLOR"));
        assert!(should_sync_env("COLORTERM"));
    }

    #[test]
    fn test_should_sync_env_skips_codex_home() {
        use super::should_sync_env;
        assert!(!should_sync_env("CODEX_HOME"));
    }

    // Tests that normal env vars (PATH, API keys, etc.) are included in sync.
    #[test]
    fn test_should_sync_env_allows_normal_vars() {
        use super::should_sync_env;
        assert!(should_sync_env("PATH"));
        assert!(should_sync_env("HOME"));
        assert!(should_sync_env("ANTHROPIC_API_KEY"));
        assert!(should_sync_env("OPENAI_API_KEY"));
        assert!(should_sync_env("EDITOR"));
    }

    // A caller's working directory and terminal size must not leak into the
    // shared server environment that every later session inherits.
    #[test]
    fn test_should_sync_env_skips_shell_and_terminal_specific_values() {
        use super::should_sync_env;
        assert!(!should_sync_env("PWD"));
        assert!(!should_sync_env("COLUMNS"));
        assert!(!should_sync_env("LINES"));
    }

    // Tests that a plain value is wrapped in double quotes.
    #[test]
    fn test_tmux_quote_plain_value() {
        use super::tmux_quote;
        assert_eq!(tmux_quote("hello"), r#""hello""#);
    }

    // Tests that quotes, backslashes, and dollar signs are escaped.
    #[test]
    fn test_tmux_quote_escapes_special_chars() {
        use super::tmux_quote;
        assert_eq!(tmux_quote(r#"a"b\c$d"#), r#""a\"b\\c\$d""#);
    }

    // Tests that an empty string is quoted as "".
    #[test]
    fn test_tmux_quote_empty() {
        use super::tmux_quote;
        assert_eq!(tmux_quote(""), r#""""#);
    }

    // Tests that collect_sync_env filters out skipped vars but includes normal ones.
    #[test]
    fn test_collect_sync_env_excludes_skipped_vars() {
        use super::collect_sync_env;
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        #[allow(deprecated)]
        unsafe {
            std::env::set_var("PMUX_TEST_SYNC", "yes");
            std::env::set_var("SHLVL", "99");
        }
        let collected = collect_sync_env();
        let keys: Vec<&str> = collected.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            keys.contains(&"PMUX_TEST_SYNC"),
            "normal vars should be collected"
        );
        assert!(!keys.contains(&"SHLVL"), "skipped vars should be excluded");
        #[allow(deprecated)]
        unsafe {
            std::env::remove_var("PMUX_TEST_SYNC");
        }
    }

    #[test]
    fn test_should_sync_env_rejects_non_identifier_keys() {
        use super::{is_valid_env_key, should_sync_env};
        for key in ["_", "A", "a_1", "PATH", "_PRIVATE"] {
            assert!(is_valid_env_key(key), "{key:?} should be valid");
        }
        for key in [
            "",
            "1ABC",
            "A B",
            "A;B",
            "A\nB",
            "A-B",
            "A=B",
            "A\"B",
            "X ; run-shell 'touch /tmp/pwn'",
            "ÄPFEL",
        ] {
            assert!(!is_valid_env_key(key), "{key:?} should be invalid");
            assert!(!should_sync_env(key), "{key:?} must not be synced");
        }
    }

    #[test]
    fn test_should_sync_env_skips_loader_and_shell_hook_vars() {
        use super::should_sync_env;
        for key in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "BASH_ENV",
            "ENV",
            "PROMPT_COMMAND",
            "PS4",
            "SHELLOPTS",
            "BASHOPTS",
            "IFS",
        ] {
            assert!(!should_sync_env(key), "{key:?} must not be synced");
        }
        // Prefix matching must not catch unrelated names.
        assert!(should_sync_env("LDFLAGS"));
        assert!(should_sync_env("ENVIRONMENT"));
    }

    #[test]
    fn test_harness_env_is_not_synced() {
        use super::{is_harness_env, should_sync_env};
        for key in [
            "CLAUDECODE",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "AI_AGENT",
            "COREPACK_ENABLE_AUTO_PIN",
            "CODEX_SANDBOX",
            "CODEX_TOOL_CALL_ID",
            "CORTEX_SESSION_ID",
            "CORTEX_CODE_CLIENT_SURFACE",
            "CORTEX_COCOBOX_SANDBOX_ENABLED",
        ] {
            assert!(should_sync_env(key), "{key:?} passes the key filter");
            assert!(is_harness_env(key, "x"), "{key:?} must not be synced");
        }
        assert!(is_harness_env("GIT_EDITOR", "true"));
        assert!(is_harness_env("GIT_EDITOR", " true\n"));
        // The user's own agent configuration and credentials still sync.
        for (key, value) in [
            ("GIT_EDITOR", "nvim"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "32000"),
            ("CODEX_API_KEY", "k"),
            ("ANTHROPIC_API_KEY", "k"),
            ("CORTEX", "x"),
        ] {
            assert!(!is_harness_env(key, value), "{key}={value} should sync");
        }
    }

    #[test]
    fn test_collect_sync_env_excludes_harness_env() {
        use super::collect_sync_env;
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = ["CLAUDE_CONFIG_DIR", "GIT_EDITOR"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        #[allow(deprecated)]
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/other-login");
            std::env::set_var("GIT_EDITOR", "true");
        }
        let collected = collect_sync_env();
        #[allow(deprecated)]
        unsafe {
            for (key, value) in saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        let keys: Vec<&str> = collected.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"CLAUDE_CONFIG_DIR"));
        assert!(!keys.contains(&"GIT_EDITOR"));
    }

    #[test]
    fn test_stale_harness_keys_finds_leaked_vars_only() {
        use super::stale_harness_keys;
        let shown = "PATH=/usr/bin\n\
                     CLAUDECODE=1\n\
                     -CLAUDE_PID\n\
                     CLAUDE_CONFIG_DIR=/tmp/other-login\n\
                     GIT_EDITOR=true\n\
                     CLAUDE_CODE_USE_BEDROCK=1\n\
                     CORTEX_SESSION_ID=d1a1fe7b\n\
                     TMUX=/tmp/sock,1,0\n";
        assert_eq!(
            stale_harness_keys(shown),
            vec![
                "CLAUDECODE",
                "CLAUDE_CONFIG_DIR",
                "GIT_EDITOR",
                "CORTEX_SESSION_ID"
            ]
        );
        assert!(stale_harness_keys("GIT_EDITOR=nvim\n").is_empty());
    }

    #[test]
    fn test_render_env_unset_script() {
        use super::render_env_unset_script;
        let script = render_env_unset_script(&[
            "CLAUDECODE".to_string(),
            "BAD KEY ; kill-server".to_string(),
        ]);
        assert_eq!(script, "set-environment -gu CLAUDECODE\n");
    }

    // Sources an unset script into a throwaway tmux server and proves the
    // leaked variable is gone while others stay.
    #[test]
    fn test_env_unset_script_removes_leaked_vars() {
        use super::render_env_unset_script;
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("pmux-envunset-")
            .tempdir_in("/tmp")
            .unwrap();
        let sock = dir.path().join("sock");
        let tmux = || {
            let mut cmd = crate::tmux::tmux_command();
            cmd.env_remove("TMUX")
                .arg("-S")
                .arg(&sock)
                .arg("-f")
                .arg("/dev/null");
            cmd
        };
        let started = tmux()
            .args(["new-session", "-d", "sleep 30"])
            .status()
            .is_ok_and(|s| s.success());
        if !started {
            return;
        }
        let _ = tmux()
            .args(["set-environment", "-g", "CLAUDECODE", "1"])
            .status();
        let _ = tmux()
            .args(["set-environment", "-g", "PMUX_T_KEEP", "1"])
            .status();
        let script_path = dir.path().join("unset.conf");
        std::fs::write(
            &script_path,
            render_env_unset_script(&["CLAUDECODE".to_string()]),
        )
        .unwrap();
        let sourced = tmux().arg("source-file").arg(&script_path).status();
        let has = |key: &str| {
            tmux()
                .args(["show-environment", "-g", key])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let leaked = has("CLAUDECODE");
        let kept = has("PMUX_T_KEEP");
        let _ = tmux().arg("kill-server").status();

        assert!(sourced.is_ok_and(|s| s.success()));
        assert!(!leaked, "the leaked variable should be removed");
        assert!(kept, "other variables must stay");
    }

    #[test]
    fn test_render_env_sync_script_drops_invalid_keys() {
        use super::render_env_sync_script;
        let script = render_env_sync_script(&[
            ("GOOD".to_string(), "v".to_string()),
            ("BAD KEY ; kill-server".to_string(), "x".to_string()),
        ]);
        assert_eq!(script, "set-environment -g GOOD \"v\"\n");
    }

    // Sources a rendered script into a throwaway tmux server and proves that
    // hostile values are stored verbatim and never run as tmux commands or
    // format jobs.
    #[test]
    fn test_env_sync_script_values_do_not_inject_commands() {
        use super::render_env_sync_script;
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("pmux-envsync-")
            .tempdir_in("/tmp")
            .unwrap();
        let sock = dir.path().join("sock");
        let marker = dir.path().join("pwned");
        let tmux = || {
            let mut cmd = crate::tmux::tmux_command();
            cmd.env_remove("TMUX")
                .arg("-S")
                .arg(&sock)
                .arg("-f")
                .arg("/dev/null");
            cmd
        };
        let values = [
            (
                "PMUX_T_NL",
                "a\nset-environment -g PMUX_T_INJECTED 1".to_string(),
            ),
            (
                "PMUX_T_FMT",
                format!("#(touch {}) #{{pane_id}}", marker.display()),
            ),
            (
                "PMUX_T_SEMI",
                "a ; set-environment -g PMUX_T_INJECTED 2".to_string(),
            ),
            (
                "PMUX_T_QUOTE",
                r#"a" ; set-environment -g PMUX_T_INJECTED 3 ; "$HOME\"#.to_string(),
            ),
        ];
        let vars: Vec<(String, String)> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let script_path = dir.path().join("env.conf");
        std::fs::write(&script_path, render_env_sync_script(&vars)).unwrap();

        let started = tmux()
            .args(["new-session", "-d", "sleep 30"])
            .status()
            .is_ok_and(|s| s.success());
        if !started {
            return;
        }
        let sourced = tmux().arg("source-file").arg(&script_path).status();
        let show = |key: &str| {
            let out = tmux()
                .args(["show-environment", "-g", key])
                .output()
                .unwrap();
            out.status.success().then(|| {
                super::undo_tmux_3_4_dollar_escape(
                    &String::from_utf8_lossy(&out.stdout),
                    test_tmux_version(),
                )
            })
        };
        let results: Vec<_> = values.iter().map(|(k, _)| show(k)).collect();
        let injected = show("PMUX_T_INJECTED");
        let _ = tmux().arg("kill-server").status();

        assert!(sourced.is_ok_and(|s| s.success()));
        for ((key, value), shown) in values.iter().zip(results) {
            assert_eq!(
                shown.as_deref(),
                Some(format!("{key}={value}\n").as_str()),
                "{key} should round-trip verbatim"
            );
        }
        assert_eq!(injected, None, "a value must never add a command");
        assert!(!marker.exists(), "#() in a value must never run");
    }
}

#[cfg(test)]
mod guard_path_tests {
    #[test]
    fn guard_dir_comes_first_exactly_once() {
        crate::commands::test_support::with_temp_home(|home| {
            let guard = home.join(".config/pmux/bin").display().to_string();
            assert_eq!(
                super::guard_path("/usr/bin:/bin"),
                format!("{guard}:/usr/bin:/bin")
            );
            let already = format!("/usr/bin:{guard}:/bin");
            assert_eq!(
                super::guard_path(&already),
                format!("{guard}:/usr/bin:/bin")
            );
            assert_eq!(
                super::guard_path(&super::guard_path("/bin")),
                format!("{guard}:/bin")
            );
        });
    }
}
