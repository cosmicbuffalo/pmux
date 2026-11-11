#!/usr/bin/env bash
# pmux TPM plugin bootstrap.
#
# Install with TPM:
#   set -g @plugin 'cosmicbuffalo/pmux'
# then press prefix + I to fetch it.
#
# IMPORTANT: pmux is a compiled binary, and TPM does NOT build or download it —
# @plugin only clones this repo and sources this file. Install the binary
# separately (see the install.sh in cosmicbuffalo/pmux); this bootstrap then
# wires the current tmux server to it. The integration is emitted by the binary
# itself (`pmux tmux-init`), so it always matches your installed pmux and never
# drifts out of sync. This runs on every tmux (re)start, so it is idempotent.

pmux_bin=""
if command -v pmux >/dev/null 2>&1; then
  pmux_bin="$(command -v pmux)"
elif [ -x "$HOME/.local/bin/pmux" ]; then
  pmux_bin="$HOME/.local/bin/pmux"
fi

if [ -n "$pmux_bin" ]; then
  # Emit the integration from the binary and source it into the current server.
  "$pmux_bin" tmux-init | tmux source-file -
else
  tmux display-message "pmux: binary not found on PATH or ~/.local/bin — install it (see cosmicbuffalo/pmux) then reload tmux"
fi
