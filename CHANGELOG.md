# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it cuts a first release. Until then, every change lands under
`[Unreleased]` and the workspace version remains `0.1.0`.

Scope notes:

- Changes to the ten agent-facing tools (`DESIGN.md` §7), the response
  envelope, the transaction state machine, or the `typedb-mcp-core`
  public re-exports (`DESIGN.md` §11) are **contract changes** and must
  appear here.
- Internal refactors that do not move those contracts may be summarized
  briefly or omitted.

## [Unreleased]

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
