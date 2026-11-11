# Agent Instructions for pmux

This file contains instructions for AI agents working within the pmux codebase or with pmux-managed sessions.

## What is pmux

pmux is a process multiplexer that runs detachable CLI sessions on a dedicated tmux server (`tmux -L pmux`), separate from the user's main tmux. It is designed for managing long-running AI CLI sessions (Claude Code, Codex, Gemini) that need to persist across SSH reconnects and device switches. User-facing docs start at [README.md](README.md); every command is in [docs/commands.md](docs/commands.md).

## Setting pmux up for a user

1. Check `tmux -V` and require 3.3a or newer.
2. Install the releases tested together, then wire ai-attn into every
   installed AI CLI:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/cosmicbuffalo/ai-attn/v0.4.0/install.sh \
     | AI_ATTN_VERSION=v0.4.0 bash
   curl -fsSL https://raw.githubusercontent.com/cosmicbuffalo/pmux/v0.1.0/install.sh \
     | PMUX_VERSION=v0.1.0 sh
   "$HOME/.local/bin/ai-attn" setup
   "$HOME/.local/bin/ai-attn" doctor
   ```

   Make sure `~/.local/bin` is on `PATH`. From a checkout, `make install`
   replaces the pmux installer (needs Rust 1.88+).
3. Connect pmux to the user's tmux following
   [docs/tmux-integration.md](docs/tmux-integration.md). For a plain config,
   find the file the user's tmux server actually loads (commonly
   `~/.tmux.conf` or `~/.config/tmux/tmux.conf`) and add
   `source-file -q ~/.config/pmux/outer-tmux.conf` exactly once, near the end.
   Preserve unrelated settings.
4. If the user already has custom `window-status-format` values, keep them:
   remove the two format lines from `outer-tmux.conf` and insert
   `#{E:@ai_attn_window_segment}` before the window name in theirs. If TPM
   already loads tmux-ai-attn, disable the generated bootstrap (see the TPM
   section of that doc).
5. Reload only the user's own tmux config. Never kill either tmux server.
6. Start an agent in pmux and verify:

   ```bash
   pmux doctor ai-attn
   pmux doctor tmux
   tmux show-hooks -g pane-focus-in
   tmux show-option -gqv @ai_attn_last_error
   tmux -L pmux show-option -gqv status-left
   tmux -L pmux show-option -gqv @ai_attn_refresh_client
   ```

   pmux's `status-left` must use native `#{T:@pmux_...}` expansions with no
   `#()` jobs, and its refresh policy must be `off` (the user's own tmux may
   keep `on`). Resolve any reported error.
7. Optional: `pmux init --autostart` and `pmux doctor restore` for reboot
   restoration ([docs/restore.md](docs/restore.md)).

## Architecture

- **Source:** `src/main.rs` calls `pmux::cli_main()` → `cli.rs` (clap dispatch) → `commands/` (command implementations) → `tmux.rs`, `state.rs`, `config.rs`, `agents.rs`, …; the sidebar and pickers live in `tui/`, `render.rs`, `attach_picker.rs`, `queue_ui.rs`
- **Library:** everything lives in the library crate, which exposes only `cli_main()`; modules reach each other through crate-internal `pub(crate) use` re-exports in `lib.rs`. External tools should use the versioned JSON CLI (`pmux ls --json`, `pmux events`)
- **Tmux socket:** All tmux commands use `tmux -L pmux` via `tmux_cmd()` for socket isolation
- **Config:** `~/.config/pmux/` (`tmux.conf`, `theme.conf`, `status.conf`, the `outer-tmux.conf` integration for the user's tmux, and command/plugin helpers) — auto-created on first run
- **State:** `~/.local/state/pmux/` (launch commands, CWDs, attach history, relay PID)

### Session naming

- Current format: `<10-digit-epoch>-<8-alnum>` (e.g., `1774574118-qz0tmz4y`)
- Legacy format: `<prefix>-<epoch>-<4-alnum>`
- `is_auto_generated_name()` uses exact length checks to distinguish auto-generated from user-named sessions

### Key conventions

- `list-panes -a` is used for session listing with deduplication in `list_sessions()`
- `ls` sorts by `last_activity_epoch` descending; the attach picker lists processes by creation time, newest first; the sidebar lists waiting, then working, then the rest, each newest change first, with spawned processes drawn under their parent (`src/tui/app/family.rs`)
- The relay daemon (`pmux relay-ai-attn --loop`) bridges ai-attn state from pmux sessions to the parent tmux

## Supervising agents through pmux

[templates/agent-docs.md](templates/agent-docs.md) is the canonical guide to
starting helpers and driving other agents (`run -d`, `capture`, `send`,
`ask`, `wait`, `reply`, and the outcome/exit-code table); follow it.
[The automation API](docs/scripting.md#automation-api) has signal freshness,
startup budgets, and the JSON schema.

`pmux init --agent-docs` installs that template as a Claude Code skill
(`~/.claude/skills/pmux`) and a marked section in Codex's `AGENTS.md` and
Gemini's `GEMINI.md`. Bump `AGENT_DOCS_VERSION` in `src/agent_docs.rs` when
the template changes.

## Running tmux commands from inside pmux

A process inside pmux has `$TMUX` pointing at pmux's dedicated tmux server,
not the user's tmux. So:

- To change the user's own tmux (reload its config, set options, add
  bindings), name its socket: `tmux -S "$(pmux host-socket)" <command>`.
  Never run a bare `tmux source-file ~/.tmux.conf` or `tmux set -g ...` from
  inside pmux; pmux's tmux guard refuses those and explains why.
- Do not set `PMUX_ALLOW_TMUX=1` unless the user explicitly asks to change
  pmux's own server.
- If pmux's server was changed by mistake (wrong prefix or status line),
  run `pmux restart --repair`; it keeps every process running. `pmux doctor tmux`
  reports mix-ups between the two servers.

## ai-attn integration

pmux integrates with [ai-attn](https://github.com/cosmicbuffalo/ai-attn) to relay AI agent attention signals from pmux sessions to the user's tmux status bar.

- The relay daemon starts automatically when a session is attached
- It watches the ai-attn state directory for changes via filesystem notifications (inotify/kqueue) and syncs immediately when state changes
- Relay records are identified by `agent: "pmux"` — the `session_ai_state()` filter in `session.rs` skips these to avoid matching relay records while still matching direct agent signals
- Relay records use `pane_id` to hold the pane ID in the user's tmux, so tmux-ai-attn routes attention indicators to the correct pane
- `pmux update-host-pane` keeps host-pane mappings current when panes move in the user's tmux
- `pmux doctor ai-attn` reports relay health, host-pane mappings, direct ai-attn records, and relayed pmux records
- For targeted debugging, use `pmux doctor ai-attn <name>`, `pmux relay-ai-attn --dry-run --verbose`, and `pmux relay-ai-attn --restart`
- The user's tmux keeps host-pane mappings current with a `pane-focus-in[100]` hook from the generated `outer-tmux.conf`; prefer sourcing that file over copying pieces of it ([docs/tmux-integration.md](docs/tmux-integration.md#how-attention-reaches-your-tmux) has the hook for a manual setup)

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the build, test, and lint commands.
All tests, clippy with `-D warnings`, and `cargo fmt --check` must pass before
committing. `make reload` reloads the tmux config on the running server.

pmux's tmux configs are generated: edit `tmux/pmux.rb` (tmux.rb), never the
`.conf` files in `templates/`, then run `make templates` (needs the tmux gem,
or `TMUX_RB=/path/to/tmux_rb`) and commit both. `make templates-check` fails
when they drift.
