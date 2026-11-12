# pmux

`pmux` keeps long-running interactive CLI processes, especially AI agents
like `claude`, `codex`, `cortex`, or `gemini`, alive on their own dedicated
tmux server, separate from your tmux. Detach from one, let it keep working,
and reattach from any terminal or device you SSH in from, or switch between
several from one stable pane, all without touching your tmux layout.

In practice: wrap your agents once (`pmux wrap claude`), keep launching them
the way you always have, press `Ctrl-p d` to detach, and reattach any time
with `pmux a`. With the tmux integration, `prefix + Ctrl-p` opens a sidebar
listing every process and what each agent is doing.

![pmux in tmux: the sidebar follows agents as they work, a helper agent appears under the agent that spawned it, an agent waiting for permission moves to the top and its tab flashes, Tab jumps to it, and the picker attaches another conversation](docs/images/demo.gif)

## Contents

- [Requirements](#requirements)
- [Install](#install)
- [First-time setup](#first-time-setup)
- [Typical usage](#typical-usage)
- [The pmux sidebar](#the-pmux-sidebar)
- [What setup installs](#what-setup-installs)
- [Documentation](#documentation)
- [Development](#development)
- [License](#license)

## Requirements

- tmux 3.3a or newer
- Bash or Zsh for command wrappers (Fish gets completions with
  `pmux completions fish`, but no wrappers)
- `curl` or `wget`, and `sha256sum` or `shasum`, for the installer
- Rust 1.88 or newer, only to build from source

`pmux setup` installs the [companion tools](#what-setup-installs) for you.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/cosmicbuffalo/pmux/main/install.sh | sh
```

<details>
<summary>What the installer does</summary>

- Downloads the release binary for your platform (Linux or macOS), checks it
  against the release's SHA-256 checksums, and installs it to
  `~/.local/bin/pmux`. Make sure `~/.local/bin` is on your `PATH`.
- Release binaries carry GitHub build provenance. With an authenticated
  [`gh`](https://cli.github.com/) on `PATH`, the installer verifies it with
  `gh attestation verify` and refuses a binary that fails; without `gh`, it
  prints the command to run yourself. `PMUX_VERIFY_ATTESTATION=1` makes the
  check mandatory and `0` skips it.
- Runs [`pmux setup`](#first-time-setup), which sets up your shell,
  installs ai-attn and Dredge, and offers to wrap your agents and connect
  your tmux. `PMUX_SKIP_SETUP=1` skips it, and
  `PMUX_SETUP_ARGS="--yes"` (or `"--skip dredge"`, and so on) passes it
  options. The uninstaller removes pmux's block from your shell rc file.
- Checks your tmux is 3.3a or newer. If yours is new enough but reports an
  unusual version string, `PMUX_SKIP_TMUX_CHECK=1` skips the check.
- `PMUX_VERSION=v0.1.0` pins a release instead of the latest.
- Read the full script at [install.sh](install.sh).

</details>

### Building from source

```sh
git clone https://github.com/cosmicbuffalo/pmux && cd pmux && make install
```

This builds pmux, installs it to `~/.local/bin/pmux`, and runs `pmux init`.

### Uninstall

```sh
~/.local/share/pmux/uninstall.sh
```

This stops pmux's tmux server, removes the restore boot service, the binary,
its config and state, and the shell rc block, asking first if processes are
running. `--keep-data` keeps config and state.

## First-time setup

The installer runs `pmux setup`, so after installing you are usually done.
It is safe to run again at any time: each step checks what is already in
place and only does what is missing.

```sh
pmux setup            # asks before wrapping agents or editing your tmux config
pmux setup --yes      # accepts every default
pmux setup --dry-run  # shows what it would do
```

It goes through these steps:

1. **Shell integration**: completions and wrapper sourcing in a marked
   block of your shell rc file, plus a row preset and a short guide that
   teaches your agents to use pmux (a Claude Code skill and a section in
   Codex's and Gemini's global instructions; `pmux init --no-agent-docs`
   removes it).
2. **ai-attn**: installs the [release pmux is tested
   with](#what-setup-installs) and wires its hooks into every agent you
   have installed, so pmux knows when an agent is working, waiting, or done.
3. **Dredge**: installs it for full-text search of past sessions.
4. **Wrap your agents**: for each agent CLI on your `PATH` (`claude`,
   `codex`, `gemini`, ...), typing the plain command then starts it inside
   pmux. `pmux unwrap <command>` undoes one.
5. **Connect your tmux**: adds
   `source-file -q ~/.config/pmux/outer-tmux.conf` to your tmux config, for
   the process sidebar on `prefix + Ctrl-p` and attention indicators on your
   window tabs, and loads it into your running tmux. With a
   [tmux.rb](https://github.com/cosmicbuffalo/tmux.rb) config it tells you
   the line to add instead, and TPM users can load pmux as a plugin: see
   [Integrating pmux with your tmux](docs/tmux-integration.md).

`--skip ai-attn,dredge,wrap,tmux` leaves steps out, and `--autostart` also
sets up [restoring your processes after a reboot](#after-a-reboot). Without a
terminal (for example `curl ... | sh` in a script), setup does what needs no
answer and lists the rest for you to finish.

If your `~/.zshrc` belongs to someone else, such as a team dotfiles checkout
that sources every file in `~/.zshrc.d/`, keep pmux's shell block in a file
of its own:

```sh
pmux setup --rc-file ~/.zshrc.d/pmux
```

pmux saves the choice (`rc_file` under `[shell]` in
`~/.config/pmux/config.toml`), so later `init`, `setup`, `wrap` and
`unwrap` runs keep using that file, and it moves an existing block out of
your old rc file. `pmux init` takes the same option.

## Typical usage

Once your agents are wrapped, use them exactly as you did before. Typing
`claude` in any terminal starts it in a pmux process, and from then on it
keeps running whether or not you're attached to it.

### Inside a process: the pmux prefix

Inside a pmux process, **Ctrl-p** is the pmux prefix. The keys you will
reach for most:

| Keys | What it does |
| --- | --- |
| `Ctrl-p` `d` | Detach from the current process |
| `Ctrl-p` `s` | Switch to another process from the picker |
| `Ctrl-p` `l` | Switch back to this pane's previous process |
| `Ctrl-p` `,` | Rename the current process |
| `Ctrl-p` `?` | Show every pmux key |

![The key hints popup opened with Ctrl-p ? inside a process](docs/images/keys.png)

These keys and the rest of pmux's tmux server are yours to change: see
[Customizing pmux's tmux server](docs/customization.md).

### From a terminal

| Command | What it does |
| --- | --- |
| `pmux a` | Attach from the picker. Type to filter; with [Dredge](#what-setup-installs) it also searches past Claude Code and Codex sessions, and picking one (`↻`) resumes it. |
| `pmux l` | Reattach the process this pane showed last |
| `pmux ls` | List running processes with their command, agent state, directory, and attached clients |
| `pmux run <cmd>` | Start a one-off process without wrapping the command |
| `pmux kill <name>` | Stop a process for good |

![The attach picker listing live processes with their agent state](docs/images/picker.png)

`pmux help` lists the rest, and the [command reference](docs/commands.md)
documents every command.

### After a reboot

pmux keeps a snapshot of your processes. `pmux restore` brings them back,
with agent conversations resumed at their prompt, and
`pmux init --autostart` does it at every login. See
[Cross-restart session restoration](docs/restore.md).

## The pmux sidebar

With the tmux integration from [First-time setup](#first-time-setup),
`prefix + Ctrl-p` toggles a sidebar listing every live pmux process
(shells, services, and agents alike) with each agent's state when
`ai-attn` is installed. Below them, a folded **Resumable** group lists past
agent sessions. Press Tab (or double-click) on a process to jump to it, or
to attach it in a new split; click an entry for its action menu. Its keys
are Vim-like, and `?` lists them.

![The pmux sidebar in tmux beside an attached Claude Code process, listing agents that are working, waiting, and done](docs/images/sidebar.png)

[The pmux sidebar](docs/sidebar.md) describes everything it shows. Pick a
row preset with `pmux preset`, or choose what rows show, their colors, and
their keys: see [Customizing the sidebar and picker](docs/sidebar-customization.md).

## What setup installs

`pmux setup` installs these companion tools, each pinned to the release
pmux is tested with. It never replaces a newer copy, and leaves alone a copy
you installed some other way (such as with Cargo or Homebrew). Each
installer checks its download's checksum.

| Tool | What it adds to pmux |
| --- | --- |
| [ai-attn](https://github.com/cosmicbuffalo/ai-attn) | Agent state (working, waiting, done) in the sidebar, the pmux status bar, and your tmux window tabs, plus `Ctrl-p a` to clear a signal. pmux installs its tmux companion, [tmux-ai-attn](https://github.com/cosmicbuffalo/tmux-ai-attn), for you. |
| [Dredge](https://github.com/cosmicbuffalo/dredge) | Full-text search of past Claude Code and Codex sessions in `pmux a` and `Ctrl-p s`. |

pmux still runs without them (skip either with `--skip`), but then it
doesn't show agent state or search past sessions.
[Integrating pmux with your tmux](docs/tmux-integration.md#check-that-it-works)
shows how to check they work.

## Documentation

**Setting up**

- [Integrating pmux with your tmux](docs/tmux-integration.md): the sidebar
  and attention indicators, in plain tmux, TPM, or tmux.rb
- [Two tmux servers, safely](docs/two-servers.md): how pmux keeps your tmux
  and its own server apart, and how to recover if they get mixed up
- [Troubleshooting](docs/troubleshooting.md)

**Using pmux**

- [The pmux sidebar](docs/sidebar.md)
- [Cross-restart session restoration](docs/restore.md)
- [Scheduled message batches](docs/message-queue.md)

**Customizing**

- [Customizing pmux's tmux server](docs/customization.md): its keys, theme,
  and the files pmux generates
- [Customizing the sidebar and picker](docs/sidebar-customization.md)

**Reference**

- [Command reference](docs/commands.md)
- [Scripting and agents](docs/scripting.md): the JSON automation API, and
  the guide pmux installs for your agents
- [AGENTS.md](AGENTS.md): setup checklist and architecture for AI agents

## Development

Build with `cargo build` and run `cargo test`; [CONTRIBUTING.md](CONTRIBUTING.md)
has the full checks CI runs and how the screenshots are rendered.

## License

Apache License 2.0. See [LICENSE](LICENSE) for details.
