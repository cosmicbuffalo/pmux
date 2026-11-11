#!/bin/sh
# Remove only the obsolete tmux-ai-attn terminal-bell hooks.
#
# tmux-ai-attn v0.3.3 and earlier registered bell-to-ai-attn.sh as an
# alert-bell handler. Codex can emit a bell after Stop, so that fallback can
# revive a completed session as waiting. This runs after TPM loads plugins in
# the outer tmux config and preserves unrelated alert-bell handlers.
set -u

tmux_socket="${1:-}"

tmux_cmd() {
  if [ -n "$tmux_socket" ]; then
    command tmux -S "$tmux_socket" "$@"
  else
    command tmux "$@"
  fi
}

indices=$(tmux_cmd show-hooks -g 2>/dev/null \
  | awk '
      $1 == "alert-bell" || index($1, "alert-bell[") == 1 {
        if ($0 ~ /bell-to-ai-attn\.sh/) {
          if (match($1, /\[[0-9]+\]$/)) {
            print substr($1, RSTART+1, RLENGTH-2)
          } else {
            print "-1"
          }
        }
      }
    ' | sort -rn)

for index in $indices; do
  if [ "$index" = "-1" ]; then
    tmux_cmd set-hook -gu alert-bell 2>/dev/null || true
  else
    tmux_cmd set-hook -gu "alert-bell[${index}]" 2>/dev/null || true
  fi
done
