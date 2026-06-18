# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The release process is documented in `RELEASE.md`.

Scope notes:

- Changes to the ten agent-facing tools (`DESIGN.md` §7), the response
  envelope, the transaction state machine, or the `typedb-mcp-core`
  public re-exports (`DESIGN.md` §11) are **contract changes** and must
  appear here.
- Internal refactors that do not move those contracts may be summarized
  briefly or omitted.

## [Unreleased]

## [0.2.0] — 2026-06-18

### Added

- GHCR images are now built and published as multi-arch manifests for
  `linux/amd64` and `linux/arm64`, making the container image directly usable
  on Apple Silicon Docker Desktop and ARM64 Linux hosts.

### Fixed

- Docker multi-arch builds now use per-architecture Cargo target,
  registry, and git caches, avoiding concurrent unpack races and cache
  contention between the amd64 and arm64 Buildx workers.

### Docs

- `RELEASE.md` now defines the project's major/minor/patch bump rules and
  requires `main` to prove every intended image platform before cutting a
  version tag.

## [0.1.4] — 2026-06-18

### Added

- Optional database-admin raw tools, `create_database` and `delete_database`,
  gated behind `server.enable_database_admin_tools` and omitted from the
  default ten-tool reference surface. `delete_database` requires explicit
  `confirm_database == database` confirmation and rejects while live sessions
  hold transactions on the target database.
- Exact MCP tool-surface regression tests for both the default ten-tool
  surface and the admin-enabled surface.
- Gated in-process regressions for admin-tool safety gates, all schema-read
  gate entry points, schema-gate clearing after schema commits, expired read
  transaction cleanup, and continued read-transaction usability after
  `RESULT_LIMIT_EXCEEDED`.

### Fixed

- Expired sessions now release any open transaction through the kind-aware
  `OpenTx::release()` helper. This preserves the read-transaction `close()`
  rule from `read_once` and avoids sending TypeDB an invalid rollback for a
  READ transaction during expiry cleanup.
- `list_databases` tests now assert the bundled TypeQL language reference on
  `start_session`, matching the tool contract that `list_databases` returns
  only the database list.
- Raw tool parameter schemas now include field-level descriptions for
  session, database, and query parameters.

### Docs

- `DESIGN.md`, `README.md`, `AGENTS.md`, and config templates now describe
  the default ten-tool surface plus separately gated database-admin tools.

## [0.1.3] — 2026-06-12

### Fixed

- `read_once` now drains the answer stream **before** closing its
  managed transaction. The previous order (query → close → drain)
  aborted on any result set larger than the driver's prefetch batch —
  server-side `TSV13` under driver 3.11.1, client-side `CXN07` under
  3.11.5 — which made `read_once` fail deterministically against real
  databases while the one-row results in the test suite passed. Gated
  regression test (`mcp_read_once_returns_full_multibatch_result`,
  400 rows) pins the corrected order. `DESIGN.md` §5.0.1 now states
  the drain-before-release rule.

### Changed

- `typedb-driver` bumped 3.11.1 → 3.11.5. TypeDB CE 3.11.5 standalone
  servers report their replica without an advertised address, which
  driver 3.11.1 filters out as unavailable and then fails the whole
  connection with `CXN02`. Driver 3.11.5 falls back to the connection
  address. Remains compatible with 3.11.1 servers.

### Project / docs

- `RELEASE.md` added, documenting the tag-driven GHCR release process.

## [0.1.2] — 2026-05-24

### Fixed

- Dockerfile updated for the workspace layout (`COPY crates ./crates`,
  `cargo build -p typedb-mcp`). The previous `COPY src ./src` /
  `--bin typedb-mcp` form had been broken since the workspace split
  (`1173591`), so no GHCR image had been produced for `0.1.0` or
  `0.1.1`. Verified with a local `podman build` before release.

## [0.1.1] — 2026-05-24

### Changed

- `next_moves` hints for `open_read` / `open_write` / `open_schema` and
  the corresponding `query`-success responses now explicitly state which
  TypeQL statement kinds each transaction kind accepts. The schema-tx
  hint previously enumerated only `define`/`undefine`/`redefine`, which
  read as exhaustive and led agents to commit-and-reopen before running
  reads. The hint now states that schema transactions accept schema
  statements, data writes, AND reads in any order, and that write
  transactions accept reads alongside writes. Hint-only change; the
  query path already permitted these statement combinations.

## [0.1.0] — 2026-05-24

### Added

- Public library extension API exposed from `typedb-mcp-core`:
  `TypeDbCore`, `HasTypeDbCore`, `SessionHandle` (with `with_read_tx` /
  `with_write_tx` / `with_schema_tx` / `with_current_tx`), `TxOutcome`,
  per-session `Extensions` typemap, generic `tools::raw_tools_router::<H>`
  with `with_prefix` / `without` knobs, and the full `envelope` module
  (`AgentEnvelope`, `ErrorPayload`, `NextMoves`, `ENVELOPE_VERSION = 1`,
  `next_moves` catalogue, `envelope_state_error[_no_session]`,
  `extract_codes`, `explain_query_error`). See `DESIGN.md` §11.
- `crates/example-semantic-mcp/` — worked consumer of the extension API.
- Regression test ensuring `read_once` does not emit TSV3 in tracing logs.
- `server.allowed_hosts` config for the Streamable HTTP transport.
- HTTP request and `SessionStore` lifecycle logging.
- `LICENSE-MIT` and `LICENSE-APACHE` files matching the dual-license
  declaration in `Cargo.toml`.
- Dockerfile and GitHub Actions workflow that publishes
  `ghcr.io/tilmon-engineering/typedb-mcp:latest` on push to `main`.
- README, including the "agent affordances are user affordances" section
  and the attention-stacking framing.
- In-process MCP integration tests over a tokio duplex (gated on
  `TYPEDB_MCP_SMOKE=1`).
- `config.local.toml` template for local stdio development.

### Changed

- Repo split into a Cargo workspace with three members:
  `crates/typedb-mcp-core/` (library kernel), `crates/typedb-mcp/`
  (binary), `crates/example-semantic-mcp/`. `handler.rs` is now an
  ~82-line thin wrapper that dogfoods the public library API.
- Server-issued session IDs are now the root of the state machine;
  every tool except `start_session` requires `session_id`. README and
  `DESIGN.md` §6 brought into line with this contract.
- Read transactions are released via `Transaction::close()` rather than
  `Rollback`. Idle timeouts are now split per transaction kind.
- Per-session transaction work is serialized to close a self-induced
  TSV13 race.
- Error classification tightened; TSV13 is now classified explicitly and
  aborted-write loss is surfaced to the agent rather than silently
  swallowed (`K_00000052`).
- Swallowed rollback errors inside `read_once` are now logged
  (`K_00000052` follow-up).
- Documented TypeDB **3.11+** as the minimum supported server version;
  deployment against pre-3.11 fails at the driver layer.

### Fixed

- TSV13 race produced by concurrent tool calls against the same session.
- Read-transaction lifecycle no longer triggers spurious rollback log
  noise (`read_once` close path).

### Project / docs

- `CLAUDE.md` updated to record OST positioning (`O_0001` / `S_0004` /
  `S_0005` / `T_0001`) and the edge-01 rollout procedure.
- `DESIGN.md` §5 table of TypeDB error classes verified empirically
  against TypeDB CE 3.10.4 (HTTP) and 3.11.1 (gRPC).

## 0.0.0 — 2026-05-22 (pre-history)

Initial scaffold of the safety-gated TypeDB MCP server, Streamable HTTP
transport wiring, and the first pass of the lifecycle-aware error
contract. Not a release; recorded here for narrative continuity.
