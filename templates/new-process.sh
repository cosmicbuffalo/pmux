#!/usr/bin/env sh
# The installer or first pmux command creates ~/.config/pmux/new-process.sh.
# pmux overwrites it when the generated-config version changes or any expected
# generated file is missing. Do not edit the installed copy.
# To customize it, copy it to new-process.custom.sh, keep it executable, replace
# its path in the `bind c` and `bind C` lines in ~/.config/pmux/tmux.conf, then
# run `pmux restart`.
set -eu

pane_id="${1:-}"
if [ -z "$pane_id" ]; then
  printf 'usage: %s <pane_id> [command ...]\n' "$0" >&2
  exit 2
fi
shift

cwd="$(tmux display-message -p -t "$pane_id" "#{pane_current_path}" 2>/dev/null || true)"
if [ -z "$cwd" ]; then
  cwd="${HOME:-.}"
fi
cd -- "$cwd"

# A person started this from a key binding, so the pane it was pressed in is
# not the new process's parent.
if [ "$#" -eq 0 ]; then
  exec "${PMUX_BIN:-pmux}" run --no-parent "${SHELL:-/bin/sh}"
fi
exec "${PMUX_BIN:-pmux}" run --no-parent "$@"
