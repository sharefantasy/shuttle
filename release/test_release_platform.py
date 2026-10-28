#!/usr/bin/env python3
"""Contract tests for the GitHub, Gitea, and Gitee release APIs."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "release"))

from release_platform import (
    Client,
    ReleaseError,
    get_or_create_release,
    release_payload,
    replace_asset,
    validate_upload_url,
)


class ApiState:
    def __init__(self, platform: str, token: str) -> None:
        self.platform = platform
        self.token = token
        self.release: dict[str, Any] | None = None
        self.assets: list[dict[str, Any]] = []
        self.requests: list[dict[str, Any]] = []
        self.server_url = ""


class ReleaseApiHandler(BaseHTTPRequestHandler):
    server: ThreadingHTTPServer

    def log_message(self, _format: str, *_args: object) -> None:
        return

    @property
    def state(self) -> ApiState:
        return self.server.state  # type: ignore[attr-defined]

    def record(self, body: bytes) -> None:
        self.state.requests.append(
            {
                "method": self.command,
                "path": self.path,
                "authorization": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type", ""),
                "body": body,
            }
        )

    def body(self) -> bytes:
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

    def send_json(self, status: int, payload: object) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        self.record(b"")
        if "/releases/tags/" in self.path:
            if self.state.release is None:
                self.send_json(404, {"message": "not found"})
            else:
                self.send_json(200, self.state.release)
            return
        if "/releases/1/assets" in self.path or "/releases/1/attach_files" in self.path:
            self.send_json(200, self.state.assets)
            return
        self.send_json(404, {"message": "unknown path"})

    def do_POST(self) -> None:
        body = self.body()
        self.record(body)
        if self.path.endswith("/releases"):
            payload = json.loads(body)
            release = {"id": 1, **payload}
            if self.state.platform == "github":
                release["upload_url"] = (
                    f"{self.state.server_url}/uploads/1/assets{{?name,label}}"
                )
            self.state.release = release
            self.send_json(201, release)
            return
        if self.path.startswith("/uploads/1/assets?"):
            self.state.assets = [{"id": 11, "name": "asset.bin"}]
            self.send_json(201, self.state.assets[0])
            return
        if self.path.endswith(("/assets", "/attach_files")):
            expected = (
                b'name="file"'
                if self.state.platform == "gitee"
                else b'name="attachment"'
            )
            if expected not in body:
                self.send_json(400, {"message": "wrong multipart field"})
                return
            self.state.assets = [{"id": 11, "name": "asset.bin"}]
            self.send_json(201, self.state.assets[0])
            return
        self.send_json(404, {"message": "unknown path"})

    def do_PATCH(self) -> None:
        body = self.body()
        self.record(body)
        payload = json.loads(body)
        assert self.state.release is not None
        self.state.release.update(payload)
        self.send_json(200, self.state.release)

    def do_DELETE(self) -> None:
        self.record(b"")
        self.state.assets = []
        self.send_response(204)
        self.end_headers()


class MockApi:
    def __init__(self, platform: str, token: str = "secret-release-token") -> None:
        self.state = ApiState(platform, token)
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), ReleaseApiHandler)
        self.server.state = self.state  # type: ignore[attr-defined]
        host, port = self.server.server_address
        self.state.server_url = f"http://{host}:{port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> MockApi:
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class ReleasePlatformTests(unittest.TestCase):
    def exercise_platform(self, platform: str) -> ApiState:
        with tempfile.TemporaryDirectory() as tmp, MockApi(platform) as api:
            asset = Path(tmp) / "asset.bin"
            asset.write_bytes(b"release bytes")
            client = Client(
                base=api.state.server_url, token=api.state.token, platform=platform
            )
            release = get_or_create_release(
                client,
                owner="owner name",
                repo="repo/name",
                tag="v0.2.0",
                title="first",
                notes="notes",
                commit="abc123",
            )
            replace_asset(
                client,
                owner="owner name",
                repo="repo/name",
                release=release,
                path=asset,
            )
            release = get_or_create_release(
                client,
                owner="owner name",
                repo="repo/name",
                tag="v0.2.0",
                title="updated",
                notes="new notes",
                commit="abc123",
            )
            replace_asset(
                client,
                owner="owner name",
                repo="repo/name",
                release=release,
                path=asset,
            )
            self.assertEqual(api.state.release["name"], "updated")
            self.assertEqual(api.state.assets, [{"id": 11, "name": "asset.bin"}])
            create = next(
                item
                for item in api.state.requests
                if item["method"] == "POST" and item["path"].endswith("/releases")
            )
            update = next(
                item for item in api.state.requests if item["method"] == "PATCH"
            )
            create_payload = json.loads(create["body"])
            update_payload = json.loads(update["body"])
            if platform == "gitee":
                self.assertNotIn("draft", create_payload)
                self.assertNotIn("target_commitish", create_payload)
                self.assertIn("tag_name", update_payload)
                self.assertNotIn("target_commitish", update_payload)
            else:
                self.assertFalse(create_payload["draft"])
                self.assertNotIn("tag_name", update_payload)
                self.assertEqual(update_payload["target_commitish"], "abc123")
            return api.state

    def test_release_payload_is_platform_specific(self) -> None:
        gitee = release_payload(
            "gitee", tag="v1.2.3", title="title", notes="notes", commit="abc", update=False
        )
        github = release_payload(
            "github", tag="v1.2.3", title="title", notes="notes", commit="abc", update=False
        )
        self.assertNotIn("target_commitish", gitee)
        self.assertNotIn("draft", gitee)
        self.assertEqual(github["target_commitish"], "abc")
        self.assertFalse(github["draft"])

    def test_create_update_delete_and_upload_for_all_platforms(self) -> None:
        for platform in ("github", "gitea", "gitee"):
            with self.subTest(platform=platform):
                state = self.exercise_platform(platform)
                methods = [item["method"] for item in state.requests]
                self.assertIn("POST", methods)
                self.assertIn("PATCH", methods)
                self.assertIn("DELETE", methods)
                self.assertTrue(
                    all(state.token not in item["path"] for item in state.requests)
                )
                expected_auth = (
                    f"token {state.token}"
                    if platform == "gitea"
                    else f"Bearer {state.token}"
                )
                self.assertTrue(
                    all(
                        item["authorization"] == expected_auth
                        for item in state.requests
                    )
                )
                if platform == "github":
                    self.assertTrue(
                        any(
                            item["path"] == "/uploads/1/assets?name=asset.bin"
                            and item["body"] == b"release bytes"
                            for item in state.requests
                        )
                    )
                else:
                    marker = (
                        b'name="file"'
                        if platform == "gitee"
                        else b'name="attachment"'
                    )
                    self.assertTrue(any(marker in item["body"] for item in state.requests))

    def test_token_is_redacted_from_http_errors(self) -> None:
        class ErrorHandler(BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_GET(self) -> None:
                body = b'{"message":"secret-release-token"}'
                self.send_response(500)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(("127.0.0.1", 0), ErrorHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            host, port = server.server_address
            client = Client(
                base=f"http://{host}:{port}",
                token="secret-release-token",
                platform="gitee",
            )
            with self.assertRaisesRegex(ReleaseError, r"\[REDACTED\]") as raised:
                client.request("GET", "/failure")
            self.assertNotIn("secret-release-token", str(raised.exception))
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    def test_github_upload_url_cannot_exfiltrate_the_token(self) -> None:
        client = Client(
            base="https://api.github.com", token="secret", platform="github"
        )
        self.assertEqual(
            validate_upload_url(
                client, "https://uploads.github.com/repos/owner/repo/releases/1/assets"
            ),
            "https://uploads.github.com/repos/owner/repo/releases/1/assets",
        )
        with self.assertRaisesRegex(ReleaseError, "untrusted origin"):
            validate_upload_url(client, "https://attacker.invalid/steal")
        with self.assertRaisesRegex(ReleaseError, "credentials"):
            validate_upload_url(client, "https://user:password@api.github.com/upload")

    def test_dry_run_does_not_need_a_token_or_network(self) -> None:
        from release_version import workspace_version

        tag = f"v{workspace_version(REPO_ROOT / 'Cargo.toml')}"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dist = root / "dist"
            dist.mkdir()
            (root / "notes.md").write_text("release notes\n", encoding="utf-8")
            (dist / "asset.bin").write_bytes(b"asset")
            (dist / "SHA256SUMS").write_text("checksum\n", encoding="utf-8")
            (dist / "release-manifest.json").write_text(
                json.dumps(
                    {
                        "tag": tag,
                        "source_commit": "abc123",
                        "assets": [{"name": "asset.bin"}],
                    }
                ),
                encoding="utf-8",
            )
            env = os.environ.copy()
            env.pop("MISSING_RELEASE_TOKEN", None)
            result = subprocess.run(
                [
                    sys.executable,
                    "release/release_platform.py",
                    "--platform",
                    "gitee",
                    "--api-base",
                    "http://127.0.0.1:1",
                    "--owner",
                    "owner",
                    "--repo",
                    "repo",
                    "--token-env",
                    "MISSING_RELEASE_TOKEN",
                    "--tag",
                    tag,
                    "--commit",
                    "abc123",
                    "--title",
                    "release",
                    "--notes",
                    str(root / "notes.md"),
                    "--dist",
                    str(dist),
                    "--dry-run",
                ],
                cwd=REPO_ROOT,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                json.loads(result.stdout)["assets"],
                ["asset.bin", "SHA256SUMS", "release-manifest.json"],
            )


if __name__ == "__main__":
    unittest.main()
