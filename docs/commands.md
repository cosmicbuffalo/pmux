# Command reference

Most people use only a handful of pmux commands day to day — `pmux wrap` once,
then `pmux a`, `pmux l`, and the keys inside a process (see
[Typical usage](../README.md#typical-usage)). This page documents everything else: the
full CLI and the details behind the everyday commands. The JSON automation
API that scripts and agents use is in [Scripting and agents](scripting.md).

## All commands

```sh
pmux                      # show the concise command guide
pmux help                 # show the concise command guide explicitly
pmux h                    # shortcut for help

pmux run <command>        # start a process and attach to it
pmux run -s <name> <cmd>  # start with a stable session name
pmux run --cwd <dir> <cmd>  # start in a specific working directory
pmux run -d <command>     # start detached and print its generated name
pmux run --no-parent <cmd>  # don't record the calling pmux process as parent
pmux r <command>          # shortcut for run

pmux parent <name>        # show which process spawned this one
pmux parent <name> --set <parent>  # adopt it; --clear forgets

pmux attach               # choose a process from the picker
pmux attach <name>        # attach to one named process
pmux attach <session-id>  # resume a past agent session (see pmux history)
pmux attach newest        # attach to the newest process
pmux attach oldest        # attach to the oldest process
pmux a                    # shortcut for attach

pmux last                 # switch to this pane's previous process
pmux l                    # shortcut for last

pmux rename <old> <new>   # rename a process
pmux kill <name>          # terminates the session and its process
pmux kill --children      # kill every process this pane's process spawned
pmux k <name>             # shortcut for kill

pmux wrap <command>       # create a shell wrapper
pmux w <command>          # shortcut for wrap

pmux unwrap <command>     # remove a shell wrapper
pmux wraps                # list wrapped commands

pmux respawn <name>...    # restart agents in place, resuming their conversations
pmux respawn --agent claude --dry-run  # show which agents would restart, and how
pmux respawn <name> -- <flags>         # restart one agent with new flags, same conversation

pmux restart              # soft reload (keeps sessions alive)
pmux restart --repair     # undo a tmux config loaded into pmux's server; processes keep running
pmux restart --hard       # ⚠ kills the pmux server and ALL its processes

pmux ls                   # list running processes
pmux ls --json            # list a versioned JSON process snapshot
pmux ls --children        # only processes this pane's process spawned
pmux inspect <name>       # show details for one process
pmux capture <name>       # read its visible screen, including approval prompts
pmux reply <name>         # read the last Claude Code / Codex / Cortex Code assistant message
pmux activity <name>      # show an agent's token, churn, and prompt totals
pmux i <name>             # shortcut for inspect
pmux history              # list past Claude Code / Codex / Cortex Code sessions you can resume
pmux history --here       # only sessions that ran in this directory (or below)
pmux history --all --json # every session, live ones included, as JSON
pmux history -q billing   # fuzzy-rank sessions; with --json, each gets its match
pmux history --resumable  # only sessions pmux can resume right now
pmux resume <id>          # resume a past session in a new pmux process (id prefix ok)
pmux events               # stream JSON Lines state snapshots as they change
pmux send <name> 'text'   # send literal input and press Enter
pmux send <name> --no-enter 'text'  # send literal input without Enter
pmux send <name> --stdin  # read literal input from standard input
pmux send <name> --confirm 'text'  # send, then wait until the agent starts on it
pmux ask <name> --prompt-file task.txt  # send, wait for done, print reply
pmux wait <name>          # block until a process exits
pmux wait <name> --until agent:waiting  # wait for matching ai-attn state

pmux snapshot             # capture live sessions for cross-restart restore
pmux restore              # recreate sessions from the last snapshot (+ resume agents)
pmux doctor restore       # diagnose the cross-restart restore setup
pmux doctor tmux          # check pmux's server and your tmux for mix-ups
pmux host-socket          # inside a pmux process: the socket of your tmux showing it
pmux doctor ai-attn       # diagnose ai-attn relay and tmux host-pane mappings
pmux doctor ai-attn <name>  # why one process is or isn't relayed
pmux equalize-panes <pane> --socket <path>  # even out your tmux panes, keeping the sidebar's width

pmux init                 # set up shell completions and wrapper sourcing
pmux init --autostart     # also install the boot service (restore after reboot)
pmux preset [NAME]        # list the row presets, or switch the sidebar and picker to one
```

## How the commands fit together

Most work in pmux follows the same loop: start a process, detach from it when
needed, inspect or switch between live processes, and eventually kill it. Use
`pmux <command> --help` for every option accepted by a command. Wherever a
command accepts a `<target>`, pass a session name or the selector `newest` or
`oldest`.

### Start and manage processes

| Command | When to use it | Example |
| --- | --- | --- |
| `run` (`r`) | Start a command in a new managed process. Put pmux options before the command so the remaining arguments are passed through unchanged. | `pmux run -s api --cwd ~/src/api codex --full-auto` |
| `attach` (`a`) | Attach to a named live process. With no target, open the interactive picker for live processes and resumable agent history. | `pmux attach api` |
| `last` (`l`) | Toggle the current pane back to its previously attached process. Outside pmux, reattach the most recent target. | `pmux last` |
| `kill` (`k`) | Stop a managed process and remove its live session. This cannot be undone. `--children` kills every process the calling pane's process spawned, `--descendants` every process below it at any depth. | `pmux kill --children` |
| `respawn` | Restart agents in their own panes with their resume command, so they pick up the server's current environment and config. Busy agents and agents with no resume id are skipped unless you pass `--force`. For one named process, arguments after `--` are appended to the relaunch command, for example new agent flags; the name, pane and parent link stay. | `pmux respawn worker -- --sandbox danger-full-access` |
| `parent` | Show which process spawned a process, or fix it after the fact with `--set PARENT` or `--clear`. | `pmux parent worker --set lead` |
| `restart` | Reload pmux's tmux configuration while keeping processes alive. `--repair` first resets pmux's server to tmux's defaults, undoing a tmux config loaded into it by mistake. Add `--hard` only when you intend to kill every pmux process. | `pmux restart --repair` |
| `rename` | Rename a process; `Ctrl-p ,` does the same from inside it. | `pmux rename 1794415680-kmprutzg api` |

Use `run --detach` when another script or terminal will attach later. It prints
the new session name, which makes it easy to capture and reuse:

```sh
session=$(pmux run --detach -s tests cargo test -- --nocapture)
pmux attach "$session"
```

When `pmux run` or `pmux resume` runs inside a pmux pane, for example an
agent starting a helper, the process in that pane is recorded as the new
process's parent and reported as `spawned_by` (see
[the automation API](scripting.md#automation-api)). These options, placed before the
command, change that:

| Option | Effect |
| --- | --- |
| `--no-parent` | Record no parent. pmux's own key bindings and the sidebar pass this, since a person started the process. |
| `--parent NAME` | Record a running local process as the parent. |

The parent is found through `$TMUX_PANE`, trusted only when tmux confirms the
pane is on the same pmux socket as `$TMUX`; if the lookup fails nothing is
recorded and the spawn goes ahead. Restore keeps the link to a recreated
parent. `pmux parent NAME --set PARENT` refuses a link that would make a
process its own ancestor.

### Find, inspect, and resume work

| Command | When to use it | Example |
| --- | --- | --- |
| `ls` | Get a compact list of currently running processes; add `--json` for scripts. `--children` lists only the processes the calling pane's process spawned, `--descendants` every process below it; `--of NAME` names another parent. | `pmux ls --children --json` |
| `inspect` (`i`) | Show command, directory, attachment, and optional agent details for one live process. | `pmux inspect newest` |
| `capture` | Read the current terminal screen, optionally including scrollback. | `pmux capture worker --lines 80` |
| `reply` | Read the last assistant text from a Claude Code, Codex or Cortex Code transcript. | `pmux reply worker --json` |
| `activity` | Summarize transcript activity for a supported Claude Code, Codex or Cortex Code process. | `pmux activity api` |
| `history` | Search past Claude Code, Codex and Cortex Code sessions. Filter with options such as `--here`, `--agent`, and `--limit`. | `pmux history --here --limit 10` |
| `resume` | Start a new pmux process that resumes a session returned by `history`; an unambiguous ID prefix is enough. | `pmux resume 019a2b` |

`ls` contains only live processes. `history` is the place to find ended agent
conversations, while `attach` combines both views in its interactive picker.

### Control processes and automate workflows

| Command | When to use it | Example |
| --- | --- | --- |
| `send` | Send literal text to a process and press Enter. Use `--no-enter` to leave it unsubmitted or `--stdin` to keep sensitive input out of the process list. | `pmux send api 'run the tests'` |
| `ask` | Send one prompt, wait for agent completion, and print its reply. | `pmux ask worker --prompt-file task.txt --timeout 10m` |
| `wait` | Block the caller until a process exits, a timeout expires, or an explicit `ai-attn` state appears. It does not pause the managed process. | `pmux wait api --until agent:waiting --timeout 30m` |
| `events` | Stream JSON Lines snapshots: the current state, then a new one whenever it changes. | `pmux events --interval 500ms` |
| `queue` | Build, inspect, schedule, pause, resume, or cancel a batch of messages for later delivery. | `pmux queue open api` |

For scripts, prefer `--json` where available and use `send --stdin` when the
input may be secret. See [the automation API](scripting.md#automation-api) for the stable JSON
schema and [Scheduled message batches](message-queue.md) for all `queue`
subcommands.

### Configure shells, wrappers, and restoration

| Command | When to use it | Example |
| --- | --- | --- |
| `init` | Install shell completions and wrapper sourcing. Add `--autostart` to install reboot restoration too. The first run at a terminal asks for a row preset; `--preset NAME` answers in advance. | `pmux init --autostart` |
| `preset` | List the row presets (`*` marks yours), or switch the sidebar and picker to one. See [Customizing the sidebar and picker](sidebar-customization.md#presets). | `pmux preset compact` |
| `wrap` (`w`) | Make an ordinary command automatically launch through pmux in Bash or Zsh. | `pmux wrap codex` |
| `wraps` / `unwrap` | List wrapped commands or remove a wrapper. | `pmux unwrap codex` |
| `snapshot` | Force a fresh capture of live sessions before a planned reboot. | `pmux snapshot` |
| `restore` | Recreate sessions from the most recent snapshot and resume supported agents when possible. | `pmux restore` |
| `doctor` | Check the restore setup, the optional `ai-attn` relay and tmux host-pane mappings, or (`doctor tmux`) both tmux servers for mix-ups. | `pmux doctor tmux` |
| `host-socket` | Inside a pmux process, print the socket of your tmux showing it, to target your tmux rather than pmux's server. | `tmux -S "$(pmux host-socket)" source-file ~/.tmux.conf` |
| `prompt` | Ask in pmux's floating prompt, then run a command with the answer; for your own tmux bindings. See [Floating prompts](customization.md#floating-prompts). | `pmux prompt --title "New branch" -- git switch -c` |
| `equalize-panes` | Even out the panes of a window in your tmux while keeping the pmux sidebar's width; bind it in place of `select-layout`. See [the equalize key](tmux-integration.md#sidebar-options-and-the-equalize-key). | `pmux equalize-panes "#{pane_id}" --socket "#{socket_path}"` |

## Attach picker

- With no name, `pmux attach` opens pmux's built-in picker, newest live process
  first. An empty query shows only live processes, and the built-in fuzzy
  search always works without another tool.
- When `dredge` is on `PATH`, pmux refreshes its local index in the background.
  Typing then searches the full text of Claude Code and Codex history alongside
  live process names, commands, directories, and titles. Rows marked `↻` are
  past sessions; selecting one resumes it in a new pmux process. A missing
  Dredge installation leaves the normal picker unchanged; a Dredge failure
  shows a short status in the prompt and falls back to live-process search.
- Type to filter; `↑`/`↓` (or `Ctrl-p`/`Ctrl-n`) move the selection, `Enter`
  attaches or resumes, `Ctrl-a` cycles the filter (below), and `Esc` closes.
  `[picker.keys]` in `~/.config/pmux/config.toml` rebinds them; see
  [Customizing the sidebar and picker](sidebar-customization.md#the-pickers-keys).
- `Ctrl-a` cycles which rows the picker lists: `all` (live processes, plus
  past sessions matching the query), `live` (only live processes), and
  `past` (only resumable past sessions, newest first, even with an empty
  query). The line under the prompt shows the key and marks the current
  filter.
- The picker needs an interactive terminal. From scripts, pass a live process
  name (`pmux attach <name>`, `newest`, or `oldest`), or a past agent
  session's id: a name that is no running process is resumed like
  `pmux resume <id>` when it names a resumable session.

Install Dredge with its one-line installer (prebuilt binary; no Rust needed):

```sh
curl -fsSL https://raw.githubusercontent.com/cosmicbuffalo/dredge/main/install.sh | sh
```

## Last process

- From a shell, `pmux last` reattaches the process the current tmux pane showed last, or, without
  that history, the most recently attached process.
- Inside a process, `Ctrl-p l` does the same; pressing it again switches back, so it toggles between
  the pane's last two processes.

## Wrappers

This writes a wrapper function to `~/.config/pmux/wrap.sh` and auto-adds a managed source block to your shell rc (e.g. `~/.zshrc` or `~/.bashrc`) so wrappers load automatically. A prompt hook in that block reloads `wrap.sh` when it changes, so later wraps and unwraps take effect at the next prompt in every open shell.

If your `~/.zshrc` is shared (for example a team dotfiles repo) and sources a personal file such as `~/.zshrc.local`, you can move the marked block into that personal file. As long as `~/.zshrc` has no block of its own, `pmux init`, `wrap` and `unwrap` refresh the block where it now lives instead of adding a new one to `~/.zshrc`, and the uninstaller removes it from there. pmux recognizes `source FILE` and `. FILE` lines whose path is literal or starts with `~`, `$HOME` or `${HOME}`.

```sh
pmux wraps             # list currently wrapped commands
pmux unwrap codex      # remove a wrapper
```

**How wrapping works:** The generated wrapper routes the command and all of
its arguments through pmux. When the wrapped process exits, pmux replays the
pane's available scrollback into the calling terminal, so the final output
remains visible as it would for a command run directly in that terminal.

```sh
codex              # → launches in a new pmux session
codex --help       # → launches in pmux and preserves its output on exit
codex --version    # → launches in pmux and preserves its output on exit
```

Inside an existing pmux session, commands still run directly to avoid creating
nested pmux sessions. Detached launches (`pmux run --detach`) are also
unchanged: they do not retain or replay output because there is no attached
terminal to receive it.

> **Note:** The wrapped command name must be a valid shell function name
> (starts with a letter or `_`, contains only letters, digits, `-`, `_`, or `.`).

## Notes and caveats

- `pmux run` always creates a new session; it does not auto-reattach existing sessions.
- `pmux last` first uses pane-local state when pmux knows the current tmux pane, then falls back to `~/.local/state/pmux/attach_history` and older `last_attached` state.
- `pmux restart` is a soft restart (reload config) and keeps sessions alive.
- `pmux restart --hard` kills the pmux tmux server and all pmux-managed processes.
- Different terminal sizes (laptop vs phone) can still affect TUI rendering inside a shared process session.
- `pmux` is designed to isolate process sessions from your main tmux layout, not to clone pane layouts across servers.
- Launch commands stored in `~/.local/state/pmux/launch_cmds/` are plain text. Avoid passing secrets as command-line arguments.
- The first pmux command you run creates `~/.config/pmux/` (generated tmux,
  theme, and status config you can customize) and `~/.local/state/pmux/`
  (private runtime state). `pmux init` additionally edits your shell rc file.
