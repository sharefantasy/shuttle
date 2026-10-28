#!/usr/bin/env python3
"""Static contracts and receipts for split Shuttle container releases."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

from docker_tags import tags
from release_version import release_config

DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
VARIANTS = ("core", "tools", "full")


def image_ref(base_image: str, variant: str, config: dict[str, Any] | None = None) -> str:
    if variant not in VARIANTS:
        raise ValueError(f"unsupported container variant: {variant}")
    config = config or release_config()
    suffix = config["docker"]["targets"][variant]["image_suffix"]
    return base_image + suffix


def variant_tags(base_image: str, variant: str, version: str) -> list[str]:
    return tags(image_ref(base_image, variant), version)


def metadata_digest(path: Path) -> str:
    metadata = json.loads(path.read_text(encoding="utf-8"))
    digest = metadata.get("containerimage.digest")
    if not isinstance(digest, str) or DIGEST.fullmatch(digest) is None:
        raise ValueError(f"{path} lacks a canonical containerimage.digest")
    return digest


def platform_digests(path: Path, platforms: list[str]) -> dict[str, str]:
    index = json.loads(path.read_text(encoding="utf-8"))
    manifests = index.get("manifests")
    if not isinstance(manifests, list):
        raise ValueError(f"{path} is not an OCI image index")
    found: dict[str, str] = {}
    for manifest in manifests:
        platform = manifest.get("platform", {})
        key = f"{platform.get('os', '')}/{platform.get('architecture', '')}"
        if key not in platforms:
            continue
        digest = manifest.get("digest")
        if not isinstance(digest, str) or DIGEST.fullmatch(digest) is None:
            raise ValueError(f"{path} contains an invalid platform digest for {key}")
        if key in found:
            raise ValueError(f"{path} contains duplicate platform manifests for {key}")
        found[key] = digest
    if set(found) != set(platforms):
        raise ValueError(f"{path} platform set drifted: {sorted(found)}")
    return {platform: found[platform] for platform in platforms}


def print_platform_digests(path: Path) -> None:
    for platform, digest in platform_digests(
        path, release_config()["docker"]["platforms"]
    ).items():
        print(f"{platform}|{digest}")


def assemble_receipt(
    base_image: str,
    version: str,
    source_commit: str,
    metadata_dir: Path,
) -> dict[str, Any]:
    config = release_config()
    images = {}
    platforms = config["docker"]["platforms"]
    for variant in VARIANTS:
        ref = image_ref(base_image, variant, config)
        images[variant] = {
            "ref": f"{ref}:{version}",
            "digest": metadata_digest(metadata_dir / f"{variant}.json"),
            "platform_digests": platform_digests(
                metadata_dir / f"{variant}.index.json", platforms
            ),
            "tags": variant_tags(base_image, variant, version),
        }
    images["full"]["components"] = {
        "core": images["core"]["digest"],
        "tools": images["tools"]["digest"],
    }
    receipt = {
        "schema_version": "shuttle_container_images_receipt/v1",
        "version": version,
        "source_commit": source_commit,
        "platforms": config["docker"]["platforms"],
        "images": images,
    }
    validate_receipt(receipt, version, source_commit)
    return receipt


def validate_receipt(
    receipt: dict[str, Any], version: str, source_commit: str
) -> None:
    if set(receipt) != {
        "schema_version",
        "version",
        "source_commit",
        "platforms",
        "images",
    } or receipt.get("schema_version") != "shuttle_container_images_receipt/v1":
        raise ValueError("container images receipt schema drifted")
    if receipt.get("version") != version or receipt.get("source_commit") != source_commit:
        raise ValueError("container images receipt coordinates drifted")
    if receipt.get("platforms") != release_config()["docker"]["platforms"]:
        raise ValueError("container images receipt platforms drifted")
    images = receipt.get("images")
    if not isinstance(images, dict) or tuple(sorted(images)) != tuple(sorted(VARIANTS)):
        raise ValueError("container images receipt variant set drifted")
    digests = set()
    for variant in VARIANTS:
        image = images[variant]
        expected_fields = {"ref", "digest", "platform_digests", "tags"}
        if variant == "full":
            expected_fields.add("components")
        if set(image) != expected_fields:
            raise ValueError(f"container image entry schema drifted: {variant}")
        if DIGEST.fullmatch(str(image["digest"])) is None:
            raise ValueError(f"container image digest is not canonical: {variant}")
        if image["ref"] != variant_tags(image_ref_from_receipt(image, variant), variant, version)[0]:
            raise ValueError(f"container image version ref drifted: {variant}")
        if image["tags"][0] != image["ref"]:
            raise ValueError(f"container image primary tag drifted: {variant}")
        platform_values = image["platform_digests"]
        if not isinstance(platform_values, dict) or list(platform_values) != receipt["platforms"]:
            raise ValueError(f"container image platform digest set drifted: {variant}")
        if any(DIGEST.fullmatch(str(value)) is None for value in platform_values.values()):
            raise ValueError(f"container image platform digest is not canonical: {variant}")
        digests.add(image["digest"])
    if len(digests) != len(VARIANTS):
        raise ValueError("core, tools, and full must publish distinct digests")
    if images["full"]["components"] != {
        "core": images["core"]["digest"],
        "tools": images["tools"]["digest"],
    }:
        raise ValueError("full image component digests drifted")


def image_ref_from_receipt(image: dict[str, Any], variant: str) -> str:
    suffix = release_config()["docker"]["targets"][variant]["image_suffix"]
    tagged = str(image["ref"])
    base_variant = tagged.rsplit(":", 1)[0]
    if suffix and not base_variant.endswith(suffix):
        raise ValueError(f"container image suffix drifted: {variant}")
    return base_variant[: -len(suffix)] if suffix else base_variant


def validate_size(
    variant: str,
    platform: str,
    measured_bytes: int,
    source_commit: str,
    exception_path: Path,
) -> str:
    config = release_config()
    budget = config["docker"]["targets"][variant]["max_uncompressed_bytes"]
    if measured_bytes <= budget:
        return "within-budget"
    exceptions = json.loads(exception_path.read_text(encoding="utf-8"))
    if exceptions.get("schema_version") != "shuttle_container_size_exceptions/v1":
        raise ValueError("unsupported size exception schema")
    exact = [
        item
        for item in exceptions.get("exceptions", [])
        if item.get("variant") == variant
        and item.get("platform") == platform
        and item.get("source_commit") == source_commit
        and item.get("measured_bytes") == measured_bytes
        and item.get("budget_bytes") == budget
        and isinstance(item.get("reason"), str)
        and item["reason"].strip()
    ]
    if len(exact) != 1:
        raise ValueError(
            f"{variant} {platform} size {measured_bytes} exceeds fixed budget {budget}; "
            "add one measured, source-commit-bound exception instead of raising the budget"
        )
    return "measured-exception"


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    refs = subparsers.add_parser("tags")
    refs.add_argument("--image", required=True)
    refs.add_argument("--variant", choices=VARIANTS, required=True)
    refs.add_argument("--version", required=True)
    assemble = subparsers.add_parser("assemble-receipt")
    assemble.add_argument("--image", required=True)
    assemble.add_argument("--version", required=True)
    assemble.add_argument("--source-commit", required=True)
    assemble.add_argument("--metadata-dir", type=Path, required=True)
    assemble.add_argument("--output", type=Path, required=True)
    size = subparsers.add_parser("validate-size")
    size.add_argument("--variant", choices=VARIANTS, required=True)
    size.add_argument("--platform", choices=("linux/amd64", "linux/arm64"), required=True)
    size.add_argument("--measured-bytes", type=int, required=True)
    size.add_argument("--source-commit", required=True)
    size.add_argument("--exceptions", type=Path, required=True)
    platform_digest_parser = subparsers.add_parser("platform-digests")
    platform_digest_parser.add_argument("--index", type=Path, required=True)
    one_platform_parser = subparsers.add_parser("platform-digest")
    one_platform_parser.add_argument("--index", type=Path, required=True)
    one_platform_parser.add_argument("--platform", choices=("linux/amd64", "linux/arm64"), required=True)
    args = parser.parse_args()
    if args.command == "tags":
        print("\n".join(variant_tags(args.image, args.variant, args.version)))
    elif args.command == "assemble-receipt":
        receipt = assemble_receipt(
            args.image, args.version, args.source_commit, args.metadata_dir
        )
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(args.output)
    elif args.command == "validate-size":
        print(validate_size(args.variant, args.platform, args.measured_bytes, args.source_commit, args.exceptions))
    elif args.command == "platform-digests":
        print_platform_digests(args.index)
    else:
        print(platform_digests(args.index, release_config()["docker"]["platforms"])[args.platform])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
