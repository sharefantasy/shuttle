#!/usr/bin/env bash

set -euo pipefail

dist_dir="${1:-dist}"
version="$(python3 release/release_version.py --tag "${RELEASE_TAG:?RELEASE_TAG is required}")"
raw="$dist_dir/.linux-export"
rm -rf "$raw"
mkdir -p "$raw"

for entry in "linux/amd64:linux-amd64" "linux/arm64:linux-arm64"; do
  platform="${entry%%:*}"
  suffix="${entry##*:}"
  platform_dir="${platform//\//_}"
  docker buildx build \
    --platform "$platform" \
    --target binary-export \
    --output "type=local,dest=$raw/$platform_dir" \
    --build-arg "VERSION=$version" \
    --build-arg "VCS_REF=${SOURCE_COMMIT:-unknown}" \
    .
  source="$raw/$platform_dir/shuttle"
  [[ -f "$source" ]] || { echo "missing buildx binary: $source" >&2; exit 1; }
  python3 release/package_release_binary.py \
    --binary "$source" \
    --suffix "$suffix" \
    --dist "$dist_dir"
done
rm -rf "$raw"
