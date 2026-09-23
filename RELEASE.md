# Release process

This repository publishes a multi-architecture container image and four native `typedb-mcp` release archives; it is not a crates.io package. Version/tag changes and deployment are authorized operational actions; do not perform them implicitly.

## Native binary release

`.github/workflows/release.yml` runs only for a pushed `v*` tag. It requires a stable `vX.Y.Z` tag, builds `typedb-mcp` natively on Ubuntu 24.04 x86_64/ARM64 and macOS 15 ARM64/Intel runners, packages one executable named `typedb-mcp` at the root of each gzip tar archive, and creates a draft GitHub Release containing exactly those four archives plus `SHA256SUMS`.

Before publishing, `verify-draft` downloads the uploaded draft assets and checks exact release asset names, archive members and executable mode, SHA-256 values, checksum formatting, tag, and draft metadata. Only a successful verification allows the draft to be published. `verify-published` then downloads the public release bytes and repeats archive/checksum verification; `verify-release-metadata` independently checks the stable tag, published/non-prerelease state, and exact asset list. A failed verification stops publication or leaves the already-published release visible with a failed Actions run; it does not delete a release.

Run the verifier's local regression tests with:

```bash
python3 -m unittest discover -s tests -v
```

The release workflow does not notify or modify the Homebrew tap. `typedb-mcp` must first be added to the tap's reviewed `tap-projects.json` inventory and have a cask reviewed by tap maintainers. Only after that enrollment, a published release, and configuration of the source repository's narrowly scoped `HOMEBREW_TOOLS_DISPATCH_TOKEN` secret should a separate notification job be added. That job must depend on `publish-release`, `verify-published`, and `verify-release-metadata`; do not dispatch for an unapproved or unverified release.

## Reproducible local release gate

Run from the workspace root:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
TYPEDB_MCP_SMOKE=1 bash scripts/compatibility_matrix.sh
```

The compatibility runner starts disposable pinned TypeDB CE 3.12.0 and 3.13.0 containers by default (or versions supplied as arguments), uses dynamically allocated loopback ports, checks authenticated readiness, runs workspace tests, and removes only its own resources. It requires podman or docker and fails with an actionable message if neither is available. Never point migration or matrix runs at production databases.

The checked-in driver is exactly 3.12.3. TypeDB 3.12.0 is the verified floor; TypeDB 3.13.0 is accepted as `unverified` unless the current matrix evidence says otherwise. Do not claim cluster failover, crash atomicity, or backup correctness from this gate.

## Authorized image/deployment workflow only

Only after explicit authorization may an operator publish a version/tag or deploy. The repository’s authorized edge-01 workflow is:

1. Publish through the repository CI workflow; do not retag or mutate production images manually.
2. Restart the edge-01 `typedb-mcp` Deployment through the approved Kubernetes workflow.
3. Wait for rollout completion.
4. Verify the running pod’s container image **digest changed**; rollout success alone is not authoritative.
5. Review startup logs.
6. Run a live Streamable HTTP smoke against `/mcp`: initialize, `start_session`, `get_schema`, and a read-only transaction flow as appropriate. The smoke must explicitly exclude `export_database` and `import_database` and must not test migration against an existing production database.

The exact cluster commands and access prerequisites belong to the authorized operator runbook; this document does not assert that deployment verification has happened. **Deployment verification remains recorded-outstanding until someone performs and records the digest check and live HTTP smoke.**

Do not change the workspace version, create a release tag, push a tag, or announce a release unless explicitly authorized. When authorized, update `CHANGELOG.md`, bump the single workspace version consistently, run the full gate, and tag only the reviewed commit.
