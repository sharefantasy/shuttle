#!/usr/bin/env bash

set -euo pipefail

base_url="${1:?usage: import-release-assets.sh <canonical-release-base-url> [dist-dir]}"
dist_dir="${2:-dist/release}"
version="$(python3 release/release_version.py --tag "${RELEASE_TAG:?RELEASE_TAG is required}")"
mkdir -p "$dist_dir"

download() {
  local url="$1"
  local destination="$2"
  if [[ -n "${RELEASE_SOURCE_TOKEN:-}" ]]; then
    printf 'header = "Authorization: Bearer %s"\n' "$RELEASE_SOURCE_TOKEN" | \
      curl --config - --fail --location --silent --show-error \
        --retry 30 --retry-delay 10 --retry-max-time 1800 --retry-all-errors \
        --output "$destination" "$url"
  else
    curl --fail --location --silent --show-error \
      --retry 30 --retry-delay 10 --retry-max-time 1800 --retry-all-errors \
      --output "$destination" "$url"
  fi
}

download "${base_url%/}/SHA256SUMS" "$dist_dir/upstream-SHA256SUMS"
for suffix in linux-amd64 linux-arm64 darwin-amd64 darwin-arm64; do
  name="shuttle-v${version}-${suffix}.tar.gz"
  download "${base_url%/}/$name" "$dist_dir/$name"
done

python3 - "$dist_dir" <<'PY'
import hashlib
import sys
from pathlib import Path

dist = Path(sys.argv[1])
expected = {}
for line in (dist / "upstream-SHA256SUMS").read_text(encoding="utf-8").splitlines():
    digest, name = line.split("  ", 1)
    if name in expected:
        raise SystemExit(f"duplicate upstream checksum: {name}")
    expected[name] = digest
archives = sorted(dist.glob("shuttle-v*.tar.gz"))
for path in archives:
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if expected.get(path.name) != digest:
        raise SystemExit(f"upstream checksum mismatch: {path.name}")
print(f"imported and verified {len(archives)} canonical binary archives")
PY
