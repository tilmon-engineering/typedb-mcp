# Release process

Last verified: 2026-06-12 (cutting 0.1.3)

This repo releases a container image, not a crate. The release artifact
is `ghcr.io/tilmon-engineering/typedb-mcp`, published by
`.github/workflows/docker.yml`. There is no crates.io publish and no
GitHub Release object — an annotated git tag plus the GHCR image *is*
the release.

## How the pipeline works

`docker.yml` triggers on two event shapes, and `docker/metadata-action`
derives image tags from each:

| Event | Image tags produced |
|---|---|
| push to `main` | `latest`, `main`, `sha-<short>` |
| push of tag `v*` | `<version>` (e.g. `0.1.3`), `<major>.<minor>` (e.g. `0.1`), `sha-<short>` |
| pull request | build only, no push |

So every merge to `main` already refreshes `:latest`; the version tag
exists to pin a semver-addressable image. CI runs **no tests** — it only
builds and pushes the image. The gated test suite is a local,
pre-release responsibility (see step 1).

## Versioning

- Single source of truth: `[workspace.package] version` in the root
  `Cargo.toml`. All three crates inherit it via `version.workspace = true`.
- SemVer, with the contract surface defined in `CHANGELOG.md`'s scope
  notes: the ten agent-facing tools (`DESIGN.md` §7), the response
  envelope, the transaction state machine, and the `typedb-mcp-core`
  public re-exports (`DESIGN.md` §11). Breaking any of those is at
  minimum a minor bump pre-1.0 (and a `DESIGN.md` change first — see
  `CLAUDE.md` Boundaries). Fixes and compatible additions are a patch.

## Cutting a release

1. **Green tests, locally.** CI does not run tests, so this gate is on
   you. From the workspace root, with a live TypeDB 3.11+ on
   `127.0.0.1:1729` (credentials `admin`/`password`):

   ```bash
   TYPEDB_MCP_SMOKE=1 cargo test
   ```

   All units, the in-process MCP suite, and the driver-level smoke
   tests must pass. (Note: the local server must be reachable by the
   rust driver — TypeDB CE 3.11.5+ standalone servers need
   typedb-driver ≥ 3.11.5; see CHANGELOG 0.1.3.)

2. **Update `CHANGELOG.md`.** Move the `[Unreleased]` content into a new
   `## [X.Y.Z] — YYYY-MM-DD` section, leaving an empty `[Unreleased]`
   heading behind. Every contract change since the last release must be
   represented (scope notes at the top of the file).

3. **Bump the version.** Edit `version` under `[workspace.package]` in
   the root `Cargo.toml`, then run `cargo build` (or `cargo update -w`)
   so `Cargo.lock` picks up the new workspace version. Commit the
   `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` changes together:

   ```bash
   git commit -m "Cut X.Y.Z"
   ```

   (0.1.2's lockfile refresh landed as a separate commit after the
   bump; including it in the release commit is the corrected practice
   so the tag points at a commit whose lockfile matches its version.)

4. **Tag and push.** Annotated tag, name `vX.Y.Z`, message `vX.Y.Z`:

   ```bash
   git push origin main
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```

   The main push builds `:latest`; the tag push builds `:X.Y.Z` and
   `:X.Y`. Watch both:

   ```bash
   gh run list --repo tilmon-engineering/typedb-mcp --limit 2
   ```

5. **Roll edge-01.** The deployment pulls `:latest` with
   `imagePullPolicy: Always`; follow the procedure in `CLAUDE.md`
   ("Deploying a new image to edge-01"): rollout restart, wait for
   status, **confirm the image digest changed** (the only authoritative
   check), then tail the logs for the Streamable HTTP listening line.

6. **Live smoke.** Exercise the changed surface end-to-end against the
   live MCP (e.g. via the `typedb-*` tools through LiteLLM): at minimum
   `start_session` → `get_schema` → one read of the affected tool(s).
   For any task tracked in the OST graph, this is part of the K_* DoD —
   don't mark it `done` before this passes.

## What a release is not

- No GitHub Release object is created (deliberate; revisit if external
  consumers appear).
- No crates.io publish — `typedb-mcp-core`'s library API is consumed
  via git/path dependencies for now. Publishing would make the
  `DESIGN.md` §11 stability guarantees externally binding; treat that
  as a separate decision.
