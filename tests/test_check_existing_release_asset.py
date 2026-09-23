from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from scripts.check_existing_release_asset import existing_asset_matches


class ExistingReleaseAssetTests(unittest.TestCase):
    def test_missing_asset_is_uploadable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata = root / "assets.json"
            candidate = root / "candidate.tar.gz"
            metadata.write_text("[]", encoding="utf-8")
            candidate.write_bytes(b"candidate")
            self.assertFalse(existing_asset_matches(metadata, "asset.tar.gz", candidate))

    def test_matching_digest_is_resumable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata = root / "assets.json"
            candidate = root / "candidate.tar.gz"
            candidate.write_bytes(b"candidate")
            digest = hashlib.sha256(candidate.read_bytes()).hexdigest()
            metadata.write_text(json.dumps([{"id": 7, "name": "asset.tar.gz", "digest": f"sha256:{digest}"}]), encoding="utf-8")
            self.assertTrue(existing_asset_matches(metadata, "asset.tar.gz", candidate))

    def test_mismatch_and_duplicate_names_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata = root / "assets.json"
            candidate = root / "candidate.tar.gz"
            candidate.write_bytes(b"candidate")
            metadata.write_text(json.dumps([{"id": 7, "name": "asset.tar.gz", "digest": "sha256:bad"}]), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "differs from candidate"):
                existing_asset_matches(metadata, "asset.tar.gz", candidate)
            metadata.write_text(json.dumps([{"id": 7, "name": "asset.tar.gz"}, {"id": 8, "name": "asset.tar.gz"}]), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate"):
                existing_asset_matches(metadata, "asset.tar.gz", candidate)


if __name__ == "__main__":
    unittest.main()
