//! Native tmux context menus for sidebar processes and groups.

use super::*;
use serde::{Deserialize, Serialize};

/// A native tmux menu's items, each `(label, shortcut key, command)`.
type NativeMenuItem = (String, String, String);

/// Unused menu requests are pruned after this long; a keyboard-driven menu
/// can legitimately stay open for a while before an item is chosen.
const MENU_REQUEST_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MENU_TOKEN_LEN: usize = 16;

/// Escape `text` so that `levels` successive tmux format expansions turn it
/// back into the literal text.
///
/// tmux expands `#{…}`, `#(…)` (which runs a shell command), `#[…]` and the
/// short `#X` aliases in menu labels, menu commands, `confirm-before` prompts
/// and `run-shell` commands, and each expansion turns `##` into `#`. Doubling
/// every `#` once per expansion therefore keeps untrusted text inert. Verified
/// against tmux 3.5a on a throwaway server:
///
/// - a `display-menu` item label is expanded once;
/// - a `confirm-before -p` prompt inside a menu command is expanded twice
///   (the menu, then the prompt);
/// - a `run-shell` command inside a menu command is expanded twice (the menu,
///   then `run-shell`), and three times inside `confirm-before`, which expands
///   its command too; `command-prompt` does not expand its template.
pub(super) fn tmux_format_escape_levels(text: &str, levels: u32) -> String {
    if levels == 0 {
        return text.to_string();
    }
    text.replace('#', &"#".repeat(1 << levels))
}

/// One action a menu item can run, kept out of the tmux command string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MenuItemSpec {
    target: String,
    action: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    /// Take the value from the text typed into the item's `command-prompt`.
    #[serde(default)]
    prompted: bool,
}

/// Everything a native menu's items need to run, saved to a short-lived state
/// file named by a random token.
///
/// tmux format-expands menu labels and commands, so splicing process names,
/// group labels, cwd paths, or typed text into them would let `#(…)` in that
/// data run shell commands in the outer tmux server (and quoting mistakes
/// would let it reach a shell). Menu commands therefore carry only the pmux
/// binary path, this pmux-generated token, and an item index; `pmux
/// sidebar-menu-item` reads the real arguments back from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MenuRequest {
    socket: String,
    host_pane: String,
    sidebar_pane: String,
    items: Vec<MenuItemSpec>,
}

/// Builds the tmux commands for one menu and records what each one runs.
pub(super) struct MenuCommands {
    binary: PathBuf,
    token: String,
    request: MenuRequest,
}

impl MenuCommands {
    pub(super) fn new(
        binary: PathBuf,
        context: &crate::SidebarContext,
        sidebar_pane: &str,
    ) -> Self {
        use rand::RngExt;
        let token = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(MENU_TOKEN_LEN)
            .map(|byte| char::from(byte).to_ascii_lowercase())
            .collect();
        Self {
            binary,
            token,
            request: MenuRequest {
                socket: context.socket.clone(),
                host_pane: context.host_pane.clone(),
                sidebar_pane: sidebar_pane.to_string(),
                items: Vec::new(),
            },
        }
    }

    fn add(
        &mut self,
        target: &str,
        action: &str,
        value: Option<&str>,
        kind: Option<&str>,
        prompted: bool,
    ) -> usize {
        self.request.items.push(MenuItemSpec {
            target: target.to_string(),
            action: action.to_string(),
            value: value.map(str::to_string),
            kind: kind.map(str::to_string),
            prompted,
        });
        self.request.items.len() - 1
    }

    /// A `run-shell -b` command for item `index`, escaped for `levels`
    /// format expansions in total. Only the binary path can contain `#`.
    fn run_shell(&self, index: usize, levels: u32) -> Result<String> {
        let shell = shell_join(&[
            self.binary.display().to_string(),
            "sidebar-menu-item".to_string(),
            self.token.clone(),
            index.to_string(),
        ])?;
        shell_join(&[
            "run-shell".to_string(),
            "-b".to_string(),
            tmux_format_escape_levels(&shell, levels),
        ])
    }

    /// A menu command that runs `action` immediately.
    pub(super) fn run(
        &mut self,
        target: &str,
        action: &str,
        value: Option<&str>,
        kind: Option<&str>,
    ) -> Result<String> {
        let index = self.add(target, action, value, kind, false);
        // Expanded by the menu, then by `run-shell`.
        self.run_shell(index, 2)
    }

    /// A menu command that asks `prompt` (untrusted text is fine) before
    /// running `action`.
    pub(super) fn confirm(
        &mut self,
        prompt: &str,
        target: &str,
        action: &str,
        value: Option<&str>,
        kind: Option<&str>,
    ) -> Result<String> {
        let index = self.add(target, action, value, kind, false);
        shell_join(&[
            "confirm-before".to_string(),
            "-p".to_string(),
            tmux_format_escape_levels(prompt, 2),
            self.run_shell(index, 3)?,
        ])
    }

    /// A menu command that prompts for text and runs `action` with it as the
    /// value.
    ///
    /// The typed text never reaches a shell or a format: `%%%` substitutes it
    /// with `"`, `\`, `$`, `;` and `~` escaped for tmux's own double-quoted
    /// string syntax, `set-option` stores it in a token-named user option
    /// without expanding it, and `pmux sidebar-menu-item` reads it back.
    pub(super) fn prompt(
        &mut self,
        prompt: &str,
        target: &str,
        action: &str,
        kind: Option<&str>,
    ) -> Result<String> {
        let index = self.add(target, action, None, kind, true);
        let template = format!(
            "set-option -g {} \"%%%\" ; {}",
            menu_input_option(&self.token),
            // Expanded by the menu, then by `run-shell`; `command-prompt`
            // does not expand its template.
            self.run_shell(index, 2)?
        );
        shell_join(&[
            "command-prompt".to_string(),
            "-p".to_string(),
            tmux_format_escape_levels(prompt, 2),
            template,
        ])
    }

    /// Save the request so the menu's commands can find it.
    pub(super) fn save(&self) -> Result<()> {
        self.save_in(&crate::state::sidebar_menu_dir()?)
    }

    fn save_in(&self, dir: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        crate::util::create_private_dir_all(dir)?;
        prune_menu_requests(dir);
        let path = dir.join(format!("{}.json", self.token));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(serde_json::to_string(&self.request)?.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

fn menu_input_option(token: &str) -> String {
    format!("@pmux_menu_input_{token}")
}

fn valid_menu_token(token: &str) -> bool {
    token.len() == MENU_TOKEN_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// Remove menu requests old enough that their menu has surely closed.
fn prune_menu_requests(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MENU_REQUEST_TTL);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Load and consume menu request `token`, returning it with item `index`.
fn take_menu_item(dir: &Path, token: &str, index: usize) -> Result<(MenuRequest, MenuItemSpec)> {
    if !valid_menu_token(token) {
        bail!("invalid pmux sidebar menu token");
    }
    let path = dir.join(format!("{token}.json"));
    let text = fs::read_to_string(&path)
        .with_context(|| format!("the sidebar menu that ran this item has expired ({token})"))?;
    // A menu runs at most one item, so the request is spent either way.
    let _ = fs::remove_file(&path);
    let request: MenuRequest =
        serde_json::from_str(&text).context("invalid pmux sidebar menu request")?;
    let item = request
        .items
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow!("pmux sidebar menu item {index} does not exist"))?;
    Ok((request, item))
}

/// Run the item a native sidebar menu's command names.
pub fn run_sidebar_menu_item(token: &str, index: usize) -> Result<()> {
    let (request, item) = take_menu_item(&crate::state::sidebar_menu_dir()?, token, index)?;
    let value = if item.prompted {
        Some(take_menu_input(&request.socket, token)?)
    } else {
        item.value
    };
    run_sidebar_action(
        &request.socket,
        &request.host_pane,
        &request.sidebar_pane,
        &item.target,
        &item.action,
        value.as_deref(),
        item.kind.as_deref(),
    )
}

/// Read and clear the text a menu item's `command-prompt` stored.
fn take_menu_input(socket: &str, token: &str) -> Result<String> {
    let option = menu_input_option(token);
    let output = crate::tmux::tmux_command()
        .args(["-S", socket, "show-option", "-gqv", &option])
        .output()
        .context("read the text typed into the sidebar menu prompt")?;
    let _ = crate::tmux::tmux_command()
        .args(["-S", socket, "set-option", "-gu", &option])
        .status();
    if !output.status.success() {
        bail!("outer tmux could not return the text typed into the menu prompt");
    }
    let mut value =
        String::from_utf8(output.stdout).context("menu prompt text is not valid UTF-8")?;
    if value.ends_with('\n') {
        value.pop();
    }
    Ok(crate::tmux::undo_tmux_3_4_dollar_escape(
        &value,
        outer_tmux_version(socket),
    ))
}

/// Open the sidebar context menu using tmux's native menu overlay.
///
/// Unlike `display-popup`, tmux menus can receive pointer events outside their
/// bounds and dismiss on an outside click. That gives this compact context
/// menu conventional click-away behavior without blocking the sidebar beneath
/// it.
pub(super) fn open_session_actions_menu(
    process: &crate::SidebarProcess,
    row: u16,
    context: &crate::SidebarContext,
    trigger: PopupTrigger,
) -> Result<()> {
    let binary = crate::resolve_pmux_binary()?;
    let sidebar_pane = std::env::var("TMUX_PANE").unwrap_or_else(|_| context.host_pane.clone());
    let grouping = crate::read_sidebar_grouping().unwrap_or_default();
    let mut commands = MenuCommands::new(binary, context, &sidebar_pane);
    let items = session_menu_items(&mut commands, process, context, &grouping.manual)?;
    commands.save()?;
    show_native_menu(
        context,
        &sidebar_pane,
        row,
        trigger,
        items,
        "process actions",
    )
}

fn session_menu_items(
    commands: &mut MenuCommands,
    process: &crate::SidebarProcess,
    context: &crate::SidebarContext,
    manual_groups: &[crate::groups::ManualGroup],
) -> Result<Vec<NativeMenuItem>> {
    let name = &process.process.name;
    let mut items = Vec::new();
    // These labels hold only fixed text and tmux window/pane indexes.
    for (action, label) in native_action_menu_items(process, context) {
        items.push((
            label,
            native_menu_shortcut(action, context.sidebar_on_right).to_string(),
            commands.run(name, native_menu_action_name(action), None, None)?,
        ));
    }
    // A blank native-menu triple is a visual separator between process actions
    // and manual grouping actions.
    items.push((String::new(), String::new(), String::new()));
    for group in manual_groups {
        let member = group.processes.iter().any(|member| member == name);
        let label = format!("{}Add to {}", if member { "✓ " } else { "" }, group.name);
        items.push((
            tmux_format_escape_levels(&label, 1),
            String::new(),
            commands.run(name, "group-add", Some(&group.id), None)?,
        ));
    }
    items.push((
        "New manual group…".to_string(),
        String::new(),
        commands.prompt("New manual group:", name, "group-create", None)?,
    ));
    Ok(items)
}

pub(super) fn open_group_actions_menu(
    group: &SidebarGroupRow,
    row: u16,
    context: &crate::SidebarContext,
    trigger: PopupTrigger,
) -> Result<()> {
    if group.kind == SidebarGroupKind::Resumable {
        // Folding (Enter or a click) is the only group-level action.
        return Ok(());
    }
    let binary = crate::resolve_pmux_binary()?;
    let sidebar_pane = std::env::var("TMUX_PANE").unwrap_or_else(|_| context.host_pane.clone());
    let mut commands = MenuCommands::new(binary, context, &sidebar_pane);
    let mut items = group_menu_items(
        &mut commands,
        group.kind,
        &group.identity,
        &group.key,
        &group.label,
        &group.members,
    )?;
    items.push(cancel_item(context));
    commands.save()?;
    show_native_menu(context, &sidebar_pane, row, trigger, items, "group actions")
}

fn group_menu_items(
    commands: &mut MenuCommands,
    kind: SidebarGroupKind,
    identity: &str,
    key: &str,
    label: &str,
    members: &[String],
) -> Result<Vec<NativeMenuItem>> {
    let kind_name = match kind {
        SidebarGroupKind::Automatic => {
            format!("auto:{}", identity.split(':').nth(1).unwrap_or("ungrouped"))
        }
        SidebarGroupKind::Manual => "manual".to_string(),
        SidebarGroupKind::Resumable => bail!("resumable groups have no actions menu"),
    };
    let kind_name = Some(kind_name.as_str());
    let members = serde_json::to_string(members)?;
    let mut items = vec![
        (
            "Move group up".to_string(),
            String::new(),
            commands.run(key, "group-up", None, kind_name)?,
        ),
        (
            "Move group down".to_string(),
            String::new(),
            commands.run(key, "group-down", None, kind_name)?,
        ),
        (
            "Attach all in new window".to_string(),
            String::new(),
            commands.run(key, "group-attach-window", Some(&members), kind_name)?,
        ),
        (
            "Kill all processes…".to_string(),
            String::new(),
            commands.confirm(
                &format!("Kill all processes in {label}?"),
                key,
                "group-kill",
                Some(&members),
                kind_name,
            )?,
        ),
    ];
    if kind == SidebarGroupKind::Manual {
        items.push((
            "Dissolve group…".to_string(),
            String::new(),
            commands.confirm(
                &format!("Dissolve {label}?"),
                key,
                "group-dissolve",
                None,
                kind_name,
            )?,
        ));
    }
    Ok(items)
}

/// Menu for one past agent session in the Resumable group.
pub(super) fn open_resumable_actions_menu(
    entry: &crate::HistoryEntry,
    row: u16,
    context: &crate::SidebarContext,
    trigger: PopupTrigger,
) -> Result<()> {
    let binary = crate::resolve_pmux_binary()?;
    let sidebar_pane = std::env::var("TMUX_PANE").unwrap_or_else(|_| context.host_pane.clone());
    let mut commands = MenuCommands::new(binary, context, &sidebar_pane);
    let mut items = resumable_menu_items(&mut commands, entry)?;
    items.push(cancel_item(context));
    commands.save()?;
    show_native_menu(
        context,
        &sidebar_pane,
        row,
        trigger,
        items,
        "resumable session",
    )
}

fn resumable_menu_items(
    commands: &mut MenuCommands,
    entry: &crate::HistoryEntry,
) -> Result<Vec<NativeMenuItem>> {
    let target = resumable_target(entry);
    let mut items = Vec::new();
    match resumable_cwd(entry) {
        Ok(cwd) => {
            for (label, action) in resumable_menu_actions() {
                items.push((
                    label.to_string(),
                    String::new(),
                    commands.run(&target, action, Some(cwd), None)?,
                ));
            }
        }
        // A leading `-` makes a native menu item a disabled note.
        Err(err) => items.push((
            format!(
                "-{}",
                tmux_format_escape_levels(&format!("Cannot resume: {err}"), 1)
            ),
            String::new(),
            String::new(),
        )),
    }
    Ok(items)
}

/// Labels and `sidebar-action` names for a resumable session's menu, with the
/// glyphs aligned in one column like the process menu's.
pub(super) fn resumable_menu_actions() -> [(&'static str, &'static str); 2] {
    [
        ("Resume beside rightmost pane ⇥", "resume"),
        ("Resume in new window         ❐", "resume-window"),
    ]
}

/// The menu's last item: the outward arrow closes it with no side effect.
fn cancel_item(context: &crate::SidebarContext) -> NativeMenuItem {
    (
        "Cancel".to_string(),
        if context.sidebar_on_right {
            "Right"
        } else {
            "Left"
        }
        .to_string(),
        String::new(),
    )
}

/// Open a native tmux menu of `(label, shortcut, command)` items beside the
/// sidebar row. Labels and commands must already be safe to format-expand.
fn show_native_menu(
    context: &crate::SidebarContext,
    sidebar_pane: &str,
    row: u16,
    trigger: PopupTrigger,
    items: Vec<NativeMenuItem>,
    what: &str,
) -> Result<()> {
    let (menu_x, menu_y) = native_action_menu_position(context.sidebar_on_right, row);
    let mut args = vec![
        "-S".to_string(),
        context.socket.clone(),
        "display-menu".to_string(),
        "-t".to_string(),
        sidebar_pane.to_string(),
        "-x".to_string(),
        menu_x,
        "-y".to_string(),
        menu_y,
    ];
    args.extend(menu_mode_flags(
        outer_tmux_version(&context.socket),
        trigger,
    ));
    for (label, shortcut, command) in items {
        args.extend([label, shortcut, command]);
    }
    let status = crate::tmux::tmux_command()
        .args(args)
        .status()
        .with_context(|| format!("open pmux {what} menu"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("outer tmux could not open the {what} menu")
    }
}

/// The outer tmux server's version, asked once: a sidebar process talks to
/// one outer server.
fn outer_tmux_version(socket: &str) -> Option<(u32, u32, u32)> {
    static VERSION: std::sync::OnceLock<Option<(u32, u32, u32)>> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        crate::tmux::tmux_command()
            .args(["-S", socket, "display-message", "-p", "#{version}"])
            .output()
            .ok()
            .and_then(|output| {
                crate::tmux::parse_tmux_version(&String::from_utf8_lossy(&output.stdout))
            })
    })
}

/// The selection-style and input-mode flags for a menu opened by `trigger`
/// on an outer tmux of `version`. tmux rejects the whole `display-menu` if
/// any flag is unknown: `-H` and `-C` arrived in 3.4, `-M` only in 3.5. An
/// unrecognized version (a development build) is assumed to be new.
fn menu_mode_flags(version: Option<(u32, u32, u32)>, trigger: PopupTrigger) -> Vec<String> {
    let at_least = |minimum| version.is_none_or(|version| version >= minimum);
    let (tmux_3_4, tmux_3_5) = (at_least((3, 4, 0)), at_least((3, 5, 0)));
    let mut flags = Vec::new();
    if tmux_3_4 {
        flags.extend([
            "-H".to_string(),
            NATIVE_ACTION_MENU_SELECTED_STYLE.to_string(),
        ]);
    }
    match trigger {
        // Mouse opens follow the pointer: `-M` makes the menu handle mouse
        // events (item clicks and click-away dismissal) and starts with nothing
        // selected. tmux honors a starting choice ONLY for keyboard-only menus,
        // so `-C` and `-M` are mutually exclusive here.
        //
        // `-O` is required alongside `-M`: tmux reports plain pointer motion
        // with the same button code as a release, so without it the first
        // mouse movement after opening closes the menu (or, over an item,
        // runs it). With `-O`, motion only moves the highlight, a click picks
        // an item, and a click outside still dismisses the menu.
        //
        // Before tmux 3.5 there is no `-M`; menus opened by a mouse event
        // take mouse input anyway.
        PopupTrigger::Mouse => {
            if tmux_3_5 {
                flags.push("-M".to_string());
            }
            flags.push("-O".to_string());
        }
        // Keyboard opens are driven entirely by keys, so omit `-M` and pre-select
        // the first item with `-C 0` so it is highlighted and Enter runs it. With
        // `-M` present tmux leaves the selection at -1 and `-C` is ignored.
        // tmux 3.3a has no `-C` and starts on the first item already.
        PopupTrigger::Keyboard => {
            if tmux_3_4 {
                flags.extend(["-C".to_string(), "0".to_string()]);
            }
        }
    }
    flags
}

/// Build tmux-native menu coordinates from the actual target pane.  tmux
/// expands these immediately before drawing, so the menu shares the sidebar's
/// outside edge even if its pane moved or changed width since the last render.
/// The vertical coordinate follows the selected process's first line and only
/// rises when the complete menu would extend past the sidebar's bottom edge.
fn native_action_menu_position(sidebar_on_right: bool, item_row: u16) -> (String, String) {
    let desired_y = format!("#{{e|+|:#{{popup_pane_top}},{item_row}}}");
    let maximum_y = "#{e|-|:#{popup_pane_bottom},#{popup_height}}";
    let y = format!("#{{?#{{>:{desired_y},{maximum_y}}},{maximum_y},{desired_y}}}");
    let x = if sidebar_on_right {
        // Sidebar on the right: place the menu's right edge against the
        // sidebar's left edge, so it opens over the content pane to the left.
        "#{e|-|:#{popup_pane_left},#{popup_width}}".to_string()
    } else {
        // Sidebar on the left: open the menu just past the sidebar's right
        // edge. `popup_pane_right` is tmux's *inside* anchor (the pane's right
        // edge minus the menu width), so add the width back to land the menu's
        // left edge on the column immediately outside the sidebar.
        "#{e|+|:#{popup_pane_right},#{popup_width}}".to_string()
    };
    (x, y)
}

pub(super) fn shell_join(parts: &[String]) -> Result<String> {
    parts
        .iter()
        .map(|part| {
            shlex::try_quote(part)
                .map(|quoted| quoted.into_owned())
                .map_err(|_| anyhow!("popup argument contains an unsupported nul byte"))
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_use_only_flags_the_outer_tmux_understands() {
        let style = NATIVE_ACTION_MENU_SELECTED_STYLE.to_string();
        let mouse = |version| menu_mode_flags(version, PopupTrigger::Mouse);
        let keyboard = |version| menu_mode_flags(version, PopupTrigger::Keyboard);
        // tmux rejects display-menu outright if it sees a flag it lacks.
        assert_eq!(mouse(Some((3, 3, 1))), ["-O"]);
        assert!(keyboard(Some((3, 3, 1))).is_empty());
        // 3.4 (Ubuntu 24.04) has -H and -C but not -M.
        assert_eq!(mouse(Some((3, 4, 0))), ["-H", &style, "-O"]);
        assert_eq!(keyboard(Some((3, 4, 0))), ["-H", &style, "-C", "0"]);
        assert_eq!(mouse(Some((3, 5, 1))), ["-H", &style, "-M", "-O"]);
        assert_eq!(keyboard(Some((3, 8, 0))), ["-H", &style, "-C", "0"]);
        assert_eq!(mouse(None), ["-H", &style, "-M", "-O"]);
    }
    use crate::tui::test_support::*;
    use std::thread;

    const HOSTILE: &str = "x#(touch /tmp/pmux-pwned)#{session_name}#[fg=red] $(id) `id` 'q\" ; ~";
    // A `#` in the binary path exercises the command escaping too.
    const BINARY: &str = "/opt/p#m dir/pmux";

    /// Model one tmux format expansion of text that must contain no live
    /// format: `##` becomes `#`, and any other `#` would start a format.
    fn expand_inert(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '#' {
                assert_eq!(
                    chars.next(),
                    Some('#'),
                    "unescaped `#` would be format-expanded in {text:?}"
                );
            }
            out.push(c);
        }
        out
    }

    fn expand_inert_times(text: &str, times: u32) -> String {
        (0..times).fold(text.to_string(), |text, _| expand_inert(&text))
    }

    fn split(command: &str) -> Vec<String> {
        shlex::split(command).expect("tmux command parses")
    }

    fn commands() -> MenuCommands {
        MenuCommands::new(PathBuf::from(BINARY), &test_context(), "%9")
    }

    /// The shell command a `run-shell` item finally runs, after `levels`
    /// expansions: only the binary, the token, and the item index.
    fn assert_runs_item(command: &str, commands: &MenuCommands, index: usize, levels: u32) {
        let parts = split(command);
        assert_eq!(parts[..2], ["run-shell", "-b"]);
        let shell = expand_inert_times(&parts[2], levels);
        assert_eq!(
            shlex::split(&shell).unwrap(),
            [
                BINARY,
                "sidebar-menu-item",
                &commands.token,
                &index.to_string()
            ]
        );
    }

    #[test]
    fn format_escape_doubles_hashes_once_per_expansion() {
        assert_eq!(tmux_format_escape_levels("a#(id)b", 0), "a#(id)b");
        assert_eq!(tmux_format_escape_levels("a#(id)b", 1), "a##(id)b");
        assert_eq!(tmux_format_escape_levels("##", 2), "########");
        assert_eq!(tmux_format_escape_levels("plain", 3), "plain");
        assert_eq!(
            expand_inert_times(&tmux_format_escape_levels(HOSTILE, 3), 3),
            HOSTILE
        );
    }

    #[test]
    fn hostile_process_and_group_names_stay_out_of_process_menu_commands() {
        let mut process = test_process(Some("/work"), Some("/work"));
        process.process.name = HOSTILE.to_string();
        let groups = vec![crate::groups::ManualGroup {
            id: "g1".to_string(),
            name: format!("costs {HOSTILE}"),
            processes: vec![HOSTILE.to_string()],
        }];
        let mut commands = commands();
        let items = session_menu_items(&mut commands, &process, &test_context(), &groups).unwrap();

        for (label, _, command) in &items {
            // Labels are expanded once and must read back literally.
            expand_inert(label);
            assert!(!command.contains("pmux-pwned") && !command.contains("$(id)"));
        }
        let add = items
            .iter()
            .find(|(label, _, _)| label.contains("Add to"))
            .unwrap();
        assert_eq!(expand_inert(&add.0), format!("✓ Add to costs {HOSTILE}"));
        let add_index = commands
            .request
            .items
            .iter()
            .position(|item| item.action == "group-add")
            .unwrap();
        assert_runs_item(&add.2, &commands, add_index, 2);
        assert_eq!(commands.request.items[add_index].target, HOSTILE);
        assert_eq!(
            commands.request.items[add_index].value.as_deref(),
            Some("g1")
        );
        assert_runs_item(&items[0].2, &commands, 0, 2);

        // The new-group prompt stores the typed text without a shell and
        // then runs the item.
        let (_, _, prompt) = items.last().unwrap();
        let parts = split(prompt);
        assert_eq!(parts[..3], ["command-prompt", "-p", "New manual group:"]);
        let index = commands.request.items.len() - 1;
        assert!(commands.request.items[index].prompted);
        assert_eq!(commands.request.items[index].action, "group-create");
        let template = expand_inert(&parts[3]);
        let (store, run) = template.split_once(" ; ").unwrap();
        assert_eq!(
            store,
            format!("set-option -g @pmux_menu_input_{} \"%%%\"", commands.token)
        );
        assert_runs_item(&tmux_format_escape_levels(run, 1), &commands, index, 2);
    }

    #[test]
    fn hostile_group_labels_and_cwds_stay_out_of_group_menu_commands() {
        let label = format!("/srv/{HOSTILE}");
        let members = vec![HOSTILE.to_string(), "b".to_string()];
        let mut commands = commands();
        let items = group_menu_items(
            &mut commands,
            SidebarGroupKind::Manual,
            "manual:g1",
            &label,
            &label,
            &members,
        )
        .unwrap();
        assert_eq!(items.len(), 5);
        for (index, (menu_label, _, command)) in items.iter().enumerate() {
            expand_inert(menu_label);
            let parts = split(command);
            if parts[0] == "confirm-before" {
                // The label appears only in the prompt, which is expanded by
                // the menu and again by the prompt, and reads back literally.
                let prompt = expand_inert_times(&parts[2], 2);
                assert!(prompt.contains(&label), "{prompt}");
                assert_runs_item(&parts[3], &commands, index, 3);
            } else {
                assert!(!command.contains("pmux-pwned") && !command.contains("$(id)"));
                assert_runs_item(command, &commands, index, 2);
            }
        }
        let kill = &commands.request.items[3];
        assert_eq!(kill.action, "group-kill");
        assert_eq!(kill.target, label);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(kill.value.as_deref().unwrap()).unwrap(),
            members
        );

        let mut commands = MenuCommands::new(PathBuf::from(BINARY), &test_context(), "%9");
        let items = group_menu_items(
            &mut commands,
            SidebarGroupKind::Automatic,
            &format!("auto:cwd:{label}"),
            &label,
            &label,
            &members,
        )
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(commands.request.items[0].kind.as_deref(), Some("auto:cwd"));
    }

    #[test]
    fn hostile_resumable_cwds_stay_out_of_menu_commands_and_labels() {
        let mut entry = test_history_entry("abc", HOSTILE, 1);
        let mut commands = commands();
        let items = resumable_menu_items(&mut commands, &entry).unwrap();
        assert_eq!(items.len(), 2);
        for (index, (_, _, command)) in items.iter().enumerate() {
            assert_runs_item(command, &commands, index, 2);
        }
        assert_eq!(commands.request.items[0].value.as_deref(), Some(HOSTILE));
        assert_eq!(commands.request.items[0].target, "claude:abc");

        // A missing directory is reported in a disabled label instead.
        entry.cwd_exists = false;
        let items = resumable_menu_items(&mut commands, &entry).unwrap();
        let label = &items[0].0;
        assert!(label.starts_with('-'));
        assert!(expand_inert(label).contains(HOSTILE));
        assert!(items[0].2.is_empty());
    }

    #[test]
    fn menu_requests_round_trip_once_through_their_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut commands = commands();
        commands.run(HOSTILE, "attach", None, None).unwrap();
        commands
            .run("k", "group-kill", Some("[\"a\"]"), Some("manual"))
            .unwrap();
        commands.save_in(dir.path()).unwrap();

        let (request, item) = take_menu_item(dir.path(), &commands.token, 1).unwrap();
        assert_eq!(request, commands.request);
        assert_eq!(item, commands.request.items[1]);
        // A menu runs one item; its request is then spent.
        assert!(take_menu_item(dir.path(), &commands.token, 0).is_err());

        commands.token = "a".repeat(MENU_TOKEN_LEN);
        commands.save_in(dir.path()).unwrap();
        assert!(take_menu_item(dir.path(), &commands.token, 7).is_err());
        assert!(take_menu_item(dir.path(), "../../etc/passwd", 0).is_err());
        assert!(take_menu_item(dir.path(), &"A".repeat(MENU_TOKEN_LEN), 0).is_err());
    }

    #[test]
    fn menu_tokens_are_fresh_and_path_safe() {
        let first = commands();
        let second = commands();
        assert!(valid_menu_token(&first.token));
        assert_ne!(first.token, second.token);
    }

    #[test]
    fn saving_a_menu_prunes_stale_requests() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("old.json");
        let fresh = dir.path().join("new.json");
        fs::write(&stale, "{}").unwrap();
        fs::write(&fresh, "{}").unwrap();
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - MENU_REQUEST_TTL - Duration::from_secs(60))
            .unwrap();
        commands().save_in(dir.path()).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    /// A throwaway tmux server (its own socket path, never a user's server)
    /// with a real client attached on a pseudo-terminal, since `display-menu`
    /// needs a client to draw on.
    struct ThrowawayTmux {
        dir: tempfile::TempDir,
        socket: String,
        client: libc::pid_t,
        terminal: fs::File,
        menus: Vec<std::process::Child>,
    }

    impl ThrowawayTmux {
        fn start() -> Option<Self> {
            use std::ffi::CString;
            use std::os::fd::FromRawFd;
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                return None;
            }
            let dir = tempfile::Builder::new()
                .prefix("pmux-menu-")
                .tempdir_in("/tmp")
                .unwrap();
            let socket = dir.path().join("s").display().to_string();
            let status = crate::tmux::tmux_command()
                .env_remove("TMUX")
                .env("PMUX_MENU_TEST_OUT", dir.path().join("argv"))
                .args(["-S", &socket, "-f", "/dev/null", "new-session", "-d"])
                .args(["-x", "100", "-y", "30", "sleep 600"])
                .status()
                .unwrap();
            assert!(status.success());
            // Prepare everything before forking: the child may only exec.
            let env = CString::new("/usr/bin/env").unwrap();
            let argv: Vec<CString> = [
                "env",
                "-u",
                "TMUX",
                "TERM=xterm-256color",
                "tmux",
                "-S",
                &socket,
                "attach",
            ]
            .iter()
            .map(|arg| CString::new(*arg).unwrap())
            .collect();
            let mut argv_ptrs: Vec<*const libc::c_char> =
                argv.iter().map(|arg| arg.as_ptr()).collect();
            argv_ptrs.push(std::ptr::null());
            let mut size = libc::winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let mut master = 0;
            // SAFETY: the child only calls exec (or _exit) on pointers that
            // stay alive in the parent's memory image.
            let client = unsafe {
                libc::forkpty(
                    &mut master,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    // A raw pointer fits both the Linux (`*const`) and macOS (`*mut`) signatures.
                    &raw mut size,
                )
            };
            assert!(client >= 0, "forkpty failed");
            if client == 0 {
                unsafe {
                    libc::execv(env.as_ptr(), argv_ptrs.as_ptr());
                    libc::_exit(127);
                }
            }
            // SAFETY: forkpty returned a fresh descriptor that we now own.
            let terminal = unsafe { fs::File::from_raw_fd(master) };
            // Drain the client's output so it never blocks on a full pty.
            let mut reader = terminal.try_clone().unwrap();
            thread::spawn(move || {
                let mut sink = [0u8; 4096];
                while matches!(io::Read::read(&mut reader, &mut sink), Ok(n) if n > 0) {}
            });
            let server = Self {
                dir,
                socket,
                client,
                terminal,
                menus: Vec::new(),
            };
            server.wait_for("an attached client", || {
                !server.tmux(&["list-clients"]).is_empty()
            });
            Some(server)
        }

        fn tmux(&self, args: &[&str]) -> String {
            let output = crate::tmux::tmux_command()
                .env_remove("TMUX")
                .args(["-S", &self.socket])
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for(&self, what: &str, mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(50));
            }
        }

        fn keys(&mut self, keys: &[u8]) {
            io::Write::write_all(&mut self.terminal, keys).unwrap();
            thread::sleep(Duration::from_millis(250));
        }

        fn open_menu(&mut self, items: &[NativeMenuItem]) {
            let mut command = crate::tmux::tmux_command();
            command
                .env_remove("TMUX")
                .args(["-S", &self.socket, "display-menu"])
                .args(menu_mode_flags(
                    outer_tmux_version(&self.socket),
                    PopupTrigger::Keyboard,
                ));
            for (label, key, item_command) in items {
                command.args([label, key, item_command]);
            }
            // `display-menu` waits for the menu to close.
            self.menus.push(command.spawn().unwrap());
            thread::sleep(Duration::from_millis(500));
        }

        /// The arguments the recorded fake pmux binary last ran with.
        fn take_invocation(&self) -> Vec<String> {
            let path = self.dir.path().join("argv");
            self.wait_for("the menu item to run", || path.exists());
            thread::sleep(Duration::from_millis(100));
            let text = fs::read_to_string(&path).unwrap();
            fs::remove_file(&path).unwrap();
            text.lines().map(str::to_string).collect()
        }
    }

    impl Drop for ThrowawayTmux {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .env_remove("TMUX")
                .args(["-S", &self.socket, "kill-server"])
                .status();
            unsafe {
                libc::kill(self.client, libc::SIGKILL);
                libc::waitpid(self.client, std::ptr::null_mut(), 0);
            }
            for menu in &mut self.menus {
                let _ = menu.kill();
                let _ = menu.wait();
            }
        }
    }

    #[test]
    fn hostile_menus_run_only_their_token_on_a_real_tmux_server() {
        use std::os::unix::fs::PermissionsExt;
        let Some(mut server) = ThrowawayTmux::start() else {
            return;
        };
        let dir = server.dir.path().to_path_buf();
        // A fake pmux, at a path that needs escaping, records its arguments.
        let bin_dir = dir.join("p#m dir 'q");
        fs::create_dir(&bin_dir).unwrap();
        let binary = bin_dir.join("pmux");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PMUX_MENU_TEST_OUT.tmp\"\nmv \"$PMUX_MENU_TEST_OUT.tmp\" \"$PMUX_MENU_TEST_OUT\"\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let pwned = |n: u32| dir.join(format!("pwned{n}")).display().to_string();
        let hostile = format!(
            "x#(touch {}) $(touch {}) `touch {}` 'q\" ; ~ #{{session_name}}",
            pwned(1),
            pwned(2),
            pwned(3)
        );
        let context = crate::SidebarContext {
            socket: server.socket.clone(),
            ..test_context()
        };

        // A group's kill confirmation runs only its own item.
        let mut commands = MenuCommands::new(binary.clone(), &context, "%0");
        let items = group_menu_items(
            &mut commands,
            SidebarGroupKind::Manual,
            "manual:g1",
            &hostile,
            &hostile,
            std::slice::from_ref(&hostile),
        )
        .unwrap();
        server.open_menu(&items);
        server.keys(b"\x1b[B\x1b[B\x1b[B\r");
        server.keys(b"y");
        assert_eq!(
            server.take_invocation(),
            ["sidebar-menu-item", &commands.token, "3"]
        );

        // The new-group prompt keeps the typed text literal.
        let mut process = test_process(Some("/work"), Some("/work"));
        process.process.name = hostile.clone();
        let groups = vec![crate::groups::ManualGroup {
            id: "g1".to_string(),
            name: hostile.clone(),
            processes: Vec::new(),
        }];
        let mut commands = MenuCommands::new(binary.clone(), &context, "%0");
        let items = session_menu_items(&mut commands, &process, &context, &groups).unwrap();
        server.open_menu(&items);
        // Up wraps to the last item, "New manual group…".
        server.keys(b"\x1b[A\r");
        // tmux before 3.4 substitutes a typed `%%` a second time, mangling
        // that text (harmlessly: nothing runs), so only newer tmux gets it.
        let percent = if outer_tmux_version(&server.socket).is_none_or(|v| v >= (3, 4, 0)) {
            " %%"
        } else {
            ""
        };
        let typed = format!("costs $HOME {hostile}{percent} \\");
        server.keys(typed.as_bytes());
        server.keys(b"\r");
        let last = (items.len() - 2).to_string(); // minus the separator
        assert_eq!(
            server.take_invocation(),
            ["sidebar-menu-item", &commands.token, &last]
        );
        let option = menu_input_option(&commands.token);
        assert_eq!(
            crate::tmux::undo_tmux_3_4_dollar_escape(
                &server.tmux(&["show-option", "-gqv", &option]),
                outer_tmux_version(&server.socket),
            ),
            format!("{typed}\n")
        );

        // A plain item runs directly.
        server.open_menu(&items);
        server.keys(b"\r");
        assert_eq!(
            server.take_invocation(),
            ["sidebar-menu-item", &commands.token, "0"]
        );

        for n in 1..=3 {
            assert!(!Path::new(&pwned(n)).exists(), "hostile command {n} ran");
        }
    }

    #[test]
    fn native_action_menu_uses_tmuxs_live_pane_edge_and_selected_row() {
        // Left sidebar: the menu opens just outside the sidebar's right edge,
        // reconstructing the raw edge from tmux's inside-anchored format.
        assert_eq!(
            native_action_menu_position(false, 8),
            (
                "#{e|+|:#{popup_pane_right},#{popup_width}}".to_string(),
                "#{?#{>:#{e|+|:#{popup_pane_top},8},#{e|-|:#{popup_pane_bottom},#{popup_height}}},#{e|-|:#{popup_pane_bottom},#{popup_height}},#{e|+|:#{popup_pane_top},8}}".to_string(),
            )
        );
        assert_eq!(
            native_action_menu_position(true, 8).0,
            "#{e|-|:#{popup_pane_left},#{popup_width}}"
        );
    }
}
