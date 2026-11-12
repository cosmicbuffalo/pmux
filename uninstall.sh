#!/usr/bin/env sh
set -eu

BIN_DIR="${PMUX_BIN_DIR:-$HOME/.local/bin}"
SHARE_DIR="${PMUX_SHARE_DIR:-$HOME/.local/share/pmux}"
PMUX_SOCKET="${PMUX_SOCKET:-pmux}"
KEEP_DATA=0
FORCE=0

for arg in "$@"; do
  case "$arg" in
    --force|-f) FORCE=1 ;;
    --keep-data) KEEP_DATA=1 ;;
    --help|-h)
      echo "Usage: uninstall.sh [--force] [--keep-data]"
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 2
      ;;
  esac
done

sessions=$(TMUX='' tmux -L "$PMUX_SOCKET" list-sessions -F '#{session_name}' 2>/dev/null || true)
if [ -n "$sessions" ] && [ "$FORCE" -ne 1 ]; then
  count=$(printf '%s\n' "$sessions" | wc -l | tr -d ' ')
  if [ -t 0 ] && [ -r /dev/tty ]; then
    printf 'There are %s active pmux session(s). Uninstall and kill them? [y/N] ' "$count" > /dev/tty
    read -r answer < /dev/tty
    case "$answer" in
      y|Y|yes|YES) ;;
      *) echo "Aborted."; exit 1 ;;
    esac
  else
    echo "Active pmux sessions found; rerun with --force in non-interactive mode." >&2
    exit 1
  fi
fi

relay_pid_file="$HOME/.local/state/pmux/ai_attn_relay.pid"
if [ -f "$relay_pid_file" ]; then
  relay_pid=$(cat "$relay_pid_file" 2>/dev/null || true)
  case "$relay_pid" in
    ''|*[!0-9]*) ;;
    *)
      relay_command=$(ps -p "$relay_pid" -o command= 2>/dev/null || true)
      case "$relay_command" in
        *"pmux relay-ai-attn --loop"*)
          kill "$relay_pid" 2>/dev/null || true
          attempts=0
          while kill -0 "$relay_pid" 2>/dev/null && [ "$attempts" -lt 20 ]; do
            sleep 0.1
            attempts=$((attempts + 1))
          done
          if kill -0 "$relay_pid" 2>/dev/null; then
            kill -KILL "$relay_pid" 2>/dev/null || true
          fi
          ;;
      esac
      ;;
  esac
  rm -f "$relay_pid_file"
fi

# Stop the backend even when it is intentionally alive with zero sessions.
TMUX='' tmux -L "$PMUX_SOCKET" kill-server >/dev/null 2>&1 || true

systemd_unit="$HOME/.config/systemd/user/pmux-restore.service"
if [ -e "$systemd_unit" ]; then
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now pmux-restore.service >/dev/null 2>&1 || true
  fi
  rm -f "$systemd_unit"
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
  fi
fi

launchd_plist="$HOME/Library/LaunchAgents/dev.pmux.restore.plist"
if [ -e "$launchd_plist" ]; then
  if command -v launchctl >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)" "$launchd_plist" >/dev/null 2>&1 ||
      launchctl unload -w "$launchd_plist" >/dev/null 2>&1 || true
  fi
  rm -f "$launchd_plist"
fi

remove_pmux_block() {
  file=$1
  if [ -f "$file" ] && grep -q '^# >>> pmux wrap >>>' "$file"; then
    # Without the end marker the sed range would run to end of file and
    # delete the user's own configuration, so leave the file alone instead.
    if ! grep -q '^# <<< pmux wrap <<<' "$file"; then
      echo "warning: $file has a pmux block without its end marker; remove it by hand" >&2
      return
    fi
    tmp=$(mktemp)
    sed '/^# >>> pmux wrap >>>/,/^# <<< pmux wrap <<</d' "$file" > "$tmp"
    # Rewrite in place so symlinked (stow/chezmoi) rc files and their
    # permissions survive.
    cat "$tmp" > "$file"
    rm -f "$tmp"
  fi
}

# Print the files an rc file sources (`source FILE` or `. FILE`, including the
# `[[ -s FILE ]] && source FILE` form) with ~, $HOME and ${HOME} expanded.
# pmux init keeps its block in such a file when the user moved it there, so
# the uninstaller has to look there too. Mirrors sourced_paths in config.rs.
sourced_rc_files() {
  awk -v home="$HOME" '
    /^[[:space:]]*#/ { next }
    {
      n = split($0, segs, /[;&|]/)
      for (i = 1; i <= n; i++) {
        seg = segs[i]
        sub(/^[[:space:]]+/, "", seg)
        if (seg !~ /^(source|\.)[[:space:]]/) continue
        sub(/^(source|\.)[[:space:]]+/, "", seg)
        split(seg, words, /[[:space:]]+/)
        p = words[1]
        gsub(/["\047]/, "", p)
        if (substr(p, 1, 2) == "~/") p = home substr(p, 2)
        else if (substr(p, 1, 7) == "${HOME}") p = home substr(p, 8)
        else if (substr(p, 1, 5) == "$HOME") p = home substr(p, 6)
        if (p ~ /^\// && p !~ /\$/) print p
      }
    }' "$1"
}

for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
  [ -f "$rc" ] || continue
  remove_pmux_block "$rc"
  sourced_rc_files "$rc" | while IFS= read -r sourced; do
    remove_pmux_block "$sourced"
  done
done

rm -f "$BIN_DIR/pmux"
if [ "$KEEP_DATA" -ne 1 ]; then
  rm -rf "$HOME/.config/pmux" "$HOME/.local/state/pmux"
fi
rm -f "$SHARE_DIR/uninstall.sh"
# PMUX_SHARE_DIR may name any directory, so remove only the file pmux put
# there and the directory itself when that leaves it empty, never `rm -rf`.
if [ -d "$SHARE_DIR" ] && ! rmdir "$SHARE_DIR" 2>/dev/null; then
  echo "warning: left $SHARE_DIR in place because it holds other files" >&2
fi

echo "pmux uninstalled."
if [ "$KEEP_DATA" -ne 1 ]; then
  echo "If your tmux config sources ~/.config/pmux/outer-tmux.conf, remove that line"
  echo "(or use 'source-file -q', which ignores the missing file)."
fi
if [ "$KEEP_DATA" -eq 1 ]; then
  echo "Configuration and state were preserved."
fi
if [ -x "$HOME/.local/share/ai-attn/uninstall.sh" ] || [ -x "$HOME/.local/bin/dredge" ]; then
  echo "ai-attn and Dredge, which 'pmux setup' installs, were left in place:"
  if [ -x "$HOME/.local/share/ai-attn/uninstall.sh" ]; then
    echo "  bash ~/.local/share/ai-attn/uninstall.sh   # removes ai-attn and its agent hooks"
  fi
  if [ -x "$HOME/.local/bin/dredge" ]; then
    echo "  rm ~/.local/bin/dredge                     # its index is in ~/.local/share/dredge"
  fi
fi
