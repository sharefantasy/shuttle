#!/usr/bin/env bash

set -euo pipefail

: "${MY_PIPE_RELEASE_ROOT:?}"
: "${MY_PIPE_REPO:?}"
: "${MY_PIPE_VERSION:?}"

release_dir="${MY_PIPE_RELEASE_ROOT}/${MY_PIPE_REPO}/${MY_PIPE_VERSION}"
target_bin="${HOME}/.local/bin"

mkdir -p "${release_dir}" "${target_bin}"
install -m 755 dist/shuttle "${release_dir}/shuttle"

tmp_link="${target_bin}/shuttle.tmp"
final_link="${target_bin}/shuttle"
ln -sfn "${release_dir}/shuttle" "${tmp_link}"
mv -f "${tmp_link}" "${final_link}"
