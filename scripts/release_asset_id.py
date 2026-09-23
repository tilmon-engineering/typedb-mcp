#!/usr/bin/env python3
"""Select exactly one GitHub release asset ID by its exact name."""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: release_asset_id.py RELEASE_JSON ASSET_NAME", file=sys.stderr)
        return 2
    metadata_path = Path(sys.argv[1])
    expected_name = sys.argv[2]
    try:
        release = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"cannot read release metadata: {error}", file=sys.stderr)
        return 1
    assets = release.get("assets")
    if not isinstance(assets, list):
        print("release metadata has no assets list", file=sys.stderr)
        return 1
    matches = [asset for asset in assets if isinstance(asset, dict) and asset.get("name") == expected_name]
    if len(matches) != 1:
        print(f"expected exactly one asset named {expected_name!r}; found {len(matches)}", file=sys.stderr)
        return 1
    asset_id = matches[0].get("id")
    if not isinstance(asset_id, int) or isinstance(asset_id, bool) or asset_id <= 0:
        print(f"asset {expected_name!r} has no valid positive integer ID", file=sys.stderr)
        return 1
    print(asset_id)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
