from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_draft_metadata_path_is_outside_exact_five_asset_directory(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        create_job = workflow.split("  create-draft:\n", 1)[1].split("  publish-release:\n", 1)[0]
        metadata_match = re.search(r'gh api "\$\{api\}/\$\{release_id\}" > ([^\s]+)', create_job)
        self.assertIsNotNone(metadata_match, "create job must save metadata fetched by release ID")
        metadata_path = metadata_match.group(1)
        self.assertEqual(metadata_path, "draft-release.json")
        asset_helper_paths = re.findall(
            r"scripts/release_asset_id\.py\s+([^\s]+)", create_job
        )
        self.assertTrue(asset_helper_paths, "workflow must select uploaded assets by ID")
        self.assertTrue(all(path == metadata_path for path in asset_helper_paths))
        self.assertNotEqual(metadata_path, "draft-download/release.json")

    def test_asset_verifier_receives_only_download_directory(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        create_job = workflow.split("  create-draft:\n", 1)[1].split("  publish-release:\n", 1)[0]
        self.assertIn("python3 scripts/verify_release_assets.py --assets-dir draft-download", create_job)
        self.assertIn("gh api \"${api}/${release_id}\" > draft-release.json", create_job)

    def test_homebrew_notification_waits_for_all_release_verification(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        notify_job = workflow.split("  notify-homebrew-tap:\n", 1)[1]
        self.assertIn("needs: [publish-release, verify-published, verify-release-metadata]", notify_job)
        self.assertIn("if: ${{ success() }}", notify_job)
        self.assertIn("DISPATCH_TOKEN: ${{ secrets.HOMEBREW_TOOLS_DISPATCH_TOKEN }}", notify_job)
        self.assertIn('[[ "${GITHUB_REPOSITORY}" == "tilmon-engineering/typedb-mcp" ]]', notify_job)
        self.assertIn(
            r'[[ "${RELEASE_TAG}" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]',
            notify_job,
        )
        self.assertIn("curl --fail-with-body --silent --show-error", notify_job)
        self.assertIn("-X POST", notify_job)
        self.assertIn("https://api.github.com/repos/tilmon-engineering/homebrew-tools/dispatches", notify_job)
        self.assertIn(
            r'-d "{\"event_type\":\"homebrew-tools-release\",\"client_payload\":{\"repository\":\"${GITHUB_REPOSITORY}\",\"tag\":\"${RELEASE_TAG}\"}}"',
            notify_job,
        )
        self.assertNotIn("GITHUB_TOKEN", notify_job)


if __name__ == "__main__":
    unittest.main()
