#!/usr/bin/env sh
set -eu

# Usage: test-coverage.sh [--html]
#   --html  also open an HTML line-level report in the browser

HTML=0
for arg in "$@"; do
  case "$arg" in
    --html) HTML=1 ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "cargo-llvm-cov not installed."
  echo "Install with: cargo install cargo-llvm-cov"
  exit 1
fi

# If rustup isn't available (e.g. system-packaged Rust on Arch), point cargo-llvm-cov
# at the system llvm binaries so it doesn't fall back to looking for llvm-tools-preview.
if ! command -v rustup >/dev/null 2>&1; then
  export LLVM_COV="${LLVM_COV:-$(command -v llvm-cov)}"
  export LLVM_PROFDATA="${LLVM_PROFDATA:-$(command -v llvm-profdata)}"
fi

# Run tests once, collect profdata only.
cargo llvm-cov --all-features --workspace --no-report

# Print per-file summary table to the terminal.
cargo llvm-cov report --summary-only

# Write lcov file for CI / IDE overlays.
cargo llvm-cov report --lcov --output-path target/lcov.info
echo ""
echo "lcov report: target/lcov.info"

if [ "$HTML" = "1" ]; then
  cargo llvm-cov report --html --open
fi
