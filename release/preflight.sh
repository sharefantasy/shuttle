#!/usr/bin/env bash

set -euo pipefail

mode="${1:-full}"
commands=(git python3 bash)
case "$mode" in
  full)
    commands+=(cargo rustc rustup shellcheck docker)
    ;;
  static)
    commands+=(shellcheck zstd)
    ;;
  mirror)
    commands+=(curl docker)
    ;;
  *)
    echo "usage: preflight.sh <full|static|mirror>" >&2
    exit 2
    ;;
esac

for command in "${commands[@]}"; do
  command -v "$command" >/dev/null || {
    echo "required release command is missing: $command" >&2
    exit 1
  }
done

python3 - <<'PY'
import sys

if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11 or newer is required")
PY

if [[ "$mode" == "full" ]]; then
  rustc_version="$(rustc --version | awk '{print $2}')"
  python3 - "$rustc_version" <<'PY'
import sys

parts = tuple(int(item) for item in sys.argv[1].split(".")[:2])
if parts < (1, 90):
    raise SystemExit("Rust 1.90 or newer is required")
PY
fi

if [[ "$mode" == "mirror" ]]; then
  docker buildx version >/dev/null
fi

printf 'release preflight passed: %s\n' "$mode"
