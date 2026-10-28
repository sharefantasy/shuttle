#!/usr/bin/env python3
"""Validate release assets, checksums, SBOM, and archive layout."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tarfile
from pathlib import Path, PurePosixPath

from release_version import (
    release_config,
    validate_git_coordinates,
    validate_tag,
    workspace_version,
)
from assemble_release import expected_tool_asset_names


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, default=Path("dist/release"))
    parser.add_argument("--tag", required=True)
    parser.add_argument("--require-image", action="store_true")
    args = parser.parse_args()
    version = workspace_version(Path("Cargo.toml"))
    validate_tag(version, args.tag)
    dist = args.dist.resolve(strict=True)
    manifest = json.loads((dist / "release-manifest.json").read_text(encoding="utf-8"))
    if manifest.get("schema_version") != "shuttle_release_manifest/v1":
        raise ValueError("unsupported release manifest")
    if manifest.get("version") != version or manifest.get("tag") != args.tag:
        raise ValueError("release manifest version coordinates drifted")
    source_commit = manifest.get("source_commit")
    if not isinstance(source_commit, str):
        raise ValueError("release manifest lacks a source commit")
    validate_git_coordinates(args.tag, source_commit)
    image = manifest.get("container_image")
    if args.require_image and not isinstance(image, dict):
        raise ValueError("release manifest lacks container image digest")
    if isinstance(image, dict) and re.fullmatch(
        r"sha256:[0-9a-f]{64}", str(image.get("digest", ""))
    ) is None:
        raise ValueError("container image digest is not canonical")
    if isinstance(image, dict):
        container_images = manifest.get("container_images")
        if isinstance(container_images, dict):
            from container_release import validate_receipt

            validate_receipt(container_images, version, source_commit)
            if container_images["images"]["full"]["digest"] != image["digest"]:
                raise ValueError("full compatibility image digest differs from split receipt")
            if json.loads((dist / "container-images.json").read_text(encoding="utf-8")) != container_images:
                raise ValueError("container images receipt differs from release manifest")
        elif container_images is not None:
            raise ValueError("container_images must be an object or null")
        else:
            image_receipt = json.loads(
                (dist / "container-image.json").read_text(encoding="utf-8")
            )
            expected_receipt = {
                "schema_version": "shuttle_container_image_receipt/v1",
                "version": version,
                "tag": args.tag,
                "source_commit": source_commit,
                **image,
            }
            if image_receipt != expected_receipt:
                raise ValueError("container image receipt differs from release manifest")

    asset_entries = manifest.get("assets", [])
    declared = {item["name"]: item for item in asset_entries}
    if len(declared) != len(asset_entries):
        raise ValueError("release manifest contains duplicate asset names")
    expected_archives = {
        f"shuttle-v{version}-{item['asset_suffix']}.tar.gz"
        for item in release_config()["binary_targets"]
    }
    actual_archives = {name for name in declared if name.endswith(".tar.gz")}
    if actual_archives != expected_archives:
        raise ValueError("release manifest binary target set differs from release config")
    tool_names = {name for name in declared if name.startswith(f"shuttle-tools-v{version}-")}
    expected_tools = expected_tool_asset_names(version)
    if tool_names != expected_tools:
        raise ValueError("release manifest external tool asset set differs")
    from external_tools_bundle import verify_archive

    for arch in ("amd64", "arm64"):
        verify_archive(dist / f"shuttle-tools-v{version}-linux-{arch}.tar.zst")
    checksum_lines = {}
    for line in (dist / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        if "  " not in line:
            raise ValueError("SHA256SUMS contains a malformed line")
        digest, name = line.split("  ", 1)
        if name in checksum_lines:
            raise ValueError(f"SHA256SUMS contains a duplicate asset: {name}")
        if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
            raise ValueError(f"SHA256SUMS contains a malformed digest: {name}")
        checksum_lines[name] = digest
    if set(declared) != set(checksum_lines):
        raise ValueError("release manifest and SHA256SUMS asset sets differ")
    for name, item in declared.items():
        if PurePosixPath(name).name != name:
            raise ValueError(f"release asset is not a basename: {name}")
        path = dist / name
        digest = sha256(path)
        if digest != item.get("sha256") or digest != checksum_lines[name]:
            raise ValueError(f"release asset digest mismatch: {name}")
        if path.stat().st_size != item.get("bytes"):
            raise ValueError(f"release asset size mismatch: {name}")
        if name.endswith(".tar.gz"):
            expected_root = name.removesuffix(".tar.gz")
            required = {
                expected_root,
                f"{expected_root}/shuttle",
                f"{expected_root}/README.md",
                f"{expected_root}/LICENSE",
            }
            with tarfile.open(path, "r:gz") as archive:
                archive_members = archive.getmembers()
                members = {member.name for member in archive_members}
                if any(
                    member.name.startswith("/")
                    or ".." in PurePosixPath(member.name).parts
                    for member in archive_members
                ):
                    raise ValueError(f"release archive contains an unsafe path: {name}")
                if members != required:
                    raise ValueError(
                        f"release archive contains unexpected entries: {name}"
                    )
                by_member = {member.name: member for member in archive_members}
                if not by_member[f"{expected_root}/shuttle"].isfile():
                    raise ValueError(f"release archive binary is not a regular file: {name}")
                if by_member[f"{expected_root}/shuttle"].mode != 0o755:
                    raise ValueError(f"release archive binary mode is not 0755: {name}")
                for document in ("README.md", "LICENSE"):
                    member = by_member[f"{expected_root}/{document}"]
                    if not member.isfile() or member.mode != 0o644:
                        raise ValueError(f"release archive document mode differs: {name}")
    checksums = dist / manifest["checksums"]["name"]
    if sha256(checksums) != manifest["checksums"]["sha256"]:
        raise ValueError("SHA256SUMS digest differs from release manifest")
    sbom = json.loads(
        (dist / f"shuttle-v{version}.cdx.json").read_text(encoding="utf-8")
    )
    if sbom.get("bomFormat") != "CycloneDX" or sbom.get("specVersion") != "1.5":
        raise ValueError("release SBOM is not CycloneDX 1.5")
    sbom_component = sbom.get("metadata", {}).get("component", {})
    if sbom_component.get("name") != "shuttle" or sbom_component.get(
        "version"
    ) != version:
        raise ValueError("release SBOM application coordinates drifted")
    print(f"verified release {args.tag}: {len(declared)} assets")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
