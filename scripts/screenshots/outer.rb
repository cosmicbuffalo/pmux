# frozen_string_literal: true

# The tmux the README screenshots run pmux inside: a Catppuccin Mocha status
# bar and pane borders, with pmux's integration and window tabs from its
# tmux.rb module. scripts/screenshots.py compiles this with tmux.rb.

require_relative "../../tmux/pmux"

# https://catppuccin.com/palette (Mocha)
mocha = {
  crust: "#11111b", mantle: "#181825", base: "#1e1e2e",
  surface0: "#313244", surface1: "#45475a", overlay0: "#6c7086",
  text: "#cdd6f4", subtext0: "#a6adc8",
  blue: "#89b4fa", mauve: "#cba6f7", peach: "#fab387", sky: "#89dceb"
}

Tmux.configure do
  global.mouse = true
  global.base_index = 1
  server.terminal_features << ",*:RGB"

  global.status.style = {fg: mocha[:text], bg: mocha[:mantle]}
  global.status.left.length = 40
  global.status.left = Tmux.format do
    style(fg: mocha[:crust], bg: mocha[:mauve], attrs: :bold, p: 1) { append Tmux.session_name }
    reset_style
    text " "
  end
  global.status.right = Tmux.format do
    style(fg: mocha[:subtext0], p: 1) { text "pmux demo" }
  end
  window.status.separator = " "
  window.pane.border.style = {fg: mocha[:surface0]}
  window.pane.active.border.style = {fg: mocha[:blue]}

  # pmux's window tabs: index, ai-attn's indicator, and name, lit in the
  # sidebar's flash color while it points at a process in that window.
  window.status.format = Pmux.window_tab(
    index: {fg: mocha[:text], bg: mocha[:surface1]},
    name: {fg: mocha[:subtext0], bg: mocha[:surface0]},
    flash: {fg: mocha[:crust], bg: mocha[:sky]},
    gap: true
  )
  window.status.current_format = Pmux.window_tab(
    current: true,
    index: {fg: mocha[:crust], bg: mocha[:peach], attrs: :bold},
    name: {fg: mocha[:text], bg: mocha[:surface1]},
    flash: {fg: mocha[:crust], bg: mocha[:sky]},
    gap: true
  )

  append Pmux.integration
  append Pmux.ai_attn
  global[:@ai_attn_color_text_fg] = mocha[:text]
  # A still picture should not catch a tab mid-flash: no flash on visiting
  # a window, and a waiting tab's "new" flash looks the same in both phases.
  global[:@ai_attn_seen_flash_seconds] = 0
  global[:@ai_attn_color_flash_bg] = "default"
  global[:@ai_attn_color_flash_fg] = "default"
end
