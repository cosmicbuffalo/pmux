#!/usr/bin/env ruby
# frozen_string_literal: true

# Compile tmux/pmux.rb into the plain tmux files in templates/, which pmux
# embeds and writes to ~/.config/pmux/. Run after changing pmux.rb:
#
#   make templates                 # or: ruby tmux/build.rb
#   ruby tmux/build.rb --check     # exit 1 if a template is out of date (CI)
#
# Needs the tmux gem (tmux.rb): installed, or on the load path with
# TMUX_RB=/path/to/tmux_rb make templates.

require_relative "pmux"

check = ARGV.delete("--check")
abort "usage: build.rb [--check]" unless ARGV.empty?

root = File.expand_path("../templates", __dir__)
stale = Pmux.templates.filter_map do |name, config|
  path = File.join(root, name)
  text = config.to_tmux
  next if File.exist?(path) && File.read(path) == text

  File.write(path, text) unless check
  name
end

if check && !stale.empty?
  warn "out of date: #{stale.join(', ')} (run `make templates`)"
  exit 1
end
puts(stale.empty? ? "templates are up to date" : "wrote #{stale.join(', ')}") unless check && stale.empty?
