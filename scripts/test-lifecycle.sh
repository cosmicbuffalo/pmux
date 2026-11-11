#!/usr/bin/env sh
set -eu
# pmux exports PMUX_BIN into every pane; never let the installed binary leak in.
unset PMUX_BIN
# PMUX_TEST_TRACE=1 prints each command, since set -e exits silently.
if [ -n "${PMUX_TEST_TRACE:-}" ]; then set -x; fi

# Exercise the user-visible lifecycle against a real, isolated tmux server.
# Every path is under a temporary HOME and socket directory, so this test can
# run safely alongside a developer's normal pmux server.
repo_dir=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
pmux_bin=${PMUX_BIN:-"$repo_dir/target/debug/pmux"}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/pmux-lifecycle-test.XXXXXX")
test_home="$test_dir/home"
# macOS runners use a long TMPDIR under /var/folders. tmux nests its socket
# below TMUX_TMPDIR, so keep that directory deliberately short.
tmux_tmp=$(mktemp -d /tmp/pmux-tmux.XXXXXX)
pmux_socket="pmux-lifecycle-test-$$"
tmux_bin=$(command -v tmux)
tmux_bin_dir=${tmux_bin%/*}
pmux_bin_dir=${pmux_bin%/*}
test_path="$pmux_bin_dir:$tmux_bin_dir:/usr/bin:/bin"
outer_socket="pmux-lifecycle-outer-$$"

run_pmux() (
  unset TMUX
  HOME="$test_home" \
    TMUX_TMPDIR="$tmux_tmp" \
    PMUX_SOCKET="$pmux_socket" \
    PATH="$test_path" \
    "$pmux_bin" "$@"
)

# Start pmux in the background and record its PID for cleanup. Backgrounding
# the run_pmux subshell would fork twice, making $! an intermediate shell and
# orphaning pmux; env execs pmux directly, so $! is the pmux process itself.
background_pids=""
start_pmux_background() {
  env -u TMUX \
    HOME="$test_home" \
    TMUX_TMPDIR="$tmux_tmp" \
    PMUX_SOCKET="$pmux_socket" \
    PATH="$test_path" \
    "$pmux_bin" "$@" &
  background_pid=$!
  background_pids="$background_pids $background_pid"
}

outer_tmux() (
  unset TMUX
  TMUX_TMPDIR="$tmux_tmp" "$tmux_bin" -f /dev/null -L "$outer_socket" "$@"
)

cleanup() {
  for pid in $background_pids; do
    kill "$pid" 2>/dev/null || true
  done
  outer_tmux kill-server >/dev/null 2>&1 || true
  # Stop the backend with tmux itself rather than `pmux restart --hard`, which
  # older builds answered with an unanswerable confirmation prompt; with the
  # backend's exit-empty off, any cleanup failure leaks the server forever.
  TMUX_TMPDIR="$tmux_tmp" "$tmux_bin" -L "$pmux_socket" kill-server >/dev/null 2>&1 || true
  # A tmux hook can finish a fraction after the server exits. Retry removal so
  # the smoke test never leaves its temporary HOME behind on a busy runner.
  attempt=0
  while [ -e "$test_dir" ] && [ "$attempt" -lt 20 ]; do
    rm -rf "$test_dir" 2>/dev/null || true
    [ ! -e "$test_dir" ] && break
    sleep 0.1
    attempt=$((attempt + 1))
  done
  rm -rf "$tmux_tmp" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

if [ ! -x "$pmux_bin" ]; then
  cargo build --manifest-path "$repo_dir/Cargo.toml"
fi
mkdir -p "$test_home" "$tmux_tmp"

test "$(run_pmux ls)" = "No running pmux processes."
# shellcheck disable=SC2016 # $value and $HOME expand inside the managed shell.
test "$(run_pmux run --detach --session-name lifecycle-smoke sh -c 'IFS= read -r value; printf %s "$value" > "$HOME/sent-input"; IFS= read -r value; printf %s "$value" > "$HOME/sent-input-stdin"; sleep 60')" = "lifecycle-smoke"

run_pmux ls > "$test_dir/list-active"
grep -Fq "lifecycle-smoke" "$test_dir/list-active"

# The machine API includes ordinary processes without inventing agent state.
run_pmux ls --json > "$test_dir/list-json"
grep -Fq '"name": "lifecycle-smoke"' "$test_dir/list-json"
grep -Fq '"lifecycle": "running"' "$test_dir/list-json"
grep -Fq '"agent": null' "$test_dir/list-json"
run_pmux ls --json --compact > "$test_dir/list-compact-json"
grep -Fq '"name": "lifecycle-smoke"' "$test_dir/list-compact-json"

# `inspect` has the short `i` alias.
run_pmux i lifecycle-smoke --json > "$test_dir/inspect-json"
grep -Fq '"pane_id":' "$test_dir/inspect-json"

# Literal process input works independently of any agent integration.
run_pmux send lifecycle-smoke --json 'hello lifecycle' > "$test_dir/send-json"
grep -Fq '"submitted": true' "$test_dir/send-json"
attempt=0
while [ ! -f "$test_home/sent-input" ] && [ "$attempt" -lt 20 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(cat "$test_home/sent-input")" = "hello lifecycle"
printf '%s' 'stdin lifecycle' | run_pmux send lifecycle-smoke --stdin --json > "$test_dir/send-stdin-json"
grep -Fq '"submitted": true' "$test_dir/send-stdin-json"
attempt=0
while [ ! -f "$test_home/sent-input-stdin" ] && [ "$attempt" -lt 20 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
test "$(cat "$test_home/sent-input-stdin")" = "stdin lifecycle"

# `wait` observes generic process lifecycle, not only AI-agent events.
test "$(run_pmux run --detach --session-name lifecycle-short sleep 1)" = "lifecycle-short"
run_pmux wait lifecycle-short --timeout 5s --json > "$test_dir/wait-json"
grep -Fq '"condition": "exited"' "$test_dir/wait-json"
grep -Fq '"outcome": "matched"' "$test_dir/wait-json"

# An unmet agent-state wait returns structured timeout information without
# treating the ordinary shell process as an agent.
if run_pmux wait lifecycle-smoke --until agent:waiting --timeout 0s --json > "$test_dir/wait-timeout-json" 2> "$test_dir/wait-timeout-error"; then
  printf '%s\n' 'expected agent-state wait to time out' >&2
  exit 1
fi
grep -Fq '"outcome": "timed_out"' "$test_dir/wait-timeout-json"

# A rename during a wait is not an exit: wait follows the pane and reports
# the session's current name.
run_pmux run -d -s wait-rename sleep 60 >/dev/null
start_pmux_background wait wait-rename --until exited --timeout 2s --json > "$test_dir/wait-rename-json"
wait_rename_pid=$background_pid
sleep 0.5
run_pmux rename-session wait-rename wait-renamed
wait "$wait_rename_pid" || true
grep -Fq '"outcome": "timed_out"' "$test_dir/wait-rename-json"
grep -Fq '"session": "wait-renamed"' "$test_dir/wait-rename-json"
run_pmux kill wait-renamed >/dev/null

# `events` emits an initial JSON Lines snapshot and a second one when a
# general process starts; this is the sidebar/automation client path.
start_pmux_background events --interval 100ms > "$test_dir/events-follow-jsonl"
events_pid=$background_pid
sleep 1
test "$(run_pmux run --detach --session-name lifecycle-event sleep 60)" = "lifecycle-event"
sleep 1
kill "$events_pid" 2>/dev/null || true
wait "$events_pid" 2>/dev/null || true
test "$(grep -c '"event":"state"' "$test_dir/events-follow-jsonl")" -ge 2
run_pmux kill lifecycle-event

run_pmux kill lifecycle-smoke
test "$(run_pmux ls)" = "No running pmux processes."

# An attached process captures its pane just before teardown. The output is
# then replayed into the outer terminal after the nested tmux client returns.
# Run this from a throwaway outer tmux pane so the nested client has a real TTY.
run_pmux wrap echo >/dev/null
outer_command="HOME='$test_home' TMUX_TMPDIR='$tmux_tmp' PMUX_SOCKET='$pmux_socket' PATH='$test_path' sh -c '. \"$test_home/.config/pmux/wrap.sh\"; echo completion replay; sleep 10'"
outer_tmux new-session -d -x 100 -y 30 -s lifecycle-outer "$outer_command"
attempt=0
outer_screen=''
while [ "$attempt" -lt 40 ]; do
  outer_screen=$(outer_tmux capture-pane -p -t lifecycle-outer)
  printf '%s\n' "$outer_screen" | grep -Fq 'completion replay' && break
  sleep 0.1
  attempt=$((attempt + 1))
done
printf '%s\n' "$outer_screen" | grep -Fq 'completion replay'
test "$(printf '%s\n' "$outer_screen" | grep -Fc 'completion replay')" -eq 1
test "$(run_pmux ls)" = "No running pmux processes."

# tmux rewrites `.` and `:` in session names to `_`; pmux applies the same
# rewrite up front, so the name it prints is the session tmux created.
dotted_name=$(run_pmux run -d -s '3.3.2 readiness' sh -c 'printf "ATTACH_TARGET_OK\n"; sleep 60')
test "$dotted_name" = "3_3_2 readiness"
test "$(run_pmux ls --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["sessions"][0]["name"])')" = "$dotted_name"
outer_command="HOME='$test_home' TMUX_TMPDIR='$tmux_tmp' PMUX_SOCKET='$pmux_socket' PATH='$test_path' '$pmux_bin' attach '$dotted_name'"
outer_tmux new-window -d -t lifecycle-outer -n dotted "$outer_command"
attempt=0
while [ "$attempt" -lt 40 ]; do
  dotted_screen=$(outer_tmux capture-pane -p -t lifecycle-outer:dotted)
  printf '%s\n' "$dotted_screen" | grep -Fq ATTACH_TARGET_OK && break
  sleep 0.1
  attempt=$((attempt + 1))
done
printf '%s\n' "$dotted_screen" | grep -Fq ATTACH_TARGET_OK
run_pmux kill "$dotted_name"

printf 'pmux lifecycle smoke test passed\n'
