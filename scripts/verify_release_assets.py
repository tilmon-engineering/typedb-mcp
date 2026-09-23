#!/usr/bin/env python3
"""Verify the typedb-mcp GitHub release asset contract."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)
ARCHIVES = tuple(f"typedb-mcp-{target}.tar.gz" for target in TARGETS)
ASSETS = frozenset((*ARCHIVES, "SHA256SUMS"))
TAG_PATTERN = re.compile(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
CHECKSUM_PATTERN = re.compile(r"([0-9a-fA-F]{64})  ([A-Za-z0-9_.-]+)\Z")


class VerificationError(ValueError):
    """An artifact or release does not satisfy the supported contract."""


def _read_checksum_manifest(path: Path) -> dict[str, str]:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise VerificationError(f"cannot read SHA256SUMS: {error}") from error

    lines = text.splitlines()
    if len(lines) != len(ARCHIVES):
        raise VerificationError(f"SHA256SUMS must contain exactly {len(ARCHIVES)} lines")

    checksums: dict[str, str] = {}
    for line in lines:
        match = CHECKSUM_PATTERN.fullmatch(line)
        if match is None:
            raise VerificationError("each SHA256SUMS line must be '<64 hex characters><two spaces><filename>'")
        digest, filename = match.groups()
        if filename not in ARCHIVES:
            raise VerificationError(f"unexpected checksum filename: {filename}")
        if filename in checksums:
            raise VerificationError(f"duplicate checksum entry: {filename}")
        checksums[filename] = digest.lower()

    if set(checksums) != set(ARCHIVES):
        raise VerificationError("SHA256SUMS must list each supported archive exactly once")
    return checksums


def verify_assets(directory: Path) -> None:
    if not directory.is_dir():
        raise VerificationError(f"asset directory does not exist: {directory}")

    entries = list(directory.iterdir())
    names = {entry.name for entry in entries}
    if names != ASSETS or len(entries) != len(ASSETS):
        missing = sorted(ASSETS - names)
        extra = sorted(names - ASSETS)
        raise VerificationError(f"asset directory must contain exactly the five release assets (missing={missing}, extra={extra})")

    for entry in entries:
        if entry.is_symlink() or not entry.is_file():
            raise VerificationError(f"release asset must be a regular file: {entry.name}")

    checksums = _read_checksum_manifest(directory / "SHA256SUMS")
    for filename in ARCHIVES:
        archive_path = directory / filename
        digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        if digest != checksums[filename]:
            raise VerificationError(f"SHA-256 mismatch for {filename}")

        try:
            with tarfile.open(archive_path, mode="r:gz") as archive:
                members = archive.getmembers()
        except (OSError, tarfile.TarError) as error:
            raise VerificationError(f"cannot read gzip tar archive {filename}: {error}") from error

        if len(members) != 1 or members[0].name != "typedb-mcp":
            raise VerificationError(f"{filename} must contain only one root-level file named typedb-mcp")
        member = members[0]
        if not member.isfile() or member.issym() or member.islnk():
            raise VerificationError(f"{filename}: typedb-mcp must be a regular file, not a link or special file")
        if member.mode & 0o111 == 0:
            raise VerificationError(f"{filename}: typedb-mcp is not executable")


def verify_release_metadata(path: Path, expected_tag: str, expect_published: bool) -> None:
    if not TAG_PATTERN.fullmatch(expected_tag):
        raise VerificationError(f"release tag is not a stable vX.Y.Z tag: {expected_tag}")
    try:
        release = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read release metadata JSON: {error}") from error

    if release.get("tag_name") != expected_tag:
        raise VerificationError("GitHub release tag does not match the requested tag")
    if release.get("prerelease") is not False:
        raise VerificationError("GitHub release must not be marked as a prerelease")
    if release.get("draft") is not (not expect_published):
        state = "published" if expect_published else "draft"
        raise VerificationError(f"GitHub release must be a {state}")
    if expect_published and not release.get("published_at"):
        raise VerificationError("published GitHub release is missing published_at")

    assets = release.get("assets")
    if not isinstance(assets, list):
        raise VerificationError("GitHub release metadata has no assets list")
    names = [asset.get("name") for asset in assets if isinstance(asset, dict)]
    if len(names) != len(assets) or len(names) != len(ASSETS) or set(names) != ASSETS:
        raise VerificationError("GitHub release must expose exactly the four archives and SHA256SUMS")
    for asset in assets:
        if asset.get("state") != "uploaded":
            raise VerificationError(f"release asset is not uploaded: {asset.get('name')}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets-dir", type=Path)
    parser.add_argument("--release-json", type=Path)
    parser.add_argument("--tag")
    parser.add_argument("--expect-published", action="store_true")
    args = parser.parse_args()

    try:
        if args.assets_dir is None and args.release_json is None:
            raise VerificationError("provide --assets-dir, --release-json, or both")
        if args.assets_dir is not None:
            verify_assets(args.assets_dir)
        if args.release_json is not None:
            if args.tag is None:
                raise VerificationError("--tag is required when --release-json is supplied")
            verify_release_metadata(args.release_json, args.tag, args.expect_published)
    except VerificationError as error:
        print(f"release verification failed: {error}", file=sys.stderr)
        return 1

    print("release assets verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
