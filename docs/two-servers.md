# Two tmux servers, safely

pmux runs your processes on its own dedicated tmux server, next to your tmux.
That keeps them alive when terminals come and go, but it means a program
running inside a pmux process — you in an agent's shell, or the agent itself —
has `$TMUX` pointing at pmux's server, not yours. pmux protects that boundary
so a mix-up never costs you your processes:

- **A guarded `tmux` command inside pmux.** In a pmux process, commands that
  would reconfigure or end pmux's server — `tmux source-file ~/.tmux.conf`,
  `tmux set -g …`, `bind`, `set-hook`, `kill-server` — are refused, with the
  exact command to run against your own tmux instead. Everything else works
  as usual. To change your tmux from inside pmux, name its socket:
  `tmux -S "$(pmux host-socket)" source-file ~/.tmux.conf`. Set
  `PMUX_ALLOW_TMUX=1` to let a command through on purpose.
- **Configs that only load where they belong.** pmux's
  `~/.config/pmux/tmux.conf` does nothing but warn if it is loaded into your
  tmux, and the [integration for your tmux](tmux-integration.md)
  (`outer-tmux.conf` and `pmux tmux-init`) does nothing but warn if it is
  loaded into pmux's server.
- **Noticed when it happens anyway.** If key bindings reach pmux's server from
  outside its config, pmux says so on screen, and the
  [sidebar](sidebar.md) header reads "pmux config changed: run pmux
  repair".
- **`pmux restart --repair` undoes it.** It resets pmux's server to tmux's defaults and
  reloads pmux's config. Every process keeps running.
- **`pmux doctor tmux` checks the whole setup:** a config in the wrong server,
  a prefix both servers use (your tmux would swallow `Ctrl-p`), tmux-ai-attn
  loaded twice, or a server still on an older tmux after an upgrade.
- **Even `kill-server` is recoverable.** pmux keeps a snapshot of your
  processes, and `pmux restore` brings them back with agent conversations
  resumed (see [Cross-restart restoration](restore.md)).

