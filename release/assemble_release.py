#!/usr/bin/env python3
"""Assemble deterministic release checksums, SBOM, and manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import tomllib
import uuid
from datetime import datetime, timezone
from pathlib import Path

from release_version import (
    release_config,
    validate_git_coordinates,
    validate_tag,
    workspace_version,
)


IMAGE_DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


def expected_tool_asset_names(version: str) -> set[str]:
    return {
        f"shuttle-tools-v{version}-linux-{arch}{suffix}"
        for arch in ("amd64", "arm64")
        for suffix in (".tar.zst", ".manifest.json", ".sha256")
    }


def collect_tool_assets(dist: Path, version: str) -> list[Path]:
    expected = expected_tool_asset_names(version)
    actual = {
        path.name: path
        for path in dist.glob(f"shuttle-tools-v{version}-linux-*")
        if path.suffix in {".zst", ".json", ".sha256"}
    }
    if set(actual) != expected:
        raise ValueError(
            "external tool bundle asset set is incomplete: "
            f"missing={sorted(expected - set(actual))}, extra={sorted(set(actual) - expected)}"
        )
    return [actual[name] for name in sorted(expected)]


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def git_timestamp(commit: str) -> str:
    raw = subprocess.check_output(
        ["git", "show", "-s", "--format=%cI", commit], text=True
    ).strip()
    return (
        datetime.fromisoformat(raw)
        .astimezone(timezone.utc)
        .isoformat()
        .replace("+00:00", "Z")
    )


def cargo_lock() -> list[dict[str, object]]:
    parsed = tomllib.loads(Path("Cargo.lock").read_text(encoding="utf-8"))
    if parsed.get("version") != 4 or not isinstance(parsed.get("package"), list):
        raise ValueError("Cargo.lock is not a supported version 4 lockfile")
    return parsed["package"]


def package_ref(package: dict[str, object]) -> str:
    name = str(package["name"])
    version = str(package["version"])
    source = str(package.get("source") or "workspace")
    source_digest = hashlib.sha256(source.encode()).hexdigest()[:16]
    return f"cargo:{name}@{version}:{source_digest}"


def purl(package: dict[str, object]) -> str:
    name = str(package["name"])
    version = str(package["version"])
    source = str(package.get("source") or "workspace")
    if source.startswith("registry+"):
        return f"pkg:cargo/{name}@{version}"
    source_digest = hashlib.sha256(source.encode()).hexdigest()[:16]
    return f"pkg:generic/{name}@{version}?source_digest={source_digest}"


def cyclonedx(version: str, commit: str) -> dict[str, object]:
    lock_packages = cargo_lock()
    packages = sorted(
        lock_packages,
        key=lambda item: (item["name"], item["version"], item.get("source") or "workspace"),
    )
    refs_by_name: dict[str, list[str]] = {}
    refs: set[str] = set()
    components = []
    for package in packages:
        source = str(package.get("source") or "workspace")
        ref = package_ref(package)
        if ref in refs:
            raise ValueError(f"duplicate stable SBOM component reference: {ref}")
        refs.add(ref)
        refs_by_name.setdefault(str(package["name"]), []).append(ref)
        component: dict[str, object] = {
            "type": "library",
            "bom-ref": ref,
            "name": package["name"],
            "version": package["version"],
            "purl": purl(package),
        }
        if package.get("license"):
            component["licenses"] = [{"expression": package["license"]}]
        checksum = package.get("checksum")
        if checksum:
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        if source != "workspace":
            component["properties"] = [{"name": "cargo:source", "value": source}]
        components.append(component)
    dependencies = []
    for package in packages:
        dependency_refs: set[str] = set()
        for raw in package.get("dependencies", []):
            dependency = str(raw)
            fields = dependency.split(" ", 2)
            name = fields[0]
            if len(fields) >= 2:
                dep_version = fields[1]
                candidates = [
                    ref
                    for ref in refs_by_name.get(name, [])
                    if f"@{dep_version}:" in ref
                ]
            else:
                candidates = refs_by_name.get(dependency, [])
            if not candidates:
                raise ValueError(f"unresolved Cargo.lock dependency: {dependency}")
            dependency_refs.update(candidates)
        dependencies.append(
            {"ref": package_ref(package), "dependsOn": sorted(dependency_refs)}
        )
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": (
            f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'shuttle:{version}:{commit}')}"
        ),
        "version": 1,
        "metadata": {
            "timestamp": git_timestamp(commit),
            "component": {
                "type": "application",
                "bom-ref": f"pkg:cargo/shuttle@{version}?vcs_url=git%2B{commit}",
                "name": "shuttle",
                "version": version,
            },
        },
        "components": components,
        "dependencies": dependencies,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, default=Path("dist/release"))
    parser.add_argument("--tag", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--image-ref")
    parser.add_argument("--image-digest")
    parser.add_argument("--container-images", type=Path)
    args = parser.parse_args()
    container_images_input = (
        json.loads(args.container_images.read_text(encoding="utf-8"))
        if args.container_images
        else None
    )

    version = workspace_version(Path("Cargo.toml"))
    validate_tag(version, args.tag)
    validate_git_coordinates(args.tag, args.source_commit)
    dist = args.dist.resolve()
    dist.mkdir(parents=True, exist_ok=True)
    for generated in (
        dist / "SHA256SUMS",
        dist / "release-manifest.json",
        dist / "container-image.json",
        dist / "container-images.json",
    ):
        generated.unlink(missing_ok=True)
    archives = sorted(dist.rglob(f"shuttle-v{version}-*.tar.gz"))
    expected = {
        f"shuttle-v{version}-{item['asset_suffix']}.tar.gz"
        for item in release_config()["binary_targets"]
    }
    by_name: dict[str, Path] = {}
    for path in archives:
        if path.name in by_name and by_name[path.name] != path:
            raise ValueError(f"duplicate release asset name: {path.name}")
        by_name[path.name] = path
    if set(by_name) != expected:
        missing = sorted(expected - set(by_name))
        extra = sorted(set(by_name) - expected)
        raise ValueError(
            f"release binary asset set differs: missing={missing}, extra={extra}"
        )
    for name, source in by_name.items():
        target = dist / name
        if source != target:
            target.write_bytes(source.read_bytes())

    sbom_path = dist / f"shuttle-v{version}.cdx.json"
    sbom_path.write_text(
        json.dumps(cyclonedx(version, args.source_commit), indent=2, sort_keys=True)
        + "\n",
        encoding="utf-8",
    )
    tool_assets = collect_tool_assets(dist, version)
    assets = [dist / name for name in sorted(by_name)] + tool_assets + [sbom_path]
    checksums = dist / "SHA256SUMS"
    checksums.write_text(
        "".join(f"{sha256(path)}  {path.name}\n" for path in assets),
        encoding="utf-8",
    )
    image = None
    container_images = None
    if args.container_images:
        if args.image_ref or args.image_digest:
            raise ValueError("container-images cannot be combined with legacy image arguments")
        container_images = container_images_input
        assert container_images is not None
        from container_release import validate_receipt

        validate_receipt(container_images, version, args.source_commit)
        image = container_images["images"]["full"]
        image = {"ref": image["ref"].rsplit(":", 1)[0], "digest": image["digest"]}
        image_path = dist / "container-images.json"
        image_path.write_text(
            json.dumps(container_images, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        assets.append(image_path)
    elif args.image_ref or args.image_digest:
        if (
            not args.image_ref
            or not args.image_digest
            or IMAGE_DIGEST.fullmatch(args.image_digest) is None
        ):
            raise ValueError(
                "image-ref and canonical image-digest must be supplied together"
            )
        image = {"ref": args.image_ref, "digest": args.image_digest}
        image_path = dist / "container-image.json"
        image_path.write_text(
            json.dumps(
                {
                    "schema_version": "ea_container_image_receipt/v1",
                    "version": version,
                    "tag": args.tag,
                    "source_commit": args.source_commit,
                    **image,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        assets.append(image_path)
    checksums.write_text(
        "".join(f"{sha256(path)}  {path.name}\n" for path in assets),
        encoding="utf-8",
    )
    manifest = {
        "schema_version": "ea_release_manifest/v1",
        "version": version,
        "tag": args.tag,
        "source_commit": args.source_commit,
        "assets": [
            {"name": path.name, "sha256": sha256(path), "bytes": path.stat().st_size}
            for path in assets
        ],
        "checksums": {"name": checksums.name, "sha256": sha256(checksums)},
        "container_image": image,
        "container_images": container_images,
    }
    (dist / "release-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(manifest, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
