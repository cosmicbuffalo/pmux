# Customizing pmux's tmux server

pmux's dedicated tmux server (`-L pmux`) has its own config, separate from
your tmux. Put keys and behavior in `~/.config/pmux/tmux.conf` and colors in
`~/.config/pmux/theme.conf`, then run `pmux restart`; every process keeps
running, and pmux keeps your edits across upgrades. If you write tmux config
in Ruby, use [tmux.rb](#tmuxrb) instead. A key you add with a note appears in
the `prefix + ?` popup:

```tmux
bind -N "git: open lazygit" g display-popup -E -w 90% -h 90% lazygit
```

The sidebar and the attach picker are configured separately, in
`~/.config/pmux/config.toml`: see
[Customizing the sidebar and picker](sidebar-customization.md). The
integration in your own tmux has its own options in
[Integrating pmux with your tmux](tmux-integration.md#sidebar-options-and-the-equalize-key).

pmux starts its server with a managed `~/.config/pmux/server.conf`, which
marks the server as pmux's and then sources `tmux.conf`. The guard lines at
the start and end of `tmux.conf` check that mark, so the file only warns if it
is ever loaded into your own tmux; keep your settings between them. Reload
with `prefix + r` or `pmux restart` (both go through `server.conf`), not by
sourcing `tmux.conf` directly.

## Defaults

The default config sets a `C-p` prefix, mouse mode, vi copy-mode keys,
emacs-style prompt keys (so `Esc` cancels a command prompt immediately), the
ai-attn status rendering, and lifecycle hooks that keep the
[cross-restart snapshot](restore.md) current. Its keys, all after the prefix:

| Key | Action |
| --- | --- |
| `c` | Ask in a floating prompt for a command to run in a new process (empty for a shell) |
| `C` | New process running `$SHELL` (or `/bin/sh`) in the current directory |
| `s` | Switch process from the picker |
| `n`, `p` | Next or previous process |
| `l` | Switch back to this tmux pane's previous process |
| `,` | Rename the current process in a floating form |
| `q` | Open the [scheduled message queue](message-queue.md) |
| `y` | Copy the current process's name (as `pmux ls` shows it) to the clipboard |
| `a` | Clear the ai-attn signal for the current pane |
| `k` | Kill the current process and switch to the next |
| `x` | Kill the current process and detach, after a floating confirmation |
| `d` | Detach |
| `r` | Reload the pmux config |
| `?`, `/`, `h` | Show the key hints popup |

`y` copies through OSC 52, from pmux and from your own tmux, then through a
local `pbcopy`/`wl-copy`/`xclip`/`xsel`/`clip.exe` when you are not on SSH,
so it needs no clipboard setup.

The key hints popup lists the keys actually bound on pmux's server, so your
own keys appear and unbound ones drop out; untouched tmux defaults are left
out (`prefix + :` then `list-keys` still shows everything). Give a binding a
note to name it, and a `group:` prefix (`git: open lazygit`) to file it
under a heading. A binding without a note is listed under "Your keys" by its command.

## Generated files

The installer runs `pmux init`, and any later pmux command recreates missing
files in `~/.config/pmux/`:

| File | What it is | Refreshed by pmux | How to customize |
| --- | --- | --- | --- |
| `tmux.conf` | Behavior and keys of pmux's server | Only its marked lifecycle block and guard lines | Edit between the guard lines, outside the block, then `pmux restart` |
| `theme.conf` | Colors and status component styles | Never, once created | Edit, then `pmux restart` |
| `status.conf` | Shell-free label, age, and ai-attn status formats | Whenever the generated config changes | Override `status-left` after its `source-file` line in `tmux.conf` |
| `outer-tmux.conf` | The [integration for your own tmux](tmux-integration.md) | Only its marked integration block | Edit, then `tmux source-file -q ~/.config/pmux/outer-tmux.conf` |
| `tmux.conf.rb` | Only after `pmux init --tmux-rb`; see [tmux.rb](#tmuxrb) | Never | Edit, then `pmux restart` |
| `server.conf`, `rb/pmux.rb`, `bin/tmux`, `new-process.sh`, `help.sh`, `ai-attn-setup.sh` | Server entry point, pmux's tmux.rb module, the [tmux guard](two-servers.md), and helpers for `prefix + c`/`C`, the key hints popup, and the tmux-ai-attn bootstrap | Whenever the generated config changes or one is missing | Copy a helper under another name and point its binding at the copy |
| Shell rc block | Completions and wrappers | By `pmux init`, `wrap`, and `unwrap` | Put your own setup outside the markers |

To use a copied helper, replace its path in `tmux.conf`: `new-process.sh` in
the `bind c` and `bind C` lines, `help.sh` in the `?`, `/` and `h` lines, and
`ai-attn-setup.sh` (also in `outer-tmux.conf` if your tmux loads it). Keep
the copy executable.

## tmux.rb

If you write tmux config with [tmux.rb](https://github.com/cosmicbuffalo/tmux.rb),
configure pmux's server in Ruby instead of editing `tmux.conf`:

```sh
pmux init --tmux-rb
```

This creates `~/.config/pmux/tmux.conf.rb`:

```ruby
require File.expand_path("~/.config/pmux/rb/pmux")

Pmux.server do
  # Your settings for pmux's server, for example:
  global.prefix = "C-a"
  bind "C-a", "send-prefix", note: "pmux: send C-a to the program in this pane"
  bind "g", "display-popup -E -w 90% -h 90% lazygit", note: "git: open lazygit"
end
```

`Pmux.server` is pmux's whole default configuration (the same one compiled
into the plain `tmux.conf`), with your block applied last and still inside the
guard that keeps it to pmux's server. pmux keeps `~/.config/pmux/rb/pmux.rb`
current on every upgrade.

While `tmux.conf.rb` exists, `tmux.conf` is its compiled output: pmux
recompiles it whenever `tmux.conf.rb` changes, on upgrades, and on
`pmux restart`, and never edits it in place, so edit `tmux.conf.rb` instead.
pmux finds tmux.rb as `tmux.rb` or `tmux_rb` on `PATH`, or through
`TMUX_RB_BIN`. Without it, pmux says how to compile by hand and leaves
`tmux.conf` alone. To go back to plain tmux, delete `tmux.conf.rb`.

The module is the source of every config pmux generates; read it at
`~/.config/pmux/rb/pmux.rb`, or [tmux/pmux.rb](../tmux/pmux.rb) in the
repository, to see each setting and reuse pieces such as `Pmux.status` or
`Pmux.process_keys`.

## Floating prompts

pmux asks for input in a floating prompt rather than tmux's command line:
`prefix + c`, renaming with `prefix + ,`, and the kill confirmation on
`prefix + x` all use it. `pmux prompt` opens the same prompt for your own
bindings, then runs a command with the answer:

```tmux
bind B run-shell -b 'tmux display-popup -w 60 -h 9 -E "pmux prompt --title \"New branch\" --action Create -- git switch -c"'
```

The answer is added as the command's last argument, or replaces an argument
that is exactly `{}`; `--split` passes it as separate words, as a shell would
split them. `--confirm "text"` asks yes/no instead and runs the command only on
yes (add `--danger` for a red button). If the command fails, its error shows in
the prompt, which stays open. The popup size (60 by 9) fits the prompt.

In tmux.rb, `Pmux.prompt_popup` builds the binding with that size:

```ruby
bind "B", Pmux.prompt_popup(%q{prompt --title \"New branch\" --action Create -- git switch -c}),
  note: "git: create a branch"
```

## Examples

Keys and behavior go in `tmux.conf`, colors in `theme.conf`.

1. Change the prefix of pmux's tmux server

```tmux
set -g prefix C-s
unbind C-p
bind -N "pmux: send C-s to the program in this pane" C-s send-prefix
```

2. Add your own key, opening lazygit in the pane's directory

```tmux
bind -N "git: open lazygit" g display-popup -E -w 90% -h 90% -d "#{pane_current_path}" lazygit
```

3. Tune status bar for session context

```tmux
set -g @pmux_status_style "bg=#111827,fg=#d1d5db"
set -g @pmux_prefix_chip_off "#[bg=#374151]#[fg=#93c5fd]#[bold] ^p #[default]"
set -g @pmux_hints_compact_off "#[bg=#1f2937]#[fg=#9ca3af] Keys (?) #[default]"
```

4. Mobile-friendly keybindings

```tmux
bind -n M-Right run-shell 'pmux next-process'
bind -n M-Left run-shell 'pmux prev-process'
```

5. Safer terminal defaults for mixed clients

```tmux
set -g default-terminal "screen-256color"
set -g history-limit 100000
```
