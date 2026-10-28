#!/usr/bin/env python3
"""Build, verify, and archive the immutable /opt/shuttle-tools bundle."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.request import urlopen

SCHEMA = "shuttle_external_tools_bundle/v1"
CONTRACT_SCHEMA = "shuttle_external_tools_build_contract/v1"
REQUIRED_TOOLS = {"git", "rg", "ctags", "tar", "ps"}
FORBIDDEN_NAMES = {"docker", "container-use", "node", "pi"}
ARCHES = {"amd64": "x86_64", "arm64": "aarch64"}
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
REQUIRED_GIT_HELPERS = (
    "add",
    "archive",
    "diff",
    "init",
    "ls-files",
    "rev-parse",
    "status",
    "symbolic-ref",
    "update-ref",
    "write-tree",
)


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def sha256_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def validate_relative(value: str) -> None:
    path = PurePosixPath(value)
    if not value or path.is_absolute() or ".." in path.parts or str(path) != value:
        raise ValueError(f"bundle path is not normalized and relative: {value!r}")


def payload_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"symlinks are forbidden in the bundle: {path}")
        if path.is_file() and path not in {
            root / "manifest.json",
            root / "checksums.sha256",
        }:
            files.append(path)
    return files


def checksum_payload(root: Path) -> bytes:
    return "".join(
        f"{sha256_file(path)[7:]}  {path.relative_to(root).as_posix()}\n"
        for path in payload_files(root)
    ).encode()


def package_version(package: str) -> str:
    return subprocess.check_output(
        ["dpkg-query", "-W", "-f=${Version}", package], text=True
    ).strip()


def source_package(package: str) -> tuple[str, str]:
    raw = subprocess.check_output(
        [
            "dpkg-query",
            "-W",
            "-f=${binary:Package}|${source:Package}|${source:Version}",
            package,
        ],
        text=True,
    ).strip()
    _binary, source, version = raw.split("|", 2)
    return source or package_basename(package), version or package_version(package)


def apt_source_archives(package: str, package_release: str, source_root: Path) -> list[dict[str, str]]:
    source_name, source_version = source_package(package)
    destination = source_root / f"{source_name}-{source_version.replace(':', '%3a')}"
    destination.mkdir(parents=True, exist_ok=True)
    coordinates = subprocess.run(
        [
            "apt-get",
            "source",
            "--print-uris",
            "--download-only",
            f"{source_name}={source_version}",
        ],
        cwd=destination,
        check=True,
        capture_output=True,
        text=True,
    )
    locked: dict[str, tuple[str, str]] = {}
    for line in coordinates.stdout.splitlines():
        match = re.fullmatch(r"'([^']+)' (\S+) \d+ SHA256:([0-9a-f]{64})", line)
        if match:
            url, name, digest = match.groups()
            locked[name] = (url.replace("http://", "https://", 1), f"sha256:{digest}")
    if not locked:
        raise ValueError(f"APT did not report source archive coordinates for {package}")
    before = {path.name for path in destination.iterdir()}
    if not before:
        subprocess.run(
            ["apt-get", "source", "--download-only", f"{source_name}={source_version}"],
            cwd=destination,
            check=True,
            stdout=subprocess.DEVNULL,
        )
    files = sorted(
        path
        for path in destination.iterdir()
        if path.is_file() and (path.name not in before or path.suffix in {".dsc", ".gz", ".xz", ".bz2"})
    )
    if not files:
        raise ValueError(f"no source archive downloaded for {package}")
    if {path.name for path in files} != set(locked):
        raise ValueError(f"downloaded source archive set drifted for {package}")
    for path in files:
        if sha256_file(path) != locked[path.name][1]:
            raise ValueError(f"downloaded source archive digest drifted for {path.name}")
    return [
        {
            "name": path.name,
            "packages": [package],
            "source_package": source_name,
            "source_version": source_version,
            "url": locked[path.name][0],
            "sha256": locked[path.name][1],
        }
        for path in files
    ]


def collect_source_archives(
    installed: dict[str, str], source_root: Path
) -> list[dict[str, Any]]:
    """Download each Debian source once and bind all binary packages to it."""

    source_archive_map: dict[tuple[str, str, str, str], dict[str, Any]] = {}
    source_package_cache: dict[tuple[str, str], list[dict[str, str]]] = {}
    for package, package_release in sorted(installed.items()):
        source_identity = source_package(package)
        archives = source_package_cache.get(source_identity)
        if archives is None:
            archives = apt_source_archives(package, package_release, source_root)
            source_package_cache[source_identity] = archives
        for cached in archives:
            archive = {**cached, "packages": [package]}
            identity = (
                archive["source_package"],
                archive["source_version"],
                archive["name"],
                archive["sha256"],
            )
            existing = source_archive_map.setdefault(identity, archive)
            existing["packages"] = sorted(
                set(existing["packages"]) | set(archive["packages"])
            )
    return sorted(
        source_archive_map.values(),
        key=lambda item: (item["source_package"], item["name"]),
    )


def copy_regular(source: Path, destination: Path, executable: bool = False) -> None:
    resolved = source.resolve(strict=True)
    if not resolved.is_file():
        raise ValueError(f"bundle input is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(resolved, destination)
    destination.chmod(0o755 if executable else 0o644)


def dynamic_libraries(executable: Path) -> list[Path]:
    output = subprocess.check_output(["ldd", str(executable)], text=True)
    libraries: set[Path] = set()
    for line in output.splitlines():
        match = re.search(r"=>\s+(/\S+)", line) or re.match(r"\s*(/\S+)", line)
        if match:
            libraries.add(Path(match.group(1)))
    return sorted(libraries)


def copy_library(source: Path, root: Path, *, executable: bool = False) -> None:
    destination = root / "lib" / source.name
    if destination.exists():
        if sha256_file(destination) != sha256_file(source.resolve(strict=True)):
            raise ValueError(f"shared-library basename collision: {source.name}")
        if executable:
            destination.chmod(0o755)
        return
    copy_regular(source, destination, executable=executable)


def relocate_elf(source: Path, destination: Path, root: Path) -> list[Path]:
    if not destination.read_bytes().startswith(b"\x7fELF"):
        raise ValueError(f"tool is not an ELF executable: {source}")
    interpreter = subprocess.check_output(
        ["patchelf", "--print-interpreter", str(destination)], text=True
    ).strip()
    runtime_files = dynamic_libraries(source.resolve(strict=True))
    if interpreter:
        interpreter_source = Path(interpreter)
        copy_library(interpreter_source, root, executable=True)
        runtime_files.append(interpreter_source)
        subprocess.run(
            [
                "patchelf",
                "--set-interpreter",
                f"/opt/shuttle-tools/lib/{interpreter_source.name}",
                str(destination),
            ],
            check=True,
        )
    relative_lib = os.path.relpath(root / "lib", destination.parent)
    subprocess.run(
        [
            "patchelf",
            "--force-rpath",
            "--set-rpath",
            f"$ORIGIN/{relative_lib}",
            str(destination),
        ],
        check=True,
    )
    for library in runtime_files:
        copy_library(library, root)
    return sorted(set(runtime_files))


def owning_package(path: Path) -> str:
    candidates = [path, path.resolve(strict=True)]
    for candidate in candidates:
        result = subprocess.run(
            ["dpkg-query", "-S", str(candidate)],
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode == 0:
            return result.stdout.split(": ", 1)[0].strip()
    raise ValueError(f"no distro package owns runtime file: {path}")


def package_basename(package: str) -> str:
    return package.split(":", 1)[0]


def copy_git_helpers(git: Path, root: Path) -> set[str]:
    exec_path = Path(
        subprocess.check_output([str(git.resolve(strict=True)), "--exec-path"], text=True).strip()
    )
    destination_dir = root / "libexec/git-core"
    destination_dir.mkdir(parents=True, exist_ok=True)
    runtime_packages: set[str] = set()
    git_destination = root / "bin/git"
    for command in REQUIRED_GIT_HELPERS:
        source = exec_path / f"git-{command}"
        resolved = source.resolve(strict=True)
        destination = destination_dir / source.name
        if os.path.samefile(resolved, git.resolve(strict=True)):
            os.link(git_destination, destination)
            continue
        copy_regular(resolved, destination, executable=True)
        if destination.read_bytes().startswith(b"\x7fELF"):
            for runtime_file in relocate_elf(resolved, destination, root):
                runtime_packages.add(owning_package(runtime_file))
    return runtime_packages


def validate_contract(contract: dict[str, Any]) -> None:
    if contract.get("schema_version") != CONTRACT_SCHEMA:
        raise ValueError("unsupported external tools build contract")
    tools = contract.get("tools")
    if not isinstance(tools, dict) or set(tools) != REQUIRED_TOOLS:
        raise ValueError("build contract must declare exactly the required tool set")
    profiles = contract.get("profiles")
    if not isinstance(profiles, dict) or set(profiles) != {
        "cell-supervisor",
        "source-analysis",
    }:
        raise ValueError("build contract profile set drifted")
    if set(contract.get("forbidden_tools", [])) != FORBIDDEN_NAMES:
        raise ValueError("build contract forbidden tool set drifted")
    packages = {entry.get("package") for entry in tools.values()}
    if set(contract.get("license_packages", [])) != packages:
        raise ValueError("license package set must match the declared tool packages")
    if not re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", str(contract.get("debian_snapshot", ""))):
        raise ValueError("build contract must pin one Debian snapshot timestamp")
    for tool_id, entry in tools.items():
        if not isinstance(entry, dict):
            raise ValueError(f"tool contract must be an object: {tool_id}")
        if not isinstance(entry.get("package_version"), str) or not entry["package_version"]:
            raise ValueError(f"{tool_id} must pin one exact package version")
        archive_sha256 = entry.get("archive_sha256")
        if (
            not isinstance(archive_sha256, dict)
            or set(archive_sha256) != set(ARCHES)
            or any(DIGEST.fullmatch(str(value)) is None for value in archive_sha256.values())
        ):
            raise ValueError(f"{tool_id} must pin amd64 and arm64 archive SHA-256 values")
        source_archives = entry.get("source_archives")
        if not isinstance(source_archives, list) or not source_archives:
            raise ValueError(f"{tool_id} must bind upstream source archives")
        for source_archive in source_archives:
            if (
                not isinstance(source_archive, dict)
                or set(source_archive) != {"name", "version", "url", "sha256"}
                or not source_archive.get("name")
                or not source_archive.get("version")
                or not str(source_archive.get("url", "")).startswith("https://snapshot.debian.org/")
                or DIGEST.fullmatch(str(source_archive.get("sha256", ""))) is None
            ):
                raise ValueError(f"{tool_id} source archive identity is invalid")
        validate_relative(str(entry.get("path", "")))
        kinds = {probe.get("kind") for probe in entry.get("probes", [])}
        if kinds != {"version", "behavior"}:
            raise ValueError(f"{tool_id} must have version and behavior probes")


def preflight_inputs(
    contract_path: Path,
    package_archives: Path,
    source_archives: Path,
    arch: str,
    snapshot: str,
) -> None:
    """Download and authenticate immutable build inputs before tool installation."""

    contract = read_json(contract_path)
    validate_contract(contract)
    if arch not in ARCHES:
        raise ValueError(f"unsupported Docker architecture: {arch}")
    if snapshot != contract["debian_snapshot"]:
        raise ValueError("Docker Debian snapshot differs from the checked-in lock")
    package_archives.mkdir(parents=True, exist_ok=True)
    source_archives.mkdir(parents=True, exist_ok=True)
    for entry in contract["tools"].values():
        package = entry["package"]
        version = entry["package_version"]
        subprocess.run(
            ["apt-get", "download", f"{package}={version}"],
            cwd=package_archives,
            check=True,
        )
        matches = sorted(package_archives.glob(f"{package}_*.deb"))
        if len(matches) != 1 or sha256_file(matches[0]) != entry["archive_sha256"][arch]:
            raise ValueError(f"pre-install package archive digest drifted for {package}")
        for source in entry["source_archives"]:
            destination = source_archives / source["name"]
            if not destination.exists():
                with urlopen(source["url"], timeout=120) as response, destination.open("wb") as output:
                    shutil.copyfileobj(response, output)
            if sha256_file(destination) != source["sha256"]:
                raise ValueError(f"source archive digest drifted for {source['name']}")


def stage_bundle(
    root: Path,
    contract_path: Path,
    package_archives: Path,
    version: str,
    arch: str,
    ctags_source: Path | None = None,
    source_archive_root: Path | None = None,
) -> None:
    contract = read_json(contract_path)
    validate_contract(contract)
    if arch not in ARCHES:
        raise ValueError(f"unsupported Docker architecture: {arch}")
    if root.exists() and any(root.iterdir()):
        raise ValueError(f"bundle root must be empty: {root}")
    root.mkdir(parents=True, exist_ok=True)
    installed: dict[str, str] = {}
    runtime_packages: set[str] = set()
    tools: dict[str, Any] = {}
    for tool_id, entry in sorted(contract["tools"].items()):
        package = entry["package"]
        installed_version = package_version(package)
        if installed_version != entry["package_version"]:
            raise ValueError(
                f"{package} version {installed_version!r} violates checked-in contract "
                f"{entry['package_version']!r}"
            )
        installed[package] = installed_version
        runtime_packages.add(package)
        source = (
            ctags_source
            if tool_id == "ctags" and ctags_source is not None
            else Path(entry["source_path"])
        )
        destination = root / entry["path"]
        copy_regular(source, destination, executable=True)
        for runtime_file in relocate_elf(source, destination, root):
            runtime_packages.add(owning_package(runtime_file))
        tools[tool_id] = {
            "path": entry["path"],
            "version": installed_version,
            "sha256": sha256_file(destination),
            "probes": entry["probes"],
        }
        if tool_id == "git":
            runtime_packages.update(copy_git_helpers(source, root))

    template_source = Path("/usr/share/git-core/templates")
    if not template_source.is_dir():
        raise ValueError("Git template directory is missing")
    for source in sorted(template_source.rglob("*")):
        if source.is_file():
            copy_regular(
                source, root / "share/git-core/templates" / source.relative_to(template_source)
            )

    tool_packages = dict(installed)
    installed = {package: package_version(package) for package in sorted(runtime_packages)}
    package_archives.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "apt-get",
            "download",
            *[f"{package}={package_release}" for package, package_release in installed.items()],
        ],
        cwd=package_archives,
        check=True,
    )

    license_dir = root / "licenses"
    license_dir.mkdir(parents=True, exist_ok=True)
    notices = []
    for package in sorted(installed):
        base_package = package_basename(package)
        source = Path("/usr/share/doc") / base_package / "copyright"
        destination = license_dir / f"{base_package}.copyright"
        copy_regular(source, destination)
        notices.append(f"{package} {installed[package]}: licenses/{destination.name}")
    (license_dir / "NOTICE").write_text("\n".join(notices) + "\n", encoding="utf-8")

    binary_archives = []
    archive_digests = {}
    for package, package_release in sorted(installed.items()):
        base_package = package_basename(package)
        matches = sorted(package_archives.glob(f"{base_package}_*.deb"))
        if len(matches) != 1:
            raise ValueError(f"expected one downloaded distro archive for {package}")
        archive_digest = sha256_file(matches[0])
        locked_archive_digest = next(
            entry["archive_sha256"][arch]
            for entry in contract["tools"].values()
            if entry["package"] == package
        ) if package in tool_packages else archive_digest
        if archive_digest != locked_archive_digest:
            raise ValueError(f"downloaded archive digest drifted for {package}")
        archive_digests[package] = archive_digest
        binary_archives.append(
            {
                "name": matches[0].name,
                "package": package,
                "version": package_release,
                "sha256": archive_digest,
                "role": (
                    "build_reference"
                    if ctags_source is not None
                    and package == contract["tools"]["ctags"]["package"]
                    else "payload_source"
                ),
            }
        )
    source_archive_root = source_archive_root or package_archives / "sources"
    source_archives = collect_source_archives(installed, source_archive_root)

    components = []
    for package, package_release in sorted(installed.items()):
        license_file = license_dir / f"{package_basename(package)}.copyright"
        component = {
            "type": "application" if package in tool_packages else "library",
            "bom-ref": f"pkg:deb/debian/{package}@{package_release}",
            "name": package,
            "version": package_release,
            "purl": f"pkg:deb/debian/{package}@{package_release}",
            "properties": [
                {"name": "shuttle:license-notice", "value": f"licenses/{license_file.name}"},
                {"name": "shuttle:binary-archive-sha256", "value": archive_digests[package]},
                {
                    "name": "shuttle:binary-archive-role",
                    "value": next(
                        item["role"]
                        for item in binary_archives
                        if item["package"] == package
                    ),
                },
            ],
        }
        matching_sources = [item for item in source_archives if package in item["packages"]]
        for item in matching_sources:
            component["properties"].extend(
                [
                    {"name": "shuttle:source-package", "value": item["source_package"]},
                    {"name": "shuttle:source-version", "value": item["source_version"]},
                    {"name": "shuttle:source-archive", "value": item["name"]},
                    {"name": "shuttle:source-url", "value": item["url"]},
                    {"name": "shuttle:source-archive-sha256", "value": item["sha256"]},
                ]
            )
        if package in archive_digests:
            component["hashes"] = [
                {"alg": "SHA-256", "content": archive_digests[package][7:]}
            ]
        components.append(component)
    sbom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{hashlib.sha256(f'{version}:{arch}'.encode()).hexdigest()[:8]}-0000-4000-8000-{hashlib.sha256(f'{arch}:{version}'.encode()).hexdigest()[:12]}",
        "version": 1,
        "metadata": {
            "component": {
                "type": "container",
                "name": contract["bundle_id"],
                "version": version,
            }
        },
        "components": components,
    }
    sbom_path = root / "sbom.cdx.json"
    sbom_path.write_bytes(canonical_json(sbom))
    checksums_path = root / "checksums.sha256"
    checksums = checksum_payload(root)
    checksums_path.write_bytes(checksums)
    manifest = {
        "schema_version": SCHEMA,
        "bundle_id": contract["bundle_id"],
        "bundle_version": version,
        "platform": {**contract["platform"], "arch": ARCHES[arch]},
        "runtime_abis": contract["runtime_abis"],
        "profiles": contract["profiles"],
        "tools": tools,
        "payload_manifest": {
            "path": "checksums.sha256",
            "sha256": sha256_bytes(checksums),
        },
        "source_archives": source_archives,
        "binary_archives": binary_archives,
        "license_notices": "licenses/NOTICE",
        "sbom_ref": "sbom.cdx.json",
    }
    (root / "manifest.json").write_bytes(canonical_json(manifest))
    verified = verify_bundle(root, contract_path)
    run_probes(root, verified)
    run_parity_fixture(root, verified)


def run_probes(root: Path, manifest: dict[str, Any]) -> None:
    for tool_id, entry in sorted(manifest["tools"].items()):
        executable = root / entry["path"]
        for probe in entry["probes"]:
            result = subprocess.run(
                [str(executable), *probe.get("args", [])],
                input=probe.get("stdin", ""),
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
                env={
                    "PATH": str(root / "bin"),
                    "GIT_EXEC_PATH": str(root / "libexec/git-core"),
                    "GIT_TEMPLATE_DIR": str(root / "share/git-core/templates"),
                    "LANG": "C",
                },
            )
            if result.returncode != probe["expected_exit"]:
                raise ValueError(f"{tool_id} {probe['kind']} probe exited {result.returncode}")
            for stream in ("stdout", "stderr"):
                expected = probe.get(f"{stream}_contains")
                if expected is not None and expected not in getattr(result, stream):
                    raise ValueError(f"{tool_id} {probe['kind']} probe {stream} drifted")


def run_parity_fixture(root: Path, manifest: dict[str, Any]) -> None:
    paths = {name: root / entry["path"] for name, entry in manifest["tools"].items()}
    env = {
        "PATH": str(root / "bin"),
        "GIT_EXEC_PATH": str(root / "libexec/git-core"),
        "GIT_TEMPLATE_DIR": str(root / "share/git-core/templates"),
        "LANG": "C",
    }
    with tempfile.TemporaryDirectory() as tmp:
        workspace = Path(tmp)
        repo = workspace / "repo"
        subprocess.run([paths["git"], "init", "-q", str(repo)], env=env, check=True)
        (repo / "main.rs").write_text("fn shuttle_probe() {}\n", encoding="utf-8")
        subprocess.run([paths["git"], "-C", repo, "add", "main.rs"], env=env, check=True)
        tracked = subprocess.check_output(
            [paths["git"], "-C", repo, "ls-files"], env=env, text=True
        ).strip()
        if tracked != "main.rs":
            raise ValueError("git add/ls-files parity failed")
        tree = subprocess.check_output(
            [paths["git"], "-C", repo, "write-tree"], env=env, text=True
        ).strip()
        subprocess.run(
            [paths["git"], "-C", repo, "update-ref", "refs/shuttle/probe", tree],
            env=env,
            check=True,
        )
        archive = workspace / "probe.tar"
        subprocess.run(
            [paths["git"], "-C", repo, "archive", f"--output={archive}", tree],
            env=env,
            check=True,
        )
        listing = subprocess.check_output([paths["tar"], "-tf", archive], env=env, text=True)
        if "main.rs" not in listing.splitlines():
            raise ValueError("git archive/tar parity failed")
        rg = subprocess.run(
            [paths["rg"], "--fixed-strings", "shuttle_probe", repo],
            env=env, text=True, capture_output=True, check=False,
        )
        if rg.returncode != 0 or "main.rs" not in rg.stdout:
            raise ValueError("ripgrep source-analysis parity failed")
        ctags = subprocess.run(
            [
                paths["ctags"],
                "--sort=no",
                "--output-format=json",
                "-o",
                "-",
                repo / "main.rs",
            ],
            env=env, text=True, capture_output=True, check=False,
        )
        if ctags.returncode != 0 or '"name": "shuttle_probe"' not in ctags.stdout:
            raise ValueError(
                "Universal Ctags JSON parser parity failed: "
                f"exit={ctags.returncode} stdout={ctags.stdout!r} stderr={ctags.stderr!r}"
            )
        ps = subprocess.run(
            [paths["ps"], "-o", "pid=,ppid=", "-p", "1"],
            env=env, text=True, capture_output=True, check=False,
        )
        if ps.returncode != 0 or len(ps.stdout.split()) != 2:
            raise ValueError("procps pid/ppid parity failed")


def parse_checksums(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\s].*)", line)
        if match is None:
            raise ValueError(f"malformed checksum line {line_number}")
        name = match.group(2)
        validate_relative(name)
        if name in values:
            raise ValueError(f"duplicate checksum path: {name}")
        values[name] = "sha256:" + match.group(1)
    return values


def verify_bundle(root: Path, contract_path: Path | None = None) -> dict[str, Any]:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("bundle root must be a real directory")
    manifest = read_json(root / "manifest.json")
    expected_keys = {
        "schema_version", "bundle_id", "bundle_version", "platform",
        "runtime_abis", "profiles", "tools", "payload_manifest",
        "source_archives", "binary_archives", "license_notices", "sbom_ref",
    }
    if set(manifest) != expected_keys or manifest.get("schema_version") != SCHEMA:
        raise ValueError("bundle manifest schema or top-level fields drifted")
    if set(manifest.get("tools", {})) != REQUIRED_TOOLS:
        raise ValueError("bundle tool set drifted")
    if manifest.get("platform", {}).get("os") != "linux" or manifest.get("platform", {}).get("libc") != "glibc":
        raise ValueError("bundle platform must be Linux glibc")
    if manifest.get("platform", {}).get("arch") not in set(ARCHES.values()):
        raise ValueError("bundle architecture is unsupported")
    archives = manifest.get("source_archives")
    if not isinstance(archives, list) or len(archives) < len(REQUIRED_TOOLS):
        raise ValueError("bundle source archive identities are incomplete")
    archive_names: set[str] = set()
    for archive in archives:
        if set(archive) != {"name", "packages", "source_package", "source_version", "url", "sha256"} or not re.fullmatch(
            r"sha256:[0-9a-f]{64}", str(archive.get("sha256", ""))
        ):
            raise ValueError("bundle source archive identity is invalid")
        if (
            not archive["name"]
            or not isinstance(archive["packages"], list)
            or not archive["packages"]
            or archive["packages"] != sorted(set(archive["packages"]))
            or not archive["source_package"]
            or not archive["source_version"]
            or not str(archive["url"]).startswith("https://snapshot.debian.org/")
            or archive["name"] in archive_names
        ):
            raise ValueError("bundle source archive identities must be non-empty and unique")
        archive_names.add(archive["name"])
    binary_archives = manifest.get("binary_archives")
    if not isinstance(binary_archives, list) or len(binary_archives) < len(REQUIRED_TOOLS):
        raise ValueError("bundle binary archive identities are incomplete")
    for archive in binary_archives:
        if set(archive) != {"name", "package", "version", "sha256", "role"} or DIGEST.fullmatch(
            str(archive.get("sha256", ""))
        ) is None:
            raise ValueError("bundle binary archive identity is invalid")
        if archive["role"] not in {"payload_source", "build_reference"}:
            raise ValueError("bundle binary archive role is invalid")
    names = {path.name for path in root.rglob("*")}
    forbidden = FORBIDDEN_NAMES & names
    if forbidden:
        raise ValueError(f"forbidden runtime/provider tool present: {sorted(forbidden)}")
    checksums_path = root / manifest["payload_manifest"]["path"]
    checksums = parse_checksums(checksums_path)
    if sha256_file(checksums_path) != manifest["payload_manifest"]["sha256"]:
        raise ValueError("payload manifest digest mismatch")
    actual = {path.relative_to(root).as_posix(): sha256_file(path) for path in payload_files(root)}
    if actual != checksums:
        raise ValueError("payload closure differs from checksums.sha256")
    expected_helpers = {f"libexec/git-core/git-{name}" for name in REQUIRED_GIT_HELPERS}
    if not expected_helpers.issubset(actual):
        raise ValueError("required Git helper payload is incomplete")
    for tool_id, entry in manifest["tools"].items():
        validate_relative(entry["path"])
        if actual.get(entry["path"]) != entry["sha256"]:
            raise ValueError(f"tool checksum differs from payload manifest: {tool_id}")
        if not entry.get("version"):
            raise ValueError(f"tool version is empty: {tool_id}")
        if {probe.get("kind") for probe in entry.get("probes", [])} != {"version", "behavior"}:
            raise ValueError(f"tool probes are incomplete: {tool_id}")
    for reference in (manifest["license_notices"], manifest["sbom_ref"]):
        validate_relative(reference)
        if reference not in actual:
            raise ValueError(f"manifest reference is not checksum-bound: {reference}")
    sbom = read_json(root / manifest["sbom_ref"])
    if sbom.get("bomFormat") != "CycloneDX" or sbom.get("specVersion") != "1.5":
        raise ValueError("tool bundle SBOM is not CycloneDX 1.5")
    if contract_path is not None:
        contract = read_json(contract_path)
        validate_contract(contract)
        if manifest["profiles"] != contract["profiles"]:
            raise ValueError("bundle profiles differ from build contract")
        required_packages = set(contract["license_packages"] )
        if not required_packages.issubset(
            {package for item in archives for package in item["packages"]}
        ):
            raise ValueError("bundle source archives omit a declared tool package")
        if not required_packages.issubset({item["package"] for item in binary_archives}):
            raise ValueError("bundle binary archives omit a declared tool package")
    return manifest


def deterministic_tar(root: Path, epoch: int) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as archive:
        archive.dereference = True
        for path in sorted(root.rglob("*")):
            relative = Path("opt/shuttle-tools") / path.relative_to(root)
            info = archive.gettarinfo(str(path), arcname=relative.as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = epoch
            if info.isdir():
                info.mode = 0o755
                archive.addfile(info)
            else:
                info.mode = 0o755 if os.access(path, os.X_OK) else 0o644
                with path.open("rb") as handle:
                    archive.addfile(info, handle)
    return buffer.getvalue()


def export_bundle(root: Path, dist: Path, epoch: int) -> Path:
    manifest = verify_bundle(root)
    platform = manifest["platform"]
    arch = {value: key for key, value in ARCHES.items()}[platform["arch"]]
    stem = f"shuttle-tools-v{manifest['bundle_version']}-linux-{arch}"
    dist.mkdir(parents=True, exist_ok=True)
    archive_path = dist / f"{stem}.tar.zst"
    subprocess.run(
        ["zstd", "-q", "-19", "-T1", "-f", "-o", str(archive_path)],
        input=deterministic_tar(root, epoch),
        check=True,
    )
    manifest_path = dist / f"{stem}.manifest.json"
    manifest_path.write_bytes((root / "manifest.json").read_bytes())
    checksum_path = dist / f"{stem}.sha256"
    checksum_path.write_text(
        f"{sha256_file(archive_path)[7:]}  {archive_path.name}\n"
        f"{sha256_file(manifest_path)[7:]}  {manifest_path.name}\n",
        encoding="utf-8",
    )
    return archive_path


def verify_archive(archive_path: Path) -> None:
    checksum_path = archive_path.with_suffix("").with_suffix(".sha256")
    declared = parse_checksums(checksum_path)
    manifest_path = archive_path.with_suffix("").with_suffix(".manifest.json")
    for path in (archive_path, manifest_path):
        if declared.get(path.name) != sha256_file(path):
            raise ValueError(f"archive sidecar checksum mismatch: {path.name}")
    tar_bytes = subprocess.check_output(["zstd", "-q", "-d", "-c", str(archive_path)])
    with tempfile.TemporaryDirectory() as tmp:
        extraction = Path(tmp)
        with tarfile.open(fileobj=io.BytesIO(tar_bytes), mode="r:") as archive:
            members = archive.getmembers()
            for member in members:
                parts = PurePosixPath(member.name).parts
                if (
                    parts[:2] != ("opt", "shuttle-tools")
                    or ".." in parts
                    or not (member.isdir() or member.isreg())
                ):
                    raise ValueError(f"unsafe bundle archive member: {member.name}")
                target = extraction.joinpath(*parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError(f"bundle member has no payload: {member.name}")
                    target.write_bytes(source.read())
                    target.chmod(member.mode & 0o777)
        root = extraction / "opt/shuttle-tools"
        manifest = verify_bundle(root)
        if canonical_json(manifest) != manifest_path.read_bytes():
            raise ValueError("archive manifest differs from sidecar")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    stage = subparsers.add_parser("stage")
    stage.add_argument("--root", type=Path, required=True)
    stage.add_argument("--contract", type=Path, required=True)
    stage.add_argument("--package-archives", type=Path, required=True)
    stage.add_argument("--version", required=True)
    stage.add_argument("--arch", choices=sorted(ARCHES), required=True)
    stage.add_argument("--ctags-source", type=Path)
    stage.add_argument("--source-archive-root", type=Path)
    preflight = subparsers.add_parser("preflight-inputs")
    preflight.add_argument("--contract", type=Path, required=True)
    preflight.add_argument("--package-archives", type=Path, required=True)
    preflight.add_argument("--source-archives", type=Path, required=True)
    preflight.add_argument("--arch", choices=sorted(ARCHES), required=True)
    preflight.add_argument("--snapshot", required=True)
    verify = subparsers.add_parser("verify")
    verify.add_argument("--root", type=Path, required=True)
    verify.add_argument("--contract", type=Path)
    export = subparsers.add_parser("export")
    export.add_argument("--root", type=Path, required=True)
    export.add_argument("--dist", type=Path, required=True)
    export.add_argument("--source-date-epoch", type=int, required=True)
    verify_archive_parser = subparsers.add_parser("verify-archive")
    verify_archive_parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    if args.command == "stage":
        stage_bundle(
            args.root,
            args.contract,
            args.package_archives,
            args.version,
            args.arch,
            args.ctags_source,
            args.source_archive_root,
        )
    elif args.command == "preflight-inputs":
        preflight_inputs(
            args.contract, args.package_archives, args.source_archives, args.arch, args.snapshot
        )
    elif args.command == "verify":
        verify_bundle(args.root, args.contract)
    elif args.command == "export":
        print(export_bundle(args.root, args.dist, args.source_date_epoch))
    else:
        verify_archive(args.archive)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
