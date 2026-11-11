# Integrating pmux with your tmux

pmux runs your processes on its own dedicated tmux server. The integration
connects that to the tmux you work in:

- `prefix + Ctrl-p` toggles the [pmux sidebar](sidebar.md), with its resize,
  focus, and double-click handling
- host-pane tracking, so pmux knows which of your panes shows each process
  (used by the sidebar, `pmux last`, and the attention relay)
- with [ai-attn](https://github.com/cosmicbuffalo/ai-attn) installed, agent
  attention indicators on your window tabs, which also light up when the
  sidebar points at a process in another window

## Choose one way to load it

Pick the one that matches how you manage your tmux config, and use only
that one: each loads the same integration, so combining them loads it twice.

| You manage your tmux config with | Use |
| --- | --- |
| a plain config file | [A plain tmux config](#a-plain-tmux-config) |
| [TPM](https://github.com/tmux-plugins/tpm) plugins | [TPM](#tpm) |
| [tmux.rb](https://github.com/cosmicbuffalo/tmux.rb) | [tmux.rb](#tmuxrb) |

All three produce the same tmux configuration: the plain file pmux writes is
compiled from pmux's tmux.rb module (`tmux/pmux.rb` in this repository).
After choosing, see [Sidebar options and the equalize key](#sidebar-options-and-the-equalize-key).

### A plain tmux config

`pmux init` (run by the installer) writes `~/.config/pmux/outer-tmux.conf`.
Source it near the end of your tmux config and reload. For `~/.tmux.conf`,
one command does both:

```sh
echo 'source-file -q ~/.config/pmux/outer-tmux.conf' >> ~/.tmux.conf && tmux source-file ~/.tmux.conf
```

If your config is `~/.config/tmux/tmux.conf`, use that path in both places.
The line it adds is:

```tmux
source-file -q ~/.config/pmux/outer-tmux.conf
```

`-q` keeps tmux quiet if the file is ever missing, for example after
uninstalling pmux. The file holds four parts:

1. **The integration block**, between `# >>> pmux managed sidebar >>>`
   markers. It runs `pmux tmux-init --apply`, which generates the sidebar
   binding and hooks from the installed pmux, so they never drift out of date.
   pmux refreshes this block on upgrade; leave it as it is.
2. **ai-attn settings**: all four agent states shown, newly seen attention
   kept visible for 30 seconds, and a flash about once a second.
3. **Window tab formats** with the attention indicator before each window
   name.
4. **The tmux-ai-attn bootstrap**, which loads pmux's pinned, verified copy of
   the plugin when ai-attn is installed.

pmux keeps your edits to everything outside the integration block.

#### If you already style your window tabs

The generated file sets `window-status-format` and
`window-status-current-format`, replacing your own. To keep yours, delete
those two lines from `outer-tmux.conf` and put the indicator in your formats,
just before the window name:

```tmux
set -g window-status-format '#[fg=colour244] #I #{E:@ai_attn_window_segment}#W '
```

To also show the sidebar's flash in your tabs, see how `Pmux.window_tab` in
[tmux/pmux.rb](../tmux/pmux.rb) uses `@pmux_flash_window`, or copy the two
compiled formats from [templates/outer-tmux.conf](../templates/outer-tmux.conf).

### TPM

Add pmux with your other plugins, above TPM's `run` line, then press
`prefix + I`:

```tmux
set -g @plugin 'cosmicbuffalo/pmux'
```

TPM installs only the integration itself (the sidebar key, hooks, and
host-pane tracking), not the pmux binary, which you install first. For
attention indicators, also load tmux-ai-attn
(`set -g @plugin 'cosmicbuffalo/tmux-ai-attn'`) and put
`#{E:@ai_attn_window_segment}` in your window tab formats, or copy the
ai-attn settings and tab formats from `~/.config/pmux/outer-tmux.conf`.

The bootstrap takes one load decision per server, serialized by a lock next
to its socket (keyed by socket path and server PID). It re-checks existing
plugin hooks under that lock, gives a synchronous TPM load a **2-second**
grace period on the first attempt, and re-checks again after waiting for the
shared directory lock and immediately before executing the plugin. This
also recognizes TPM using the same plugin directory. A successful load or
recognized existing copy is recorded in `@pmux_ai_attn_loaded`; subsequent
bootstrap runs leave that server's hooks and daemon alone. A missing plugin
or failed verified download leaves the decision open for a later retry.

TPM does not participate in this lock. The grace period cannot guarantee
ownership if TPM loads later than the final check. When TPM manages
`tmux-ai-attn`, disable the generated bootstrap; in tmux.rb use
`Pmux.ai_attn_options` instead of `Pmux.ai_attn`. That makes TPM the only
loader regardless of its speed. The bootstrap does not arbitrate unrelated
external plugin loaders.

The shared plugin-directory lock is held through verification and plugin
execution, so another server cannot replace the directory while it is being
loaded. Setup waits at most about 30 seconds for the server lock and 120
seconds for the directory lock, then logs and skips loading instead of
proceeding unlocked. Normal exit and termination signals release owned locks.
A hard kill can leave a lock behind; it is deliberately not stolen by age,
because a slow download may still be using it. Only remove an abandoned lock
named in `hooks.log` after confirming no setup process owns it. The server
ownership option lasts for that server's lifetime; bootstrap reloads do not
upgrade or re-execute an already loaded plugin.

### tmux.rb

pmux writes its tmux.rb module to `~/.config/pmux/rb/pmux.rb` and keeps it up
to date. Require it from your tmux.rb config and use what you want:

```ruby
# ~/.tmux.conf.rb
require File.expand_path("~/.config/pmux/rb/pmux")

Tmux.configure do
  # ...your own configuration...

  # The sidebar binding and hooks, generated by the installed pmux.
  append Pmux.integration

  # ai-attn settings and the bootstrap. If TPM loads tmux-ai-attn, use
  # append Pmux.ai_attn_options instead, leaving plugin loading to TPM.
  append Pmux.ai_attn

  # Window tabs with the attention indicator and the sidebar's flash.
  window.status.format = Pmux.window_tab
  window.status.current_format = Pmux.window_tab(current: true)
end
```

Compile it as usual, then reload tmux:

```sh
tmux.rb build ~/.tmux.conf.rb --output ~/.tmux.conf
```

`Pmux.window_tab` takes style hashes for each part, so the tabs can match
your theme:

```ruby
window.status.format = Pmux.window_tab(
  index: {fg: :colour250, bg: :colour237},
  name: {fg: :white, bg: :colour235}
)
window.status.current_format = Pmux.window_tab(
  current: true,
  index: {fg: :black, bg: :colour39, attrs: :bold},
  name: {fg: :white, bg: :colour238},
  flash: {fg: :black, bg: :cyan}   # while the sidebar points here
)
```

Three optional extras round out a full tab: `gap: true` puts a space
before the name when a window has no indicator, `zoom: {fg: ...}` shows a
zoomed window's name as `(name)` in that style, and `sync: {fg: ...}` adds a
`(sync)` marker while panes are synchronized.

Note that the module's methods return configurations, so they go through
`append` (tmux.rb's `use` is for objects with a `to_config` method).

The module also has the pieces individually (`Pmux.ai_attn_options`,
`Pmux.ai_attn_bootstrap`) and everything pmux's own server uses; see
[tmux/pmux.rb](../tmux/pmux.rb). To configure pmux's own server with tmux.rb
too, see [Customizing pmux's tmux server](customization.md#tmuxrb).

When pmux upgrades, it rewrites `~/.config/pmux/rb/pmux.rb`. The integration
itself comes from the installed pmux at runtime, so your tmux keeps working;
rebuild your config to pick up changes to the tab formats or ai-attn
settings.

## Sidebar options and the equalize key

Set these after the integration, in either syntax.

| Option | Values | Default |
| --- | --- | --- |
| `@pmux_sidebar_side` | `left`, `right` | `left` |
| `@pmux_sidebar_open_focus` | `preserve` (stay where you are), `sidebar` (move into it) | `preserve` |

`sidebar` also accepts `jump` or `focus`; any other value means `preserve`.
tmux's layout commands would also resize the sidebar, so bind
`pmux equalize-panes` to even out your panes while it is open. It keeps the
window's arrangement and splits each level evenly between its direct
children:

```tmux
set -g @pmux_sidebar_side right
bind e run-shell 'pmux equalize-panes "#{pane_id}" --socket "#{socket_path}"'
```

```ruby
global[:@pmux_sidebar_side] = "right"
bind "e", %q{run-shell 'pmux equalize-panes "#{pane_id}" --socket "#{socket_path}"'},
  note: "Even out panes, keeping the pmux sidebar's width"
```

## How attention reaches your tmux

AI agents inside pmux write ordinary ai-attn records, but those name a pane
on pmux's server, which your tmux can't see. pmux tracks which pane in your
tmux is showing each process and republishes each record for that pane, so
tmux-ai-attn lights up the right window tab without knowing pmux exists.
Detach a process and attach it somewhere else, and its indicator moves with
it.

The mapping updates on `pmux attach`, `pmux last`, and `prefix + n`/`p`, and
the integration adds a `pane-focus-in` hook so it follows you between panes.
Without the integration, add the hook yourself (adjust the path if pmux is
not in `~/.local/bin`):

```tmux
set-hook -g 'pane-focus-in[100]' "run-shell -b '\"$HOME/.local/bin/pmux\" update-host-pane \"#{pane_id}\" --socket \"#{socket_path}\" >/dev/null 2>&1 || true'"
```

Claude background agents can run hooks outside their terminal, so their
records may name no pane. pmux routes such a record by agent and directory
only when exactly one pmux process matches, and ignores it otherwise rather
than light up the wrong window.

pmux recognizes `codex`, `claude`, `gemini`, `opencode`, `muse`, and `pi` as
agents. Another AI CLI still gets attention indicators if its ai-attn hooks
are set up, but not the agent-only features such as the `?` marker in
`pmux ls`. Issues and pull requests adding agents are welcome.

## Check that it works

Start an agent with `pmux run claude` (or `codex`), and from inside it run
`ai-attn test`: the pmux status bar and that window's tab in your tmux should
react. From a normal pane in your tmux, `pmux doctor ai-attn` should report a
running relay, the process's pane on pmux's server, the host pane and socket
in your tmux it is mapped to, and an `agent=pmux` relay record. `pmux doctor
tmux` checks both servers' configs.

If processes were already attached before you loaded the integration, map
them without restarting anything:

```sh
pmux update-host-pane --all --socket "$(tmux display-message -p '#{socket_path}')"
```

## Keeping the two servers apart

The integration belongs in your tmux only; see
[Two tmux servers, safely](two-servers.md) for how pmux keeps the two apart.

## What pmux changes in your tmux

Only what you load. pmux owns the socket of its own server and the files
under `~/.config/pmux` and `~/.local/state/pmux`; your tmux gets the
integration only through the one line you add, which leaves the rest of your
config alone. It needs no plugin manager, theme, or helper scripts of yours.
Two cases need a one-time choice, both covered above: merging
`#{E:@ai_attn_window_segment}` into window formats you already have, and
picking one loader when TPM already loads tmux-ai-attn.

Bindings pmux generates call `"${PMUX_BIN:-pmux}"`, and pmux sets `PMUX_BIN`
on its server to the binary that started it, so they work wherever pmux is
installed, even when the server inherited an old `PATH`. Your own bindings
that call pmux can use the same form.
