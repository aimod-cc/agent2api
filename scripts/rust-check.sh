#!/bin/bash
# Run the repository's own Rust checks on a runner that ships no Rust.
#
# The independent verifier runs the plan's argv in a minimal environment: PATH,
# LANG and HOME only. Nothing puts a toolchain on that PATH, so a bare `cargo`
# check dies with `[Errno 2] No such file or directory: 'cargo'` before the
# repository's tests ever start. This wrapper provisions the pinned toolchain
# once into a scratch directory outside the repository and then execs the
# requested command, so `cargo test` / `cargo build` keep working everywhere a
# contributor would run them.
#
#   scripts/rust-check.sh test --manifest-path desktop-tauri/src-tauri/server/Cargo.toml
#
# Layout (never inside the repository, so the verified patch stays untouched):
#   $HOME/.agent2api-rust/{tools,rustup,cargo,target}
#
# Overrides: RUST_TOOLCHAIN (default 1.98), CARGO_TARGET_DIR.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN="${RUST_TOOLCHAIN:-1.98}"
STORE="${AGENT2API_RUST_HOME:-$HOME/.agent2api-rust}"

export RUSTUP_HOME="${RUSTUP_HOME:-$STORE/rustup}"
export CARGO_HOME="${CARGO_HOME:-$STORE/cargo}"
# Build output goes to the repository's own target dir (`.gitignore`d, and where
# `desktop-tauri/src-tauri/.cargo/config.toml` points it anyway), so the GUI
# evidence check finds the binary it starts.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"

log() { printf '[rust-check] %s\n' "$*" >&2; }

cargo_on_path() {
  # Resolve on the *current* PATH only: a pre-provisioned toolchain on the image
  # is used as-is, and no other scratch directory is consulted.
  command -v cargo >/dev/null 2>&1
}

install_toolchain() {
  local python="${PYTHON:-python3}"
  mkdir -p "$STORE/tools" "$RUSTUP_HOME" "$CARGO_HOME"
  # The `rustup` PyPI wheel ships the official static binaries, so a runner
  # without Rust needs nothing but Python to bootstrap the pinned toolchain.
  if [ ! -x "$STORE/tools/bin/rustup" ]; then
    log "fetching rustup into $STORE/tools"
    "$python" -m pip install --disable-pip-version-check --break-system-packages \
      --no-deps --quiet --target "$STORE/tools" rustup
  fi
  log "installing Rust $TOOLCHAIN into $RUSTUP_HOME"
  "$STORE/tools/bin/rustup" toolchain install "$TOOLCHAIN" --profile minimal --no-self-update
  "$STORE/tools/bin/rustup" run "$TOOLCHAIN" cargo --version
}

if ! cargo_on_path; then
  install_toolchain
  # Put the resolved toolchain's bin dir on PATH without depending on the shim's
  # own PATH shimming.
  RUSTUP_BIN_DIR="$(dirname "$("$STORE/tools/bin/rustup" which --toolchain "$TOOLCHAIN" cargo)")"
  export PATH="$RUSTUP_BIN_DIR:$PATH"
fi

cd "$REPO"
log "cargo $* (target dir: $CARGO_TARGET_DIR)"
exec cargo "$@"
