# Using pmux from an agent

You may be running inside pmux, a process multiplexer that keeps CLI sessions
(agents, shells, servers, tests) alive on a dedicated tmux server. Inside a
pmux pane `$TMUX` names pmux's server, not the user's tmux. `pmux ls` lists
processes; `pmux ls --here` lists the ones working in this directory or below
it, such as other agents in the same repo, so check it before large edits.
`pmux inspect NAME` shows one, including `spawned by` when another process
started it.

## When to use pmux

Start a pmux helper for work that runs long, that the user may want to watch
or take over (they can attach to it), or that needs a different agent CLI
than yours. For a quick, self-contained lookup, your own built-in subagents
are simpler.

## Start a helper

```sh
pmux run -d -s <name> --cwd <dir> <command> [args...]
```

- `-d` starts it detached and prints its name. Put pmux options before the
  command; everything after the command is passed to it unchanged.
- Pick a short, unique name that says what it does, such as `auth-tests`,
  `review-api-docs` or `codex-migrate-db`. Avoid `test`, `agent` or `helper`.
  An existing name is refused.
- When you run this from inside a pmux pane, your process is recorded as the
  helper's parent, and the user's apps show it under you. Pass `--no-parent`
  for a process that isn't your helper (one the user asked for that should
  stand on its own). `pmux parent NAME` shows or fixes the link.
- Launch agents (`claude`, `codex`) without a prompt, then talk to them as
  shown below.

## Drive another agent

```sh
pmux capture worker                    # read the screen: startup or approval dialogs
pmux send worker --key Escape          # answer a dialog with named keys
pmux ask worker --prompt-file task.txt --timeout 10m --capture-on-block --json --compact
```

`ask` sends once, waits for the agent to finish, and returns its reply. It
needs ai-attn signals. Use `--stdin` or literal text instead of a file, and
put `--` before a prompt that starts with `-`.

| Outcome | Exit | Next step |
| --- | --- | --- |
| `matched` | 0 | Read `reply.text`. If `reply.available` is false, run `pmux reply worker --json` again. |
| `blocked` | 2 | Read `process.agent.reason` and `capture.text`, answer the question, then `wait`. |
| `not_ready` | 3 | `capture` and clear the startup or approval screen. Run `pmux doctor ai-attn` if signals never arrive. |
| `timed_out` | 1 | `capture`, then `wait` again if it is still working. |
| `exited` | 1 | The process ended. Check its output before you start another. |

After answering a block, don't repeat `ask`, because that sends the prompt
again. Resume like this:

```sh
pmux send worker --no-enter y          # a single-key approval
pmux wait worker --until agent:done --timeout 10m --capture-on-block --json --compact
pmux reply worker --json
```

`pmux send worker --prompt-file f --confirm --capture-on-block --json`
sends and returns once the agent starts working on it, with the same
outcomes as `wait` (`blocked`, or `not_ready` for a startup dialog), without
waiting for the turn to finish. Plain `send` only confirms that the input
was typed, so check the screen or signal afterwards.

`pmux wait A B C --all --until agent:done --json` waits on several helpers
together and stops early when one is blocked (a helper with no signal yet
is watched, not reported, until the timeout); `--any` returns the first one
that finished or needs you, named in `decided_by`.

`--key BSpace --repeat 12` and `--key C-u` edit the input line.

## Rules

- Touch only processes you started. Never send to, rename, kill or respawn a
  process you didn't create, even one with a similar name, unless the user
  asks you to.
- `pmux ls --children` lists only the helpers you started (`--descendants`
  includes theirs). Use it, not a name match on `pmux ls`, to find them.
- To restart one of your helpers with different flags, keeping its name,
  pane and conversation, use `pmux respawn NAME -- <new flags>` rather than
  kill plus run. `pmux inspect NAME --json` shows its `agent.session_id`.
- Clean up: `pmux kill NAME` each scratch process when you are done with it,
  or `pmux kill --children` for all of them, and say which ones you leave
  running and why.
- Leave the user's approval and auto-mode settings alone.
- To change the user's own tmux from inside pmux, use
  `tmux -S "$(pmux host-socket)" ...`. A bare `tmux set -g` or `source-file`
  would hit pmux's server, and pmux refuses it. Don't set `PMUX_ALLOW_TMUX`.
- More detail: `pmux help`, `pmux <command> --help`.
