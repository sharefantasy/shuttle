#!/usr/bin/env bash

set -euo pipefail

platform="${1:?usage: publish-release.sh <github|gitea|gitee> [dist-dir]}"
dist_dir="${2:-dist/release}"
tag="${RELEASE_TAG:-$(git describe --tags --exact-match)}"
commit="${SOURCE_COMMIT:-$(git rev-parse HEAD)}"
version="$(python3 release/release_version.py --tag "$tag")"
owner="${RELEASE_OWNER:?RELEASE_OWNER is required}"
repo="${RELEASE_REPO:?RELEASE_REPO is required}"
notes="${RELEASE_NOTES:-release/v${version}.md}"
python3 release/verify_release_assets.py \
  --dist "$dist_dir" --tag "$tag" --require-image

case "$platform" in
  github)
    api_base="${GITHUB_API_URL:-https://api.github.com}"
    token_env="GITHUB_TOKEN"
    ;;
  gitea)
    api_base="${GITEA_API_URL:?GITEA_API_URL is required}"
    token_env="GITEA_TOKEN"
    ;;
  gitee)
    api_base="${GITEE_API_URL:-https://gitee.com/api/v5}"
    token_env="GITEE_TOKEN"
    ;;
  *)
    echo "unsupported release platform: $platform" >&2
    exit 2
    ;;
esac

args=(
  --platform "$platform"
  --api-base "$api_base"
  --owner "$owner"
  --repo "$repo"
  --token-env "$token_env"
  --tag "$tag"
  --commit "$commit"
  --title "Shuttle $tag"
  --notes "$notes"
  --dist "$dist_dir"
)
if [[ "${RELEASE_DRY_RUN:-0}" == "1" ]]; then
  args+=(--dry-run)
fi
python3 release/release_platform.py "${args[@]}"
