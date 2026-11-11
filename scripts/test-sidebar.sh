#!/usr/bin/env sh
set -eu
# pmux exports PMUX_BIN into every pane; never let the installed binary leak in.
unset PMUX_BIN
# PMUX_TEST_TRACE=1 prints each command, since set -e exits silently.
if [ -n "${PMUX_TEST_TRACE:-}" ]; then set -x; fi

# Exercise the bundled outer-tmux sidebar against a real, isolated outer tmux
# server. This covers its default side, resize hook, width persistence, and
# right-side customization without touching a developer's normal tmux server.
repo_dir=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
# PMUX_TEST_BIN, not PMUX_BIN: pmux sets PMUX_BIN in every pane it runs, so a
# test started from inside pmux would otherwise test the installed binary.
build_pmux_bin=${PMUX_TEST_BIN:-"$repo_dir/target/debug/pmux"}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/pmux-sidebar-test.XXXXXX")
pmux_bin="$test_dir/pmux"
test_home="$test_dir/home"
# macOS runners use a long TMPDIR under /var/folders. tmux nests its socket
# below TMUX_TMPDIR, so keep both the server directory and outer socket short.
tmux_tmp=$(mktemp -d /tmp/pmux-tmux.XXXXXX)
outer_socket="$tmux_tmp/outer.sock"
pmux_socket="pmux-sidebar-test-$$"
tmux_bin=$(command -v tmux)
tmux_bin_dir=${tmux_bin%/*}
test_path="$tmux_bin_dir:/usr/bin:/bin"

run_pmux() (
  unset TMUX
  HOME="$test_home" \
    TMUX_TMPDIR="$tmux_tmp" \
    PMUX_SOCKET="$pmux_socket" \
    PATH="$test_path" \
    "$pmux_bin" "$@"
)

outer_tmux() (
  unset TMUX
  HOME="$test_home" \
    TMUX_TMPDIR="$tmux_tmp" \
    PATH="$test_path" \
    "$tmux_bin" -S "$outer_socket" "$@"
)

cleanup() {
  result=$?
  trap - EXIT HUP INT TERM
  outer_tmux kill-server >/dev/null 2>&1 || true
  # Stop the backend with tmux itself rather than `pmux restart --hard`, which
  # older builds answered with an unanswerable confirmation prompt; with the
  # backend's exit-empty off, any cleanup failure leaks the server forever.
  TMUX_TMPDIR="$tmux_tmp" "$tmux_bin" -L "$pmux_socket" kill-server >/dev/null 2>&1 || true
  attempt=0
  while [ -e "$test_dir" ] && [ "$attempt" -lt 20 ]; do
    rm -rf "$test_dir" 2>/dev/null || true
    [ ! -e "$test_dir" ] && break
    sleep 0.1
    attempt=$((attempt + 1))
  done
  rm -rf "$tmux_tmp" 2>/dev/null || true
  exit "$result"
}
trap cleanup EXIT HUP INT TERM

if [ ! -x "$build_pmux_bin" ]; then
  cargo build --manifest-path "$repo_dir/Cargo.toml"
fi
cp "$build_pmux_bin" "$pmux_bin"
chmod +x "$pmux_bin"
mkdir -p "$test_home/.local/bin" "$test_home/project" "$tmux_tmp"
ln -s "$pmux_bin" "$test_home/.local/bin/pmux"

# Generate the installation-managed config files, then load the outer plugin
# into a detached outer server with a predictable size.
run_pmux ls >/dev/null
run_pmux run -d -s demo --cwd "$test_home/project" sleep 60 >/dev/null
cap_name='sidebar-width-cap-validation-session'
run_pmux run -d -s "$cap_name" --cwd "$test_home/project" sleep 60 >/dev/null
auto_name=$(run_pmux run -d sleep 60)
width_file="$test_home/.local/state/pmux/sidebar_width"
# The cap row is intentionally wider than the default and resize-persistence
# test widths below.
outer_tmux new-session -d -x 120 -y 30 -s outer /bin/sh
# Exercise the normal customized setup where pane labels are visible and show
# the CWD for ordinary panes. pmux must replace that label only for its pane.
outer_tmux set-option -g pane-border-status top
outer_tmux set-option -g pane-border-format ' cwd: #{pane_current_path} '
test "$(outer_tmux show-options -wAqv -t outer pane-border-status)" = "top"
# Opening leaves focus in place by default; this lifecycle test was written
# around the opt-in behaviour that focuses the new sidebar.
outer_tmux set-option -g @pmux_sidebar_open_focus sidebar
outer_tmux source-file "$test_home/.config/pmux/outer-tmux.conf"
# The managed block applies the integration via `pmux tmux-init --apply`, which
# must run in the background (run-shell -b) — a synchronous run-shell that calls
# back into tmux would deadlock the server at config-load time. So wait for the
# binding to land before asserting, rather than racing the background apply.
attempt=0
while [ "$attempt" -lt 100 ]; do
  if outer_tmux list-keys -T prefix 2>/dev/null | grep -Eq '[[:space:]]C-p[[:space:]].*toggle-sidebar'; then
    break
  fi
  sleep 0.1
  attempt=$((attempt + 1))
done
outer_tmux list-keys -T prefix | grep -Eq '[[:space:]]C-p[[:space:]].*toggle-sidebar'
outer_tmux show-hooks -g after-resize-pane | grep -Fq 'sidebar-width'
outer_tmux show-hooks -g window-resized | grep -Fq 'sidebar-stabilize'
outer_tmux show-hooks -g after-kill-pane | grep -Fq 'sidebar-stabilize'
outer_tmux show-hooks -g pane-exited | grep -Fq 'sidebar-stabilize'
# The migration also installs the focus hook (relay host-pane tracking + sidebar
# nudge), which the old static sidebar.conf never carried.
outer_tmux show-hooks -g pane-focus-in | grep -Fq 'on-focus'
# The window's pane geometry. tmux 3.8 reports layouts as JSON that also
# records the active and last pane ("a"/"l"); those follow focus, not layout,
# so drop them before comparing.
window_geometry() {
  outer_tmux display-message -p -t "$1" '#{window_layout}' \
    | sed -E 's/,"(a|l)":(true|[0-9]+)//g'
}

host_pane=$(outer_tmux display-message -p -t outer '#{pane_id}')
# A second normal pane catches the original regression: a full-height sidebar
# used to leave an existing horizontal split narrower after every close.
other_pane=$(outer_tmux split-window -d -h -P -F '#{pane_id}' -t "$host_pane" /bin/sh)
original_layout=$(window_geometry "$host_pane")

find_sidebar() {
  outer_tmux list-panes -t outer -F '#{pane_id} #{@pmux_sidebar}' \
    | awk '$2 == "1" { print $1; exit }'
}

sidebar_width_delta() {
  pane_key=$1
  delta_data=$2
  printf '%s\n' "$delta_data" | tr ',' '\n' | awk -F: -v pane_key="$pane_key" '$1 == pane_key { print $2; exit }'
}

# The default is a full-height, left-edge pane at the documented width, and it
# receives focus while remembering exactly which host pane opened it.
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)
test -n "$sidebar_pane"
test "$(outer_tmux display-message -p '#{pane_id}')" = "$sidebar_pane"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{@pmux_sidebar_host_pane}')" = "$host_pane"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_dead}')" = "0"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "42"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left}')" = "0"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_height}')" = "$(outer_tmux display-message -p -t "$host_pane" '#{pane_height}')"
test -n "$(outer_tmux display-message -p -t "$sidebar_pane" '#{@pmux_sidebar_width_deltas}')"
# Retain the user's normal pane-border format as a fallback, while giving
# only the marked sidebar pane its own concise label.
outer_tmux show-options -gqv pane-border-format | grep -Fq '@pmux_sidebar_pane_border_fallback'
outer_tmux show-options -gqv pane-border-format | grep -Fq '#[fg=cyan] pmux processes #[default]'

# The user's equalize binding delegates to this sidebar-aware helper. It must
# keep the edge sidebar at its exact current width while evenly distributing
# only the ordinary panes in the rest of the window.
sidebar_width_before_equalize=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
run_pmux equalize-panes "$host_pane" --socket "$outer_socket"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "$sidebar_width_before_equalize"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left}')" = "0"
test "$(outer_tmux list-panes -t outer -F '#{@pmux_sidebar}\t#{pane_width}' | awk -F '\t' '$1 != "1" { widths[$2] = 1 } END { for (width in widths) count++; print count + 0 }')" = "1"

# The sidebar is a real Rust TUI: it paints its header (the attached and
# detached counts) and general-process records, hides generated
# identifiers, and never renders the foreground command as the old third
# line did.
attempt=0
sidebar_screen=''
while [ "$attempt" -lt 100 ]; do
  sidebar_screen=$(outer_tmux capture-pane -p -t "$sidebar_pane")
  printf '%s\n' "$sidebar_screen" | grep -Eq '[0-9]+ attached, [0-9]+ detached' && break
  sleep 0.1
  attempt=$((attempt + 1))
done
printf '%s\n' "$sidebar_screen" | grep -Eq '[0-9]+ attached, [0-9]+ detached'
printf '%s\n' "$sidebar_screen" | grep -Fq '◂⠿▸'
if printf '%s\n' "$sidebar_screen" | grep -Fq 'pmux processes'; then
  echo 'duplicated pane title leaked into sidebar content' >&2
  exit 1
fi
# Manually named sessions render as colored pills; capture-pane strips their
# styling, so assert the label text rather than the old square-bracket form.
printf '%s\n' "$sidebar_screen" | grep -Fq 'demo'
printf '%s\n' "$sidebar_screen" | grep -Fq 'process'
# The tilde is a literal we want: the sidebar renders the home-abbreviated path.
# shellcheck disable=SC2088
printf '%s\n' "$sidebar_screen" | grep -Fq '~/project'
printf '%s\n' "$sidebar_screen" | grep -Fq '●'
printf '%s\n' "$sidebar_screen" | grep -Fq '·'
printf '%s\n' "$sidebar_screen" | grep -Fq 'ago'
if printf '%s\n' "$sidebar_screen" | grep -Fq 'Started:'; then
  echo 'obsolete Started label leaked into sidebar' >&2
  exit 1
fi
if printf '%s\n' "$sidebar_screen" | grep -Fq 'as of'; then
  echo 'obsolete as of label leaked into sidebar' >&2
  exit 1
fi
if printf '%s\n' "$sidebar_screen" | grep -Fq "$auto_name"; then
  echo 'generated session name leaked into sidebar' >&2
  exit 1
fi
if printf '%s\n' "$sidebar_screen" | grep -Fq 'unnamed process'; then
  echo 'generated session fallback label leaked into sidebar' >&2
  exit 1
fi
if printf '%s\n' "$sidebar_screen" | grep -Fq 'sleep'; then
  echo 'foreground command leaked into sidebar third line' >&2
  exit 1
fi

# Replacing the binary beneath an open sidebar must restart its renderer in
# place. Linux lets this isolated integration test distinguish the old deleted
# executable inode from the fresh one via /proc. (macOS runs the rest of this
# smoke test; its equivalent process-executable inspection is not portable.)
if [ -d /proc/self ]; then
  sidebar_pid=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_pid}')
  replacement="$test_dir/pmux.replacement"
  cp "$build_pmux_bin" "$replacement"
  chmod +x "$replacement"
  mv "$replacement" "$pmux_bin"
  expected_inode=$(stat -Lc '%i' "$pmux_bin")
  attempt=0
  reloaded_inode=''
  while [ "$attempt" -lt 100 ]; do
    reloaded_inode=$(stat -Lc '%i' "/proc/$sidebar_pid/exe" 2>/dev/null || true)
    [ "$reloaded_inode" = "$expected_inode" ] && break
    sleep 0.1
    attempt=$((attempt + 1))
  done
  test "$reloaded_inode" = "$expected_inode"
fi

# The in-pane handle owns its drag gesture instead of relying on tmux's narrow
# border hit target. Feed the same SGR mouse events a terminal would send:
# press the middle of the handle, drag five columns right, then release.
mouse_drag=$(printf '\033[<0;40;1M\033[<32;45;1M\033[<0;45;1m')
# After a reload the new process must have redrawn (and re-enabled mouse
# reporting) before it can see the drag.
attempt=0
while [ "$attempt" -lt 100 ]; do
  outer_tmux capture-pane -p -t "$sidebar_pane" | grep -Eq '[0-9]+ attached, [0-9]+ detached' && break
  sleep 0.1
  attempt=$((attempt + 1))
done
sleep 0.3
outer_tmux send-keys -t "$sidebar_pane" -l "$mouse_drag"
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
  [ "$current_width" = "47" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "47"
# Leave the remainder of the lifecycle test at its original baseline.
outer_tmux resize-pane -t "$sidebar_pane" -x 42

# A direct oversized resize is immediately clamped to the longest useful
# process presentation, and the corrected value replaces a stale remembered
# width for future opens. Use an isolated outer window so this intentional
# resize cannot affect the primary layout-restoration scenario below.
cap_host=$(outer_tmux new-window -d -P -F '#{pane_id}' -n sidebar-width-cap /bin/sh)
cap_window=$(outer_tmux display-message -p -t "$cap_host" '#{window_id}')
run_pmux toggle-sidebar "$cap_host" --socket "$outer_socket"
cap_sidebar=$(outer_tmux list-panes -t "$cap_window" -F '#{pane_id} #{@pmux_sidebar}' | awk '$2 == "1" { print $1; exit }')
test -n "$cap_sidebar"
# after-resize-pane runs in the session's current-window context, so resize
# from the visible window as a user would.
outer_tmux select-window -t "$cap_window"
outer_tmux resize-pane -t "$cap_sidebar" -x 90
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$cap_sidebar" '#{pane_width}')
  # The width file is written by an asynchronous hook; wait for both.
  [ "$current_width" -eq 90 ] && [ "$(cat "$width_file")" = "90" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
# Content length no longer caps a manually chosen width; only window space does.
sidebar_cap=$(outer_tmux display-message -p -t "$cap_sidebar" '#{pane_width}')
test "$sidebar_cap" -eq 90
test "$(cat "$width_file")" = "$sidebar_cap"
run_pmux toggle-sidebar "$cap_sidebar" --socket "$outer_socket"
run_pmux toggle-sidebar "$cap_host" --socket "$outer_socket"
cap_sidebar=$(outer_tmux list-panes -t "$cap_window" -F '#{pane_id} #{@pmux_sidebar}' | awk '$2 == "1" { print $1; exit }')
test "$(outer_tmux display-message -p -t "$cap_sidebar" '#{pane_width}')" = "$sidebar_cap"
run_pmux toggle-sidebar "$cap_sidebar" --socket "$outer_socket"

# Toggle is pure: an open sidebar closes from any pane and leaves the user
# exactly where they were.
outer_tmux select-window -t "$(outer_tmux display-message -p -t "$host_pane" '#{window_id}')"
outer_tmux select-pane -t "$other_pane"
run_pmux toggle-sidebar "$other_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"
test "$(outer_tmux display-message -p '#{pane_id}')" = "$other_pane"
test "$(window_geometry "$host_pane")" = "$original_layout"
# Closing from inside the sidebar returns to the pane focused before it
# opened, rather than the pane recorded as its host.
run_pmux toggle-sidebar "$other_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)
test "$(outer_tmux display-message -p '#{pane_id}')" = "$sidebar_pane"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{@pmux_sidebar_previous_pane}')" = "$other_pane"
run_pmux toggle-sidebar "$sidebar_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"
test "$(outer_tmux display-message -p '#{pane_id}')" = "$other_pane"
test "$(window_geometry "$host_pane")" = "$original_layout"
outer_tmux select-pane -t "$host_pane"
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)

# Reopening restores the saved width and stays on the same edge. Closing
# records the final width too, covering outer tmux setups where a mouse drag
# does not emit after-resize-pane.
outer_tmux set-hook -gu 'after-resize-pane[100]'
outer_tmux resize-pane -t "$sidebar_pane" -x 51
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "51"
run_pmux toggle-sidebar "$sidebar_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"
test "$(outer_tmux display-message -p '#{pane_id}')" = "$host_pane"
test "$(cat "$width_file")" = "51"
test "$(window_geometry "$host_pane")" = "$original_layout"
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "51"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left}')" = "0"

# Terminal window resizes must preserve the saved sidebar preference. When the
# terminal becomes too narrow the sidebar may be constrained temporarily, but
# the width file remains untouched and it returns to the user-selected width
# once the terminal has room again.
outer_tmux resize-window -t outer -x 90
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
  [ "$current_width" = "51" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "51"
test "$(cat "$width_file")" = "51"
outer_tmux resize-window -t outer -x 40
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
  [ "$current_width" -lt 51 ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" -lt 51
test "$(cat "$width_file")" = "51"
outer_tmux resize-window -t outer -x 120
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
  [ "$current_width" = "51" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')" = "51"
test "$(cat "$width_file")" = "51"

# Closing reflows the layout that exists *now*, so a manual resize while the
# sidebar is open is preserved proportionally rather than reset to its
# pre-sidebar geometry.
outer_tmux resize-pane -t "$other_pane" -x 50
host_width_before_close=$(outer_tmux display-message -p -t "$host_pane" '#{pane_width}')
other_width_before_close=$(outer_tmux display-message -p -t "$other_pane" '#{pane_width}')
test "$other_width_before_close" = "50"
width_deltas=$(outer_tmux display-message -p -t "$sidebar_pane" '#{@pmux_sidebar_width_deltas}')
host_index=${host_pane#%}
other_index=${other_pane#%}
host_expected=$((host_width_before_close + $(sidebar_width_delta "$host_index" "$width_deltas")))
other_expected=$((other_width_before_close + $(sidebar_width_delta "$other_index" "$width_deltas")))
run_pmux toggle-sidebar "$sidebar_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"
test "$(outer_tmux display-message -p -t "$host_pane" '#{pane_width}')" = "$host_expected"
test "$(outer_tmux display-message -p -t "$other_pane" '#{pane_width}')" = "$other_expected"
test "$(window_geometry "$host_pane")" != "$original_layout"
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)

# Changing the documented outer option moves the next open to the right while
# preserving the same width preference.
run_pmux toggle-sidebar "$sidebar_pane" --socket "$outer_socket"
outer_tmux set-option -g @pmux_sidebar_side right
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)
pane_left=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left}')
pane_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_width}')
window_width=$(outer_tmux display-message -p -t "$sidebar_pane" '#{window_width}')
test "$pane_width" = "51"
test $((pane_left + pane_width)) = "$window_width"
run_pmux toggle-sidebar "$sidebar_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"

# Closing a work pane must not allow tmux to absorb its width into the edge
# sidebar. The after-kill-pane hook restores the remembered sidebar width and
# leaves the released space with the remaining work pane.
outer_tmux set-option -g @pmux_sidebar_side left
close_host=$(outer_tmux new-window -d -P -F '#{pane_id}' -n sidebar-close /bin/sh)
close_window=$(outer_tmux display-message -p -t "$close_host" '#{window_id}')
closing_pane=$(outer_tmux split-window -d -h -P -F '#{pane_id}' -t "$close_host" /bin/sh)
run_pmux toggle-sidebar "$close_host" --socket "$outer_socket"
close_sidebar=$(outer_tmux list-panes -t "$close_window" -F '#{pane_id} #{@pmux_sidebar}' | awk '$2 == "1" { print $1; exit }')
test -n "$close_sidebar"
close_sidebar_width=$(outer_tmux display-message -p -t "$close_sidebar" '#{pane_width}')
outer_tmux kill-pane -t "$closing_pane"
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$close_sidebar" '#{pane_width}')
  [ "$current_width" = "$close_sidebar_width" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$close_sidebar" '#{pane_width}')" = "$close_sidebar_width"
run_pmux toggle-sidebar "$close_sidebar" --socket "$outer_socket"

# Closing the pane that *opened* the sidebar is the same case, except the
# sidebar's recorded host pane is now gone. The hook must still restore the
# sidebar width (handing the space to the remaining split), and the toggle must
# still close the sidebar from any pane rather than failing on the dead host.
orphan_host=$(outer_tmux new-window -d -P -F '#{pane_id}' -n sidebar-orphan /bin/sh)
orphan_window=$(outer_tmux display-message -p -t "$orphan_host" '#{window_id}')
orphan_other=$(outer_tmux split-window -d -h -P -F '#{pane_id}' -t "$orphan_host" /bin/sh)
run_pmux toggle-sidebar "$orphan_host" --socket "$outer_socket"
orphan_sidebar=$(outer_tmux list-panes -t "$orphan_window" -F '#{pane_id} #{@pmux_sidebar}' | awk '$2 == "1" { print $1; exit }')
test -n "$orphan_sidebar"
orphan_sidebar_width=$(outer_tmux display-message -p -t "$orphan_sidebar" '#{pane_width}')
# after-kill-pane runs in the session's current-window context, so kill from
# the visible window as a user would.
outer_tmux select-window -t "$orphan_window"
outer_tmux kill-pane -t "$orphan_host"
attempt=0
while [ "$attempt" -lt 100 ]; do
  current_width=$(outer_tmux display-message -p -t "$orphan_sidebar" '#{pane_width}')
  [ "$current_width" = "$orphan_sidebar_width" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux display-message -p -t "$orphan_sidebar" '#{pane_width}')" = "$orphan_sidebar_width"
run_pmux toggle-sidebar "$orphan_other" --socket "$outer_socket"
test -z "$(outer_tmux list-panes -t "$orphan_window" -F '#{@pmux_sidebar}' | grep -Fx 1 || true)"
outer_tmux kill-window -t "$orphan_window"

# The notification hook covers a program that exits on its own as well as a
# manually killed pane.
natural_host=$(outer_tmux new-window -d -P -F '#{pane_id}' -n sidebar-exit /bin/sh)
natural_window=$(outer_tmux display-message -p -t "$natural_host" '#{window_id}')
outer_tmux split-window -d -h -t "$natural_host" 'sleep 1'
run_pmux toggle-sidebar "$natural_host" --socket "$outer_socket"
natural_sidebar=$(outer_tmux list-panes -t "$natural_window" -F '#{pane_id} #{@pmux_sidebar}' | awk '$2 == "1" { print $1; exit }')
test -n "$natural_sidebar"
natural_sidebar_width=$(outer_tmux display-message -p -t "$natural_sidebar" '#{pane_width}')
attempt=0
while [ "$attempt" -lt 100 ]; do
  pane_count=$(outer_tmux list-panes -t "$natural_window" | wc -l | tr -d ' ')
  current_width=$(outer_tmux display-message -p -t "$natural_sidebar" '#{pane_width}')
  [ "$pane_count" = "2" ] && [ "$current_width" = "$natural_sidebar_width" ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(outer_tmux list-panes -t "$natural_window" | wc -l | tr -d ' ')" = "2"
test "$(outer_tmux display-message -p -t "$natural_sidebar" '#{pane_width}')" = "$natural_sidebar_width"
run_pmux toggle-sidebar "$natural_sidebar" --socket "$outer_socket"

# Tab is the fast path: an unattached process splits only the rightmost work
# pane. The other work pane and the sidebar must retain their exact geometry;
# a full-window `split-window -f` would resize both and fail these assertions.
# This exercises the complete sidebar → outer tmux → pmux attach lifecycle.
run_pmux toggle-sidebar "$host_pane" --socket "$outer_socket"
sidebar_pane=$(find_sidebar)
# Keys sent before the TUI has drawn its list are lost, so wait for the row.
attempt=0
while [ "$attempt" -lt 100 ]; do
  outer_tmux capture-pane -p -t "$sidebar_pane" | grep -Fq 'demo' && break
  sleep 0.1
  attempt=$((attempt + 1))
done
before_pane_count=$(outer_tmux list-panes -t outer | wc -l | tr -d ' ')
host_geometry_before=$(outer_tmux display-message -p -t "$host_pane" '#{pane_left},#{pane_top},#{pane_width},#{pane_height}')
other_left_before=$(outer_tmux display-message -p -t "$other_pane" '#{pane_left}')
other_top_before=$(outer_tmux display-message -p -t "$other_pane" '#{pane_top}')
other_width_before=$(outer_tmux display-message -p -t "$other_pane" '#{pane_width}')
other_height_before=$(outer_tmux display-message -p -t "$other_pane" '#{pane_height}')
sidebar_geometry_before=$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left},#{pane_top},#{pane_width},#{pane_height}')
outer_tmux send-keys -t "$sidebar_pane" Tab
attempt=0
while [ "$attempt" -lt 100 ]; do
  pane_count=$(outer_tmux list-panes -t outer | wc -l | tr -d ' ')
  [ "$pane_count" -eq $((before_pane_count + 1)) ] && break
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$pane_count" -eq $((before_pane_count + 1))
attached_pane=$(outer_tmux display-message -p '#{pane_id}')
test "$attached_pane" != "$host_pane"
test "$attached_pane" != "$sidebar_pane"
attached_left=$(outer_tmux display-message -p -t "$attached_pane" '#{pane_left}')
attached_width=$(outer_tmux display-message -p -t "$attached_pane" '#{pane_width}')
other_left_after=$(outer_tmux display-message -p -t "$other_pane" '#{pane_left}')
other_top_after=$(outer_tmux display-message -p -t "$other_pane" '#{pane_top}')
other_width_after=$(outer_tmux display-message -p -t "$other_pane" '#{pane_width}')
other_height_after=$(outer_tmux display-message -p -t "$other_pane" '#{pane_height}')
test "$(outer_tmux display-message -p -t "$host_pane" '#{pane_left},#{pane_top},#{pane_width},#{pane_height}')" = "$host_geometry_before"
test "$(outer_tmux display-message -p -t "$sidebar_pane" '#{pane_left},#{pane_top},#{pane_width},#{pane_height}')" = "$sidebar_geometry_before"
test "$other_left_after" = "$other_left_before"
test "$other_top_after" = "$other_top_before"
test "$other_height_after" = "$other_height_before"
test "$attached_left" -eq $((other_left_after + other_width_after + 1))
test $((other_width_after + 1 + attached_width)) -eq "$other_width_before"
# Closing after the sidebar creates a process pane must retain that current
# topology rather than relying on the layout that existed when it opened.
run_pmux toggle-sidebar "$attached_pane" --socket "$outer_socket"
test -z "$(find_sidebar)"
test "$(outer_tmux display-message -p '#{pane_id}')" = "$attached_pane"

printf 'pmux sidebar smoke test passed\n'
