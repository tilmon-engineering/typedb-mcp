"""Regression tests for the release artifact contract verifier."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import tarfile
import tempfile
import unittest
from io import BytesIO
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts" / "verify_release_assets.py"
SPEC = importlib.util.spec_from_file_location("verify_release_assets", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VERIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFY)


class ReleaseAssetsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.directory = Path(self.temp.name) / "assets"
        self.directory.mkdir()
        self.archive_bytes: dict[str, bytes] = {}
        for filename in VERIFY.ARCHIVES:
            archive_path = self.directory / filename
            with tarfile.open(archive_path, mode="w:gz") as archive:
                payload = BytesIO(b"typedb-mcp test executable")
                member = tarfile.TarInfo("typedb-mcp")
                member.size = len(payload.getvalue())
                member.mode = 0o755
                archive.addfile(member, payload)
            self.archive_bytes[filename] = archive_path.read_bytes()
        self.write_manifest()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_manifest(self) -> None:
        lines = [
            f"{hashlib.sha256((self.directory / filename).read_bytes()).hexdigest()}  {filename}"
            for filename in VERIFY.ARCHIVES
        ]
        (self.directory / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")

    def test_accepts_exact_archives_and_checksums(self) -> None:
        VERIFY.verify_assets(self.directory)

    def test_rejects_unexpected_asset(self) -> None:
        (self.directory / "unexpected.txt").write_text("extra", encoding="utf-8")
        with self.assertRaisesRegex(VERIFY.VerificationError, "exactly the five"):
            VERIFY.verify_assets(self.directory)

    def test_rejects_checksum_mismatch(self) -> None:
        archive_path = self.directory / VERIFY.ARCHIVES[0]
        archive_path.write_bytes(archive_path.read_bytes() + b"tampered")
        with self.assertRaisesRegex(VERIFY.VerificationError, "SHA-256 mismatch"):
            VERIFY.verify_assets(self.directory)

    def test_rejects_archive_with_extra_member(self) -> None:
        archive_path = self.directory / VERIFY.ARCHIVES[0]
        with tarfile.open(archive_path, mode="w:gz") as archive:
            for name in ("typedb-mcp", "extra"):
                payload = BytesIO(b"content")
                member = tarfile.TarInfo(name)
                member.size = len(payload.getvalue())
                member.mode = 0o755
                archive.addfile(member, payload)
        self.write_manifest()
        with self.assertRaisesRegex(VERIFY.VerificationError, "only one root-level"):
            VERIFY.verify_assets(self.directory)

    def test_accepts_only_exact_stable_published_metadata(self) -> None:
        metadata = {
            "tag_name": "v1.2.3",
            "draft": False,
            "prerelease": False,
            "published_at": "2026-09-23T00:00:00Z",
            "assets": [{"name": name, "state": "uploaded"} for name in sorted(VERIFY.ASSETS)],
        }
        path = Path(self.temp.name) / "release.json"
        path.write_text(json.dumps(metadata), encoding="utf-8")
        VERIFY.verify_release_metadata(path, "v1.2.3", expect_published=True)

    def test_rejects_prerelease_or_wrong_tag_metadata(self) -> None:
        metadata = {
            "tag_name": "v1.2.3-rc.1",
            "draft": False,
            "prerelease": True,
            "published_at": "2026-09-23T00:00:00Z",
            "assets": [{"name": name, "state": "uploaded"} for name in sorted(VERIFY.ASSETS)],
        }
        path = Path(self.temp.name) / "release.json"
        path.write_text(json.dumps(metadata), encoding="utf-8")
        with self.assertRaisesRegex(VERIFY.VerificationError, "stable vX.Y.Z"):
            VERIFY.verify_release_metadata(path, "v1.2.3-rc.1", expect_published=True)
        with self.assertRaisesRegex(VERIFY.VerificationError, "does not match"):
            VERIFY.verify_release_metadata(path, "v1.2.3", expect_published=True)

    def test_rejects_draft_when_published_is_required(self) -> None:
        metadata = {
            "tag_name": "v1.2.3",
            "draft": True,
            "prerelease": False,
            "published_at": None,
            "assets": [{"name": name, "state": "uploaded"} for name in sorted(VERIFY.ASSETS)],
        }
        path = Path(self.temp.name) / "release.json"
        path.write_text(json.dumps(metadata), encoding="utf-8")
        with self.assertRaisesRegex(VERIFY.VerificationError, "must be a published"):
            VERIFY.verify_release_metadata(path, "v1.2.3", expect_published=True)


if __name__ == "__main__":
    unittest.main()
