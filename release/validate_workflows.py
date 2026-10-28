#!/usr/bin/env python3
"""Static drift checks for the three release workflow front-ends."""

from __future__ import annotations

import re
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def require(path: str, patterns: list[str]) -> None:
    text = (ROOT / path).read_text(encoding="utf-8")
    for pattern in patterns:
        if re.search(pattern, text, flags=re.MULTILINE) is None:
            raise ValueError(f"{path} lacks required release contract: {pattern}")


def forbid(path: str, patterns: list[str]) -> None:
    text = (ROOT / path).read_text(encoding="utf-8")
    for pattern in patterns:
        if re.search(pattern, text, flags=re.MULTILINE):
            raise ValueError(f"{path} contains forbidden release pattern: {pattern}")


def main() -> int:
    require(
        ".github/workflows/release.yml",
        [
            r'tags:\s*\n\s*- "v\*\.\*\.\*"',
            r"x86_64-apple-darwin",
            r"aarch64-apple-darwin",
            r"release/docker-release\.sh push",
            r"SHUTTLE_TOOLS_DIST: dist/release",
            r"container-images\.json",
            r"release/export-linux-binaries\.sh",
            r"release/assemble-from-artifacts\.sh",
            r"release/publish-release\.sh github",
        ],
    )
    for workflow in (".github/workflows/release.yml",):
        forbid(
            workflow,
            [
                r"https?://[^\s]*\$\{[^\n]*(?:TOKEN|PASSWORD|SECRET)",
                r"docker login[^\n]+(?:-p|--password)\s",
                r"access_token=",
            ],
        )
    require(
        "Dockerfile",
        [
            r"FROM rust:1\.90-bookworm@sha256:[0-9a-f]{64} AS builder",
            r"FROM scratch AS core",
            r"FROM debian:bookworm-slim@sha256:[0-9a-f]{64} AS tools-builder",
            r"FROM scratch AS tools",
            r"FROM debian:bookworm-slim@sha256:[0-9a-f]{64} AS compat-builder",
            r"FROM scratch AS full",
            r"FROM full AS runtime",
            r"external_tools_bundle\.py stage",
            r"/core-root\$\{library\}",
            r"USER 65532:65532",
            r"COPY --from=core / /",
            r"COPY --from=compat-builder /compat-root/ /",
            r"COPY --from=tools /opt/shuttle-tools/ /opt/shuttle-tools/",
            r"ldd /usr/local/bin/shuttle",
            r'destination="/core-root\$\{library\}"',
            r'cp -L "\$library" "\$destination"',
        ],
    )
    require(
        "release/Dockerfile.full",
        [
            r"ARG CORE_IMAGE",
            r"ARG TOOLS_IMAGE",
            r"ARG CORE_COMPONENT_DIGEST=\$CORE_IMAGE",
            r"ARG TOOLS_COMPONENT_DIGEST=\$TOOLS_IMAGE",
            r"FROM \$\{CORE_IMAGE\} AS core-component",
            r"FROM \$\{TOOLS_IMAGE\} AS tools-component",
            r"COPY --from=core-component / /",
            r"COPY --from=compat-builder /compat-root/ /",
            r"io\.shuttle\.core\.image",
            r"io\.shuttle\.tools\.image",
        ],
    )
    require(
        "release/docker-release.sh",
        [
            r"for variant in core tools",
            r"--target \"\$variant\"",
            r"Dockerfile\.full",
            r"CORE_IMAGE=\$core_ref",
            r"TOOLS_IMAGE=\$tools_ref",
            r"CORE_COMPONENT_DIGEST=\$\(metadata_digest",
            r"TOOLS_COMPONENT_DIGEST=\$\(metadata_digest",
            r"docker pull --platform",
            r"container_release\.py assemble-receipt",
            r"platform-digests",
            r"verify-container\.sh \"\$variant\" \"\$immutable_ref\"",
        ],
    )
    require(
        "release/release-config.json",
        [
            r'"core"[\s\S]*"max_uncompressed_bytes": 31457280',
            r'"tools"[\s\S]*"max_uncompressed_bytes": 62914560',
            r'"full"[\s\S]*"max_uncompressed_bytes": 104857600',
            r'"size_exceptions": "release/size-exceptions\.json"',
        ],
    )
    require(
        "release/external-tools-contract.json",
        [
            r'"schema_version": "shuttle_external_tools_build_contract/v1"',
            r'"cell-supervisor"',
            r'"source-analysis"',
            r'"docker", "container-use", "node", "pi"',
        ],
    )
    print("validated release workflow contracts: github, docker")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
