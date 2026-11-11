# Troubleshooting

**`tmux` not found**
Install tmux: `apt install tmux` / `brew install tmux`.

**Sessions not persisting across SSH reconnects**
Make sure you're running processes inside pmux sessions (`pmux run <cmd>`), not your main tmux. pmux sessions live on the pmux server socket (`tmux -L pmux`), which is independent of any SSH connection as long as the host is running.

**`pmux attach` says "no running pmux sessions found"**
The pmux server starts on-demand when you first run `pmux run`. If you haven't started any sessions yet, there's nothing to attach to.

**Picker shows no output / garbled display on mobile**
Terminal size may be reporting 0. Try `pmux attach <name>` directly, or run `pmux ls` to see session names first.

**`pmux wrap codex` says the command name is invalid**
Wrap command names must be valid shell function names: start with a letter or `_`, and contain only letters, digits, hyphens, underscores, or dots.

**Config changes not taking effect**
Run `pmux restart` to reload `~/.config/pmux/tmux.conf` without disturbing running sessions.

**Typing inside pmux lags while raw shell panes remain responsive**
Verify that `tmux -L pmux show-option -gqv status-left` contains native
`#{T:@pmux_...}` expansions rather than `#()` jobs, and that
`tmux -L pmux show-option -gqv @ai_attn_refresh_client` prints `off`. Run
`pmux restart` to apply the current generated-config migration. Your own
tmux server may keep `@ai_attn_refresh_client` set to `on` for its fast tab spinner.

**A tmux window has no ai-attn indicator**
Make sure your tmux config sources
`~/.config/pmux/outer-tmux.conf`, then run `ai-attn doctor` and
`pmux doctor ai-attn`. Existing attached panes can be mapped immediately with
`pmux update-host-pane --all --socket "$(tmux display-message -p '#{socket_path}')"`.

**`pmux last` does not flip back to the prior process**
`pmux last` is pane-local when pmux knows the current tmux pane. If a custom bind or wrapper bypasses the normal pmux attach flow, pane-local history may not be updated. Use the built-in `prefix + l` binding or `pmux last` from the same pane.

**pmux's prefix, keys, or status bar suddenly look like your own tmux's**
A tmux config meant for your tmux was loaded into pmux's dedicated tmux server,
usually by `tmux source-file ~/.tmux.conf` run inside a pmux process (where
`$TMUX` names pmux's server). Run `pmux restart --repair`: it resets pmux's server and
reloads pmux's config, and every process keeps running. The sidebar header
says "pmux config changed: run pmux restart --repair" while this is the case.

**`tmux` refuses a command inside an agent ("pmux: refusing …")**
That command would have changed or ended pmux's server rather than your own
tmux. The message shows the command to run against your tmux instead —
`tmux -S "$(pmux host-socket)" …` — and `PMUX_ALLOW_TMUX=1 tmux …` runs it on
pmux's server if that is really what you meant. Processes started before
upgrading to a pmux with the guard keep their old PATH until restarted.

**`Ctrl-p` does nothing inside pmux**
Your own tmux probably uses `Ctrl-p` as its prefix too and takes the key
first. `pmux doctor tmux` reports this; change the prefix of either server
(see [Customizing](customization.md)).

**Not sure which server is misconfigured**
Run `pmux doctor tmux` (inside your tmux, or with `--socket PATH`). It checks
both servers and prints a fix for each problem it finds.

**An agent's attention never reaches your tmux**
`pmux doctor ai-attn` checks the relay, the ai-attn binary and state
directory, host-pane mappings, and current relay records, and
`pmux doctor ai-attn NAME` shows why one process is or isn't relayed.
`pmux relay-ai-attn --dry-run --verbose` prints what a one-shot sync would
do, and `pmux relay-ai-attn --restart` restarts the relay without touching
any process.
