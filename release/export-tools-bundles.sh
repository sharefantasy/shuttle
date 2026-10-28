#!/usr/bin/env bash

set -euo pipefail

dist_dir="${1:-dist/release}"
version="$(python3 release/release_version.py --tag "${RELEASE_TAG:?RELEASE_TAG is required}")"
source_epoch="${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH is required}"
mkdir -p "$dist_dir"
stage_root="$(mktemp -d)"
trap 'rm -rf "$stage_root"' EXIT

for arch in amd64 arm64; do
  output="$stage_root/$arch"
  docker buildx build \
    --platform "linux/$arch" \
    --target tools-bundle-export \
    --build-arg "VERSION=$version" \
    --output "type=local,dest=$output" .
  archive="$(python3 release/external_tools_bundle.py export \
    --root "$output" --dist "$dist_dir" --source-date-epoch "$source_epoch")"
  python3 release/external_tools_bundle.py verify-archive "$archive"
done
