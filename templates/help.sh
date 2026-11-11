#!/usr/bin/env sh
# The installer or first pmux command creates ~/.config/pmux/help.sh.
# pmux overwrites it when the generated-config version changes or any expected
# generated file is missing. Do not edit the installed copy.
#
# The hints come from the server's live key bindings (`pmux key-hints`), so
# there is nothing to edit here to change them: give your own bindings a note
# and they appear, grouped by the note's "group:" prefix, for example
#   bind -N "git: open lazygit" g display-popup -E lazygit
# Bindings without a note are listed by their command; untouched tmux
# defaults are left out.
#
#   help.sh --open CLIENT  open the key hints popup on CLIENT, sized to fit
#   help.sh --popup        draw the hints and wait for a key (runs in the popup)
#   help.sh --full         page the hints with less
#   help.sh                print the hints
set -eu

pmux_bin=${PMUX_BIN:-pmux}

render_help() {
  "$pmux_bin" key-hints "$@" 2>/dev/null ||
    printf ' pmux could not read the key bindings.\n Run: %s key-hints\n' "$pmux_bin"
}

case "${1:-}" in
  --open)
    # Size the popup to the hints so it never needs to scroll; tmux shrinks
    # it to fit a smaller client.
    hints=$(render_help)
    rows=$(printf '%s\n' "$hints" | wc -l)
    cols=$(printf '%s\n' "$hints" | awk '{ if (length > w) w = length } END { print w }')
    tmux display-popup -c "${2:?client required}" -w $((cols + 4)) -h $((rows + 4)) \
      -T " pmux " -E "\"$0\" --popup"
    ;;
  --popup)
    render_help --color
    printf '\n \033[2mpress any key to close\033[0m'
    # One raw keypress closes the popup, Esc and arrows included.
    stty -icanon -echo 2>/dev/null || true
    dd bs=1 count=1 >/dev/null 2>&1 || true
    stty icanon echo 2>/dev/null || true
    ;;
  --full)
    if command -v less >/dev/null 2>&1; then
      render_help --color | LESS='' less -R +g
    else
      render_help
      printf '\nPress Enter to close...'
      read -r _
    fi
    ;;
  *)
    render_help
    ;;
esac
