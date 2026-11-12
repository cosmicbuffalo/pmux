# Cross-restart session restoration

pmux can survive a machine reboot: it keeps a snapshot of your live sessions and
recreates them on demand, resuming AI agent conversations where they left off.

- **Snapshot.** pmux records each session's name, cwd, and launch command to
  `~/.local/state/pmux/restore.json`. The snapshot is refreshed automatically as
  sessions are created, renamed, killed, or exit on their own. You can force a
  fresh capture right before a planned reboot with `pmux snapshot`. The
  snapshot is never replaced when the pmux server has disappeared (crash,
  `kill-server`, `restart --hard`) or, on systemd, while the machine or user
  session is shutting down. Before any refresh that drops sessions, the prior
  snapshot is kept in `restore.prev.json` beside it.
- **Agent resume.** For AI agents, pmux also records the native session id (read
  from `ai-attn`) in durable per-session metadata, so a later refresh cannot
  erase it after the attention record expires. Restore relaunches agents with
  their own resume command —
  `claude --resume <id>`, `codex resume <id>`, `opencode --session <id>`,
  `copilot --resume=<id>`, `grok --resume <id>`, `pi`/`omp --session <id>` —
  instead of a fresh conversation. Gemini CLI and Antigravity (`agy`) have no
  confirmed resume command yet. By default, agents without a captured id
  are skipped and make `pmux restore` fail rather than silently opening a fresh
  conversation.
- **Restore.** `pmux restore` recreates snapshotted sessions that aren't
  already running. It's idempotent, so it's safe to run repeatedly. By default
  it resumes agents and recreates plain interactive shells, but does **not**
  re-run other saved commands (a deploy, a migration, a server with side
  effects); it lists how many it held back. Set `relaunch = "all"` to re-run
  everything, or `"agents"` to recreate agents only.
- **Boot autostart.** `pmux init --autostart` writes a `systemd --user` unit
  (Linux) or launchd agent (macOS) that runs `pmux restore` on login/boot, and
  prints the exact command to enable it. This is opt-in.

> **Honest limitation:** a reboot kills the live process. Restored agents come
> back **idle at their prompt with full conversation history — not mid-task**.
> pmux restores continuity of *context*, not of a running turn. (No tool can
> keep a live process alive across a kernel restart.)

Everything here is opt-out-able via `~/.config/pmux/config.toml`:

```toml
[restore]
enabled = true            # keep a fresh snapshot and allow `pmux restore`
resume_agents = true      # rebuild agent conversations after restore
relaunch = "safe"         # non-agent sessions: "safe" (shells only), "agents", or "all"
autostart_on_boot = false # whether `init` installs the boot service
```

Run `pmux doctor restore` to see the current config, snapshot freshness, how
many sessions are resumable, and the boot-service status.

## Restarting agents in place: `pmux respawn`

A running agent keeps the environment and config it started with. After you
fix pmux's server environment, change a hook or settings file, or upgrade the
agent, `pmux respawn` restarts it in its own pane with its resume command, so
the conversation continues under the new setup:

```sh
pmux respawn api                 # one process
pmux respawn api docs tests      # several
pmux respawn --agent claude      # every Claude Code process
pmux respawn --all --dry-run     # show the plan, change nothing
```

- **Same process, same pane.** The pmux process name, pane id, host-pane
  attachment and pane options stay; only the program in the pane is replaced
  (tmux `respawn-pane -k`). It starts from the pmux server's current global
  environment and the recorded working directory, never the caller's
  environment.
- **Resume id.** pmux first asks the running agent itself (Claude Code's
  `sessions/<pid>.json` registry in the config directory that process uses,
  or the thread a Codex process holds open), then falls back to the same
  records `pmux restore` uses. `--resume-id <id>` overrides both for one
  process. `--dry-run` shows each id and where it came from.
- **Safety.** Agents that are `working` or `waiting` are skipped, and so are
  agents with no resume id; `--force` interrupts the first and starts the
  second fresh. Processes that are not agents are skipped by `--agent` and
  `--all`, and relaunched with their recorded command only when named.
  Respawning more than one process asks first; without a terminal it needs
  `--yes`. Run from inside a pmux pane, that pane is respawned last, after
  the report is printed.
- Like restore, the agent is relaunched with its plain resume command
  (`claude --resume <id>`, `codex --no-daemon resume <id>`, ...); other flags
  from its original launch command are not kept.

## Optional: pane scrollback with tmux-resurrect / continuum

pmux's native restore covers sessions, cwds, commands, and agent resume with no
external dependency. If you also want **pane scrollback** restored, install
[tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect) +
[tmux-continuum](https://github.com/tmux-plugins/tmux-continuum) and point them
at the pmux socket. These remain fully optional.

## `PMUX_SOCKET` (testing on a throwaway socket)

pmux talks to its dedicated tmux server on the socket named `pmux` (`tmux -L pmux`), which
is shared across your user. Set `PMUX_SOCKET` to run pmux against a different,
isolated socket — useful for development and for exercising restore without
touching your live sessions:

```sh
PMUX_SOCKET=pmux-test pmux run -d claude
PMUX_SOCKET=pmux-test pmux snapshot
PMUX_SOCKET=pmux-test pmux restore
tmux -L pmux-test kill-server   # tear down the sandbox; never touches -L pmux
```
