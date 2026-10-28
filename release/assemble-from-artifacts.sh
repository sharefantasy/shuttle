#!/usr/bin/env bash

set -euo pipefail

artifacts_dir="${1:?usage: assemble-from-artifacts.sh <artifacts-dir> [dist-dir]}"
dist_dir="${2:-dist/release}"
mkdir -p "$dist_dir"
if [[ "$(cd "$artifacts_dir" && pwd -P)" != "$(cd "$dist_dir" && pwd -P)" ]]; then
  find "$artifacts_dir" -type f -name 'shuttle-v*.tar.gz' -exec cp {} "$dist_dir/" \;
  find "$artifacts_dir" -type f -name 'shuttle-tools-v*' -exec cp {} "$dist_dir/" \;
fi

version="$(python3 release/release_version.py --tag "${RELEASE_TAG:?RELEASE_TAG is required}")"
archives=()
while IFS= read -r archive; do
  archives+=("$archive")
done < <(find "$dist_dir" -maxdepth 1 -type f -name "shuttle-v${version}-*.tar.gz" -print | sort)
if [[ "${#archives[@]}" -ne 4 ]]; then
  echo "expected 4 binary archives, found ${#archives[@]}" >&2
  exit 1
fi

args=(
  --dist "$dist_dir"
  --tag "$RELEASE_TAG"
  --source-commit "${SOURCE_COMMIT:?SOURCE_COMMIT is required}"
)
if [[ -n "${IMAGE_REF:-}" || -n "${IMAGE_DIGEST:-}" ]]; then
  args+=(--image-ref "${IMAGE_REF:?IMAGE_REF is required with IMAGE_DIGEST}")
  args+=(--image-digest "${IMAGE_DIGEST:?IMAGE_DIGEST is required with IMAGE_REF}")
fi
if [[ -n "${CONTAINER_IMAGES_FILE:-}" ]]; then
  args+=(--container-images "$CONTAINER_IMAGES_FILE")
fi
python3 release/assemble_release.py "${args[@]}"
python3 release/verify_release_assets.py --dist "$dist_dir" --tag "$RELEASE_TAG"
