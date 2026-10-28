#!/usr/bin/env bash

set -euo pipefail

variant="${1:?usage: verify-container.sh <core|tools|full> <image-ref>}"
image="${2:?usage: verify-container.sh <core|tools|full> <image-ref>}"
config="release/release-config.json"
version="$(python3 release/release_version.py)"
container_id="$(docker create "$image" true)"
rootfs="$(mktemp)"
trap 'docker rm "$container_id" >/dev/null 2>&1 || true; rm -f "$rootfs"' EXIT
docker export --output "$rootfs" "$container_id"
python3 - "$config" "$variant" "$rootfs" <<'PY'
import json
import pathlib
import sys
import tarfile

config = json.load(open(sys.argv[1], encoding="utf-8"))["docker"]["targets"][sys.argv[2]]
with tarfile.open(sys.argv[3]) as archive:
    members = archive.getmembers()
    names = {"/" + member.name.lstrip("./") for member in members}
missing = sorted(set(config["required_paths"]) - names)
if missing:
    raise SystemExit(f"container is missing required paths: {missing}")
basenames = {
    pathlib.PurePosixPath(member.name).name
    for member in members
    if member.isfile() and member.mode & 0o111
}
forbidden = sorted(set(config["forbidden_commands"]) & basenames)
if forbidden:
    raise SystemExit(f"container contains forbidden commands: {forbidden}")
PY

case "$variant" in
  core|full)
    [[ "$(docker run --rm --entrypoint /usr/local/bin/shuttle "$image" --version)" == "shuttle $version" ]]
    [[ "$(docker inspect --format '{{.Config.User}}' "$image")" == "65532:65532" ]]
    ;;
  tools) ;;
  *) echo "unsupported container variant: $variant" >&2; exit 2 ;;
esac
if [[ "$variant" == full ]]; then
  [[ "$(docker run --rm --entrypoint /bin/sh "$image" -c 'printf %s "$HOME"')" == "/home/shuttle" ]]
  docker run --rm --entrypoint /bin/sh "$image" -c 'test -r /etc/ssl/certs/ca-certificates.crt'
  if [[ -n "${SHUTTLE_CORE_IMAGE:-}" || -n "${SHUTTLE_TOOLS_IMAGE:-}" ]]; then
    [[ "$(docker image inspect --format '{{index .Config.Labels "io.shuttle.core.image"}}' "$image")" == "${SHUTTLE_CORE_IMAGE:?SHUTTLE_CORE_IMAGE is required for full verification}" ]]
    [[ "$(docker image inspect --format '{{index .Config.Labels "io.shuttle.tools.image"}}' "$image")" == "${SHUTTLE_TOOLS_IMAGE:?SHUTTLE_TOOLS_IMAGE is required for full verification}" ]]
  fi
fi
if [[ "$variant" == tools || "$variant" == full ]]; then
  for tool in git rg ctags tar ps; do
    docker run --rm --entrypoint "/opt/shuttle-tools/bin/$tool" "$image" --version >/dev/null
  done
fi
if [[ "$variant" == full ]]; then
  bundle_digest="${SHUTTLE_TOOLS_BUNDLE_DIGEST:?SHUTTLE_TOOLS_BUNDLE_DIGEST is required for full verification}"
  doctor="$(docker run --rm \
    --env SHUTTLE_TOOL_MODE=strict \
    --env SHUTTLE_TOOL_BUNDLE_ROOT=/opt/shuttle-tools \
    --env SHUTTLE_TOOL_MANIFEST=/opt/shuttle-tools/manifest.json \
    --env SHUTTLE_TOOL_PROFILE=cell-supervisor,source-analysis \
    --env SHUTTLE_TOOL_BUNDLE_DIGEST="$bundle_digest" \
    "$image" tools doctor --json)"
  python3 - "$doctor" "$bundle_digest" <<'PY'
import json, sys
report = json.loads(sys.argv[1])
if report.get("status") != "ok" or report.get("bundle_digest") != sys.argv[2]:
    raise SystemExit("strict doctor did not bind the expected tools digest")
if set(report.get("profiles", [])) != {"cell-supervisor", "source-analysis"}:
    raise SystemExit("strict doctor profile set drifted")
if set(report.get("tools", {})) != {"git", "rg", "ctags", "tar", "ps"}:
    raise SystemExit("strict doctor tool set drifted")
PY
fi

max_bytes="$(python3 - "$config" "$variant" <<'PY'
import json
import sys

print(json.load(open(sys.argv[1], encoding="utf-8"))["docker"]["targets"][sys.argv[2]]["max_uncompressed_bytes"])
PY
)"
size="$(docker image inspect --format '{{.Size}}' "$image")"
python3 release/container_release.py validate-size \
  --variant "$variant" --platform "${DOCKER_LOAD_PLATFORM:-linux/arm64}" \
  --measured-bytes "$size" --source-commit "${SOURCE_COMMIT:?SOURCE_COMMIT is required}" \
  --exceptions "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["docker"]["size_exceptions"])' "$config")"
printf 'verified %s container %s: %s bytes (budget %s)\n' "$variant" "$image" "$size" "$max_bytes"
