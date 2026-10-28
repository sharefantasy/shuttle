#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "darwin-arm64 build must run on macOS" >&2
  exit 1
fi

if [[ "$(uname -m)" != "arm64" ]]; then
  echo "darwin-arm64 build expects an Apple Silicon host" >&2
  exit 1
fi

mkdir -p dist

cargo_bin="${CARGO_BIN:-}"
if [[ -z "$cargo_bin" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    cargo_bin="$(command -v cargo)"
  elif [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
    cargo_bin="${HOME}/.cargo/bin/cargo"
  else
    echo "cargo not found; install Rust or set CARGO_BIN" >&2
    exit 1
  fi
fi

"$cargo_bin" build --release -p shuttle-cli
install -m 755 target/release/shuttle dist/shuttle-darwin-arm64
