//! Opt-in guidance for AI agents that run inside pmux.
//!
//! One source, `templates/agent-docs.md`, is installed as a Claude Code skill
//! (`~/.claude/skills/pmux/SKILL.md`) and as a marked section in Codex's
//! `AGENTS.md` and Gemini's `GEMINI.md`. `pmux init` asks once at a terminal;
//! `--agent-docs` / `--no-agent-docs` answer in advance, and the answer is
//! remembered so later runs refresh (or keep away from) the files.
//!
//! pmux only edits what it manages: a SKILL.md without pmux's marker is left
//! alone, other text in AGENTS.md/GEMINI.md is kept, and a user's CLAUDE.md is
//! never touched.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Bump when `templates/agent-docs.md` or the wrapping below changes, so
/// installed copies are rewritten.
const AGENT_DOCS_VERSION: u32 = 4;

const BODY: &str = include_str!("../templates/agent-docs.md");

const SKILL_DESCRIPTION: &str = "Use pmux to start, supervise and message other agents or \
     processes: spawning helpers with pmux run, driving them with capture/send/ask/wait/reply, \
     and cleaning up. Use \
     whenever you spawn, supervise or message another agent or process, or pmux is mentioned.";

const SKILL_MARKER: &str = "<!-- managed by pmux init";
const SECTION_BEGIN: &str = "<!-- >>> pmux agent docs >>>";
const SECTION_END: &str = "<!-- <<< pmux agent docs <<< -->";

/// What `pmux init` should do with agent docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDocsChoice {
    /// Install or refresh, and remember yes (`--agent-docs`).
    Install,
    /// Remove pmux's copies, and remember no (`--no-agent-docs`).
    Remove,
    /// Follow the remembered answer, asking once at a terminal.
    Ask,
}

/// One file pmux changed (or would have, but left alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDocsChange {
    pub path: PathBuf,
    pub action: &'static str,
}

/// Where the guidance goes. Codex honours `$CODEX_HOME`.
struct Targets {
    claude_skill: Option<PathBuf>,
    sections: Vec<PathBuf>,
}

fn targets(home: &Path) -> Targets {
    let claude = home.join(".claude");
    let codex = env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let gemini = home.join(".gemini");
    // Only agents that are set up on this machine get a file.
    Targets {
        claude_skill: claude
            .is_dir()
            .then(|| claude.join("skills").join("pmux").join("SKILL.md")),
        sections: [codex.join("AGENTS.md"), gemini.join("GEMINI.md")]
            .into_iter()
            .filter(|path| path.parent().is_some_and(Path::is_dir))
            .collect(),
    }
}

fn skill_text() -> String {
    format!(
        "---\nname: pmux\ndescription: {SKILL_DESCRIPTION}\n---\n\n\
         {SKILL_MARKER}; version {AGENT_DOCS_VERSION}. `pmux init --no-agent-docs` removes it. \
         Edits are overwritten: copy it to another skill name to customize. -->\n\n{BODY}"
    )
}

fn section_text() -> String {
    format!(
        "{SECTION_BEGIN} managed by pmux init; version {AGENT_DOCS_VERSION}; \
         `pmux init --no-agent-docs` removes it -->\n{BODY}{SECTION_END}\n"
    )
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    // A plain write keeps the permissions of the user's existing file.
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

// `text` with pmux's section replaced by `section` (or removed when None),
// and whether a section was found. Text outside the markers is kept.
fn splice_section(text: &str, section: Option<&str>) -> (String, bool) {
    let Some(start) = text.find(SECTION_BEGIN) else {
        let mut out = text.to_string();
        if let Some(section) = section {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(section);
        }
        return (out, false);
    };
    let end = text[start..]
        .find(SECTION_END)
        .map(|i| start + i + SECTION_END.len())
        .unwrap_or(text.len());
    let end = if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    let before = &text[..start];
    let after = &text[end..];
    let out = match section {
        Some(section) => format!("{before}{section}{after}"),
        // Also drop the blank line pmux put before its section.
        None => format!(
            "{}{after}",
            before
                .strip_suffix('\n')
                .filter(|b| b.ends_with('\n'))
                .unwrap_or(before)
        ),
    };
    (out, true)
}

fn install(home: &Path) -> Result<Vec<AgentDocsChange>> {
    let targets = targets(home);
    let mut changes = Vec::new();
    if let Some(path) = targets.claude_skill {
        let wanted = skill_text();
        match read_optional(&path)? {
            Some(existing) if existing == wanted => {}
            Some(existing) if !existing.contains(SKILL_MARKER) => changes.push(AgentDocsChange {
                path,
                action: "left alone (not managed by pmux)",
            }),
            existing => {
                write(&path, &wanted)?;
                changes.push(AgentDocsChange {
                    path,
                    action: if existing.is_some() {
                        "updated"
                    } else {
                        "installed"
                    },
                });
            }
        }
    }
    for path in targets.sections {
        let existing = read_optional(&path)?.unwrap_or_default();
        let (updated, had) = splice_section(&existing, Some(&section_text()));
        if updated != existing {
            write(&path, &updated)?;
            changes.push(AgentDocsChange {
                path,
                action: if had { "updated" } else { "installed" },
            });
        }
    }
    Ok(changes)
}

fn remove(home: &Path) -> Result<Vec<AgentDocsChange>> {
    let targets = targets(home);
    let mut changes = Vec::new();
    if let Some(path) = targets.claude_skill
        && read_optional(&path)?.is_some_and(|text| text.contains(SKILL_MARKER))
    {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        if let Some(dir) = path.parent() {
            // Only removes the directory if nothing else is in it.
            let _ = fs::remove_dir(dir);
        }
        changes.push(AgentDocsChange {
            path,
            action: "removed",
        });
    }
    for path in targets.sections {
        let Some(existing) = read_optional(&path)? else {
            continue;
        };
        let (updated, had) = splice_section(&existing, None);
        if had {
            write(&path, &updated)?;
            changes.push(AgentDocsChange {
                path,
                action: "removed pmux section from",
            });
        }
    }
    Ok(changes)
}

fn answer_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/pmux/agent_docs"))
}

fn remembered() -> Option<bool> {
    match fs::read_to_string(answer_path().ok()?).ok()?.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn remember(yes: bool) -> Result<()> {
    crate::state::write_state_file(&answer_path()?, if yes { "yes\n" } else { "no\n" })
}

fn ask_at_terminal() -> Result<Option<bool>> {
    use std::io::{BufRead, IsTerminal, Write};
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }
    println!(
        "Install pmux guidance for AI agents (a Claude Code skill and a section in Codex/Gemini \
         AGENTS.md), so agents in pmux know how to spawn and supervise helpers?"
    );
    print!("Install agent docs? [Y/n] (pmux init --no-agent-docs undoes it): ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().lock().read_line(&mut answer)? == 0 {
        return Ok(None);
    }
    Ok(Some(!matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    )))
}

/// Apply `choice` for `pmux init`, returning the files touched.
pub fn apply(choice: AgentDocsChoice) -> Result<Vec<AgentDocsChange>> {
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let install_docs = match choice {
        AgentDocsChoice::Install => true,
        AgentDocsChoice::Remove => false,
        AgentDocsChoice::Ask => match remembered() {
            Some(yes) => yes,
            None => match ask_at_terminal()? {
                Some(yes) => yes,
                // No terminal and no earlier answer: leave everything as is.
                None => return Ok(Vec::new()),
            },
        },
    };
    if choice != AgentDocsChoice::Ask || remembered().is_none() {
        remember(install_docs)?;
    }
    if install_docs {
        install(&home)
    } else if choice == AgentDocsChoice::Remove {
        remove(&home)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch home with each agent's directory. It holds ENV_LOCK with
    /// CODEX_HOME cleared, since another test sets CODEX_HOME and
    /// [`targets`] reads it.
    struct TestHome {
        dir: tempfile::TempDir,
        _env: crate::EnvGuard,
    }

    impl TestHome {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn home() -> TestHome {
        let mut env = crate::EnvGuard::new();
        env.remove("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        for agent in [".claude", ".codex", ".gemini"] {
            fs::create_dir_all(dir.path().join(agent)).unwrap();
        }
        TestHome { dir, _env: env }
    }

    #[test]
    fn installs_refreshes_and_removes_only_managed_text() {
        let home = home();
        let agents = home.path().join(".codex/AGENTS.md");
        fs::write(&agents, "# Mine\n\nKeep this.\n").unwrap();
        let claude_md = home.path().join(".claude/CLAUDE.md");
        fs::write(&claude_md, "user rules\n").unwrap();

        let changes = install(home.path()).unwrap();
        assert_eq!(changes.len(), 3, "{changes:?}");
        let skill = home.path().join(".claude/skills/pmux/SKILL.md");
        let text = fs::read_to_string(&skill).unwrap();
        assert!(text.starts_with("---\nname: pmux\ndescription: "));
        assert!(text.contains("pmux run -d -s"));
        let agents_text = fs::read_to_string(&agents).unwrap();
        assert!(agents_text.starts_with("# Mine\n\nKeep this.\n\n<!-- >>> pmux agent docs"));
        assert!(home.path().join(".gemini/GEMINI.md").exists());

        // Up to date: nothing to do.
        assert!(install(home.path()).unwrap().is_empty());

        // An old version is replaced in place, keeping the user's text.
        let old = agents_text.replace(&format!("version {AGENT_DOCS_VERSION}"), "version 0");
        fs::write(&agents, old).unwrap();
        let changes = install(home.path()).unwrap();
        assert_eq!(changes[0].action, "updated");
        assert_eq!(fs::read_to_string(&agents).unwrap(), agents_text);

        remove(home.path()).unwrap();
        assert!(!skill.exists());
        assert_eq!(
            fs::read_to_string(&agents).unwrap(),
            "# Mine\n\nKeep this.\n"
        );
        assert_eq!(fs::read_to_string(&claude_md).unwrap(), "user rules\n");
    }

    #[test]
    fn an_unmanaged_skill_is_left_alone() {
        let home = home();
        let skill = home.path().join(".claude/skills/pmux/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "my own pmux skill\n").unwrap();
        let changes = install(home.path()).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.path == skill && c.action.contains("left alone"))
        );
        remove(home.path()).unwrap();
        assert_eq!(fs::read_to_string(&skill).unwrap(), "my own pmux skill\n");
    }

    #[test]
    fn missing_agents_get_no_files() {
        let home = tempfile::tempdir().unwrap();
        assert!(install(home.path()).unwrap().is_empty());
        assert!(!home.path().join(".claude").exists());
    }

    #[test]
    fn a_section_alone_in_a_file_removes_cleanly() {
        let (added, _) = splice_section("", Some(&section_text()));
        assert_eq!(splice_section(&added, None), (String::new(), true));
        let (added, _) = splice_section("a\n", Some(&section_text()));
        assert_eq!(splice_section(&format!("{added}b\n"), None).0, "a\nb\n");
    }
}
