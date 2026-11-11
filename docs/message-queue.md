# Scheduled message batches

Press **Ctrl-p, then q** inside a pmux process to open its message queue.
Add each instruction separately, then choose one release time for the batch.
At that time pmux joins the instructions into one prompt, pastes it into the
original process, and presses Enter once. There is no sequential agent-turn
tracking or automatic usage-limit detection.

## Popup controls

| Key | Action |
| --- | --- |
| `a` | Add a multiline instruction |
| `e` | Edit the selected instruction |
| Up / Down or `k` / `j` | Select an instruction |
| `J` / `K` | Move the selected instruction down / up |
| `d` | Delete the selected instruction |
| `t` | Open the date/time picker and arm the batch |
| Space | Pause or resume the batch |
| `c` | Cancel the batch |
| `q` / Esc | Close the queue; an armed schedule keeps running |

The message editor supports arrow keys, Home/End, Unicode, and multiline paste.
Enter inserts a newline, **Esc saves and backs out** to the queue, and **Ctrl-c
discards** the edit. Ctrl-s is deliberately unused: it is your tmux prefix
and never reaches the popup. Saving an empty editor leaves the queue unchanged.
Edits save to disk immediately. An editor open across a concurrent batch change
cannot silently overwrite the newer batch.

New batches start as drafts. Adding more instructions to an armed batch keeps
its existing time. Removing its final instruction clears the schedule. After
a batch is sent or cancelled, adding a message starts a new unscheduled batch.
An expired paused batch needs a new future time before it can resume.

## Choosing a time

Times use the **pmux host's local timezone**, which may differ from the timezone
of the phone or laptop you are using to attach.

### In the popup

`t` opens an arrow-key picker:

```
                 ▲
       2026-10-13   04:00
                 ▼

  Tue 2026-10-13 04:00 CDT (-0500)
           in 30d 3h 2m
```

Left / Right choose a field, Up / Down adjust it, Enter arms the batch, and
Esc or Ctrl-c backs out without changing the schedule. The picker opens on the
batch's current time if it already has one, otherwise on the next whole minute.

Fields carry: incrementing a minute past 59 rolls the hour, and a day past the
end of the month rolls the month. Changing month or year clamps a stranded day,
so 31 January becomes 28 (or 29) February. The weekday, timezone, and countdown
under the clock update as you move, so there is no separate review step.

### From the CLI

`pmux queue schedule` and `--after` take the same times as text:

- `in 45m`, `in 2h 15m`, `in 30s`
- `7pm`, `19:30` (the next occurrence)
- `today 19:30`, `tomorrow 9am`
- `2026-09-11 19:30`

Explicit dates in the past and invalid dates are rejected. Local clock times
that are ambiguous or missing during a daylight-saving change are rejected;
a relative duration can be used instead. The chosen time is stored as a UTC
epoch, so subsequent timezone changes do not move the deadline. The scheduler
checks approximately once per second and sends after the deadline when the
host is running. It does not send early.

## Combined prompt

A single instruction is sent exactly as entered. Multiple instructions preserve
their original whitespace and Markdown inside numbered sections:

```text
The user queued the following 2 instructions for this time.
Treat each numbered section as a separate user instruction, in the order listed.

--- Instruction 1 of 2 ---
Continue the implementation.

--- Instruction 2 of 2 ---
Run the tests and fix failures.
```

Delivery uses tmux bracketed paste and one Enter key. The target CLI needs to
support multiline/bracketed paste and Enter-to-submit. This first version sends
at the chosen time without checking agent readiness, existing typed input,
permission prompts, or whether a provider's usage limit has actually reset.
Leave the agent at an empty prompt when scheduling.

## CLI

```sh
# Collect first, then schedule the entire batch.
pmux queue add my-agent "Continue the implementation."
pmux queue add my-agent --stdin < follow-up.md
pmux queue schedule my-agent "in 45m"

# Or add and schedule atomically.
pmux queue add my-agent --after "7pm" "Continue the implementation."

pmux queue open my-agent
pmux queue preview my-agent
pmux queue edit my-agent 1 --stdin < replacement.md
pmux queue move my-agent 2 1
pmux queue remove my-agent 2
pmux queue pause my-agent
pmux queue resume my-agent
pmux queue list my-agent
pmux queue list --json
pmux queue cancel <batch-id>
```

Message indices start at 1. `list` without a target includes history for exited
processes; `--json` includes the complete saved messages and delivery details.
Targets accept names and `newest`/`oldest`; selectors are resolved when queueing,
never again at delivery. Use `--stdin` for multiline text or to keep prompt text
out of shell history and process arguments.

## Persistence and recovery

Queues are stored under `~/.local/state/pmux/queues/<encoded-socket>/`, separate
for each `PMUX_SOCKET`. The private directory contains an atomic JSON store,
OS lock files, and a worker error log. Saved prompt text and history are plain
text in files accessible only to the owning user by default.

One detached scheduler owns each socket's queue. It starts automatically on
queue operations and is independent of ai-attn. Closing the popup, detaching,
or disconnecting SSH does not cancel it. Attaching with pmux or opening/listing
the queue restarts a stopped scheduler. `pmux queue worker` can also run it
manually. The scheduler exits once nothing is scheduled and restarts with the
next queue operation. An error such as an unreadable store is logged to the
worker log and retried with backoff rather than stopping the scheduler.

Delivery is late rather than skipped: if the machine was asleep or the
scheduler was stopped when a batch came due, it is sent as soon as the
scheduler runs again, provided the original process is still alive.

Session renames preserve targeting. Exited, respawned, or reboot-restored
processes cannot inherit an old queue: those batches are held when due. Use
`pmux queue list --json` to recover their messages and create a new schedule
for the intended process; old batches can be cancelled by ID.

Before delivery pmux durably records `sending`. If the worker crashes while
submitting, that record becomes `uncertain` on recovery and is never retried
automatically. Inspect the terminal before creating another batch. `sent`
means input was submitted to the terminal, not that the agent accepted or
completed it. The most recent 200 sent and cancelled batches remain in local
history; held, uncertain, and editable batches are never pruned.

## Installing the binding

After installing the new binary, run `pmux restart` to refresh generated config
and reload pmux's tmux server without stopping sessions. Existing custom `q` bindings
are preserved; use `pmux queue open <name>` or bind your preferred key using
[`templates/queue-binding.conf`](../templates/queue-binding.conf).

The status bar shows an armed or paused queue's message count and time. Status
updates use pane options, without adding shell polling to tmux's status format.
