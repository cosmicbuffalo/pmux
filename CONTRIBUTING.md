# Contributing to pmux

Thanks for your interest in contributing to pmux.

## Getting started

### Prerequisites

- Rust 1.88+ (`rustup` recommended)
- tmux 3.3a or newer
- `shellcheck` (for linting shell scripts)

### Build

```sh
cargo build
```

### Test

```sh
cargo test
```

### Format

```sh
cargo fmt
```

### Lint

```sh
cargo clippy --all-targets --all-features -- -D warnings
shellcheck install.sh uninstall.sh scripts/*.sh templates/*.sh
shellcheck -e SC1091 -s bash templates/shell-integration.sh.in
```

### tmux configuration

pmux's tmux configs are written in [tmux.rb](https://github.com/cosmicbuffalo/tmux.rb)
in `tmux/pmux.rb`; the files in `templates/` with a `.conf` extension are its
compiled output (except `shell-integration.sh.in` and the shell scripts, which
are hand-written). Edit `tmux/pmux.rb`, then regenerate and commit both:

```sh
make templates                   # needs the tmux gem, or TMUX_RB=/path/to/tmux_rb
make templates-check             # fails if templates/ is out of date
```

Upgrade code in `src/config.rs` matches some lines of users' existing files
exactly; when a binding or block changes shape, add the old form there so
existing configs migrate.

### Coverage and smoke tests

```sh
./scripts/test-coverage.sh
python3 scripts/queue-smoke.py
```

The queue smoke test creates its own temporary HOME, tmux socket, and
attached terminal, and drives the real message-queue popup end to end:
multiline editing, scheduling, combined delivery, pausing, process
replacement, and scheduler crash recovery. It never connects to your normal
pmux server.

### Screenshots

The README and docs screenshots are rendered from a real pmux setup with
default config, on throwaway tmux servers and an isolated home directory:

```sh
python3 scripts/screenshots.py
```

It needs tmux, vhs, ttyd, ffmpeg, Ruby with the tmux gem (or
`TMUX_RB=/path/to/tmux.rb`), and ai-attn. The surrounding tmux theme comes
from [scripts/screenshots/outer.rb](scripts/screenshots/outer.rb).

## Submitting changes

1. Fork the repo and create a feature branch from `main`.
2. Make your changes. Add tests where appropriate.
3. Run `cargo fmt`, `cargo clippy`, and `cargo test` before pushing.
4. Open a pull request against `main` with a clear description of the change.

## Code style

- Use `anyhow::Result` for error propagation; avoid `unwrap()`/`expect()` in non-test code.
- Keep dependencies minimal; prefer the standard library when practical.
- Follow existing patterns in the codebase for consistency.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
