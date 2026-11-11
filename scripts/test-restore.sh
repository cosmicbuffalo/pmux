#!/usr/bin/env sh
set -eu
# pmux exports PMUX_BIN into every pane; never let the installed binary leak in.
unset PMUX_BIN
# PMUX_TEST_TRACE=1 prints each command, since set -e exits silently.
if [ -n "${PMUX_TEST_TRACE:-}" ]; then set -x; fi

# Exercise cross-restart restore against a real, isolated tmux server: a soft
# restart keeps sessions, a lost server keeps its snapshot, and restore
# recreates shells, resumes agents with their native command, and holds back
# other commands. Nothing touches a developer's normal pmux server.
repo_dir=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
# PMUX_TEST_BIN, not PMUX_BIN: pmux sets PMUX_BIN in every pane it runs, so a
# test started from inside pmux would otherwise test the installed binary.
pmux_bin=${PMUX_TEST_BIN:-"$repo_dir/target/debug/pmux"}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/pmux-restore-test.XXXXXX")
test_home="$test_dir/home"
fake_bin="$test_dir/bin"
# macOS runners use a long TMPDIR; tmux nests its socket below TMUX_TMPDIR.
tmux_tmp=$(mktemp -d /tmp/pmux-rt.XXXXXX)
pmux_socket="pmux-restore-test-$$"
tmux_bin=$(command -v tmux)
test_path="$fake_bin:${tmux_bin%/*}:/usr/bin:/bin"
state_dir="$test_home/.local/state/pmux/sockets/$(printf '%s' "$pmux_socket" | od -An -tx1 | tr -d ' \n')"
snapshot="$state_dir/restore.json"

run_pmux() (
  unset TMUX
  HOME="$test_home" \
    TMUX_TMPDIR="$tmux_tmp" \
    PMUX_SOCKET="$pmux_socket" \
    AI_ATTN_STATE_DIR="$test_dir/ai-attn" \
    PATH="$test_path" \
    "$pmux_bin" "$@"
)

backend() {
  TMUX_TMPDIR="$tmux_tmp" "$tmux_bin" -L "$pmux_socket" "$@"
}

cleanup() {
  backend kill-server >/dev/null 2>&1 || true
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

snapshot_names() {
  python3 -c 'import json,sys; print(" ".join(sorted(s["name"] for s in json.load(open(sys.argv[1]))["sessions"])))' "$1"
}

live_names() {
  run_pmux ls --json | python3 -c 'import json,sys; print(" ".join(sorted(s["name"] for s in json.load(sys.stdin)["sessions"])))'
}

eventually() {
  attempt=0
  while ! "$@"; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ]; then
      return 1
    fi
    sleep 0.1
  done
}

if [ ! -x "$pmux_bin" ]; then
  cargo build --manifest-path "$repo_dir/Cargo.toml"
fi
mkdir -p "$test_home" "$fake_bin" "$test_dir/ai-attn"

# A stand-in agent CLI: records how it was invoked, then idles like an agent
# waiting at its prompt.
cat > "$fake_bin/claude" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$test_dir/claude-invocations"
exec sleep 600
EOF
chmod +x "$fake_bin/claude"

run_pmux run -d -s shell sh >/dev/null
run_pmux run -d -s job sleep 600 >/dev/null
run_pmux run -d -s agent claude >/dev/null

# What ai-attn's hook would record for the agent's pane.
agent_pane=$(run_pmux ls --json | python3 -c 'import json,sys; print([s["pane_id"] for s in json.load(sys.stdin)["sessions"] if s["name"]=="agent"][0])')
printf '{"state":"waiting","updated_at":%s,"agent":"claude","pane_id":"%s","session_id":"resume-me-42"}\n' \
  "$(date +%s)" "$agent_pane" > "$test_dir/ai-attn/agent.json"
run_pmux snapshot >/dev/null
test "$(snapshot_names "$snapshot")" = "agent job shell"
grep -q '"agent_session_id": "resume-me-42"' "$snapshot"

# A soft restart reloads config without touching sessions.
run_pmux restart >/dev/null
test "$(live_names)" = "agent job shell"

# Losing the server (crash, kill-server, reboot) must not wipe the snapshot.
backend kill-server
run_pmux snapshot >/dev/null 2>&1 && {
  echo "snapshot claimed success with no server running" >&2
  exit 1
}
test "$(snapshot_names "$snapshot")" = "agent job shell"

# Restore resumes the agent natively, recreates the shell, and holds the job.
restore_output=$(run_pmux restore)
printf '%s\n' "$restore_output" | grep -q '1 saved command(s) were not re-run'
test "$(live_names)" = "agent shell"
eventually grep -q '^--resume resume-me-42$' "$test_dir/claude-invocations"
# Restoring again is a no-op.
run_pmux restore >/dev/null
test "$(live_names)" = "agent shell"

# The refresh after restore dropped `job`; the previous snapshot survives.
run_pmux snapshot >/dev/null
test "$(snapshot_names "$snapshot")" = "agent shell"
test "$(snapshot_names "$state_dir/restore.prev.json")" = "agent job shell"

echo "pmux restore smoke test passed"
