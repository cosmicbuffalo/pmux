# Changelog

## Unreleased

### Setup

- `pmux setup` sets up everything in one command, and the installer now
  runs it in place of `pmux init`. It runs the shell integration, installs
  ai-attn and Dredge at the releases pmux is tested with (upgrading copies
  their installers put there, never replacing a newer one or one installed
  another way), wires ai-attn into every installed agent, wraps the agent
  CLIs on your `PATH`, and adds the integration to your tmux config. It is
  safe to rerun; `--yes`, `--dry-run`, and `--skip` control it. ai-attn and
  Dredge are no longer optional add-ons to install yourself.
- pmux recognizes GitHub Copilot CLI (`copilot`), Grok Build (`grok`),
  Antigravity CLI (`agy`) and Oh My Pi (`omp`) as agents: `pmux setup` offers
  to wrap them, the sidebar tags them, and `restore`, `respawn` and `resume`
  resume Copilot, Grok and Oh My Pi conversations.
- `pmux setup` installs ai-attn v0.6.0, which adds hooks for those agents
  and for Gemini CLI and Pi.
- The installer's `PMUX_SKIP_SETUP=1` replaces `PMUX_SKIP_INIT=1` (still
  accepted), and `PMUX_SETUP_ARGS` passes options to `pmux setup`.

## 0.1.0 — 2026-11-12

First public release.

### Processes

- Run commands as detachable processes on pmux's own tmux server
  (`tmux -L pmux`), so they survive SSH disconnects and terminal changes
  without touching your tmux: `run`, `attach` (with a picker), `last`,
  `rename`, `kill`, and `ls`.
- `pmux wrap <command>` makes a command start in pmux whenever you run it,
  with shell integration and completions for Bash and Zsh.
- Inside a process, `Ctrl-p` is the prefix for creating, switching,
  renaming, copying the name of, and killing processes, with a `?` popup
  of the keys actually bound.
- pmux's tmux server is customizable in plain tmux or
  [tmux.rb](https://github.com/cosmicbuffalo/tmux.rb), and keeps your edits
  across upgrades.
- A guard keeps a bare `tmux` command inside pmux from reconfiguring pmux's
  server. `pmux doctor tmux` finds mix-ups between the two servers, and
  `pmux restart --repair` undoes them while every process keeps running.

### The sidebar

- `prefix + Ctrl-p` in your own tmux toggles a sidebar of every process,
  live as it changes, with agent state, grouping, folding, a `/` filter,
  and a Resumable group of past agent sessions.
- Enter opens a drawer with an agent's activity timeline: output tokens,
  input tokens per turn, or lines changed.
- Choose what rows show, their colors, outline, and keys in
  `~/.config/pmux/config.toml`, or pick a preset with `pmux preset`.
- Integrates through plain tmux, TPM, or tmux.rb.

### AI agents

- Recognizes Claude Code, Codex, Gemini, OpenCode, pi, and Muse. With
  [ai-attn](https://github.com/cosmicbuffalo/ai-attn), each agent's state
  (working, waiting, done) reaches the sidebar, pmux's status bar, and the
  window tab in your tmux that holds it.
- `pmux history` and `pmux resume` find and reopen past Claude Code and
  Codex conversations; with [Dredge](https://github.com/cosmicbuffalo/dredge)
  the picker searches their full text.
- `pmux respawn` restarts agents in place, resuming their conversations,
  to pick up new config or an upgrade.
- `pmux restore` recreates processes after a reboot and resumes agents in
  their conversations; `pmux init --autostart` does it at login.
- `pmux queue` (`prefix + q`) schedules a batch of messages as one prompt.
- `pmux init --agent-docs` installs a guide that teaches Claude Code, Codex,
  and Gemini to start and drive helpers through pmux.

### Automation

- A versioned JSON API (`api_version: 1`) for scripts and agents: `ls`,
  `inspect`, `events`, `capture`, `send`, `wait`, `reply`, `ask`, and
  `activity`. `ask` sends a prompt, waits for the agent to finish, and
  returns its reply, stopping on approval prompts with the screen included.
- Processes started from a pmux pane record their parent, so an agent can
  list (`ls --children`) and clean up (`kill --children`) its own helpers.

### Installation

- Linux or macOS with tmux 3.3a or newer. `install.sh` installs a
  checksum-verified release binary and checks its GitHub build provenance
  when `gh` is available; building from source needs Rust 1.88 or newer.
