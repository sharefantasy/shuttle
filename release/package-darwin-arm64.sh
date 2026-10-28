#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "package-darwin-arm64 requires an Apple Silicon macOS host" >&2
  exit 1
fi

version="$(awk '
  /^\[workspace.package\]$/ { in_workspace_package = 1; next }
  /^\[/ { in_workspace_package = 0 }
  in_workspace_package && /^version = / { gsub(/[" ]/, "", $3); print $3; exit }
' Cargo.toml)"
if [[ -z "$version" ]]; then
  echo "workspace package version is missing" >&2
  exit 1
fi

binary="target/release/shuttle"
if [[ ! -x "$binary" ]]; then
  echo "release binary is missing: $binary" >&2
  exit 1
fi
if [[ "$("$binary" --version)" != "shuttle $version" ]]; then
  echo "release binary version does not match Cargo.toml" >&2
  exit 1
fi

package="shuttle-v${version}-darwin-arm64"
stage="dist/${package}"
archive="dist/${package}.tar.gz"
rm -rf "$stage"
mkdir -p "$stage"
install -m 755 "$binary" "$stage/shuttle"
install -m 644 README.md LICENSE "$stage/"
COPYFILE_DISABLE=1 tar -C dist -czf "$archive" "$package"
(cd dist && shasum -a 256 "${package}.tar.gz" > "${package}.tar.gz.sha256")
echo "packaged $archive"
