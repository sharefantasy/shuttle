#!/usr/bin/env python3
"""Create a byte-reproducible release archive for one SHUTTLE binary."""

from __future__ import annotations

import argparse
import gzip
import io
import os
import subprocess
import tarfile
from pathlib import Path

from release_version import release_config, workspace_version


def source_epoch() -> int:
    configured = os.environ.get("SOURCE_DATE_EPOCH")
    if configured is not None:
        return int(configured)
    tag = os.environ.get("RELEASE_TAG")
    if not tag:
        raise ValueError("SOURCE_DATE_EPOCH or RELEASE_TAG is required")
    return int(
        subprocess.check_output(
            ["git", "show", "-s", "--format=%ct", f"{tag}^{{commit}}"], text=True
        )
    )


def tar_info(
    name: str, *, mode: int, size: int, epoch: int, directory: bool = False
) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.mtime = epoch
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.size = size
    if directory:
        info.type = tarfile.DIRTYPE
    return info


def add_bytes(
    archive: tarfile.TarFile, name: str, data: bytes, *, mode: int, epoch: int
) -> None:
    archive.addfile(
        tar_info(name, mode=mode, size=len(data), epoch=epoch), io.BytesIO(data)
    )


def package(binary: Path, suffix: str, dist: Path) -> Path:
    version = workspace_version(Path("Cargo.toml"))
    allowed = {item["asset_suffix"] for item in release_config()["binary_targets"]}
    if suffix not in allowed:
        raise ValueError(f"unsupported release asset suffix: {suffix}")
    if not binary.is_file():
        raise ValueError(f"release binary is missing: {binary}")
    epoch = source_epoch()
    root = f"shuttle-v{version}-{suffix}"
    dist.mkdir(parents=True, exist_ok=True)
    destination = dist / f"{root}.tar.gz"
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    with temporary.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as zipped:
            with tarfile.open(
                fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT
            ) as archive:
                archive.addfile(
                    tar_info(root, mode=0o755, size=0, epoch=epoch, directory=True)
                )
                add_bytes(
                    archive,
                    f"{root}/shuttle",
                    binary.read_bytes(),
                    mode=0o755,
                    epoch=epoch,
                )
                for source in (Path("LICENSE"), Path("README.md")):
                    add_bytes(
                        archive,
                        f"{root}/{source.name}",
                        source.read_bytes(),
                        mode=0o644,
                        epoch=epoch,
                    )
    temporary.replace(destination)
    print(destination)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--suffix", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist/release"))
    args = parser.parse_args()
    package(args.binary, args.suffix, args.dist)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
