#!/usr/bin/env python3
"""Unit tests for deterministic packaging and release metadata."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "release"))

from assemble_release import (
    IMAGE_DIGEST,
    cargo_lock,
    collect_tool_assets,
    cyclonedx,
    package_ref,
    purl,
)
from docker_tags import tags
from external_tools_bundle import validate_contract
from release_version import (
    release_config,
    validate_git_coordinates,
    validate_tag,
    workspace_version,
)


class ReleaseToolingTests(unittest.TestCase):
    def test_release_config_and_tags(self) -> None:
        config = release_config(REPO_ROOT / "release/release-config.json")
        self.assertEqual(
            [item["asset_suffix"] for item in config["binary_targets"]],
            ["linux-amd64", "linux-arm64", "darwin-amd64", "darwin-arm64"],
        )
        self.assertEqual(
            tags("registry.example/owner/shuttle", "1.2.3"),
            [
                "registry.example/owner/shuttle:1.2.3",
                "registry.example/owner/shuttle:1.2",
                "registry.example/owner/shuttle:1",
                "registry.example/owner/shuttle:latest",
            ],
        )
        with self.assertRaises(ValueError):
            tags("image", "1.2.3-rc1")

    def test_version_and_git_coordinate_validation(self) -> None:
        validate_tag("0.2.0", "v0.2.0")
        with self.assertRaises(ValueError):
            validate_tag("0.2.0", "v0.2.1")
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            subprocess.run(
                ["git", "config", "user.name", "release-test"], cwd=repo, check=True
            )
            subprocess.run(
                ["git", "config", "user.email", "release@example.invalid"],
                cwd=repo,
                check=True,
            )
            tracked = repo / "tracked"
            tracked.write_text("one\n", encoding="utf-8")
            subprocess.run(["git", "add", "tracked"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-qm", "one"], cwd=repo, check=True)
            subprocess.run(["git", "tag", "v1.0.0"], cwd=repo, check=True)
            tagged = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True
            ).strip()
            tracked.write_text("two\n", encoding="utf-8")
            subprocess.run(["git", "commit", "-qam", "two"], cwd=repo, check=True)
            head = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True
            ).strip()
            original_cwd = Path.cwd()
            try:
                os.chdir(repo)
                validate_git_coordinates("v1.0.0", tagged)
                with self.assertRaises(ValueError):
                    validate_git_coordinates("v1.0.0", head)
            finally:
                os.chdir(original_cwd)

    def test_archive_packaging_is_byte_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "shuttle"
            binary.write_bytes(b"fake executable")
            outputs = []
            env = {**os.environ, "SOURCE_DATE_EPOCH": "1700000000"}
            version = workspace_version(REPO_ROOT / "Cargo.toml")
            for name in ("one", "two"):
                dist = root / name
                result = subprocess.run(
                    [
                        sys.executable,
                        "release/package_release_binary.py",
                        "--binary",
                        str(binary),
                        "--suffix",
                        "darwin-arm64",
                        "--dist",
                        str(dist),
                    ],
                    cwd=REPO_ROOT,
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                outputs.append(Path(result.stdout.strip()))
            digests = [hashlib.sha256(path.read_bytes()).hexdigest() for path in outputs]
            self.assertEqual(digests[0], digests[1])
            with tarfile.open(outputs[0], "r:gz") as archive:
                members = {member.name: member for member in archive.getmembers()}
                root_name = f"shuttle-v{version}-darwin-arm64"
                self.assertEqual(members[f"{root_name}/shuttle"].mode, 0o755)
                self.assertEqual(members[f"{root_name}/shuttle"].mtime, 1700000000)
                self.assertEqual(members[f"{root_name}/README.md"].mode, 0o644)

    def test_release_config_is_valid_json(self) -> None:
        config = json.loads(
            (REPO_ROOT / "release/release-config.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            {
                name: target["max_uncompressed_bytes"]
                for name, target in config["docker"]["targets"].items()
            },
            {"core": 30 * 1024 * 1024, "tools": 60 * 1024 * 1024, "full": 100 * 1024 * 1024},
        )

    def test_external_tool_inputs_are_exactly_pinned(self) -> None:
        contract = json.loads(
            (REPO_ROOT / "release/external-tools-contract.json").read_text(encoding="utf-8")
        )
        validate_contract(contract)
        self.assertEqual(contract["debian_snapshot"], "20260824T000000Z")
        for tool in contract["tools"].values():
            self.assertIn("package_version", tool)
            self.assertNotIn("package_version_regex", tool)
            self.assertEqual(set(tool["archive_sha256"]), {"amd64", "arm64"})
            self.assertTrue(tool["source_archives"])

    def test_stable_release_requires_all_tool_bundle_assets(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "asset set is incomplete"):
                collect_tool_assets(Path(tmp), "0.2.0")

    def test_assemble_rejects_noncanonical_image_digest(self) -> None:
        self.assertIsNone(IMAGE_DIGEST.fullmatch("sha256:short"))
        self.assertIsNotNone(IMAGE_DIGEST.fullmatch("sha256:" + "a" * 64))

    def test_workspace_package_does_not_claim_crates_io_purl(self) -> None:
        self.assertTrue(
            purl({"name": "shuttle-cli", "version": "0.2.0"}).startswith("pkg:generic/")
        )
        self.assertEqual(
            purl(
                {
                    "name": "serde",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                }
            ),
            "pkg:cargo/serde@1.0.0",
        )

    def test_sbom_is_stable_and_contains_no_workspace_paths(self) -> None:
        head = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, text=True
        ).strip()
        version = workspace_version(REPO_ROOT / "Cargo.toml")
        original_cwd = Path.cwd()
        try:
            os.chdir(REPO_ROOT)
            first = cyclonedx(version, head)
            second = cyclonedx(version, head)
        finally:
            os.chdir(original_cwd)
        self.assertEqual(first, second)
        serialized = json.dumps(first)
        self.assertNotIn(str(REPO_ROOT), serialized)
        self.assertNotIn("/private/tmp/", serialized)
        dependency_refs = {item["ref"] for item in first["dependencies"]}
        expected_refs = {package_ref(package) for package in cargo_lock()}
        self.assertEqual(dependency_refs, expected_refs)


if __name__ == "__main__":
    unittest.main()
