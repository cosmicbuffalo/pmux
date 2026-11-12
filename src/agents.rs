//! Everything pmux knows about specific AI agent CLIs.
//!
//! pmux is a general process multiplexer; agents are optional enrichment. All
//! per-agent facts live here so the rest of the code asks "which agent is
//! this, and how does it resume / where are its transcripts?" instead of
//! matching on agent names itself:
//!
//! - detection from a command line ([`Agent::detect`]);
//! - native resume commands for cross-restart restore ([`Agent::resume_argv`]);
//! - where Claude Code, Codex and Cortex Code keep their state
//!   ([`claude_config_dir`], [`codex_home`], [`cortex_home`],
//!   [`locate_transcript`]);
//! - launch-time argv adjustments ([`apply_launch_rewrites`]);
//! - the user's opt-in screen text that shows an agent is ready for input
//!   ([`load_ready_texts`], `[agents.<kind>] ready_text` in config.toml).
//!
//! Parsers for a specific agent's transcript format stay with the feature that
//! reads them (activity timelines, history, rename propagation); they get
//! their paths from here.
//!
//! At process and file boundaries (ai-attn records, restore snapshots, the
//! JSON API) an agent is its lowercase [`Agent::name`].

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// An AI agent CLI pmux recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Agent {
    Agy,
    Claude,
    Codex,
    Copilot,
    Cortex,
    Gemini,
    Grok,
    Muse,
    Omp,
    OpenCode,
    Pi,
}

// Detection order matters only for prefixes of one another; `opencode` is
// listed first so it can never be shadowed if a shorter name is added.
const DETECTION_ORDER: [Agent; 11] = [
    Agent::OpenCode,
    Agent::Codex,
    Agent::Claude,
    Agent::Copilot,
    Agent::Cortex,
    Agent::Gemini,
    Agent::Grok,
    Agent::Muse,
    Agent::Omp,
    Agent::Pi,
    Agent::Agy,
];

impl Agent {
    /// Every agent pmux recognizes.
    pub(crate) const ALL: [Agent; 11] = DETECTION_ORDER;

    /// The agent's lowercase name, as used in ai-attn records and snapshots.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Agent::Agy => "agy",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::Cortex => "cortex",
            Agent::Gemini => "gemini",
            Agent::Grok => "grok",
            Agent::Muse => "muse",
            Agent::Omp => "omp",
            Agent::OpenCode => "opencode",
            Agent::Pi => "pi",
        }
    }

    /// Parse an agent name recorded by pmux or ai-attn.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        DETECTION_ORDER
            .into_iter()
            .find(|agent| agent.name() == name)
    }

    /// Identify the agent a command line runs, without allocating.
    ///
    /// Takes the basename of the first whitespace-delimited token and matches
    /// it case-insensitively when it equals an agent name, or when the
    /// character right after the name is non-alphanumeric (`claude-3.5`,
    /// `codex.exe`, `PI-SAFE`). That avoids false positives like `claudette`.
    pub(crate) fn detect(command: &str) -> Option<Self> {
        let token = command.split_whitespace().next()?;
        let basename = match token.rfind('/') {
            Some(pos) => &token[pos + 1..],
            None => token,
        };
        if basename.is_empty() {
            return None;
        }
        DETECTION_ORDER.into_iter().find(|agent| {
            let prefix = agent.name();
            basename.len() >= prefix.len()
                && basename[..prefix.len()].eq_ignore_ascii_case(prefix)
                && (basename.len() == prefix.len()
                    || !basename.as_bytes()[prefix.len()].is_ascii_alphanumeric())
        })
    }

    /// The argv that resumes this agent at `session_id`, or `None` if the
    /// agent has no known resume command or the id is unsafe.
    ///
    /// NOTE: `gemini` is intentionally absent pending confirmation of its
    /// resume flag, and `agy` has no resume flag; restore treats both as
    /// having no resume command.
    pub(crate) fn resume_argv(self, session_id: &str) -> Option<Vec<String>> {
        if !valid_session_id(session_id) {
            return None;
        }
        let id = session_id.to_string();
        let argv = match self {
            Agent::Claude => vec!["claude".into(), "--resume".into(), id],
            // Resume in-process so hooks keep this pane's TMUX_PANE; see
            // `apply_launch_rewrites`.
            Agent::Codex => vec!["codex".into(), "--no-daemon".into(), "resume".into(), id],
            Agent::Cortex => vec!["cortex".into(), "resume".into(), id],
            Agent::OpenCode => vec!["opencode".into(), "--session".into(), id],
            Agent::Muse => vec!["muse".into(), "resume".into(), id],
            // The pi launcher injects `--session-id` only when the invocation
            // does not already select a session, so `--session` here
            // suppresses that and resumes the recorded session instead.
            Agent::Pi => vec!["pi".into(), "--session".into(), id],
            Agent::Omp => vec!["omp".into(), "--session".into(), id],
            // `--resume` takes an optional value; Copilot documents the
            // `--resume=<id>` form.
            Agent::Copilot => vec!["copilot".into(), format!("--resume={id}")],
            Agent::Grok => vec!["grok".into(), "--resume".into(), id],
            Agent::Gemini | Agent::Agy => return None,
        };
        Some(argv)
    }
}

/// Resume argv for an agent recorded by name; see [`Agent::resume_argv`].
pub(crate) fn resume_argv(agent: &str, session_id: &str) -> Option<Vec<String>> {
    Agent::from_name(agent)?.resume_argv(session_id)
}

/// Return true if a session id is safe to interpolate into a resume command.
///
/// Restored commands are shell-joined before being handed to tmux, so ids
/// containing shell metacharacters or whitespace are rejected as
/// defense-in-depth against a malicious or garbage session id.
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// Claude Code's state directory, honoring `CLAUDE_CONFIG_DIR`.
pub(crate) fn claude_config_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
    {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// Codex's state directory, honoring `CODEX_HOME`.
pub(crate) fn codex_home() -> Result<PathBuf> {
    if let Some(dir) = env::var("CODEX_HOME").ok().filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Cortex Code's state directory (`~/.snowflake/cortex`).
pub(crate) fn cortex_home() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".snowflake").join("cortex"))
}

/// Find the transcript file for `(agent, session_id)`.
///
/// The id is the filename stem for Claude (`projects/<mangled-cwd>/<id>.jsonl`,
/// so each project dir is probed rather than reconstructing the mangle) and an
/// embedded segment for Codex (`sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`).
/// Cortex keeps one message per line in `conversations/<id>.history.jsonl`.
/// Other agents' transcripts are not read.
pub(crate) fn locate_transcript(agent: &str, session_id: &str) -> Option<PathBuf> {
    match Agent::from_name(agent)? {
        Agent::Claude => {
            let projects = claude_config_dir().ok()?.join("projects");
            let target = format!("{session_id}.jsonl");
            fs::read_dir(&projects)
                .ok()?
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                .map(|entry| entry.path().join(&target))
                .find(|candidate| candidate.is_file())
        }
        Agent::Codex => find_file_containing(&codex_home().ok()?.join("sessions"), session_id, 4),
        Agent::Cortex => {
            if !valid_session_id(session_id) {
                return None;
            }
            let path = cortex_home()
                .ok()?
                .join("conversations")
                .join(format!("{session_id}.history.jsonl"));
            path.is_file().then_some(path)
        }
        _ => None,
    }
}

/// Recursively search `dir` (up to `depth` levels) for a `.jsonl` file whose
/// name contains `needle`. Depth-bounded so a stray deep tree can't stall.
fn find_file_containing(dir: &Path, needle: &str, depth: usize) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            subdirs.push(path);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.ends_with(".jsonl")
            && name.contains(needle)
        {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs
        .into_iter()
        .find_map(|sub| find_file_containing(&sub, needle, depth - 1))
}

// Codex subcommands that exist to manage or browse the shared daemon itself.
const CODEX_DAEMON_SUBCOMMANDS: &[&str] = &["agents", "app-server", "remote-control"];

/// Adjust an agent's argv before pmux launches it.
///
/// Codex: run in-process instead of on its shared app-server daemon. Codex
/// hooks execute inside whichever process hosts the thread. On the shared
/// daemon that is the environment of the codex that first started it, so
/// every session's ai-attn records carry that session's `TMUX_PANE` and the
/// relay lights up the wrong outer pane. `--no-daemon` keeps hooks in the
/// pmux pane's own process tree. The flag is top-level, so it goes directly
/// after the executable and is accepted ahead of any subcommand. Only an
/// executable named exactly `codex` is rewritten.
pub(crate) fn apply_launch_rewrites(parts: &mut Vec<String>) {
    let Some(program) = parts.first() else {
        return;
    };
    if Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("codex")
    {
        return;
    }
    let args = &parts[1..];
    if args.iter().any(|arg| arg == "--no-daemon")
        || args
            .first()
            .is_some_and(|arg| CODEX_DAEMON_SUBCOMMANDS.contains(&arg.as_str()))
    {
        return;
    }
    parts.insert(1, "--no-daemon".to_string());
}

/// Per-agent ready text from `[agents.<kind>] ready_text` in config.toml,
/// keyed by [`Agent::name`].
///
/// An agent such as Codex fires no hook until its first prompt, so pmux cannot
/// otherwise tell an idle agent at its prompt from one stuck on a startup or
/// approval dialog. When configured, seeing one of these texts on the visible
/// screen confirms readiness (never agent state).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReadyTexts(HashMap<String, Vec<String>>);

impl ReadyTexts {
    pub(crate) fn for_agent(&self, kind: &str) -> &[String] {
        self.0.get(kind).map(Vec::as_slice).unwrap_or(&[])
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawAgentsConfig {
    #[serde(default)]
    agents: HashMap<String, RawAgentConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAgentConfig {
    #[serde(default)]
    ready_text: Option<OneOrMany>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// Parses `[agents]` from config.toml text. Only that table is read, so an
/// error elsewhere in the file does not disable ready text; a malformed
/// `[agents]` table disables it rather than breaking pmux.
pub(crate) fn parse_ready_texts(text: &str) -> ReadyTexts {
    let Ok(raw) = toml::from_str::<RawAgentsConfig>(text) else {
        return ReadyTexts::default();
    };
    ReadyTexts(
        raw.agents
            .into_iter()
            .filter_map(|(kind, agent)| {
                let texts: Vec<String> = match agent.ready_text? {
                    OneOrMany::One(text) => vec![text],
                    OneOrMany::Many(texts) => texts,
                }
                .into_iter()
                .filter(|text| !text.trim().is_empty())
                .collect();
                (!texts.is_empty()).then(|| (kind.to_ascii_lowercase(), texts))
            })
            .collect(),
    )
}

/// Loads ready text from `~/.config/pmux/config.toml`; none when the file is
/// absent or unreadable.
pub(crate) fn load_ready_texts() -> ReadyTexts {
    crate::config::pmux_config_toml_path()
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|text| parse_ready_texts(&text))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_text_config_accepts_one_or_many_and_ignores_the_rest() {
        let texts = parse_ready_texts(
            r#"
[restore]
enabled = "not a bool, but restore's problem"

[agents.codex]
ready_text = "Ask Codex to do anything"

[agents.Claude]
ready_text = ["> ", "  "]
other = 1

[agents.muse]
"#,
        );
        assert_eq!(texts.for_agent("codex"), ["Ask Codex to do anything"]);
        assert_eq!(texts.for_agent("claude"), ["> "]);
        assert!(texts.for_agent("muse").is_empty());
        assert!(texts.for_agent("cortex").is_empty());
    }

    #[test]
    fn malformed_or_missing_ready_text_config_is_empty() {
        assert!(parse_ready_texts("").is_empty());
        assert!(parse_ready_texts("[agents.codex]\nready_text = 3\n").is_empty());
        assert!(parse_ready_texts("not toml [[[").is_empty());
    }

    fn detect(command: &str) -> Option<&'static str> {
        Agent::detect(command).map(Agent::name)
    }

    #[test]
    fn names_round_trip() {
        for agent in DETECTION_ORDER {
            assert_eq!(Agent::from_name(agent.name()), Some(agent));
        }
        assert_eq!(Agent::from_name("bash"), None);
        assert_eq!(Agent::from_name("Claude"), None);
    }

    #[test]
    fn detect_exact_match() {
        assert_eq!(detect("codex"), Some("codex"));
        assert_eq!(detect("claude"), Some("claude"));
        assert_eq!(detect("opencode"), Some("opencode"));
        assert_eq!(detect("gemini"), Some("gemini"));
        assert_eq!(detect("muse"), Some("muse"));
        assert_eq!(detect("pi"), Some("pi"));
        assert_eq!(detect("copilot --yolo"), Some("copilot"));
        assert_eq!(detect("grok"), Some("grok"));
        assert_eq!(detect("/usr/local/bin/agy"), Some("agy"));
        assert_eq!(detect("omp"), Some("omp"));
        assert_eq!(detect("ompa"), None);
        assert_eq!(detect("cortex"), Some("cortex"));
        assert_eq!(
            detect("/home/user/.local/bin/cortex resume abc"),
            Some("cortex")
        );
        // Unrecognized agents return None.
        assert_eq!(detect("amp"), None);
        assert_eq!(detect("goose"), None);
        assert_eq!(detect("aider"), None);
    }

    #[test]
    fn detect_is_case_insensitive() {
        assert_eq!(detect("CODEX"), Some("codex"));
        assert_eq!(detect("Claude"), Some("claude"));
        assert_eq!(detect("OPENCODE"), Some("opencode"));
        assert_eq!(detect("Muse"), Some("muse"));
        assert_eq!(detect("PI-SAFE"), Some("pi"));
    }

    #[test]
    fn detect_strips_path_prefix() {
        assert_eq!(detect("/usr/bin/claude"), Some("claude"));
        assert_eq!(detect("/opt/tools/codex"), Some("codex"));
        assert_eq!(detect("/home/user/.local/bin/pi"), Some("pi"));
    }

    #[test]
    fn detect_ignores_arguments() {
        assert_eq!(detect("codex --model x"), Some("codex"));
        assert_eq!(detect("claude -p foo"), Some("claude"));
    }

    #[test]
    fn detect_has_no_false_positives() {
        assert_eq!(detect("claudette"), None);
        assert_eq!(detect("amplify"), None);
        assert_eq!(detect("goosebumps"), None);
        assert_eq!(detect("aiderx"), None);
    }

    #[test]
    fn detect_empty_and_whitespace() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("  "), None);
    }

    #[test]
    fn detect_requires_a_word_boundary() {
        // A non-alphanumeric suffix still identifies the agent.
        assert_eq!(detect("claude-3.5"), Some("claude"));
        // An alphanumeric suffix makes it a different program.
        assert_eq!(detect("claudeX"), None);
    }

    #[test]
    fn valid_session_id_accepts_typical_ids() {
        assert!(valid_session_id("abc123"));
        assert!(valid_session_id("019-abc_DEF.99"));
        assert!(valid_session_id("codex:thread:42"));
    }

    #[test]
    fn valid_session_id_rejects_injection_and_empty() {
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("abc; rm -rf /"));
        assert!(!valid_session_id("a b"));
        assert!(!valid_session_id("$(whoami)"));
        assert!(!valid_session_id("a`b`"));
        assert!(!valid_session_id(&"x".repeat(257)));
    }

    #[test]
    fn resume_argv_builds_known_agent_commands() {
        let argv = |parts: &[&str]| Some(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>());
        assert_eq!(
            resume_argv("claude", "sess1"),
            argv(&["claude", "--resume", "sess1"])
        );
        assert_eq!(
            resume_argv("codex", "sess2"),
            argv(&["codex", "--no-daemon", "resume", "sess2"])
        );
        assert_eq!(
            resume_argv("cortex", "cx-123"),
            argv(&["cortex", "resume", "cx-123"])
        );
        assert_eq!(
            resume_argv("opencode", "sess3"),
            argv(&["opencode", "--session", "sess3"])
        );
        assert_eq!(
            resume_argv("muse", "01a08985-7790-7d42-a038-73ccbdf85f10"),
            argv(&["muse", "resume", "01a08985-7790-7d42-a038-73ccbdf85f10"])
        );
        assert_eq!(
            resume_argv("pi", "d4c25f0d-14c6-470b-a236-7fefdcb81ff4"),
            argv(&["pi", "--session", "d4c25f0d-14c6-470b-a236-7fefdcb81ff4"])
        );
    }

    #[test]
    fn resume_argv_for_newer_agents() {
        assert_eq!(
            resume_argv("copilot", "abc-123"),
            Some(vec!["copilot".to_string(), "--resume=abc-123".to_string()])
        );
        assert_eq!(
            resume_argv("grok", "abc-123"),
            Some(vec![
                "grok".to_string(),
                "--resume".to_string(),
                "abc-123".to_string()
            ])
        );
        assert_eq!(
            resume_argv("omp", "abc-123"),
            Some(vec![
                "omp".to_string(),
                "--session".to_string(),
                "abc-123".to_string()
            ])
        );
    }

    #[test]
    fn resume_argv_unknown_or_unconfirmed_agent_is_none() {
        assert_eq!(resume_argv("gemini", "sess"), None);
        assert_eq!(resume_argv("agy", "sess"), None);
        assert_eq!(resume_argv("bash", "sess"), None);
    }

    #[test]
    fn resume_argv_rejects_bad_session_id() {
        assert_eq!(resume_argv("claude", "bad; rm -rf /"), None);
        assert_eq!(resume_argv("codex", ""), None);
    }

    #[test]
    fn transcripts_honor_agent_home_overrides_and_skip_unknown_formats() {
        let mut env = crate::EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude-home");
        let codex = dir.path().join("codex-home");
        fs::create_dir_all(claude.join("projects/-work")).unwrap();
        fs::write(claude.join("projects/-work/abc.jsonl"), "{}\n").unwrap();
        fs::create_dir_all(codex.join("sessions/2026/10/01")).unwrap();
        fs::write(
            codex.join("sessions/2026/10/01/rollout-x-thread9.jsonl"),
            "{}\n",
        )
        .unwrap();
        env.set("CLAUDE_CONFIG_DIR", &claude);
        env.set("CODEX_HOME", &codex);
        let found = (
            locate_transcript("claude", "abc"),
            locate_transcript("codex", "thread9"),
            locate_transcript("opencode", "thread9"),
        );
        assert_eq!(found.0, Some(claude.join("projects/-work/abc.jsonl")));
        assert_eq!(
            found.1,
            Some(codex.join("sessions/2026/10/01/rollout-x-thread9.jsonl"))
        );
        // Codex used to be the fallback for every non-Claude agent.
        assert_eq!(found.2, None);
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn codex_launches_get_no_daemon_after_the_executable() {
        let mut parts = argv(&["codex", "--model", "gpt-5"]);
        apply_launch_rewrites(&mut parts);
        assert_eq!(parts, argv(&["codex", "--no-daemon", "--model", "gpt-5"]));

        let mut parts = argv(&["/usr/local/bin/codex", "resume", "--last"]);
        apply_launch_rewrites(&mut parts);
        assert_eq!(
            parts,
            argv(&["/usr/local/bin/codex", "--no-daemon", "resume", "--last"])
        );
    }

    #[test]
    fn codex_no_daemon_is_not_duplicated_or_forced_on_daemon_commands() {
        let mut parts = argv(&["codex", "--no-daemon"]);
        apply_launch_rewrites(&mut parts);
        assert_eq!(parts, argv(&["codex", "--no-daemon"]));

        for subcommand in ["agents", "app-server", "remote-control"] {
            let mut parts = argv(&["codex", subcommand]);
            apply_launch_rewrites(&mut parts);
            assert_eq!(parts, argv(&["codex", subcommand]));
        }
    }

    #[test]
    fn non_codex_launches_are_unchanged() {
        for command in [
            &["claude"][..],
            &["codex-helper", "run"][..],
            &["sh", "-c", "codex"][..],
        ] {
            let mut parts = argv(command);
            apply_launch_rewrites(&mut parts);
            assert_eq!(parts, argv(command));
        }
    }
}
