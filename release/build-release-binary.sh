#!/usr/bin/env bash

set -euo pipefail

target="${1:?usage: build-release-binary.sh <rust-target> [dist-dir]}"
dist_dir="${2:-dist}"
version="$(python3 release/release_version.py --tag "${RELEASE_TAG:-v$(python3 release/release_version.py)}")"

case "$target" in
  x86_64-unknown-linux-gnu) suffix=linux-amd64 ;;
  aarch64-unknown-linux-gnu) suffix=linux-arm64 ;;
  x86_64-apple-darwin) suffix=darwin-amd64 ;;
  aarch64-apple-darwin) suffix=darwin-arm64 ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac

toolchain="${RUSTUP_TOOLCHAIN:-stable}"
rustup target add --toolchain "$toolchain" "$target"
rustc_bin="$(rustup which --toolchain "$toolchain" rustc)"
RUSTC="$rustc_bin" rustup run "$toolchain" cargo build \
  --release --locked -p shuttle-cli --target "$target"
binary="target/$target/release/shuttle"
"$binary" --version | grep -Fx "shuttle $version"

python3 release/package_release_binary.py \
  --binary "$binary" \
  --suffix "$suffix" \
  --dist "$dist_dir"
