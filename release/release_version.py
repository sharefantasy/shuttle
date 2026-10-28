#!/usr/bin/env python3
"""Read and validate the workspace release version without third-party deps."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path
from typing import Any


VERSION = re.compile(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)"$')


def workspace_version(path: Path) -> str:
    in_package = False
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if line == "[workspace.package]":
            in_package = True
            continue
        if line.startswith("["):
            in_package = False
        if in_package and (match := VERSION.fullmatch(line)):
            return match.group(1)
    raise ValueError("workspace.package.version is missing")


def validate_tag(version: str, tag: str) -> None:
    if tag != f"v{version}":
        raise ValueError(f"release tag {tag!r} must equal v{version}")


def release_config(path: Path = Path("release/release-config.json")) -> dict[str, Any]:
    config = json.loads(path.read_text(encoding="utf-8"))
    if config.get("schema_version") != "ea_release_config/v1":
        raise ValueError("unsupported release config schema")
    return config


def validate_git_coordinates(tag: str, commit: str) -> None:
    tag_commit = subprocess.check_output(
        ["git", "rev-parse", f"{tag}^{{commit}}"], text=True
    ).strip()
    commit_oid = subprocess.check_output(
        ["git", "rev-parse", f"{commit}^{{commit}}"], text=True
    ).strip()
    if tag_commit != commit_oid:
        raise ValueError(
            f"release tag {tag!r} points to {tag_commit}, not source commit {commit_oid}"
        )


def tag_timestamp(tag: str) -> int:
    return int(
        subprocess.check_output(
            ["git", "show", "-s", "--format=%ct", f"{tag}^{{commit}}"], text=True
        ).strip()
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo-toml", type=Path, default=Path("Cargo.toml"))
    parser.add_argument("--tag")
    parser.add_argument("--source-epoch", action="store_true")
    args = parser.parse_args()
    version = workspace_version(args.cargo_toml)
    if args.tag:
        validate_tag(version, args.tag)
    if args.source_epoch:
        if not args.tag:
            parser.error("--source-epoch requires --tag")
        print(tag_timestamp(args.tag))
    else:
        print(version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
