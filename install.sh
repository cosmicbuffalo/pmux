#!/usr/bin/env sh
# pmux installer. Everything runs from main(), called on the last line, so a
# download truncated mid-way by `curl | sh` defines functions but runs nothing.
#
# Environment:
#   PMUX_VERSION              release to install (default: latest)
#   PMUX_BIN_DIR              binary directory (default: ~/.local/bin)
#   PMUX_SHARE_DIR            uninstaller directory (default: ~/.local/share/pmux)
#   PMUX_SKIP_SETUP=1         do not run `pmux setup`, which edits your shell rc
#                             and tmux config and installs ai-attn and Dredge
#                             (PMUX_SKIP_INIT=1 is the old name and still works)
#   PMUX_SETUP_ARGS           extra arguments for `pmux setup`, such as
#                             "--yes" or "--skip dredge"
#   PMUX_SKIP_TMUX_CHECK=1    install even if tmux is missing or not recognized
#   PMUX_VERIFY_ATTESTATION   auto (default): verify GitHub build provenance when
#                             an authenticated `gh` is available; 1: require it;
#                             0: skip it
set -eu

REPO="${PMUX_REPO:-cosmicbuffalo/pmux}"
VERSION="${PMUX_VERSION:-latest}"
BIN_DIR="${PMUX_BIN_DIR:-$HOME/.local/bin}"
SHARE_DIR="${PMUX_SHARE_DIR:-$HOME/.local/share/pmux}"

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)
      echo "Unsupported architecture: $arch" >&2
      exit 1
      ;;
  esac

  case "$os" in
    linux) printf '%s-unknown-linux-musl\n' "$arch" ;;
    darwin) printf '%s-apple-darwin\n' "$arch" ;;
    *)
      echo "Unsupported operating system: $os" >&2
      exit 1
      ;;
  esac
}

download() {
  url=$1
  destination=$2
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$destination"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$destination" "$url"
  else
    echo "curl or wget is required" >&2
    exit 1
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "sha256sum or shasum is required" >&2
    exit 1
  fi
}

verify_asset() {
  file=$1
  name=$2
  expected=$(awk -v name="$name" '$2 == name || $2 == "*" name {print $1; exit}' "$checksums")
  if [ -z "$expected" ]; then
    echo "Checksum entry not found for $name" >&2
    exit 1
  fi
  actual=$(sha256_file "$file")
  if [ "$actual" != "$expected" ]; then
    echo "Checksum mismatch for $name" >&2
    exit 1
  fi
}

# Check GitHub build provenance for the downloaded files. checksums.txt comes
# from the same release as the binary, so on its own it only catches corrupted
# downloads; an attestation proves the files were built by this repository's
# release workflow. Optional by default: skipped (with a note) when gh is
# missing, too old, or not logged in, and for custom PMUX_RELEASE_BASE_URL
# mirrors. A failed verification is always fatal.
verify_attestations() {
  mode=${PMUX_VERIFY_ATTESTATION:-auto}
  case "$mode" in
    0|no|false) return 0 ;;
    1|yes|true) required=1 ;;
    *)
      required=0
      if [ -n "${PMUX_RELEASE_BASE_URL:-}" ]; then
        return 0
      fi
      ;;
  esac

  reason=""
  if ! command -v gh >/dev/null 2>&1; then
    reason="gh is not installed"
  elif ! gh attestation --help >/dev/null 2>&1; then
    reason="this gh has no 'attestation' command"
  elif ! gh auth status >/dev/null 2>&1; then
    reason="gh is not logged in"
  fi
  if [ -n "$reason" ]; then
    if [ "$required" -eq 1 ]; then
      echo "Cannot verify build provenance: $reason (PMUX_VERIFY_ATTESTATION=1)" >&2
      exit 1
    fi
    echo "Skipping build provenance check: $reason."
    echo "  Verify later with: gh attestation verify \"$BIN_DIR/pmux\" --repo $REPO"
    return 0
  fi

  echo "Verifying build provenance with gh..."
  for file in "$@"; do
    if ! gh attestation verify "$file" --repo "$REPO" >/dev/null; then
      echo "Build provenance verification FAILED for $(basename "$file"); not installing." >&2
      echo "  Set PMUX_VERIFY_ATTESTATION=0 only if you trust this source anyway." >&2
      exit 1
    fi
  done
}

tmux_version_supported() {
  printf '%s\n' "$1" | awk '
    {
      version = $2
      # Builds from tmux git report "next-X.Y" for the upcoming X.Y release.
      sub(/^next-/, "", version)
      if (version !~ /^[0-9]+\.[0-9]+/) {
        exit 1
      }
      split(version, parts, ".")
      major = parts[1] + 0
      minor = parts[2] + 0
      suffix = parts[2]
      sub(/^[0-9]+/, "", suffix)

      if (major > 3 || (major == 3 && minor > 3)) {
        exit 0
      }
      if (major == 3 && minor == 3 && suffix >= "a") {
        exit 0
      }
      exit 1
    }
  '
}

check_tmux() {
  if [ "${PMUX_SKIP_TMUX_CHECK:-0}" = "1" ]; then
    echo "Skipping the tmux check (PMUX_SKIP_TMUX_CHECK=1); pmux needs tmux 3.3a or newer to run."
    return 0
  fi
  if ! command -v tmux >/dev/null 2>&1; then
    echo "tmux is required" >&2
    echo "  To install pmux anyway, rerun with PMUX_SKIP_TMUX_CHECK=1" >&2
    exit 1
  fi
  tmux_version=$(tmux -V 2>/dev/null || true)
  if ! tmux_version_supported "$tmux_version"; then
    echo "tmux 3.3a or newer is required (found: ${tmux_version:-unknown})" >&2
    echo "  If this tmux is new enough but not recognized, rerun with PMUX_SKIP_TMUX_CHECK=1" >&2
    exit 1
  fi
}

main() {
  check_tmux

  target=$(detect_target)
  asset="pmux-${target}"

  if [ -n "${PMUX_RELEASE_BASE_URL:-}" ]; then
    download_base=${PMUX_RELEASE_BASE_URL%/}
  elif [ "$VERSION" = "latest" ]; then
    download_base="https://github.com/${REPO}/releases/latest/download"
  else
    case "$VERSION" in
      v*) tag=$VERSION ;;
      *) tag="v${VERSION}" ;;
    esac
    download_base="https://github.com/${REPO}/releases/download/${tag}"
  fi

  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/pmux-install.XXXXXX")
  trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
  binary="$tmp_dir/$asset"
  uninstaller="$tmp_dir/pmux-uninstall.sh"
  checksums="$tmp_dir/checksums.txt"

  echo "Downloading pmux ${VERSION} for ${target}..."
  download "$download_base/$asset" "$binary"
  download "$download_base/pmux-uninstall.sh" "$uninstaller"
  download "$download_base/checksums.txt" "$checksums"
  verify_asset "$binary" "$asset"
  verify_asset "$uninstaller" "pmux-uninstall.sh"
  # checksums.txt covers the uninstaller as well as the binary.
  verify_attestations "$binary" "$checksums"

  mkdir -p "$BIN_DIR" "$SHARE_DIR"
  install -m 0755 "$binary" "$BIN_DIR/pmux"
  install -m 0755 "$uninstaller" "$SHARE_DIR/uninstall.sh"

  if [ "${PMUX_SKIP_SETUP:-${PMUX_SKIP_INIT:-0}}" != "1" ]; then
    echo "Running 'pmux setup': it adds a marked block to your shell rc file,"
    echo "installs ai-attn (agent state) and Dredge (session search), wires"
    echo "ai-attn into your agents, and offers to wrap your agents and connect"
    echo "pmux to your tmux. Set PMUX_SKIP_SETUP=1 to skip it; rerun it any time"
    echo "with 'pmux setup'."
    # Under `curl | sh` this script is stdin, so give setup the terminal for
    # its questions when there is one. Setup also reloads a running pmux
    # server, which keeps the old binary's config until then.
    setup=setup
    # Releases before `pmux setup` (PMUX_VERSION=v0.1.0) only have init.
    if ! "$BIN_DIR/pmux" setup --help >/dev/null 2>&1; then
      setup=init
    fi
    # shellcheck disable=SC2086 # PMUX_SETUP_ARGS is a word list on purpose
    if (: </dev/tty) 2>/dev/null; then
      "$BIN_DIR/pmux" "$setup" ${PMUX_SETUP_ARGS:-} </dev/tty ||
        echo "pmux setup did not finish; rerun 'pmux setup'." >&2
    else
      "$BIN_DIR/pmux" "$setup" ${PMUX_SETUP_ARGS:-} ||
        echo "pmux setup did not finish; rerun 'pmux setup'." >&2
    fi
    if [ "$setup" = init ]; then
      "$BIN_DIR/pmux" restart ||
        echo "Could not reload the running pmux server; run 'pmux restart'." >&2
    fi
  fi

  echo "pmux installed to $BIN_DIR/pmux"
  echo "Ensure $BIN_DIR is on PATH"
  echo "Uninstall with: $SHARE_DIR/uninstall.sh"
}

main "$@"
