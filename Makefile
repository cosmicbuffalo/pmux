SHELL := /bin/sh

BIN_NAME := pmux
BIN_DIR ?= $(HOME)/.local/bin
TARGET_DIR := target/release
BIN_PATH := $(TARGET_DIR)/$(BIN_NAME)

.PHONY: build release install uninstall reinstall fmt test clean reload restart templates templates-check

build:
	cargo build

# Compile tmux/pmux.rb (tmux.rb) into templates/. TMUX_RB=/path/to/tmux_rb
# uses a checkout instead of the installed tmux gem.
RUBY_TMUX_RB := $(if $(TMUX_RB),-I "$(TMUX_RB)/lib")

templates:
	ruby $(RUBY_TMUX_RB) tmux/build.rb

templates-check:
	ruby $(RUBY_TMUX_RB) tmux/build.rb --check

release:
	cargo build --release

install: release
	mkdir -p "$(BIN_DIR)"
	install -m 0755 "$(BIN_PATH)" "$(BIN_DIR)/$(BIN_NAME)"
	mkdir -p "$(HOME)/.local/share/pmux"
	install -m 0755 uninstall.sh "$(HOME)/.local/share/pmux/uninstall.sh"
	@echo "Installed $(BIN_NAME) to $(BIN_DIR)/$(BIN_NAME)"
	@"$(BIN_DIR)/$(BIN_NAME)" init
	@"$(BIN_DIR)/$(BIN_NAME)" restart || echo "Could not reload the running pmux server; run 'pmux restart'." >&2

uninstall:
	@PMUX_BIN_DIR="$(BIN_DIR)" sh ./uninstall.sh

reinstall:
	./scripts/reinstall.sh

fmt:
	cargo fmt

test:
	cargo test

clean:
	cargo clean

reload:
	@tmux -L pmux source-file "$(HOME)/.config/pmux/tmux.conf"
	@tmux -L pmux show -gv prefix

restart:
	@tmux -L pmux kill-server >/dev/null 2>&1 || true
	@echo "pmux tmux server restarted. Next 'pmux run' or 'pmux attach' will start it with current config."
	@echo "Configured prefix:"
	@grep -E '^[[:space:]]*set -g prefix[[:space:]]+' "$(HOME)/.config/pmux/tmux.conf" | awk '{print $$4; exit}' || echo "(not set)"
