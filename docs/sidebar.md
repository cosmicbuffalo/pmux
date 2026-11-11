# The pmux sidebar

`prefix + Ctrl-p` toggles the sidebar in any tmux window once pmux is
[connected to your tmux](tmux-integration.md). It is a full-height pane on
the **left** by default, and lists every live pmux process—shells, services,
REPLs, and agents alike. Agent state is optional enrichment from the most recent
matching ai-attn signal; an agent with no signal is shown as idle.

The sidebar reads the same `pmux events` JSON Lines feed available to
other local clients. By default it opens without taking focus,
restores the pane that opened it when closed, and remembers a manual resize
in `~/.local/state/pmux/`. Its
header remains fixed while the process list scrolls internally with
the mouse wheel, arrows, or `j`/`k`; `↑ (N more)` and `↓ (N more)` show when
entries are outside the viewport. The renderer captures mouse input, so
scrolling in the sidebar does not put the tmux pane into copy mode.
When pane labels are enabled, the sidebar's tmux border says `pmux processes`
instead of its CWD and the duplicate in-pane heading is hidden.
The header counts attached and detached processes, and adds the past
sessions you could resume (`3 resumable`) when the sidebar is wide enough
for all of it. Its second line briefly names the grouping or graph a key
just switched to (`Grouping by window`, `Graph: input tokens`).

Each process entry begins with a status-colored dot, an optional pill for a
manual session name, and its current working directory. An unnamed agent
process instead shows its agent's own conversation title (Claude's generated
title, Codex's thread name) in an italic pill; the tmux session name is
unchanged. A right-pinned arrow
shows whether it is attached in this window, another window, or another
tmux session. The second line aligns the agent/process kind with the colored
status and its age; any attachment location (`pane N`, `window N`, or
`N attached`) is pinned at the right edge. Paths under your home directory are
shortened with `~`. When a process changes directory after launch, a muted
`Launched in: …` line preserves that original directory. Selected entries use a
full-width background highlight while preserving the session-name pill shape. A
blank line separates entries. General processes remain first-class and display
as `process · running`. pmux-generated session identifiers are deliberately
never shown.

The list runs waiting on you first, then working, then the rest. An entry
moves only when its state changes, not on every signal while it works, so the
list stays steady; within each part, the most recent change is on top.

A process started from another pmux process (`spawned_by` in `pmux ls
--json`, see `pmux parent`) is drawn under it, indented, joined by a thin
line: the parent's box edge runs on down and turns into each child, and a
child's own children nest under it the same way, up to three generations
deep. A family moves as one entry, placed by its most pressing member, and
stays together in its root's group when grouping is on. A child waiting on
you leaves its family for the top of the list, its row ending in a muted
`from <parent>`; it goes back under its parent once it stops waiting. A child
whose parent is on another machine is listed on its own the same way, as
`from lead on macbook`. Manual groups list their members flat, each naming
its parent.

Press **Tab** to jump to an attached process, or attach an unattached process
in a new right-hand split. Click an entry to select it and click it again to
open its compact action menu; the inward arrow also opens the menu (`→` from a
left sidebar, `←` from a right sidebar). The outward arrow cancels it.
Double-click an entry to jump to the process wherever it is attached in this
tmux session, or, if it is not attached here, to open it in a new tmux
window; double-clicking a resumable session resumes it in a new window. The
generated tmux integration rebinds tmux's `DoubleClick1Pane` and
`TripleClick1Pane` so that on the sidebar pane they only forward the click
(other panes keep tmux's defaults); without it, tmux's delayed double-click
would re-focus the sidebar, so a double-click then waits about 300ms for it. Menu
actions are clickable, show a matching action glyph, and dismiss when clicked
outside. A cross-window jump opens the sidebar in the destination window before
focusing the process. The menu omits the current-window attach action when it
would do nothing. The rename form is also used by the regular pmux `prefix + ,`
binding. Generated names start with an empty rename field rather than exposing
the internal name.

Below the live processes, a **Resumable** group lists past Claude Code and
Codex conversations that are not running anywhere (the same catalog as
`pmux history`), from every project, newest first. It starts folded; Enter or a
click unfolds it. Each row shows the agent, how long ago the session was last
active, its prompt count once its transcript has been read (only for rows on
screen), and its directory. A past session takes the same keys as a live process: Enter
opens its drawer, with the activity graph and last prompts read from its saved
transcript; Tab resumes it with `pmux resume` in a new split beside the
rightmost pane, starting in the session's own directory; double-click resumes
it in a new window, and its menu offers both. A resumed session moves
into the live list. Sessions whose directory is gone are dimmed and cannot be
resumed.

When the sidebar is not focused, it follows the active tmux pane when that
pane hosts a pmux process: its full-width selection is muted gray and the
matching entry uses tmux's active-pane border color. No selection is shown
for an unrelated pane.

`@pmux_sidebar_side` puts it on the right and `@pmux_sidebar_open_focus`
makes opening it move focus into it; see
[sidebar options](tmux-integration.md#sidebar-options-and-the-equalize-key).
Closing is a pure toggle: `prefix + Ctrl-p` closes an open sidebar no matter
which pane is focused. Its width starts at 42 columns and keeps any manual
resize.

When the last other pane in a window closes (killed, or its process exits),
the window closes too rather than leaving the sidebar stretched across it.
The one exception is a session's last window, which keeps its sidebar so the
tmux session does not end.

A plain `select-layout` would also resize the sidebar; bind
[`pmux equalize-panes`](tmux-integration.md#sidebar-options-and-the-equalize-key)
to even out panes instead.

## Keys

Press `?` in the sidebar for its keymap. The defaults are Vim-style:

| Key | Action (`[sidebar.keys]` name) |
| --- | --- |
| `k` / `↑`, `j` / `↓` | move the selection (`move_up`, `move_down`) |
| `Ctrl-u`, `Ctrl-d` | move half a visible page (`half_page_up`, `half_page_down`) |
| `gg`, `G` | first / last item (`first`, `last`) |
| `Tab` | jump to the process, or attach it (or resume a past session) in a new split (`open`) |
| `Enter` | open or close the selected process's or past session's drawer, fold or unfold a group; on the header graph, every process drawer (`toggle`) |
| `→` (`←` on the right) | the selected entry's action menu (`menu`) |
| `z` | fold or unfold the group the selection is in (`fold_section`) |
| `a` | cycle grouping: ungrouped, window, CWD, agent (`cycle_grouping`) |
| `/` | filter the list as you type; see [Searching](#searching) (`search`) |
| `m`, `x` | with a drawer open, cycle the graph between output tokens (`↓`), input tokens per turn (`↑`), and lines changed (`+/-`), and cycle the timeline scale (`cycle_metric`, `cycle_scale`) |
| `q` / `Esc` | close the help, or the sidebar (`close`) |
| `?` | toggle the keymap (`help`) |

`PgUp`/`PgDn` (`page_up`, `page_down`), `End`, and `Home` (`select_header`,
which selects the header graph) also work, but the keymap leaves them out.
While the keymap is open, the movement keys and the mouse wheel scroll it in a
window too small to show it whole.

`[sidebar.keys]` in `~/.config/pmux/config.toml` rebinds any of them
(`[keys]` rebinds the sidebar and the attach picker together), and
`[rows]` there chooses what process rows show, on which lines, in what
colors and outline; see
[Customizing the sidebar and picker](sidebar-customization.md).

### Searching

`/` opens a search line at the top of the list, and the list narrows to what matches
as you type. Each word of the query has to match one of a process's name,
its agent's conversation title, the agent (`claude`, `codex`) or its state
(`waiting`, `working`, `done`), the running command, or the directory. Past
sessions in the Resumable group match on their title, agent, branch, and
directory. Matching is fuzzy, like the attach picker's, with fzf's syntax:
`'word` for an exact match, `^word` for a prefix, `!word` to leave out what
matches it.

A process spawned by another stays under its parent, even when only the
child matches, and folded groups open while the query is active. While
typing, `↑`/`↓` (or `Ctrl-p`/`Ctrl-n`) move the selection, `Tab` jumps to
it, `Enter` keeps the filter and gives the keys back to the list, and `Esc`
drops the query. With a filter kept, `/` edits it again and `Esc` clears it
before it would close the sidebar. When the filtered list scrolls, the
search line ends with how many rows are above the view (`↑ 3`).
