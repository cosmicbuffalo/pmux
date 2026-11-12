# Scripting and agents

Beyond the everyday commands, pmux has a local JSON/CLI surface for scripts
and for AI agents that drive pmux themselves:

| Command | What it does |
| --- | --- |
| `ls --json`, `inspect NAME --json` | A versioned snapshot of every process, or of one |
| `events` | A JSON Lines stream: the current snapshot, then a new one on every change |
| `run -d -s NAME CMD` | Start a process detached; inside a pmux pane it is recorded as your helper (`ls --children`) |
| `send` | Type literal text or named keys into a process |
| `capture` | Read its screen, including startup and approval dialogs |
| `wait` | Block until a process exits or an agent reaches a state |
| `reply` | Read the last Claude Code, Codex, or Cortex Code assistant message from its transcript |
| `ask` | Send a prompt, wait for the agent to finish, and return its reply |

After checking startup with `capture`, one command runs a turn:

```sh
pmux ask worker --prompt-file task.txt --timeout 10m --capture-on-block --json --compact
```

`ask` sends the prompt, waits for `agent:done`, and returns the reply. Both
`ask` and `wait --until agent:done` stop on `blocked` (exit 2) or `not_ready`
(exit 3); timeout and early exit return 1, and a match returns 0. On blocking,
read the included screen, answer with `send --no-enter` or `send --key`, then
resume `wait` and read `reply`; repeating `ask` sends another prompt.
Completion requires [ai-attn](https://github.com/cosmicbuffalo/ai-attn)
signals. JSON reports unavailable transcript replies explicitly.

## Teaching your agents to use pmux

`pmux init --agent-docs` installs a short guide to all of this, from
[templates/agent-docs.md](../templates/agent-docs.md), as a Claude Code skill
and as a marked section in Codex's `AGENTS.md` and Gemini's `GEMINI.md`. It
tells your agents when a pmux helper beats a built-in subagent, how to start
and drive one, and the rules: touch only their own helpers, and clean them
up. The installer offers it once; `pmux init --no-agent-docs` removes it.
pmux keeps the installed copies current on later `pmux init` runs and leaves
text it didn't write alone.

To have an AI agent set pmux up for you, point it at
[AGENTS.md](../AGENTS.md), which carries an end-to-end setup checklist and
the architecture notes an agent needs.

Every command and its options are in the [command reference](commands.md).

## Automation API

pmux has a local, versioned JSON interface for scripts, skills, sidebars, and
other clients. It models every live pmux session as a general process; an
optional `agent` object appears only when pmux recognizes an AI command. A
missing agent object, or an agent `state` of `null`, is normal and does not
mean the process is unhealthy.

```sh
# All live processes, including shells, REPLs, test commands, and agents.
pmux ls --json

# Only processes working in a directory or below it, e.g. other agents in a
# repo before you edit it (same matching as `history --cwd/--here`).
pmux ls --cwd ~/src/project
pmux ls --here --json

# One process; `i` is a shortcut for inspect.
pmux inspect build --json
pmux i build --json
```

Human output from `inspect` limits the launch command to
120 terminal columns and marks truncation. Use `--full-command` to show the
entire command; terminal control sequences are still stripped. Default JSON retains full command strings. Human agent state timestamps
are shown as relative ages (for example `done, 25m ago`).

For less command noise in JSON, use `--compact` with `ls`, `inspect`,
`wait`, or `ask` (requires `--json`):

```sh
pmux inspect build --json --compact
pmux ls --json --compact
pmux wait codex-fix --until agent:done --json --compact
```

Compact output omits `start_command` from each process object, including
`wait` and `ask`'s nested `process`. It also limits `launch_command` to 120 Unicode
characters (including a trailing `…`) and adds `launch_command_truncated: true`
when shortened. The marker is absent for untruncated or null commands. Use
`inspect NAME --full-command` or default JSON for the complete command.
Other fields and envelopes are unchanged.
Default JSON retains `start_command` for API v1 compatibility.

The current schema is `api_version: 1`. `ls --json` returns
`{api_version, generated_at, sessions: [...]}`, `inspect --json` returns one
process object plus `api_version`, and each `events` line is
`{api_version, event: "state", emitted_at, state}` wrapping a full snapshot.
Each process object has these fields (times are Unix epoch seconds):

| Field | Type | Meaning |
|-------|------|---------|
| `name` | string | pmux process name |
| `pane_id` | string | pane id on pmux's dedicated tmux server (not your own tmux) |
| `lifecycle` | string | `"running"` for every live process |
| `created_at` | number | when the process started |
| `last_activity_at` | number | last output in its pane |
| `attached_clients` | number | terminals currently attached |
| `current_command` | string | the pane's foreground command |
| `start_command` | string or null | low-level tmux pane command, with tmux shell quoting; omitted with `--compact` |
| `current_path` | string or null | the pane's current directory |
| `launch_command` | string or null | original shell-quoted command given to `pmux run`, without the completion supervisor |
| `launch_cwd` | string or null | the directory it was launched from |
| `agent` | object or null | present only for a recognized AI command |
| `startup` | optional object | present for an agent without a current-launch ai-attn signal |
| `spawned_by` | optional object | the process that started this one; omitted when there is none |

`spawned_by` is `{process, created_at, machine, url}`. For a local parent
`machine` and `url` are `null`, `created_at` is the parent's `created_at`, and
it is reported only while that same parent process (name and `created_at`) is
running, so a child of an ended parent stands alone and a later process reusing
the parent's name never adopts it. `process` follows the parent's renames. For
a parent on another machine, `machine` and `url` name that machine and the
base URL recorded for it, `created_at` may be `null`, and the record is reported as
given. Human `inspect` output shows it as `spawned by NAME` or
`spawned by NAME on MACHINE`.

Prefer `launch_command` for the original launch text. It is a shell command
string, not an argv array, and may be `null` for sessions without saved launch
metadata. `start_command` is a diagnostic field: it may include pmux's
completion supervisor and extra tmux quoting, so it can look like a second,
more heavily escaped copy of the launch command. Neither field is a JSON
string containing another JSON document; parse the output once with a JSON
parser. Compact output does not fall back to `start_command` if
`launch_command` is `null`.

`agent` has `kind` (`claude`, `codex`, `copilot`, `cortex`, `gemini`, `agy`,
`grok`, `opencode`, `muse`, `pi`, or `omp`; more may be added), and `state`, `updated_at`, and `source`, which are
all `null` unless a matching direct ai-attn record exists. `state` is passed
through from ai-attn unchanged (currently values such as `working`,
`waiting`, and `done`), so treat it as an opaque label. pmux does not infer
agent state from terminal text. `inspect` alone also adds `session_id`, the
agent's own conversation id (what `claude --resume` or `codex resume`
takes), found from the live process or pmux's records; it is omitted when
unknown and never appears in `ls` or `events`.

A recognized agent without a current-launch signal has `startup` with
`started_at` (Unix epoch), `status` (`starting` for the first five seconds,
then `not_ready`), and `capture_command`. Human inspect/state output reports
the time since launch and suggests checking startup or approval screens with
capture. This means readiness is **unconfirmed**, not that pmux recognized a
particular screen. By default pmux does not guess readiness from terminal text
or treat a transcript/resume identity as proof the input loop is ready.

#### Opt-in ready text

Some agents send no signal until their first prompt. Codex, for example, runs
no hooks while a fresh or resumed session waits at its prompt, so it stays
`not_ready` however long it sits there. To confirm such an agent by its screen,
give pmux text that appears only when the agent is at its prompt, per agent
kind, in `~/.config/pmux/config.toml`:

```toml
[agents.codex]
ready_text = "Ask Codex to do anything"   # or a list of texts
```

While an agent of that kind has no current-launch signal, pmux checks its
visible screen (not scrollback) for any of the texts on one line. On a match,
`startup.status` becomes `ready`, with `ready_at` and `source: "screen"`;
human state prints `agent state: ready (prompt seen … after launch; no agent
signal yet)`. The match is kept for that launch, so pmux stops capturing the
pane; respawn and restore start over. Agent-state `wait`, and so `ask`, no
longer stop with `not_ready` for a ready agent, though they still wait for
the state they asked for.

While the ready text is configured but not on screen, `startup` also has
`ready_text_seen: false` (absent when no ready text is configured for that
kind), and human state and the `not_ready` message say the ready text is not
on screen, which usually means a startup or approval dialog. `ask` uses the
same check before it types anything: see below.

Readiness never sets `agent.state`, which still comes only from ai-attn. Pick
text a startup, trust, or approval dialog does not show; Codex's composer
placeholder is hidden while those dialogs are open. Screen text can change
when the agent updates its UI, so a stale `ready_text` simply stops matching
and leaves the agent `not_ready`. Unset kinds, and kinds with only empty text,
are unchanged.

Launch and respawn generations reject signals older than the launch and exact
copies of pre-launch records, including same-second records. A current signal
removes `startup`; if the new record is identical in its parsed fields to an
old same-second record, it is conservatively ignored until the signal changes.
Generation metadata follows rename and is cleared with other session metadata.
Existing sessions without generation metadata use the session creation time
for the unconfirmed-startup age.

`reply NAME` prints the latest complete assistant text message from a Claude
Code, Codex or Cortex Code transcript, including a conversation resumed by `pmux respawn`.
It uses the live native session ID or pmux's saved resume identity, with the
same transcript locations as activity/history. It does not require a newly
created transcript or a reply written after respawn. Tool output, reasoning,
Claude sidechains, malformed records and incomplete final lines are excluded.
Codex analysis messages are excluded; final and commentary text are eligible.
Older event-only Codex transcripts are supported as a fallback.
Cortex replies join assistant text blocks and exclude thinking and tool blocks.
Cortex activity counts marked user prompts and tool calls; token totals remain
zero because Cortex history messages do not include per-message token usage.

```sh
pmux reply worker
pmux reply worker --json
```

JSON fields are `api_version`, `session`, `agent`, `session_id`, `available`,
`text`, `timestamp` (native string or null), `channel` (native string or null),
and `reason` (why unavailable, otherwise null). No reply yields
`available: false` and null text in JSON (exit 0); human output exits 1 with an
explanation. Missing processes and unreadable transcripts are errors. Human
output strips terminal controls while retaining line breaks; JSON preserves
original text. This is the last message, not a guarantee the current turn is
finished: wait for a completion signal separately.

`capture` reads the current screen of a named process (or `newest`/`oldest`)
without attaching, sending input, changing the screen, or creating a tmux paste
buffer. Output is plain text with terminal controls removed. It preserves
physical row breaks, indentation, blank rows, and the screen below the cursor,
so a multi-line approval box at the bottom is included. Wrapped lines stay
wrapped; this is a screen snapshot, not the last assistant reply.

```sh
pmux capture worker                  # the entire visible screen
pmux capture worker --lines 80       # last 80 rows, including available history
pmux capture worker --history        # all saved history plus the visible screen
pmux capture worker --json           # {api_version, session, pane_id, text}
```

`--lines N` (also `-n N`) requires a positive count and conflicts with
`--history`. It counts physical rows from the bottom, including blank rows;
output may be shorter if fewer rows are available. The default captures only
the visible screen, including an active alternate-screen TUI. Scrollback is
limited to what tmux retained; alternate-screen applications may have no
history. There is no cursor-based cropping or automatic prompt detection.

Use `send` to write text to the process's active pane. By default, nonempty
text is delivered through a private tmux buffer as a bracketed paste when the
application enables it. pmux preserves newlines, waits 100ms for paste handling,
and sends Enter separately. Without bracketed-paste support, tmux delivers the
plain text. An empty message sends only Enter. `--no-enter` sends literal keys
without paste framing or Enter, suitable for single-key approval responses.
`--stdin` avoids putting sensitive text in the calling shell's command line.

The confirmation describes terminal injection, not whether the application
accepted the message. JSON has `enter_sent` and `submission_confirmed` (always
`false`, since pmux cannot confirm application-level acceptance). The existing
`submitted` boolean is retained as a compatibility alias for `enter_sent`;
new clients should use `enter_sent`. Check agent state or its reply separately.
Failed input delivery returns an error without retrying, to avoid duplicate
prompts. Temporary paste buffers are removed after delivery or failure.

```sh
pmux send repl 'help'
printf '%s' 'continue with the tests' | pmux send worker --stdin
pmux send repl --no-enter 'draft text'
```

For long prompts, launch the agent without prompt text, check its screen for
startup or approval dialogs, then deliver a UTF-8 file explicitly:

```sh
pmux run -d -s worker codex
pmux capture worker
# Dismiss any startup dialog with named keys; capture again to check the composer.
pmux send worker --prompt-file ./task.txt
# Or stream input without putting the prompt in argv:
pmux send worker --stdin < ./task.txt
```

`--prompt-file FILE` reads the file once and sends its full contents through
the same paste/Enter path as ordinary text. It preserves Unicode, quotes,
backslashes, line breaks and trailing newlines. It conflicts with inline text,
`--stdin` and named keys; `--no-enter` remains available for literal input.
Unreadable files, invalid UTF-8 and NUL bytes fail before delivery. The source
file is not modified or copied into pmux state. Its contents appear in neither
pmux/agent argv nor the saved launch command (the agent's transcript may still
record the prompt normally).

There is deliberately no automatic file delivery during `run`: initial idle
readiness is not reliably signaled by the supported interactive CLIs, and
pasting into a trust or update screen could answer the dialog instead of
entering the prompt. `not_ready` also means readiness is unconfirmed; check
capture before explicitly sending. This two-step workflow retains the agent's
interactive mode and avoids guessing when its input loop is ready.

Named keys use a separate input mode with no paste framing and no automatic
Enter. A key is a named key, optionally with `C-`, `M-` or `S-`, or one
printable character such as `y` (so `--key t` presses t without Enter, unlike
`send worker t`). Repeat `--key` to send an ordered sequence; `--repeat N` repeats the
entire sequence (1–1000, at most 10000 total keys). This mode conflicts with
text, `--stdin`, and `--no-enter`.

```sh
pmux send worker --key Escape
pmux send worker --key y
pmux send worker --key C-u
pmux send worker --key End
pmux send worker --key BSpace --repeat 12
pmux send worker --key Down --key Enter --json
```

Allowed names are `Escape`, `Enter`, `BSpace`, `Tab`, `BTab`, `Space`, `Up`,
`Down`, `Left`, `Right`, `Home`, `End`, `PPage`, `NPage`, `IC`, `DC`, and
`F1`–`F12`, or a single printable ASCII character. Prefix with `C-` (Control),
`M-` (Meta/Alt), or `S-` (Shift), each at most once. Names are case sensitive;
unknown names fail before any keys are sent. Key encoding and effects depend
on the application's terminal mode. JSON for this mode contains `api_version`,
`session`, `pane_id`, `keys` (the requested sequence), `repeat`, `keys_sent`
(total keys injected), and `submission_confirmed: false`.

`wait` blocks only the caller, never the managed process. Its default condition
is process exit. Agent-state waits require an explicit matching ai-attn signal:

```sh
pmux wait test-suite --until exited --timeout 10m
pmux wait codex-fix --until agent:waiting --timeout 30m
pmux wait codex-fix --until agent:waiting --until agent:done --json
# Equivalent: --until agent:waiting,agent:done
```

Name several processes with `--all` or `--any` to wait on them together:

```sh
pmux wait helper-a helper-b helper-c --all --until agent:done --timeout 30m --json --compact
pmux wait helper-a helper-b --any --until agent:done --capture-on-block
```

`--all` returns once every process matched, but stops as soon as one is
`blocked` or `exited` (or the timeout passes), since the batch cannot finish
without attention. `--any` returns at the first process that matched or
needs attention. An agent with no ai-attn signal yet does not end either
wait: its prompt was usually confirmed already, and some agents go quiet for
a while, so it is watched until its first signal and reported `not_ready`
only if it still has none when `--timeout` ends. `--startup-timeout` applies
to single-process waits only. The other options apply to every process. JSON has `api_version`, `mode`, `condition`, `outcome` (`matched`
when the mode was satisfied, else the outcome of the deciding process),
`decided_by` (that process's name; null when every process matched under
`--all`), `observed_at`, and `results`: one ordinary wait result per process
in the order named, where a process the wait ended before deciding is
`pending`. A process that already matched keeps its result. Exit codes and
hints follow the deciding process; human output prints one status line per
process.

Repeat `--until` or comma-separate conditions to return on any match. JSON
`condition` contains the requested labels joined by commas; `matched_condition`
identifies the requested condition that matched (absent for other outcomes).
Human output names the matched condition on success; non-matches say, for
example, `blocked while waiting for agent:done`.

An agent-state wait stops immediately when ai-attn reports `waiting`, so a
supervisor can answer an approval or other input prompt. If `agent:waiting` was
requested, it matches normally. Otherwise the outcome is `blocked` with exit
code **2**. JSON includes the observed state and reason in `process.agent`;
human diagnostics print them too. `--ignore-waiting` explicitly keeps waiting
through input prompts for supervisors that handle input separately. Exit-only
waits retain their existing behavior.

Successful `send` (text, Enter, or named keys), including `ask`'s send step,
records a per-process input receipt. An unchanged pre-input `waiting` signal,
or a waiting signal timestamped before that input, no longer blocks an
agent-state wait or matches `--until agent:waiting`. This handles approval
acceptance when the agent emits no new hook until its command finishes.
A newer waiting signal still blocks; changed signals in the same second also
count. Identical same-second signals cannot be distinguished, so the old one
stays superseded until it changes. This tracks delivery, not acceptance by
the application. Empty `--no-enter` sends and rejected input do not acknowledge
signals. `ls`, `inspect`, and JSON `process.agent` continue to show the raw
latest signal; a timed-out wait can therefore contain the superseded waiting
record. Receipts follow rename and are checked against pane/creation identity.

Add `--capture-on-block` to `wait` or `ask` to include the last 80 physical
screen rows (including available history) on `blocked`, or use
`--capture-on-block=N` for a positive row count. An explicit count requires
`=` so the optional value cannot consume a process name or literal prompt.
Requested blocked captures wait a fixed 2.5 seconds before reading the screen,
allowing the approval question to render after its ai-attn hook. This settling
is bounded and does not infer prompt contents or wait indefinitely for a
screen change. It adds up to 2.5 seconds after the wait outcome is observed
(outside `--timeout`'s wait budget); calls without capture have no settling
delay. If drawing takes longer, use standalone `capture` again.
Human output appends plain screen text; JSON adds `capture` with the same
`{api_version, session, pane_id, text}` shape as `capture --json`. This uses
the normal capture path, including blank rows below the cursor. It runs only
on `blocked`, not an explicit `agent:waiting` match or `not_ready`.

```sh
pmux wait worker --until agent:done --capture-on-block --json --timeout 10m
pmux ask worker --capture-on-block=80 --prompt-file task.txt --timeout 10m
```

Capture is best effort and does not change the outcome or exit code. If the
pane disappears, is replaced, or capture fails, `capture` is absent and
`capture_error` explains the failure (also printed to stderr). Both fields
are absent unless requested and applicable; existing wait JSON is unchanged.
Use standalone `capture NAME` for startup screens or another screen read.

Agent-state waits also stop with `not_ready` (exit **3**) if no current-launch
signal arrives within `--startup-timeout` (default `5s`, measured since launch
or respawn). The JSON process includes `startup`; stderr points to capture.
This prevents a startup/update screen from matching an old completion or
blocking a supervisor indefinitely. Increase the startup budget for slow
launches; `--startup-timeout 0s` checks immediately. `--ignore-waiting` does not
bypass readiness checks. Exit-only waits are unchanged.

Exit codes: **0** for a requested match; **1** for timeout, early session exit,
or an error; **2** for input-needed blocking; **3** for unconfirmed readiness. JSON `outcome` is respectively
`matched`, `timed_out`, `exited`, `blocked`, or `not_ready` (errors may only emit stderr).
CLI argument validation can exit 2 without JSON; distinguish it from blocking
by the structured `outcome`, not the exit code alone.
Add `--json` to receive the observed outcome on stdout, even for nonzero wait
outcomes. Wait observes the current signal, including one already present when
it starts; it does not require a new transition.

`ask NAME` combines one submitted `send`, a wait for `agent:done`, and `reply`.
Start the agent without a prompt and resolve startup dialogs with `capture`
and `send --key` first. If the agent's kind has [ready text](#opt-in-ready-text)
configured and the agent has not signalled, `ask` sends only once that text is
on screen, so a prompt can't answer a startup or approval dialog. If the text
doesn't appear within `--startup-timeout` of launch, `ask` sends nothing and
exits **3** with outcome `not_ready` and `"send": null`. Options may appear before or after the literal prompt.
Use `--` before prompt arguments that begin with a hyphen or contain literal
pmux option names as separate arguments (or use file/stdin input):

```sh
pmux ask worker 'Review the changes' --timeout 10m
pmux ask worker --prompt-file ./task.txt --timeout 10m --json --compact
pmux ask worker --stdin < ./task.txt
```

The text, `--prompt-file FILE`, and `--stdin` input modes are mutually exclusive
and use the same UTF-8 reading and paste/Enter path as `send`. `--timeout` is
measured from the start of the wait after sending; absent a timeout it waits
indefinitely. `--startup-timeout` has the same launch-based meaning as in `wait`.
An unchanged pre-send `done` signal cannot complete the new turn: pmux waits
until the signal changes. An identical same-second signal is conservatively
indistinguishable from the old one. This does not confirm application acceptance.

On completion human output is the assistant reply. JSON flattens the normal
wait fields (`api_version`, `session`, `condition`, `outcome`,
`matched_condition`, `observed_at`, `process`) and adds `send` (the normal send
confirmation) and `reply` (the normal reply object, or null for non-matches).
`--compact` requires `--json` and projects `process` just as in `wait`.
An unchanged pre-send transcript reply is marked unavailable rather than
returned as the answer to the new prompt. If the transcript is missing or has
not flushed, JSON retains `outcome: "matched"`, exit 0, with
`reply.available: false` and a reason; human output exits 1 with that reason,
consistent with `reply`. Retry `pmux reply NAME` after checking the agent.

Other outcomes and exit codes are identical to `wait`: `blocked` (2),
`not_ready` (3), `timed_out` (1), and `exited` (1). These print no transcript
reply. On blocking, capture the approval, answer with `send`, then resume
`wait --until agent:done` and read `reply`. On timeout, inspect/capture and
resume waiting. **Do not repeat `ask` to resume a turn**: it sends another
prompt. On exit inspect process logs or history before launching again.

`send --confirm[=TIMEOUT]` sends a prompt like `ask` but stops as soon as
the agent takes it up, without waiting for the turn to finish. It matches a
fresh `agent:working` signal, or `agent:done` for a turn that already ended,
for at most TIMEOUT (default 10s). As in `ask`, an unchanged signal from
before the send does not count; an agent that was already working and keeps
signalling can confirm without having read the new prompt, so send to an idle
agent. Outcomes and exit codes are those of `wait`: a question shown right
after the send is `blocked` (2). An agent with no signal since launch counts
as merely unconfirmed while the timeout runs, since a fresh agent emits
nothing until its first turn starts; if no signal at all has arrived when the
timeout ends, for example because it is stopped at a folder-trust or login
dialog, the outcome is `not_ready` (3) instead of `timed_out`.
`--capture-on-block[=N]` adds the screen to either. JSON is shaped like `ask`'s without `reply`, and
`send.submission_confirmed` is true on a match. It cannot be combined with
`--key` or `--no-enter`.

```sh
pmux send worker --prompt-file task.txt --confirm --capture-on-block --json --compact
```

`activity --json` reports what the sidebar's details drawer shows for one
agent process, read from its transcript: `prompts`, `tools`, `input_tokens`,
`output_tokens`, `lines_added`, `lines_deleted`, `first_at`, `last_at`,
`recent_prompts` (up to three, oldest first), and `churn_series` and
`output_token_series` resampled to `--buckets` points (default 48) on the
sidebar's default time axis. `available` is false, with every count zero, for a
process with no parseable Claude Code, Codex or Cortex Code transcript. Times are Unix epoch
seconds, and `first_at`/`last_at` are null when unknown. The series values
follow the drawer's graph and may be resampled differently in later releases;
their field names and types are stable.

```sh
pmux activity fix-relay --json --buckets 32
```

`events` streams newline-delimited JSON snapshots, making it useful for a
sidebar or a long-running automation client (for a single snapshot, use
`ls --json`):

```sh
pmux events --interval 500ms
```

The first line is always a full snapshot. pmux then emits another
full snapshot only when the observed process or agent state changes. This is a
polling change feed, not an event journal, so clients that need a current view
should retain the latest snapshot rather than assume every short-lived
transition was observed.

Every top-level JSON object (`ls --json`, `inspect --json`,
`activity --json`, `capture --json`, `reply --json`, `send --json`,
`ask --json`, `wait --json`, and each `events` line)
carries `api_version`.
Within one `api_version`, changes are additive only: new fields may appear and
consumers should ignore fields they don't know, but existing fields keep their
names, types, and meanings. Removing or changing a field bumps `api_version`.
`lifecycle` is currently always `"running"` (pmux reports live processes);
treat unknown values as "not running". `queue list --json` and
`history --json` return arrays and are not yet covered by this policy.

The JSON API is local to the host and can expose command lines and working
directories. Treat its output like other local process metadata; avoid passing
secrets as process arguments and prefer `pmux send --stdin` for sensitive
input.
