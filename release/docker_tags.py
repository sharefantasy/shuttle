#!/usr/bin/env python3
"""Render canonical semver Docker tags for a release image."""

from __future__ import annotations

import argparse


def tags(image: str, version: str) -> list[str]:
    parts = version.split(".")
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        raise ValueError(f"stable release version must be numeric semver: {version}")
    major, minor, _patch = parts
    return [
        f"{image}:{version}",
        f"{image}:{major}.{minor}",
        f"{image}:{major}",
        f"{image}:latest",
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--format", choices=("lines", "csv"), default="lines")
    args = parser.parse_args()
    separator = "\n" if args.format == "lines" else ","
    print(separator.join(tags(args.image, args.version)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
