#!/usr/bin/env sh
set -eu
# pmux exports PMUX_BIN into every pane; never let the installed binary leak in.
unset PMUX_BIN

repo_dir=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/pmux-install-test.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) target="x86_64-unknown-linux-musl" ;;
  Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin:x86_64|Darwin:amd64) target="x86_64-apple-darwin" ;;
  Darwin:aarch64|Darwin:arm64) target="aarch64-apple-darwin" ;;
  *) echo "unsupported test platform" >&2; exit 1 ;;
esac

release_dir="$tmp_dir/release"
home_dir="$tmp_dir/home"
bin_dir="$home_dir/.local/bin"
share_dir="$home_dir/.local/share/pmux"
mkdir -p "$release_dir" "$home_dir"

asset="pmux-${target}"
printf '#!/usr/bin/env sh\nexit 0\n' > "$release_dir/$asset"
cp "$repo_dir/uninstall.sh" "$release_dir/pmux-uninstall.sh"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$release_dir" && sha256sum "$asset" pmux-uninstall.sh > checksums.txt)
else
  (cd "$release_dir" && shasum -a 256 "$asset" pmux-uninstall.sh > checksums.txt)
fi

# Reject unsupported tmux versions before downloading or writing installation
# files.
old_tmux_bin="$tmp_dir/old-tmux-bin"
old_home="$tmp_dir/old-home"
mkdir -p "$old_tmux_bin" "$old_home"
cat > "$old_tmux_bin/tmux" <<'EOF'
#!/usr/bin/env sh
printf 'tmux 3.2\n'
EOF
chmod +x "$old_tmux_bin/tmux"
if HOME="$old_home" \
  PATH="$old_tmux_bin:$PATH" \
  PMUX_BIN_DIR="$old_home/.local/bin" \
  PMUX_SHARE_DIR="$old_home/.local/share/pmux" \
  PMUX_RELEASE_BASE_URL="file://$release_dir" \
  PMUX_SKIP_INIT=1 \
  sh "$repo_dir/install.sh"; then
  echo "install unexpectedly accepted tmux 3.2" >&2
  exit 1
fi
test ! -e "$old_home/.local/bin/pmux"

HOME="$home_dir" \
PMUX_BIN_DIR="$bin_dir" \
PMUX_SHARE_DIR="$share_dir" \
PMUX_RELEASE_BASE_URL="file://$release_dir" \
PMUX_SKIP_SETUP=1 \
sh "$repo_dir/install.sh"

test -x "$bin_dir/pmux"
test -x "$share_dir/uninstall.sh"

# Refusing to uninstall active sessions must leave the installation and relay
# ownership record untouched.
fake_bin="$tmp_dir/fake-bin"
mkdir -p "$fake_bin" "$home_dir/.local/state/pmux"
cat > "$fake_bin/tmux" <<'EOF'
#!/usr/bin/env sh
case " $* " in
  *" list-sessions "*) printf 'active-session\n' ;;
esac
EOF
chmod +x "$fake_bin/tmux"
printf '999999\n' > "$home_dir/.local/state/pmux/ai_attn_relay.pid"
if HOME="$home_dir" PMUX_BIN_DIR="$bin_dir" PMUX_SHARE_DIR="$share_dir" \
  PATH="$fake_bin:$PATH" sh "$share_dir/uninstall.sh" </dev/null; then
  echo "uninstall unexpectedly succeeded with active sessions" >&2
  exit 1
fi
test -f "$home_dir/.local/state/pmux/ai_attn_relay.pid"
test -x "$bin_dir/pmux"
test -x "$share_dir/uninstall.sh"

HOME="$home_dir" PMUX_BIN_DIR="$bin_dir" PMUX_SHARE_DIR="$share_dir" \
  PMUX_SOCKET="pmux-install-test-$$" sh "$share_dir/uninstall.sh" --force --keep-data
test ! -e "$bin_dir/pmux"
test ! -e "$share_dir"

# --- Real release artifact ------------------------------------------------
# Everything above installs a stub. Below, the installer gets a real pmux
# binary (PMUX_INSTALL_TEST_BINARY, e.g. a downloaded release asset in CI, or
# a local build), runs its real `init`, and the installed binary must work.
real_binary=${PMUX_INSTALL_TEST_BINARY:-"$repo_dir/target/debug/pmux"}
if [ ! -x "$real_binary" ]; then
  cargo build --manifest-path "$repo_dir/Cargo.toml"
fi
expected_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/Cargo.toml" | head -1)

real_release="$tmp_dir/real-release"
real_home="$tmp_dir/real-home"
real_bin="$real_home/.local/bin"
real_share="$real_home/.local/share/pmux"
# tmux nests its socket below TMUX_TMPDIR; keep it short for macOS runners.
real_tmux_tmp=$(mktemp -d /tmp/pmux-it.XXXXXX)
real_socket="pmux-install-real-$$"
mkdir -p "$real_release" "$real_home/dotfiles"
cp "$real_binary" "$real_release/$asset"
cp "$repo_dir/uninstall.sh" "$real_release/pmux-uninstall.sh"
write_checksums() {
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$real_release" && sha256sum "$asset" pmux-uninstall.sh > checksums.txt)
  else
    (cd "$real_release" && shasum -a 256 "$asset" pmux-uninstall.sh > checksums.txt)
  fi
}
cleanup_real() {
  TMUX_TMPDIR="$real_tmux_tmp" tmux -L "$real_socket" kill-server >/dev/null 2>&1 || true
  rm -rf "$real_tmux_tmp"
}
trap 'cleanup_real; rm -rf "$tmp_dir"' EXIT HUP INT TERM

# A managed (stow/chezmoi-style) rc file: a symlink with user content.
printf 'export PMUX_TEST_USER_LINE=1\n' > "$real_home/dotfiles/bashrc"
ln -s "$real_home/dotfiles/bashrc" "$real_home/.bashrc"

real_env() {
  env -u TMUX HOME="$real_home" SHELL=/bin/bash TMUX_TMPDIR="$real_tmux_tmp" \
    PMUX_SOCKET="$real_socket" PMUX_BIN_DIR="$real_bin" PMUX_SHARE_DIR="$real_share" \
    PMUX_RELEASE_BASE_URL="file://$real_release" \
    PMUX_SETUP_ARGS="--skip ai-attn,dredge" "$@"
}

# A checksum mismatch must abort before anything is installed.
write_checksums
printf 'tampered' >> "$real_release/$asset"
if real_env sh "$repo_dir/install.sh" >/dev/null 2>&1; then
  echo "install accepted an asset that does not match checksums.txt" >&2
  exit 1
fi
test ! -e "$real_bin/pmux"

cp "$real_binary" "$real_release/$asset"
write_checksums
real_env sh "$repo_dir/install.sh"

# The installed binary is the real one and runs. A build with git history
# names its commit after the version: `pmux 0.1.0 (abc1234)`.
case "$(real_env "$real_bin/pmux" --version)" in
  "pmux $expected_version" | "pmux $expected_version ("*")") ;;
  *)
    echo "unexpected pmux --version: $(real_env "$real_bin/pmux" --version)" >&2
    exit 1
    ;;
esac
# init edited the rc file through the symlink, keeping the user's content.
test -L "$real_home/.bashrc"
grep -q '^export PMUX_TEST_USER_LINE=1$' "$real_home/dotfiles/bashrc"
grep -q '^# >>> pmux wrap >>>$' "$real_home/dotfiles/bashrc"
grep -q '^# <<< pmux wrap <<<$' "$real_home/dotfiles/bashrc"
# Generated config finds pmux via PMUX_BIN, not a hardcoded install path.
grep -q 'PMUX_BIN:-pmux' "$real_home/.config/pmux/tmux.conf"
test -f "$real_home/.config/pmux/outer-tmux.conf"

# The installed binary manages a real process on an isolated backend.
real_env "$real_bin/pmux" run -d -s install-smoke sleep 60 >/dev/null
real_env "$real_bin/pmux" ls | grep -q '^install-smoke '
real_env "$real_bin/pmux" kill install-smoke >/dev/null

# Uninstall reverses init: the binary, config, and rc block go; the user's
# rc content and the symlink stay.
real_env sh "$real_share/uninstall.sh" --force >/dev/null
test ! -e "$real_bin/pmux"
test ! -e "$real_share"
test ! -e "$real_home/.config/pmux"
test -L "$real_home/.bashrc"
grep -q '^export PMUX_TEST_USER_LINE=1$' "$real_home/dotfiles/bashrc"
if grep -q 'pmux wrap' "$real_home/dotfiles/bashrc"; then
  echo "uninstall left the pmux block in the rc file" >&2
  exit 1
fi

echo "install smoke test passed (stub and real artifact)"
