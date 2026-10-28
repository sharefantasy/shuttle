#!/usr/bin/env bash

set -euo pipefail

mode="${1:?usage: docker-release.sh <push|load> <base-image>}"
image="${2:?usage: docker-release.sh <push|load> <base-image>}"
version="$(python3 release/release_version.py --tag "${RELEASE_TAG:?RELEASE_TAG is required}")"
source_commit="${SOURCE_COMMIT:?SOURCE_COMMIT is required}"
source_url="${SOURCE_URL:-https://codeup.aliyun.com/67b894ecf6e36ba63f99da60/growth/shuttle}"

tags_for() {
  python3 release/container_release.py tags \
    --image "$image" --variant "$1" --version "$version"
}

metadata_digest() {
  python3 - "$1" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["containerimage.digest"])
PY
}

export_published_tools() {
  local platform="$1"
  local immutable_ref="$2"
  local arch="${platform#linux/}"
  local root="$metadata_dir/tools-$arch"
  local container_id
  mkdir -p "$root"
  container_id="$(docker create --platform "$platform" "$immutable_ref" true)"
  docker cp "$container_id:/opt/shuttle-tools/." "$root/"
  docker rm "$container_id" >/dev/null
  archive="$(python3 release/external_tools_bundle.py export \
    --root "$root" --dist "${SHUTTLE_TOOLS_DIST:?SHUTTLE_TOOLS_DIST is required}" \
    --source-date-epoch "${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH is required}")"
  python3 release/external_tools_bundle.py verify-archive "$archive"
}

case "$mode" in
  push)
    receipt="${DOCKER_METADATA_FILE:-dist/container-images.json}"
    metadata_dir="$(mktemp -d)"
    trap 'rm -rf "$metadata_dir"' EXIT
    for variant in core tools; do
      tag_args=()
      while IFS= read -r tag; do
        tag_args+=(--tag "$tag")
      done < <(tags_for "$variant")
      docker buildx build \
        --platform linux/amd64,linux/arm64 \
        --target "$variant" \
        --build-arg "VERSION=$version" \
        --build-arg "VCS_REF=$source_commit" \
        --build-arg "SOURCE_URL=$source_url" \
        "${tag_args[@]}" \
        --metadata-file "$metadata_dir/$variant.json" \
        --push .
      ref="$(tags_for "$variant" | head -1)@$(metadata_digest "$metadata_dir/$variant.json")"
      docker buildx imagetools inspect --raw "$ref" > "$metadata_dir/$variant.index.json"
    done
    core_ref="$(tags_for core | head -1)@$(metadata_digest "$metadata_dir/core.json")"
    tools_ref="$(tags_for tools | head -1)@$(metadata_digest "$metadata_dir/tools.json")"
    tag_args=()
    while IFS= read -r tag; do
      tag_args+=(--tag "$tag")
    done < <(tags_for full)
    docker buildx build \
      --platform linux/amd64,linux/arm64 \
      --file release/Dockerfile.full \
      --build-arg "CORE_IMAGE=$core_ref" \
      --build-arg "TOOLS_IMAGE=$tools_ref" \
      --build-arg "CORE_COMPONENT_DIGEST=$(metadata_digest "$metadata_dir/core.json")" \
      --build-arg "TOOLS_COMPONENT_DIGEST=$(metadata_digest "$metadata_dir/tools.json")" \
      --build-arg "VERSION=$version" \
      --build-arg "VCS_REF=$source_commit" \
      --build-arg "SOURCE_URL=$source_url" \
      "${tag_args[@]}" \
      --metadata-file "$metadata_dir/full.json" \
      --push .
    full_ref="$(tags_for full | head -1)@$(metadata_digest "$metadata_dir/full.json")"
    docker buildx imagetools inspect --raw "$full_ref" > "$metadata_dir/full.index.json"

    for variant in core tools full; do
      ref="$(tags_for "$variant" | head -1)"
      while IFS='|' read -r platform digest; do
        immutable_ref="${ref}@${digest}"
        tools_bundle_digest="$(python3 release/container_release.py platform-digest \
          --index "$metadata_dir/tools.index.json" --platform "$platform")"
        docker pull --platform "$platform" "$immutable_ref" >/dev/null
        DOCKER_LOAD_PLATFORM="$platform" SOURCE_COMMIT="$source_commit" \
          SHUTTLE_CORE_IMAGE="$(metadata_digest "$metadata_dir/core.json")" \
          SHUTTLE_TOOLS_IMAGE="$(metadata_digest "$metadata_dir/tools.json")" \
          SHUTTLE_TOOLS_BUNDLE_DIGEST="$tools_bundle_digest" \
          ./release/verify-container.sh "$variant" "$immutable_ref"
        if [[ "$variant" == tools && -n "${SHUTTLE_TOOLS_DIST:-}" ]]; then
          export_published_tools "$platform" "$immutable_ref"
        fi
      done < <(python3 release/container_release.py platform-digests \
        --index "$metadata_dir/$variant.index.json")
    done
    python3 release/container_release.py assemble-receipt \
      --image "$image" --version "$version" --source-commit "$source_commit" \
      --metadata-dir "$metadata_dir" --output "$receipt" >/dev/null
    digest="$(metadata_digest "$metadata_dir/full.json")"
    if [[ -n "${DOCKER_DIGEST_FILE:-}" ]]; then
      mkdir -p "$(dirname "$DOCKER_DIGEST_FILE")"
      printf '%s\n' "$digest" > "$DOCKER_DIGEST_FILE"
    fi
    printf '%s\n' "$receipt"
    ;;
  load)
    variant="${DOCKER_TARGET:-full}"
    first_tag="$(tags_for "$variant" | head -1)"
    if [[ "$variant" == full ]]; then
      core_tag="$(tags_for core | head -1)"
      tools_tag="$(tags_for tools | head -1)"
      core_digest="$(docker image inspect --format '{{.Id}}' "$core_tag")"
      tools_digest="$(docker image inspect --format '{{.Id}}' "$tools_tag")"
      core_ref="$core_tag"
      tools_ref="$tools_tag"
      docker buildx build \
        --platform "${DOCKER_LOAD_PLATFORM:-linux/arm64}" \
        --file release/Dockerfile.full \
        --build-arg "CORE_IMAGE=$core_ref" \
        --build-arg "TOOLS_IMAGE=$tools_ref" \
        --build-arg "CORE_COMPONENT_DIGEST=$core_digest" \
        --build-arg "TOOLS_COMPONENT_DIGEST=$tools_digest" \
        --build-arg "VERSION=$version" --build-arg "VCS_REF=$source_commit" \
        --build-arg "SOURCE_URL=$source_url" --tag "$first_tag" --load .
    else
      docker buildx build \
        --platform "${DOCKER_LOAD_PLATFORM:-linux/arm64}" \
        --target "$variant" \
        --build-arg "VERSION=$version" --build-arg "VCS_REF=$source_commit" \
        --build-arg "SOURCE_URL=$source_url" --tag "$first_tag" --load .
    fi
    printf '%s\n' "$first_tag"
    ;;
  *)
    echo "unsupported docker release mode: $mode" >&2
    exit 2
    ;;
esac
