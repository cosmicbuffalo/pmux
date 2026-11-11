//! `pmux respawn`: restart an agent in its own pane, resuming its conversation.
//!
//! A long-lived agent keeps the environment and config it started with. After
//! the server's environment is fixed, a hook or settings file changes, or the
//! agent is upgraded, the only way to pick that up is to start it again. Kill
//! plus resume would give it a new process name, pane and host attachment;
//! respawn keeps all of those and swaps only the program running in the pane.

use crate::agents::{apply_launch_rewrites, resume_argv};
use crate::api::build_state_snapshot;
use crate::history::{Sources, live_session_for_pid, process_tree};
use crate::restore::preview_snapshot;
use crate::session::{SessionInfo, session_agent};
use crate::state::write_session_agent_identity;
use crate::tmux::{list_sessions, tmux_cmd, tmux_failure, tmux_format_escape_path};
use crate::util::shell_join;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

/// Which processes `pmux respawn` acts on and how.
#[derive(Debug, Clone, Default)]
pub struct RespawnOptions {
    /// Processes named on the command line (`newest`/`oldest` work too).
    pub names: Vec<String>,
    /// Every process running this agent (`claude`, `codex`, ...).
    pub agent: Option<String>,
    /// Every pmux process.
    pub all: bool,
    pub dry_run: bool,
    /// Also interrupt busy agents and start agents with no resume id fresh.
    pub force: bool,
    pub yes: bool,
    /// Resume this session id instead of the one pmux finds (one process only).
    pub resume_id: Option<String>,
    /// Arguments appended to the relaunch command, such as new agent flags
    /// (one process only).
    pub extra_args: Vec<String>,
}

/// Everything the plan needs to know about one selected process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RespawnCandidate {
    pub name: String,
    pub pane_id: String,
    pub created_epoch: i64,
    pub agent: Option<String>,
    /// The agent's current ai-attn state (`working`, `waiting`, `done`, ...).
    pub state: Option<String>,
    /// The session to resume and where the id came from.
    pub session: Option<(String, &'static str)>,
    /// The recorded launch command, used for a fresh relaunch.
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub cwd_exists: bool,
    /// Named on the command line rather than matched by `--agent`/`--all`.
    pub explicit: bool,
    /// The pane `pmux respawn` itself is running in.
    pub caller: bool,
}

/// One row of the respawn plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnPlanItem {
    pub name: String,
    pub pane_id: String,
    pub created_epoch: i64,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub id_source: Option<&'static str>,
    pub cwd: Option<String>,
    /// The command the pane will run; `None` when the item is skipped.
    pub shell_cmd: Option<String>,
    /// Whether the agent resumes its conversation (vs. starting fresh).
    pub resumed: bool,
    pub caller: bool,
    pub skip: Option<String>,
    /// `shell_cmd` carries arguments given after `--`.
    pub extra_args: bool,
}

impl RespawnPlanItem {
    fn action(&self) -> String {
        match (&self.skip, self.resumed) {
            (Some(reason), _) => format!("skip: {reason}"),
            (None, true) if self.extra_args => {
                format!("resume: {}", self.shell_cmd.as_deref().unwrap_or_default())
            }
            (None, true) => "resume".to_string(),
            (None, false) if self.agent.is_some() => "start fresh".to_string(),
            (None, false) => "relaunch".to_string(),
        }
    }
}

/// The shell command that resumes `session_id` for `agent`, with the same
/// launch rewrites `pmux run` applies (e.g. `codex --no-daemon`).
pub(crate) fn resume_shell_command(agent: &str, session_id: &str) -> Option<String> {
    let mut argv = resume_argv(agent, session_id)?;
    apply_launch_rewrites(&mut argv);
    Some(shell_join(&argv))
}

/// Decide what to do with each candidate. Pure, so every skip rule is
/// unit-testable. The calling pane always comes last: respawning it ends the
/// `pmux respawn` process itself.
pub(crate) fn plan_respawn(candidates: &[RespawnCandidate], force: bool) -> Vec<RespawnPlanItem> {
    let mut plan: Vec<RespawnPlanItem> = candidates
        .iter()
        .map(|c| {
            let mut item = RespawnPlanItem {
                name: c.name.clone(),
                pane_id: c.pane_id.clone(),
                created_epoch: c.created_epoch,
                agent: c.agent.clone(),
                session_id: c.session.as_ref().map(|(id, _)| id.clone()),
                id_source: c.session.as_ref().map(|(_, source)| *source),
                cwd: c.cwd.clone(),
                shell_cmd: None,
                resumed: false,
                caller: c.caller,
                skip: None,
                extra_args: false,
            };
            item.skip = skip_reason(c, force);
            if item.skip.is_some() {
                return item;
            }
            let resume = match (&c.agent, &c.session) {
                (Some(agent), Some((id, _))) => resume_shell_command(agent, id),
                _ => None,
            };
            match resume {
                Some(cmd) => {
                    item.shell_cmd = Some(cmd);
                    item.resumed = true;
                }
                None => match &c.command {
                    Some(command) => item.shell_cmd = Some(command.clone()),
                    None => item.skip = Some("no recorded launch command".to_string()),
                },
            }
            item
        })
        .collect();
    plan.sort_by_key(|item| item.caller);
    plan
}

fn skip_reason(c: &RespawnCandidate, force: bool) -> Option<String> {
    let Some(agent) = c.agent.as_deref() else {
        return (!c.explicit).then(|| "not an agent (name it to relaunch it)".to_string());
    };
    if !c.cwd_exists {
        return Some(match &c.cwd {
            Some(cwd) => format!("working directory {cwd} no longer exists"),
            None => "no recorded working directory".to_string(),
        });
    }
    // The agent running `pmux respawn` is busy by definition; it asked.
    if !c.caller && !force && matches!(c.state.as_deref(), Some("working" | "waiting")) {
        return Some(format!(
            "agent is {} (--force interrupts it)",
            c.state.as_deref().unwrap_or_default()
        ));
    }
    if force {
        return None;
    }
    match &c.session {
        None => Some("no resume id found (--force starts it fresh)".to_string()),
        Some((id, _)) if resume_argv(agent, id).is_none() => Some(format!(
            "pmux cannot resume {agent} sessions (--force starts it fresh)"
        )),
        Some(_) => None,
    }
}

/// Render the plan as an aligned table for `--dry-run` and confirmations.
pub fn render_respawn_plan(plan: &[RespawnPlanItem]) -> String {
    let rows: Vec<[String; 5]> = plan
        .iter()
        .map(|item| {
            let name = if item.caller {
                format!("{} (this pane)", item.name)
            } else {
                item.name.clone()
            };
            let id = match (&item.session_id, item.id_source) {
                (Some(id), Some(source)) => format!("{id} ({source})"),
                (Some(id), None) => id.clone(),
                _ => "-".to_string(),
            };
            [
                name,
                item.pane_id.clone(),
                item.agent.clone().unwrap_or_else(|| "-".to_string()),
                id,
                item.action(),
            ]
        })
        .collect();
    let header = ["NAME", "PANE", "AGENT", "RESUME ID", "ACTION"].map(str::to_string);
    let mut widths = header.clone().map(|h| h.len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .enumerate()
            .map(|(i, (cell, width))| {
                if i + 1 == row.len() {
                    cell.clone()
                } else {
                    format!("{cell:<width$}")
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// Select processes and build the respawn plan without touching any pane.
pub fn respawn_plan(opts: &RespawnOptions) -> Result<Vec<RespawnPlanItem>> {
    if opts.resume_id.is_some() && (opts.names.len() != 1 || opts.agent.is_some() || opts.all) {
        bail!("--resume-id applies to exactly one named process");
    }
    if !opts.extra_args.is_empty() && (opts.names.len() != 1 || opts.agent.is_some() || opts.all) {
        bail!("arguments after -- apply to exactly one named process");
    }
    let sessions = list_sessions()?;
    let selected = select_sessions(&sessions, opts)?;
    if selected.is_empty() {
        bail!("no pmux processes match");
    }

    let states: HashMap<String, Option<String>> = build_state_snapshot(&sessions)
        .sessions
        .into_iter()
        .map(|p| (p.name, p.agent.and_then(|agent| agent.state)))
        .collect();
    let recorded: HashMap<String, crate::restore::SessionSnapshot> = preview_snapshot(&sessions)
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    let sources = Sources::from_env();
    let caller_pane = caller_pane();

    let candidates = selected
        .into_iter()
        .map(|(session, explicit)| {
            let agent = session_agent(session).map(str::to_string);
            let saved = recorded.get(&session.name);
            let session_id = match (&opts.resume_id, agent.as_deref()) {
                (Some(id), _) => Some((id.clone(), "--resume-id")),
                (None, Some(agent)) => live_session_id(&session.pane_id, agent, &sources)
                    .map(|id| (id, "live process"))
                    .or_else(|| {
                        saved
                            .and_then(|s| s.agent_session_id.clone())
                            .map(|id| (id, "pmux records"))
                    }),
                (None, None) => None,
            };
            let cwd = saved.map(|s| s.cwd.clone()).or_else(|| {
                session
                    .current_path
                    .clone()
                    .or_else(|| session.launch_cwd.clone())
            });
            RespawnCandidate {
                name: session.name.clone(),
                pane_id: session.pane_id.clone(),
                created_epoch: session.created_epoch,
                state: states.get(&session.name).cloned().flatten(),
                session: session_id,
                command: saved.map(|s| s.command.clone()).or_else(|| {
                    session
                        .launch_command
                        .clone()
                        .or_else(|| session.start_command.clone())
                }),
                cwd_exists: cwd.as_deref().is_some_and(|cwd| Path::new(cwd).is_dir()),
                cwd,
                explicit,
                caller: caller_pane.as_deref() == Some(session.pane_id.as_str()),
                agent,
            }
        })
        .collect::<Vec<_>>();
    let mut plan = plan_respawn(&candidates, opts.force);
    if !opts.extra_args.is_empty() {
        let extra = shell_join(&opts.extra_args);
        for item in &mut plan {
            if let Some(command) = item.shell_cmd.as_mut() {
                command.push(' ');
                command.push_str(&extra);
                item.extra_args = true;
            }
        }
    }
    Ok(plan)
}

/// The selected sessions, each with whether it was named explicitly.
fn select_sessions<'a>(
    sessions: &'a [SessionInfo],
    opts: &RespawnOptions,
) -> Result<Vec<(&'a SessionInfo, bool)>> {
    if opts.all {
        return Ok(sessions.iter().map(|s| (s, false)).collect());
    }
    if let Some(agent) = &opts.agent {
        return Ok(sessions
            .iter()
            .filter(|s| session_agent(s) == Some(agent.as_str()))
            .map(|s| (s, false))
            .collect());
    }
    let mut picked: Vec<(&SessionInfo, bool)> = Vec::new();
    for name in &opts.names {
        let session = super::resolve_attach_target(sessions, name)?;
        if !picked.iter().any(|(s, _)| s.name == session.name) {
            picked.push((session, true));
        }
    }
    Ok(picked)
}

/// The session the agent in `pane` is running right now, read from the
/// agent's own live registry for the pane's process tree.
/// The conversation id an agent process runs: found from its live process
/// when possible, else from what pmux recorded for this session.
pub(super) fn agent_session_id(
    name: &str,
    pane: &str,
    created_epoch: i64,
    agent: &str,
) -> Option<String> {
    live_session_id(pane, agent, &Sources::from_env()).or_else(|| {
        crate::state::read_live_session_agent_identity(name, created_epoch)
            .ok()
            .flatten()
            .filter(|identity| identity.agent == agent)
            .map(|identity| identity.session_id)
    })
}

pub(super) fn live_session_id(pane: &str, agent: &str, sources: &Sources) -> Option<String> {
    let pid = tmux_cmd()
        .args(["display-message", "-p", "-t", pane, "#{pane_pid}"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<i64>()
                .ok()
        })?;
    process_tree(pid, sources)
        .into_iter()
        .find_map(|pid| live_session_for_pid(agent, pid, sources))
}

/// The pmux pane this command runs in, if it runs inside pmux's server.
fn caller_pane() -> Option<String> {
    let pane = env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())?;
    let socket = env::var("TMUX").ok()?.split(',').next()?.to_string();
    let output = tmux_cmd()
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .ok()?;
    let server = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let same = |a: &str, b: &str| {
        std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
    };
    (output.status.success() && same(&socket, &server)).then_some(pane)
}

/// Run `pmux respawn`: print the plan, confirm when needed, then restart each
/// selected agent in place. Returns how many panes were respawned.
pub fn respawn(opts: &RespawnOptions) -> Result<usize> {
    let plan = respawn_plan(opts)?;
    let actionable: Vec<&RespawnPlanItem> = plan.iter().filter(|i| i.skip.is_none()).collect();
    if opts.dry_run || actionable.is_empty() {
        print!("{}", render_respawn_plan(&plan));
        if actionable.is_empty() && !opts.dry_run {
            println!("Nothing to respawn.");
        }
        return Ok(0);
    }
    if actionable.len() > 1 && !opts.yes {
        print!("{}", render_respawn_plan(&plan));
        if !io::stdin().is_terminal() {
            bail!(
                "refusing to respawn {} processes without confirmation; rerun with --yes",
                actionable.len()
            );
        }
        eprint!("Respawn {} processes? [y/N] ", actionable.len());
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(0);
        }
    }

    // Respawned panes start from the server's environment, not the caller's,
    // so give that environment the tmux guard first.
    crate::tmux::guard_server_path_best_effort();
    let mut respawned = 0;
    // Skips are reported first: respawning the calling pane ends this command.
    for item in plan.iter().filter(|i| i.skip.is_some()) {
        println!(
            "Skipped {}: {}",
            item.name,
            item.skip.as_deref().unwrap_or_default()
        );
    }
    for item in plan.iter().filter(|i| i.skip.is_none()) {
        let auto_was_on = pane_option(&item.pane_id, "@ai_auto").as_deref() == Some("on");
        let how = describe(item);
        if item.caller {
            // This pane's process (and this command with it) ends next.
            println!("Respawning {} (this pane): {how}", item.name);
            if auto_was_on {
                println!("  auto mode was on; turn it back on once the agent is up");
            }
            io::stdout().flush()?;
        }
        respawn_pane(item)?;
        respawned += 1;
        if !item.caller {
            println!("Respawned {}: {how}", item.name);
            if auto_was_on {
                println!(
                    "  auto mode was on; ai-auto drops a pane whose process changes, so turn it back on"
                );
            }
        }
    }
    Ok(respawned)
}

fn describe(item: &RespawnPlanItem) -> String {
    match (&item.agent, &item.session_id, item.resumed) {
        (Some(_), Some(_), true) if item.extra_args => format!(
            "resumed with `{}`",
            item.shell_cmd.as_deref().unwrap_or_default()
        ),
        (Some(agent), Some(id), true) => format!("resumed {agent} session {id}"),
        (Some(agent), _, false) => format!("started a fresh {agent} session"),
        _ => format!(
            "relaunched `{}`",
            item.shell_cmd.as_deref().unwrap_or_default()
        ),
    }
}

/// Kill the pane's process and start `item.shell_cmd` in the same pane.
fn respawn_pane(item: &RespawnPlanItem) -> Result<()> {
    let shell_cmd = item.shell_cmd.as_deref().context("nothing to run")?;
    let cwd = item.cwd.as_deref().context("no working directory")?;
    let previous_start = crate::startup::read_start(&item.name, item.created_epoch);
    crate::startup::write_start(
        &item.name,
        &crate::startup::AgentStart::new(item.created_epoch, crate::AiAttnSnapshot::load()),
    )?;
    let output = tmux_cmd()
        .args(["respawn-pane", "-k", "-t", &item.pane_id, "-c"])
        .arg(tmux_format_escape_path(Path::new(cwd)))
        .arg(shell_cmd)
        .output()
        .context("failed to execute tmux respawn-pane");
    match output {
        Ok(output) if output.status.success() => (),
        other => {
            crate::startup::restore_start(&item.name, previous_start)?;
            let output = other?;
            return Err(tmux_failure("failed to respawn pmux process", &output));
        }
    };
    if let (true, Some(agent), Some(id)) = (item.resumed, &item.agent, &item.session_id) {
        // The session itself was not recreated, so its epoch still applies.
        let _ = write_session_agent_identity(&item.name, agent, id, item.created_epoch);
    }
    crate::restore::snapshot_best_effort();
    Ok(())
}

fn pane_option(pane: &str, option: &str) -> Option<String> {
    let output = tmux_cmd()
        .args([
            "display-message",
            "-p",
            "-t",
            pane,
            &format!("#{{{option}}}"),
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str) -> RespawnCandidate {
        RespawnCandidate {
            name: name.to_string(),
            pane_id: format!("%{}", name.len()),
            created_epoch: 100,
            agent: Some("claude".to_string()),
            state: Some("done".to_string()),
            session: Some(("sess-1".to_string(), "pmux records")),
            command: Some("claude".to_string()),
            cwd: Some("/work".to_string()),
            cwd_exists: true,
            explicit: false,
            caller: false,
        }
    }

    fn plan_one(c: RespawnCandidate, force: bool) -> RespawnPlanItem {
        plan_respawn(&[c], force).remove(0)
    }

    #[test]
    fn resumes_an_idle_agent_with_a_known_session() {
        let item = plan_one(candidate("a"), false);
        assert_eq!(item.skip, None);
        assert!(item.resumed);
        assert_eq!(item.shell_cmd.as_deref(), Some("claude --resume sess-1"));
    }

    #[test]
    fn busy_agents_are_skipped_unless_forced() {
        for state in ["working", "waiting"] {
            let mut c = candidate("a");
            c.state = Some(state.to_string());
            let item = plan_one(c.clone(), false);
            assert_eq!(
                item.skip.as_deref(),
                Some(format!("agent is {state} (--force interrupts it)").as_str())
            );
            assert!(plan_one(c, true).skip.is_none());
        }
    }

    #[test]
    fn the_calling_pane_is_never_skipped_for_being_busy_and_goes_last() {
        let mut caller = candidate("me");
        caller.caller = true;
        caller.state = Some("working".to_string());
        let plan = plan_respawn(&[caller, candidate("other")], false);
        assert_eq!(plan[0].name, "other");
        assert_eq!(plan[1].name, "me");
        assert!(plan[1].caller && plan[1].skip.is_none());
    }

    #[test]
    fn a_missing_resume_id_skips_unless_forced_then_starts_fresh() {
        let mut c = candidate("a");
        c.session = None;
        assert_eq!(
            plan_one(c.clone(), false).skip.as_deref(),
            Some("no resume id found (--force starts it fresh)")
        );
        let forced = plan_one(c, true);
        assert!(forced.skip.is_none() && !forced.resumed);
        assert_eq!(forced.shell_cmd.as_deref(), Some("claude"));
    }

    #[test]
    fn agents_pmux_cannot_resume_are_skipped_unless_forced() {
        let mut c = candidate("a");
        c.agent = Some("gemini".to_string());
        c.command = Some("gemini".to_string());
        assert_eq!(
            plan_one(c.clone(), false).skip.as_deref(),
            Some("pmux cannot resume gemini sessions (--force starts it fresh)")
        );
        assert_eq!(plan_one(c, true).shell_cmd.as_deref(), Some("gemini"));
    }

    #[test]
    fn non_agents_relaunch_only_when_named() {
        let mut c = candidate("a");
        c.agent = None;
        c.session = None;
        c.command = Some("npm run dev".to_string());
        assert_eq!(
            plan_one(c.clone(), false).skip.as_deref(),
            Some("not an agent (name it to relaunch it)")
        );
        c.explicit = true;
        let item = plan_one(c, false);
        assert_eq!(item.shell_cmd.as_deref(), Some("npm run dev"));
        assert_eq!(item.action(), "relaunch");
    }

    #[test]
    fn a_vanished_working_directory_always_skips() {
        let mut c = candidate("a");
        c.cwd_exists = false;
        assert_eq!(
            plan_one(c, true).skip.as_deref(),
            Some("working directory /work no longer exists")
        );
    }

    #[test]
    fn resume_commands_keep_launch_rewrites() {
        assert_eq!(
            resume_shell_command("claude", "abc-123").as_deref(),
            Some("claude --resume abc-123")
        );
        assert_eq!(
            resume_shell_command("codex", "01a1-thread").as_deref(),
            Some("codex --no-daemon resume 01a1-thread")
        );
        assert_eq!(resume_shell_command("claude", "bad id; rm -rf /"), None);
    }

    #[test]
    fn the_plan_table_names_the_source_of_each_resume_id() {
        let mut busy = candidate("busy");
        busy.state = Some("working".to_string());
        let table = render_respawn_plan(&plan_respawn(&[candidate("idle"), busy], false));
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("NAME"));
        assert!(lines[1].contains("sess-1 (pmux records)") && lines[1].ends_with("resume"));
        assert!(lines[2].ends_with("skip: agent is working (--force interrupts it)"));
    }
    #[test]
    fn cortex_respawn_resumes_native_conversation() {
        let mut c = candidate("cortex-worker");
        c.agent = Some("cortex".into());
        c.command = Some("cortex".into());
        let item = plan_one(c, false);
        assert_eq!(item.skip, None);
        assert!(item.resumed);
        assert_eq!(item.shell_cmd.as_deref(), Some("cortex resume sess-1"));
    }
}
