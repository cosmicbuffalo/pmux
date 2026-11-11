use anyhow::{Context, Result, anyhow};
use rand::{Rng, distr::Alphanumeric};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Resolve the running pmux binary to a path that can be re-executed.
///
/// `std::env::current_exe()` reads `/proc/self/exe`, which on Linux gains a
/// literal `" (deleted)"` suffix once the binary's inode is replaced — exactly
/// what happens when pmux is reinstalled or upgraded while a long-lived process
/// (such as the sidebar renderer) keeps running. That suffixed path is not a
/// real file, so handing it to `tmux`/`sh` to relaunch pmux fails with errors
/// like `'/home/you/.local/bin/pmux (deleted)' sidebar ...`.
///
/// Strip the suffix and prefer the freshly installed binary at the original
/// location; fall back to plain `pmux` (resolved via `PATH`) when nothing lives
/// there anymore.
pub fn resolve_pmux_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to determine pmux binary path")?;
    Ok(sanitize_exe_path(exe))
}

fn sanitize_exe_path(exe: PathBuf) -> PathBuf {
    const DELETED_SUFFIX: &str = " (deleted)";
    let raw = exe.to_string_lossy();
    if let Some(stripped) = raw.strip_suffix(DELETED_SUFFIX) {
        let stripped = PathBuf::from(stripped);
        if stripped.exists() {
            return stripped;
        }
        // The binary was removed and not replaced at the same path; rely on
        // PATH resolution rather than a path that no longer names a file.
        return PathBuf::from("pmux");
    }
    exe
}

/// Format an epoch timestamp as a human-readable relative age (e.g. "5m", "2h").
/// Used in the picker and `ls` output.
pub fn relative_age(epoch: i64) -> String {
    let now = now_epoch();
    let delta = now.saturating_sub(epoch);
    format_duration(delta as u64)
}

// Format a duration in seconds as a compact string (e.g. "5s", "2m", "3h", "1d").
pub(crate) fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

// Return Some(trimmed) if the string is non-empty, otherwise None.
pub(crate) fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// Return the current Unix epoch in seconds.
pub(crate) fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// Generate a unique session name using the current epoch and random alphanumeric suffix.
pub(crate) fn generate_session_name() -> String {
    let ts = now_epoch();
    let rand: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(8)
        .map(char::from)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("{}-{}", ts, rand)
}

// Join command parts into a single shell-safe string with proper quoting.
pub(crate) fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_quote_lossy(p))
        .collect::<Vec<_>>()
        .join(" ")
}

// Shell-quote a string, returning an error if it contains nul bytes.
pub(crate) fn shell_quote(input: &str) -> Result<String> {
    shlex::try_quote(input)
        .map(|v| v.into_owned())
        .map_err(|_| anyhow!("argument contains unsupported nul byte"))
}

// Shell-quote a string, falling back to '' if it contains nul bytes.
pub(crate) fn shell_quote_lossy(input: &str) -> String {
    shell_quote(input).unwrap_or_else(|_| "''".to_string())
}

/// Return a string's display width in terminal columns.
pub fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

/// Truncate a string to fit within `width` terminal columns, appending "…" if
/// truncated. Used for column rendering in the picker and `ls` output.
pub fn truncate_col(input: &str, width: usize) -> String {
    if display_width(input) <= width {
        return input.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in input.chars() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + char_width > width - 1 {
            break;
        }
        out.push(c);
        used += char_width;
    }
    out.push('…');
    out
}

/// Abbreviate an exact HOME-directory prefix for human-facing path displays.
/// Machine-readable APIs continue to expose the original absolute path.
pub fn display_path(path: &str) -> String {
    let home = std::env::var("HOME").ok();
    abbreviate_home_path(path, home.as_deref())
}

/// PathBuf companion for human-facing status and diagnostics output.
pub fn display_path_buf(path: &Path) -> String {
    display_path(&path.to_string_lossy())
}

fn abbreviate_home_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|home| home.trim_end_matches('/'))
        .filter(|home| !home.is_empty())
    else {
        return path.to_string();
    };
    if path == home {
        return "~".to_string();
    }
    path.strip_prefix(home)
        .filter(|suffix| suffix.starts_with('/'))
        .map(|suffix| format!("~{suffix}"))
        .unwrap_or_else(|| path.to_string())
}

/// Check if a session name was auto-generated by pmux.
///
/// NOTE: This logic is mirrored in the native tmux format in
/// `templates/status.conf`. If you change the name format, update both places.
pub(crate) fn is_auto_generated_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    // Current format: <10-digit-epoch>-<8-alnum>  (e.g. "1772262306-9rtuxxxx")
    if parts.len() == 2 && is_epoch(parts[0]) && is_alnum_exact(parts[1], 8) {
        return true;
    }
    // Legacy format: <prefix>-<10-digit-epoch>-<4-alnum>  (e.g. "codex-1772262287-zvxg")
    if parts.len() >= 3 {
        let ts = parts[parts.len() - 2];
        let rand = parts[parts.len() - 1];
        if is_epoch(ts) && is_alnum_exact(rand, 4) {
            return true;
        }
    }
    false
}

/// Returns true if `s` looks like a Unix epoch: exactly 10 ASCII digits.
fn is_epoch(text: &str) -> bool {
    text.len() == 10 && text.chars().all(|c| c.is_ascii_digit())
}

// Returns true if `text` is exactly `len` ASCII alphanumeric characters.
fn is_alnum_exact(text: &str, len: usize) -> bool {
    text.len() == len && text.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Read one process's argv without invoking a shell.
pub(crate) fn process_argv(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv = data
            .split(|&byte| byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();
        (!argv.is_empty()).then_some(argv)
    }

    #[cfg(target_os = "macos")]
    if let Some(argv) = macos_procargs(pid) {
        return Some(argv);
    }

    // `ps` joins arguments with spaces without quoting them, so this
    // reconstruction is wrong for arguments containing spaces or quotes. It is
    // only the fallback where the exact argument vector is unavailable.
    #[cfg(not(target_os = "linux"))]
    {
        let pid = pid.to_string();
        let output = std::process::Command::new("ps")
            .args(["-ww", "-p", &pid, "-o", "command="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let command = String::from_utf8_lossy(&output.stdout);
        shlex::split(command.trim()).filter(|argv| !argv.is_empty())
    }
}

/// The exact argument vector of `pid` on macOS, via `sysctl KERN_PROCARGS2`.
#[cfg(target_os = "macos")]
fn macos_procargs(pid: u32) -> Option<Vec<String>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    let mut size: libc::size_t = 0;
    // SAFETY: `mib` names three valid integers; a null buffer asks the kernel
    // for the required size, written to `size`.
    let probe = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if probe != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: `buffer` is `size` bytes long, as `size` tells the kernel.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

/// Parse a `KERN_PROCARGS2` buffer: a native-endian `int` argc, the
/// executable path, NUL padding, then `argc` NUL-terminated arguments.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|&argc| argc > 0)?;
    let rest = &buffer[4..];
    let exec_end = rest.iter().position(|&byte| byte == 0)?;
    let args_start = exec_end + rest[exec_end..].iter().position(|&byte| byte != 0)?;
    let argv: Vec<String> = rest[args_start..]
        .split(|&byte| byte == 0)
        .take(argc)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (argv.len() == argc).then_some(argv)
}

#[cfg(not(target_os = "linux"))]
fn parse_pid_parent_pairs(raw: &str) -> Vec<(u32, u32)> {
    raw.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            Some((pid, parent))
        })
        .collect()
}

/// Run `command` like [`std::process::Command::output`], but kill it and fail
/// with [`std::io::ErrorKind::TimedOut`] if it has not exited within
/// `timeout`. For background queries (outer tmux, `ps`) whose server can hang:
/// a stuck tmux must cost a sidebar one late refresh, not a frozen thread.
pub(crate) fn output_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes concurrently so a chatty child cannot block on a full
    // pipe and be mistaken for a hung one.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let deadline = std::time::Instant::now() + timeout;
    let mut nap = std::time::Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command did not finish within {}ms", timeout.as_millis()),
            ));
        }
        std::thread::sleep(nap.min(deadline - now));
        nap = (nap * 2).min(std::time::Duration::from_millis(20));
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Map each of `pids` to its nearest ancestor (inclusive) that is one of
/// `targets`, e.g. a pmux client process to the outer tmux pane it runs in.
/// Pids with no such ancestor are omitted.
pub(crate) fn nearest_ancestors_in(
    pids: &[u32],
    targets: &std::collections::HashSet<u32>,
) -> std::collections::HashMap<u32, u32> {
    #[cfg(not(target_os = "linux"))]
    let parents: std::collections::HashMap<u32, u32> = output_with_timeout(
        std::process::Command::new("ps").args(["-axo", "pid=,ppid="]),
        std::time::Duration::from_secs(2),
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| parse_pid_parent_pairs(&String::from_utf8_lossy(&output.stdout)))
    .unwrap_or_default()
    .into_iter()
    .collect();
    #[cfg(target_os = "linux")]
    let parent_of = |pid: u32| -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may contain spaces or parentheses; fields after the
        // final `)` are `state ppid ...`.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    #[cfg(not(target_os = "linux"))]
    let parent_of = |pid: u32| parents.get(&pid).copied();

    let mut result = std::collections::HashMap::new();
    for &pid in pids {
        let mut current = pid;
        // Bounded walk: a process tree is never this deep, and it guards
        // against a pid-reuse cycle while the tree changes underneath us.
        for _ in 0..64 {
            if targets.contains(&current) {
                result.insert(pid, current);
                break;
            }
            match parent_of(current) {
                Some(parent) if parent > 1 && parent != current => current = parent,
                _ => break,
            }
        }
    }
    result
}

// Collect all descendant PIDs of the given root (inclusive).
pub(crate) fn descendant_pids(root: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        descendant_pids_proc(root)
    }

    #[cfg(not(target_os = "linux"))]
    {
        descendant_pids_ps(root)
    }
}

#[cfg(target_os = "linux")]
fn descendant_pids_proc(root: u32) -> Vec<u32> {
    let mut result = vec![root];
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root);
    while let Some(pid) = queue.pop_front() {
        let path = format!("/proc/{}/task/{}/children", pid, pid);
        if let Ok(data) = std::fs::read_to_string(&path) {
            for token in data.split_whitespace() {
                if let Ok(child) = token.parse::<u32>() {
                    result.push(child);
                    queue.push_back(child);
                }
            }
        }
    }
    result
}

#[cfg(not(target_os = "linux"))]
fn descendant_pids_ps(root: u32) -> Vec<u32> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output();
    let Ok(output) = output else {
        return vec![root];
    };
    if !output.status.success() {
        return vec![root];
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for (pid, parent) in parse_pid_parent_pairs(&raw) {
        children.entry(parent).or_default().push(pid);
    }

    let mut result = vec![root];
    let mut queue = std::collections::VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            result.push(child);
            queue.push_back(child);
        }
    }
    result
}

/// Search the process tree for a `tmux -L pmux ... attach-session -t <name>`
/// and return the session name.
pub(crate) fn find_pmux_session_in_tree(root_pid: u32) -> Option<String> {
    for pid in descendant_pids(root_pid) {
        let Some(argv) = process_argv(pid) else {
            continue;
        };

        let socket = crate::tmux::pmux_socket();
        let has_pmux_socket = argv.windows(2).any(|w| w[0] == "-L" && w[1] == socket);
        if !has_pmux_socket {
            continue;
        }
        if let Some(attach_idx) = argv.iter().position(|a| a == "attach-session") {
            for w in argv[attach_idx..].windows(2) {
                if w[0] == "-t" {
                    // argv keeps the name the client attached with, which goes
                    // stale after `pmux rename-session`; prefer the session the
                    // client is on now.
                    return Some(client_current_session(pid).unwrap_or_else(|| {
                        // New clients use an exact session target with an explicit
                        // session separator so dots cannot be parsed as pane syntax.
                        attach_session_name(&w[1]).to_owned()
                    }));
                }
            }
        }
    }
    None
}

/// Ask the pmux server which session the client with this pid is attached to.
fn client_current_session(client_pid: u32) -> Option<String> {
    let output = crate::tmux::tmux_cmd()
        .args(["list-clients", "-F", "#{client_pid}\t#{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    session_for_client_pid(&String::from_utf8_lossy(&output.stdout), client_pid)
}

fn session_for_client_pid(list_clients: &str, client_pid: u32) -> Option<String> {
    list_clients.lines().find_map(|line| {
        let (pid, session) = line.split_once('\t')?;
        (pid.trim().parse::<u32>().ok()? == client_pid && !session.is_empty())
            .then(|| session.to_owned())
    })
}

fn attach_session_name(target: &str) -> &str {
    target
        .strip_prefix('=')
        .and_then(|name| name.strip_suffix(':'))
        .unwrap_or(target)
}

/// Build a tmux target that matches exactly the session `name`.
///
/// A bare `-t name` falls back to prefix and glob matching when no session
/// has that exact name, so `kill-session -t foo` kills `foobar`.
pub(crate) fn exact_session_target(name: &str) -> String {
    format!("={name}:")
}

/// Apply tmux's own session-name rewriting up front.
///
/// tmux silently replaces `.` and `:` (target separators) with `_` when it
/// creates or renames a session. Doing the same before any state is written
/// keeps pmux's metadata, targets, and printed names in step with tmux.
pub(crate) fn normalize_session_name(name: &str) -> String {
    name.replace(['.', ':'], "_")
}

// Validate that a user-supplied session name is safe for filesystem and tmux use.
pub(crate) fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("session name cannot be empty"));
    }
    if name.starts_with('-') {
        return Err(anyhow!(
            "session name '{}' cannot start with '-' (conflicts with tmux option parsing)",
            name
        ));
    }
    if name.contains('/') {
        return Err(anyhow!("session name cannot contain '/'"));
    }
    // tmux treats `#` as the start of a format (`#{...}`, `#(...)` runs a
    // shell command) in session names, status lines and menus. pmux escapes
    // the names it passes, but a `#` name would still be mangled by any
    // user config or tool that interpolates it, so refuse it outright.
    if name.contains('#') {
        return Err(anyhow!(
            "session name '{}' cannot contain '#' (tmux reads it as a format sequence)",
            name
        ));
    }
    if name.contains('\0') {
        return Err(anyhow!("session name contains an unsupported null byte"));
    }
    // Session lists are parsed from tab-separated tmux output, so a tab or
    // newline in a name would make that session silently disappear.
    if name.chars().any(char::is_control) {
        return Err(anyhow!(
            "session name cannot contain control characters such as tabs or newlines"
        ));
    }
    if name == "." || name == ".." || name.contains("..") {
        return Err(anyhow!(
            "session name '{}' contains path traversal sequence",
            name
        ));
    }
    Ok(())
}

/// A pmux session name in exactly the form tmux uses.
///
/// Every name chosen by a person or read from a file (`run -s`, `resume -s`,
/// rename, restore snapshots) enters through [`SessionName::parse`], which
/// applies tmux's own rewriting and rejects names pmux cannot store as a state
/// file or parse back from tmux output. Names a person types go through
/// [`SessionName::from_user_input`] instead, which also accepts `/`. Address any
/// session in a tmux command with [`exact_session_target`], which never
/// prefix-matches another session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionName(String);

impl SessionName {
    /// Normalize and validate a user-supplied name.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let name = normalize_session_name(raw);
        validate_session_name(&name)?;
        Ok(Self(name))
    }

    /// [`SessionName::parse`] for a name a person typed: `/` becomes `_`, so
    /// `feature/login` is accepted as `feature_login` rather than rejected.
    /// Names read back from files keep the strict check, where a `/` means the
    /// data is corrupt or hostile rather than a naming convention.
    pub(crate) fn from_user_input(raw: &str) -> Result<Self> {
        Self::parse(&raw.replace('/', "_"))
    }

    /// A fresh auto-generated name.
    pub(crate) fn generate() -> Self {
        Self(generate_session_name())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for SessionName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Make untrusted text (agent transcript titles and prompts, launch argv,
/// directory names) safe to print to a terminal.
///
/// Removes ANSI/VT escape sequences whole (CSI, OSC, DCS/SOS/PM/APC strings,
/// and two-byte/nF escapes, in both their 7-bit `ESC` and 8-bit C1 forms) so
/// they cannot retitle the terminal, write to the clipboard, move the cursor
/// or forge output. Tabs and line breaks become spaces, keeping one value on
/// one line; every other control character and the bidi embedding/override/
/// isolate controls are dropped. Printable Unicode is kept unchanged.
pub(crate) fn sanitize_terminal_text(input: &str) -> String {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{7}';
    const C1_ST: char = '\u{9c}';

    // Skip a control string (OSC/DCS/...) up to and including its
    // terminator. OSC may also end with BEL.
    fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, bel_ends: bool) {
        while let Some(c) = chars.next() {
            match c {
                BEL if bel_ends => return,
                C1_ST => return,
                ESC if chars.peek() == Some(&'\\') => {
                    chars.next();
                    return;
                }
                _ => {}
            }
        }
    }
    // Skip CSI parameter/intermediate bytes and the final byte.
    fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                return;
            }
            // Anything outside the CSI byte ranges aborts the sequence.
            if !('\u{20}'..='\u{3f}').contains(&c) {
                return;
            }
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ESC => match chars.next() {
                Some('[') => skip_csi(&mut chars),
                Some(']') => skip_string(&mut chars, true),
                Some('P' | 'X' | '^' | '_') => skip_string(&mut chars, false),
                Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                    // nF escape: intermediates, then one final byte.
                    while chars
                        .peek()
                        .is_some_and(|c| ('\u{20}'..='\u{2f}').contains(c))
                    {
                        chars.next();
                    }
                    chars.next();
                }
                // Two-byte escape (ESC 7, ESC c, ...): the byte is consumed.
                Some(_) | None => {}
            },
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_string(&mut chars, true),
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars, false),
            '\t' | '\n' | '\r' => out.push(' '),
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Create `dir` (and any missing parents) readable only by the owner.
///
/// pmux state holds launch commands (argv may carry secrets), cwds and agent
/// prompts. Newly created directories get mode 0700; an existing leaf that is
/// group- or world-accessible is tightened to 0700, since it belongs to pmux.
pub(crate) fn create_private_dir_all(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let meta =
        std::fs::metadata(dir).with_context(|| format!("failed to inspect {}", dir.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict {}", dir.display()))?;
    }
    Ok(())
}

/// Atomically replace `path` with `contents`, readable only by the owner.
///
/// Writes a dot-prefixed temp file (created 0600 regardless of umask) in the
/// same, private directory and renames it over the target, so readers never
/// see a partial file and the data is never briefly world-readable.
pub(crate) fn write_private_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    create_private_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to create a temp file in {}", dir.display()))?;
    temp.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Read a user-editable text file that pmux is about to rewrite.
///
/// Returns `None` when the file does not exist. Any other read failure, and
/// content that is not valid UTF-8, is an error: treating either as an empty
/// file would make the subsequent write replace the user's whole file.
pub(crate) fn read_text_for_rewrite(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| {
            anyhow!(
                "{} is not valid UTF-8; pmux will not rewrite it",
                path.display()
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Remove every `start`..`end` marker-delimited block from `content`,
/// returning the remaining lines.
///
/// Fails when a start marker has no matching end marker. Treating the rest of
/// the file as part of the block would silently delete the user's own
/// configuration on the next rewrite.
pub(crate) fn strip_marked_blocks<'a>(
    content: &'a str,
    start: &str,
    end: &str,
) -> Result<Vec<&'a str>> {
    let mut output = Vec::new();
    let mut inside = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if !inside && trimmed == start {
            inside = true;
        } else if inside && trimmed == end {
            inside = false;
        } else if inside && trimmed == start {
            return Err(anyhow!(
                "found a second '{start}' before '{end}'; fix the markers and retry"
            ));
        } else if !inside {
            output.push(line);
        }
    }
    if inside {
        return Err(anyhow!(
            "found '{start}' without a matching '{end}'; restore the end marker or remove the block and retry"
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod procargs_tests {
    use super::parse_procargs2;

    fn buffer(argc: i32, exec: &str, args: &[&str]) -> Vec<u8> {
        let mut out = argc.to_ne_bytes().to_vec();
        out.extend_from_slice(exec.as_bytes());
        out.extend_from_slice(&[0, 0, 0]);
        for arg in args {
            out.extend_from_slice(arg.as_bytes());
            out.push(0);
        }
        // The kernel follows argv with the environment; it must be ignored.
        out.extend_from_slice(b"HOME=/Users/me\0");
        out
    }

    // macOS reconstructed argv by shell-splitting `ps` output, which does not
    // quote arguments: a space in the binary path or attach target broke relay
    // detection (and could start a duplicate relay) and session lookup.
    #[test]
    fn procargs2_keeps_arguments_with_spaces_and_quotes_intact() {
        let argv = parse_procargs2(&buffer(
            3,
            "/Users/me/My Apps/pmux",
            &["/Users/me/My Apps/pmux", "relay-ai-attn", "--loop"],
        ))
        .unwrap();
        assert_eq!(argv, ["/Users/me/My Apps/pmux", "relay-ai-attn", "--loop"]);
        let argv = parse_procargs2(&buffer(2, "/usr/bin/tmux", &["tmux", "it's \"odd\""])).unwrap();
        assert_eq!(argv, ["tmux", "it's \"odd\""]);
    }

    #[test]
    fn procargs2_rejects_truncated_or_empty_buffers() {
        assert_eq!(parse_procargs2(&[]), None);
        assert_eq!(parse_procargs2(&buffer(0, "/bin/sh", &[])), None);
        assert_eq!(parse_procargs2(&buffer(3, "/bin/sh", &["sh"])[..12]), None);
    }
}

#[cfg(test)]
mod output_with_timeout_tests {
    use super::output_with_timeout;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn returns_the_output_of_a_prompt_command() {
        let output = output_with_timeout(
            Command::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn kills_a_command_that_outlives_its_deadline() {
        let started = Instant::now();
        let error =
            output_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(100))
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_large_output_is_not_mistaken_for_a_hang() {
        let output = output_with_timeout(
            Command::new("sh").args(["-c", "head -c 300000 /dev/zero"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.stdout.len(), 300_000);
    }
}

#[cfg(test)]
mod marked_block_tests {
    use super::{read_text_for_rewrite, strip_marked_blocks};

    #[test]
    fn strips_every_complete_block() {
        let content = "a\n# S\nx\n# E\nb\n  # S  \ny\n# E\nc\n";
        assert_eq!(
            strip_marked_blocks(content, "# S", "# E").unwrap(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn missing_end_marker_is_an_error_not_a_truncation() {
        let err = strip_marked_blocks("a\n# S\nuser line\n", "# S", "# E").unwrap_err();
        assert!(err.to_string().contains("without a matching"));
    }

    #[test]
    fn nested_start_marker_is_an_error() {
        assert!(strip_marked_blocks("# S\n# S\n# E\n", "# S", "# E").is_err());
    }

    #[test]
    fn stray_end_marker_is_kept_as_user_content() {
        assert_eq!(
            strip_marked_blocks("a\n# E\n", "# S", "# E").unwrap(),
            vec!["a", "# E"]
        );
    }

    #[test]
    fn rewrite_read_rejects_invalid_utf8_and_allows_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("rc");
        std::fs::write(&bad, b"export X=1\n# caf\xe9\n").unwrap();
        assert!(read_text_for_rewrite(&bad).is_err());
        assert!(
            read_text_for_rewrite(&dir.path().join("missing"))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_terminal_text_strips_escape_sequences_and_controls() {
        let cases = [
            ("plain title", "plain title"),
            ("\u{1b}[31mred\u{1b}[0m", "red"),
            ("\u{1b}[?1049hscreen", "screen"),
            ("a\u{1b}]0;pwned title\u{7}b", "ab"),
            ("a\u{1b}]52;c;Y2xpcGJvYXJk\u{1b}\\b", "ab"),
            (
                "a\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\b",
                "alinkb",
            ),
            ("a\u{1b}Pq#0;2;0;0;0\u{1b}\\b", "ab"),
            ("a\u{1b}_apc\u{1b}\\b", "ab"),
            ("a\u{1b}cb", "ab"),
            ("a\u{1b}(Bb", "ab"),
            ("a\u{9b}2Jb", "ab"),
            ("a\u{9d}0;t\u{9c}b", "ab"),
            ("line1\nline2\r\tend", "line1 line2  end"),
            ("bell\u{7}\u{8}\u{7f}\u{85}x", "bellx"),
            ("rtl\u{202e}evil\u{2066}x\u{2069}", "rtlevilx"),
            ("unterminated \u{1b}]0;title", "unterminated "),
            ("trailing esc \u{1b}", "trailing esc "),
            ("café 日本語 🚀", "café 日本語 🚀"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_terminal_text(input), expected, "input {input:?}");
        }
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_dirs_are_created_and_tightened_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b");
        create_private_dir_all(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&tmp.path().join("a")), 0o700);

        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700, "an existing open dir is tightened");
    }

    #[test]
    fn private_files_are_written_atomically_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/secret.json");
        write_private_file_atomic(&path, b"one").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        assert_eq!(mode(&path), 0o600);

        // Replacing a file left 0644 by an older version drops the old mode.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(mode(&path), 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("secret.json")]);
    }

    #[test]
    fn sanitize_exe_path_keeps_a_live_binary_path() {
        let path = PathBuf::from("/home/user/.local/bin/pmux");
        assert_eq!(sanitize_exe_path(path.clone()), path);
    }

    #[test]
    fn sanitize_exe_path_falls_back_to_path_when_binary_is_gone() {
        // A replaced/removed inode yields a `" (deleted)"` suffix that names no
        // real file, so we fall back to PATH resolution.
        let deleted = PathBuf::from("/nonexistent/pmux-xyz (deleted)");
        assert_eq!(sanitize_exe_path(deleted), PathBuf::from("pmux"));
    }

    #[test]
    fn sanitize_exe_path_recovers_the_reinstalled_binary() {
        // Simulate a reinstall: the real file exists at the stripped path even
        // though `/proc/self/exe` reported it as deleted.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("pmux");
        std::fs::write(&real, b"binary").unwrap();
        let deleted = PathBuf::from(format!("{} (deleted)", real.display()));
        assert_eq!(sanitize_exe_path(deleted), real);
    }

    // Tests that format_duration produces correct compact time strings.
    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(7200), "2h");
        assert_eq!(format_duration(172800), "2d");
    }

    #[test]
    fn display_width_and_truncation_use_terminal_columns() {
        assert_eq!(display_width("表a"), 3);
        assert_eq!(truncate_col("表abc", 4), "表a…");
        assert_eq!(display_width(&truncate_col("表abc", 4)), 4);
    }

    // Tests that generated names follow the <epoch>-<8-alnum> format.
    #[test]
    fn test_generate_session_name() {
        let name = generate_session_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].parse::<i64>().is_ok());
        assert_eq!(parts[1].len(), 8);
    }

    // Tests that shell_join properly quotes arguments with spaces.
    #[test]
    fn test_shell_join_quotes() {
        let cmd = shell_join(&["codex".to_string(), "hello world".to_string()]);
        assert!(cmd.contains("hello world") || cmd.contains("'hello world'"));
    }

    // Tests that auto-generated name formats are correctly identified.
    #[test]
    fn test_is_auto_generated_name_formats() {
        // Current format: <10-digit-epoch>-<exactly-8-alnum>
        assert!(is_auto_generated_name("1772262306-9rtuxxxx"));
        // Legacy prefix format: <prefix>-<10-digit-epoch>-<exactly-4-alnum>
        assert!(is_auto_generated_name("codex-1772262287-zvxg"));
        // User-named sessions
        assert!(!is_auto_generated_name("my-session"));
        assert!(!is_auto_generated_name("bugfix-auth"));
        // Would have been a false positive with old logic: 8-digit "date" + 4-char suffix
        assert!(!is_auto_generated_name("my-proj-20250301-beta"));
        // Short random suffix without 10-digit epoch is not auto-generated
        assert!(!is_auto_generated_name("1772262306-9rtu"));
    }

    // Tests that valid session names are accepted.
    #[test]
    fn test_validate_session_name_accepts_valid_names() {
        assert!(validate_session_name("my-session").is_ok());
        assert!(validate_session_name("codex").is_ok());
        assert!(validate_session_name("my_project.1").is_ok());
    }

    // Tests that empty session names are rejected.
    #[test]
    fn test_validate_session_name_rejects_empty() {
        assert!(validate_session_name("").is_err());
    }

    // Tests that path traversal sequences and slashes in names are rejected.
    #[test]
    fn test_validate_session_name_rejects_path_traversal() {
        assert!(validate_session_name("..").is_err());
        assert!(validate_session_name("../etc").is_err());
        assert!(validate_session_name("foo/bar").is_err());
        assert!(validate_session_name(".").is_err());
    }

    // Tests that non_empty returns Some for non-empty content.
    #[test]
    fn test_non_empty_returns_some_for_content() {
        assert_eq!(non_empty("hello"), Some("hello".to_string()));
    }

    // Tests that non_empty returns None for empty strings.
    #[test]
    fn test_non_empty_returns_none_for_empty() {
        assert_eq!(non_empty(""), None);
    }

    // Tests that non_empty trims whitespace and returns None for whitespace-only.
    #[test]
    fn test_non_empty_trims_whitespace() {
        assert_eq!(non_empty("  hello  "), Some("hello".to_string()));
        assert_eq!(non_empty("   "), None);
    }

    #[test]
    fn abbreviate_home_path_only_rewrites_exact_path_boundaries() {
        assert_eq!(
            super::abbreviate_home_path("/home/alice", Some("/home/alice")),
            "~"
        );
        assert_eq!(
            super::abbreviate_home_path("/home/alice/project", Some("/home/alice/")),
            "~/project"
        );
        assert_eq!(
            super::abbreviate_home_path("/home/alicex/project", Some("/home/alice")),
            "/home/alicex/project"
        );
    }

    // Tests format_duration at exact boundary values.
    #[test]
    fn test_format_duration_boundaries() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(3599), "59m");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(86399), "23h");
        assert_eq!(format_duration(86400), "1d");
    }

    // Tests that shell_quote rejects strings containing nul bytes.
    #[test]
    fn test_shell_quote_rejects_nul() {
        assert!(shell_quote("foo\0bar").is_err());
    }

    // Tests that shell_quote_lossy returns '' for strings with nul bytes.
    #[test]
    fn test_shell_quote_lossy_nul() {
        assert_eq!(shell_quote_lossy("foo\0bar"), "''");
    }

    // Tests that session names starting with '-' are rejected.
    #[test]
    fn test_validate_session_name_rejects_leading_dash() {
        assert!(validate_session_name("-foo").is_err());
    }

    // Tests that session names containing null bytes are rejected.
    #[test]
    fn test_validate_session_name_rejects_null_byte() {
        assert!(validate_session_name("foo\0bar").is_err());
    }

    // Tests that session names containing ".." are rejected.
    #[test]
    fn test_validate_session_name_rejects_dotdot() {
        assert!(validate_session_name("..").is_err());
        assert!(validate_session_name("foo..bar").is_err());
    }

    // Tests that session names containing '/' are rejected.
    #[test]
    fn test_validate_session_name_rejects_slash() {
        assert!(validate_session_name("foo/bar").is_err());
    }

    #[test]
    fn test_validate_session_name_rejects_hash() {
        for name in ["a#b", "#(touch pwned)", "x#{session_name}", "#", "issue#12"] {
            let err = validate_session_name(name).unwrap_err().to_string();
            assert!(err.contains("'#'"), "{name}: {err}");
            assert!(SessionName::parse(name).is_err(), "{name}");
            assert!(SessionName::from_user_input(name).is_err(), "{name}");
        }
    }

    #[test]
    fn test_generated_and_normalized_names_pass_validation() {
        for _ in 0..50 {
            let generated = SessionName::generate();
            assert!(validate_session_name(&generated).is_ok(), "{generated}");
            assert!(SessionName::parse(&generated).is_ok(), "{generated}");
        }
        for raw in ["3.3.2 readiness", "host:8080", "feature/login"] {
            assert!(SessionName::from_user_input(raw).is_ok(), "{raw}");
        }
    }

    // Tests that various valid session names are accepted.
    #[test]
    fn test_validate_session_name_accepts_valid() {
        assert!(validate_session_name("my-session").is_ok());
        assert!(validate_session_name("session.1").is_ok());
        assert!(validate_session_name("test_123").is_ok());
    }

    // tmux rewrites target separators in session names; pmux must agree.
    #[test]
    fn test_normalize_session_name_matches_tmux() {
        assert_eq!(normalize_session_name("3.3.2 readiness"), "3_3_2 readiness");
        assert_eq!(normalize_session_name("host:8080"), "host_8080");
        assert_eq!(normalize_session_name("plain-name"), "plain-name");
        assert!(validate_session_name(&normalize_session_name("../etc")).is_err());
    }

    // Regression: a tab or newline in a name broke the tab-separated session
    // list, so the session silently vanished from ls, the picker, and sidebar.
    #[test]
    fn test_session_name_parse_normalizes_and_rejects_unlistable_names() {
        assert_eq!(
            SessionName::parse("v1.2 work").unwrap().as_str(),
            "v1_2 work"
        );
        for bad in ["", "-x", "a/b", "a\tb", "a\nb", "a\u{7}b"] {
            assert!(
                SessionName::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(SessionName::generate().len() > 10);
    }

    #[test]
    fn test_session_name_from_user_input_converts_slashes() {
        assert_eq!(
            SessionName::from_user_input("feature/login")
                .unwrap()
                .as_str(),
            "feature_login"
        );
        // Traversal still cannot survive: every `.` and `/` is rewritten.
        assert_eq!(
            SessionName::from_user_input("../etc").unwrap().as_str(),
            "___etc"
        );
        for bad in ["", "-x", "a\tb"] {
            assert!(
                SessionName::from_user_input(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_exact_session_target_disables_prefix_matching() {
        assert_eq!(exact_session_target("foo"), "=foo:");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_parse_pid_parent_pairs() {
        let pairs = parse_pid_parent_pairs("  1     0\n 42     1\ninvalid\n 99    42 extra\n");
        assert_eq!(pairs, vec![(1, 0), (42, 1), (99, 42)]);
    }
}

#[cfg(test)]
mod exact_attach_target_tests {
    #[test]
    fn process_tree_target_recovers_original_session_name() {
        for name in ["3.3.2 readiness", "simple", "=special", "a b"] {
            assert_eq!(super::attach_session_name(&format!("={name}:")), name);
            assert_eq!(super::attach_session_name(name), name);
        }
    }

    #[test]
    fn client_session_lookup_follows_renames() {
        let clients = "2959867\t3.3.4 sol review\n4151600\t1790228557-r4qxdm8g\n";
        assert_eq!(
            super::session_for_client_pid(clients, 2959867).as_deref(),
            Some("3.3.4 sol review")
        );
        assert_eq!(super::session_for_client_pid(clients, 1), None);
        assert_eq!(super::session_for_client_pid("", 2959867), None);
        assert_eq!(super::session_for_client_pid("42\t\n", 42), None);
    }
}
