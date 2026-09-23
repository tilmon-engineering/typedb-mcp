#!/usr/bin/env python3
"""Check whether a draft asset can be safely reused for a release retry."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path


def existing_asset_matches(metadata_path: Path, name: str, candidate_path: Path) -> bool:
    try:
        assets = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read release asset metadata: {error}") from error
    if not isinstance(assets, list):
        raise ValueError("release asset metadata must be a JSON array")

    matches = [asset for asset in assets if isinstance(asset, dict) and asset.get("name") == name]
    if len(matches) > 1:
        raise ValueError(f"duplicate draft assets named {name}")
    if not matches:
        return False

    try:
        digest = hashlib.sha256(candidate_path.read_bytes()).hexdigest()
    except OSError as error:
        raise ValueError(f"cannot read candidate asset {candidate_path}: {error}") from error
    if matches[0].get("digest") != f"sha256:{digest}":
        raise ValueError(f"existing draft asset {name} differs from candidate bytes")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("name")
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()
    try:
        print("yes" if existing_asset_matches(args.metadata, args.name, args.candidate) else "no")
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
