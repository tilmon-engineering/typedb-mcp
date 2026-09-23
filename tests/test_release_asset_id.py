from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts" / "release_asset_id.py"


class ReleaseAssetIdTests(unittest.TestCase):
    def run_selector(self, metadata: dict, name: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text(json.dumps(metadata), encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(SCRIPT), str(path), name],
                capture_output=True,
                text=True,
                check=False,
            )

    def test_returns_unique_positive_integer_asset_id(self) -> None:
        result = self.run_selector({"assets": [{"id": 123, "name": "SHA256SUMS"}]}, "SHA256SUMS")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "123")

    def test_rejects_missing_or_duplicate_names(self) -> None:
        for assets in ([], [{"id": 1, "name": "a"}, {"id": 2, "name": "a"}]):
            with self.subTest(assets=assets):
                result = self.run_selector({"assets": assets}, "a")
                self.assertNotEqual(result.returncode, 0)

    def test_rejects_invalid_asset_ids_and_non_object_entries(self) -> None:
        for asset in ({"id": True, "name": "a"}, {"id": 0, "name": "a"}, [], {"id": "1", "name": "a"}):
            with self.subTest(asset=asset):
                result = self.run_selector({"assets": [asset]}, "a")
                self.assertNotEqual(result.returncode, 0)

    def test_rejects_missing_asset_list(self) -> None:
        result = self.run_selector({}, "a")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
