use crate::truncate_col;
use anyhow::{Result, anyhow, bail};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use serde::Serialize;
use std::io::{self, IsTerminal, Read, Write};
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "pmux")]
#[command(about = "Process multiplexer")]
#[command(version = env!("PMUX_VERSION"))]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

/// Who spawned a new process. Inside a pmux pane the caller's process is
/// recorded by default.
#[derive(Args, Debug, Default)]
struct ParentArgs {
    /// Record no parent, even when run inside a pmux pane.
    #[arg(long, conflicts_with_all = ["parent", "parent_machine", "parent_url"])]
    no_parent: bool,
    /// Record this process as the parent (must be running, unless remote).
    #[arg(long, value_name = "NAME")]
    parent: Option<String>,
    /// The remote parent's machine; needs --parent and --parent-url.
    #[arg(long, value_name = "MACHINE", requires_all = ["parent", "parent_url"], hide = true)]
    parent_machine: Option<String>,
    /// The remote parent's base URL; needs --parent and --parent-machine.
    #[arg(long, value_name = "URL", requires_all = ["parent", "parent_machine"], hide = true)]
    parent_url: Option<String>,
}

impl ParentArgs {
    fn spec(self) -> crate::ParentSpec {
        if self.no_parent {
            return crate::ParentSpec::None;
        }
        match (self.parent, self.parent_machine, self.parent_url) {
            (Some(process), Some(machine), Some(url)) => crate::ParentSpec::Remote {
                process,
                machine,
                url,
            },
            (Some(name), _, _) => crate::ParentSpec::Local(name),
            _ => crate::ParentSpec::Auto,
        }
    }
}

#[derive(Subcommand, Debug)]
enum QueueCommand {
    /// Add a message. Without --after, new batches stay drafts.
    Add {
        target: String,
        /// Release the whole batch at this local time (e.g. "7pm" or "in 45m").
        #[arg(long)]
        after: Option<String>,
        #[arg(long, conflicts_with = "text")]
        stdin: bool,
        #[arg(required_unless_present = "stdin", trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// List batches including delivery history; omit target to see all processes.
    List {
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set a future release time and arm the batch.
    Schedule { target: String, time: String },
    /// Hold the batch so it is not sent at its release time.
    Pause { target: String },
    /// Re-arm a paused batch; its release time must still be in the future.
    Resume { target: String },
    /// Cancel a batch by ID, including one whose process has exited.
    Cancel { id: String },
    /// Replace a message by its one-based index.
    Edit {
        target: String,
        index: usize,
        #[arg(long, conflicts_with = "text")]
        stdin: bool,
        #[arg(required_unless_present = "stdin", trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Delete a message by its one-based index.
    Remove { target: String, index: usize },
    /// Move a message to a new one-based position.
    Move {
        target: String,
        from: usize,
        to: usize,
    },
    /// Print exactly the combined prompt that will be sent.
    Preview { target: String },
    /// Open the interactive queue for a process.
    Open { target: String },
    /// Run the scheduler (normally started automatically).
    #[command(hide = true)]
    Worker,
}

fn queue_command(command: QueueCommand) -> Result<()> {
    use crate::queue::{self, Edit};
    let index = |n: usize| {
        n.checked_sub(1)
            .ok_or_else(|| anyhow!("message indices start at 1"))
    };
    let input = |stdin: bool, text: Vec<String>| -> Result<String> {
        if stdin {
            let mut value = String::new();
            io::stdin().read_to_string(&mut value)?;
            Ok(value)
        } else {
            Ok(text.join(" "))
        }
    };
    let (selector, changes) = match command {
        QueueCommand::Worker => return queue::worker(),
        QueueCommand::Open { target } => return crate::queue_ui::run(&target),
        QueueCommand::Cancel { id } => {
            queue::cancel(&id)?;
            println!("Cancelled {id}.");
            return Ok(());
        }
        QueueCommand::List { target, json } => {
            queue::ensure_worker()?;
            let target = target.as_deref().map(queue::target).transpose()?;
            let batches = queue::list(target.as_ref())?;
            if json {
                return print_json(&batches);
            }
            if batches.is_empty() {
                println!("No queued batches.");
            }
            for batch in batches {
                println!(
                    "{}  {}  {}  {} message(s)  {}",
                    batch.id,
                    batch.target.session,
                    batch.status.label(),
                    batch.messages.len(),
                    batch
                        .after
                        .map(queue::format_time)
                        .unwrap_or_else(|| "unscheduled".into())
                );
                if let Some(detail) = batch.detail {
                    println!("  {detail}");
                }
            }
            return Ok(());
        }
        QueueCommand::Preview { target } => {
            let target = queue::target(&target)?;
            let batches = queue::list(Some(&target))?;
            let batch = batches
                .iter()
                .rev()
                .find(|b| b.status.editable())
                .ok_or_else(|| anyhow!("no editable batch"))?;
            println!("{}", queue::combined(&batch.messages));
            return Ok(());
        }
        QueueCommand::Add {
            target,
            after,
            stdin,
            text,
        } => {
            let mut changes = vec![Edit::Add(input(stdin, text)?)];
            if let Some(time) = after {
                changes.push(Edit::Schedule(queue::parse_time(&time, queue_now())?));
            }
            (target, changes)
        }
        QueueCommand::Schedule { target, time } => (
            target,
            vec![Edit::Schedule(queue::parse_time(&time, queue_now())?)],
        ),
        QueueCommand::Pause { target } => (target, vec![Edit::Pause]),
        QueueCommand::Resume { target } => (target, vec![Edit::Resume]),
        QueueCommand::Edit {
            target,
            index: n,
            stdin,
            text,
        } => (target, vec![Edit::Replace(index(n)?, input(stdin, text)?)]),
        QueueCommand::Remove { target, index: n } => (target, vec![Edit::Remove(index(n)?)]),
        QueueCommand::Move { target, from, to } => {
            (target, vec![Edit::Move(index(from)?, index(to)?)])
        }
    };
    let batch = queue::edit_many(&queue::target(&selector)?, queue::Revision::Any, changes)?;
    println!(
        "{}: {} message(s), {}{}",
        batch.id,
        batch.messages.len(),
        batch.status.label(),
        batch
            .after
            .map(|at| format!(", {}", queue::format_time(at)))
            .unwrap_or_default()
    );
    Ok(())
}
fn queue_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show help.
    #[command(visible_alias = "h")]
    Help,

    /// Run a command in a new pmux-managed process.
    #[command(visible_alias = "r")]
    Run {
        /// Optional working directory override.
        #[arg(long)]
        cwd: Option<String>,
        /// Create the session without attaching. Prints the session name to stdout.
        #[arg(long, short = 'd')]
        detach: bool,
        /// Explicit session name. Must appear before the command.
        #[arg(long, short = 's', value_name = "NAME")]
        session_name: Option<String>,
        #[command(flatten)]
        lineage: ParentArgs,
        /// Command and arguments. Options after the command are passed through unchanged.
        #[arg(action = ArgAction::Append, trailing_var_arg = true, required = true)]
        parts: Vec<String>,
    },

    /// Attach to a live process or use the picker to resume agent history.
    #[command(visible_alias = "a")]
    Attach {
        /// Process name, special selectors newest/oldest, or a past agent session id (see `pmux history`).
        target: Option<String>,
    },

    /// Switch to the previous pmux process for this pane, or attach the most recently connected one.
    #[command(visible_alias = "l")]
    Last,

    /// Cycle to the next process by creation order (wrap-around).
    /// Under the hood, each "process" is a tmux session on the pmux socket.
    #[command(name = "next-process", hide = true)]
    NextProcess {
        current: Option<String>,
        client: Option<String>,
    },

    /// Cycle to the previous process by creation order (wrap-around).
    /// Under the hood, each "process" is a tmux session on the pmux socket.
    #[command(name = "prev-process", hide = true)]
    PrevProcess {
        current: Option<String>,
        client: Option<String>,
    },

    /// Create a shell wrapper so command runs through pmux.
    #[command(visible_alias = "w")]
    Wrap {
        /// Command names to wrap (example: claude codex).
        #[arg(required = true, value_name = "COMMAND")]
        commands: Vec<String>,
    },

    /// Remove a shell wrapper created by wrap.
    Unwrap {
        /// Command names to unwrap.
        #[arg(required = true, value_name = "COMMAND")]
        commands: Vec<String>,
    },

    /// List currently wrapped commands.
    Wraps,

    /// Reload pmux's tmux config; every process keeps running. --repair
    /// first undoes a tmux config loaded into pmux's server by mistake;
    /// --hard kills the server and every process.
    Restart {
        /// Hard restart: kill the pmux tmux server and all pmux processes.
        #[arg(long, conflicts_with = "repair")]
        hard: bool,
        /// Reset pmux's server to tmux's defaults before reloading pmux's
        /// config, undoing a tmux config loaded into it by mistake.
        #[arg(long)]
        repair: bool,
        /// Skip the --hard confirmation prompt (required without a terminal).
        #[arg(long, short = 'y', requires = "hard")]
        yes: bool,
    },

    /// Restart agents in their own panes, resuming each conversation, so they
    /// pick up the server's current environment and config.
    #[command(group(
        clap::ArgGroup::new("respawn_target")
            .required(true)
            .args(["names", "agent", "all"])
    ))]
    Respawn {
        /// Process names (or newest/oldest).
        names: Vec<String>,
        /// Every process running this agent (claude, codex, cortex, opencode, muse, pi).
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Every pmux process; processes that are not agents are skipped.
        #[arg(long)]
        all: bool,
        /// Print what would be respawned, with each resume id, and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Also interrupt busy agents, and start agents with no resume id fresh.
        #[arg(long)]
        force: bool,
        /// Skip the confirmation when respawning several processes (required without a terminal).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Resume this session id instead of the one pmux finds (one process only).
        #[arg(long, value_name = "ID")]
        resume_id: Option<String>,
        /// Arguments after `--` are appended to the relaunch command, e.g. new
        /// agent flags (one process only).
        #[arg(last = true, value_name = "ARGS")]
        extra_args: Vec<String>,
    },

    /// Kill a running pmux process, or the processes this one spawned.
    #[command(visible_alias = "k")]
    #[command(group(
        clap::ArgGroup::new("kill_target")
            .required(true)
            .args(["target", "children", "descendants"])
    ))]
    Kill {
        /// Process name, or special selectors: newest/oldest.
        target: Option<String>,
        /// Kill every process this pane's pmux process spawned directly.
        #[arg(long, conflicts_with = "descendants")]
        children: bool,
        /// Kill every process below this pane's pmux process, at any depth.
        #[arg(long)]
        descendants: bool,
    },

    /// List running pmux processes.
    #[command(group(
        clap::ArgGroup::new("spawned_filter").args(["children", "descendants"])
    ))]
    Ls {
        /// Emit the stable machine-readable state snapshot as JSON.
        #[arg(long)]
        json: bool,
        /// Omit start_command and shorten launch_command in JSON.
        #[arg(long, requires = "json")]
        compact: bool,
        /// Only processes this pane's pmux process (or --of) spawned directly.
        #[arg(long, conflicts_with = "descendants")]
        children: bool,
        /// Only processes below this pane's pmux process (or --of), at any depth.
        #[arg(long)]
        descendants: bool,
        /// The parent for --children/--descendants instead of this pane's process.
        #[arg(long, value_name = "NAME", requires = "spawned_filter")]
        of: Option<String>,
        /// Only processes whose current directory is this directory or below it.
        #[arg(long, value_name = "DIR", conflicts_with = "here")]
        cwd: Option<String>,
        /// Only processes whose current directory is the current directory or below it.
        #[arg(long)]
        here: bool,
    },

    /// List past agent sessions (Claude Code, Codex) that can be resumed.
    History {
        /// Emit the catalog as JSON.
        #[arg(long)]
        json: bool,
        /// Include sessions that are running right now.
        #[arg(long)]
        all: bool,
        /// Only sessions from this agent (claude, codex, cortex).
        #[arg(long)]
        agent: Option<String>,
        /// Only sessions that ran in this directory or below it.
        #[arg(long, value_name = "DIR", conflicts_with = "here")]
        cwd: Option<String>,
        /// Only sessions that ran in the current directory or below it.
        #[arg(long)]
        here: bool,
        /// Maximum sessions to list (default: 30 in the table, all with --json).
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Only sessions pmux can resume now (not running, resume command known, directory present).
        #[arg(long, conflicts_with = "all")]
        resumable: bool,
        /// Fuzzy-rank sessions against this query.
        /// With --json, each session gains a `match` with its score and matched positions.
        #[arg(long, short = 'q')]
        query: Option<String>,
    },

    /// Resume a past agent session in a new pmux process.
    Resume {
        /// Session id, unambiguous id prefix, or agent:id (see `pmux history`).
        target: String,
        /// Create the process without attaching. Prints its name to stdout.
        #[arg(long, short = 'd')]
        detach: bool,
        /// Explicit name for the new pmux process.
        #[arg(long, short = 's', value_name = "NAME")]
        session_name: Option<String>,
        #[command(flatten)]
        lineage: ParentArgs,
    },

    /// Show, set or clear the process that spawned a process.
    Parent {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Record this process as the parent (must be running, unless remote).
        #[arg(long, value_name = "PARENT", conflicts_with = "clear")]
        set: Option<String>,
        /// The remote parent's machine; needs --set and --url.
        #[arg(long, value_name = "MACHINE", requires_all = ["set", "url"], hide = true)]
        machine: Option<String>,
        /// The remote parent's base URL; needs --set and --machine.
        #[arg(long, value_name = "URL", requires_all = ["set", "machine"], hide = true)]
        url: Option<String>,
        /// Forget the recorded parent.
        #[arg(long)]
        clear: bool,
        /// Print the reported parent (or null) as JSON.
        #[arg(long, conflicts_with_all = ["set", "clear"])]
        json: bool,
    },

    /// Inspect one pmux process by name, newest, or oldest.
    #[command(visible_alias = "i")]
    Inspect {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Emit machine-readable process details as JSON.
        #[arg(long)]
        json: bool,
        /// Show the entire launch command in human output (default: 120 columns).
        #[arg(long, conflicts_with = "json")]
        full_command: bool,
        /// Omit start_command and shorten launch_command in JSON.
        #[arg(long, requires = "json")]
        compact: bool,
    },

    /// Capture a process's current screen as plain text, including prompt boxes.
    Capture {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Last N physical rows, including saved scrollback when available.
        #[arg(long, short = 'n', conflicts_with = "history", value_parser = clap::value_parser!(u32).range(1..))]
        lines: Option<u32>,
        /// Include all saved scrollback before the current screen.
        #[arg(long)]
        history: bool,
        /// Emit api_version, session, pane_id, and text as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Print the last assistant text message from Claude Code or Codex.
    Reply {
        target: String,
        #[arg(long)]
        json: bool,
    },

    /// Show an agent process's transcript activity: tokens, churn, prompts.
    Activity {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Emit the activity summary and graph buckets as JSON.
        #[arg(long)]
        json: bool,
        /// How many buckets each emitted activity series is resampled to.
        #[arg(long, default_value_t = 48)]
        buckets: usize,
    },

    /// Wait for a process to exit or for an explicit agent state.
    Wait {
        /// Process names, or special selectors: newest/oldest. Several need --all or --any.
        #[arg(required = true, value_name = "TARGET")]
        targets: Vec<String>,
        /// Return once every process matches, or as soon as one does not
        /// (blocked, not_ready, exited, timed_out).
        #[arg(long, conflicts_with = "any")]
        all: bool,
        /// Return as soon as any process matches or needs attention, naming it.
        #[arg(long)]
        any: bool,
        /// `exited` (default) or `agent:<state>`; repeat or comma-separate for any match.
        #[arg(long, default_value = "exited", value_delimiter = ',')]
        until: Vec<String>,
        /// Keep waiting through input-needed signals unless explicitly requested.
        #[arg(long)]
        ignore_waiting: bool,
        /// Time allowed since agent launch without a current signal before
        /// not_ready. One process only: with --all/--any an unsignalled agent
        /// is watched until --timeout.
        #[arg(long, default_value = "5s", value_parser = parse_duration)]
        startup_timeout: Duration,
        /// Maximum wait time, such as 500ms, 30s, 5m, or 1h.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
        /// Include the last N screen rows when blocked (bare flag: 80).
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "80", value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        capture_on_block: Option<u32>,
        /// Emit the observed outcome as JSON.
        #[arg(long)]
        json: bool,
        /// Omit start_command and shorten launch_command in JSON.
        #[arg(long, requires = "json")]
        compact: bool,
    },

    /// Send one prompt, wait for agent:done, and print the transcript reply.
    Ask {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Read the literal prompt from standard input.
        #[arg(long, conflicts_with_all = ["text", "prompt_file"])]
        stdin: bool,
        /// Read UTF-8 prompt text without exposing its contents in argv.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["text", "stdin"])]
        prompt_file: Option<std::path::PathBuf>,
        /// Time allowed since agent launch without a current signal before not_ready.
        #[arg(long, default_value = "5s", value_parser = parse_duration)]
        startup_timeout: Duration,
        /// Maximum wait time after sending, such as 500ms, 30s, 5m, or 1h.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
        /// Include the last N screen rows when blocked (bare flag: 80).
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "80", value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        capture_on_block: Option<u32>,
        /// Emit the wait outcome, send confirmation, and optional reply as JSON.
        #[arg(long)]
        json: bool,
        /// Omit start_command and shorten launch_command in JSON.
        #[arg(long, requires = "json")]
        compact: bool,
        /// Literal prompt; options may follow it. Use -- before hyphen-leading text.
        #[arg(required_unless_present_any = ["stdin", "prompt_file"])]
        text: Vec<String>,
    },

    /// Stream JSON Lines state snapshots: the current one, then a new one
    /// whenever the state changes. `ls --json` prints a single snapshot.
    Events {
        /// Accepted for compatibility; streaming is the default.
        #[arg(long, hide = true)]
        follow: bool,
        /// How often to check for changes, such as 250ms or 1s.
        #[arg(long, default_value = "1s", value_parser = parse_duration)]
        interval: Duration,
    },

    /// Show a live process dashboard (normally opened by the outer tmux binding).
    #[command(hide = true)]
    Sidebar {
        /// Outer tmux server socket supplied by the sidebar binding.
        #[arg(long, hide = true)]
        socket: Option<String>,
        /// Outer tmux pane that opened the sidebar.
        #[arg(long, hide = true)]
        host_pane: Option<String>,
        /// This renderer owns a pmux-specific pane border label.
        #[arg(long, hide = true)]
        pane_label: bool,
    },

    /// Schedule a batch of messages for a pmux process.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Open the message queue popup.
    #[command(hide = true)]
    QueueUi { target: String },

    /// Send literal input or named keys to an interactive pmux process.
    Send {
        /// Process name, or special selectors: newest/oldest.
        target: String,
        /// Read the literal input from standard input instead of command arguments.
        #[arg(long, conflicts_with_all = ["text", "key", "prompt_file"])]
        stdin: bool,
        /// Read UTF-8 prompt text from a file without exposing its contents in argv.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["text", "stdin", "key", "repeat"])]
        prompt_file: Option<std::path::PathBuf>,
        /// Do not append an Enter key after the literal input.
        #[arg(long)]
        no_enter: bool,
        /// Send a named key (Escape, C-u, BSpace, etc.) or one character such as y; repeat for a sequence.
        #[arg(long, conflicts_with_all = ["text", "stdin", "no_enter", "prompt_file"])]
        key: Vec<String>,
        /// Repeat the entire named-key sequence (1..=1000), without implicit Enter.
        #[arg(long, requires = "key", conflicts_with_all = ["text", "stdin", "no_enter", "prompt_file"], value_parser = clap::value_parser!(u16).range(1..=1000))]
        repeat: Option<u16>,
        /// Wait until the agent takes the prompt up (a fresh working or done
        /// signal), for at most TIMEOUT (bare flag: 10s). Exits like `wait`.
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "10s", value_name = "TIMEOUT", value_parser = parse_duration, conflicts_with_all = ["key", "no_enter"])]
        confirm: Option<Duration>,
        /// With --confirm: include the last N screen rows when blocked or not_ready (bare flag: 80).
        #[arg(long, requires = "confirm", num_args = 0..=1, require_equals = true, default_missing_value = "80", value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        capture_on_block: Option<u32>,
        /// Emit the send confirmation as JSON.
        #[arg(long)]
        json: bool,
        /// With --confirm --json: omit start_command and shorten launch_command.
        #[arg(long, requires_all = ["json", "confirm"])]
        compact: bool,
        /// Literal text to send. Put options before this text; use --stdin for secrets.
        #[arg(
            required_unless_present_any = ["stdin", "key", "prompt_file"],
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        text: Vec<String>,
    },

    /// Generate shell completions.
    #[command(hide = true)]
    Completions {
        /// Shell to generate completions for (bash, zsh, fish).
        shell: clap_complete::Shell,
    },

    /// Set up everything in one go: shell integration, ai-attn and Dredge,
    /// agent hooks, wrappers for your agents, and your tmux. Safe to rerun;
    /// it only does what is missing.
    Setup {
        /// Accept every default without asking.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Leave out a step (repeatable, or comma-separated).
        #[arg(long, value_enum, value_delimiter = ',')]
        skip: Vec<crate::onboard::Step>,
        /// Also install the boot service so pmux restores sessions after a reboot.
        #[arg(long)]
        autostart: bool,
        /// How the sidebar and picker show processes (see `pmux preset`).
        #[arg(long)]
        preset: Option<String>,
    },

    /// Set up shell completions and wrapper sourcing in your shell rc.
    Init {
        /// Also install the boot service so pmux restores sessions after a reboot.
        #[arg(long)]
        autostart: bool,
        /// Configure pmux's tmux server with tmux.rb: create
        /// ~/.config/pmux/tmux.conf.rb and compile it into tmux.conf.
        #[arg(long = "tmux-rb")]
        tmux_rb: bool,
        /// How the sidebar and picker show processes (see `pmux preset`).
        /// Without it, a first `pmux init` at a terminal asks.
        #[arg(long)]
        preset: Option<String>,
        /// Install guidance for AI agents: a Claude Code skill and a section in
        /// Codex/Gemini AGENTS.md. Without either flag, a first `pmux init` at a
        /// terminal asks, and later runs keep the answer.
        #[arg(long, conflicts_with = "no_agent_docs")]
        agent_docs: bool,
        /// Remove pmux's agent guidance and stop installing it.
        #[arg(long)]
        no_agent_docs: bool,
    },

    /// List the row presets, or switch the sidebar and picker to one.
    Preset {
        /// The preset to use: pmux, compact, or herdr.
        name: Option<String>,
    },

    /// Snapshot the current pmux sessions for cross-restart restoration.
    /// Run before a planned reboot to guarantee a fresh capture.
    Snapshot,

    /// Restore pmux sessions from the last snapshot (recreate panes and resume agents).
    Restore {
        /// Invoked from the boot service; adjusts output only.
        #[arg(long, hide = true)]
        boot: bool,
    },

    /// Run diagnostics.
    Doctor {
        #[command(subcommand)]
        target: DoctorTarget,
    },

    /// Internal command run by tmux hooks: publish each process's sidebar
    /// label and tag color as session options for the status bar.
    #[command(hide = true, name = "status-labels")]
    StatusLabels,

    /// Print the socket of your own tmux server that is showing this pmux
    /// process, for running tmux commands there from inside pmux:
    /// `tmux -S "$(pmux host-socket)" ...`.
    #[command(name = "host-socket")]
    HostSocket {
        /// The pmux process to look up (default: the one this runs in).
        #[arg(long)]
        session: Option<String>,
    },

    /// Internal command behind the `prefix ?` popup: list the keys the pmux
    /// server has configured, leaving out untouched tmux defaults.
    #[command(hide = true, name = "key-hints")]
    KeyHints {
        /// Color keys and headings with ANSI escapes.
        #[arg(long)]
        color: bool,
    },

    /// Internal command used by pmux to relay ai-attn state to parent tmux panes.
    #[command(hide = true, name = "relay-ai-attn")]
    RelayAiAttn {
        #[arg(long = "loop")]
        loop_forever: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short = 'v')]
        verbose: bool,
        #[arg(long)]
        restart: bool,
    },

    /// Internal command used by the `prefix + y` binding: copy text to the
    /// clipboard of the user at the pmux client, through every route that
    /// can reach it (pmux's tmux, the outer tmux, a local clipboard tool).
    #[command(hide = true, name = "copy-to-clipboard")]
    CopyToClipboard {
        /// The pmux client (tmux's `client_name`) the key was pressed in.
        #[arg(long)]
        client: String,
        /// The pmux process that client is showing.
        #[arg(long)]
        session: String,
        text: String,
    },

    /// Internal command used by pmux tmux bindings to kill the current process and switch away.
    #[command(hide = true, name = "kill-current-and-next")]
    KillCurrentAndNext {
        current: Option<String>,
        client: Option<String>,
    },

    /// Update the host-pane mapping for a pmux session visible in an outer tmux pane.
    #[command(hide = true, name = "update-host-pane")]
    UpdateHostPane {
        /// Outer tmux pane ID (e.g. %69).
        pane_id: Option<String>,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: Option<String>,
        /// Refresh every pane on the supplied outer tmux socket.
        #[arg(long)]
        all: bool,
    },

    /// Internal command used by the outer tmux binding to toggle its sidebar pane.
    #[command(hide = true, name = "toggle-sidebar")]
    ToggleSidebar {
        /// Outer tmux pane where the toggle was invoked.
        pane_id: String,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: String,
    },

    /// Internal command used by the outer `pane-focus-in` hook: refresh the
    /// relay host-pane mapping and nudge the focused window's sidebar.
    #[command(hide = true, name = "on-focus")]
    OnFocus {
        /// Outer tmux pane that gained focus.
        pane_id: String,
        /// Outer tmux window containing that pane.
        window_id: String,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: String,
    },

    /// Emit (or apply) the outer-tmux integration pmux needs. The generated
    /// ~/.config/pmux/outer-tmux.conf runs this; source that file from your
    /// tmux config instead of calling it directly.
    #[command(hide = true, name = "tmux-init")]
    TmuxInit {
        /// Apply the integration directly to a running outer tmux server instead
        /// of printing it. Requires --socket.
        #[arg(long)]
        apply: bool,
        /// Socket path for the outer tmux server (required with --apply).
        #[arg(long)]
        socket: Option<String>,
    },

    /// Even out the panes in a window of your tmux, keeping an open pmux
    /// sidebar's width. Bind it in place of select-layout.
    #[command(name = "equalize-panes")]
    EqualizePanes {
        /// Outer tmux pane where the layout binding was invoked.
        pane_id: String,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: String,
    },

    /// Internal command used by the outer tmux resize hook.
    #[command(hide = true, name = "sidebar-width")]
    SidebarWidth {
        /// Outer tmux pane that was resized.
        pane_id: String,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: String,
    },

    /// Internal command used after an outer pane closes near the sidebar.
    #[command(hide = true, name = "sidebar-stabilize")]
    SidebarStabilize {
        /// Outer tmux window where a pane closed.
        window_id: String,
        /// Socket path for the outer tmux server.
        #[arg(long)]
        socket: String,
    },

    /// Rename a pmux process.
    #[command(name = "rename", alias = "rename-session")]
    RenameSession {
        /// Current session name.
        old_name: String,
        /// New session name.
        new_name: String,
    },

    /// Interactive process actions shown from the sidebar.
    #[command(hide = true, name = "session-actions")]
    SessionActions {
        session_name: String,
        #[arg(long)]
        socket: String,
        #[arg(long)]
        host_pane: String,
        #[arg(long, hide = true)]
        mode: Option<String>,
        #[arg(long, hide = true)]
        mouse_opened: bool,
    },

    /// Run one item of a native sidebar context menu.
    ///
    /// Menu commands carry only an opaque token and an item index; the
    /// item's real arguments are read back from a state file so no process
    /// name, path, or typed text is ever spliced into a tmux command.
    #[command(hide = true, name = "sidebar-menu-item")]
    SidebarMenuItem { token: String, item: usize },

    /// Ask in pmux's floating prompt, then run a command with the answer.
    ///
    /// Run it in a tmux popup, as pmux's own bindings do:
    /// `display-popup -w 60 -h 9 -E 'pmux prompt --title "Branch" -- git switch -c'`.
    /// An argument that is exactly `{}` is replaced by the answer, which is
    /// otherwise appended. When the command fails, its last line of error
    /// output is shown and the prompt stays open.
    Prompt {
        /// Heading at the top of the prompt.
        #[arg(long)]
        title: String,
        /// Label of the button that runs the command.
        #[arg(long, default_value = "OK")]
        action: String,
        /// Text the input starts with.
        #[arg(long, default_value = "")]
        value: String,
        /// Ask a yes/no question with this text instead of reading input;
        /// the command then gets no answer.
        #[arg(long, value_name = "TEXT")]
        confirm: Option<String>,
        /// Show the action in red, for one that destroys something.
        #[arg(long)]
        danger: bool,
        /// Split the answer into words, as a shell would, passing each as an
        /// argument.
        #[arg(long, conflicts_with = "confirm")]
        split: bool,
        /// The command to run.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Interactive rename form used by the pmux `prefix + ,` binding.
    ///
    /// Bindings pass `--pane #{pane_id}`: a popup has no client context of
    /// its own, so "the current session" is ambiguous with several clients
    /// attached, and pane ids never need shell quoting. Without either it
    /// falls back to the session of the client running it.
    #[command(hide = true, name = "rename-session-ui")]
    RenameSessionUi {
        session_name: Option<String>,
        /// Rename the session that owns this pane.
        #[arg(long, conflicts_with = "session_name")]
        pane: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DoctorTarget {
    /// Diagnose the pmux ai-attn relay, or why one process is or isn't relayed.
    #[command(name = "ai-attn")]
    AiAttn {
        /// Show relay matching details for this process instead.
        process: Option<String>,
    },

    /// Diagnose the cross-restart restore setup.
    Restore,

    /// Check that pmux's tmux server and your own tmux are each configured
    /// as themselves: no config loaded into the wrong one, no shared prefix,
    /// no plugin loaded twice.
    Tmux {
        /// Also check the tmux server at this socket path.
        #[arg(long)]
        socket: Option<String>,
    },
}

// Build the human-readable help text, substituting the binary name into each example.
// Sections run from what people use daily to what scripts and agents use, so
// the commands most readers need come first.
fn build_help_text(binary: &str) -> String {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Every day",
            &[
                (
                    "wrap <cmd>...",
                    "Make each command start inside pmux from now on",
                ),
                (
                    "attach [name]",
                    "Attach from the picker, or to one process (shortcut: a)",
                ),
                (
                    "last",
                    "Reattach the process this pane showed last (l; Ctrl-p l inside)",
                ),
            ],
        ),
        (
            "Occasionally",
            &[
                (
                    "run <cmd> [args...]",
                    "Start a one-off process (-s name, --cwd dir, -d detached)",
                ),
                (
                    "restart",
                    "Reload pmux's tmux config; processes keep running",
                ),
                ("snapshot", "Capture processes now, before a planned reboot"),
                (
                    "restore",
                    "Recreate processes and resume agents after a reboot",
                ),
                (
                    "respawn <name>...",
                    "Restart agents in place, resuming them (--agent, --dry-run)",
                ),
                ("rename <old> <new>", "Rename a process (Ctrl-p , inside)"),
                ("kill <name>", "Stop a process for good (k)"),
                ("unwrap <cmd>...", "Remove wrappers (`wraps` lists them)"),
            ],
        ),
        (
            "Scripts and agents",
            &[
                ("ls [--json]", "Every running process, with agent state"),
                ("inspect <name>", "Details for one process (i)"),
                (
                    "parent <name>",
                    "Who spawned a process (--set/--clear to fix it)",
                ),
                (
                    "capture <name>",
                    "Read the screen, including approval prompts",
                ),
                ("send <name> <text>", "Send literal input and Enter"),
                (
                    "ask <name> <text>",
                    "Send a prompt, wait for done, and read the reply",
                ),
                (
                    "reply <name>",
                    "Read the last Claude Code or Codex assistant message",
                ),
                (
                    "wait <name>",
                    "Block until a process exits or reaches an agent state",
                ),
                ("events", "Stream JSON Lines state snapshots as they change"),
                (
                    "history",
                    "Past Claude Code and Codex sessions (`resume <id>` reopens one)",
                ),
                (
                    "activity <name>",
                    "An agent's token, churn, and prompt totals",
                ),
                (
                    "queue open <name>",
                    "Schedule a combined message batch (prefix + q)",
                ),
                (
                    "host-socket",
                    "Socket of your tmux showing this process, for `tmux -S`",
                ),
                (
                    "prompt -- <command>",
                    "Ask in a floating prompt, then run <command> with the answer",
                ),
            ],
        ),
        (
            "Setup and diagnostics",
            &[
                (
                    "setup [--yes]",
                    "Install and wire everything; safe to rerun",
                ),
                (
                    "init [--autostart]",
                    "Shell integration; --autostart restores at login",
                ),
                ("doctor restore", "Check the cross-restart restore setup"),
                (
                    "doctor tmux",
                    "Check pmux's tmux server and your tmux for mix-ups",
                ),
                (
                    "doctor ai-attn [name]",
                    "Check the ai-attn relay, or one process's relaying",
                ),
                (
                    "restart --repair",
                    "Undo a tmux config loaded into pmux's server by mistake",
                ),
                ("restart --hard", "Kill the pmux server and EVERY process"),
            ],
        ),
    ];

    let mut text = format!("{binary} - process multiplexer\n");
    for (title, commands) in SECTIONS {
        text.push_str(&format!("\n{title}:\n"));
        for (usage, summary) in *commands {
            let usage = format!("{binary} {usage}");
            text.push_str(&format!("  {usage:<28} {summary}\n"));
        }
    }
    text.push_str(
        "\nInside a process, the prefix is Ctrl-p: d detaches, s switches, ? lists every key.\n",
    );
    text.push_str("Shortcuts: a=attach  h=help  i=inspect  k=kill  l=last  r=run  w=wrap\n");
    text.push_str(&format!(
        "Run `{binary} <command> --help` for a command's options.\n"
    ));
    text
}

// Parse a concise duration for the automation-oriented wait/events commands.
// Bare values are seconds; accepted suffixes are ms, s, m, and h.
fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("duration must not be empty".to_string());
    }
    let (number, multiplier) = if let Some(number) = raw.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = raw.strip_suffix('s') {
        (number, 1_000_u64)
    } else if let Some(number) = raw.strip_suffix('m') {
        (number, 60_000_u64)
    } else if let Some(number) = raw.strip_suffix('h') {
        (number, 3_600_000_u64)
    } else {
        (raw, 1_000_u64)
    };
    let value = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration '{raw}'; use 500ms, 30s, 5m, or 1h"))?;
    let millis = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration '{raw}' is too large"))?;
    Ok(Duration::from_millis(millis))
}

fn parse_wait_condition(raw: &str) -> Result<crate::WaitCondition> {
    if raw.eq_ignore_ascii_case("exited") {
        return Ok(crate::WaitCondition::Exited);
    }
    let Some(state) = raw.strip_prefix("agent:") else {
        bail!("--until must be 'exited' or 'agent:<ai-attn-state>'");
    };
    if state.trim().is_empty() {
        bail!("agent wait state must not be empty");
    }
    Ok(crate::WaitCondition::AgentState(state.to_string()))
}

fn read_prompt(path: Option<std::path::PathBuf>, stdin: bool, text: Vec<String>) -> Result<String> {
    if let Some(path) = path {
        std::fs::read_to_string(&path)
            .map_err(|err| anyhow!("failed to read prompt file {}: {err}", path.display()))
    } else if stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        Ok(input)
    } else {
        Ok(text.join(" "))
    }
}

fn print_reply(reply: &crate::commands::ReplyResult) -> Result<()> {
    if let Some(text) = &reply.text {
        println!(
            "{}",
            text.split('\n')
                .map(crate::util::sanitize_terminal_text)
                .collect::<Vec<_>>()
                .join("\n")
        );
        Ok(())
    } else {
        bail!(
            "{}",
            reply
                .reason
                .as_deref()
                .unwrap_or("no assistant reply available")
        )
    }
}

fn print_block_capture(result: &crate::commands::WaitResult, json: bool) {
    if !json && let Some(capture) = &result.capture {
        print!("{}", capture.text);
    }
    if let Some(error) = &result.capture_error {
        eprintln!(
            "pmux: blocked screen capture unavailable: {}",
            crate::util::sanitize_terminal_text(error)
        );
    }
}

fn print_wait_status(result: &crate::commands::WaitResult) {
    let session = crate::util::sanitize_terminal_text(&result.session);
    if let Some(condition) = &result.matched_condition {
        println!("{session}: {} ({condition})", result.outcome);
    } else {
        println!(
            "{session}: {} while waiting for {}",
            result.outcome, result.condition
        );
    }
}

fn finish_wait(result: &crate::commands::WaitResult, read_reply_after: bool) -> Result<()> {
    match result.outcome.as_str() {
        "matched" => Ok(()),
        "not_ready" => {
            let startup = result.process.as_ref().and_then(|p| p.startup.as_ref());
            let capture = startup
                .map(|s| s.capture_command.as_str())
                .unwrap_or("pmux capture");
            let why = if startup.is_some_and(|s| s.ready_text_missing()) {
                "agent's ready text is not on screen, likely a startup or approval dialog"
            } else {
                "agent readiness unconfirmed"
            };
            eprintln!(
                "pmux: {why}; check startup/approval screens with {}",
                crate::util::sanitize_terminal_text(capture)
            );
            io::stdout().flush()?;
            std::process::exit(3);
        }
        "blocked" => {
            let agent = result.process.as_ref().and_then(|p| p.agent.as_ref());
            let state = agent.and_then(|a| a.state.as_deref()).unwrap_or("waiting");
            let reason = agent
                .and_then(|a| a.reason.as_deref())
                .unwrap_or("input needed");
            let command = |args: &[&str]| {
                crate::util::shell_join(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
            };
            let resume = command(&[
                "pmux",
                "wait",
                &result.session,
                "--until",
                &result.condition,
            ]);
            let next = if read_reply_after {
                format!(
                    "{resume}, then read {} (do not repeat ask)",
                    command(&["pmux", "reply", &result.session])
                )
            } else {
                resume
            };
            eprintln!(
                "pmux: '{}' blocked: {} ({}); inspect with {}, answer with {}, then resume {}",
                crate::util::sanitize_terminal_text(&result.session),
                crate::util::sanitize_terminal_text(state),
                crate::util::sanitize_terminal_text(reason),
                crate::util::sanitize_terminal_text(&command(&[
                    "pmux",
                    "capture",
                    &result.session
                ])),
                crate::util::sanitize_terminal_text(&command(&["pmux", "send", &result.session])),
                crate::util::sanitize_terminal_text(&next)
            );
            io::stdout().flush()?;
            std::process::exit(2);
        }
        "exited" => bail!(
            "pmux process '{}' exited before reaching {}; inspect history/logs before starting a new process",
            crate::util::sanitize_terminal_text(&result.session),
            result.condition
        ),
        "timed_out" => bail!(
            "timed out waiting for pmux process '{}' to reach {}; inspect/capture it, then resume pmux wait (do not resend the prompt)",
            crate::util::sanitize_terminal_text(&result.session),
            result.condition
        ),
        unexpected => bail!("unexpected pmux wait outcome '{unexpected}'"),
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// `inspect --json`: one process plus the API
/// version every top-level JSON object carries.
#[derive(Serialize)]
struct VersionedProcess<'a> {
    api_version: u32,
    #[serde(flatten)]
    process: &'a crate::ProcessInfo,
}

fn print_process_json(process: &crate::ProcessInfo, compact: bool) -> Result<()> {
    print_command_json(
        &VersionedProcess {
            api_version: crate::API_VERSION,
            process,
        },
        compact,
    )
}

/// The compact projection is opt-in so existing API v1 clients keep every
/// field. Process objects lose the low-level tmux command and receive a bounded
/// launch command preview, marked explicitly when truncated.
fn command_json<T: Serialize>(value: &T, compact: bool) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(value)?;
    if compact {
        fn omit_start_command(process: &mut serde_json::Value) {
            if let Some(object) = process.as_object_mut() {
                object.remove("start_command");
                if let Some(command) = object.get("launch_command").and_then(|v| v.as_str())
                    && command.chars().count() > 120
                {
                    let preview: String = command.chars().take(119).chain(['…']).collect();
                    object.insert("launch_command".into(), preview.into());
                    object.insert("launch_command_truncated".into(), true.into());
                }
            }
        }
        omit_start_command(&mut value);
        if let Some(process) = value.get_mut("process") {
            omit_start_command(process);
        }
        if let Some(sessions) = value.get_mut("sessions").and_then(|v| v.as_array_mut()) {
            for process in sessions {
                omit_start_command(process);
            }
        }
        if let Some(results) = value.get_mut("results").and_then(|v| v.as_array_mut()) {
            for result in results {
                if let Some(process) = result.get_mut("process") {
                    omit_start_command(process);
                }
            }
        }
    }
    Ok(value)
}

fn print_command_json<T: Serialize>(value: &T, compact: bool) -> Result<()> {
    if compact {
        print_json(&command_json(value, true)?)
    } else {
        print_json(value)
    }
}

/// `activity --json`: the sidebar details drawer's data for one process.
///
/// `available` is false for a process with no parseable agent transcript (a
/// shell, or an agent whose transcript is gone); every count is then zero.
/// Series are resampled onto the sidebar's default warped time axis, so a
/// client can draw the same graph without the raw per-event data.
#[derive(Serialize)]
struct ActivityReport {
    api_version: u32,
    name: String,
    available: bool,
    prompts: u32,
    tools: u32,
    input_tokens: u64,
    output_tokens: u64,
    lines_added: u64,
    lines_deleted: u64,
    first_at: Option<i64>,
    last_at: Option<i64>,
    recent_prompts: Vec<String>,
    churn_series: Vec<f64>,
    output_token_series: Vec<f64>,
}

impl ActivityReport {
    fn load(name: &str, buckets: usize) -> Self {
        let data = match crate::load_activity(name) {
            crate::DrawerState::Ready(data) => Some(*data),
            _ => None,
        };
        Self::from_data(name, data, buckets)
    }

    /// Build the report from already-parsed activity (`None`: no transcript).
    fn from_data(name: &str, data: Option<crate::ActivityData>, buckets: usize) -> Self {
        let available = data.is_some();
        let data = data.unwrap_or_default();
        let mode = crate::AXIS_PRESETS[crate::DEFAULT_AXIS_INDEX].1;
        let buckets = buckets.max(1);
        let series = |metric| {
            let points = data.series(metric);
            if points.is_empty() {
                Vec::new()
            } else {
                crate::warp_buckets(points, buckets, mode)
            }
        };
        Self {
            api_version: crate::API_VERSION,
            name: name.to_string(),
            available,
            churn_series: series(crate::DrawerMetric::Churn),
            output_token_series: series(crate::DrawerMetric::OutputTokens),
            prompts: data.prompts,
            tools: data.tools,
            input_tokens: data.total_in,
            output_tokens: data.total_out,
            lines_added: data.added,
            lines_deleted: data.deleted,
            first_at: data.first,
            last_at: data.last,
            recent_prompts: data.recent_prompts,
        }
    }
}

fn render_activity(report: &ActivityReport) -> String {
    if !report.available {
        return format!("{}: no agent transcript activity\n", report.name);
    }
    let mut out = format!(
        "{}\n  prompts: {}  tools: {}\n  tokens: {} in / {} out\n  lines: +{} / -{}\n",
        report.name,
        report.prompts,
        report.tools,
        report.input_tokens,
        report.output_tokens,
        report.lines_added,
        report.lines_deleted,
    );
    for prompt in &report.recent_prompts {
        out.push_str(&format!("  > {prompt}\n"));
    }
    out
}

/// Sidebar menus run these commands through tmux `run-shell`, which shows a
/// failed command's stdout but discards its stderr, leaving only "... returned
/// 1". Repeat the error on stdout so the reason reaches the user.
fn show_error_under_run_shell(result: Result<()>) -> Result<()> {
    if let Err(err) = &result {
        println!("pmux: {err:#}");
    }
    result
}

fn print_json_line<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    io::stdout().flush()?;
    Ok(())
}

#[derive(Serialize)]
struct StateEvent<'a> {
    api_version: u32,
    event: &'static str,
    emitted_at: i64,
    state: &'a crate::StateSnapshot,
}

fn emit_state_event(state: &crate::StateSnapshot) -> Result<()> {
    print_json_line(&StateEvent {
        api_version: crate::API_VERSION,
        event: "state",
        emitted_at: state.generated_at,
        state,
    })
}

fn render_parent_report(report: &crate::ParentReport) -> String {
    let clean = crate::util::sanitize_terminal_text;
    match (&report.reported, &report.recorded) {
        (Some(parent), _) => format!("spawned by {}\n", clean(&parent.describe())),
        (None, Some(parent)) => format!(
            "no live parent (recorded parent {} has ended)\n",
            clean(&parent.describe())
        ),
        (None, None) => "no parent\n".to_string(),
    }
}

fn render_inspect(process: &crate::ProcessInfo, full_command: bool) -> String {
    // Values come from tmux, argv and the filesystem; none may reach the
    // terminal with escape sequences intact.
    let clean = crate::util::sanitize_terminal_text;
    let mut out = String::new();
    out.push_str(&format!("name: {}\n", clean(&process.name)));
    if let Some(parent) = &process.spawned_by {
        out.push_str(&format!("spawned by {}\n", clean(&parent.describe())));
    }
    out.push_str(&format!("lifecycle: {}\n", process.lifecycle));
    out.push_str(&format!("pane: {}\n", process.pane_id));
    out.push_str(&format!("clients: {}\n", process.attached_clients));
    out.push_str(&format!(
        "current command: {}\n",
        clean(&process.current_command)
    ));
    if let Some(path) = process.current_path.as_deref() {
        out.push_str(&format!(
            "current path: {}\n",
            clean(&crate::display_path(path))
        ));
    }
    if let Some(command) = process.launch_command.as_deref() {
        let command = clean(command);
        let displayed = if full_command {
            command.clone()
        } else {
            crate::util::truncate_col(&command, 120)
        };
        out.push_str(&format!("launch command: {displayed}\n"));
        if displayed != command {
            out.push_str("  (truncated; use --full-command to show all)\n");
        }
    }
    if let Some(path) = process.launch_cwd.as_deref() {
        out.push_str(&format!(
            "launch cwd: {}\n",
            clean(&crate::display_path(path))
        ));
    }
    if let Some(agent) = process.agent.as_ref() {
        out.push_str(&format!("agent: {}\n", clean(&agent.kind)));
        if let Some(id) = &agent.session_id {
            out.push_str(&format!("agent session: {}\n", clean(id)));
        }
        match (&agent.state, agent.updated_at) {
            (Some(state), Some(updated_at)) => {
                out.push_str(&format!(
                    "agent state: {}, {} ago\n",
                    clean(state),
                    crate::util::format_duration(
                        crate::util::now_epoch().saturating_sub(updated_at).max(0) as u64
                    )
                ));
            }
            _ => {
                if let Some(startup) = process.startup.as_ref().filter(|s| s.is_ready()) {
                    let age = crate::util::format_duration(
                        startup
                            .ready_at
                            .unwrap_or(startup.started_at)
                            .saturating_sub(startup.started_at)
                            .max(0) as u64,
                    );
                    out.push_str(&format!(
                        "agent state: ready (prompt seen {age} after launch; no agent signal yet)\n"
                    ));
                } else if let Some(startup) = &process.startup {
                    let age = crate::util::format_duration(
                        crate::util::now_epoch()
                            .saturating_sub(startup.started_at)
                            .max(0) as u64,
                    );
                    let why = if startup.ready_text_missing() {
                        "ready text not on screen; likely a startup or approval dialog"
                    } else {
                        "readiness unconfirmed"
                    };
                    out.push_str(&format!(
                        "agent state: {} ({age} since launch; {why})\n",
                        startup.status
                    ));
                    out.push_str(&format!(
                        "  May be at a startup or approval screen; run {}\n",
                        clean(&startup.capture_command)
                    ));
                } else {
                    out.push_str("agent state: readiness unconfirmed; use pmux capture to check startup prompts\n");
                }
            }
        }
    }
    out
}

/// Parse CLI arguments and dispatch to the appropriate pmux command.
/// Called from `main()` as the top-level entry point.
pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        None | Some(Commands::Help) => {
            print!("{}", build_help_text("pmux"));
            Ok(())
        }
        Some(Commands::Run {
            cwd,
            detach,
            session_name,
            lineage,
            parts,
        }) => {
            let parent = lineage.spec();
            let (command, args) = parse_run_parts(parts)?;
            if detach {
                let cwd_path = match cwd {
                    Some(dir) => std::path::PathBuf::from(dir),
                    None => std::env::current_dir()
                        .map_err(|e| anyhow!("failed to get current directory: {e}"))?,
                };
                let session = crate::spawn(command, args, session_name, &cwd_path, &parent)?;
                println!("{session}");
                Ok(())
            } else {
                match cwd {
                    Some(dir) => {
                        let path = std::path::PathBuf::from(dir);
                        crate::run_in_cwd(command, args, session_name, &path, &parent)
                    }
                    None => crate::run(command, args, session_name, &parent),
                }
            }
        }
        Some(Commands::Attach { target }) => match crate::attach(target) {
            Ok(()) => Ok(()),
            Err(err) if is_selection_cancelled(&err) => Ok(()),
            Err(err) => Err(err),
        },
        Some(Commands::Last) => crate::last(),
        Some(Commands::NextProcess { current, client }) => crate::next_from(current, client),
        Some(Commands::PrevProcess { current, client }) => crate::prev_from(current, client),
        Some(Commands::Wrap { commands }) => {
            for command in commands {
                let result = crate::wrap(&command)?;
                println!(
                    "Wrapped '{command}' → {}",
                    crate::display_path_buf(&result.wrap_path)
                );
                if result.rc_updated {
                    if let Some(rc) = result.shell_rc_path {
                        println!("Updated {}", crate::display_path_buf(&rc));
                    }
                    println!(
                        "Reload your shell to activate: source {}",
                        crate::display_path_buf(&result.wrap_path)
                    );
                }
            }
            Ok(())
        }
        Some(Commands::Wraps) => {
            let commands = crate::wraps()?;
            if commands.is_empty() {
                println!("No wrapped commands.");
            } else {
                for cmd in commands {
                    println!("{cmd}");
                }
            }
            Ok(())
        }
        Some(Commands::Unwrap { commands }) => {
            for command in commands {
                crate::unwrap(&command)?;
                println!("Unwrapped '{command}'.");
            }
            Ok(())
        }
        Some(Commands::Respawn {
            names,
            agent,
            all,
            dry_run,
            force,
            yes,
            resume_id,
            extra_args,
        }) => {
            crate::respawn(&crate::RespawnOptions {
                names,
                agent,
                all,
                dry_run,
                force,
                yes,
                resume_id,
                extra_args,
            })?;
            Ok(())
        }
        Some(Commands::Kill {
            target,
            children,
            descendants,
        }) => {
            let Some(target) = target else {
                let killed = crate::kill_spawned(descendants)?;
                if killed.is_empty() {
                    let which = if children { "child" } else { "descendant" };
                    println!("No {which} processes to kill.");
                }
                for name in killed {
                    println!("Killed pmux process '{name}'.");
                }
                return Ok(());
            };
            crate::kill(&target)?;
            println!("Killed pmux process '{target}'.");
            Ok(())
        }
        Some(Commands::Restart { repair: true, .. }) => {
            match crate::repair::repair()? {
                crate::repair::RepairResult::Repaired => println!(
                    "pmux's tmux server is back to tmux's defaults plus pmux's config; \
                     every process kept running."
                ),
                crate::repair::RepairResult::NoServer => {
                    println!("pmux's tmux server is not running; nothing to repair.")
                }
            }
            Ok(())
        }
        Some(Commands::Restart { hard, yes, .. }) => {
            if hard && !yes {
                let sessions = crate::list_sessions()?;
                if !sessions.is_empty() {
                    // Without a terminal the prompt either blocks forever on
                    // an open pipe or reads EOF and silently aborts while
                    // exiting 0, which scripts mistake for success.
                    if !io::stdin().is_terminal() {
                        anyhow::bail!(
                            "refusing to kill {} pmux process(es) without confirmation; \
                             rerun with --yes",
                            sessions.len()
                        );
                    }
                    eprint!(
                        "This will kill {} pmux process(es) and all their children. Continue? [y/N] ",
                        sessions.len()
                    );
                    io::stderr().flush()?;
                    let mut resp = String::new();
                    io::stdin().read_line(&mut resp)?;
                    if !resp.trim().eq_ignore_ascii_case("y") {
                        println!("Aborted.");
                        return Ok(());
                    }
                }
            }
            match crate::restart(hard)? {
                crate::RestartResult::HardRestarted => {
                    println!("Hard restart complete: pmux tmux server stopped.");
                }
                crate::RestartResult::SoftReloaded => {
                    println!("Soft restart complete: pmux tmux config reloaded.");
                }
                crate::RestartResult::SoftNoServer => {
                    println!("No pmux server running; nothing to reload.");
                }
            }

            Ok(())
        }
        Some(Commands::History {
            json,
            all,
            agent,
            cwd,
            here,
            limit,
            resumable,
            query,
        }) => {
            let within = if here {
                Some(std::env::current_dir()?)
            } else {
                cwd.map(|dir| std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.into()))
            };
            let limit = limit.or((!json).then_some(30));
            // Filtering and ranking come before the limit, so the catalog is
            // read whole when either is asked for.
            let history_query = crate::HistoryQuery {
                include_live: all,
                agent,
                within,
                limit: (!resumable && query.is_none()).then_some(limit).flatten(),
            };
            let mut entries = crate::history(&history_query)?;
            if resumable {
                entries.retain(crate::HistoryEntry::can_resume);
            }
            let Some(query) = query.filter(|q| !q.trim().is_empty()) else {
                entries.truncate(limit.unwrap_or(usize::MAX));
                if json {
                    return print_json(&entries);
                }
                print!("{}", render_history_table(&entries));
                return Ok(());
            };
            let mut ranked = crate::history_search::rank(entries, &query);
            ranked.truncate(limit.unwrap_or(usize::MAX));
            if json {
                return print_json(&ranked);
            }
            let entries: Vec<crate::HistoryEntry> =
                ranked.into_iter().map(|ranked| ranked.entry).collect();
            print!("{}", render_history_table(&entries));
            Ok(())
        }
        Some(Commands::Resume {
            target,
            detach,
            session_name,
            lineage,
        }) => {
            if let Some(session) = crate::resume(&target, session_name, detach, &lineage.spec())? {
                println!("{session}");
            }
            Ok(())
        }
        Some(Commands::Parent {
            target,
            set,
            machine,
            url,
            clear,
            json,
        }) => {
            let change = match (set, machine, url) {
                _ if clear => Some(crate::ParentSpec::None),
                (Some(process), Some(machine), Some(url)) => Some(crate::ParentSpec::Remote {
                    process,
                    machine,
                    url,
                }),
                (Some(name), _, _) => Some(crate::ParentSpec::Local(name)),
                _ => None,
            };
            let report = crate::parent(&target, change)?;
            if json {
                return print_json(&report.reported);
            }
            print!("{}", render_parent_report(&report));
            Ok(())
        }
        Some(Commands::Ls {
            json,
            compact,
            children,
            descendants,
            of,
            cwd,
            here,
        }) => {
            let within = if here {
                Some(std::env::current_dir()?)
            } else {
                cwd.map(|dir| std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.into()))
            };
            let in_dir = |path: Option<&str>| {
                within.as_deref().is_none_or(|root| {
                    path.is_some_and(|p| std::path::Path::new(p).starts_with(root))
                })
            };
            let sessions = crate::ls()?;
            let spawned = if children || descendants {
                Some(crate::spawned_names(&sessions, of.as_deref(), descendants)?)
            } else {
                None
            };
            let keep = |name: &String, path: Option<&str>| {
                spawned.as_ref().is_none_or(|names| names.contains(name)) && in_dir(path)
            };
            if json {
                // Built from every process so each `spawned_by` sees its
                // parent alive, then narrowed.
                let mut snapshot = crate::build_state_snapshot(&sessions);
                snapshot
                    .sessions
                    .retain(|process| keep(&process.name, process.current_path.as_deref()));
                return print_command_json(&snapshot, compact);
            }
            let shown: Vec<_> = sessions
                .iter()
                .filter(|session| keep(&session.name, session.current_path.as_deref()))
                .cloned()
                .collect();
            if spawned.is_none() {
                print!("{}", render_ls_table(&shown));
                return Ok(());
            }
            if shown.is_empty() {
                let which = if children { "child" } else { "descendant" };
                println!("No {which} processes.");
                return Ok(());
            }
            print!("{}", render_ls_rows(&shown, &sessions));
            Ok(())
        }
        Some(Commands::Inspect {
            target,
            json,
            full_command,
            compact,
        }) => {
            let process = crate::inspect(&target)?;
            if json {
                print_process_json(&process, compact)
            } else {
                print!("{}", render_inspect(&process, full_command));
                Ok(())
            }
        }
        Some(Commands::Capture {
            target,
            lines,
            history,
            json,
        }) => {
            let capture = crate::commands::capture(&target, lines, history)?;
            if json {
                print_json(&capture)
            } else {
                print!("{}", capture.text);
                Ok(())
            }
        }
        Some(Commands::Reply { target, json }) => {
            let reply = crate::commands::reply(&target)?;
            if json {
                print_json(&reply)
            } else {
                print_reply(&reply)
            }
        }

        Some(Commands::Activity {
            target,
            json,
            buckets,
        }) => {
            let process = crate::inspect(&target)?;
            let report = ActivityReport::load(&process.name, buckets);
            if json {
                print_json(&report)
            } else {
                print!("{}", render_activity(&report));
                Ok(())
            }
        }
        Some(Commands::Wait {
            targets,
            all,
            any,
            until,
            ignore_waiting,
            startup_timeout,
            timeout,
            capture_on_block,
            json,
            compact,
        }) => {
            let conditions = until
                .iter()
                .map(|value| parse_wait_condition(value))
                .collect::<Result<Vec<_>>>()?;
            if all || any {
                let mode = if all {
                    crate::commands::WaitMode::All
                } else {
                    crate::commands::WaitMode::Any
                };
                let mut result = crate::commands::wait_many(
                    &targets,
                    mode,
                    conditions,
                    ignore_waiting,
                    timeout,
                )?;
                for each in &mut result.results {
                    each.capture_on_block(capture_on_block);
                }
                if json {
                    print_command_json(&result, compact)?;
                } else {
                    for each in &result.results {
                        print_wait_status(each);
                    }
                }
                for each in &result.results {
                    print_block_capture(each, json);
                }
                return match result.deciding() {
                    Some(deciding) => finish_wait(deciding, false),
                    None => Ok(()),
                };
            }
            let [target] = targets.as_slice() else {
                bail!("waiting on several processes needs --all or --any");
            };
            let mut result =
                crate::wait(target, conditions, ignore_waiting, startup_timeout, timeout)?;
            result.capture_on_block(capture_on_block);
            if json {
                print_command_json(&result, compact)?;
            } else {
                print_wait_status(&result);
            }
            print_block_capture(&result, json);
            finish_wait(&result, false)
        }
        Some(Commands::Ask {
            target,
            stdin,
            prompt_file,
            startup_timeout,
            timeout,
            capture_on_block,
            json,
            compact,
            text,
        }) => {
            let input = read_prompt(prompt_file, stdin, text)?;
            let mut result = crate::commands::ask(&target, &input, startup_timeout, timeout)?;
            result.wait.capture_on_block(capture_on_block);
            if json {
                print_command_json(&result, compact)?;
            } else if let Some(reply) = &result.reply {
                print_reply(reply)?;
            } else {
                print_wait_status(&result.wait);
            }
            print_block_capture(&result.wait, json);
            finish_wait(&result.wait, true)
        }

        Some(Commands::Events { interval, .. }) => {
            if interval.is_zero() {
                bail!("--interval must be greater than zero");
            }
            // Followers are long-lived (the sidebar, third-party dashboards),
            // so a tmux error — even on the very first read, e.g. a pmux
            // server that just crashed refusing connections — must not end
            // the stream. They also write only on change, so a reader that
            // died would go unnoticed forever; exit once the process that
            // started us is gone.
            let parent = std::os::unix::process::parent_id();
            let mut last_error = None;
            let mut previous: Option<crate::StateSnapshot> = None;
            loop {
                if previous.is_some() {
                    thread::sleep(interval);
                }
                if std::os::unix::process::parent_id() != parent {
                    return Ok(());
                }
                let current = match crate::state() {
                    Ok(current) => current,
                    Err(err) => {
                        let message = format!("{err:#}");
                        if last_error.as_ref() != Some(&message) {
                            eprintln!("pmux events: {message}; retrying");
                            last_error = Some(message);
                        }
                        if previous.is_none() {
                            thread::sleep(interval);
                        }
                        continue;
                    }
                };
                last_error = None;
                if previous
                    .as_ref()
                    .is_none_or(|previous| current.sessions != previous.sessions)
                {
                    emit_state_event(&current)?;
                    previous = Some(current);
                }
            }
        }
        Some(Commands::Sidebar {
            socket,
            host_pane,
            pane_label,
        }) => crate::tui::run_sidebar(socket.as_deref(), host_pane.as_deref(), pane_label),
        Some(Commands::Queue { command }) => queue_command(command),
        Some(Commands::QueueUi { target }) => crate::queue_ui::run(&target),
        Some(Commands::Send {
            target,
            stdin,
            prompt_file,
            no_enter,
            key,
            repeat,
            confirm,
            capture_on_block,
            json,
            compact,
            text,
        }) => {
            if let Some(timeout) = confirm {
                let input = read_prompt(prompt_file, stdin, text)?;
                let mut result = crate::commands::send_confirmed(&target, &input, timeout)?;
                result.wait.capture_on_block(capture_on_block);
                result.wait.capture_on_not_ready(capture_on_block);
                if json {
                    print_command_json(&result, compact)?;
                } else if let Some(condition) = &result.wait.matched_condition {
                    println!(
                        "Sent {} byte(s) to '{}'; the agent took it up ({condition}).",
                        result.send.bytes_sent,
                        crate::util::sanitize_terminal_text(&result.wait.session)
                    );
                } else {
                    print_wait_status(&result.wait);
                }
                print_block_capture(&result.wait, json);
                return finish_wait(&result.wait, false);
            }
            if !key.is_empty() {
                let result = crate::commands::send_keys(&target, &key, repeat.unwrap_or(1))?;
                if json {
                    return print_json(&result);
                }
                println!(
                    "Sent {} named key(s) to '{}'; application handling not confirmed.",
                    result.keys_sent,
                    crate::util::sanitize_terminal_text(&result.session)
                );
                return Ok(());
            }
            let input = read_prompt(prompt_file, stdin, text)?;
            let result = crate::send(&target, &input, !no_enter)?;
            if json {
                print_json(&result)
            } else {
                println!(
                    "Sent {} byte(s) to '{}'{}.",
                    result.bytes_sent,
                    crate::util::sanitize_terminal_text(&result.session),
                    if result.enter_sent {
                        "; sent Enter (application submission not confirmed)"
                    } else {
                        ""
                    }
                );
                Ok(())
            }
        }
        Some(Commands::Setup {
            yes,
            dry_run,
            skip,
            autostart,
            preset,
        }) => crate::onboard::run(crate::onboard::Options {
            yes,
            dry_run,
            skip,
            autostart,
            preset,
        }),
        Some(Commands::Init {
            autostart,
            tmux_rb,
            preset,
            agent_docs,
            no_agent_docs,
        }) => {
            let (rc_path, changed, boot_message) = crate::init_with_autostart(autostart)?;
            match preset {
                Some(name) => {
                    crate::set_row_preset(&name)?;
                    println!("Rows use the {name} preset.");
                }
                None => {
                    if let Some(name) = crate::prompt_for_row_preset()? {
                        println!("Rows use the {name} preset.");
                    }
                }
            }
            let docs = match (agent_docs, no_agent_docs) {
                (true, _) => crate::agent_docs::AgentDocsChoice::Install,
                (_, true) => crate::agent_docs::AgentDocsChoice::Remove,
                _ => crate::agent_docs::AgentDocsChoice::Ask,
            };
            for change in crate::agent_docs::apply(docs)? {
                println!(
                    "Agent docs: {} {}",
                    change.action,
                    crate::display_path_buf(&change.path)
                );
            }
            if tmux_rb {
                let (source, created) = crate::config::init_tmux_conf_rb()?;
                let source = crate::display_path_buf(&source);
                if created {
                    println!("Created {source}; edit it, then run `pmux restart`.");
                } else {
                    println!("Compiled {source}.");
                }
            }
            match (rc_path, changed) {
                (Some(rc), true) => {
                    let rc = crate::display_path_buf(&rc);
                    println!("Updated {rc}");
                    println!("Reload your shell to activate: source {rc}");
                }
                (Some(rc), false) => {
                    println!("Shell already configured: {}", crate::display_path_buf(&rc));
                }
                (None, _) => {
                    println!("Automatic shell integration supports Bash and Zsh.");
                    println!(
                        "Generate completions for another shell with 'pmux completions <shell>'."
                    );
                }
            }
            println!("Outer tmux integration: ~/.config/pmux/outer-tmux.conf");
            println!(
                "Source it from your outer tmux config to enable routed ai-attn indicators and the prefix + Ctrl-p sidebar."
            );
            if let Some(msg) = boot_message {
                println!("{msg}");
            }
            Ok(())
        }
        Some(Commands::Preset { name }) => {
            match name {
                Some(name) => {
                    let path = crate::set_row_preset(&name)?;
                    println!(
                        "Rows use the {name} preset (set in {}). Reopen the sidebar to see it.",
                        crate::display_path_buf(&path)
                    );
                }
                None => println!("{}", crate::row_preset_list()),
            }
            Ok(())
        }
        Some(Commands::StatusLabels) => crate::status_labels::publish_status_labels(),
        Some(Commands::HostSocket { session }) => {
            let session = match session {
                Some(session) => session,
                None => {
                    let pane = std::env::var("TMUX_PANE")
                        .ok()
                        .filter(|_| crate::tmux::inside_pmux_client())
                        .ok_or_else(|| {
                            anyhow::anyhow!("not inside a pmux process; pass --session NAME")
                        })?;
                    crate::tmux::session_name_for_pane(&pane)?
                }
            };
            match crate::state::read_session_host_socket(&session)? {
                Some(socket) => {
                    println!("{socket}");
                    Ok(())
                }
                None => anyhow::bail!(
                    "pmux does not know which tmux server is showing '{session}'; \
                     it learns this when the process is attached from your tmux"
                ),
            }
        }
        Some(Commands::KeyHints { color }) => crate::key_hints::print_key_hints(color),
        Some(Commands::Snapshot) => {
            let snap = crate::snapshot()?;
            println!("Snapshot saved: {} session(s).", snap.sessions.len());
            Ok(())
        }
        Some(Commands::Restore { boot }) => {
            let outcome = crate::restore(boot)?;
            if outcome.disabled {
                println!("Restore is disabled in config ([restore] enabled = false).");
            } else if outcome.restored == 0 && outcome.failed == 0 {
                println!(
                    "Nothing restored ({} skipped, {} missing a resume id).",
                    outcome.skipped, outcome.missing_resume_id
                );
            } else {
                println!(
                    "Restored {} session(s): {} agent(s) resumed, {} skipped, {} missing a resume id, {} failed.",
                    outcome.restored,
                    outcome.resumed,
                    outcome.skipped,
                    outcome.missing_resume_id,
                    outcome.failed
                );
            }
            if outcome.held > 0 {
                println!(
                    "{} saved command(s) were not re-run ([restore] relaunch = \"safe\" only recreates plain shells and agents). \
                     Start them again yourself, or set relaunch = \"all\" in ~/.config/pmux/config.toml.",
                    outcome.held
                );
            }
            if outcome.failed > 0 {
                bail!("{} session(s) failed to restore", outcome.failed);
            }
            if outcome.missing_resume_id > 0 {
                bail!(
                    "{} agent session(s) were not restored because their native resume id is missing",
                    outcome.missing_resume_id
                );
            }
            Ok(())
        }
        Some(Commands::Doctor { target }) => match target {
            DoctorTarget::AiAttn { process: None } => {
                print!("{}", crate::doctor_ai_attn()?);
                Ok(())
            }
            DoctorTarget::AiAttn {
                process: Some(name),
            } => {
                print!("{}", crate::debug_ai_attn_session(&name)?);
                Ok(())
            }
            DoctorTarget::Restore => {
                print!("{}", crate::restore_doctor()?);
                Ok(())
            }
            DoctorTarget::Tmux { socket } => {
                let (report, found_problem) = crate::doctor_tmux::doctor(socket.as_deref())?;
                print!("{report}");
                if found_problem {
                    std::process::exit(1);
                }
                Ok(())
            }
        },
        Some(Commands::Completions { shell }) => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "pmux", &mut io::stdout());
            Ok(())
        }
        Some(Commands::RelayAiAttn {
            loop_forever,
            dry_run,
            verbose,
            restart,
        }) => {
            if restart {
                crate::restart_ai_attn_relay()?;
                println!("Restarted ai-attn relay.");
                return Ok(());
            }
            if dry_run {
                print!("{}", crate::relay_ai_attn_dry_run(verbose)?);
                return Ok(());
            }
            crate::relay_ai_attn(loop_forever)
        }
        Some(Commands::CopyToClipboard {
            client,
            session,
            text,
        }) => crate::clipboard::copy_to_clipboard(&text, &client, &session),
        Some(Commands::KillCurrentAndNext { current, client }) => {
            crate::kill_current_and_next(current, client)
        }
        Some(Commands::UpdateHostPane {
            pane_id,
            socket,
            all,
        }) => {
            if all {
                let socket = socket
                    .as_deref()
                    .ok_or_else(|| anyhow!("--socket is required with --all"))?;
                let refresh = crate::update_all_host_panes(socket)?;
                println!(
                    "Updated {} host-pane mapping(s); {} pane(s) without a pmux session skipped.",
                    refresh.updated, refresh.skipped
                );
                for err in &refresh.errors {
                    eprintln!("warning: {err}");
                }
                if !refresh.errors.is_empty() {
                    bail!("{} pane(s) failed to update", refresh.errors.len());
                }
                return Ok(());
            }
            let pane_id =
                pane_id.ok_or_else(|| anyhow!("pane_id is required unless --all is set"))?;
            crate::update_host_pane(&pane_id, socket.as_deref())?;
            Ok(())
        }
        Some(Commands::ToggleSidebar { pane_id, socket }) => {
            crate::toggle_sidebar(&pane_id, &socket)
        }
        Some(Commands::OnFocus {
            pane_id,
            window_id,
            socket,
        }) => crate::on_focus(&pane_id, &window_id, &socket),
        Some(Commands::TmuxInit { apply, socket }) => {
            let binary = crate::resolve_pmux_binary()?;
            if apply {
                let socket = socket
                    .as_deref()
                    .ok_or_else(|| anyhow!("--socket is required with --apply"))?;
                crate::apply_tmux_init(&binary, socket)
            } else {
                print!("{}", crate::tmux_init_script(&binary));
                Ok(())
            }
        }
        Some(Commands::EqualizePanes { pane_id, socket }) => {
            crate::equalize_panes(&pane_id, &socket)
        }
        Some(Commands::SidebarWidth { pane_id, socket }) => {
            crate::record_sidebar_width(&pane_id, &socket)
        }
        Some(Commands::SidebarStabilize { window_id, socket }) => {
            crate::stabilize_sidebar_width(&window_id, &socket)
        }
        Some(Commands::RenameSession { old_name, new_name }) => {
            crate::rename_session(&old_name, &new_name)
        }
        Some(Commands::SessionActions {
            session_name,
            socket,
            host_pane,
            mode,
            mouse_opened,
        }) => crate::tui::run_session_actions(
            &socket,
            &host_pane,
            &session_name,
            mode.as_deref(),
            mouse_opened,
        ),
        Some(Commands::SidebarMenuItem { token, item }) => {
            show_error_under_run_shell(crate::tui::run_sidebar_menu_item(&token, item))
        }
        Some(Commands::Prompt {
            title,
            action,
            value,
            confirm,
            danger,
            split,
            command,
        }) => {
            let prompt = match confirm {
                Some(detail) => crate::tui::Prompt::confirm(title, action, detail),
                None => crate::tui::Prompt::text(title, action, &value),
            };
            let prompt = if danger { prompt.danger() } else { prompt };
            crate::tui::run_prompt_command(prompt, split, &command)
        }
        Some(Commands::RenameSessionUi { session_name, pane }) => {
            let session_name = match (session_name, pane) {
                (Some(name), _) => name,
                (None, Some(pane)) => crate::tmux::session_name_for_pane(&pane)?,
                (None, None) => crate::current_session_name()?,
            };
            crate::tui::run_rename_session_ui(&session_name)
        }
    }
}

// Format agent history entries into a columnar table for `pmux history`.
fn render_history_table(entries: &[crate::HistoryEntry]) -> String {
    if entries.is_empty() {
        return "No resumable agent sessions.\n".to_string();
    }
    let headers = ["ID", "AGENT", "ACTIVE", "STATUS", "TITLE", "CWD"];
    // Transcript-derived text is sanitized at ingest; this also covers ids,
    // agent names, process names and cwds read from the same files.
    let clean = crate::util::sanitize_terminal_text;
    let rows: Vec<[String; 6]> = entries
        .iter()
        .map(|entry| {
            let status = match (&entry.pmux_process, entry.live, entry.cwd_exists) {
                (Some(process), _, _) => format!("in {}", clean(process)),
                (None, true, _) => "running".to_string(),
                (None, false, false) => "cwd gone".to_string(),
                (None, false, true) => "-".to_string(),
            };
            [
                clean(&entry.session_id).chars().take(8).collect(),
                clean(&entry.agent),
                format!("{} ago", crate::relative_age(entry.last_activity_epoch)),
                status,
                truncate_col(&clean(entry.title.as_deref().unwrap_or("-")), 50),
                entry
                    .cwd
                    .as_deref()
                    .map_or("-".to_string(), |cwd| clean(&crate::display_path(cwd))),
            ]
        })
        .collect();
    let mut widths: [usize; 6] = std::array::from_fn(|i| headers[i].len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(crate::display_width(cell));
        }
    }
    let line = |cells: [&str; 6]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            out.push_str(cell);
            if i + 1 < cells.len() {
                let pad = widths[i] - crate::display_width(cell) + 1;
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push('\n');
        out
    };
    let mut out = line(headers);
    for row in &rows {
        out.push_str(&line(std::array::from_fn(|i| row[i].as_str())));
    }
    out
}

// Format the list of sessions into a columnar table for `pmux ls` output.
fn render_ls_table(sessions: &[crate::SessionInfo]) -> String {
    render_ls_rows(sessions, sessions)
}

// Rows for `shown`; agent signals are matched against every running process
// in `all`, so a filtered list cannot make an agent look like the only one.
fn render_ls_rows(shown: &[crate::SessionInfo], all: &[crate::SessionInfo]) -> String {
    let sessions = shown;
    if sessions.is_empty() {
        return "No running pmux processes.\n".to_string();
    }

    let snapshot = crate::AiAttnSnapshot::load();
    let counts = crate::agent_counts(all);
    let cwd_counts = crate::agent_cwd_counts(all);
    let mut rows: Vec<[String; 7]> = Vec::with_capacity(sessions.len());
    for s in sessions {
        let ai_state = crate::session_ai_state(
            s,
            crate::is_sole_agent(s, &counts),
            crate::is_sole_agent_cwd(s, &cwd_counts),
            &snapshot,
        );
        let clean = crate::util::sanitize_terminal_text;
        let display_cmd = truncate_col(&clean(&crate::display_command(s)), 20);
        let (state_text, since) = match ai_state {
            Some((state, updated_at)) => (state.to_string(), crate::relative_age(updated_at)),
            None => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let idle_seconds = now - s.last_activity_epoch;
                let state = if idle_seconds > 5 { "idle" } else { "active" };
                (
                    state.to_string(),
                    crate::relative_age(s.last_activity_epoch),
                )
            }
        };
        let display_cwd = s
            .current_path
            .as_deref()
            .or(s.launch_cwd.as_deref())
            .unwrap_or("-");
        rows.push([
            truncate_col(&clean(&s.name), 24),
            truncate_col(&display_cmd, 20),
            format!("{} ago", crate::relative_age(s.created_epoch)),
            state_text,
            since,
            s.attached_clients.to_string(),
            clean(&crate::display_path(display_cwd)),
        ]);
    }

    let headers = [
        "NAME", "COMMAND", "STARTED", "STATE", "SINCE", "CLIENTS", "CWD",
    ];
    let mut widths: [usize; 7] = std::array::from_fn(|i| headers[i].len());
    for row in &rows {
        for i in 0..7 {
            widths[i] = widths[i].max(row[i].len());
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} {:<w5$} {:<w6$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        headers[6],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
        w6 = widths[6],
    ));

    for row in rows {
        out.push_str(&format!(
            "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} {:<w5$} {:<w6$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            row[6],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
        ));
    }

    out
}

// Check whether an error represents a user-cancelled picker selection.
fn is_selection_cancelled(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("selection cancelled"))
}

// Split the required command from its trailing arguments. Clap consumes pmux
// options before this point and preserves everything after the command.
fn parse_run_parts(parts: Vec<String>) -> Result<(String, Vec<String>)> {
    let Some(command) = parts.first().cloned() else {
        return Err(anyhow!("run requires a command"));
    };
    let args = parts.into_iter().skip(1).collect();
    Ok((command, args))
}

#[cfg(test)]
mod tests {
    use super::{
        ActivityReport, Cli, Commands, build_help_text, is_selection_cancelled, parse_duration,
        parse_run_parts, parse_wait_condition, render_activity, render_ls_table, truncate_col,
    };
    use clap::Parser;

    // --- parse_run_parts ---

    #[test]
    fn ask_parses_options_after_prompt_and_allows_explicit_literal_separator() {
        let cli = Cli::try_parse_from([
            "pmux",
            "ask",
            "worker",
            "Run curl -sI https://example.com",
            "--timeout",
            "3m",
            "--capture-on-block=10",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Commands::Ask {
                text,
                timeout,
                capture_on_block,
                json,
                ..
            } => {
                assert_eq!(text, ["Run curl -sI https://example.com"]);
                assert_eq!(timeout, Some(std::time::Duration::from_secs(180)));
                assert_eq!(capture_on_block, Some(10));
                assert!(json);
            }
            _ => panic!("expected ask"),
        }
        let cli = Cli::try_parse_from(["pmux", "ask", "worker", "--", "--json", "--timeout", "3m"])
            .unwrap();
        match cli.command.unwrap() {
            Commands::Ask {
                text,
                timeout,
                json,
                ..
            } => {
                assert_eq!(text, ["--json", "--timeout", "3m"]);
                assert_eq!(timeout, None);
                assert!(!json);
            }
            _ => panic!("expected ask"),
        }
    }

    #[test]
    fn cli_parses_short_session_name_before_command() {
        let cli =
            Cli::try_parse_from(["pmux", "run", "-s", "mobile-codex", "codex"]).expect("parse");
        let Some(Commands::Run {
            session_name,
            parts,
            ..
        }) = cli.command
        else {
            panic!("expected run command");
        };
        assert_eq!(session_name.as_deref(), Some("mobile-codex"));
        assert_eq!(parts, vec!["codex"]);
    }

    #[test]
    fn cli_parses_long_session_name_before_command() {
        let cli = Cli::try_parse_from(["pmux", "run", "--session-name", "my-session", "claude"])
            .expect("parse");
        let Some(Commands::Run {
            session_name,
            parts,
            ..
        }) = cli.command
        else {
            panic!("expected run command");
        };
        assert_eq!(session_name.as_deref(), Some("my-session"));
        assert_eq!(parts, vec!["claude"]);
    }

    #[test]
    fn cli_passes_session_flag_after_command_to_child() {
        let cli = Cli::try_parse_from([
            "pmux",
            "run",
            "codex",
            "-s",
            "child-session",
            "--unsafe-arg",
        ])
        .expect("parse");
        let Some(Commands::Run {
            session_name,
            parts,
            ..
        }) = cli.command
        else {
            panic!("expected run command");
        };
        assert!(session_name.is_none());
        assert_eq!(parts, vec!["codex", "-s", "child-session", "--unsafe-arg"]);
    }

    #[test]
    fn cli_rejects_session_name_without_command() {
        assert!(Cli::try_parse_from(["pmux", "run", "-s", "name"]).is_err());
    }

    #[test]
    fn cli_rejects_missing_session_name_value() {
        assert!(Cli::try_parse_from(["pmux", "run", "-s"]).is_err());
    }

    #[test]
    fn parse_run_empty_is_error() {
        assert!(parse_run_parts(vec![]).is_err());
    }

    #[test]
    fn parse_run_no_extra_args() {
        let (cmd, args) = parse_run_parts(vec!["codex".to_string()]).expect("parse");
        assert_eq!(cmd, "codex");
        assert!(args.is_empty());
    }

    fn process_with_command(command: &str) -> crate::ProcessInfo {
        crate::ProcessInfo {
            name: "review".into(),
            pane_id: "%1".into(),
            lifecycle: "running".into(),
            created_at: 1,
            last_activity_at: 1,
            attached_clients: 0,
            current_command: "codex".into(),
            start_command: Some(format!("supervisor {command:?}")),
            current_path: None,
            launch_command: Some(command.into()),
            launch_cwd: None,
            agent: None,
            startup: None,
            auto: None,
            spawned_by: None,
        }
    }

    #[test]
    fn inspect_bounds_long_commands_and_full_command_restores_them() {
        let command = format!("codex {}", "表🙂".repeat(1000));
        let process = process_with_command(&command);
        let rendered = super::render_inspect(&process, false);
        let line = rendered
            .lines()
            .find_map(|line| line.strip_prefix("launch command: "))
            .unwrap();
        assert!(crate::util::display_width(line) <= 120);
        assert!(line.ends_with('…'));
        assert!(rendered.contains("--full-command"));
        assert!(super::render_inspect(&process, true).contains(&command));
        let short = super::render_inspect(&process_with_command("codex 'hello'"), false);
        assert!(short.contains("launch command: codex 'hello'\n"));
        assert!(!short.contains("truncated"));
    }

    #[test]
    fn inspect_sanitizes_commands_even_when_showing_full_text() {
        let process = process_with_command("codex \u{1b}[2Jhello\nworld");
        for full in [false, true] {
            let rendered = super::render_inspect(&process, full);
            assert!(!rendered.contains('\u{1b}'));
            assert!(!rendered.contains("hello\nworld"));
        }
    }

    #[test]
    fn compact_json_preserves_launch_text_and_legacy_json_is_unchanged() {
        let command = "codex 'quotes \" and \\ and\nnewlines'";
        let process = process_with_command(command);
        let inspected = super::VersionedProcess {
            api_version: crate::API_VERSION,
            process: &process,
        };
        let snapshot = crate::StateSnapshot {
            api_version: crate::API_VERSION,
            generated_at: 1,
            sessions: vec![process.clone()],
        };
        let waited = crate::commands::WaitResult {
            api_version: crate::API_VERSION,
            session: "review".into(),
            condition: "agent:done".into(),
            outcome: "matched".into(),
            matched_condition: Some("agent:done".into()),
            observed_at: 1,
            capture: None,
            capture_error: None,
            process: Some(process.clone()),
        };
        for (legacy, compact, path) in [
            (
                super::command_json(&inspected, false).unwrap(),
                super::command_json(&inspected, true).unwrap(),
                "",
            ),
            (
                super::command_json(&snapshot, false).unwrap(),
                super::command_json(&snapshot, true).unwrap(),
                "/sessions/0",
            ),
            (
                super::command_json(&waited, false).unwrap(),
                super::command_json(&waited, true).unwrap(),
                "/process",
            ),
        ] {
            assert!(legacy.pointer(path).unwrap().get("start_command").is_some());
            assert!(
                compact
                    .pointer(path)
                    .unwrap()
                    .get("start_command")
                    .is_none()
            );
            assert_eq!(compact.pointer(path).unwrap()["launch_command"], command);
            let mut expected = legacy;
            expected
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("start_command");
            assert_eq!(compact, expected);
        }
        assert_eq!(
            super::command_json(&waited, false).unwrap(),
            serde_json::to_value(&waited).unwrap()
        );
    }

    #[test]
    fn compact_json_bounds_unicode_launch_commands_and_marks_truncation() {
        let process = process_with_command(&format!("codex {}", "表🙂".repeat(2000)));
        let compact = super::command_json(&process, true).unwrap();
        let preview = compact["launch_command"].as_str().unwrap();
        assert_eq!(preview.chars().count(), 120);
        assert!(preview.ends_with('…'));
        assert_eq!(compact["launch_command_truncated"], true);
        assert_eq!(
            super::command_json(&process, false).unwrap()["launch_command"],
            process.launch_command.unwrap()
        );
    }

    #[test]
    fn human_agent_state_uses_relative_time() {
        let mut process = process_with_command("codex");
        process.agent = Some(crate::api::AgentInfo {
            kind: "codex".into(),
            state: Some("done".into()),
            updated_at: Some(crate::util::now_epoch() - 1500),
            source: None,
            reason: None,
            session_id: None,
        });
        assert!(super::render_inspect(&process, false).contains("agent state: done, 25m ago"));
        assert!(!super::render_inspect(&process, false).contains("agent session:"));
        process.agent.as_mut().unwrap().session_id = Some("019a-thread".into());
        assert!(super::render_inspect(&process, false).contains("agent session: 019a-thread\n"));
    }

    #[test]
    fn prompt_file_is_an_exclusive_text_source() {
        assert!(
            Cli::try_parse_from(["pmux", "send", "reader", "--prompt-file", "prompt.txt"]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "pmux",
                "send",
                "reader",
                "--prompt-file",
                "prompt.txt",
                "--no-enter"
            ])
            .is_ok()
        );
        for suffix in [
            vec!["text"],
            vec!["--stdin"],
            vec!["--key", "Escape"],
            vec!["--repeat", "2"],
        ] {
            let mut args = vec!["pmux", "send", "reader", "--prompt-file", "prompt.txt"];
            args.extend(suffix);
            assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn named_keys_are_an_exclusive_input_mode() {
        for args in [
            vec!["pmux", "send", "reader", "--key", "Escape"],
            vec![
                "pmux", "send", "reader", "--key", "C-u", "--key", "BSpace", "--repeat", "12",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["pmux", "send", "reader", "--key", "Escape", "text"],
            vec!["pmux", "send", "reader", "--key", "Escape", "--stdin"],
            vec!["pmux", "send", "reader", "--key", "Escape", "--no-enter"],
            vec!["pmux", "send", "reader", "--repeat", "2", "text"],
            vec!["pmux", "send", "reader", "--key", "Escape", "--repeat", "0"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "accepted: {args:?}");
        }
    }

    #[test]
    fn command_output_flags_require_the_appropriate_output_mode() {
        assert!(Cli::try_parse_from(["pmux", "inspect", "review", "--full-command"]).is_ok());
        assert!(
            Cli::try_parse_from(["pmux", "inspect", "review", "--full-command", "--json"]).is_err()
        );
        for command in ["inspect", "wait"] {
            assert!(Cli::try_parse_from(["pmux", command, "review", "--compact"]).is_err());
            assert!(
                Cli::try_parse_from(["pmux", command, "review", "--json", "--compact"]).is_ok()
            );
        }
    }

    #[test]
    fn inspect_alias_parses_with_json_output() {
        let cli = Cli::try_parse_from(["pmux", "i", "build", "--json"]).expect("parse");
        let Some(Commands::Inspect { target, json, .. }) = cli.command else {
            panic!("expected inspect command");
        };
        assert_eq!(target, "build");
        assert!(json);
    }

    #[test]
    fn parses_activity_with_default_buckets() {
        let cli = Cli::try_parse_from(["pmux", "activity", "build", "--json"]).unwrap();
        let Some(Commands::Activity {
            target,
            json,
            buckets,
        }) = cli.command
        else {
            panic!("expected activity command");
        };
        assert_eq!(target, "build");
        assert!(json);
        assert_eq!(buckets, 48);
    }

    #[test]
    fn activity_for_a_process_without_a_transcript_is_unavailable_and_zeroed() {
        let report = ActivityReport::load("no-such-process-for-activity", 8);
        assert!(!report.available);
        assert_eq!(report.prompts, 0);
        assert!(report.churn_series.is_empty());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["api_version"], crate::API_VERSION);
        assert!(render_activity(&report).contains("no agent transcript"));
    }

    // `activity --json` is covered by the JSON compatibility policy: renaming,
    // retyping, or dropping a field must fail here. The series values depend
    // on the drawer's graph resampling, which is not part of the contract, so
    // only their shape (an array of `buckets` numbers) is pinned.
    #[test]
    fn activity_json_matches_the_published_v1_fixture() {
        let data = crate::ActivityData {
            churn: vec![(100, 5.0), (200, 8.0), (300, 3.0)],
            out_tokens: vec![(100, 40.0), (250, 90.0)],
            in_tokens: vec![(100, 400.0), (250, 900.0)],
            prompts: 3,
            total_in: 45_678,
            total_out: 12_345,
            added: 10,
            deleted: 4,
            tools: 7,
            first: Some(100),
            last: Some(300),
            summary: Some("do the thing".to_string()),
            recent_prompts: vec!["do the thing".to_string()],
        };
        let report = ActivityReport::from_data("agent", Some(data), 8);
        let mut actual = serde_json::to_value(&report).unwrap();
        for series in ["churn_series", "output_token_series"] {
            let values = actual[series].as_array().unwrap();
            assert_eq!(values.len(), 8, "{series}");
            assert!(values.iter().all(serde_json::Value::is_number), "{series}");
            actual[series] = serde_json::json!([]);
        }
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/api/activity-v1.json")).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn parses_human_friendly_durations() {
        assert_eq!(parse_duration("500ms").unwrap().as_millis(), 500);
        assert_eq!(parse_duration("30s").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("2m").unwrap().as_secs(), 120);
        assert_eq!(parse_duration("1h").unwrap().as_secs(), 3600);
        assert_eq!(parse_duration("7").unwrap().as_secs(), 7);
        assert!(parse_duration("later").is_err());
    }

    #[test]
    fn wait_conditions_require_explicit_agent_prefix() {
        assert_eq!(
            parse_wait_condition("exited").unwrap(),
            crate::WaitCondition::Exited
        );
        assert_eq!(
            parse_wait_condition("agent:waiting").unwrap(),
            crate::WaitCondition::AgentState("waiting".to_string())
        );
        assert!(parse_wait_condition("waiting").is_err());
        assert!(parse_wait_condition("agent:").is_err());
    }

    // --- render_ls_table ---

    // Tests that render_ls_table prints a "no processes" message when empty.
    #[test]
    fn render_ls_empty_message() {
        let out = render_ls_table(&[]);
        assert_eq!(out, "No running pmux processes.\n");
    }

    // Helper to construct a SessionInfo for table rendering tests.
    fn make_session(name: &str, cmd: &str) -> crate::SessionInfo {
        crate::SessionInfo {
            name: name.to_string(),
            pane_id: "%0".to_string(),
            created_epoch: 1_000_000_000,
            last_activity_epoch: 1_000_000_000,
            current_command: cmd.to_string(),
            start_command: Some(cmd.to_string()),
            current_path: Some("/home/user".to_string()),
            launch_command: None,
            launch_cwd: None,
            attached_clients: 0,
        }
    }

    // Escape sequences in cwds, argv or transcript titles never reach the terminal.
    #[test]
    fn plain_text_output_strips_terminal_escapes() {
        let mut session = make_session("esc", "sh");
        session.current_path = Some("/tmp/\u{1b}]0;pwned\u{7}dir".to_string());
        session.launch_command = Some("sh -c '\u{1b}[2Jecho'".to_string());
        let ls = render_ls_table(&[session]);
        assert!(!ls.contains('\u{1b}'), "ls leaked an escape: {ls:?}");
        assert!(ls.contains("/tmp/dir"));

        let entry = crate::HistoryEntry {
            agent: "claude".to_string(),
            session_id: "0f8e2c4a-7b1d".to_string(),
            title: Some("t\u{1b}[31mitle".to_string()),
            title_source: None,
            first_prompt: None,
            cwd: Some("/w/\u{1b}[1Aup".to_string()),
            cwd_exists: true,
            git_branch: None,
            started_epoch: None,
            last_activity_epoch: 1_000_000_000,
            transcript: String::new(),
            live: true,
            pmux_process: Some("p\u{9b}2J".to_string()),
            resume_command: None,
        };
        let history = super::render_history_table(&[entry]);
        assert!(
            !history.contains('\u{1b}') && !history.contains('\u{9b}'),
            "history leaked an escape: {history:?}"
        );
        assert!(history.contains("title") && history.contains("/w/up"));
    }

    // Tests that the table includes header columns and session data.
    #[test]
    fn render_ls_table_with_sessions_has_headers_and_row() {
        let sessions = vec![make_session("my-project", "codex")];
        let out = render_ls_table(&sessions);
        assert!(out.contains("NAME"), "missing NAME header");
        assert!(out.contains("COMMAND"), "missing COMMAND header");
        assert!(out.contains("STATE"), "missing STATE header");
        assert!(out.contains("SINCE"), "missing SINCE header");
        assert!(out.contains("my-project"), "missing session name");
        assert!(out.contains("codex"), "missing command name");
        assert!(out.contains("CLIENTS"), "missing CLIENTS header");
    }

    // Tests that multiple sessions produce multiple rows in the table.
    #[test]
    fn render_ls_table_multiple_rows() {
        let sessions = vec![
            make_session("alpha", "codex"),
            make_session("beta", "claude"),
        ];
        let out = render_ls_table(&sessions);
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains("claude"));
        // Each row should be on its own line (header + 2 data rows = 3 lines min).
        assert!(out.lines().count() >= 3);
    }

    // --- truncate_col ---

    // Tests that a string fitting exactly within width is returned unchanged.
    #[test]
    fn truncate_fits_exactly() {
        assert_eq!(truncate_col("hello", 5), "hello");
    }

    // Tests that a short string is returned as-is.
    #[test]
    fn truncate_shorter_than_max() {
        assert_eq!(truncate_col("hi", 10), "hi");
    }

    // Tests that a string exceeding max width is truncated with an ellipsis.
    #[test]
    fn truncate_longer_than_max() {
        let out = truncate_col("hello world", 6);
        assert_eq!(out.chars().count(), 6);
        assert!(out.ends_with('…'));
    }

    // Tests that width=1 produces just an ellipsis.
    #[test]
    fn truncate_width_one() {
        assert_eq!(truncate_col("anything", 1), "…");
    }

    // Tests that width=0 produces just an ellipsis.
    #[test]
    fn truncate_width_zero() {
        assert_eq!(truncate_col("anything", 0), "…");
    }

    // --- is_selection_cancelled ---

    // Tests that an error containing "selection cancelled" is detected.
    #[test]
    fn is_selection_cancelled_matches_message() {
        let err = anyhow::anyhow!("selection cancelled by user");
        assert!(is_selection_cancelled(&err));
    }

    // Tests that unrelated errors are not treated as cancelled.
    #[test]
    fn is_selection_cancelled_non_matching() {
        let err = anyhow::anyhow!("some other error occurred");
        assert!(!is_selection_cancelled(&err));
    }

    // --- build_help_text ---

    // Tests that help text includes command aliases.
    #[test]
    fn test_help_text_mentions_aliases() {
        let text = build_help_text("pmux");
        assert!(text.contains("a=attach"));
        assert!(text.contains("w=wrap"));
    }

    // Tests that the binary name is properly substituted into help text.
    #[test]
    fn test_help_text_substitutes_binary_name() {
        let text = build_help_text("mymux");
        assert!(text.contains("mymux run"));
        assert!(text.contains("mymux attach"));
        assert!(
            !text.contains("{binary}"),
            "template variable should be replaced"
        );
    }
}
