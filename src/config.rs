use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::util::{read_text_for_rewrite, strip_marked_blocks};

const GENERATED_CONFIG_VERSION: &str = "63";
const MANAGED_TMUX_START: &str = "# >>> pmux managed lifecycle >>>";
const MANAGED_TMUX_END: &str = "# <<< pmux managed lifecycle <<<";
const MANAGED_TMUX_BLOCK: &str = include_str!("../templates/managed-tmux-lifecycle.conf");
const MANAGED_OUTER_SIDEBAR_START: &str = "# >>> pmux managed sidebar >>>";
const MANAGED_OUTER_SIDEBAR_END: &str = "# <<< pmux managed sidebar <<<";
const MANAGED_OUTER_SIDEBAR_BLOCK: &str = include_str!("../templates/managed-outer-sidebar.conf");
const LEGACY_STATUS_LEFT: &str = r##"set -g status-left "#{@pmux_process_chip_style} [#(~/.config/pmux/session-label.sh \"#{session_name}\" \"#{?pane_start_command,#{pane_start_command},#{pane_current_command}}\" \"#{pane_current_command}\")] #{@pmux_age_style} #(~/.config/pmux/session-age.sh \"#{session_created}\") ago#{?@ai_attn_pane_state, #(~/.config/pmux/ai-attn-age.sh \"#{@ai_attn_pane_updated_at}\" \"#{@ai_attn_pane_state}\"),} #[default]""##;
const LEGACY_ZSH_BINDING: &str =
    r#"bind C run-shell -b "~/.config/pmux/new-process.sh '#{pane_id}' zsh""#;
const DEFAULT_SHELL_BINDING: &str =
    r#"bind C run-shell -b "~/.config/pmux/new-process.sh '#{pane_id}'""#;
const NATIVE_STATUS_SOURCE: &str = "source-file ~/.config/pmux/status.conf";
const AI_ATTN_SETUP_MARKER: &str = "# Silently skipped when ai-attn is not available.";
const AI_ATTN_REFRESH_POLICY: &str = "set -g @ai_attn_refresh_client \"off\"";
const PMUX_WRAP_START: &str = "# >>> pmux wrap >>>";
const PMUX_WRAP_END: &str = "# <<< pmux wrap <<<";
const RETIRED_STATUS_HELPERS: [&str; 3] = ["session-age.sh", "session-label.sh", "ai-attn-age.sh"];

// Return the path to the pmux config directory (~/.config/pmux).
pub(crate) fn pmux_config_dir() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("pmux"))
}

// Return the path to the pmux tmux.conf file.
/// The file pmux starts its server with and reloads its config through. It
/// marks the server as pmux's and sources `tmux.conf`; see
/// `templates/server.conf`.
pub(crate) fn server_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("server.conf"))
}

/// pmux's tmux.rb module, written for tmux.rb users to build on.
pub(crate) fn rb_module_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("rb").join("pmux.rb"))
}

/// A tmux.rb user's configuration for pmux's server; when it exists, pmux
/// compiles it into tmux.conf instead of editing tmux.conf in place.
pub(crate) fn tmux_conf_rb_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("tmux.conf.rb"))
}

pub(crate) fn tmux_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("tmux.conf"))
}

// Return the path to the pmux config.toml file (structured user config).
pub(crate) fn pmux_config_toml_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("config.toml"))
}

// Return the path to the new-process.sh helper script.
fn new_process_script_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("new-process.sh"))
}

// Return the path to the help.sh helper script.
fn help_script_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("help.sh"))
}

// Return the path to the ai-attn-setup.sh helper script.
fn ai_attn_setup_script_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("ai-attn-setup.sh"))
}

/// The directory pmux puts first on PATH for its processes. It holds only
/// the tmux guard (see `templates/tmux-guard.sh`).
pub(crate) fn tmux_guard_dir() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("bin"))
}

fn legacy_bell_hook_cleanup_script_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("clear-legacy-ai-attn-bell-hook.sh"))
}

fn generated_config_version_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join(".generated-version"))
}

// Return the path to the theme.conf file.
fn theme_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("theme.conf"))
}

// Return the generated shell-free status format file.
fn status_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("status.conf"))
}

// Return the optional outer tmux integration template.
fn outer_tmux_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("outer-tmux.conf"))
}

// Return the generated sidebar integration sourced by outer-tmux.conf.
fn sidebar_conf_path() -> Result<PathBuf> {
    Ok(pmux_config_dir()?.join("sidebar.conf"))
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    write_file_atomic(path, content, FileMode::Exactly(0o755))
}

/// Permissions for a file written by [`write_file_atomic`].
#[derive(Clone, Copy)]
enum FileMode {
    /// Keep an existing file's mode; use this one for a new file.
    PreserveOr(u32),
    /// Always use this mode (generated helper scripts must stay executable).
    Exactly(u32),
}

// Follow a symlink chain to the file that actually holds the contents, so a
// dotfile managed as a symlink stays a symlink after pmux edits it. A chain
// ending in a missing file resolves to that missing path.
fn resolve_write_target(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&current)
                    .with_context(|| format!("failed to read link {}", current.display()))?;
                current = match current.parent() {
                    Some(parent) if link.is_relative() => parent.join(link),
                    _ => link,
                };
            }
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to stat {}", current.display()));
            }
        }
    }
    anyhow::bail!("too many levels of symbolic links at {}", path.display())
}

/// Replace `path` atomically: write a temp file beside the real target,
/// fsync it, then rename it into place. An interrupted write can no longer
/// leave a user's tmux.conf or shell rc file truncated.
fn write_file_atomic(path: &Path, contents: &str, mode: FileMode) -> Result<()> {
    use std::io::Write;
    let target = resolve_write_target(path)?;
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(unix)]
    let mode = match mode {
        FileMode::Exactly(bits) => bits,
        FileMode::PreserveOr(default) => fs::metadata(&target)
            .map(|meta| meta.permissions().mode() & 0o7777)
            .unwrap_or(default),
    };
    #[cfg(not(unix))]
    let _ = mode;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to create a temp file in {}", dir.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", target.display()))?;
    #[cfg(unix)]
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to chmod {}", temp.path().display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync {}", target.display()))?;
    temp.persist(&target)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", target.display()))?;
    // Persist the rename itself; failure here does not lose the contents.
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

// A user-editable config file: keep its mode, default to 0644 when new.
fn write_config_file(path: &Path, contents: &str) -> Result<()> {
    write_file_atomic(path, contents, FileMode::PreserveOr(0o644))
}

// Replace (or append) one marker-delimited block, keeping everything else.
// Fails instead of guessing when the existing markers are unbalanced.
fn upsert_marked_block(existing: &str, start: &str, end: &str, block: &str) -> Result<String> {
    let mut lines = strip_marked_blocks(existing, start, end)?;
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut updated = lines.join("\n");
    if !updated.is_empty() {
        updated.push_str("\n\n");
    }
    updated.push_str(block.trim_end());
    updated.push('\n');
    Ok(updated)
}

fn upsert_managed_tmux_block(existing: &str) -> Result<String> {
    upsert_marked_block(
        existing,
        MANAGED_TMUX_START,
        MANAGED_TMUX_END,
        MANAGED_TMUX_BLOCK,
    )
}

// Add or replace pmux's small, marked sidebar source block without touching
// the rest of a user's outer tmux integration file.
fn upsert_managed_outer_sidebar_block(existing: &str) -> Result<String> {
    upsert_marked_block(
        existing,
        MANAGED_OUTER_SIDEBAR_START,
        MANAGED_OUTER_SIDEBAR_END,
        &managed_outer_sidebar_block(),
    )
}

const PMUX_BIN_PLACEHOLDER: &str = "@PMUX_BIN@";

// The binary the outer bootstrap line should run, or `pmux` (a PATH lookup)
// when the running binary has no absolute UTF-8 path. A newline cannot be
// carried through the one-line tmux command, so it falls back as well.
fn outer_bootstrap_binary() -> String {
    crate::util::resolve_pmux_binary()
        .ok()
        .filter(|path| path.is_absolute())
        .and_then(|path| path.to_str().map(str::to_string))
        .filter(|path| !path.contains(['\n', '\0']))
        .unwrap_or_else(|| "pmux".to_string())
}

/// Escape `binary` for the `pmux_bin="@PMUX_BIN@"` slot of the outer
/// bootstrap line. The value passes through three layers, escaped innermost
/// first: a double-quoted shell word (escape `\`, `"`, `$` and backquote),
/// run-shell's format expansion (`#` becomes `##`), and the single-quoted
/// tmux string around the whole command, which cannot contain `'` itself, so
/// each `'` closes the quote, adds a double-quoted `'` and reopens it.
fn bootstrap_binary_slot(binary: &str) -> String {
    let mut shell_word = String::with_capacity(binary.len());
    for ch in binary.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            shell_word.push('\\');
        }
        shell_word.push(ch);
    }
    crate::tmux::tmux_format_escape(&shell_word).replace('\'', "'\"'\"'")
}

// tmux/pmux.rb with its PMUX_BIN placeholder replaced: the same escaped
// value the outer bootstrap line uses, as a Ruby string literal.
fn default_rb_module() -> String {
    let slot = bootstrap_binary_slot(&outer_bootstrap_binary());
    let mut literal = String::from('"');
    for ch in slot.chars() {
        if matches!(ch, '\\' | '"' | '#') {
            literal.push('\\');
        }
        literal.push(ch);
    }
    literal.push('"');
    include_str!("../tmux/pmux.rb").replace(&format!("\"{PMUX_BIN_PLACEHOLDER}\""), &literal)
}

/// The tmux.rb command: TMUX_RB_BIN, else `tmux.rb` or `tmux_rb` on PATH.
fn tmux_rb_command() -> Option<PathBuf> {
    if let Some(bin) = env::var_os("TMUX_RB_BIN").filter(|bin| !bin.is_empty()) {
        return Some(PathBuf::from(bin)).filter(|bin| bin.is_file());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .flat_map(|dir| [dir.join("tmux.rb"), dir.join("tmux_rb")])
        .find(|candidate| candidate.is_file())
}

/// Compile ~/.config/pmux/tmux.conf.rb into tmux.conf. Ok(false) when there
/// is no tmux.conf.rb; an error explains how to compile it by hand.
pub(crate) fn compile_tmux_conf_rb() -> Result<bool> {
    let source = tmux_conf_rb_path()?;
    if !source.exists() {
        return Ok(false);
    }
    let output = tmux_conf_path()?;
    let hint = format!(
        "tmux.rb build {} --output {}",
        crate::util::display_path_buf(&source),
        crate::util::display_path_buf(&output)
    );
    let Some(tmux_rb) = tmux_rb_command() else {
        bail!(
            "{} needs tmux.rb (the tmux gem) to compile; install it, then run: {hint}",
            crate::util::display_path_buf(&source)
        );
    };
    let result = std::process::Command::new(&tmux_rb)
        .arg("build")
        .arg(&source)
        .arg("--output")
        .arg(&output)
        .output()
        .with_context(|| format!("failed to run {}", tmux_rb.display()))?;
    if !result.status.success() {
        bail!(
            "tmux.rb could not compile {}: {}",
            crate::util::display_path_buf(&source),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(true)
}

/// Recompile tmux.conf.rb when it changed since tmux.conf was written, so a
/// server pmux starts or reloads never runs a stale compile. Two stats when
/// there is nothing to do; a failed compile is reported, not fatal.
fn compile_tmux_conf_rb_if_newer() {
    let (Ok(source), Ok(output)) = (tmux_conf_rb_path(), tmux_conf_path()) else {
        return;
    };
    let Ok(source_time) = fs::metadata(&source).and_then(|meta| meta.modified()) else {
        return;
    };
    let output_time = fs::metadata(&output).and_then(|meta| meta.modified()).ok();
    if output_time.is_some_and(|time| time >= source_time) {
        return;
    }
    if let Err(err) = compile_tmux_conf_rb() {
        eprintln!("pmux: {err}");
    }
}

/// Create the starter tmux.conf.rb for `pmux init --tmux-rb` (keeping an
/// existing one) and compile it. Returns the path and whether it was created.
pub(crate) fn init_tmux_conf_rb() -> Result<(PathBuf, bool)> {
    ensure_pmux_config()?;
    let source = tmux_conf_rb_path()?;
    let created = !source.exists();
    if created {
        write_config_file(&source, include_str!("../templates/tmux.conf.rb"))
            .with_context(|| format!("failed to write {}", source.display()))?;
    }
    compile_tmux_conf_rb()?;
    Ok((source, created))
}

fn render_pmux_bin(template: &str) -> String {
    template.replace(
        PMUX_BIN_PLACEHOLDER,
        &bootstrap_binary_slot(&outer_bootstrap_binary()),
    )
}

fn managed_outer_sidebar_block() -> String {
    render_pmux_bin(MANAGED_OUTER_SIDEBAR_BLOCK)
}

// Generated config files are refreshed from nearly every pmux command. When
// a user has broken a managed block's markers, leave that file untouched and
// say why once, rather than truncating it or making every command fail.
fn warn_skipped_managed_block(path: &Path, err: &anyhow::Error) {
    eprintln!(
        "pmux: not updating {}: {err}. pmux's managed block there may be stale.",
        path.display()
    );
}

fn upgrade_managed_tmux_commands(existing: &str) -> String {
    let mut updated = existing
        .replace(LEGACY_STATUS_LEFT, NATIVE_STATUS_SOURCE)
        .replace(LEGACY_ZSH_BINDING, DEFAULT_SHELL_BINDING)
        .replace(
            r#"'run-shell "~/.config/pmux/ai-attn-setup.sh"'"#,
            r#"'run-shell -b "~/.config/pmux/ai-attn-setup.sh"'"#,
        );
    // Every earlier `bind r` template, oldest first. The v0.1.0 one used tmux's
    // native rename, which skips pmux's state migration and orphans the
    // session's launch command and host-pane mapping.
    // Earlier templates spliced "#{session_name}" into shell text, so a
    // session name containing `"` or `$(...)` ran as shell code.
    // The popup form without --pane guessed the session from client context,
    // which picks the wrong process when several clients are attached.
    const RENAME_POPUP: &str = r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##;
    for legacy in [
        r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "$HOME/.local/bin/pmux rename-session-ui"'"##,
        r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui"'"##,
        r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "$HOME/.local/bin/pmux rename-session-ui \"#{session_name}\""'"##,
        r##"bind r command-prompt -I "#{session_name}" "rename-session -- '%%'""##,
        r##"bind r command-prompt -I "#{session_name}" "run-shell -b '$HOME/.local/bin/pmux rename-session \"#{session_name}\" \"%%\"'""##,
        r##"bind r run-shell -b 'tmux display-popup -w 70% -h 12 -E "$HOME/.local/bin/pmux rename-session-ui \"#{session_name}\""'"##,
        r##"bind r run-shell -b 'tmux display-popup -B -w 70% -h 12 -E "$HOME/.local/bin/pmux rename-session-ui \"#{session_name}\""'"##,
    ] {
        updated = updated.replace(legacy, RENAME_POPUP);
    }
    updated = updated.replace(
        r##"bind k run-shell -b '$HOME/.local/bin/pmux kill-current-and-next "#{session_name}" "#{client_tty}" >/dev/null 2>&1 || true'"##,
        r##"bind k run-shell -b '$HOME/.local/bin/pmux kill-current-and-next #{q:session_name} #{q:client_tty} >/dev/null 2>&1 || true'"##,
    );
    // Earlier templates hardcoded ~/.local/bin/pmux, which breaks every key
    // binding when pmux lives elsewhere. Both generated shapes (a bare command
    // in a single-quoted run-shell, and a popup command in double quotes)
    // switch to the server's PMUX_BIN with a PATH fallback.
    updated = updated
        .replace("'$HOME/.local/bin/pmux ", "'\"${PMUX_BIN:-pmux}\" ")
        .replace("\"$HOME/.local/bin/pmux ", "\"${PMUX_BIN:-pmux} ");
    if !updated.contains("@ai_attn_refresh_client") && updated.contains(AI_ATTN_SETUP_MARKER) {
        updated = updated.replace(
            AI_ATTN_SETUP_MARKER,
            &format!("{AI_ATTN_REFRESH_POLICY}\n{AI_ATTN_SETUP_MARKER}"),
        );
    }
    updated
}

fn upgrade_queue_binding(existing: &str) -> String {
    append_binding_unless_bound(
        existing,
        "q",
        include_str!("../templates/queue-binding.conf"),
    )
}

// The first ai-auto toggle binding shipped without a note, so it was missing
// from the `prefix + /` and `prefix + ?` key hints.
const LEGACY_AUTO_TOGGLE: &str = r##"bind Tab run-shell -b '"${AI_AUTO_BIN:-ai-auto}" toggle -t "#{pane_id}" -S "#{socket_path}" --message >/dev/null 2>&1 || tmux display-message "ai-auto failed (is it installed?)"'"##;

// The first history binding opened a plain pager; the viewer that replaced it
// folds sections, navigates by item, and follows client resizes.
const LEGACY_AUTO_HISTORY: &str = r##"bind -N "ai-auto: inspect auto mode history for this process" I run-shell -b 'tmux display-popup -w 90% -h 90% -T " ai-auto history " -E "${AI_AUTO_BIN:-ai-auto} history -t \"#{pane_id}\" -S \"#{socket_path}\" --pager"'"##;

// Earlier templates bound auto mode unconditionally, so without ai-auto
// installed `prefix + Tab` only reported a failure and the key hints listed
// two dead keys.
const UNCONDITIONAL_AUTO_TOGGLE: &str = r##"bind -N "ai-auto: toggle auto mode for this process" Tab run-shell -b '"${AI_AUTO_BIN:-ai-auto}" toggle -t "#{pane_id}" -S "#{socket_path}" --message >/dev/null 2>&1 || tmux display-message "ai-auto failed (is it installed?)"'"##;
const UNCONDITIONAL_AUTO_HISTORY: &str = r##"bind -N "ai-auto: inspect auto mode history for this process" I run-shell -b 'tmux display-popup -w 90% -h 90% -T " ai-auto history " -E "${AI_AUTO_BIN:-ai-auto} history -t \"#{pane_id}\" -S \"#{socket_path}\" --tui --client \"#{client_name}\""'"##;
const AUTO_MODE_MARKER: &str = "# >>> pmux auto mode >>>";

// Replace any unconditional auto mode binding with the marked block that
// binds the keys only when ai-auto is installed, unless the user bound Tab
// or I to something else.
fn upgrade_auto_binding(existing: &str) -> String {
    if existing.contains(AUTO_MODE_MARKER) {
        return existing.to_string();
    }
    let unconditional = [
        LEGACY_AUTO_TOGGLE,
        LEGACY_AUTO_HISTORY,
        UNCONDITIONAL_AUTO_TOGGLE,
        UNCONDITIONAL_AUTO_HISTORY,
    ];
    let kept: String = existing
        .split_inclusive('\n')
        .filter(|line| !unconditional.contains(&line.trim_end_matches('\n')))
        .collect();
    if kept
        .lines()
        .any(|line| matches!(prefix_table_key(line), Some("Tab" | "I")))
    {
        return kept;
    }
    let mut updated = kept;
    if !updated.ends_with('\n') && !updated.is_empty() {
        updated.push('\n');
    }
    updated.push('\n');
    updated.push_str(include_str!("../templates/auto-binding.conf"));
    updated
}

// The first help binding opened the hints as a pager in a new window, and
// `prefix + ?` and `prefix + /` were left to tmux's full key list.
const LEGACY_HELP: &str = r#"bind h new-window -n pmux-help "~/.config/pmux/help.sh --full""#;

fn upgrade_help_binding(existing: &str) -> String {
    let snippet = include_str!("../templates/help-binding.conf");
    let bindings: Vec<&str> = snippet.lines().filter(|l| l.starts_with("bind")).collect();
    let help = bindings
        .iter()
        .find(|binding| binding_key(binding) == Some("h"))
        .expect("help template binds h");
    let mut updated = existing.replace(LEGACY_HELP, help);
    for binding in bindings {
        let key = binding_key(binding).expect("template bindings name a key");
        updated = append_binding_unless_bound(&updated, key, &format!("{binding}\n"));
    }
    updated
}

// Bindings first shipped without the `-N` notes the key hints popup reads
// (`pmux key-hints`). Each template binding's note-less form is its legacy
// line; only exact copies are noted, so an edited binding stays as written
// and the popup shows its command instead. `x` and `[` are tmux defaults the
// popup should still list, so they are added with notes unless already bound.
// Every binding as it read before bindings carried `-N` notes for the key
// hints popup (`pmux key-hints`). A config line still matching one exactly is
// replaced by the current template binding for the same key.
const UNNOTED_BINDINGS: &[&str] = &[
    r##"bind C-p send-prefix"##,
    r##"bind d detach-client"##,
    r##"bind x confirm-before -p "kill-pane #P? (y/n)" kill-pane"##,
    r##"bind [ copy-mode"##,
    r##"bind q run-shell -b 'tmux display-popup -w 85% -h 85% -E "${PMUX_BIN:-pmux} queue-ui \"#{pane_id}\""'"##,
    r##"bind c command-prompt -p "Run command:" "run-shell -b \"~/.config/pmux/new-process.sh '#{pane_id}' %%\"""##,
    r##"bind C run-shell -b "~/.config/pmux/new-process.sh '#{pane_id}'""##,
    r##"bind s run-shell -b 'h=$(tmux display-message -p "#{client_height}"); [ "$h" -gt 1 ] && h=$((h-1)); tmux display-popup -x 0 -y 0 -w 100% -h "$h" -E "${PMUX_BIN:-pmux} attach"'"##,
    r##"bind l run-shell -b '"${PMUX_BIN:-pmux}" last >/dev/null 2>&1 || true'"##,
    r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
    r##"bind R source-file ~/.config/pmux/server.conf \; display-message "pmux config reloaded""##,
    r##"bind n run-shell -b '"${PMUX_BIN:-pmux}" next-process >/dev/null 2>&1 || true'"##,
    r##"bind p run-shell -b '"${PMUX_BIN:-pmux}" prev-process >/dev/null 2>&1 || true'"##,
    r##"bind k run-shell -b '"${PMUX_BIN:-pmux}" kill-current-and-next #{q:session_name} #{q:client_tty} >/dev/null 2>&1 || true'"##,
    r##"bind a run-shell -b 'ai-attn clear --pane >/dev/null 2>&1 || true'"##,
    r##"bind ? run-shell -b '~/.config/pmux/help.sh --open "#{client_name}"'"##,
    r##"bind / run-shell -b '~/.config/pmux/help.sh --open "#{client_name}"'"##,
    r##"bind h run-shell -b '~/.config/pmux/help.sh --open "#{client_name}"'"##,
];

fn upgrade_binding_notes(existing: &str) -> String {
    let templates = [
        include_str!("../templates/tmux.conf"),
        include_str!("../templates/queue-binding.conf"),
        include_str!("../templates/help-binding.conf"),
    ];
    // Top-level template bindings only; the auto mode keys sit indented in
    // their own conditional block and have their own migration.
    let current: Vec<&str> = templates
        .iter()
        .flat_map(|template| template.lines())
        .filter(|line| line.starts_with("bind-key -N "))
        .collect();
    let for_key = |key: &str| {
        NOTED_RENAME_RELOAD
            .iter()
            .copied()
            .chain(current.iter().copied())
            .find(|line| prefix_table_key(line) == Some(key))
    };
    let mut updated: String = existing
        .split_inclusive('\n')
        .map(|line| {
            let (body, end) = match line.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (line, ""),
            };
            let replacement = UNNOTED_BINDINGS
                .contains(&body)
                .then(|| prefix_table_key(body))
                .flatten()
                .and_then(for_key);
            match replacement {
                Some(noted) => format!("{noted}{end}"),
                None => line.to_string(),
            }
        })
        .collect();
    for key in ["x", "["] {
        let line = for_key(key).expect("tmux.conf template notes x and [");
        updated = append_binding_unless_bound(&updated, key, &format!("{line}\n"));
    }
    updated
}

// The first `R` binding re-sourced tmux.conf directly; reloading through
// server.conf keeps the server marker and loading flag that tmux.conf's
// guard and the foreign-config hook rely on.
const LEGACY_RELOAD_BINDINGS: [&str; 2] = [
    r#"bind R source-file ~/.config/pmux/tmux.conf \; display-message "pmux config reloaded""#,
    r#"bind -N "pmux: reload the pmux config" R source-file ~/.config/pmux/tmux.conf \; display-message "pmux config reloaded""#,
];

// Rename and reload as they were bound until rename moved to `,` (tmux's own
// rename-window key) and reload to `r`. Earlier migrations bring old configs
// to these lines; `upgrade_moved_bindings` then moves them.
const NOTED_RENAME_RELOAD: [&str; 2] = [
    r##"bind-key -N "process: rename this process" "r" run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
    r##"bind-key -N "pmux: reload the pmux config" "R" source-file ~/.config/pmux/server.conf \; display-message "pmux config reloaded""##,
];

// Stock bindings pmux has since moved or replaced, as (old key, old command,
// new key). A config line binding the old key to exactly the old command, in
// any form pmux has written (`bind` or `bind-key`, noted or not, quoted key or
// bare), becomes the template's binding for the new key, unless another line
// already binds that key. They apply in order, so rename leaves `r` free
// before reload moves onto it. `c` and `x` used tmux's command line before
// pmux's floating prompt.
const MOVED_BINDINGS: [(&str, &str, &str); 4] = [
    (
        "r",
        r##"run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
        ",",
    ),
    (
        "R",
        r##"source-file ~/.config/pmux/server.conf \; display-message "pmux config reloaded""##,
        "r",
    ),
    (
        "c",
        r##"command-prompt -p "Run command:" "run-shell -b \"~/.config/pmux/new-process.sh '#{pane_id}' %%\"""##,
        "c",
    ),
    (
        "x",
        r##"confirm-before -p "kill-pane #P? (y/n)" kill-pane"##,
        "x",
    ),
];

fn upgrade_moved_bindings(existing: &str) -> String {
    let template = include_str!("../templates/tmux.conf");
    let mut updated = existing.to_string();
    for (old_key, old_command, new_key) in MOVED_BINDINGS {
        let new = template
            .lines()
            .find(|line| binding_key(line) == Some(new_key))
            .expect("template binds every moved key");
        let key_taken = new_key != old_key
            && updated
                .lines()
                .any(|line| prefix_table_key(line) == Some(new_key));
        if key_taken {
            continue;
        }
        updated = updated
            .split_inclusive('\n')
            .map(|line| {
                let body = line.trim_end_matches('\n');
                if binding_command(body) == Some(("prefix", old_key, old_command)) {
                    let indent = &body[..body.len() - body.trim_start().len()];
                    format!("{indent}{new}{}", &line[body.len()..])
                } else {
                    line.to_string()
                }
            })
            .collect();
    }
    updated
}

// Earlier `prefix + y` bindings, oldest first, each as (its comment, its
// line). The first printed the name from a shell, so a `#` in it was read as a
// format, and did not show an auto-named process's agent title. The second
// only sent OSC 52 from pmux's server, which a stock outer tmux drops
// (`set-clipboard external`), and had no fallback for terminals without it.
const LEGACY_COPY_NAME_BINDINGS: [(&[&str], &str); 2] = [
    (
        &[
            "# `y` copies this process's name (what `pmux ls` and other pmux commands",
            "# take) to the clipboard, e.g. to hand it to another agent session.",
        ],
        r##"bind-key -N "process: copy this process's name to the clipboard" "y" run-shell -b 'tmux set-buffer -w -t #{q:client_name} -- #{q:session_name} && tmux display-message -c #{q:client_name} "copied process name: "#{q:session_name}'"##,
    ),
    (
        &[
            "# `y` copies this process's name (what `pmux ls` and other pmux commands",
            "# take) to the clipboard, e.g. to hand it to another agent session. The",
            "# message also shows the agent title of an auto-named process.",
        ],
        r##"bind-key -N "process: copy this process's name to the clipboard" "y" run-shell -b 'tmux set-buffer -w -t #{q:client_name} -- #{q:session_name}' \; display-message "copied #{session_name}#{?@pmux_status_label_derived, (#{E:@pmux_status_label}),}""##,
    ),
];

// `prefix + y` (copy the process name) arrived after configs were written.
// Insert it with its template comment after the `,` rename binding, or at the
// end when `,` is gone, unless the prefix table already binds or unbinds `y`.
// An exact copy of an earlier version, with or without its comment, becomes
// the current one.
fn upgrade_copy_name_binding(existing: &str) -> String {
    let template: Vec<&str> = include_str!("../templates/tmux.conf").lines().collect();
    let at = template
        .iter()
        .position(|line| binding_key(line) == Some("y"))
        .expect("template binds y");
    let start = template[..at]
        .iter()
        .rposition(|line| !line.starts_with('#'))
        .map_or(0, |i| i + 1);
    let block = &template[start..=at];

    for (comment, binding) in LEGACY_COPY_NAME_BINDINGS {
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        let Some(old) = lines.iter().position(|line| line.trim() == binding) else {
            continue;
        };
        let indent = lines[old][..lines[old].len() - lines[old].trim_start().len()].to_string();
        let has_comment = old >= comment.len()
            && lines[old - comment.len()..old]
                .iter()
                .zip(comment)
                .all(|(line, expected)| line.trim() == *expected);
        let (from, replacement) = if has_comment {
            (old - comment.len(), block)
        } else {
            (old, &template[at..=at])
        };
        lines.splice(
            from..=old,
            replacement.iter().map(|line| format!("{indent}{line}")),
        );
        let mut updated = lines.join("\n");
        if existing.ends_with('\n') {
            updated.push('\n');
        }
        return updated;
    }
    if existing
        .lines()
        .any(|line| prefix_table_key(line) == Some("y"))
    {
        return existing.to_string();
    }

    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    match lines
        .iter()
        .position(|line| prefix_table_key(line) == Some(","))
    {
        Some(rename) => {
            let body = &lines[rename];
            let indent = body[..body.len() - body.trim_start().len()].to_string();
            let inserted = block.iter().map(|line| format!("{indent}{line}"));
            lines.splice(rename + 1..rename + 1, inserted);
        }
        None => {
            lines.push(String::new());
            lines.extend(block.iter().map(|line| line.to_string()));
        }
    }
    let mut updated = lines.join("\n");
    if existing.ends_with('\n') || existing.is_empty() {
        updated.push('\n');
    }
    updated
}

// Popup bindings as they read before each named the client whose key opened
// it, as (key, command). Without `-c`, the popup's `tmux` picked a client from
// the server's TMUX_PANE, which could name another client's pane, so the
// popup (and the switch picker's switch-client) acted on another client. A
// prefix-table line binding the key to exactly the old command, in any form
// pmux has written (`bind` or `bind-key`, quoted key or bare), becomes the
// template's binding for that key.
const UNTARGETED_POPUP_BINDINGS: [(&str, &str); 6] = [
    (
        "x",
        r##"run-shell -b 'tmux display-popup -w 60 -h 9 -E "${PMUX_BIN:-pmux} prompt --title \"Kill this process?\" --confirm \"This ends the process and detaches.\" --action Kill --danger -- tmux kill-pane -t \"#{pane_id}\""'"##,
    ),
    (
        "q",
        r##"run-shell -b 'tmux display-popup -w 85% -h 85% -E "${PMUX_BIN:-pmux} queue-ui \"#{pane_id}\""'"##,
    ),
    (
        "c",
        r##"run-shell -b 'tmux display-popup -w 60 -h 9 -E "${PMUX_BIN:-pmux} prompt --title \"Command for a new process\" --action Run --split -- ~/.config/pmux/new-process.sh \"#{pane_id}\""'"##,
    ),
    (
        ",",
        r##"run-shell -b 'tmux display-popup -w 60 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
    ),
    (
        "I",
        r##"run-shell -b 'tmux display-popup -w 90% -h 90% -T " ai-auto history " -E "${AI_AUTO_BIN:-ai-auto} history -t \"#{pane_id}\" -S \"#{socket_path}\" --tui --client \"#{client_name}\""'"##,
    ),
    (
        "s",
        r##"run-shell -b 'h=$(tmux display-message -p "#{client_height}"); [ "$h" -gt 1 ] && h=$((h-1)); tmux display-popup -x 0 -y 0 -w 100% -h "$h" -E "${PMUX_BIN:-pmux} attach"'"##,
    ),
];

fn upgrade_popup_client_target(existing: &str) -> String {
    let template = include_str!("../templates/tmux.conf");
    existing
        .split_inclusive('\n')
        .map(|line| {
            let body = line.trim_end_matches('\n');
            let Some(("prefix", key, command)) = binding_command(body) else {
                return line.to_string();
            };
            if !UNTARGETED_POPUP_BINDINGS.contains(&(key, command)) {
                return line.to_string();
            }
            let new = template
                .lines()
                .map(str::trim_start)
                .find(|candidate| binding_key(candidate) == Some(key))
                .expect("template binds every popup key");
            let indent = &body[..body.len() - body.trim_start().len()];
            format!("{indent}{new}{}", &line[body.len()..])
        })
        .collect()
}

const SERVER_GUARD_MARKER: &str = "# >>> pmux server guard >>>";

// Wrap a tmux.conf that predates the server guard in the guard lines from
// the template, so loading it into another tmux server does nothing.
fn upgrade_server_guard(existing: &str) -> String {
    let template = include_str!("../templates/tmux.conf");
    let mut updated = existing.to_string();
    for legacy in LEGACY_RELOAD_BINDINGS {
        updated = updated.replace(legacy, NOTED_RENAME_RELOAD[1]);
    }
    if updated.contains(SERVER_GUARD_MARKER) {
        return updated;
    }
    wrap_in_guard(template, SERVER_GUARD_MARKER, &updated)
}

const INTEGRATION_GUARD_MARKER: &str = "# >>> pmux integration guard >>>";

// Wrap an outer-tmux.conf that predates its guard, so loading it into pmux's
// own server does nothing.
fn upgrade_integration_guard(existing: &str) -> String {
    if existing.contains(INTEGRATION_GUARD_MARKER) {
        return existing.to_string();
    }
    wrap_in_guard(
        include_str!("../templates/outer-tmux.conf"),
        INTEGRATION_GUARD_MARKER,
        existing,
    )
}

// Put `body` between the opening and closing guard blocks that `template`
// marks with `open_marker` (`# >>> NAME >>>`, each block ending with the
// matching `# <<< NAME <<<` line).
fn wrap_in_guard(template: &str, open_marker: &str, body: &str) -> String {
    let close_line = format!("{}\n", open_marker.replace(">>>", "<<<"));
    let open_start = template.find(open_marker).expect("template guard");
    let open_end = template[open_start..]
        .find(&close_line)
        .map(|i| open_start + i + close_line.len())
        .expect("template guard close");
    let close_start = template.rfind(open_marker).expect("template closing guard");
    format!(
        "{}{}\n{}",
        &template[open_start..open_end],
        body.trim_end_matches('\n'),
        &template[close_start..]
    )
}

// The key of a `bind [-N note] [-T table] KEY command` line.
fn binding_key(line: &str) -> Option<&str> {
    binding_parts(line).map(|(_, key)| key)
}

// The key table and key a `bind`/`unbind` line names, skipping its flags. Keys
// may be quoted, as tmux.rb writes them (`bind-key -N "note" "d" ...`).
fn binding_parts(line: &str) -> Option<(&str, &str)> {
    binding_command(line).map(|(table, key, _)| (table, key))
}

// A `bind` line's table, key, and the command after the key, trimmed.
fn binding_command(line: &str) -> Option<(&str, &str, &str)> {
    // Words are slices of `line`, so a word's end offset locates the command.
    let end_of = |word: &str| word.as_ptr() as usize - line.as_ptr() as usize + word.len();
    let mut words = line.split_whitespace();
    if !matches!(
        words.next(),
        Some("bind" | "bind-key" | "unbind" | "unbind-key")
    ) {
        return None;
    }
    let mut table = "prefix";
    while let Some(word) = words.next() {
        match word {
            "-n" => table = "root",
            "-T" => table = unquote(words.next()?),
            "-N" => {
                // Notes are double-quoted and may contain spaces.
                let mut note = words.next()?;
                while !(note.len() > 1 && note.ends_with('"')) {
                    note = words.next()?;
                }
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {}
            key => return Some((table, unquote(key), line[end_of(key)..].trim())),
        }
    }
    None
}

fn unquote(word: &str) -> &str {
    word.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(word)
}

// The prefix-table key a `bind`/`unbind` line names, skipping its flags.
// Matching the parsed key rather than any word keeps a `[` inside a shell
// test, or a copy-mode `v`, from counting as a prefix binding.
fn prefix_table_key(line: &str) -> Option<&str> {
    binding_parts(line)
        .filter(|(table, _)| *table == "prefix")
        .map(|(_, key)| key)
}

// Respect any existing binding for `key`; otherwise install the new shortcut.
fn append_binding_unless_bound(existing: &str, key: &str, snippet: &str) -> String {
    let mut updated = existing.to_string();
    let has_key = updated
        .lines()
        .any(|line| prefix_table_key(line) == Some(key));
    if !has_key {
        updated.push('\n');
        updated.push_str(snippet);
    }
    updated
}

fn remove_unreferenced_retired_status_helpers(cfg_dir: &Path, tmux_conf: &str) -> Result<()> {
    for helper in RETIRED_STATUS_HELPERS {
        if tmux_conf.contains(helper) {
            continue;
        }
        let path = cfg_dir.join(helper);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

fn is_newer_generated_version(on_disk: &str) -> bool {
    match (
        on_disk.parse::<u32>(),
        GENERATED_CONFIG_VERSION.parse::<u32>(),
    ) {
        (Ok(disk), Ok(ours)) => disk > ours,
        _ => false,
    }
}

// Create the pmux config directory and all default files (tmux.conf, scripts, theme)
// if they don't already exist. Called at the start of most pmux commands.
pub(crate) fn ensure_pmux_config() -> Result<()> {
    let cfg_dir = pmux_config_dir()?;
    compile_tmux_conf_rb_if_newer();

    let generated_version = fs::read_to_string(generated_config_version_path()?)
        .unwrap_or_default()
        .trim()
        .to_string();
    // A newer pmux already generated these files. Long-lived processes from an
    // older binary (event feeds, other tools) must not downgrade them, or binaries
    // fight and rewrite the config on every poll.
    if is_newer_generated_version(&generated_version) {
        return Ok(());
    }
    // Fast path: generated files are current and all expected files exist.
    if generated_version == GENERATED_CONFIG_VERSION
        && cfg_dir.is_dir()
        && cfg_dir.join("tmux.conf").exists()
        && cfg_dir.join("theme.conf").exists()
        && cfg_dir.join("status.conf").exists()
        && cfg_dir.join("outer-tmux.conf").exists()
        && cfg_dir.join("new-process.sh").exists()
        && cfg_dir.join("help.sh").exists()
        && cfg_dir.join("ai-attn-setup.sh").exists()
        && cfg_dir.join("clear-legacy-ai-attn-bell-hook.sh").exists()
        && cfg_dir.join("bin/tmux").exists()
        && cfg_dir.join("server.conf").exists()
        && cfg_dir.join("rb/pmux.rb").exists()
    {
        return Ok(());
    }

    fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("failed to create {}", cfg_dir.display()))?;

    // pmux's tmux.rb module, for tmux.rb users; written first so a
    // tmux.conf.rb compiled below builds on the current version.
    let rb_module = rb_module_path()?;
    fs::create_dir_all(rb_module.parent().expect("rb module has a directory"))
        .with_context(|| format!("failed to create {}", cfg_dir.join("rb").display()))?;
    write_config_file(&rb_module, &default_rb_module())
        .with_context(|| format!("failed to write {}", rb_module.display()))?;

    let conf = tmux_conf_path()?;
    let compiled_from_rb = tmux_conf_rb_path()?.exists();
    if compiled_from_rb {
        // tmux.conf is tmux.rb output: recompile it rather than editing it,
        // which the next compile would undo anyway.
        if let Err(err) = compile_tmux_conf_rb() {
            eprintln!("pmux: {err}");
        }
    }
    if !conf.exists() {
        write_config_file(&conf, default_tmux_conf())
            .with_context(|| format!("failed to write {}", conf.display()))?;
    } else if !compiled_from_rb {
        let existing = fs::read_to_string(&conf)
            .with_context(|| format!("failed to read {}", conf.display()))?;
        match upsert_managed_tmux_block(&existing) {
            Ok(with_block) => {
                let updated = upgrade_popup_client_target(&upgrade_copy_name_binding(
                    &upgrade_moved_bindings(&upgrade_server_guard(&upgrade_binding_notes(
                        &upgrade_help_binding(&upgrade_auto_binding(&upgrade_queue_binding(
                            &upgrade_managed_tmux_commands(&with_block),
                        ))),
                    ))),
                ));
                if updated != existing {
                    write_config_file(&conf, &updated)
                        .with_context(|| format!("failed to update {}", conf.display()))?;
                }
            }
            Err(err) => warn_skipped_managed_block(&conf, &err),
        }
    }

    let current_tmux_conf =
        fs::read_to_string(&conf).with_context(|| format!("failed to read {}", conf.display()))?;
    remove_unreferenced_retired_status_helpers(&cfg_dir, &current_tmux_conf)?;

    let theme_conf = theme_conf_path()?;
    if !theme_conf.exists() {
        write_config_file(&theme_conf, default_theme_conf())
            .with_context(|| format!("failed to write {}", theme_conf.display()))?;
    }

    write_config_file(&status_conf_path()?, default_status_conf())
        .context("failed to write generated status.conf")?;

    let outer_tmux_conf = outer_tmux_conf_path()?;
    if !outer_tmux_conf.exists() {
        write_config_file(&outer_tmux_conf, &default_outer_tmux_conf())
            .with_context(|| format!("failed to write {}", outer_tmux_conf.display()))?;
    } else {
        let existing = fs::read_to_string(&outer_tmux_conf)
            .with_context(|| format!("failed to read {}", outer_tmux_conf.display()))?;
        match upsert_managed_outer_sidebar_block(&existing)
            .map(|text| upgrade_integration_guard(&text))
        {
            Ok(updated) if updated != existing => {
                write_config_file(&outer_tmux_conf, &updated)
                    .with_context(|| format!("failed to update {}", outer_tmux_conf.display()))?;
            }
            Ok(_) => {}
            Err(err) => warn_skipped_managed_block(&outer_tmux_conf, &err),
        }
    }

    // The outer integration is now generated by `pmux tmux-init` (invoked from
    // the managed block), so the standalone sidebar.conf is obsolete. Remove any
    // copy left by an earlier install rather than leaving a stale, unsourced
    // file to masquerade as live config.
    let _ = fs::remove_file(sidebar_conf_path()?);

    write_executable(&new_process_script_path()?, default_new_process_script())?;
    write_executable(&help_script_path()?, default_help_script())?;
    write_executable(
        &ai_attn_setup_script_path()?,
        default_ai_attn_setup_script(),
    )?;
    write_executable(
        &legacy_bell_hook_cleanup_script_path()?,
        default_legacy_bell_hook_cleanup_script(),
    )?;
    write_config_file(
        &server_conf_path()?,
        include_str!("../templates/server.conf"),
    )
    .context("failed to write generated server.conf")?;
    // A server started by an older pmux has no marker, and the guarded
    // tmux.conf would refuse to load into it on the next reload.
    if crate::tmux::pmux_server_running().unwrap_or(false) {
        let _ = crate::tmux::tmux_cmd()
            .args(["set-option", "-g", "@pmux_server", "1"])
            .output();
    }
    let guard_dir = tmux_guard_dir()?;
    fs::create_dir_all(&guard_dir)
        .with_context(|| format!("failed to create {}", guard_dir.display()))?;
    write_executable(
        &guard_dir.join("tmux"),
        include_str!("../templates/tmux-guard.sh"),
    )?;
    write_config_file(
        &generated_config_version_path()?,
        &format!("{GENERATED_CONFIG_VERSION}\n"),
    )
    .context("failed to write generated config version")?;

    Ok(())
}

// Return the default tmux.conf content for the pmux server.
fn default_tmux_conf() -> &'static str {
    include_str!("../templates/tmux.conf")
}

// Return the default theme.conf content for pmux status bar styling.
fn default_theme_conf() -> &'static str {
    include_str!("../templates/theme.conf")
}

// Return the generated shell-free pmux status formats.
fn default_status_conf() -> &'static str {
    include_str!("../templates/status.conf")
}

// Return the optional config users can source from their outer tmux server.
fn default_outer_tmux_conf() -> String {
    render_pmux_bin(include_str!("../templates/outer-tmux.conf"))
}

// Return the default new-process.sh script for creating new processes from tmux keybindings.
fn default_new_process_script() -> &'static str {
    include_str!("../templates/new-process.sh")
}

// Return the default help.sh script shown by the `h` keybinding.
fn default_help_script() -> &'static str {
    include_str!("../templates/help.sh")
}

// Return the default ai-attn-setup.sh script that bootstraps the tmux-ai-attn plugin.
fn default_ai_attn_setup_script() -> &'static str {
    include_str!("../templates/ai-attn-setup.sh")
}

fn default_legacy_bell_hook_cleanup_script() -> &'static str {
    include_str!("../templates/clear-legacy-ai-attn-bell-hook.sh")
}

// Ensure the user's shell rc file sources pmux's wrap.sh and completions.
// Returns the rc path and whether it was modified.
pub(crate) fn ensure_wrap_sourced() -> Result<(Option<PathBuf>, bool)> {
    let rc_path = select_shell_rc_path()?;
    let Some(rc_path) = rc_path else {
        return Ok((None, false));
    };

    let existing = read_text_for_rewrite(&rc_path)?.unwrap_or_default();
    let shell_name = detect_shell_name();
    let source_block = build_source_block(&shell_name);
    let (next, changed) = upsert_pmux_source_block(&existing, &source_block)
        .with_context(|| format!("not updating {}", rc_path.display()))?;
    if changed {
        write_config_file(&rc_path, &next)
            .with_context(|| format!("failed to update {}", rc_path.display()))?;
    }

    Ok((Some(rc_path), changed))
}

// Extract the shell name (e.g. "zsh", "bash") from the SHELL env var.
fn detect_shell_name() -> String {
    let shell = env::var("SHELL").unwrap_or_default();
    Path::new(&shell)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_string()
}

// Build the shell snippet that loads wrap.sh, installs the prompt hook that
// reloads it after `pmux wrap`/`unwrap`, and sets up completions for the
// detected shell.
fn build_source_block(shell_name: &str) -> String {
    let completions_line = match shell_name {
        "zsh" => "\neval \"$(command pmux completions zsh)\"",
        "bash" => "\neval \"$(command pmux completions bash)\"",
        _ => "",
    };
    format!(
        "{}{completions_line}",
        include_str!("../templates/shell-integration.sh.in").trim_end()
    )
}

// Determine the appropriate shell rc file (.zshrc, .bashrc, or .profile) to modify.
fn select_shell_rc_path() -> Result<Option<PathBuf>> {
    let home = env::var("HOME").context("HOME is not set")?;
    let shell_name = detect_shell_name();

    let mut candidates: Vec<PathBuf> = Vec::new();
    match shell_name.as_str() {
        "zsh" => candidates.push(PathBuf::from(&home).join(".zshrc")),
        "bash" => candidates.push(PathBuf::from(&home).join(".bashrc")),
        _ => return Ok(None),
    }
    candidates.push(PathBuf::from(&home).join(".profile"));

    if let Some(existing) = candidates.iter().find(|p| p.exists()) {
        let rc = sourced_rc_with_pmux_block(existing, &home).unwrap_or_else(|| existing.clone());
        return Ok(Some(rc));
    }
    Ok(candidates.first().cloned())
}

// When the rc file has no pmux block but a file it sources does, return that
// file so the block is refreshed where it lives. This keeps a shared ~/.zshrc
// (e.g. a team dotfiles repo that sources ~/.zshrc.local) free of a
// duplicate block after the user moves it into their personal file.
fn sourced_rc_with_pmux_block(rc: &Path, home: &str) -> Option<PathBuf> {
    let content = fs::read_to_string(rc).ok()?;
    if has_pmux_wrap_block(&content) {
        return None;
    }
    content
        .lines()
        .flat_map(|line| sourced_paths(line, home))
        .find(|path| {
            path != rc && fs::read_to_string(path).is_ok_and(|text| has_pmux_wrap_block(&text))
        })
}

fn has_pmux_wrap_block(content: &str) -> bool {
    content
        .lines()
        .any(|line| line.trim_end() == PMUX_WRAP_START)
}

// Paths a shell rc line sources via `source FILE` or `. FILE`, including the
// `[[ -s FILE ]] && source FILE` guard form. Only literal paths and ones
// starting with ~, $HOME or ${HOME} are understood; anything else is skipped.
fn sourced_paths(line: &str, home: &str) -> Vec<PathBuf> {
    let line = line.trim();
    if line.starts_with('#') {
        return Vec::new();
    }
    line.split(['&', '|', ';'])
        .filter_map(|segment| {
            let segment = segment.trim();
            let rest = segment
                .strip_prefix("source ")
                .or_else(|| segment.strip_prefix(". "))?;
            let token = rest.split_whitespace().next()?;
            let token = token.trim_matches(|c| c == '"' || c == '\'');
            let expanded = if let Some(tail) = token.strip_prefix("~/") {
                format!("{home}/{tail}")
            } else if let Some(tail) = token
                .strip_prefix("${HOME}")
                .or_else(|| token.strip_prefix("$HOME"))
            {
                format!("{home}{tail}")
            } else {
                token.to_string()
            };
            (!expanded.contains('$') && expanded.starts_with('/')).then(|| PathBuf::from(expanded))
        })
        .collect()
}

// Insert or replace the pmux source block in shell rc content.
// Returns (new_content, changed). Idempotent.
pub(crate) fn upsert_pmux_source_block(
    existing: &str,
    source_block: &str,
) -> Result<(String, bool)> {
    let start = PMUX_WRAP_START;
    let end = PMUX_WRAP_END;

    let mut output: Vec<String> = strip_marked_blocks(existing, start, end)?
        .into_iter()
        .map(str::to_string)
        .collect();

    while output.last().is_some_and(|l| l.trim().is_empty()) {
        output.pop();
    }
    if !output.is_empty() {
        output.push(String::new());
    }
    output.push(start.to_string());
    output.push(source_block.to_string());
    output.push(end.to_string());
    output.push(String::new());

    let updated_content = output.join("\n");
    let changed = updated_content != existing;
    Ok((updated_content, changed))
}

#[cfg(test)]
mod tests {
    use super::{
        GENERATED_CONFIG_VERSION, MANAGED_TMUX_BLOCK, default_ai_attn_setup_script,
        ensure_pmux_config, ensure_wrap_sourced, managed_outer_sidebar_block, pmux_config_dir,
        sourced_paths, tmux_conf_path, upgrade_managed_tmux_commands,
        upsert_managed_outer_sidebar_block, upsert_managed_tmux_block, upsert_pmux_source_block,
    };
    use std::fs;
    use std::path::PathBuf;

    // Set up a temporary HOME with a specified shell for isolated config tests.
    fn with_temp_home<F: FnOnce(&std::path::Path)>(shell: &str, f: F) {
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());
        env.set("SHELL", shell);
        f(tmp.path());
    }

    // --- path helpers ---

    // Tests that pmux_config_dir returns ~/.config/pmux.
    #[test]
    fn test_pmux_config_dir_uses_home() {
        with_temp_home("/bin/sh", |tmp| {
            let dir = pmux_config_dir().unwrap();
            assert_eq!(dir, tmp.join(".config").join("pmux"));
        });
    }

    // Tests that tmux_conf_path is inside the pmux config directory.
    #[test]
    fn test_tmux_conf_path_under_config_dir() {
        with_temp_home("/bin/sh", |tmp| {
            let conf = tmux_conf_path().unwrap();
            assert_eq!(conf, tmp.join(".config").join("pmux").join("tmux.conf"));
        });
    }

    // --- ensure_pmux_config ---

    // Tests that ensure_pmux_config creates all expected config files.
    #[test]
    fn test_ensure_pmux_config_creates_expected_files() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().expect("ensure_pmux_config should succeed");
            let cfg = tmp.join(".config").join("pmux");
            assert!(
                cfg.join("tmux.conf").exists(),
                "tmux.conf should be created"
            );
            let tmux_conf = fs::read_to_string(cfg.join("tmux.conf")).expect("read tmux.conf");
            assert!(
                tmux_conf.contains(r#"set-option -s set-clipboard "on""#),
                "tmux.conf should enable clipboard integration"
            );
            assert!(
                tmux_conf.contains(r#"set-option -gw mode-keys "vi""#),
                "tmux.conf should use vi copy-mode keys"
            );
            assert!(
                tmux_conf.contains(r#"set-option -g status-keys "emacs""#),
                "tmux.conf should use emacs-style prompt keys so Esc cancels prompts"
            );
            assert!(
                tmux_conf.contains(
                    r#"bind-key -T "copy-mode-vi" "y" send-keys -X copy-selection-and-cancel"#
                ),
                "tmux.conf should bind y to copy in vi copy-mode"
            );
            assert!(
                cfg.join("theme.conf").exists(),
                "theme.conf should be created"
            );
            assert!(
                cfg.join("status.conf").exists(),
                "status.conf should be created"
            );
            assert!(
                cfg.join("outer-tmux.conf").exists(),
                "outer-tmux.conf should be created"
            );
            assert!(
                !cfg.join("sidebar.conf").exists(),
                "sidebar.conf is obsolete and must not be generated"
            );
            assert!(!cfg.join("session-age.sh").exists());
            assert!(!cfg.join("session-label.sh").exists());
            assert!(
                cfg.join("new-process.sh").exists(),
                "new-process.sh should be created"
            );
            assert!(cfg.join("help.sh").exists(), "help.sh should be created");
            assert!(!cfg.join("ai-attn-age.sh").exists());
            assert!(
                cfg.join("ai-attn-setup.sh").exists(),
                "ai-attn-setup.sh should be created"
            );
            assert!(
                cfg.join("clear-legacy-ai-attn-bell-hook.sh").exists(),
                "legacy bell hook cleanup script should be created"
            );
            assert_eq!(
                fs::read_to_string(cfg.join(".generated-version"))
                    .unwrap()
                    .trim(),
                GENERATED_CONFIG_VERSION
            );
        });
    }

    // Tests that calling ensure_pmux_config twice doesn't fail.
    #[test]
    fn test_ensure_pmux_config_is_idempotent() {
        with_temp_home("/bin/sh", |_| {
            ensure_pmux_config().expect("first call");
            ensure_pmux_config().expect("second call should not fail");
        });
    }

    #[test]
    fn test_ensure_pmux_config_removes_obsolete_sidebar_conf() {
        with_temp_home("/bin/sh", |tmp| {
            let cfg = tmp.join(".config").join("pmux");
            fs::create_dir_all(&cfg).expect("create cfg dir");
            let stale = cfg.join("sidebar.conf");
            fs::write(&stale, "# stale generated sidebar integration\n").expect("write stale");
            assert!(stale.exists());
            ensure_pmux_config().expect("ensure_pmux_config should succeed");
            assert!(
                !stale.exists(),
                "an obsolete sidebar.conf from an earlier install should be removed"
            );
        });
    }

    #[test]
    fn test_missing_generated_file_refreshes_helpers_but_preserves_user_configs() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let cfg = tmp.join(".config/pmux");
            fs::write(cfg.join("help.sh"), "# custom helper\n").unwrap();
            fs::write(cfg.join("theme.conf"), "# custom theme\n").unwrap();
            fs::write(cfg.join("outer-tmux.conf"), "# custom outer config\n").unwrap();
            fs::remove_file(cfg.join("new-process.sh")).unwrap();

            ensure_pmux_config().unwrap();

            assert!(
                fs::read_to_string(cfg.join("help.sh"))
                    .unwrap()
                    .contains("key-hints"),
                "a missing generated file should refresh all managed helpers"
            );
            assert_eq!(
                fs::read_to_string(cfg.join("theme.conf")).unwrap(),
                "# custom theme\n"
            );
            let outer = fs::read_to_string(cfg.join("outer-tmux.conf")).unwrap();
            assert!(outer.contains("# custom outer config"));
            assert!(
                outer.contains(&managed_outer_sidebar_block()),
                "the marked sidebar block should be added without replacing custom outer config"
            );
        });
    }

    // Tests that generated helper scripts have executable permissions.
    #[cfg(unix)]
    #[test]
    fn test_ensure_pmux_config_scripts_are_executable() {
        use std::os::unix::fs::PermissionsExt;
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let cfg = tmp.join(".config").join("pmux");
            for script in &[
                "new-process.sh",
                "help.sh",
                "ai-attn-setup.sh",
                "clear-legacy-ai-attn-bell-hook.sh",
            ] {
                let mode = fs::metadata(cfg.join(script)).unwrap().permissions().mode();
                assert!(mode & 0o111 != 0, "{script} should be executable");
            }
        });
    }

    // Tests that existing config files are preserved on re-run.
    #[test]
    fn test_ensure_pmux_config_does_not_overwrite_existing() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let conf = tmp.join(".config").join("pmux").join("tmux.conf");
            fs::write(&conf, "# custom config").unwrap();
            ensure_pmux_config().unwrap();
            let contents = fs::read_to_string(&conf).unwrap();
            assert_eq!(
                contents, "# custom config",
                "should not overwrite existing tmux.conf"
            );
        });
    }

    #[test]
    fn test_ensure_pmux_config_migrates_managed_files_only() {
        with_temp_home("/bin/sh", |tmp| {
            let cfg = tmp.join(".config/pmux");
            fs::create_dir_all(&cfg).unwrap();
            fs::write(cfg.join("tmux.conf"), "# custom config\nset -g mouse off\n").unwrap();
            fs::write(cfg.join("theme.conf"), "# custom theme\n").unwrap();
            fs::write(cfg.join("outer-tmux.conf"), "# custom outer config\n").unwrap();
            fs::write(cfg.join("help.sh"), "#!/bin/sh\necho stale\n").unwrap();

            ensure_pmux_config().unwrap();

            let conf = fs::read_to_string(cfg.join("tmux.conf")).unwrap();
            assert!(conf.contains("# custom config"));
            assert!(conf.contains("set -g mouse off"));
            assert!(conf.contains(MANAGED_TMUX_BLOCK));
            assert_eq!(
                fs::read_to_string(cfg.join("theme.conf")).unwrap(),
                "# custom theme\n"
            );
            let outer = fs::read_to_string(cfg.join("outer-tmux.conf")).unwrap();
            assert!(outer.contains("# custom outer config"));
            assert!(outer.contains(&managed_outer_sidebar_block()));
            assert!(
                !fs::read_to_string(cfg.join("help.sh"))
                    .unwrap()
                    .contains("echo stale")
            );
            assert_eq!(
                fs::read_to_string(cfg.join("ai-attn-setup.sh")).unwrap(),
                default_ai_attn_setup_script(),
                "config migrations must refresh the managed ai-attn setup helper"
            );
        });
    }

    #[test]
    fn test_upsert_managed_tmux_block_replaces_existing_block() {
        let existing = format!("set -g mouse off\n\n{MANAGED_TMUX_BLOCK}\n");
        let updated = upsert_managed_tmux_block(&existing).unwrap();
        assert!(updated.contains("set -g mouse off"));
        assert_eq!(updated.matches(MANAGED_TMUX_BLOCK).count(), 1);
    }

    #[test]
    fn test_upsert_managed_outer_sidebar_block_preserves_custom_content() {
        let existing = "set -g mouse off\n\n# >>> pmux managed sidebar >>>\nstale\n# <<< pmux managed sidebar <<<\n";
        let updated = upsert_managed_outer_sidebar_block(existing).unwrap();
        assert!(updated.contains("set -g mouse off"));
        assert_eq!(updated.matches(&managed_outer_sidebar_block()).count(), 1);
        assert_eq!(
            updated,
            upsert_managed_outer_sidebar_block(&updated).unwrap()
        );
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_backgrounds_plugin_setup() {
        let existing = r#"if-shell 'command -v ai-attn' \
  'run-shell "~/.config/pmux/ai-attn-setup.sh"'"#;
        let updated = upgrade_managed_tmux_commands(existing);
        assert!(updated.contains(r#"run-shell -b "~/.config/pmux/ai-attn-setup.sh""#));
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_adds_low_churn_refresh_policy() {
        let updated = upgrade_managed_tmux_commands(super::AI_ATTN_SETUP_MARKER);
        assert_eq!(
            updated,
            format!(
                "{}\n{}",
                super::AI_ATTN_REFRESH_POLICY,
                super::AI_ATTN_SETUP_MARKER
            )
        );
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_preserves_explicit_refresh_policy() {
        let existing = format!(
            "set -g @ai_attn_refresh_client \"on\"\n{}",
            super::AI_ATTN_SETUP_MARKER
        );
        assert_eq!(upgrade_managed_tmux_commands(&existing), existing);
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_replaces_generated_status_jobs() {
        let updated = upgrade_managed_tmux_commands(super::LEGACY_STATUS_LEFT);
        assert_eq!(updated, super::NATIVE_STATUS_SOURCE);
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_preserves_custom_status() {
        let custom = r#"set -g status-left '#(my-custom-status)'"#;
        assert_eq!(upgrade_managed_tmux_commands(custom), custom);
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_uses_default_shell_binding() {
        assert_eq!(
            upgrade_managed_tmux_commands(super::LEGACY_ZSH_BINDING),
            super::DEFAULT_SHELL_BINDING
        );
    }

    #[test]
    fn test_upgrade_managed_tmux_commands_preserves_custom_shell_binding() {
        let custom = r#"bind C run-shell -b "~/.config/pmux/new-process.sh '#{pane_id}' bash""#;
        assert_eq!(upgrade_managed_tmux_commands(custom), custom);
    }

    #[test]
    fn test_generated_version_upgrade_installs_shell_free_status() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let cfg = tmp.join(".config/pmux");
            fs::write(
                cfg.join("tmux.conf"),
                format!("{}\n", super::LEGACY_STATUS_LEFT),
            )
            .unwrap();
            fs::write(cfg.join(".generated-version"), "5\n").unwrap();
            for helper in super::RETIRED_STATUS_HELPERS {
                fs::write(cfg.join(helper), "#!/bin/sh\n").unwrap();
            }

            ensure_pmux_config().unwrap();

            let tmux_conf = fs::read_to_string(cfg.join("tmux.conf")).unwrap();
            assert!(tmux_conf.contains(super::NATIVE_STATUS_SOURCE));
            assert!(!tmux_conf.contains("session-label.sh"));
            assert!(!tmux_conf.contains("session-age.sh"));
            assert!(!tmux_conf.contains("ai-attn-age.sh"));
            for helper in super::RETIRED_STATUS_HELPERS {
                assert!(!cfg.join(helper).exists(), "{helper} should be retired");
            }
        });
    }

    #[test]
    fn test_generated_version_upgrade_uses_default_shell_binding() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let cfg = tmp.join(".config/pmux");
            fs::write(cfg.join("tmux.conf"), super::LEGACY_ZSH_BINDING).unwrap();
            fs::write(cfg.join(".generated-version"), "10\n").unwrap();

            ensure_pmux_config().unwrap();

            let tmux_conf = fs::read_to_string(cfg.join("tmux.conf")).unwrap();
            // The upgrade also gives the binding its key hints note.
            let command = super::DEFAULT_SHELL_BINDING
                .strip_prefix("bind C ")
                .unwrap();
            assert!(tmux_conf.contains(&format!(
                "bind-key -N \"process: new process with a shell\" \"C\" {command}"
            )));
            assert!(!tmux_conf.contains(super::LEGACY_ZSH_BINDING));
        });
    }

    #[test]
    fn test_generated_version_upgrade_preserves_referenced_legacy_helper() {
        with_temp_home("/bin/sh", |tmp| {
            ensure_pmux_config().unwrap();
            let cfg = tmp.join(".config/pmux");
            fs::write(
                cfg.join("tmux.conf"),
                "set -g status-left '#(~/.config/pmux/session-age.sh)'\n",
            )
            .unwrap();
            fs::write(cfg.join("session-age.sh"), "#!/bin/sh\n").unwrap();
            fs::write(cfg.join(".generated-version"), "7\n").unwrap();

            ensure_pmux_config().unwrap();

            assert!(cfg.join("session-age.sh").exists());
        });
    }

    // --- ensure_wrap_sourced ---

    // Tests that ensure_wrap_sourced creates .zshrc with the pmux source block for zsh.
    #[test]
    fn test_ensure_wrap_sourced_zsh_creates_zshrc() {
        with_temp_home("/bin/zsh", |tmp| {
            let (rc_path, changed) = ensure_wrap_sourced().unwrap();
            assert!(changed, "should have updated rc on first call");
            let rc = rc_path.expect("should return a rc path");
            // zsh → .zshrc (since it doesn't exist yet, picks first candidate)
            assert_eq!(rc.file_name().unwrap(), ".zshrc");
            let contents = fs::read_to_string(&rc).unwrap();
            assert!(
                contents.contains("pmux/wrap.sh"),
                "rc should source wrap.sh"
            );
            assert!(contents.contains("# >>> pmux wrap >>>"));
            // Verify the file exists under our temp home
            assert!(rc.starts_with(tmp));
        });
    }

    // Tests that calling ensure_wrap_sourced twice reports no change on the second call.
    #[test]
    fn test_ensure_wrap_sourced_idempotent() {
        with_temp_home("/bin/zsh", |_| {
            let (rc1, changed1) = ensure_wrap_sourced().unwrap();
            assert!(changed1);
            let (rc2, changed2) = ensure_wrap_sourced().unwrap();
            assert!(!changed2, "second call should report no change");
            assert_eq!(rc1, rc2, "same rc path on both calls");
        });
    }

    #[test]
    fn test_ensure_wrap_sourced_skips_unsupported_shells() {
        with_temp_home("/usr/bin/fish", |tmp| {
            let (rc_path, changed) = ensure_wrap_sourced().unwrap();
            assert_eq!(rc_path, None);
            assert!(!changed);
            assert!(!tmp.join(".profile").exists());
        });
    }

    // Regression: rc files were rewritten with fs::write, which replaced
    // nothing atomically. The atomic replacement must keep a symlinked
    // dotfile a symlink (updating its target) and keep the file's mode.
    #[cfg(unix)]
    #[test]
    fn test_ensure_wrap_sourced_keeps_symlinked_rc_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        with_temp_home("/bin/zsh", |tmp| {
            let dotfiles = tmp.join("dotfiles");
            fs::create_dir_all(&dotfiles).unwrap();
            let real = dotfiles.join("zshrc");
            fs::write(&real, "export KEEP=1\n").unwrap();
            fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::fs::symlink("dotfiles/zshrc", tmp.join(".zshrc")).unwrap();

            let (_, changed) = ensure_wrap_sourced().unwrap();
            assert!(changed);
            let link_meta = fs::symlink_metadata(tmp.join(".zshrc")).unwrap();
            assert!(
                link_meta.file_type().is_symlink(),
                ".zshrc must stay a symlink"
            );
            let content = fs::read_to_string(&real).unwrap();
            assert!(content.starts_with("export KEEP=1\n"));
            assert!(content.contains("pmux"));
            let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o600);
            let leftovers: Vec<_> = fs::read_dir(&dotfiles)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(leftovers, vec![std::ffi::OsString::from("zshrc")]);
        });
    }

    // A shared ~/.zshrc that sources a personal file holding the pmux block
    // must not grow a second block; the personal copy is refreshed instead.
    #[test]
    fn test_ensure_wrap_sourced_updates_block_in_sourced_file() {
        with_temp_home("/bin/zsh", |tmp| {
            let shared = "export SHARED=1\n[[ -s \"${HOME}/.zshrc.local\" ]] && source \"${HOME}/.zshrc.local\"\n";
            fs::write(tmp.join(".zshrc"), shared).unwrap();
            let personal = tmp.join(".zshrc.local");
            fs::write(
                &personal,
                "alias vi=nvim\n# >>> pmux wrap >>>\nold block\n# <<< pmux wrap <<<\n",
            )
            .unwrap();

            let (rc_path, changed) = ensure_wrap_sourced().unwrap();
            assert!(changed);
            assert_eq!(rc_path.as_deref(), Some(personal.as_path()));
            assert_eq!(fs::read_to_string(tmp.join(".zshrc")).unwrap(), shared);
            let content = fs::read_to_string(&personal).unwrap();
            assert!(content.starts_with("alias vi=nvim\n"));
            assert!(content.contains("pmux/wrap.sh"));
            assert!(!content.contains("old block"));
        });
    }

    // The rc file's own block wins, and a sourced file without a block (or a
    // commented-out source line) never redirects the write.
    #[test]
    fn test_ensure_wrap_sourced_prefers_rc_block_and_ignores_comments() {
        with_temp_home("/bin/zsh", |tmp| {
            let block_file = "# >>> pmux wrap >>>\nold\n# <<< pmux wrap <<<\n";
            fs::write(tmp.join(".commented"), block_file).unwrap();
            fs::write(tmp.join(".plain"), "export PLAIN=1\n").unwrap();
            fs::write(
                tmp.join(".zshrc"),
                "# source ~/.commented\nsource ~/.plain\n",
            )
            .unwrap();

            let (rc_path, _) = ensure_wrap_sourced().unwrap();
            assert_eq!(rc_path, Some(tmp.join(".zshrc")));
            assert_eq!(
                fs::read_to_string(tmp.join(".commented")).unwrap(),
                block_file
            );

            fs::write(tmp.join(".zshrc.local"), block_file).unwrap();
            fs::write(
                tmp.join(".zshrc"),
                format!("source $HOME/.zshrc.local\n{block_file}"),
            )
            .unwrap();
            let (rc_path, _) = ensure_wrap_sourced().unwrap();
            assert_eq!(rc_path, Some(tmp.join(".zshrc")));
            assert_eq!(
                fs::read_to_string(tmp.join(".zshrc.local")).unwrap(),
                block_file
            );
        });
    }

    #[test]
    fn test_sourced_paths_parses_common_forms() {
        let home = "/h";
        assert_eq!(
            sourced_paths("source ~/.a", home),
            vec![PathBuf::from("/h/.a")]
        );
        assert_eq!(
            sourced_paths(". \"$HOME/.b\"", home),
            vec![PathBuf::from("/h/.b")]
        );
        assert_eq!(
            sourced_paths("[[ -s \"${HOME}/.c\" ]] && source \"${HOME}/.c\"", home),
            vec![PathBuf::from("/h/.c")]
        );
        assert_eq!(
            sourced_paths("source /etc/d", home),
            vec![PathBuf::from("/etc/d")]
        );
        assert!(sourced_paths("# source ~/.a", home).is_empty());
        assert!(sourced_paths("source $ZDOTDIR/.e", home).is_empty());
        assert!(sourced_paths("source relative", home).is_empty());
        assert!(sourced_paths("export SOURCE=1", home).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_config_writes_preserve_mode_and_keep_scripts_executable() {
        use std::os::unix::fs::PermissionsExt;
        with_temp_home("/bin/sh", |_| {
            ensure_pmux_config().unwrap();
            let cfg = pmux_config_dir().unwrap();
            let conf = cfg.join("tmux.conf");
            fs::set_permissions(&conf, fs::Permissions::from_mode(0o640)).unwrap();
            fs::set_permissions(cfg.join("help.sh"), fs::Permissions::from_mode(0o600)).unwrap();
            // Force the slow path, which rewrites managed files.
            fs::write(cfg.join(".generated-version"), "0\n").unwrap();
            fs::write(&conf, "# custom config\n").unwrap();
            ensure_pmux_config().unwrap();
            let rewritten = fs::read_to_string(&conf).unwrap();
            // The upgrade wraps the user's content in the server guard.
            assert!(rewritten.starts_with(super::SERVER_GUARD_MARKER));
            assert!(rewritten.contains("%else\n# <<< pmux server guard <<<\n# custom config\n"));
            assert!(rewritten.contains("# >>> pmux managed lifecycle >>>"));
            let mode =
                |name: &str| fs::metadata(cfg.join(name)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode("tmux.conf"), 0o640);
            assert_eq!(mode("help.sh"), 0o755);
            assert_eq!(mode("status.conf"), 0o644);
        });
    }

    // --- build_source_block ---

    // The block must not define a `pmux` function: it shadowed the binary in
    // `which pmux`. A prompt hook reloads wrap.sh instead.
    #[test]
    fn test_source_block_reloads_from_prompt_hook() {
        let block = super::build_source_block("zsh");
        assert!(
            block.contains("Do not edit or copy the installed block"),
            "managed shell integration should explain its ownership policy"
        );
        assert!(
            !block.contains("pmux()"),
            "block must not shadow the binary"
        );
        assert!(block.contains("add-zsh-hook precmd __pmux_wrap_reload"));
        assert!(block.contains("PROMPT_COMMAND"));
    }

    // Runs the real block in each available shell: a wrap defines the
    // function at the next prompt, an unwrap removes it, `$?` survives the
    // hook, and `pmux` still resolves to the binary.
    #[test]
    fn test_source_block_hook_tracks_wrap_sh_in_real_shells() {
        let script = r#"
. "$BLOCK"
w="$HOME/.config/pmux/wrap.sh"
printf '# pmux wrap definitions\n# pmux-wrap:foo\nfoo() { echo wrapped; }\n# end-pmux-wrap:foo\n' >"$w"
__pmux_wrap_reload
foo
false
__pmux_wrap_reload
echo "rc=$?"
printf '# pmux wrap definitions\n' >"$w"
__pmux_wrap_reload
type foo >/dev/null 2>&1 && echo foo-still-defined
type pmux 2>/dev/null | grep -q function && echo pmux-is-function
echo done
"#;
        let mut ran = 0;
        for shell in ["bash", "zsh"] {
            let tmp = tempfile::tempdir().unwrap();
            fs::create_dir_all(tmp.path().join(".config/pmux")).unwrap();
            let block = tmp.path().join("block.sh");
            fs::write(&block, super::build_source_block("none")).unwrap();
            let Ok(out) = std::process::Command::new(shell)
                .args(["-u", "-c", script])
                .env("HOME", tmp.path())
                .env("BLOCK", &block)
                .env_remove("PROMPT_COMMAND")
                .output()
            else {
                continue;
            };
            ran += 1;
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                stdout,
                "wrapped\nrc=1\ndone\n",
                "{shell} stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(ran > 0, "need bash or zsh to exercise the shell block");
    }

    const SRC: &str = r#"[ -f "$HOME/.config/pmux/wrap.sh" ] && . "$HOME/.config/pmux/wrap.sh""#;

    // T1: config block insertion edge cases

    // Tests that upserting into an empty file creates the block.
    #[test]
    fn test_upsert_into_empty_file() {
        let (out, changed) = upsert_pmux_source_block("", SRC).unwrap();
        assert!(changed);
        assert!(out.contains("# >>> pmux wrap >>>"));
        assert!(out.contains(SRC));
        assert!(out.contains("# <<< pmux wrap <<<"));
    }

    // Tests that existing rc content before the block is preserved.
    #[test]
    fn test_upsert_preserves_existing_content_before_block() {
        let existing = "export FOO=bar\nexport BAZ=qux\n";
        let (out, changed) = upsert_pmux_source_block(existing, SRC).unwrap();
        assert!(changed);
        assert!(out.starts_with("export FOO=bar\nexport BAZ=qux"));
        assert!(out.contains(SRC));
    }

    // Tests that updating the source block replaces old content.
    #[test]
    fn test_upsert_replaces_old_source_line_on_update() {
        let old_src = "source ~/.config/pmux/wrap.sh";
        let (first, _) = upsert_pmux_source_block("", old_src).unwrap();
        // Now update to a new source line.
        let (second, changed) = upsert_pmux_source_block(&first, SRC).unwrap();
        assert!(changed);
        assert!(
            !second.contains(old_src),
            "old source line should be removed"
        );
        assert!(second.contains(SRC));
    }

    // Tests that trailing blank lines before the block are stripped.
    #[test]
    fn test_upsert_strips_trailing_blank_lines_before_block() {
        let existing = "export A=1\n\n\n";
        let (out, _) = upsert_pmux_source_block(existing, SRC).unwrap();
        // There should be exactly one blank line between user content and the marker.
        assert!(!out.contains("export A=1\n\n\n# >>>"));
        assert!(out.contains("export A=1\n\n# >>>"));
    }

    // --- default_ai_attn_setup_script ---

    // Setup script must honor the @ai_attn_plugin_dir tmux option (so
    // dev clones don't need a symlink) and fall back to the env var.
    #[test]
    fn test_ai_attn_setup_script_honors_plugin_dir_option_and_env() {
        let script = super::default_ai_attn_setup_script();
        assert!(
            script.contains("show-option -gqv @ai_attn_plugin_dir"),
            "should read @ai_attn_plugin_dir tmux option so dev clones don't need a symlink"
        );
        assert!(
            script.contains("PMUX_AI_ATTN_PLUGIN_DIR"),
            "should fall back to PMUX_AI_ATTN_PLUGIN_DIR env var"
        );
        assert!(
            script.contains("MANAGED_PLUGIN=1"),
            "should distinguish the default managed clone from explicit dev clones"
        );
        assert!(
            script.contains(r#"$HOME/.local/bin/ai-attn"#),
            "should find the normal user install even when tmux has a stale PATH"
        );
        assert!(
            script.contains("set-option -gq @ai_attn_cli"),
            "should publish the resolved ai-attn path for the plugin"
        );
    }

    #[test]
    fn test_ai_attn_setup_script_pins_compatible_plugin_release() {
        let script = super::default_ai_attn_setup_script();
        assert!(script.contains("REQUIRED_PLUGIN_VERSION=\"v0.4.1\""));
        // Tags are mutable: pmux downloads the tested commit's archive, checks
        // its pinned SHA-256, and verifies the commit before any plugin code
        // runs. No git is needed.
        assert!(
            script.contains("REQUIRED_PLUGIN_COMMIT=\"9b714c32dc868c118ba6656628ca508c76e00385\"")
        );
        assert!(script.contains("archive/${REQUIRED_PLUGIN_COMMIT}.tar.gz"));
        assert!(script.contains("if [ \"$actual\" != \"$REQUIRED_PLUGIN_SHA256\" ]"));
        assert!(!script.contains("git clone"));
        let verify = script
            .find("if [ \"$actual_head\" != \"$REQUIRED_PLUGIN_COMMIT\" ]")
            .expect("HEAD verification");
        let execute = script
            .find("if \"$entry\"; then")
            .expect("plugin entry call");
        assert!(verify < execute, "HEAD must be verified before execution");
    }

    // tmux-ai-attn uses `set-hook -ag` (append). Without pre-clearing,
    // every config reload stacks another hook entry, and if the plugin
    // repo moves the stale entries fail forever at hook fire time.
    // ai-attn-setup.sh must strip any existing hook entries that
    // reference our start-loop.sh before the plugin re-registers.
    #[test]
    fn test_ai_attn_setup_script_clears_stale_hooks() {
        let script = super::default_ai_attn_setup_script();
        assert!(
            script.contains("clear_stale_hooks"),
            "setup script should define the stale-hook cleaner"
        );
        assert!(
            script.contains("start-loop.sh"),
            "cleaner should match on start-loop.sh (ours), not unrelated hooks"
        );
        for hook in &[
            "client-attached",
            "client-session-changed",
            "session-created",
            "after-new-session",
            "alert-bell",
        ] {
            assert!(
                script.contains(hook),
                "should clear stale entries on {hook}"
            );
        }
        assert!(
            script.contains("bell-to-ai-attn.sh"),
            "cleaner should also match the alert-bell hook script"
        );
        assert!(
            script.contains("clear_legacy_bell_hook"),
            "setup should remove the retired bell hook after an older plugin loads"
        );
    }

    // A missing plugin must never be silent — log file + transient
    // display-message so the user notices, but we still exit 0.
    #[test]
    fn test_ai_attn_setup_script_surfaces_missing_plugin() {
        let script = super::default_ai_attn_setup_script();
        assert!(
            script.contains("display-message"),
            "missing-plugin path should surface via tmux display-message"
        );
        assert!(
            script.contains("hooks.log"),
            "missing-plugin path should append to the hooks log"
        );
    }

    // The included scripts are what actually ship to ~/.config/pmux/, so
    // keep a Rust-side check in addition to linting templates/*.sh in CI.
    #[test]
    fn test_default_scripts_pass_shellcheck() {
        let scripts: &[(&str, &str)] = &[
            ("ai-attn-setup.sh", super::default_ai_attn_setup_script()),
            (
                "clear-legacy-ai-attn-bell-hook.sh",
                super::default_legacy_bell_hook_cleanup_script(),
            ),
            ("new-process.sh", super::default_new_process_script()),
            ("help.sh", super::default_help_script()),
        ];
        for (name, body) in scripts {
            run_shellcheck(name, body);
        }
    }

    fn run_shellcheck(name: &str, body: &str) {
        use std::io::Write;
        let tmp =
            std::env::temp_dir().join(format!("pmux-shellcheck-{}-{}", std::process::id(), name));
        let mut f = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => panic!("failed to create {}: {e}", tmp.display()),
        };
        f.write_all(body.as_bytes())
            .expect("write shellcheck input");
        drop(f);

        let output = std::process::Command::new("shellcheck")
            .arg("-s")
            .arg("sh")
            .arg(&tmp)
            .output();
        let _ = std::fs::remove_file(&tmp);

        match output {
            Ok(out) if out.status.success() => {}
            Ok(out) => panic!(
                "shellcheck failed for {name}:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping shellcheck for {name}: shellcheck not installed");
            }
            Err(e) => panic!("failed to invoke shellcheck for {name}: {e}"),
        }
    }

    // --- default_tmux_conf & default_help_script ---

    #[test]
    fn test_default_tmux_conf_has_reload_bind() {
        let conf = super::default_tmux_conf();
        assert!(
            conf.contains(r#""r" source-file"#),
            "prefix+r should reload the pmux tmux config"
        );
        assert!(
            conf.contains("pmux config reloaded"),
            "reload bind should confirm via display-message"
        );
    }

    #[test]
    fn test_default_tmux_conf_uses_default_shell_binding() {
        let conf = super::default_tmux_conf();
        // The template notes the binding (`bind-key -N "..." "C" ...`).
        let command = super::DEFAULT_SHELL_BINDING
            .strip_prefix("bind C ")
            .unwrap();
        assert!(conf.contains(&format!(r#""C" {command}"#)));
        assert!(!conf.contains(super::LEGACY_ZSH_BINDING));
    }

    #[test]
    fn generated_config_migrates_rename_to_the_interactive_popup() {
        let legacy = r##"bind r command-prompt -I "#{session_name}" "run-shell -b '$HOME/.local/bin/pmux rename-session \"#{session_name}\" \"%%\"'""##;
        let upgraded = super::upgrade_managed_tmux_commands(legacy);
        assert!(upgraded.contains("rename-session-ui"));
        assert!(upgraded.contains("display-popup -w 48 -h 9"));
        assert!(!upgraded.contains("display-popup -B"));
        assert!(!upgraded.contains("command-prompt -I"));

        let previous_popup = r##"bind r run-shell -b 'tmux display-popup -B -w 70% -h 12 -E "$HOME/.local/bin/pmux rename-session-ui \"#{session_name}\""'"##;
        let upgraded_popup = super::upgrade_managed_tmux_commands(previous_popup);
        assert!(upgraded_popup.contains("display-popup -w 48 -h 9"));
        assert!(!upgraded_popup.contains("display-popup -B"));

        let native = r##"bind r command-prompt -I "#{session_name}" "rename-session -- '%%'""##;
        let upgraded_native = super::upgrade_managed_tmux_commands(native);
        assert!(upgraded_native.contains("rename-session-ui"));
        assert!(!upgraded_native.contains("command-prompt -I"));
    }

    // Regression: no generated binding may splice a raw session name into a
    // shell command, where `"` or `$(...)` in the name would run as code.
    #[test]
    fn generated_bindings_never_splice_raw_session_names_into_shell() {
        for conf in [
            super::default_tmux_conf().to_string(),
            super::upgrade_managed_tmux_commands(
                r##"bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E "$HOME/.local/bin/pmux rename-session-ui \"#{session_name}\""'
bind k run-shell -b '$HOME/.local/bin/pmux kill-current-and-next "#{session_name}" "#{client_tty}" >/dev/null 2>&1 || true'"##,
            ),
        ] {
            // Only the shell commands count: a tmux command chained after
            // `\;` (like `y`'s display-message) expands formats without a shell.
            for line in conf.lines() {
                for command in line.split(" \\; ").filter(|c| c.contains("run-shell")) {
                    assert!(
                        !command.contains("#{session_name}"),
                        "raw session name in shell binding: {line}"
                    );
                }
            }
            assert!(conf.contains("kill-current-and-next #{q:session_name} #{q:client_tty}"));
        }
    }

    #[test]
    fn outer_bootstrap_line_runs_a_binary_path_with_quotes_dollars_and_hashes() {
        let Some(server) = crate::tmux::TestTmuxServer::start() else {
            return;
        };
        let (bin, log) = server.hostile_binary();
        let slot = super::bootstrap_binary_slot(bin.to_str().unwrap());
        for (name, template) in [
            ("managed sidebar", super::MANAGED_OUTER_SIDEBAR_BLOCK),
            (
                "outer-tmux.conf",
                include_str!("../templates/outer-tmux.conf"),
            ),
        ] {
            let _ = std::fs::remove_file(&log);
            let line = template
                .lines()
                .find(|line| line.contains(super::PMUX_BIN_PLACEHOLDER))
                .unwrap_or_else(|| panic!("{name} has no bootstrap line"))
                .replace(super::PMUX_BIN_PLACEHOLDER, &slot);
            let ran = server.source(&line);
            assert!(
                ran.status.success(),
                "{name}: {}\n{line}",
                String::from_utf8_lossy(&ran.stderr)
            );
            let content = server.wait_for(&log, "tmux-init");
            assert!(
                content.starts_with("tmux-init\n--apply\n--socket\n"),
                "{name}: {content}\n{line}"
            );
        }
        assert!(
            !server.hostile_marker().exists(),
            "tmux expanded #(...) in the binary path"
        );
    }

    #[test]
    fn bootstrap_binary_slot_leaves_plain_paths_unchanged() {
        assert_eq!(
            super::bootstrap_binary_slot("/home/me/.cargo/bin/pmux"),
            "/home/me/.cargo/bin/pmux"
        );
        assert_eq!(
            super::bootstrap_binary_slot("/a b/it's/$x/#y"),
            "/a b/it'\"'\"'s/\\$x/##y"
        );
    }

    // Regression: generated config must not assume ~/.local/bin/pmux, or every
    // hook and binding breaks for cargo/Homebrew/PMUX_BIN_DIR installs.
    #[test]
    fn generated_config_never_hardcodes_the_install_location() {
        for (name, text) in [
            ("tmux.conf", super::default_tmux_conf().to_string()),
            ("managed lifecycle", MANAGED_TMUX_BLOCK.to_string()),
            ("outer-tmux.conf", super::default_outer_tmux_conf()),
            ("managed sidebar", managed_outer_sidebar_block()),
            (
                "new-process.sh",
                super::default_new_process_script().to_string(),
            ),
        ] {
            assert!(
                !text.contains(".local/bin/pmux"),
                "{name} hardcodes the install location"
            );
            assert!(!text.contains("@PMUX_BIN@"), "{name} left a placeholder");
        }
    }

    #[test]
    fn upgrade_moves_hardcoded_bindings_to_pmux_bin() {
        let old = r##"bind l run-shell -b '$HOME/.local/bin/pmux last >/dev/null 2>&1 || true'
bind s run-shell -b 'tmux display-popup -E "$HOME/.local/bin/pmux attach"'"##;
        let upgraded = super::upgrade_managed_tmux_commands(old);
        assert!(!upgraded.contains(".local/bin/pmux"));
        assert!(upgraded.contains(r#"'"${PMUX_BIN:-pmux}" last"#));
        assert!(upgraded.contains(r#"-E "${PMUX_BIN:-pmux} attach""#));
    }

    #[test]
    fn test_default_tmux_conf_sources_shell_free_status() {
        let conf = super::default_tmux_conf();
        let status = super::default_status_conf();

        assert!(conf.contains(r#"source-file "~/.config/pmux/status.conf""#));
        assert!(status.contains("@pmux_session_label_format"));
        assert!(status.contains("@pmux_launch_command"));
        assert!(status.contains("@pmux_session_age_format"));
        assert!(status.contains("@pmux_ai_attn_format"));
        assert!(status.contains("working"));
        assert!(status.contains("#[fg=#a6e3a1]"));
        assert!(status.contains("#{e|%:"), "working state should animate");
        assert!(
            !status
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .any(|line| line.contains("#(")),
            "active status formats must not launch embedded shell commands"
        );
    }

    #[test]
    fn test_default_tmux_conf_snapshots_closed_sessions() {
        let conf = super::default_tmux_conf();
        assert!(
            conf.contains(MANAGED_TMUX_BLOCK),
            "the default config and migration fragment must stay in sync"
        );
        assert!(conf.contains(r#"set-option -s exit-empty "off""#));
        assert!(conf.contains("session-closed[100]"));
        assert!(conf.contains(r#"\"\${PMUX_BIN:-pmux}\" snapshot"#));
        assert!(conf.contains("run-shell -b"));
        assert!(conf.contains(r#"run-shell -b "~/.config/pmux/ai-attn-setup.sh""#));
        assert!(
            conf.contains(r#"[ -x \"\$HOME/.local/bin/ai-attn\" ]"#),
            "backend plugin setup should not depend on a freshly inherited PATH"
        );
    }

    #[test]
    fn test_default_outer_tmux_conf_reproduces_full_integration() {
        let conf = super::default_outer_tmux_conf();
        // The pmux integration (sidebar toggle, hooks, focus hook) is generated
        // by the binary via `pmux tmux-init --apply`, invoked from the managed
        // block — not written statically here — so it can never drift.
        assert!(conf.contains("tmux-init --apply"));
        assert!(!conf.contains("sidebar.conf"));
        assert!(!conf.contains("pane-focus-in[100]"));
        assert!(conf.contains("#{E:@ai_attn_window_segment}"));
        assert!(conf.contains("@ai_attn_seen_flash_seconds \"30\""));
        assert!(conf.contains("@ai_attn_flash_multiplier \"8\""));
        assert!(conf.contains("ai-attn-setup.sh"));
        assert!(conf.contains(&managed_outer_sidebar_block()));
        assert!(conf.contains("clear-legacy-ai-attn-bell-hook.sh"));
        assert!(conf.contains("clear-legacy-ai-attn-bell-hook.sh \"#{socket_path}\""));
    }

    #[test]
    fn test_managed_sidebar_block_applies_binary_generated_integration() {
        // The upserted managed block delegates to the binary and carries no
        // static hook definitions of its own.
        let block = managed_outer_sidebar_block();
        assert!(block.contains("tmux-init --apply"));
        assert!(block.contains("--socket \"#{socket_path}\""));
        assert!(block.contains("clear-legacy-ai-attn-bell-hook.sh"));
        assert!(!block.contains("source-file"));
    }

    #[test]
    fn test_default_help_script_lists_the_live_bindings() {
        let help = super::default_help_script();
        assert!(
            help.contains("key-hints"),
            "the help popup should list the server's bindings, not a fixed copy"
        );
    }

    // Tests that upserting the same content twice produces no change.
    #[test]
    fn test_upsert_pmux_source_block_idempotent() {
        let source_line =
            r#"[ -f "$HOME/.config/pmux/wrap.sh" ] && . "$HOME/.config/pmux/wrap.sh""#;
        let (first, changed_first) = upsert_pmux_source_block("", source_line).unwrap();
        assert!(changed_first);
        let (second, changed_second) = upsert_pmux_source_block(&first, source_line).unwrap();
        assert!(!changed_second);
        assert_eq!(first, second);
        assert!(second.contains("# >>> pmux wrap >>>"));
        assert!(second.contains(source_line));
    }

    // Regression: a rc file pmux cannot decode must never be replaced by just
    // the pmux block.
    #[test]
    fn test_ensure_wrap_sourced_refuses_non_utf8_rc() {
        with_temp_home("/bin/zsh", |home| {
            let rc = home.join(".zshrc");
            let original = b"export EDITOR=vim\n# caf\xe9\nalias ll='ls -l'\n".to_vec();
            fs::write(&rc, &original).unwrap();
            assert!(ensure_wrap_sourced().is_err());
            assert_eq!(fs::read(&rc).unwrap(), original);
        });
    }

    // Regression: deleting only the end marker must not truncate the rest of
    // the user's rc file on the next upsert.
    #[test]
    fn test_ensure_wrap_sourced_refuses_unbalanced_markers() {
        with_temp_home("/bin/zsh", |home| {
            let rc = home.join(".zshrc");
            let original = "# >>> pmux wrap >>>\nold block\nexport KEEP=1\n";
            fs::write(&rc, original).unwrap();
            let err = ensure_wrap_sourced().unwrap_err();
            assert!(format!("{err:#}").contains("without a matching"));
            assert_eq!(fs::read_to_string(&rc).unwrap(), original);
        });
    }

    #[test]
    fn test_managed_block_upsert_refuses_unbalanced_markers() {
        let existing = format!("{}\nset -g mouse off\n", super::MANAGED_TMUX_START);
        assert!(upsert_managed_tmux_block(&existing).is_err());
    }
}

#[cfg(test)]
mod generated_version_tests {
    use super::{GENERATED_CONFIG_VERSION, is_newer_generated_version};
    #[test]
    fn only_a_strictly_newer_generation_blocks_regeneration() {
        let ours: u32 = GENERATED_CONFIG_VERSION.parse().unwrap();
        assert!(is_newer_generated_version(&(ours + 1).to_string()));
        assert!(!is_newer_generated_version(GENERATED_CONFIG_VERSION));
        assert!(!is_newer_generated_version(&(ours - 1).to_string()));
        assert!(!is_newer_generated_version(""));
        assert!(!is_newer_generated_version("garbage"));
    }
}

#[cfg(test)]
mod queue_binding_tests {
    use super::{
        upgrade_auto_binding, upgrade_binding_notes, upgrade_help_binding, upgrade_queue_binding,
    };
    #[test]
    fn queue_binding_migration_is_idempotent_and_preserves_custom_key() {
        let old = "bind r rename-session\n";
        let upgraded = upgrade_queue_binding(old);
        assert!(upgraded.contains("queue-ui"));
        assert_eq!(upgrade_queue_binding(&upgraded), upgraded);
        for custom in [
            "bind q detach-client\n",
            "bind-key -T prefix q display-message custom\n",
        ] {
            assert_eq!(upgrade_queue_binding(custom), custom);
        }
    }

    #[test]
    fn auto_binding_migration_is_idempotent_and_preserves_custom_key() {
        let upgraded = upgrade_auto_binding("bind a run-shell 'ai-attn clear --pane'\n");
        assert!(upgraded.contains(super::AUTO_MODE_MARKER));
        assert!(upgraded.contains("toggle -t \"#{pane_id}\" -S \"#{socket_path}\""));
        assert!(upgraded.contains("history -t"));
        assert_eq!(upgrade_auto_binding(&upgraded), upgraded);
        let custom = "bind Tab display-message custom\nbind I display-message mine\n";
        assert_eq!(upgrade_auto_binding(custom), custom);
    }

    #[test]
    fn auto_binding_migration_makes_every_older_binding_conditional() {
        for old in [
            super::LEGACY_AUTO_TOGGLE,
            super::LEGACY_AUTO_HISTORY,
            super::UNCONDITIONAL_AUTO_TOGGLE,
            super::UNCONDITIONAL_AUTO_HISTORY,
        ] {
            let upgraded = upgrade_auto_binding(&format!("set -g mouse on\n{old}\n"));
            // Only the indented copy inside the conditional block remains.
            assert!(!upgraded.lines().any(|line| line == old), "{old}");
            assert!(upgraded.contains(r#"if-shell "[ -x \"\$HOME/.local/bin/ai-auto\" ]"#));
            assert_eq!(upgraded.matches(r#""Tab" run-shell"#).count(), 1);
            assert_eq!(upgraded.matches(r#""I" run-shell"#).count(), 1);
            assert!(upgraded.contains("--tui --client"));
        }
    }

    #[test]
    fn help_binding_migration_opens_the_hints_popup_on_question_slash_and_h() {
        let old = format!("bind d detach-client\n{}\n", super::LEGACY_HELP);
        let upgraded = upgrade_help_binding(&old);
        assert!(!upgraded.contains("--full"));
        for key in ["?", "/", "h"] {
            let line = format!(r#"bind-key -N "pmux: show key hints" "{key}" run-shell"#);
            assert_eq!(upgraded.matches(&line).count(), 1, "{key}: {upgraded}");
        }
        assert!(upgraded.contains("help.sh --open \"#{client_name}\""));
        assert_eq!(upgrade_help_binding(&upgraded), upgraded);
    }

    #[test]
    fn help_binding_migration_preserves_custom_keys() {
        let custom = "bind ? list-keys -N\nbind / display-message mine\nbind h new-window htop\n";
        assert_eq!(upgrade_help_binding(custom), custom);
    }

    #[test]
    fn binding_notes_migration_notes_exact_template_lines_only() {
        let old = concat!(
            "bind d detach-client\n",
            "bind n run-shell -b '\"${PMUX_BIN:-pmux}\" next-process >/dev/null 2>&1 || true'\n",
            "bind p run-shell -b 'my-own-prev-script'\n",
            "bind g run-shell lazygit\n",
        );
        let upgraded = upgrade_binding_notes(old);
        assert!(upgraded.contains("bind-key -N \"process: detach\" \"d\" detach-client\n"));
        assert!(upgraded.contains("bind-key -N \"process: next process\" \"n\" run-shell"));
        assert!(upgraded.contains("bind p run-shell -b 'my-own-prev-script'\n"));
        assert!(upgraded.contains("bind g run-shell lazygit\n"));
        assert!(upgraded.contains("bind-key -N \"process: kill this process and detach\" \"x\" "));
        assert!(
            upgraded.contains("bind-key -N \"copy: copy mode (v select, y copy, q quit)\" \"[\" ")
        );
        assert_eq!(upgrade_binding_notes(&upgraded), upgraded);
    }

    // The stock lines as the previous release wrote them, in tmux.rb's form
    // and in the `bind -N` form of earlier generated configs.
    const OLD_STOCK_KEYS: [&str; 2] = [
        concat!(
            r##"bind-key -N "process: kill this process and detach" "x" confirm-before -p "kill-pane #P? (y/n)" kill-pane"##,
            "\n",
            r##"bind-key -N "process: new process running a command" "c" command-prompt -p "Run command:" "run-shell -b \"~/.config/pmux/new-process.sh '#{pane_id}' %%\"""##,
            "\n",
            r##"bind-key -N "process: rename this process" "r" run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
            "\n",
            r##"bind-key -N "pmux: reload the pmux config" "R" source-file ~/.config/pmux/server.conf \; display-message "pmux config reloaded""##,
            "\n",
        ),
        concat!(
            r##"bind -N "process: new process running a command" c command-prompt -p "Run command:" "run-shell -b \"~/.config/pmux/new-process.sh '#{pane_id}' %%\"""##,
            "\n",
            r##"bind -N "process: rename this process" r run-shell -b 'tmux display-popup -w 48 -h 9 -E "${PMUX_BIN:-pmux} rename-session-ui --pane \"#{pane_id}\""'"##,
            "\n",
            r##"bind -N "pmux: reload the pmux config" R source-file ~/.config/pmux/server.conf \; display-message "pmux config reloaded""##,
            "\n",
            r##"bind -N "process: kill this process and detach" x confirm-before -p "kill-pane #P? (y/n)" kill-pane"##,
            "\n",
        ),
    ];

    fn template_binding(key: &str) -> &'static str {
        include_str!("../templates/tmux.conf")
            .lines()
            .find(|line| super::binding_key(line) == Some(key))
            .unwrap()
    }

    #[test]
    fn stock_keys_move_to_their_current_bindings_in_every_form() {
        use super::upgrade_moved_bindings;
        for old in OLD_STOCK_KEYS {
            let upgraded = upgrade_moved_bindings(&format!("set -g mouse on\n{old}"));
            for key in [",", "r", "c", "x"] {
                assert!(
                    upgraded.contains(&format!("{}\n", template_binding(key))),
                    "{key} not upgraded:\n{upgraded}"
                );
            }
            assert!(!upgraded.contains("command-prompt"));
            assert!(!upgraded.contains("confirm-before"));
            assert!(!upgraded.contains("\" R ") && !upgraded.contains("\"R\""));
            assert!(upgraded.starts_with("set -g mouse on\n"));
            assert_eq!(upgrade_moved_bindings(&upgraded), upgraded);
        }
    }

    #[test]
    fn unnoted_rename_and_reload_keep_their_keys_until_they_move() {
        use super::{NOTED_RENAME_RELOAD, upgrade_binding_notes, upgrade_moved_bindings};
        let [rename, reload] = NOTED_RENAME_RELOAD;
        // Unnoted bindings from before key hints gain notes on their own keys
        // (not the template's current `r`), then move like the noted ones.
        let unnoted = concat!(
            "bind r run-shell -b 'tmux display-popup -w 48 -h 9 -E \"${PMUX_BIN:-pmux} rename-session-ui --pane \\\"#{pane_id}\\\"\"'\n",
            "bind R source-file ~/.config/pmux/server.conf \\; display-message \"pmux config reloaded\"\n",
        );
        let noted = upgrade_binding_notes(unnoted);
        assert!(noted.contains(&format!("{rename}\n")));
        assert!(noted.contains(&format!("{reload}\n")));
        let moved = upgrade_moved_bindings(&noted);
        assert!(moved.contains(template_binding(",")));
        assert!(moved.contains(template_binding("r")));
    }

    #[test]
    fn copy_name_binding_lands_after_rename_with_its_comment() {
        use super::upgrade_copy_name_binding;
        let rename = template_binding(",");
        let old = format!("set -g mouse on\n{rename}\nbind d detach-client\n");
        let upgraded = upgrade_copy_name_binding(&old);
        let y = template_binding("y");
        let comment: String = include_str!("../templates/tmux.conf")
            .lines()
            .skip_while(|line| !line.starts_with("# `y` copies"))
            .take_while(|line| line.starts_with('#'))
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(!comment.is_empty());
        let expected = format!("set -g mouse on\n{rename}\n{comment}{y}\nbind d detach-client\n");
        assert_eq!(upgraded, expected);
        assert_eq!(upgrade_copy_name_binding(&upgraded), upgraded);
    }

    #[test]
    fn copy_name_binding_keeps_indent_and_falls_back_to_the_end() {
        use super::upgrade_copy_name_binding;
        let indented = "if-shell true {\n  bind , command-prompt mine\n}\n";
        let upgraded = upgrade_copy_name_binding(indented);
        assert!(upgraded.contains(&format!("\n  {}\n}}\n", template_binding("y"))));
        assert_eq!(upgrade_copy_name_binding(&upgraded), upgraded);

        let no_rename = "bind d detach-client\n";
        let upgraded = upgrade_copy_name_binding(no_rename);
        assert!(upgraded.starts_with("bind d detach-client\n\n# `y` copies"));
        assert!(upgraded.ends_with(&format!("{}\n", template_binding("y"))));
        assert_eq!(upgrade_copy_name_binding(&upgraded), upgraded);
    }

    #[test]
    fn earlier_copy_name_bindings_become_the_current_one() {
        use super::{LEGACY_COPY_NAME_BINDINGS, upgrade_copy_name_binding};
        let rename = template_binding(",");
        let y = template_binding("y");
        let fresh = upgrade_copy_name_binding(&format!("{rename}\nbind d detach-client\n"));
        for (comment, binding) in LEGACY_COPY_NAME_BINDINGS {
            let comment: String = comment.iter().map(|line| format!("{line}\n")).collect();
            let old = format!("{rename}\n{comment}{binding}\nbind d detach-client\n");
            let upgraded = upgrade_copy_name_binding(&old);
            assert_eq!(upgraded, fresh, "from:\n{old}");
            assert!(upgraded.contains("copy-to-clipboard"));
            assert_eq!(upgrade_copy_name_binding(&upgraded), upgraded);

            // Without its comment only the binding is replaced, keeping its indent.
            let bare = format!("if-shell true {{\n  {binding}\n}}\n");
            assert_eq!(
                upgrade_copy_name_binding(&bare),
                format!("if-shell true {{\n  {y}\n}}\n")
            );

            // An edited earlier version is the user's own binding and stays.
            let config = format!("{}\n", binding.replace("copied", "got"));
            assert_eq!(upgrade_copy_name_binding(&config), config);
        }
    }

    #[test]
    fn copy_name_binding_respects_a_user_bound_or_unbound_y() {
        use super::upgrade_copy_name_binding;
        let rename = template_binding(",");
        for own in [
            "bind y display-message mine",
            "bind-key -T prefix y run-shell x",
            "unbind y",
        ] {
            let config = format!("{rename}\n{own}\n");
            assert_eq!(upgrade_copy_name_binding(&config), config);
        }
        // A copy-mode `y` is a different table and does not count.
        let vi = "bind-key -T copy-mode-vi y send-keys -X copy-selection-and-cancel\n";
        assert!(upgrade_copy_name_binding(vi).contains("copy this process's name"));
    }

    #[test]
    fn default_config_already_has_the_copy_name_binding() {
        use super::upgrade_copy_name_binding;
        let default = include_str!("../templates/tmux.conf");
        assert_eq!(upgrade_copy_name_binding(default), default);
    }

    #[test]
    fn customized_keys_stay_where_they_are() {
        use super::{NOTED_RENAME_RELOAD, upgrade_moved_bindings};
        let [rename, reload] = NOTED_RENAME_RELOAD;
        // A user's own `,` keeps rename on `r`, so reload stays on `R`.
        let own_comma = format!("bind , command-prompt mine\n{rename}\n{reload}\n");
        assert_eq!(upgrade_moved_bindings(&own_comma), own_comma);
        // An edited rename stays on `r`; reload has nowhere to go.
        let edited = format!("bind r run-shell my-rename\n{reload}\n");
        assert_eq!(upgrade_moved_bindings(&edited), edited);
        // An edited reload stays; rename still moves to `,`.
        let custom_reload = format!("{rename}\nbind R run-shell my-reload\n");
        let moved = upgrade_moved_bindings(&custom_reload);
        assert!(moved.contains(template_binding(",")));
        assert!(moved.contains("bind R run-shell my-reload\n"));
        // Edited c and x bindings are left alone.
        let custom =
            "bind c command-prompt -p \"Run:\" \"run-shell 'mine %%'\"\nbind x kill-pane\n";
        assert_eq!(upgrade_moved_bindings(custom), custom);
    }

    #[test]
    fn prompt_bindings_open_a_popup_of_the_prompt_size() {
        let size = format!(
            "-w {} -h {} ",
            crate::tui::PROMPT_POPUP_WIDTH,
            crate::tui::PROMPT_POPUP_HEIGHT
        );
        for key in [",", "c", "x"] {
            assert!(template_binding(key).contains(&size), "{key} popup size");
        }
    }

    #[test]
    fn binding_notes_migration_keeps_custom_x_and_copy_keys() {
        let custom = "bind x kill-session\nbind [ copy-mode -e\n";
        assert_eq!(upgrade_binding_notes(custom), custom);
        let shell_test = "bind s run-shell -b '[ \"$h\" -gt 1 ] && echo'\n";
        assert!(upgrade_binding_notes(shell_test).contains(" \"[\" copy-mode\n"));
    }

    #[test]
    fn every_template_binding_has_a_key_hints_note() {
        for template in [
            include_str!("../templates/tmux.conf"),
            include_str!("../templates/queue-binding.conf"),
            include_str!("../templates/auto-binding.conf"),
            include_str!("../templates/help-binding.conf"),
        ] {
            let prefix_bindings = template
                .lines()
                .map(str::trim_start)
                .filter(|line| line.starts_with("bind"))
                .filter(|line| super::prefix_table_key(line).is_some());
            for line in prefix_bindings {
                assert!(line.contains(" -N \""), "binding without a note: {line}");
            }
        }
    }

    #[test]
    fn prefix_table_key_ignores_command_words_and_other_tables() {
        use super::prefix_table_key;
        let s_binding = r#"bind s run-shell -b 'h=1; [ "$h" -gt 1 ] && echo'"#;
        assert_eq!(prefix_table_key(s_binding), Some("s"));
        assert_eq!(
            prefix_table_key("bind-key -T copy-mode-vi v send-keys"),
            None
        );
        assert_eq!(prefix_table_key("bind -n M-x kill-pane"), None);
        assert_eq!(
            prefix_table_key("bind-key -T prefix q display-message"),
            Some("q")
        );
        assert_eq!(
            prefix_table_key("bind -r -N \"a long note\" Up resize"),
            Some("Up")
        );
        assert_eq!(prefix_table_key("unbind q"), Some("q"));
        assert_eq!(prefix_table_key("set -g mouse on"), None);
    }

    #[test]
    fn binding_key_skips_notes_and_tables() {
        use super::binding_key;
        assert_eq!(
            binding_key(r#"bind -N "a note with spaces" Tab run-shell x"#),
            Some("Tab")
        );
        assert_eq!(binding_key("bind -T prefix I run-shell x"), Some("I"));
        assert_eq!(binding_key("bind q run-shell x"), Some("q"));
    }
}

#[cfg(test)]
mod tmux_guard_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Run the guard as a process in a pmux pane would: with `$TMUX` naming
    /// pmux's server and a terminal as its controlling terminal. The "real"
    /// tmux it finds on PATH just records its arguments.
    fn guard(args: &[&str], extra_env: &[(&str, &str)], tty: bool) -> (bool, String, String) {
        guards_on_path(1, args, extra_env, tty)
    }

    /// [`guard`] with `copies` separate copies of the guard first on PATH,
    /// running the first. A guard that hangs fails the test after 10 seconds.
    fn guards_on_path(
        copies: usize,
        args: &[&str],
        extra_env: &[(&str, &str)],
        tty: bool,
    ) -> (bool, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let guard_dirs: Vec<_> = (0..copies)
            .map(|i| dir.path().join(format!("guard{i}")))
            .collect();
        let real_dir = dir.path().join("real");
        let mut path = Vec::new();
        for guard_dir in &guard_dirs {
            fs::create_dir_all(guard_dir).unwrap();
            let guard = guard_dir.join("tmux");
            fs::write(&guard, include_str!("../templates/tmux-guard.sh")).unwrap();
            fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
            path.push(guard_dir.display().to_string());
        }
        fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("tmux");
        fs::write(
            &real,
            "#!/bin/sh\nprintf 'REAL:'; printf ' [%s]' \"$@\"; echo\n",
        )
        .unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        path.extend([real_dir.display().to_string(), "/usr/bin:/bin".to_string()]);

        let mut cmd = Command::new(guard_dirs[0].join("tmux"));
        cmd.args(args)
            .env_clear()
            .env("PATH", path.join(":"))
            .env("HOME", dir.path())
            .env("TMUX", "/tmp/tmux-1000/pmux,1,0")
            .env("PMUX_BIN", "/bin/false");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        let mut pty = None;
        if tty {
            let (mut leader, mut follower) = (0, 0);
            let opened = unsafe {
                libc::openpty(
                    &mut leader,
                    &mut follower,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(opened, 0, "openpty");
            pty = Some((leader, follower));
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setsid() < 0 || libc::ioctl(follower, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Another test thread that forks while the guard script is still
        // open for writing briefly holds that write handle in its child, and
        // Linux refuses to exec a file open for writing (ETXTBSY). Retry
        // until that child has exec'd and dropped it.
        let mut attempts = 0;
        let mut child = loop {
            match cmd.spawn() {
                Err(err)
                    if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 50 =>
                {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => break result.unwrap(),
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                // The guard runs in its own session; end everything in it.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                panic!("tmux guard {args:?} did not finish within 10 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        if let Some((leader, follower)) = pty {
            unsafe {
                libc::close(leader);
                libc::close(follower);
            }
        }
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    fn refuses_commands_that_would_reconfigure_or_end_pmux() {
        for args in [
            &["source-file", "/home/me/.tmux.conf"][..],
            &["set", "-g", "prefix", "C-a"],
            &["setw", "-g", "mode-keys", "vi"],
            &["bind", "r", "source-file", "x"],
            &["set-hook", "-g", "after-new-window", "x"],
            &["kill-server"],
            &["kill-ser"],
            &["kill-session", "-t", "other"],
            &["list-keys", ";", "unbind", "-a"],
        ] {
            let (ok, stdout, stderr) = guard(args, &[], true);
            assert!(!ok, "{args:?} should be refused");
            assert!(stdout.is_empty(), "{args:?} reached tmux: {stdout}");
            assert!(stderr.contains("pmux: refusing"), "{args:?}: {stderr}");
            assert!(
                stderr.contains("pmux restart --repair"),
                "{args:?}: {stderr}"
            );
        }
    }

    #[test]
    fn passes_everything_else_through_unchanged() {
        // (arguments, extra environment, has a controlling terminal)
        type Case<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)], bool);
        let cases: &[Case] = &[
            (&["display-message", "-p", "hello world"], &[], true),
            (&["set", "-p", "@mine", "1"], &[], true),
            (&["kill-session"], &[], true),
            (
                &["-u", "-L", "mine", "set", "-g", "prefix", "C-a"],
                &[],
                true,
            ),
            (&["source-file", "~/.config/pmux/tmux.conf"], &[], true),
            // No controlling terminal: tmux hooks and run-shell jobs.
            (&["set", "-g", "@plugin_option", "1"], &[], false),
            // Another server.
            (
                &["kill-server"],
                &[("TMUX", "/tmp/tmux-1000/default,1,0")],
                true,
            ),
            (&["kill-server"], &[("PMUX_ALLOW_TMUX", "1")], true),
        ];
        for (args, env, tty) in cases {
            let (ok, stdout, stderr) = guard(args, env, *tty);
            assert!(ok, "{args:?} should pass: {stderr}");
            let expected: String = args.iter().map(|a| format!(" [{a}]")).collect();
            assert_eq!(stdout.trim_end(), format!("REAL:{expected}"), "{args:?}");
        }
    }

    // Regression: each guard skipped only its own directory, so with a second
    // copy on PATH (a test HOME's guard ahead of the user's, when `cargo test`
    // ran in a pmux pane) the two ran each other as "the real tmux" forever.
    #[test]
    fn another_guard_on_path_is_skipped_not_run_as_the_real_tmux() {
        for copies in [2, 3] {
            let (ok, stdout, stderr) = guards_on_path(copies, &["display-message", "x"], &[], true);
            assert!(ok, "{copies} guards: {stderr}");
            assert_eq!(stdout.trim_end(), "REAL: [display-message] [x]");

            let (ok, stdout, stderr) =
                guards_on_path(copies, &["source-file", "/home/me/.tmux.conf"], &[], true);
            assert!(!ok && stdout.is_empty(), "{copies} guards: {stdout}");
            assert!(stderr.contains("refusing"), "{stderr}");
        }
    }
}

#[cfg(test)]
mod server_guard_tests {
    use super::{upgrade_integration_guard, upgrade_server_guard};
    use crate::tmux::TestTmuxServer;

    #[test]
    fn upgrades_wrap_old_files_once_and_move_reload_to_server_conf() {
        let old = format!("set -g mouse on\n{}\n", super::LEGACY_RELOAD_BINDINGS[0]);
        let upgraded = upgrade_server_guard(&old);
        assert!(upgraded.contains("%if \"#{!=:#{@pmux_server},1}\""));
        assert!(upgraded.contains("set -g mouse on\n"));
        assert!(upgraded.contains(r#""R" source-file ~/.config/pmux/server.conf"#));
        let moved = super::upgrade_moved_bindings(&upgraded);
        assert!(moved.contains(r#""r" source-file ~/.config/pmux/server.conf"#));
        assert!(!upgraded.contains("R source-file ~/.config/pmux/tmux.conf"));
        assert!(upgraded.trim_end().ends_with("# <<< pmux server guard <<<"));
        assert_eq!(upgrade_server_guard(&upgraded), upgraded);

        let outer = upgrade_integration_guard("set -g @mine 1\n");
        assert!(outer.contains("%if \"#{==:#{@pmux_server},1}\""));
        assert!(outer.contains("set -g @mine 1\n"));
        assert_eq!(upgrade_integration_guard(&outer), outer);
    }

    /// Each config only applies to the server it is meant for: tmux.conf and
    /// the tmux-init script check the marker server.conf sets.
    #[test]
    fn configs_only_load_into_the_server_they_belong_to() {
        let Some(server) = TestTmuxServer::start() else {
            return;
        };
        let show = |option: &str| {
            String::from_utf8_lossy(&server.run(&["show-options", "-gqv", option]).stdout)
                .trim()
                .to_string()
        };
        // pmux's tmux.conf, loaded into a server without the marker.
        let pmux_conf = include_str!("../templates/tmux.conf")
            .replace(r#"source-file "~/.config/pmux/theme.conf""#, "")
            .replace(r#"source-file "~/.config/pmux/status.conf""#, "");
        assert!(server.source(&pmux_conf).status.success());
        assert_eq!(
            show("prefix"),
            "C-b",
            "pmux's config applied to another server"
        );

        // The tmux-init integration, loaded into pmux's server.
        assert!(server.source("set -g @pmux_server 1\n").status.success());
        let init = crate::sidebar::tmux_init_script(std::path::Path::new("/usr/bin/pmux"));
        assert!(server.source(&init).status.success());
        let keys = String::from_utf8_lossy(&server.run(&["list-keys", "-T", "prefix"]).stdout)
            .into_owned();
        assert!(
            !keys.contains("toggle-sidebar"),
            "integration applied to pmux's server"
        );

        // And with the marker, pmux's config applies.
        assert!(server.source(&pmux_conf).status.success());
        assert_eq!(show("prefix"), "C-p");
    }
}

#[cfg(test)]
mod tmux_rb_mode_tests {
    use super::{ensure_pmux_config, pmux_config_dir};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // A stand-in for tmux.rb that writes a marker to the --output path.
    fn fake_tmux_rb(dir: &std::path::Path) -> std::path::PathBuf {
        let bin = dir.join("fake-tmux.rb");
        fs::write(
            &bin,
            "#!/bin/sh\n[ \"$1\" = build ] || exit 2\nprintf '# compiled from %s\\n' \"$2\" > \"$4\"\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn writes_the_module_with_the_binary_path_filled_in() {
        crate::commands::test_support::with_temp_home(|_| {
            ensure_pmux_config().unwrap();
            let module = fs::read_to_string(pmux_config_dir().unwrap().join("rb/pmux.rb")).unwrap();
            assert!(module.contains("module Pmux"));
            assert!(
                !module.contains("@PMUX_BIN@"),
                "placeholder left in the module"
            );
            assert!(module.contains("PMUX_BIN = \""));
        });
    }

    #[test]
    fn a_tmux_conf_rb_is_compiled_and_tmux_conf_is_never_edited_in_place() {
        // One guard for HOME and TMUX_RB_BIN: guards share a lock, so this
        // cannot nest inside with_temp_home.
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        env.set("HOME", home);
        {
            let cfg = home.join(".config/pmux");
            fs::create_dir_all(&cfg).unwrap();
            fs::write(cfg.join("tmux.conf.rb"), "Pmux.server\n").unwrap();

            // Without tmux.rb, an old-style tmux.conf is left exactly as is.
            env.set("TMUX_RB_BIN", home.join("missing-tmux.rb"));
            let legacy = "bind d detach-client\n";
            fs::write(cfg.join("tmux.conf"), legacy).unwrap();
            ensure_pmux_config().unwrap();
            assert_eq!(fs::read_to_string(cfg.join("tmux.conf")).unwrap(), legacy);

            // With tmux.rb, the next refresh recompiles it.
            env.set("TMUX_RB_BIN", fake_tmux_rb(home));
            fs::write(cfg.join(".generated-version"), "0\n").unwrap();
            ensure_pmux_config().unwrap();
            let compiled = fs::read_to_string(cfg.join("tmux.conf")).unwrap();
            assert!(compiled.starts_with("# compiled from "), "{compiled}");
            assert!(compiled.contains("tmux.conf.rb"));
        }
    }
}

#[cfg(test)]
mod popup_client_target_tests {
    use super::{UNTARGETED_POPUP_BINDINGS, binding_key, upgrade_popup_client_target};

    const TEMPLATES: [&str; 3] = [
        include_str!("../templates/tmux.conf"),
        include_str!("../templates/queue-binding.conf"),
        include_str!("../templates/auto-binding.conf"),
    ];

    #[test]
    fn every_template_popup_targets_the_client_that_opened_it() {
        for template in TEMPLATES {
            for line in template.lines().filter(|l| l.contains("display-popup")) {
                assert!(
                    line.contains("display-popup -c #{q:client_name} "),
                    "untargeted popup: {line}"
                );
            }
        }
    }

    #[test]
    fn switch_picker_runs_with_the_triggering_pane() {
        let template = TEMPLATES[0];
        let switch = template
            .lines()
            .find(|line| binding_key(line) == Some("s"))
            .unwrap();
        assert!(switch.contains(r#"-E "TMUX_PANE=#{pane_id} ${PMUX_BIN:-pmux} attach""#));
        assert!(!switch.contains("display-message"));
    }

    #[test]
    fn upgrades_each_untargeted_popup_in_every_written_form() {
        for (key, command) in UNTARGETED_POPUP_BINDINGS {
            let new = TEMPLATES[0]
                .lines()
                .map(str::trim_start)
                .find(|line| binding_key(line) == Some(key))
                .unwrap();
            for old in [
                format!(r#"bind-key -N "note" "{key}" {command}"#),
                format!(r#"bind -N "older note" {key} {command}"#),
                format!("bind {key} {command}"),
            ] {
                let existing = format!("set -g mouse on\n  {old}\nbind z kill-server\n");
                let upgraded = upgrade_popup_client_target(&existing);
                assert_eq!(
                    upgraded,
                    format!("set -g mouse on\n  {new}\nbind z kill-server\n"),
                    "from {old}"
                );
                assert_eq!(upgrade_popup_client_target(&upgraded), upgraded);
            }
        }
    }

    #[test]
    fn leaves_other_key_tables_alone() {
        let (key, command) = UNTARGETED_POPUP_BINDINGS[0];
        let root = format!("bind -n {key} {command}\n");
        assert_eq!(upgrade_popup_client_target(&root), root);
    }

    #[test]
    fn leaves_edited_popup_bindings_alone() {
        let custom = "bind-key s run-shell -b 'tmux display-popup -w 50% -E \"pmux attach\"'\n";
        assert_eq!(upgrade_popup_client_target(custom), custom);
    }

    #[test]
    fn server_bootstrap_drops_an_inherited_tmux_pane() {
        let server = include_str!("../templates/server.conf");
        let unset = server.find("set-environment -gu TMUX_PANE").unwrap();
        assert!(unset < server.find("source-file").unwrap());
    }
}
