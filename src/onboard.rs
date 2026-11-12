//! `pmux setup`: one command that installs and wires the whole pmux stack.
//!
//! It runs `pmux init`, installs the companion tools pmux is tested with
//! (ai-attn for agent state, Dredge for session search), wires ai-attn into
//! every installed agent, wraps the agent CLIs found on PATH, and connects
//! pmux to the user's tmux. Every step checks what is already in place, so a
//! second run only does what is missing.

use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A step `pmux setup --skip` can leave out. The shell integration from
/// `pmux init` always runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Step {
    /// Installing or upgrading ai-attn and wiring its agent hooks.
    AiAttn,
    /// Installing or upgrading Dredge.
    Dredge,
    /// Wrapping the agent CLIs found on PATH.
    Wrap,
    /// Connecting pmux to your own tmux.
    Tmux,
}

pub(crate) struct Options {
    /// Accept every default without asking.
    pub yes: bool,
    /// Report what would happen without changing anything.
    pub dry_run: bool,
    pub skip: Vec<Step>,
    pub autostart: bool,
    pub preset: Option<String>,
}

type Version = (u32, u32, u32);

/// A tool pmux installs alongside itself, pinned to the release pmux is
/// tested with. The installer script is pinned by tag and checksum, and the
/// installer itself verifies the binary it downloads.
struct Companion {
    name: &'static str,
    tag: &'static str,
    version: Version,
    installer_sha256: &'static str,
    interpreter: &'static str,
    version_env: &'static str,
    version_args: &'static [&'static str],
    installer_args: &'static [&'static str],
    purpose: &'static str,
}

const AI_ATTN: Companion = Companion {
    name: "ai-attn",
    tag: "v0.5.0",
    version: (0, 5, 0),
    installer_sha256: "8331fe4eccba549f95fda33fb41a317c3a327266939a85ff172177ba7ee49709",
    interpreter: "bash",
    version_env: "AI_ATTN_VERSION",
    version_args: &["version"],
    // Without --force the installer asks before replacing a binary, and
    // refuses outright when nobody can answer. Setup only runs it to install
    // or upgrade, so replacing is the point.
    installer_args: &["--force"],
    purpose: "agent state (working, waiting, done) in the sidebar and your tmux tabs",
};

const DREDGE: Companion = Companion {
    name: "dredge",
    tag: "v0.1.1",
    version: (0, 1, 1),
    installer_sha256: "cd2add9046b7b23615205733814f8c55917f1071c12fa680764f0d6b6ee68c77",
    interpreter: "sh",
    version_env: "DREDGE_VERSION",
    version_args: &["--version"],
    installer_args: &[],
    purpose: "full-text search of past agent sessions in the picker",
};

impl Companion {
    fn installer_url(&self) -> String {
        format!(
            "https://raw.githubusercontent.com/cosmicbuffalo/{}/{}/install.sh",
            self.name, self.tag
        )
    }

    /// Where the installer puts the command on PATH.
    fn managed_path(&self, home: &Path) -> PathBuf {
        home.join(".local").join("bin").join(self.name)
    }
}

/// What setup does about one companion.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Install,
    Upgrade {
        path: PathBuf,
        from: Version,
    },
    /// Leave the installed copy alone, with an optional note on why.
    Keep {
        path: PathBuf,
        note: Option<String>,
    },
}

fn plan(found: Option<(PathBuf, Option<Version>)>, pinned: (Version, &str), managed: bool) -> Plan {
    let Some((path, version)) = found else {
        return Plan::Install;
    };
    match version {
        Some(version) if version >= pinned.0 => Plan::Keep { path, note: None },
        Some(version) if managed => Plan::Upgrade {
            path,
            from: version,
        },
        Some(version) => Plan::Keep {
            path,
            note: Some(format!(
                "{} is older than the {} pmux is tested with; it was not installed by its \
                 installer, so update it the way you installed it",
                format_version(version),
                pinned.1
            )),
        },
        None => Plan::Keep {
            path,
            note: Some("could not read its version".to_string()),
        },
    }
}

fn format_version((major, minor, patch): Version) -> String {
    format!("v{major}.{minor}.{patch}")
}

/// The first `X.Y.Z` in a `--version` line, with or without a leading `v`.
fn parse_version(text: &str) -> Option<Version> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .map(|word| word.trim_start_matches('v'))
        .find_map(|word| {
            let mut parts = word.splitn(3, '.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch: String = parts
                .next()?
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            Some((major, minor, patch.parse().ok()?))
        })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first executable `name` on PATH.
fn find_on_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

struct Session {
    opts: Options,
    interactive: bool,
    home: PathBuf,
    /// Follow-ups for the summary at the end.
    todo: Vec<String>,
}

impl Session {
    fn heading(&self, title: &str) {
        println!("\n==> {title}");
    }

    /// Ask a yes/no question that defaults to yes. `--yes` answers it, and
    /// without a terminal nobody can, so the answer is no.
    fn confirm(&self, question: &str) -> Result<bool> {
        if self.opts.yes {
            return Ok(true);
        }
        if !self.interactive {
            return Ok(false);
        }
        print!("{question} [Y/n] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer)? == 0 {
            return Ok(false);
        }
        Ok(!matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "n" | "no"
        ))
    }

    fn skipped(&self, step: Step) -> bool {
        self.opts.skip.contains(&step)
    }

    /// Note that a step needs an answer nobody was there to give.
    fn unanswered(&mut self, what: &str) {
        self.todo.push(format!(
            "{what}: run `pmux setup` in a terminal, or `pmux setup --yes`"
        ));
    }
}

pub(crate) fn run(opts: Options) -> Result<()> {
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let mut session = Session {
        opts,
        interactive,
        home,
        todo: Vec::new(),
    };
    if session.opts.dry_run {
        println!("Dry run: nothing will be changed.");
    }

    shell_step(&mut session)?;
    let mut companions_changed = false;
    if !session.skipped(Step::AiAttn) {
        companions_changed |= companion_step(&mut session, &AI_ATTN)?;
    }
    if !session.skipped(Step::Dredge) {
        companions_changed |= companion_step(&mut session, &DREDGE)?;
    }
    if !session.skipped(Step::Wrap) {
        wrap_step(&mut session)?;
    }
    if !session.skipped(Step::Tmux) {
        tmux_step(&mut session)?;
    }

    if !session.opts.dry_run {
        // A running pmux server keeps the config it loaded, which skips the
        // ai-attn integration when ai-attn was missing then.
        match crate::restart(false) {
            Ok(crate::RestartResult::SoftReloaded) if companions_changed => {
                println!("\nReloaded pmux's tmux config; every process kept running.");
            }
            Ok(_) => {}
            Err(error) => session
                .todo
                .push(format!("Reload pmux's tmux config: pmux restart ({error})")),
        }
    }

    println!();
    if session.opts.dry_run {
        println!("Dry run complete; run without --dry-run to apply.");
    } else if session.todo.is_empty() {
        println!("pmux is set up. `pmux doctor` checks everything.");
    } else {
        println!("Almost done. Still to do:");
        for item in &session.todo {
            println!("  - {item}");
        }
    }
    Ok(())
}

fn shell_step(session: &mut Session) -> Result<()> {
    session.heading("Shell integration");
    if session.opts.dry_run {
        println!("Would run `pmux init`: completions and wrappers in your shell rc file.");
        return Ok(());
    }
    let (rc_path, changed, boot_message) = crate::init_with_autostart(session.opts.autostart)?;
    match (rc_path, changed) {
        (Some(rc), true) => {
            let rc = crate::display_path_buf(&rc);
            println!("Updated {rc}.");
            session.todo.push(format!(
                "Reload your shell to pick up wrappers: source {rc}"
            ));
        }
        (Some(rc), false) => println!("Already set up in {}.", crate::display_path_buf(&rc)),
        (None, _) => println!(
            "Automatic shell integration supports Bash and Zsh; \
             `pmux completions <shell>` covers others."
        ),
    }
    if let Some(message) = boot_message {
        println!("{message}");
    }
    match &session.opts.preset {
        Some(name) => {
            crate::set_row_preset(name)?;
            println!("Rows use the {name} preset.");
        }
        None if !session.opts.yes => {
            if let Some(name) = crate::prompt_for_row_preset()? {
                println!("Rows use the {name} preset.");
            }
        }
        None => {}
    }
    let docs = if session.opts.yes {
        crate::agent_docs::AgentDocsChoice::Install
    } else {
        crate::agent_docs::AgentDocsChoice::Ask
    };
    for change in crate::agent_docs::apply(docs)? {
        println!(
            "Agent docs: {} {}",
            change.action,
            crate::display_path_buf(&change.path)
        );
    }
    Ok(())
}

/// Install, upgrade, or keep one companion, and for ai-attn wire its hooks.
/// Returns whether anything was installed.
fn companion_step(session: &mut Session, companion: &Companion) -> Result<bool> {
    session.heading(&format!("{}: {}", companion.name, companion.purpose));
    let managed = companion.managed_path(&session.home);
    let found = find_on_path(companion.name)
        .or_else(|| is_executable(&managed).then(|| managed.clone()))
        .map(|path| {
            let version = Command::new(&path)
                .args(companion.version_args)
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| parse_version(&String::from_utf8_lossy(&out.stdout)));
            (path, version)
        });
    let is_managed = found
        .as_ref()
        .is_some_and(|(path, _)| same_file(path, &managed));
    let pinned = (companion.version, companion.tag);
    let installed = match plan(found, pinned, is_managed) {
        Plan::Keep { path, note } => {
            println!("Using {}.", crate::display_path_buf(&path));
            if let Some(note) = note {
                println!("Note: {note}.");
            }
            if companion.name == AI_ATTN.name {
                wire_ai_attn(session, &path)?;
            }
            false
        }
        Plan::Install => {
            let action = format!("install {} {}", companion.name, companion.tag);
            run_installer(session, companion, &action)?
        }
        Plan::Upgrade { path, from } => {
            let action = format!(
                "upgrade {} from {} to {}",
                crate::display_path_buf(&path),
                format_version(from),
                companion.tag
            );
            run_installer(session, companion, &action)?
        }
    };
    if installed && companion.name == DREDGE.name {
        println!("pmux keeps Dredge's index up to date itself; no need to run `dredge index`.");
    }
    if installed && find_on_path(companion.name).is_none() {
        session.todo.push(format!(
            "Add {} to PATH so pmux finds {}",
            crate::display_path_buf(managed.parent().unwrap_or(&managed)),
            companion.name
        ));
    }
    Ok(installed)
}

fn wire_ai_attn(session: &mut Session, ai_attn: &Path) -> Result<()> {
    if session.opts.dry_run {
        println!("Would run `ai-attn setup` to wire hooks into your installed agents.");
        return Ok(());
    }
    println!("Wiring hooks into your installed agents (ai-attn setup)...");
    let status = Command::new(ai_attn)
        .arg("setup")
        .status()
        .context("failed to run ai-attn setup")?;
    if !status.success() {
        session
            .todo
            .push("Fix the problem `ai-attn setup` reported, then rerun it".to_string());
    }
    Ok(())
}

/// Download the pinned installer, check its checksum, and run it. Its own
/// output covers what it installs; ai-attn's also wires the agent hooks.
fn run_installer(session: &mut Session, companion: &Companion, action: &str) -> Result<bool> {
    let url = companion.installer_url();
    if session.opts.dry_run {
        println!("Would {action} with {url}.");
        return Ok(false);
    }
    println!("Going to {action}.");
    let dir = tempfile::Builder::new()
        .prefix("pmux-setup-")
        .tempdir()
        .context("failed to create a temporary directory")?;
    let script = dir.path().join("install.sh");
    let result = download(&url, &script)
        .and_then(|()| verify_sha256(&script, companion.installer_sha256))
        .and_then(|()| {
            let status = Command::new(companion.interpreter)
                .arg(&script)
                .args(companion.installer_args)
                .env(companion.version_env, companion.tag)
                .status()
                .with_context(|| format!("failed to run {}", companion.interpreter))?;
            if !status.success() {
                bail!("the {} installer failed ({status})", companion.name);
            }
            Ok(())
        });
    match result {
        Ok(()) => Ok(true),
        Err(error) => {
            eprintln!("Could not {action}: {error:#}");
            session.todo.push(format!(
                "Install {}: curl -fsSL {url} | {}={} {}",
                companion.name, companion.version_env, companion.tag, companion.interpreter
            ));
            Ok(false)
        }
    }
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let status = if find_on_path("curl").is_some() {
        Command::new("curl")
            .args(["-fsSL", url, "-o"])
            .arg(destination)
            .status()
    } else if find_on_path("wget").is_some() {
        Command::new("wget")
            .arg("-qO")
            .arg(destination)
            .arg(url)
            .status()
    } else {
        bail!("curl or wget is required");
    };
    if !status.context("failed to start the download")?.success() {
        bail!("download of {url} failed");
    }
    Ok(())
}

fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
    let output = if find_on_path("sha256sum").is_some() {
        Command::new("sha256sum").arg(file).output()
    } else if find_on_path("shasum").is_some() {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(file)
            .output()
    } else {
        bail!("sha256sum or shasum is required");
    }
    .context("failed to checksum the installer")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let actual = text.split_whitespace().next().unwrap_or_default();
    if actual != expected {
        bail!("installer checksum mismatch (expected {expected}, got {actual}); not running it");
    }
    Ok(())
}

/// Agent commands on PATH that are not wrapped yet, sorted.
fn unwrapped_agents(wrapped: &[String], on_path: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let mut agents: Vec<&'static str> = crate::agents::Agent::ALL
        .into_iter()
        .map(|agent| agent.name())
        .filter(|name| on_path(name) && !wrapped.iter().any(|w| w == name))
        .collect();
    agents.sort_unstable();
    agents
}

fn wrap_step(session: &mut Session) -> Result<()> {
    session.heading("Wrap your agents");
    let wrapped = crate::wraps()?;
    let agents = unwrapped_agents(&wrapped, |name| find_on_path(name).is_some());
    if !wrapped.is_empty() {
        println!("Already wrapped: {}.", wrapped.join(", "));
    }
    if agents.is_empty() {
        if wrapped.is_empty() {
            println!("No agent CLIs found on PATH. Later, `pmux wrap <command>` wraps one.");
        }
        return Ok(());
    }
    let list = agents.join(", ");
    if session.opts.dry_run {
        println!("Would wrap {list}.");
        return Ok(());
    }
    println!("Wrapping makes the plain command start in pmux; `pmux unwrap` undoes it.");
    if !session.confirm(&format!("Wrap {list}?"))? {
        if session.interactive && !session.opts.yes {
            println!("Skipped. `pmux wrap <command>` wraps one later.");
        } else {
            session.unanswered(&format!("Wrap {list}"));
        }
        return Ok(());
    }
    for agent in agents {
        crate::wrap(agent)?;
        println!("Wrapped {agent}.");
    }
    Ok(())
}

const SOURCE_LINE: &str = "source-file -q ~/.config/pmux/outer-tmux.conf";
const SOURCE_COMMENT: &str =
    "# pmux: the sidebar (prefix + Ctrl-p) and agent attention on window tabs";

/// How the user's tmux config stands with pmux's integration.
#[derive(Debug, PartialEq, Eq)]
enum TmuxConfig {
    /// The config already loads the integration.
    Configured(PathBuf),
    /// A tmux.rb config, which setup leaves to the user.
    TmuxRb(PathBuf),
    /// A plain config (or none yet) to add the source line to.
    Plain { path: PathBuf, exists: bool },
}

fn xdg_tmux_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(xdg).join("tmux"));
    }
    let default = home.join(".config").join("tmux");
    if !dirs.contains(&default) {
        dirs.push(default);
    }
    dirs
}

fn classify_tmux_config(home: &Path) -> TmuxConfig {
    let read = |path: &Path| fs::read_to_string(path).ok();
    let mut rb = vec![home.join(".tmux.conf.rb")];
    let mut plain = vec![home.join(".tmux.conf")];
    for dir in xdg_tmux_dirs(home) {
        rb.push(dir.join("tmux.conf.rb"));
        plain.push(dir.join("tmux.conf"));
    }
    for path in &rb {
        if read(path).is_some_and(|text| text.contains("Pmux.integration")) {
            return TmuxConfig::Configured(path.clone());
        }
    }
    for path in &plain {
        if read(path).is_some_and(|text| {
            text.contains("pmux/outer-tmux.conf") || text.contains("cosmicbuffalo/pmux")
        }) {
            return TmuxConfig::Configured(path.clone());
        }
    }
    if let Some(path) = rb.iter().find(|path| path.is_file()) {
        return TmuxConfig::TmuxRb(path.clone());
    }
    match plain.iter().find(|path| path.is_file()) {
        Some(path) => TmuxConfig::Plain {
            path: path.clone(),
            exists: true,
        },
        None => TmuxConfig::Plain {
            path: home.join(".tmux.conf"),
            exists: false,
        },
    }
}

fn is_tpm_run_line(line: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("run ") || line.starts_with("run-shell ")) && line.contains("tpm")
}

/// The config with the source line added: before TPM's `run` line, which
/// TPM wants last, or else at the end.
fn add_source_line(text: &str) -> String {
    let block = format!("{SOURCE_COMMENT}\n{SOURCE_LINE}\n");
    let mut out = String::with_capacity(text.len() + block.len() + 1);
    let mut added = false;
    for line in text.split_inclusive('\n') {
        if !added && is_tpm_run_line(line) {
            out.push_str(&block);
            out.push('\n');
            added = true;
        }
        out.push_str(line);
    }
    if !added {
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(&block);
    }
    out
}

fn sets_window_formats(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#') && line.contains("window-status-") && line.contains("format")
    })
}

/// The socket of the tmux the user works in: the one showing this pmux
/// process, or the one this shell runs in.
fn user_tmux_socket() -> Option<String> {
    if crate::tmux::inside_pmux_client() {
        let session = crate::current_session_name().ok()?;
        crate::state::read_session_host_socket(&session).ok()?
    } else {
        crate::tmux::current_tmux_socket()
    }
}

fn on_socket(socket: &str) -> Command {
    let mut cmd = crate::tmux::tmux_command();
    cmd.arg("-S").arg(socket).env_remove("TMUX");
    cmd
}

fn integration_loaded(socket: &str) -> Option<bool> {
    let out = on_socket(socket)
        .args(["list-keys", "-T", "prefix"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).contains("toggle-sidebar"))
}

fn tmux_step(session: &mut Session) -> Result<()> {
    session.heading("Connect pmux to your tmux");
    let socket = user_tmux_socket();
    let loaded = socket.as_deref().and_then(integration_loaded);
    if loaded == Some(true) {
        println!("Your tmux already loads pmux's integration.");
        return Ok(());
    }
    match classify_tmux_config(&session.home) {
        TmuxConfig::Configured(path) => {
            let path = crate::display_path_buf(&path);
            println!("{path} already loads pmux's integration.");
            if loaded == Some(false) {
                session.todo.push(format!(
                    "Reload your tmux config ({path}) to load pmux's integration"
                ));
            }
        }
        TmuxConfig::TmuxRb(path) => {
            let path = crate::display_path_buf(&path);
            println!("Your tmux config is tmux.rb ({path}); setup leaves it to you.");
            session.todo.push(format!(
                "Add `append Pmux.integration` to {path}, then rebuild and reload it"
            ));
        }
        TmuxConfig::Plain { path, exists } => {
            let shown = crate::display_path_buf(&path);
            let text = if exists {
                fs::read_to_string(&path).with_context(|| format!("failed to read {shown}"))?
            } else {
                String::new()
            };
            if session.opts.dry_run {
                println!("Would add `{SOURCE_LINE}` to {shown}.");
                return Ok(());
            }
            println!(
                "pmux's integration adds the sidebar on prefix + Ctrl-p and agent attention \
                 on your window tabs."
            );
            if !session.confirm(&format!("Add `{SOURCE_LINE}` to {shown}?"))? {
                if session.interactive && !session.opts.yes {
                    println!("Skipped. docs/tmux-integration.md covers doing it by hand.");
                } else {
                    session.unanswered(&format!("Connect pmux to your tmux ({shown})"));
                }
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, add_source_line(&text))
                .with_context(|| format!("failed to write {shown}"))?;
            println!("Added it to {shown}.");
            if sets_window_formats(&text) {
                session.todo.push(format!(
                    "{shown} sets window-status formats, which pmux's integration replaces; \
                     to keep yours, see \"If you already style your window tabs\" in \
                     docs/tmux-integration.md"
                ));
            }
            reload_integration(session, socket.as_deref(), loaded);
        }
    }
    Ok(())
}

/// Load just the integration file into the running tmux, never the whole
/// config: that is the user's to reload.
fn reload_integration(session: &mut Session, socket: Option<&str>, loaded: Option<bool>) {
    let (Some(socket), Some(false)) = (socket, loaded) else {
        println!("It loads the next time your tmux starts.");
        return;
    };
    let conf = match crate::config::outer_tmux_conf_path() {
        Ok(conf) => conf,
        Err(error) => {
            session
                .todo
                .push(format!("Reload your tmux config ({error})"));
            return;
        }
    };
    let ok = on_socket(socket)
        .arg("source-file")
        .arg(&conf)
        .status()
        .is_ok_and(|status| status.success());
    if ok {
        println!("Loaded it into your running tmux.");
    } else {
        session
            .todo
            .push("Reload your tmux config to load pmux's integration".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_from_tool_output() {
        assert_eq!(parse_version("ai-attn v0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("dredge 0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_version("tool 1.2.3-rc1 (abc)"), Some((1, 2, 3)));
        assert_eq!(parse_version("ai-attn dev"), None);
    }

    #[test]
    fn plans_install_upgrade_and_keep() {
        let pinned = ((0, 5, 0), "v0.5.0");
        let path = PathBuf::from("/x/ai-attn");
        assert_eq!(plan(None, pinned, false), Plan::Install);
        assert_eq!(
            plan(Some((path.clone(), Some((0, 6, 0)))), pinned, true),
            Plan::Keep {
                path: path.clone(),
                note: None
            }
        );
        assert_eq!(
            plan(Some((path.clone(), Some((0, 4, 1)))), pinned, true),
            Plan::Upgrade {
                path: path.clone(),
                from: (0, 4, 1)
            }
        );
        // An older copy installed some other way is the user's to update.
        assert!(matches!(
            plan(Some((path.clone(), Some((0, 4, 1)))), pinned, false),
            Plan::Keep { note: Some(_), .. }
        ));
        assert!(matches!(
            plan(Some((path, None)), pinned, true),
            Plan::Keep { note: Some(_), .. }
        ));
    }

    #[test]
    fn source_line_goes_before_tpm_or_at_the_end() {
        let tpm = "set -g mouse on\nset -g @plugin 'x/y'\nrun '~/.tmux/plugins/tpm/tpm'\n";
        let out = add_source_line(tpm);
        let source = out.find(SOURCE_LINE).unwrap();
        assert!(source < out.find("run '~/.tmux").unwrap());
        assert!(out.starts_with("set -g mouse on\n"));

        let plain = add_source_line("set -g mouse on");
        assert_eq!(
            plain,
            format!("set -g mouse on\n\n{SOURCE_COMMENT}\n{SOURCE_LINE}\n")
        );
        assert_eq!(
            add_source_line(""),
            format!("{SOURCE_COMMENT}\n{SOURCE_LINE}\n")
        );
    }

    #[test]
    fn notices_custom_window_formats() {
        assert!(sets_window_formats("set -g window-status-format '#I #W'\n"));
        assert!(!sets_window_formats("# set -g window-status-format x\n"));
        assert!(!sets_window_formats("set -g status-left x\n"));
    }

    #[test]
    fn classifies_tmux_configs() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: tests that touch the environment hold ENV_LOCK.
        unsafe { env::remove_var("XDG_CONFIG_HOME") };
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(
            classify_tmux_config(home),
            TmuxConfig::Plain {
                path: home.join(".tmux.conf"),
                exists: false
            }
        );
        let xdg = home.join(".config/tmux/tmux.conf");
        fs::create_dir_all(xdg.parent().unwrap()).unwrap();
        fs::write(&xdg, "set -g mouse on\n").unwrap();
        assert_eq!(
            classify_tmux_config(home),
            TmuxConfig::Plain {
                path: xdg.clone(),
                exists: true
            }
        );
        let rb = home.join(".config/tmux/tmux.conf.rb");
        fs::write(&rb, "set :mouse, true\n").unwrap();
        assert_eq!(classify_tmux_config(home), TmuxConfig::TmuxRb(rb.clone()));
        fs::write(&xdg, format!("{SOURCE_LINE}\n")).unwrap();
        assert_eq!(classify_tmux_config(home), TmuxConfig::Configured(xdg));
    }

    #[test]
    fn offers_only_unwrapped_agents_on_path() {
        let wrapped = vec!["claude".to_string()];
        let agents = unwrapped_agents(&wrapped, |name| matches!(name, "claude" | "codex" | "pi"));
        assert_eq!(agents, vec!["codex", "pi"]);
    }
}
