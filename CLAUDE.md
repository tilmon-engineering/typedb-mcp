# typedb-mcp

Last verified: 2026-05-22

A safety-focused MCP server, written in Rust, that exposes a TypeDB 3.x
database to an LLM agent through a connection-bound transaction model.

## Source of truth

**`DESIGN.md`** is the design contract for this project. Read it before
making any change that touches the tool surface, the transaction state
machine, the error semantics, or the response envelope. If a code change
contradicts `DESIGN.md`, update `DESIGN.md` first or push back on the
change.

The empirically verified TypeDB behaviour in `DESIGN.md` §5 (which error
classes poison a transaction and which do not) was probed directly against
TypeDB CE 3.10.4 (HTTP) and 3.11.1 (gRPC). Treat that table as authoritative
over anything you remember about TypeDB error handling.

## Tech stack

- Language: Rust (stable), async on Tokio
- MCP library: `rmcp` (official Anthropic Rust SDK), stdio + Streamable HTTP
- TypeDB driver: `typedb-driver` crate (official, gRPC). Requires TypeDB
  exposing its gRPC port (default `1729`)

## Conventions

- TypeQL examples in code and docs use TypeDB **3.x** syntax. 2.x patterns
  (`get`, `sub entity`, `regex`, `abstract` without `@`) are wrong.
- Tool descriptions are agent-facing prose. They must teach the lifecycle,
  not just describe parameters.
- Error messages are lifecycle-aware: every error tells the agent whether
  the transaction is still alive. See `DESIGN.md` §5.

## Design principle: make the state machine easy to follow

The agent is required to navigate a small state machine (schema-read →
open → query → commit/rollback). It is also stateless between tool calls
beyond what we tell it. The reconciliation: **every tool response must
re-teach the immediate horizon** — what moves are valid right now, given
this session's state.

Concretely, every response envelope carries a `next_moves` field — a short
list of one-line usage reminders. Implementations should keep them short,
concrete, and named (with literal tool names). Examples:

- `list_databases` →
  *"Call `get_schema(database=<name>)` for any database before querying it."*
- `get_schema` →
  *"Open a transaction: `open_read(db)` for reads, `open_write(db)` for
  mutations, `open_schema(db)` for schema changes. Or `read_once(db, query)`
  for a one-shot read."*
- `open_read` / `open_write` / `open_schema` →
  *"Submit a query with `query(typeql)`. End with `commit` (write/schema
  only) or `rollback`."*
- `query` (success) →
  *"Continue with more `query` calls, or close with `commit` (write/schema
  only) or `rollback`."*
- `commit` / `rollback` →
  *"Open another transaction with `open_*`, or call `get_schema` for a
  different database."*

When state forks (e.g. `query` returned a recoverable error vs. fatal
error), `next_moves` should differ accordingly. The agent should never
need to consult `DESIGN.md` mid-session to know what call to make next.

This is a design contract, not a convenience: changing it changes how
the agent reasons about the server. If you find yourself omitting
`next_moves`, surface that and update this file rather than letting it
drift.

## Boundaries

- Safe to edit: `src/`, `DESIGN.md`, `CLAUDE.md`, `Cargo.toml`.
- The nine tools enumerated in `DESIGN.md` §7 are the entire agent-facing
  surface. Adding a tenth tool is a design change, not an implementation
  change — update `DESIGN.md` first.

## Running and testing

- `.mcp.json` is gitignored (each developer constructs their own).
  `config.local.toml` is the committed per-developer wiring template that
  `.mcp.json` points at via `TYPEDB_MCP_CONFIG`.
- Tests live in `src/error.rs::tests` (classifier units), `tests/smoke_local.rs`
  + `tests/smoke_integration.rs` (driver-level, gated on `TYPEDB_MCP_SMOKE=1`
  with a live TypeDB at `127.0.0.1:1729`), and `tests/in_process.rs`
  (in-process MCP client↔server over a tokio duplex; also gated on
  `TYPEDB_MCP_SMOKE=1`).
- Run gated tests with `TYPEDB_MCP_SMOKE=1 cargo test`.

## What is deliberately not here

- `write_once` / `schema_once` convenience tools (would break the safety
  thesis).
- Server-initiated notifications to the agent (clients route them to logs,
  not to the model's context).
- Idle warnings in the session block (no actionable value; see
  `DESIGN.md` §6).
