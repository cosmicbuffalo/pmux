//! `pmux init` and shell wrappers (`wrap`, `unwrap`, `wraps`).

use crate::config::{ensure_pmux_config, ensure_wrap_sourced, pmux_config_dir};
use crate::util::{read_text_for_rewrite, shell_quote, strip_marked_blocks};
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WrapInstallResult {
    pub wrap_path: std::path::PathBuf,
    pub shell_rc_path: Option<std::path::PathBuf>,
    pub rc_updated: bool,
}

/// Generate a shell wrapper function for a command so it routes through pmux.
/// Called by `pmux wrap <command>`.
pub fn wrap(command: &str) -> Result<WrapInstallResult> {
    if command.trim().is_empty() {
        bail!("wrap command cannot be empty");
    }
    if !is_valid_shell_function_name(command) {
        bail!(
            "wrap command '{}' is not a valid shell function name; \
             must start with a letter or underscore and contain only \
             letters, digits, hyphens, underscores, or dots",
            command
        );
    }

    let cfg_dir = pmux_config_dir()?;
    fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("failed to create {}", cfg_dir.display()))?;

    let wrap_path = cfg_dir.join("wrap.sh");
    let marker_start = format!("# pmux-wrap:{}", command);
    let marker_end = format!("# end-pmux-wrap:{}", command);

    let existing = read_text_for_rewrite(&wrap_path)?.unwrap_or_default();
    let mut output: Vec<String> = strip_marked_blocks(&existing, &marker_start, &marker_end)
        .with_context(|| format!("not updating {}", wrap_path.display()))?
        .into_iter()
        .map(str::to_string)
        .collect();

    if !output.iter().any(|l| l.contains("pmux wrap definitions")) {
        output.push("# pmux wrap definitions".to_string());
    }

    output.push(marker_start);
    output.push(format!(
        r#"{}() {{ if [ -n "$TMUX" ] && printf '%s' "$TMUX" | grep -q '/pmux,'; then command {} "$@"; else pmux run --cwd "$PWD" {} "$@"; fi; }}"#,
        command,
        shell_quote(command)?,
        shell_quote(command)?
    ));
    output.push(marker_end);

    let mut body = output.join("\n");
    body.push('\n');

    fs::write(&wrap_path, body)
        .with_context(|| format!("failed to write {}", wrap_path.display()))?;

    let (shell_rc_path, rc_updated) = ensure_wrap_sourced()?;

    Ok(WrapInstallResult {
        wrap_path,
        shell_rc_path,
        rc_updated,
    })
}

/// List all commands currently wrapped via `pmux wrap`. Called by `pmux wraps`.
pub fn wraps() -> Result<Vec<String>> {
    let wrap_path = pmux_config_dir()?.join("wrap.sh");
    let contents = fs::read_to_string(&wrap_path).unwrap_or_default();
    let mut commands = Vec::new();
    for line in contents.lines() {
        if let Some(name) = line.strip_prefix("# pmux-wrap:") {
            commands.push(name.to_string());
        }
    }
    Ok(commands)
}

// Regenerate existing wrapper blocks when pmux itself is upgraded. This keeps
// wrapper behavior consistent without requiring a new wrap command for every
// command the user already wrapped.
fn refresh_wrapped_commands() -> Result<()> {
    for command in wraps()? {
        // A manually edited marker should not make init unusable. Generated
        // wrapper names are valid by construction.
        if is_valid_shell_function_name(&command) {
            wrap(&command)?;
        }
    }
    Ok(())
}

/// Remove a shell wrapper created by `pmux wrap`. Called by `pmux unwrap <command>`.
pub fn unwrap(command: &str) -> Result<PathBuf> {
    let wrap_path = pmux_config_dir()?.join("wrap.sh");
    let existing = read_text_for_rewrite(&wrap_path)?.unwrap_or_default();

    let marker_start = format!("# pmux-wrap:{}", command);
    let marker_end = format!("# end-pmux-wrap:{}", command);

    if !existing.contains(&marker_start) {
        bail!("'{}' is not currently wrapped", command);
    }

    let output = strip_marked_blocks(&existing, &marker_start, &marker_end)
        .with_context(|| format!("not updating {}", wrap_path.display()))?;

    let mut body = output.join("\n");
    body.push('\n');

    fs::write(&wrap_path, &body)
        .with_context(|| format!("failed to write {}", wrap_path.display()))?;

    // Also refresh the shell rc source block so an upgraded pmux replaces
    // an older template (such as the `pmux()` shell function that once
    // shadowed the binary) with the current one.
    let _ = ensure_wrap_sourced();

    Ok(wrap_path)
}

/// Set up pmux config files and ensure shell rc sources wrap.sh.
/// Called by `pmux init`.
pub fn init() -> Result<(Option<PathBuf>, bool)> {
    crate::relay::restart_ai_attn_relay_if_running()?;
    ensure_pmux_config()?;
    refresh_wrapped_commands()?;
    ensure_wrap_sourced()
}

/// Like [`init`], but optionally also installs the cross-restart boot service.
/// Returns the rc path, whether the rc changed, and any boot-service message.
/// Called by `pmux init [--autostart]`.
pub fn init_with_autostart(autostart: bool) -> Result<(Option<PathBuf>, bool, Option<String>)> {
    let (rc_path, changed) = init()?;
    let install_autostart = autostart || crate::restore::load_config().autostart_on_boot;
    let boot_message = if install_autostart {
        Some(crate::restore::install_boot_service()?)
    } else {
        None
    };
    Ok((rc_path, changed, boot_message))
}

/// The row preset `~/.config/pmux/config.toml` names under `[rows]`, if any.
pub fn configured_row_preset() -> Option<String> {
    let path = crate::config::pmux_config_toml_path().ok()?;
    crate::render::configured_preset(&fs::read_to_string(path).ok()?)
}

/// Set `[rows]` in `~/.config/pmux/config.toml` to the preset `name`,
/// leaving the rest of the file as it is.
pub fn set_row_preset(name: &str) -> Result<PathBuf> {
    let path = crate::config::pmux_config_toml_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let edited = crate::render::with_preset(&text, name).map_err(|message| anyhow!(message))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, edited).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// The row presets, one per line, marking the one in use.
pub fn row_preset_list() -> String {
    let current = configured_row_preset().unwrap_or_else(|| "pmux".to_string());
    crate::render::RowLayout::PRESETS
        .iter()
        .map(|(name, about)| {
            let mark = if *name == current { "*" } else { " " };
            format!("{mark} {name:<8} {about}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ask once, at a terminal, which row preset to use, and save the answer.
/// Does nothing when one is already set or nobody is there to answer.
pub fn prompt_for_row_preset() -> Result<Option<String>> {
    use std::io::{BufRead, IsTerminal, Write};
    if configured_row_preset().is_some()
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
    {
        return Ok(None);
    }
    let presets = crate::render::RowLayout::PRESETS;
    println!("How should the sidebar and picker show processes? (`pmux preset` changes it later)");
    for (index, (name, about)) in presets.iter().enumerate() {
        println!("  {}) {name:<8} {about}", index + 1);
    }
    let name = loop {
        print!("Preset [1]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        if std::io::stdin().lock().read_line(&mut answer)? == 0 {
            return Ok(None);
        }
        let answer = answer.trim();
        let chosen = if answer.is_empty() {
            presets.first()
        } else {
            answer
                .parse::<usize>()
                .ok()
                .and_then(|number| presets.get(number.wrapping_sub(1)))
                .or_else(|| presets.iter().find(|(name, _)| *name == answer))
        };
        match chosen {
            Some((name, _)) => break *name,
            None => println!(
                "Enter a number from 1 to {}, or a preset name.",
                presets.len()
            ),
        }
    };
    set_row_preset(name)?;
    Ok(Some(name.to_string()))
}

/// Returns true if `name` is safe to use as a shell function name.
/// Must start with a letter or underscore; subsequent chars may be
/// letters, digits, hyphens, underscores, or dots.
fn is_valid_shell_function_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::commands::test_support::with_temp_home;

    // --- is_valid_shell_function_name ---
    // Tests that valid and invalid shell function names are correctly classified.
    #[test]
    fn test_is_valid_shell_function_name() {
        assert!(is_valid_shell_function_name("codex"));
        assert!(is_valid_shell_function_name("claude"));
        assert!(is_valid_shell_function_name("my-tool"));
        assert!(is_valid_shell_function_name("_private"));
        assert!(is_valid_shell_function_name("tool.v2"));
        assert!(!is_valid_shell_function_name(""));
        assert!(!is_valid_shell_function_name("123cmd"));
        assert!(!is_valid_shell_function_name("-leading-dash"));
        assert!(!is_valid_shell_function_name("evil; rm -rf ~"));
        assert!(!is_valid_shell_function_name("has space"));
        assert!(!is_valid_shell_function_name("has$var"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn init_honors_configured_boot_autostart() {
        with_temp_home(|tmp| {
            let config_dir = tmp.join(".config/pmux");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("config.toml"),
                "[restore]\nautostart_on_boot = true\n",
            )
            .unwrap();

            let (_, _, message) = init_with_autostart(false).expect("init");
            assert!(message.is_some());
            #[cfg(target_os = "linux")]
            assert!(
                tmp.join(".config/systemd/user/pmux-restore.service")
                    .exists()
            );
            #[cfg(target_os = "macos")]
            assert!(
                tmp.join("Library/LaunchAgents/dev.pmux.restore.plist")
                    .exists()
            );
        });
    }

    #[test]
    fn wrap_creates_wrap_sh_with_function() {
        with_temp_home(|tmp| {
            let result = wrap("codex").expect("wrap should succeed");
            assert!(result.wrap_path.exists(), "wrap.sh should be created");
            let contents = fs::read_to_string(&result.wrap_path).unwrap();
            assert!(
                contents.contains("codex()"),
                "should contain function definition"
            );
            assert!(contents.contains("pmux run"), "should call pmux run");
            assert!(
                contents.contains("--cwd \"$PWD\""),
                "wrapper should pass invocation directory"
            );
            assert!(
                contents.contains("# pmux-wrap:codex"),
                "should contain start marker"
            );
            // wrap_path should be under our temp HOME
            assert!(result.wrap_path.starts_with(tmp));
        });
    }

    // Tests that the wrapper bypasses pmux when already inside a pmux session.
    #[test]
    fn wrap_includes_pmux_guard() {
        with_temp_home(|_| {
            wrap("codex").expect("wrap should succeed");
            let path = crate::config::pmux_config_dir().unwrap().join("wrap.sh");
            let contents = fs::read_to_string(&path).unwrap();
            assert!(
                contents.contains("command codex"),
                "should bypass pmux when inside pmux session"
            );
            assert!(
                contents.contains("/pmux,"),
                "should check for pmux socket in TMUX env var"
            );
            assert!(
                contents.contains(r#"pmux run --cwd "$PWD" codex "$@""#),
                "wrapper should forward every argument through pmux"
            );
            assert!(
                !contents.contains(r#"[ "$#" -gt 0 ]"#),
                "wrapper should not bypass pmux merely because arguments are present"
            );
        });
    }

    #[test]
    fn refreshing_wrappers_migrates_argument_bypass_blocks() {
        with_temp_home(|_| {
            let path = crate::config::pmux_config_dir().unwrap().join("wrap.sh");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                r#"# pmux wrap definitions
# pmux-wrap:codex
codex() { if [ "$#" -gt 0 ]; then command codex "$@"; else pmux run codex; fi; }
# end-pmux-wrap:codex
"#,
            )
            .unwrap();

            refresh_wrapped_commands().unwrap();
            let contents = fs::read_to_string(path).unwrap();
            assert!(contents.contains(r#"pmux run --cwd "$PWD" codex "$@""#));
            assert!(!contents.contains(r#"[ "$#" -gt 0 ]"#));
        });
    }

    // Tests that wrapping an empty command is rejected.
    #[test]
    fn wrap_empty_command_is_error() {
        with_temp_home(|_| {
            assert!(wrap("").is_err());
            assert!(wrap("   ").is_err());
        });
    }

    // Tests that wrapping an invalid function name is rejected.
    #[test]
    fn wrap_invalid_name_is_error() {
        with_temp_home(|_| {
            assert!(wrap("123bad").is_err());
            assert!(wrap("has space").is_err());
        });
    }

    // Tests that calling wrap twice for the same command doesn't duplicate it.
    #[test]
    fn wrap_is_idempotent() {
        with_temp_home(|_| {
            wrap("codex").expect("first wrap");
            wrap("codex").expect("second wrap");
            let path = crate::config::pmux_config_dir().unwrap().join("wrap.sh");
            let contents = fs::read_to_string(&path).unwrap();
            // Function definition should appear exactly once.
            assert_eq!(
                contents.matches("codex()").count(),
                1,
                "function should appear exactly once"
            );
        });
    }

    // --- unwrap ---
    // Tests that unwrap removes the function definition and marker from wrap.sh.
    #[test]
    fn unwrap_removes_wrapper_block() {
        with_temp_home(|_| {
            wrap("codex").expect("wrap codex");
            unwrap("codex").expect("unwrap codex");
            let path = crate::config::pmux_config_dir().unwrap().join("wrap.sh");
            let contents = fs::read_to_string(&path).unwrap();
            assert!(
                !contents.contains("codex()"),
                "codex function definition should be gone after unwrap"
            );
            assert!(
                !contents.contains("# pmux-wrap:codex"),
                "codex marker should be gone after unwrap"
            );
        });
    }

    // Tests that unwrapping a command that was never wrapped is an error.
    #[test]
    fn unwrap_not_wrapped_is_error() {
        with_temp_home(|_| {
            assert!(unwrap("nope").is_err());
        });
    }

    // When the rc source-block template changes (e.g. the switch to a
    // prompt hook) older installs would otherwise keep the stale block in
    // .zshrc forever. unwrap() must call ensure_wrap_sourced so an
    // upgrade silently re-writes the block.
    #[test]
    fn unwrap_refreshes_shell_rc_template() {
        with_temp_home(|tmp| {
            wrap("codex").expect("wrap codex");
            // Simulate an older install: overwrite the pmux block in the
            // rc with a prior-template version.
            let rc = tmp.join(".bashrc");
            let stale_block = "\n# >>> pmux wrap >>>\n# legacy block\n# <<< pmux wrap <<<\n";
            fs::write(&rc, stale_block).unwrap();

            unwrap("codex").expect("unwrap codex");

            let contents = fs::read_to_string(&rc).unwrap();
            assert!(
                contents.contains("__pmux_wrap_reload"),
                "unwrap should refresh the rc block to the current template; got:\n{contents}"
            );
            assert!(
                !contents.contains("legacy block"),
                "stale template text should be replaced"
            );
        });
    }

    // Tests that wrapping two different commands keeps both in wrap.sh.
    #[test]
    fn wrap_two_commands_both_present() {
        with_temp_home(|_| {
            wrap("codex").expect("wrap codex");
            wrap("claude").expect("wrap claude");
            let path = crate::config::pmux_config_dir().unwrap().join("wrap.sh");
            let contents = fs::read_to_string(&path).unwrap();
            assert!(
                contents.contains("codex()"),
                "codex function should be present"
            );
            assert!(
                contents.contains("claude()"),
                "claude function should be present"
            );
        });
    }
}
