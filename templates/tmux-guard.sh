#!/bin/sh
# The installer or first pmux command creates ~/.config/pmux/bin/tmux.
# pmux overwrites it when the generated-config version changes or any expected
# generated file is missing. Do not edit the installed copy.
#
# pmux's tmux guard. pmux puts this directory first on PATH for the processes
# it runs. Inside a pmux pane, $TMUX names pmux's dedicated tmux server, so a
# bare `tmux source-file ~/.tmux.conf`, `tmux set -g ...`, or
# `tmux kill-server` typed there (or run by an agent) would reconfigure or
# destroy pmux instead of your own tmux. This wrapper refuses those commands
# and says how to run them where they were meant to go. Everything else runs
# the real tmux unchanged, as does any command that:
#   - names its server with -L or -S,
#   - runs outside a pmux pane ($TMUX names another server, or none),
#   - runs without a terminal (pmux's own scripts and tmux hooks), or
#   - runs with PMUX_ALLOW_TMUX=1 set.

self_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

# Another copy of this guard (another HOME's, as in pmux's tests, or an older
# install) may also be on PATH. Skip every guard, not just this one: two
# guards that each run the other as "the real tmux" would loop forever.
is_guard() {
  head -c 1024 "$1" 2>/dev/null | grep -q "^# pmux's tmux guard"
}

real=${PMUX_REAL_TMUX:-}
if [ -z "$real" ]; then
  old_ifs=$IFS
  IFS=:
  for dir in $PATH; do
    [ "$dir" = "$self_dir" ] && continue
    if [ -x "$dir/tmux" ] && [ ! -d "$dir/tmux" ] && ! is_guard "$dir/tmux"; then
      real="$dir/tmux"
      break
    fi
  done
  IFS=$old_ifs
fi
if [ -z "$real" ]; then
  echo "pmux: tmux not found on PATH" >&2
  exit 127
fi

pass() {
  exec "$real" "$@"
}

[ -n "${PMUX_ALLOW_TMUX:-}" ] && pass "$@"
case ${TMUX:-} in
  */"${PMUX_SOCKET:-pmux}",*) ;;
  *) pass "$@" ;;
esac
# Hooks and run-shell jobs have no controlling terminal; processes in a pane
# keep the pane's even when their input is piped, as an agent's commands are.
case $(ps -o tty= -p $$ 2>/dev/null | tr -d ' ') in
  '' | '?' | '??') pass "$@" ;;
esac

original="$*"

# tmux accepts any unambiguous prefix of a command name.
is() {
  word=$1
  shift
  for name in "$@"; do
    [ "$word" = "$name" ] && return 0
  done
  [ ${#word} -ge 4 ] || return 1
  for name in "$@"; do
    case $name in "$word"*) return 0 ;; esac
  done
  return 1
}

has_flag() {
  letters=$1
  shift
  for arg in "$@"; do
    case $arg in
      --) return 1 ;;
      -*["$letters"]*) return 0 ;;
    esac
  done
  return 1
}

reason=''
check() {
  command=$1
  shift
  if is "$command" kill-server; then
    reason="it would end every pmux process at once"
  elif is "$command" source-file source; then
    for arg in "$@"; do
      # tmux expands a leading ~ in source-file paths itself.
      # shellcheck disable=SC2088
      case $arg in
        -*) ;;
        "$HOME"/.config/pmux/* | '~/.config/pmux/'*) ;;
        *) reason="it would load $arg into pmux's server"; return ;;
      esac
    done
  elif is "$command" bind-key bind unbind-key unbind; then
    reason="it would change pmux's key bindings"
  elif is "$command" set-hook; then
    reason="it would change pmux's hooks"
  elif is "$command" set-option set set-window-option setw; then
    has_flag gs "$@" && reason="it would change a global option of pmux's server"
  elif is "$command" set-environment setenv; then
    has_flag g "$@" && reason="it would change the environment of every pmux process"
  elif is "$command" kill-session; then
    has_flag at "$@" && reason="it would end other pmux processes"
  fi
}

# Walk the arguments without changing them, so a command that passes runs
# exactly as given. tmux's own flags come first: -L and -S choose a server
# explicitly and -c runs a shell command, so those pass; -f and -T take a
# value. After them, commands are separated by `;` arguments.
in_flags=1
skip_value=''
current=''
for arg in "$@"; do
  if [ -n "$skip_value" ]; then
    skip_value=''
    continue
  fi
  if [ -n "$in_flags" ]; then
    case $arg in
      -L* | -S* | -c* | -V) pass "$@" ;;
      -f | -T) skip_value=1; continue ;;
      --) in_flags=''; continue ;;
      -?*) continue ;;
      *) in_flags='' ;;
    esac
  fi
  if [ "$arg" = ';' ] || [ "$arg" = '\;' ]; then
    # shellcheck disable=SC2086 # split the collected command back into words
    [ -n "$current" ] && check $current
    [ -n "$reason" ] && break
    current=''
  else
    current="$current $arg"
  fi
done
# shellcheck disable=SC2086
[ -z "$reason" ] && [ -n "$current" ] && check $current
[ -z "$reason" ] && pass "$@"

host=$("${PMUX_BIN:-pmux}" host-socket 2>/dev/null)
{
  echo "pmux: refusing \`tmux $original\`: $reason."
  echo "This runs inside pmux, where a bare \`tmux\` command talks to pmux's dedicated"
  echo "tmux server, not your own tmux."
  if [ -n "$host" ]; then
    echo "  For your own tmux:          tmux -S $host $original"
  fi
  echo "  To run it on pmux's server: PMUX_ALLOW_TMUX=1 tmux $original"
  echo "  If pmux's server was already changed by mistake: pmux restart --repair"
} >&2
exit 1
