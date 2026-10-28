#!/usr/bin/env python3
"""Unit tests for split images and deterministic external-tool bundles."""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "release"))

from container_release import (
    assemble_receipt,
    image_ref,
    platform_digests,
    validate_receipt,
    validate_size,
    variant_tags,
)
from external_tools_bundle import (
    REQUIRED_GIT_HELPERS,
    canonical_json,
    collect_source_archives,
    export_bundle,
    payload_files,
    sha256_bytes,
    verify_archive,
    verify_bundle,
)
from unittest import mock


MECHANICAL_TOOL_NAMES = ("git", "rg", "ctags", "tar", "ps", "ast-grep", "mergiraf")
HARDCODED_MECHANICAL_COMMAND = re.compile(
    r'(?:std\s*::\s*process\s*::\s*)?Command\s*::\s*new\s*\(\s*"'
    + r"(?P<tool>" + "|".join(re.escape(name) for name in MECHANICAL_TOOL_NAMES) + r')"'
)
TEST_MODULE_START = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+tests\s*\{",
    re.MULTILINE,
)


def production_rust_source(source: str) -> str:
    """Exclude the conventional trailing unit-test module from a Rust source file."""
    marker = TEST_MODULE_START.search(source)
    return source[: marker.start()] if marker else source


class ExternalToolsReleaseTests(unittest.TestCase):
    def fixture_bundle(self, root: Path) -> None:
        tools = {}
        profiles = {
            "cell-supervisor": {"required_tools": ["git", "tar", "ps"]},
            "source-analysis": {"required_tools": ["git", "rg", "ctags", "tar"]},
        }
        for name in ("git", "rg", "ctags", "tar", "ps"):
            path = root / "bin" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"fake-{name}\n", encoding="utf-8")
            path.chmod(0o755)
            tools[name] = {
                "path": f"bin/{name}",
                "version": "1.0.0-1",
                "sha256": sha256_bytes(path.read_bytes()),
                "probes": [
                    {"kind": "version", "args": ["--version"], "expected_exit": 0},
                    {"kind": "behavior", "args": ["--help"], "expected_exit": 0},
                ],
            }
        for command in REQUIRED_GIT_HELPERS:
            helper = root / "libexec/git-core" / f"git-{command}"
            helper.parent.mkdir(parents=True, exist_ok=True)
            os.link(root / "bin/git", helper)
        (root / "licenses").mkdir()
        (root / "licenses/NOTICE").write_text("test licenses\n", encoding="utf-8")
        sbom = {"bomFormat": "CycloneDX", "specVersion": "1.5", "components": []}
        (root / "sbom.cdx.json").write_bytes(canonical_json(sbom))
        payload = sorted(
            path for path in root.rglob("*") if path.is_file()
        )
        checksums = "".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.relative_to(root).as_posix()}\n"
            for path in payload
        ).encode()
        (root / "checksums.sha256").write_bytes(checksums)
        manifest = {
            "schema_version": "shuttle_external_tools_bundle/v1",
            "bundle_id": "shuttle-tools",
            "bundle_version": "0.2.0",
            "platform": {"os": "linux", "arch": "x86_64", "libc": "glibc"},
            "runtime_abis": ["shuttle-runtime/v1"],
            "profiles": profiles,
            "tools": tools,
            "payload_manifest": {"path": "checksums.sha256", "sha256": sha256_bytes(checksums)},
            "source_archives": [
                {"name": f"{name}_1.0.0.orig.tar.xz", "packages": [name], "source_package": name, "source_version": "1.0.0-1", "url": f"https://snapshot.debian.org/{name}.tar.xz", "sha256": "sha256:" + str(index) * 64}
                for index, name in enumerate(("git", "rg", "ctags", "tar", "ps"), 1)
            ],
            "binary_archives": [
                {"name": f"{name}_1.0.0-1.deb", "package": name, "version": "1.0.0-1", "sha256": "sha256:" + str(index) * 64, "role": "payload_source"}
                for index, name in enumerate(("git", "rg", "ctags", "tar", "ps"), 1)
            ],
            "license_notices": "licenses/NOTICE",
            "sbom_ref": "sbom.cdx.json",
        }
        (root / "manifest.json").write_bytes(canonical_json(manifest))

    def test_build_contract_binds_snapshot_binary_and_source_archives(self) -> None:
        contract = json.loads(
            (REPO_ROOT / "release/external-tools-contract.json").read_text(encoding="utf-8")
        )
        from external_tools_bundle import validate_contract

        validate_contract(contract)
        self.assertEqual(contract["debian_snapshot"], "20260824T000000Z")
        for entry in contract["tools"].values():
            self.assertIn("package_version", entry)
            self.assertEqual(set(entry["archive_sha256"]), {"amd64", "arm64"})
            self.assertTrue(entry["source_archives"])
            self.assertTrue(
                all(source["url"].startswith("https://snapshot.debian.org/") for source in entry["source_archives"])
            )

    def test_production_rust_resolves_fixed_mechanical_tools(self) -> None:
        violations = []
        for source_path in sorted((REPO_ROOT / "crates").glob("*/src/**/*.rs")):
            source = production_rust_source(source_path.read_text(encoding="utf-8"))
            for match in HARDCODED_MECHANICAL_COMMAND.finditer(source):
                line = source.count("\n", 0, match.start()) + 1
                violations.append(
                    f"{source_path.relative_to(REPO_ROOT)}:{line}: {match.group('tool')}"
                )

        self.assertEqual(
            violations,
            [],
            "fixed mechanical tools must be resolved through shuttle-runtime-tools before "
            "Command::new(...):\n" + "\n".join(violations),
        )

    def test_mechanical_command_scanner_ignores_only_trailing_unit_tests(self) -> None:
        fixture = """
fn production() { Command::new("git"); }
#[cfg(test)]
mod tests {
    fn helper() { std::process::Command::new("ast-grep"); }
}
"""
        matches = [
            match.group("tool")
            for match in HARDCODED_MECHANICAL_COMMAND.finditer(
                production_rust_source(fixture)
            )
        ]
        self.assertEqual(matches, ["git"])

    def test_bundle_export_is_deterministic_and_verifiable(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            root = base / "bundle"
            root.mkdir()
            self.fixture_bundle(root)
            verify_bundle(root)
            first = export_bundle(root, base / "one", 1_700_000_000)
            second = export_bundle(root, base / "two", 1_700_000_000)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            verify_archive(first)

    def test_source_archive_identity_covers_runtime_binary_package(self) -> None:
        from external_tools_bundle import apt_source_archives

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            destination = root / "glibc-2.36-1"
            destination.mkdir()
            payload = destination / "glibc_2.36.orig.tar.xz"
            payload.write_bytes(b"source")
            with mock.patch(
                "external_tools_bundle.source_package", return_value=("glibc", "2.36-1")
            ), mock.patch(
                "external_tools_bundle.subprocess.run",
                return_value=mock.Mock(
                    stdout=(
                        "'http://snapshot.debian.org/archive/debian/test/pool/main/g/"
                        "glibc/glibc_2.36.orig.tar.xz' glibc_2.36.orig.tar.xz 6 "
                        f"SHA256:{hashlib.sha256(b'source').hexdigest()}\n"
                    )
                ),
            ):
                archives = apt_source_archives("libc6:arm64", "2.36-1", root)
            self.assertEqual(archives[0]["packages"], ["libc6:arm64"])
            self.assertEqual(archives[0]["source_package"], "glibc")
            self.assertEqual(archives[0]["source_version"], "2.36-1")
            self.assertTrue(archives[0]["url"].startswith("https://snapshot.debian.org/"))
            self.assertTrue(archives[0]["url"].endswith("glibc_2.36.orig.tar.xz"))
            self.assertEqual(archives[0]["sha256"], sha256_bytes(b"source"))

    def test_source_archive_collection_reuses_shared_source_package(self) -> None:
        source_archives = [
            {
                "name": "procps_4.0.2.orig.tar.xz",
                "packages": ["libproc2-0"],
                "source_package": "procps",
                "source_version": "4.0.2-3",
                "url": "https://snapshot.debian.org/procps.tar.xz",
                "sha256": "sha256:" + "a" * 64,
            }
        ]
        with mock.patch(
            "external_tools_bundle.source_package",
            return_value=("procps", "4.0.2-3"),
        ), mock.patch(
            "external_tools_bundle.apt_source_archives",
            return_value=source_archives,
        ) as downloader:
            merged = collect_source_archives(
                {"libproc2-0": "2:4.0.2-3", "procps": "2:4.0.2-3"},
                Path("/tmp"),
            )

        downloader.assert_called_once()
        self.assertEqual(merged[0]["packages"], ["libproc2-0", "procps"])

    def test_checksum_closure_excludes_only_root_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "nested").mkdir()
            for relative in ("manifest.json", "checksums.sha256", "nested/checksums.sha256"):
                (root / relative).write_text(relative, encoding="utf-8")
            self.assertEqual(
                [path.relative_to(root).as_posix() for path in payload_files(root)],
                ["nested/checksums.sha256"],
            )

    def test_bundle_requires_nonempty_regular_git_helper_payload(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture_bundle(root)
            helper = root / "libexec/git-core/git-archive"
            self.assertTrue(helper.is_file())
            helper.unlink()
            checksums = (root / "checksums.sha256").read_text(encoding="utf-8")
            (root / "checksums.sha256").write_text(
                "\n".join(line for line in checksums.splitlines() if not line.endswith("libexec/git-core/git-archive")) + "\n",
                encoding="utf-8",
            )
            manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
            manifest["payload_manifest"]["sha256"] = sha256_bytes((root / "checksums.sha256").read_bytes())
            (root / "manifest.json").write_bytes(canonical_json(manifest))
            with self.assertRaisesRegex(ValueError, "Git helper"):
                verify_bundle(root)

    def test_core_docker_stage_copies_dynamic_loader_at_absolute_path(self) -> None:
        dockerfile = (REPO_ROOT / "Dockerfile").read_text(encoding="utf-8")
        self.assertIn("ldd /usr/local/bin/shuttle", dockerfile)
        self.assertIn('destination="/core-root${library}"', dockerfile)
        self.assertIn('cp -L "$library" "$destination"', dockerfile)
        self.assertRegex(dockerfile, r"FROM scratch AS core[\s\S]+COPY --from=core-builder /core-root/ /")
        self.assertIn("snapshot.debian.org/archive/debian/", dockerfile)
        self.assertIn("/release/packages/git_*.deb", dockerfile)
        bundle_builder = (REPO_ROOT / "release/external_tools_bundle.py").read_text()
        self.assertIn("--sort=no", bundle_builder)
        self.assertIn("--force-rpath", bundle_builder)
        self.assertIn("os.path.samefile", bundle_builder)

    def test_full_release_image_is_composed_from_immutable_components(self) -> None:
        dockerfile = (REPO_ROOT / "release/Dockerfile.full").read_text(encoding="utf-8")
        self.assertIn("FROM ${CORE_IMAGE} AS core-component", dockerfile)
        self.assertIn("FROM ${TOOLS_IMAGE} AS tools-component", dockerfile)
        self.assertIn('io.shuttle.core.image="${CORE_COMPONENT_DIGEST}"', dockerfile)
        self.assertIn('io.shuttle.tools.image="${TOOLS_COMPONENT_DIGEST}"', dockerfile)
        self.assertIn("ca-certificates=20230311+deb12u1", dockerfile)
        self.assertIn("COPY --from=core-component / /", dockerfile)
        self.assertIn("COPY --from=compat-builder /compat-root/ /", dockerfile)
        self.assertIn("io.shuttle.compatibility=\"debian-shell-ca-v1\"", dockerfile)
        self.assertIn("PATH=/opt/shuttle-tools/bin:/usr/local/bin:/usr/bin:/bin", dockerfile)
        verifier = (REPO_ROOT / "release/verify-container.sh").read_text()
        self.assertIn("python3 -c 'import json,sys;", verifier)
        self.assertNotIn('python3 - "$config" -c', verifier)
        self.assertIn("member.isfile() and member.mode & 0o111", verifier)

    def test_bundle_verifier_rejects_tampering_and_forbidden_tools(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture_bundle(root)
            (root / "bin/git").write_text("tampered", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "payload closure"):
                verify_bundle(root)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture_bundle(root)
            (root / "bin/docker").write_text("forbidden", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "forbidden"):
                verify_bundle(root)

    def test_split_image_tags_keep_full_compatibility(self) -> None:
        self.assertEqual(image_ref("registry.example/shuttle", "core"), "registry.example/shuttle-core")
        self.assertEqual(image_ref("registry.example/shuttle", "tools"), "registry.example/shuttle-tools")
        self.assertEqual(image_ref("registry.example/shuttle", "full"), "registry.example/shuttle")
        self.assertEqual(
            variant_tags("registry.example/shuttle", "full", "1.2.3")[0],
            "registry.example/shuttle:1.2.3",
        )

    def test_container_receipt_requires_three_distinct_digests(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            metadata = Path(tmp)
            for index, variant in enumerate(("core", "tools", "full"), 1):
                (metadata / f"{variant}.json").write_text(
                    json.dumps({"containerimage.digest": "sha256:" + str(index) * 64}),
                    encoding="utf-8",
                )
                (metadata / f"{variant}.index.json").write_text(
                    json.dumps(
                        {
                            "manifests": [
                                {"digest": "sha256:" + str(index + 3) * 64, "platform": {"os": "linux", "architecture": "amd64"}},
                                {"digest": "sha256:" + str(index + 6) * 64, "platform": {"os": "linux", "architecture": "arm64"}},
                            ]
                        }
                    ),
                    encoding="utf-8",
                )
            receipt = assemble_receipt("registry.example/shuttle", "1.2.3", "a" * 40, metadata)
            validate_receipt(receipt, "1.2.3", "a" * 40)
            self.assertEqual(
                receipt["images"]["full"]["components"],
                {
                    "core": receipt["images"]["core"]["digest"],
                    "tools": receipt["images"]["tools"]["digest"],
                },
            )
            receipt["images"]["full"]["digest"] = receipt["images"]["core"]["digest"]
            with self.assertRaisesRegex(ValueError, "distinct"):
                validate_receipt(receipt, "1.2.3", "a" * 40)

    def test_platform_digest_parser_requires_both_architectures(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            index = Path(tmp) / "index.json"
            index.write_text(
                json.dumps({"manifests": [{"digest": "sha256:" + "1" * 64, "platform": {"os": "linux", "architecture": "amd64"}}]}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "platform set drifted"):
                platform_digests(index, ["linux/amd64", "linux/arm64"])

    def test_size_budget_requires_exact_measured_exception(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            exceptions = Path(tmp) / "exceptions.json"
            exceptions.write_text(
                json.dumps({"schema_version": "shuttle_container_size_exceptions/v1", "exceptions": []}),
                encoding="utf-8",
            )
            self.assertEqual(
                validate_size("core", "linux/amd64", 30 * 1024 * 1024, "abc", exceptions),
                "within-budget",
            )
            with self.assertRaisesRegex(ValueError, "measured.*exception"):
                validate_size("core", "linux/amd64", 30 * 1024 * 1024 + 1, "abc", exceptions)
            exceptions.write_text(
                json.dumps(
                    {
                        "schema_version": "shuttle_container_size_exceptions/v1",
                        "exceptions": [
                            {
                                "variant": "core",
                                "platform": "linux/amd64",
                                "source_commit": "abc",
                                "measured_bytes": 30 * 1024 * 1024 + 1,
                                "budget_bytes": 30 * 1024 * 1024,
                                "reason": "measured release exception",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            self.assertEqual(
                validate_size("core", "linux/amd64", 30 * 1024 * 1024 + 1, "abc", exceptions),
                "measured-exception",
            )


if __name__ == "__main__":
    unittest.main()
