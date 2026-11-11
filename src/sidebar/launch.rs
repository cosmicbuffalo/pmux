//! Launching `pmux attach`/`pmux resume` into new outer-tmux splits and
//! windows from the sidebar.

use super::pane::ensure_sidebar;
use super::*;

fn parse_split_target_panes(raw: &str) -> Result<Vec<SplitTargetPane>> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.splitn(5, '\t');
            let id = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("outer tmux returned a pane without an id"))?;
            let left = fields
                .next()
                .ok_or_else(|| anyhow!("outer tmux omitted a pane's left coordinate"))?
                .parse::<u16>()
                .context("outer tmux returned an invalid pane left coordinate")?;
            let width = fields
                .next()
                .ok_or_else(|| anyhow!("outer tmux omitted a pane's width"))?
                .parse::<u16>()
                .context("outer tmux returned an invalid pane width")?;
            let top = fields
                .next()
                .ok_or_else(|| anyhow!("outer tmux omitted a pane's top coordinate"))?
                .parse::<u16>()
                .context("outer tmux returned an invalid pane top coordinate")?;
            let sidebar = fields.next().is_some_and(|value| value.trim() == "1");
            Ok(SplitTargetPane {
                id: id.to_string(),
                left,
                width,
                top,
                sidebar,
            })
        })
        .collect()
}

fn select_rightmost_work_pane<'a>(
    panes: &'a [SplitTargetPane],
    preferred_pane: &str,
) -> Option<&'a str> {
    panes
        .iter()
        .filter(|pane| !pane.sidebar)
        .max_by_key(|pane| {
            (
                pane.left.saturating_add(pane.width),
                pane.left,
                pane.id == preferred_pane,
                Reverse(pane.top),
            )
        })
        .map(|pane| pane.id.as_str())
}

fn rightmost_work_pane(context: &SidebarContext) -> Result<String> {
    let raw = outer_tmux_output(
        &context.socket,
        &[
            "list-panes",
            "-t",
            &context.host_window_id,
            "-F",
            "#{pane_id}\t#{pane_left}\t#{pane_width}\t#{pane_top}\t#{@pmux_sidebar}",
        ],
        "find the rightmost outer tmux work pane",
    )?;
    let panes = parse_split_target_panes(&raw)?;
    select_rightmost_work_pane(&panes, &context.host_pane)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the current outer tmux window has no non-sidebar pane to split"))
}

/// Split the rightmost ordinary pane in the current outer-tmux window in half
/// and attach the new right-hand pane to a pmux process.
///
/// Targeting one existing pane, rather than asking tmux for a full-height split,
/// keeps every other branch of the user's layout unchanged. The sidebar itself
/// is never eligible as the split target.
pub fn attach_in_new_outer_split(name: &str, context: &SidebarContext) -> Result<()> {
    open_in_new_outer_split(
        &["attach", name],
        "#{pane_current_path}",
        context,
        Launch::Direct,
    )
}

/// Open a brand-new outer-tmux window with a single pane, attach a pmux process
/// to it, and bring the pmux sidebar along into that new window.
///
/// Unlike the split action this is offered for every process, attached or not,
/// so any process can be surfaced in a fresh, uncluttered window on demand.
pub fn attach_in_new_outer_window(name: &str, context: &SidebarContext) -> Result<()> {
    open_in_new_outer_window(
        &["attach", name],
        "#{pane_current_path}",
        context,
        Launch::Direct,
    )
}

/// Split beside the rightmost pane and resume a past agent session there with
/// `pmux resume <target>`. The pane starts in the session's own directory, so
/// the shell left behind when the agent exits is in its project.
pub fn resume_in_new_outer_split(target: &str, cwd: &str, context: &SidebarContext) -> Result<()> {
    let start_dir = crate::tmux::tmux_format_escape(cwd);
    open_in_new_outer_split(
        &["resume", "--no-parent", target],
        &start_dir,
        context,
        Launch::UserShell,
    )
}

/// Resume a past agent session in a new outer-tmux window, bringing the
/// sidebar along as [`attach_in_new_outer_window`] does.
pub fn resume_in_new_outer_window(target: &str, cwd: &str, context: &SidebarContext) -> Result<()> {
    let start_dir = crate::tmux::tmux_format_escape(cwd);
    open_in_new_outer_window(
        &["resume", "--no-parent", target],
        &start_dir,
        context,
        Launch::UserShell,
    )
}

/// How a sidebar-opened pane runs its `pmux` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// Run it directly; enough for commands that only attach a client.
    Direct,
    /// Run it through the user's interactive shell. A new outer pane only has
    /// the outer tmux server's environment, usually a bare system PATH, while
    /// agents and their tools are often found through PATH entries added by
    /// the user's shell rc (e.g. `~/.local/bin`). A command that starts an
    /// agent must see what typing it in a shell would, both to find the agent
    /// and to hand it a working environment.
    UserShell,
}

/// The shell command line for `pmux <args>` launched as `launch` describes.
fn pmux_pane_command(args: &[&str], launch: Launch) -> Result<String> {
    let binary = crate::util::resolve_pmux_binary()?;
    let mut parts = pmux_command_parts(&binary);
    parts.extend(args.iter().map(|arg| arg.to_string()));
    let command = shell_join(&parts);
    Ok(match (launch, user_shell()) {
        (Launch::UserShell, Some(shell)) => wrap_in_interactive_shell(&shell, &command),
        _ => command,
    })
}

/// The user's shell from `$SHELL`, when it names an executable absolute path.
/// Anything else falls back to running the command directly.
fn user_shell() -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let shell = env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/'))?;
    let metadata = std::fs::metadata(&shell).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(shell)
}

fn wrap_in_interactive_shell(shell: &str, command: &str) -> String {
    // `exec` replaces the shell once its rc files have run, so the pane's
    // process is pmux itself and exits with it.
    shell_join(&[
        shell.to_string(),
        "-ic".to_string(),
        format!("exec {command}"),
    ])
}

/// Run `pmux <args>` in a new pane split from the rightmost work pane.
///
/// `start_dir` is a tmux format (tmux expands `-c`): pass a format such as
/// `#{pane_current_path}`, or a literal path escaped with
/// [`crate::tmux::tmux_format_escape`].
fn open_in_new_outer_split(
    args: &[&str],
    start_dir: &str,
    context: &SidebarContext,
    launch: Launch,
) -> Result<()> {
    let command = pmux_pane_command(args, launch)?;
    let target_pane = rightmost_work_pane(context)?;
    outer_tmux_output(
        &context.socket,
        &[
            "split-window",
            "-h",
            // `-l 50%`, not the deprecated `-p 50`, which tmux 3.4 rejects
            // ("size missing").
            "-l",
            "50%",
            "-t",
            &target_pane,
            "-c",
            start_dir,
            &command,
        ],
        "split the rightmost pane for a pmux process",
    )?;
    Ok(())
}

/// Run `pmux <args>` as the only pane of a new outer window, then bring the
/// sidebar into that window. `start_dir` is a tmux format, as for
/// [`open_in_new_outer_split`].
fn open_in_new_outer_window(
    args: &[&str],
    start_dir: &str,
    context: &SidebarContext,
    launch: Launch,
) -> Result<()> {
    let command = pmux_pane_command(args, launch)?;
    let new_pane = outer_tmux_output(
        &context.socket,
        &[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            start_dir,
            &command,
        ],
        "open pmux process in a new outer window",
    )?;
    if new_pane.trim().is_empty() {
        bail!("outer tmux did not return the new window's pane id");
    }
    // The new window starts with only its pmux pane. Bring the sidebar in
    // alongside it; `ensure_sidebar` opens it detached, so the pmux pane keeps
    // focus in the new window.
    ensure_sidebar(&new_pane, &context.socket)?;
    Ok(())
}

/// Open every named process as panes in one new outer-tmux window.
pub fn attach_group_in_new_outer_window(names: &[String], context: &SidebarContext) -> Result<()> {
    let Some(first) = names.first() else {
        bail!("the group has no running processes");
    };
    let binary = crate::util::resolve_pmux_binary()?;
    let attach_command = |name: &str| {
        let mut parts = pmux_command_parts(&binary);
        parts.extend(["attach".to_string(), name.to_string()]);
        shell_join(&parts)
    };
    let created = outer_tmux_output(
        &context.socket,
        &[
            "new-window",
            "-P",
            "-F",
            "#{window_id}\t#{pane_id}",
            "-c",
            "#{pane_current_path}",
            &attach_command(first),
        ],
        "open a new outer window for a pmux group",
    )?;
    let mut fields = created.split('\t');
    let window_id = fields
        .next()
        .filter(|value| !value.is_empty())
        .context("outer tmux did not return the new group window id")?;
    let first_pane = fields
        .next()
        .filter(|value| !value.is_empty())
        .context("outer tmux did not return the new group pane id")?;
    for name in &names[1..] {
        outer_tmux_output(
            &context.socket,
            &[
                "split-window",
                "-h",
                "-t",
                window_id,
                "-c",
                "#{pane_current_path}",
                &attach_command(name),
            ],
            "attach a pmux group member in the new outer window",
        )?;
        outer_tmux_output(
            &context.socket,
            &["select-layout", "-t", window_id, "tiled"],
            "tile the new pmux group window",
        )?;
    }
    ensure_sidebar(first_pane, &context.socket)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_target_is_the_rightmost_non_sidebar_pane() {
        let panes = parse_split_target_panes(
            "%1\t0\t42\t0\t1\n%2\t43\t30\t0\t\n%3\t74\t46\t0\t\n%4\t74\t46\t15\t\n%5\t121\t20\t0\t1\n",
        )
        .unwrap();

        // The preferred pane wins when several panes share the rightmost
        // column, which keeps the user's last work pane as the intuitive target.
        assert_eq!(select_rightmost_work_pane(&panes, "%4"), Some("%4"));
        // If the previous pane was elsewhere, choose the uppermost pane in the
        // rightmost column. A farther-right sidebar is never eligible.
        assert_eq!(select_rightmost_work_pane(&panes, "%2"), Some("%3"));
    }

    #[test]
    fn agent_launches_go_through_the_users_interactive_shell() {
        let wrapped = super::wrap_in_interactive_shell("/bin/zsh", "/opt/pmux resume claude:abc");
        assert_eq!(wrapped, "/bin/zsh -ic 'exec /opt/pmux resume claude:abc'");
        // A plain attach stays a direct command.
        let direct = super::pmux_pane_command(&["attach", "demo"], super::Launch::Direct).unwrap();
        assert!(!direct.contains(" -ic "), "{direct}");
        assert!(direct.ends_with("attach demo"), "{direct}");
    }
}
