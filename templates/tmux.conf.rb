# pmux's tmux server, configured with tmux.rb (https://github.com/cosmicbuffalo/tmux.rb).
#
# `pmux init --tmux-rb` created this file. pmux compiles it into
# ~/.config/pmux/tmux.conf with tmux.rb whenever pmux upgrades and on
# `pmux restart`, so edit this file rather than tmux.conf, then run
# `pmux restart`. pmux's defaults come from ~/.config/pmux/rb/pmux.rb, which
# pmux keeps up to date; your settings in the block below apply on top of
# them, only on pmux's server.
require File.expand_path("~/.config/pmux/rb/pmux")

Pmux.server do
  # Your settings for pmux's server, for example:
  #   global.prefix = "C-a"
  #   bind "g", "display-popup -E lazygit", note: "git: open lazygit"
end
