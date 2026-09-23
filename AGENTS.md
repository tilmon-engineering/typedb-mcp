# typedb-mcp

This repository is a safety-focused TypeDB 3.12+ MCP server and reusable Rust library. `DESIGN.md` is the source of truth for tool behavior, transaction lifecycle, envelopes, and public API. Read it before changing those contracts.

## Workspace

- `crates/typedb-mcp-core/`: library kernel and raw tools
- `crates/typedb-mcp/`: reference binary
- `crates/example-semantic-mcp/`: library consumer example

The reference binary exposes twelve default tools: `start_session`, `list_databases`, `get_schema`, `open_read`, `open_write`, `open_schema`, `query`, `checkpoint`, `commit`, `rollback`, `read_once`, and `server_info`. `create_database`/`delete_database` are optional admin tools. `export_database`/`import_database` are optional, stdio-only migration tools. Keep the canonical `tools::names::ALL` fixture synchronized with the default surface; it is the authoritative names list for the twelve defaults.

Every tool except `start_session` requires a server-issued `session_id`; schema-read and explicit transaction gates must remain intact. Responses teach the next valid move through `next_moves`. TypeQL documentation and examples use TypeDB 3.x syntax.

## Verification

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
TYPEDB_MCP_SMOKE=1 bash scripts/compatibility_matrix.sh
```

The matrix runner requires podman or docker and disposable TypeDB resources. It fails clearly when prerequisites are missing. Do not use production databases for migration or compatibility testing. Do not claim cluster failover, crash atomicity, or deployment verification without direct evidence.

## Boundaries

Do not add tools, alter transport authority, or weaken migration path/confirmation/no-overwrite rules without first updating `DESIGN.md`. Migration is local stdio authority only; HTTP must not expose file operations. Never put credentials in committed examples, logs, responses, or diagnostics.
