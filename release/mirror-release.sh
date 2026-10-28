#!/usr/bin/env bash

set -euo pipefail

platform="${1:?usage: mirror-release.sh <gitea|gitee>}"
case "$platform" in
  gitea|gitee) ;;
  *) echo "mirror release only supports gitea or gitee" >&2; exit 2 ;;
esac

export RELEASE_TAG="${RELEASE_TAG:-$(git describe --tags --exact-match)}"
SOURCE_COMMIT="$(git rev-parse "${SOURCE_COMMIT:-HEAD}^{commit}")"
export SOURCE_COMMIT
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(python3 release/release_version.py --tag "$RELEASE_TAG" --source-epoch)}"
./release/preflight.sh mirror
python3 release/release_version.py --tag "$RELEASE_TAG" >/dev/null
PYTHONPATH=release python3 -c 'from release_version import validate_git_coordinates; import os; validate_git_coordinates(os.environ["RELEASE_TAG"], os.environ["SOURCE_COMMIT"])'

dist_dir="${RELEASE_DIST:-dist/release}"
rm -rf "$dist_dir"
mkdir -p "$dist_dir"
canonical_url="${CANONICAL_RELEASE_BASE_URL:?CANONICAL_RELEASE_BASE_URL is required}"
canonical_url="${canonical_url//\{tag\}/$RELEASE_TAG}"
./release/import-release-assets.sh \
  "$canonical_url" \
  "$dist_dir"
rm -f "$dist_dir/upstream-SHA256SUMS"

registry="${CONTAINER_REGISTRY:?CONTAINER_REGISTRY is required}"
image="${CONTAINER_IMAGE:?CONTAINER_IMAGE is required}"
printf '%s' "${CONTAINER_PASSWORD:?CONTAINER_PASSWORD is required}" | \
  docker login "$registry" \
    --username "${CONTAINER_USERNAME:?CONTAINER_USERNAME is required}" \
    --password-stdin
trap 'docker logout "$registry" >/dev/null 2>&1 || true' EXIT
export DOCKER_METADATA_FILE="$dist_dir/container-images.json"
export DOCKER_DIGEST_FILE="$dist_dir/container.digest"
export SHUTTLE_TOOLS_DIST="$dist_dir"
./release/docker-release.sh push "$image"
rm -f "$DOCKER_DIGEST_FILE"

CONTAINER_IMAGES_FILE="$dist_dir/container-images.json" \
  ./release/assemble-from-artifacts.sh "$dist_dir" "$dist_dir"
./release/publish-release.sh "$platform" "$dist_dir"
