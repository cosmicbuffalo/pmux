#!/usr/bin/env sh
# The installer or first pmux command creates ~/.config/pmux/ai-attn-setup.sh.
# pmux overwrites it when the generated-config version changes or any expected
# generated file is missing. Do not edit the installed copy.
# To customize it, copy it to ai-attn-setup.custom.sh, keep it executable, and
# replace its path in ~/.config/pmux/tmux.conf and any outer-tmux.conf that
# loads it. Run `pmux restart`, then reload the outer config if it is in use.
#
# Bootstraps tmux-ai-attn plugin for the current tmux server.
# Called from tmux.conf via if-shell only when ai-attn is available.
set -u

# Prefer the normal per-user install even when the tmux server was started
# before ~/.local/bin was added to PATH.
AI_ATTN_CLI="$(tmux show-option -gqv @ai_attn_cli 2>/dev/null || true)"
if [ -z "$AI_ATTN_CLI" ]; then
  if [ -x "$HOME/.local/bin/ai-attn" ]; then
    AI_ATTN_CLI="$HOME/.local/bin/ai-attn"
  else
    AI_ATTN_CLI="$(command -v ai-attn 2>/dev/null || true)"
  fi
  if [ -n "$AI_ATTN_CLI" ]; then
    tmux set-option -gq @ai_attn_cli "$AI_ATTN_CLI"
  fi
fi

# Plugin location. Precedence: @ai_attn_plugin_dir tmux option,
# PMUX_AI_ATTN_PLUGIN_DIR env var, then default install path. Setting the
# tmux option lets you point pmux at a dev clone anywhere without relying
# on a symlink.
TMUX_PLUGIN_DIR="$(tmux show-option -gqv @ai_attn_plugin_dir 2>/dev/null || true)"
ENV_PLUGIN_DIR="${PMUX_AI_ATTN_PLUGIN_DIR:-}"
MANAGED_PLUGIN=0
if [ -n "$TMUX_PLUGIN_DIR" ]; then
  PLUGIN_DIR="$TMUX_PLUGIN_DIR"
elif [ -n "$ENV_PLUGIN_DIR" ]; then
  PLUGIN_DIR="$ENV_PLUGIN_DIR"
else
  PLUGIN_DIR="${HOME}/.config/pmux/plugins/tmux-ai-attn"
  MANAGED_PLUGIN=1
fi
REQUIRED_PLUGIN_VERSION="v0.4.1"
# The commit the tag above pointed at when this pmux release was tested (the
# peeled commit of the annotated tag), and the SHA-256 of GitHub's source
# archive of that commit. Tags are mutable, so pmux downloads the commit
# itself and only runs it when the archive matches. Update all three together:
#   git ls-remote https://github.com/cosmicbuffalo/tmux-ai-attn 'refs/tags/vX.Y.Z*'
#   curl -fsSL https://github.com/cosmicbuffalo/tmux-ai-attn/archive/COMMIT.tar.gz | sha256sum
REQUIRED_PLUGIN_COMMIT="9b714c32dc868c118ba6656628ca508c76e00385"
REQUIRED_PLUGIN_SHA256="7336005dcf29edbd07a853b15f9d059583df2f74efe688f177ce5ceefa71e3e1"
PLUGIN_ARCHIVE_URL="https://github.com/cosmicbuffalo/tmux-ai-attn/archive/${REQUIRED_PLUGIN_COMMIT}.tar.gz"
# Written into a downloaded copy, recording the commit it holds.
PIN_FILE=".pmux-commit"

LOG_FILE="${XDG_STATE_HOME:-$HOME/.local/state}/tmux-ai-attn/hooks.log"
mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true

log() {
  printf '%s [setup] %s\n' "$(date '+%Y-%m-%dT%H:%M:%S')" "$1" \
    >> "$LOG_FILE" 2>/dev/null || true
}

# Pin every command to this server, even if the caller's environment changes.
# The socket path plus server PID separates both sockets and server lifetimes.
SERVER_SOCKET="$(tmux display-message -p '#{socket_path}' 2>/dev/null)" || exit 0
SERVER_PID="$(tmux display-message -p '#{pid}' 2>/dev/null)" || exit 0
case "$SERVER_PID" in ''|*[!0-9]*) exit 0 ;; esac
[ -n "$SERVER_SOCKET" ] || exit 0
tmux() { command tmux -S "$SERVER_SOCKET" "$@"; }
SERVER_LOCK="${SERVER_SOCKET}.pmux-ai-attn-${SERVER_PID}.lock"
LOCK_DIR="$(dirname "$PLUGIN_DIR")/.tmux-ai-attn.lock"
SERVER_LOCKED=0
PLUGIN_LOCKED=0
cleanup() {
  if [ "$PLUGIN_LOCKED" -eq 1 ]; then rmdir "$LOCK_DIR" 2>/dev/null || true; fi
  if [ "$SERVER_LOCKED" -eq 1 ]; then rmdir "$SERVER_LOCK" 2>/dev/null || true; fi
}
trap cleanup 0
trap 'exit 0' HUP INT TERM

# mkdir is atomic and portable (no flock dependency on macOS). Never proceed
# without ownership or expire a lock by age: a download may still be running.
# A hard-killed owner can leave a lock; bounded failure is safer than stealing
# it. A new server PID uses a fresh server lock.
lock_directory() {
  mkdir -p "$(dirname "$1")" 2>/dev/null || return 1
  tries=0
  until mkdir "$1" 2>/dev/null; do
    tries=$((tries + 1))
    if [ "$tries" -ge "$2" ]; then
      log "gave up waiting for $1; not loading without the lock"
      return 1
    fi
    sleep 1
  done
}
lock_directory "$SERVER_LOCK" 30 || exit 0
SERVER_LOCKED=1

# The commit a plugin directory holds: the marker pmux writes into a copy it
# downloaded, or the HEAD of a git checkout (older pmux releases cloned it).
plugin_head() {
  if [ -f "$PLUGIN_DIR/$PIN_FILE" ]; then
    head -n 1 "$PLUGIN_DIR/$PIN_FILE"
  elif [ -d "$PLUGIN_DIR/.git" ] && command -v git >/dev/null 2>&1; then
    git -C "$PLUGIN_DIR" rev-parse --verify --quiet 'HEAD^{commit}' 2>/dev/null || true
  fi
}

# A successful setup, including deferring to TPM, owns the server for its
# lifetime. Serial contenders/reloads must not invoke even the same entry
# twice: newer plugins may restart their daemon on each invocation. The one
# exception is pmux's own managed copy falling behind the pin, which happens
# when pmux is upgraded while this server keeps running: then the copy is
# replaced and its entry runs once more, so the server picks up the release
# this pmux was tested with. Copies at other paths are never touched.
UPGRADING=0
managed_copy_is_stale() {
  [ "$MANAGED_PLUGIN" -eq 1 ] && [ -d "$PLUGIN_DIR" ] &&
    [ "$(plugin_head)" != "$REQUIRED_PLUGIN_COMMIT" ]
}
loaded="$(tmux show-option -gqv @pmux_ai_attn_loaded 2>/dev/null || true)"
if [ -n "$loaded" ]; then
  if [ "$loaded" = "$PLUGIN_DIR" ] && managed_copy_is_stale; then
    UPGRADING=1
    log "upgrading the loaded tmux-ai-attn at $PLUGIN_DIR from $(plugin_head) to $REQUIRED_PLUGIN_VERSION ($REQUIRED_PLUGIN_COMMIT)"
  else
    log "tmux-ai-attn load decision already belongs to $loaded; skipping"
    exit 0
  fi
fi

# tmux-ai-attn registers its hooks with `set-hook -ag` (append). Without
# pre-clearing, every config reload adds another copy, and if the plugin
# repo ever moves, stale entries point at dead paths. We surgically remove
# only entries that look like ours (reference start-loop.sh or
# bell-to-ai-attn.sh) — unrelated hooks on the same events are left
# intact. Indices are deleted in descending order so earlier indices
# don't shift.
clear_stale_hooks() {
  for hook in client-attached client-session-changed session-created after-new-session alert-bell; do
    indices=$(tmux show-hooks -g 2>/dev/null \
      | awk -v h="$hook" '
          $1 == h || index($1, h"[") == 1 {
            if ($0 ~ /start-loop\.sh|bell-to-ai-attn\.sh/) {
              if (match($1, /\[[0-9]+\]$/)) {
                print substr($1, RSTART+1, RLENGTH-2)
              } else {
                print "-1"
              }
            }
          }
        ' | sort -rn)
    for idx in $indices; do
      if [ "$idx" = "-1" ]; then
        tmux set-hook -gu "$hook" 2>/dev/null || true
      else
        tmux set-hook -gu "${hook}[${idx}]" 2>/dev/null || true
      fi
    done
  done
}

# Hooks from any existing checkout already own this server, including when
# TPM uses the same path as pmux. Never invoke that entry a second time.
loaded_copy() {
  tmux show-hooks -g 2>/dev/null \
    | grep -o '/[^"\\ ]*/scripts/start-loop\.sh' \
    | sort -u \
    | while read -r loop; do
        dir=${loop%/scripts/start-loop.sh}
        if [ -x "$loop" ]; then
          echo "$dir"
          break
        fi
      done
}
respect_loaded_copy() {
  other=$(loaded_copy)
  if [ "$other" = "$PLUGIN_DIR" ] && [ "$UPGRADING" -eq 0 ] && managed_copy_is_stale; then
    UPGRADING=1
    log "upgrading the loaded tmux-ai-attn at $PLUGIN_DIR from $(plugin_head) to $REQUIRED_PLUGIN_VERSION ($REQUIRED_PLUGIN_COMMIT)"
  fi
  if [ "$other" = "$PLUGIN_DIR" ] && [ "$UPGRADING" -eq 1 ]; then
    return 0
  fi
  if [ -n "$other" ]; then
    log "tmux-ai-attn is already loaded from $other; not invoking pmux's copy at $PLUGIN_DIR. Set @ai_attn_plugin_dir to $other to make that explicit."
    tmux set-option -gq @pmux_ai_attn_loaded "$other"
    return 1
  fi
}
respect_loaded_copy || exit 0

# This helper is launched with run-shell -b before a config's synchronous TPM
# loader may run. Give it a bounded chance to register its hooks, then check
# again under the server lock. Check once more after downloading as well.
# TPM itself does not share our lock; users needing guaranteed TPM ownership
# should omit this bootstrap (Pmux.ai_attn_options instead of Pmux.ai_attn).
sleep 2
respect_loaded_copy || exit 0

# tmux-ai-attn v0.3.3 and earlier translated Codex terminal bells into
# `waiting` records. A bell is not a lifecycle event: Codex can ring after
# its Stop hook has already reported `done`. Remove only that retired plugin
# hook after loading the pinned release below, so existing pmux installations
# stop producing false waiting signals before they can upgrade the plugin.
clear_legacy_bell_hook() {
  indices=$(tmux show-hooks -g 2>/dev/null \
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
  for idx in $indices; do
    if [ "$idx" = "-1" ]; then
      tmux set-hook -gu alert-bell 2>/dev/null || true
    else
      tmux set-hook -gu "alert-bell[${idx}]" 2>/dev/null || true
    fi
  done
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    log "neither curl nor wget is available to download tmux-ai-attn"
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Download the pinned commit's source and swap it in for whatever is at
# $PLUGIN_DIR, checking the archive against REQUIRED_PLUGIN_SHA256 first. The
# copy is unpacked beside the target and moved into place in one rename.
install_pinned_plugin() {
  parent=$(dirname "$PLUGIN_DIR")
  mkdir -p "$parent" || return 1
  staging=$(mktemp -d "$parent/.tmux-ai-attn.XXXXXX") || return 1
  archive="$staging/source.tar.gz"
  if ! download "$PLUGIN_ARCHIVE_URL" "$archive"; then
    log "could not download $PLUGIN_ARCHIVE_URL"
    rm -rf "$staging"
    return 1
  fi
  actual=$(sha256_of "$archive")
  if [ "$actual" != "$REQUIRED_PLUGIN_SHA256" ]; then
    log "refusing tmux-ai-attn $REQUIRED_PLUGIN_VERSION: archive SHA-256 is ${actual:-unknown}, expected $REQUIRED_PLUGIN_SHA256"
    rm -rf "$staging"
    return 1
  fi
  unpacked="$staging/tmux-ai-attn-$REQUIRED_PLUGIN_COMMIT"
  if ! tar -xzf "$archive" -C "$staging" || [ ! -f "$unpacked/tmux-ai-attn.tmux" ]; then
    log "could not unpack $PLUGIN_ARCHIVE_URL"
    rm -rf "$staging"
    return 1
  fi
  printf '%s\n' "$REQUIRED_PLUGIN_COMMIT" > "$unpacked/$PIN_FILE"
  if [ -e "$PLUGIN_DIR" ]; then
    mv "$PLUGIN_DIR" "$staging/previous" || { rm -rf "$staging"; return 1; }
  fi
  mv "$unpacked" "$PLUGIN_DIR" || { rm -rf "$staging"; return 1; }
  rm -rf "$staging"
}

# Different servers can share the downloaded directory. Keep this lock until
# execution ends, so another server cannot swap the plugin while it loads.
lock_directory "$LOCK_DIR" 120 || exit 0
PLUGIN_LOCKED=1
respect_loaded_copy || exit 0

# A copy this script installs is verified like the managed one, even at an
# explicit option/env path.
VERIFY_PLUGIN="$MANAGED_PLUGIN"
if [ ! -d "$PLUGIN_DIR" ]; then
  VERIFY_PLUGIN=1
  # Self-heal with the plugin release tested against this pmux version.
  if ! install_pinned_plugin; then
    tmux display-message "[tmux-ai-attn] plugin unavailable (see $LOG_FILE)" 2>/dev/null || true
    exit 0
  fi
fi

# Keep only pmux's default managed copy pinned; explicit option/env paths are
# treated as developer-managed checkouts and are never changed here. An older
# release, including a git checkout from an earlier pmux, is replaced by a
# fresh download of the pinned commit.
if [ "$MANAGED_PLUGIN" -eq 1 ] && [ "$(plugin_head)" != "$REQUIRED_PLUGIN_COMMIT" ]; then
  if ! install_pinned_plugin; then
    log "plugin upgrade to $REQUIRED_PLUGIN_VERSION ($REQUIRED_PLUGIN_COMMIT) failed at $PLUGIN_DIR"
    tmux display-message "[tmux-ai-attn] plugin upgrade failed (see $LOG_FILE)" 2>/dev/null || true
    exit 0
  fi
fi

# Never execute plugin code that is not the reviewed commit. Developer
# checkouts at an explicit @ai_attn_plugin_dir / PMUX_AI_ATTN_PLUGIN_DIR path
# that already existed are the user's own and are not checked.
if [ "$VERIFY_PLUGIN" -eq 1 ]; then
  actual_head=$(plugin_head)
  if [ "$actual_head" != "$REQUIRED_PLUGIN_COMMIT" ]; then
    log "refusing to run tmux-ai-attn at $PLUGIN_DIR: HEAD is ${actual_head:-unknown}, expected $REQUIRED_PLUGIN_COMMIT ($REQUIRED_PLUGIN_VERSION); Remove the directory to download it again, or point @ai_attn_plugin_dir at a checkout you trust."
    tmux display-message "[tmux-ai-attn] refusing to run unverified plugin at $PLUGIN_DIR (see $LOG_FILE)" 2>/dev/null || true
    exit 0
  fi
fi

entry="$PLUGIN_DIR/tmux-ai-attn.tmux"
if [ -x "$entry" ]; then
  respect_loaded_copy || exit 0
  clear_stale_hooks
  if "$entry"; then
    tmux set-option -gq @pmux_ai_attn_loaded "$PLUGIN_DIR"
  else
    log "plugin entry exited non-zero: $entry"
  fi
else
  log "plugin entry not executable: $entry"
  tmux display-message "[tmux-ai-attn] plugin entry missing at $entry" 2>/dev/null || true
fi

clear_legacy_bell_hook
