#!/usr/bin/env python3
"""Idempotently create/update a release and replace its assets."""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import secrets
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

from release_version import validate_tag, workspace_version


class ReleaseError(RuntimeError):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> None:
        raise urllib.error.HTTPError(req.full_url, code, msg, headers, fp)


class Client:
    def __init__(self, *, base: str, token: str, platform: str) -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.platform = platform

    def headers(self, *, json_body: bool = False) -> dict[str, str]:
        headers = {
            "Accept": "application/vnd.github+json" if self.platform == "github" else "application/json",
            "User-Agent": "shuttle-release/v1",
            "Authorization": f"token {self.token}" if self.platform == "gitea" else f"Bearer {self.token}",
        }
        if self.platform == "github":
            headers["X-GitHub-Api-Version"] = "2022-11-28"
        if json_body:
            headers["Content-Type"] = "application/json"
        return headers

    def request(
        self,
        method: str,
        path: str,
        *,
        payload: object | None = None,
        raw: bytes | None = None,
        headers: dict[str, str] | None = None,
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, Any]:
        body = raw
        request_headers = self.headers(json_body=payload is not None)
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode()
        request_headers.update(headers or {})
        url = path if path.startswith(("http://", "https://")) else self.base + path
        request = urllib.request.Request(url, data=body, headers=request_headers, method=method)
        try:
            with urllib.request.build_opener(NoRedirect).open(request, timeout=60) as response:
                status, data = response.status, response.read()
        except urllib.error.HTTPError as error:
            try:
                status, data = error.code, error.read()
            finally:
                error.close()
        if status not in expected:
            detail = data.decode("utf-8", errors="replace")[:2000].replace(
                self.token, "[REDACTED]"
            )
            raise ReleaseError(f"{method} {path} returned HTTP {status}: {detail}")
        if not data:
            return status, None
        try:
            return status, json.loads(data)
        except json.JSONDecodeError:
            return status, data


def multipart(path: Path, field: str) -> tuple[bytes, str]:
    boundary = "shuttle-release-" + secrets.token_hex(16)
    content_type = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="{field}"; filename="{path.name}"\r\n'
        f"Content-Type: {content_type}\r\n\r\n"
    ).encode() + path.read_bytes() + f"\r\n--{boundary}--\r\n".encode()
    return body, f"multipart/form-data; boundary={boundary}"


def segment(value: object) -> str:
    return urllib.parse.quote(str(value), safe="")


def release_prefix(owner: str, repo: str) -> str:
    return f"/repos/{segment(owner)}/{segment(repo)}/releases"


def validate_upload_url(client: Client, upload_url: str) -> str:
    base = urllib.parse.urlparse(client.base)
    target = urllib.parse.urlparse(upload_url)
    if target.username or target.password:
        raise ReleaseError("GitHub upload URL must not contain credentials")
    same_origin = target.scheme == base.scheme and target.netloc == base.netloc
    github_upload = (
        base.scheme == "https"
        and base.hostname == "api.github.com"
        and target.scheme == "https"
        and target.hostname == "uploads.github.com"
    )
    if not same_origin and not github_upload:
        raise ReleaseError("GitHub upload URL points to an untrusted origin")
    return upload_url


def release_payload(
    platform: str, *, tag: str, title: str, notes: str, commit: str, update: bool
) -> dict[str, object]:
    payload: dict[str, object] = {
        "tag_name": tag,
        "name": title,
        "body": notes,
        "prerelease": False,
    }
    if platform != "gitee":
        payload["target_commitish"] = commit
        payload["draft"] = False
    if update and platform != "gitee":
        payload.pop("tag_name")
    return payload


def get_or_create_release(client: Client, *, owner: str, repo: str, tag: str, title: str, notes: str, commit: str) -> dict[str, Any]:
    prefix = release_prefix(owner, repo)
    status, release = client.request(
        "GET", f"{prefix}/tags/{segment(tag)}", expected=(200, 404)
    )
    payload = release_payload(
        client.platform,
        tag=tag,
        title=title,
        notes=notes,
        commit=commit,
        update=False,
    )
    if status == 404:
        _status, release = client.request("POST", prefix, payload=payload, expected=(201,))
    else:
        if not isinstance(release, dict) or "id" not in release:
            raise ReleaseError("release lookup returned no release id")
        release_id = release["id"]
        update = release_payload(
            client.platform,
            tag=tag,
            title=title,
            notes=notes,
            commit=commit,
            update=True,
        )
        _status, release = client.request(
            "PATCH", f"{prefix}/{segment(release_id)}", payload=update, expected=(200,)
        )
    if not isinstance(release, dict) or "id" not in release:
        raise ReleaseError("release API returned no release id")
    return release


def list_assets(client: Client, *, owner: str, repo: str, release_id: object) -> list[dict[str, Any]]:
    suffix = "attach_files" if client.platform == "gitee" else "assets"
    _status, assets = client.request(
        "GET",
        f"{release_prefix(owner, repo)}/{segment(release_id)}/{suffix}?per_page=100",
        expected=(200,),
    )
    if not isinstance(assets, list):
        raise ReleaseError("release asset list is not an array")
    return [item for item in assets if isinstance(item, dict)]


def replace_asset(client: Client, *, owner: str, repo: str, release: dict[str, Any], path: Path) -> None:
    release_id = release["id"]
    existing = [
        item
        for item in list_assets(
            client, owner=owner, repo=repo, release_id=release_id
        )
        if item.get("name") == path.name
    ]
    if len(existing) > 1:
        raise ReleaseError(f"release has duplicate existing assets: {path.name}")
    suffix = "attach_files" if client.platform == "gitee" else "assets"
    prefix = f"{release_prefix(owner, repo)}/{segment(release_id)}/{suffix}"
    if existing:
        client.request(
            "DELETE", f"{prefix}/{segment(existing[0]['id'])}", expected=(204,)
        )
    if client.platform == "github":
        upload_base = str(release.get("upload_url", "")).split("{", 1)[0]
        if not upload_base:
            raise ReleaseError("GitHub release lacks upload_url")
        upload_url = validate_upload_url(client, upload_base) + "?" + urllib.parse.urlencode(
            {"name": path.name}
        )
        client.request(
            "POST",
            upload_url,
            raw=path.read_bytes(),
            headers={
                "Content-Type": mimetypes.guess_type(path.name)[0]
                or "application/octet-stream"
            },
            expected=(201,),
        )
        return
    body, content_type = multipart(
        path, "file" if client.platform == "gitee" else "attachment"
    )
    client.request(
        "POST",
        prefix,
        raw=body,
        headers={"Content-Type": content_type},
        expected=(201,),
    )


def release_assets(dist: Path, manifest: dict[str, Any]) -> list[Path]:
    names: list[str] = []
    for item in manifest.get("assets", []):
        if not isinstance(item, dict) or not isinstance(item.get("name"), str):
            raise ReleaseError("release manifest contains an invalid asset entry")
        name = item["name"]
        if Path(name).name != name:
            raise ReleaseError(f"release asset is not a basename: {name}")
        names.append(name)
    names.extend(["SHA256SUMS", "release-manifest.json"])
    if len(names) != len(set(names)):
        raise ReleaseError("release manifest contains duplicate publish assets")
    assets = [dist / name for name in names]
    missing = [path.name for path in assets if not path.is_file()]
    if missing:
        raise ReleaseError(f"release assets are missing: {', '.join(sorted(missing))}")
    return assets


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", choices=("github", "gitea", "gitee"), required=True)
    parser.add_argument("--api-base", required=True)
    parser.add_argument("--owner", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--token-env", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--title", required=True)
    parser.add_argument("--notes", type=Path, required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist/release"))
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    version = workspace_version(Path("Cargo.toml"))
    validate_tag(version, args.tag)
    notes = args.notes.read_text(encoding="utf-8")
    dist = args.dist.resolve(strict=True)
    manifest = json.loads((dist / "release-manifest.json").read_text(encoding="utf-8"))
    if manifest.get("source_commit") != args.commit or manifest.get("tag") != args.tag:
        raise ReleaseError("release manifest coordinates differ from publish request")
    assets = release_assets(dist, manifest)
    if args.dry_run:
        print(
            json.dumps(
                {
                    "platform": args.platform,
                    "tag": args.tag,
                    "commit": args.commit,
                    "assets": [item.name for item in assets],
                },
                sort_keys=True,
            )
        )
        return 0
    token = os.environ.get(args.token_env)
    if not token:
        raise ReleaseError(f"missing token environment variable: {args.token_env}")
    client = Client(base=args.api_base, token=token, platform=args.platform)
    release = get_or_create_release(
        client,
        owner=args.owner,
        repo=args.repo,
        tag=args.tag,
        title=args.title,
        notes=notes,
        commit=args.commit,
    )
    for path in assets:
        replace_asset(client, owner=args.owner, repo=args.repo, release=release, path=path)
    print(
        json.dumps(
            {
                "status": "published",
                "platform": args.platform,
                "release_id": release["id"],
                "asset_count": len(assets),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, ReleaseError) as error:
        print(f"release-platform: {error}", file=sys.stderr)
        raise SystemExit(1)
