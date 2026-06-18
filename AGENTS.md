# typedb-mcp

Last verified: 2026-05-24 (workspace split: code now lives under
`crates/typedb-mcp-core/` (library kernel), `crates/typedb-mcp/`
(binary), `crates/example-semantic-mcp/` (worked consumer);
`handler.rs` is now an 82-line thin wrapper dogfooding the public
library API; new public extension API — `TypeDbCore`, `HasTypeDbCore`,
`SessionHandle`, `TxOutcome`, `tools::raw_tools_router`, `Extensions`,
and the full `envelope` module — see `DESIGN.md` §11).

A safety-focused MCP server, written in Rust, that exposes a TypeDB
3.11+ database to an LLM agent through a connection-bound transaction
model. **TypeDB 3.10.x and earlier are not supported** — deployment
against pre-3.11 servers fails at the driver layer.

## Where this project sits in the OST graph

The OST (Outcome / Strategy / Tactic) graph lives in the `agents`
TypeDB database on edge-01. This project's coordinates:

- **Outcome `O_0001`** — *AI agents autonomously manage projects and
  sustain progress toward open-ended goals.*
- **Strategy `S_0004`** — *typedb-mcp: semantic database substrate for
  agents.* This repo IS that strategy.
- **Strategy `S_0005`** — *Minimized-indirection state-machine design
  as the agent-safety pattern.* The meta-strategy this server's design
  exemplifies. See README.md "Theory: agent affordances are user
  affordances" for the long-form articulation. Sibling strategies that
  apply the same pattern (`S_0006` Kubernetes MCP, `S_0007` Terraform
  MCP) are planned but not yet built.
- **Tactic `T_0001`** — *Deploy TypeDB StatefulSet, MCP gateway, and
  the OST schema on edge-01.* All tasks for this repo belong under
  this tactic.

When opening a new task in the OST graph for work in this repo, attach
it to `T_0001` via the `tactic-task` relation. Use the
`outcomes:using-ost-framework` skill for the falsifiability gates and
ID-allocation discipline.

## Source of truth

**`DESIGN.md`** is the design contract for this project. Read it before
making any change that touches the tool surface, the transaction state
machine, the error semantics, the response envelope, or the public
library API (§11). If a code change contradicts `DESIGN.md`, update
`DESIGN.md` first or push back on the change.

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

The agent is required to navigate a small state machine
(`start_session` → schema-read → open → query → commit/rollback). It is
also stateless between tool calls beyond what we tell it. The
reconciliation: **every tool response must re-teach the immediate
horizon** — what moves are valid right now, given this session's state.

Concretely, every response envelope carries a `next_moves` field — a short
list of one-line usage reminders. Implementations should keep them short,
concrete, and named (with literal tool names). Every tool except
`start_session` requires a `session_id` argument, and the hints reflect
that. Examples:

- `start_session` →
  *"Pass `session_id` to EVERY subsequent tool call. Use the returned
  `databases` list to pick one and call `get_schema(session_id=...,
  database=<name>)` before opening a transaction."*
- `list_databases` →
  *"Call `get_schema(session_id=..., database=<name>)` for any database
  before querying it."*
- `get_schema` →
  *"Open a transaction: `open_read(session_id=..., database=...)` for
  reads, `open_write(...)` for mutations, `open_schema(...)` for schema
  changes. Or `read_once(session_id=..., database=..., query=...)` for a
  one-shot read."*
- `open_read` / `open_write` / `open_schema` →
  *"Submit a query with `query(session_id=..., query=...)`. End with
  `commit(session_id=...)` (write/schema only) or
  `rollback(session_id=...)`."*
- `query` (success) →
  *"Continue with more `query` calls, or close with `commit` (write/schema
  only) or `rollback`."*
- `commit` / `rollback` →
  *"Open another transaction with `open_*`, or call `get_schema` for a
  different database."*
- Any tool, on `SESSION_UNKNOWN` / `SESSION_EXPIRED` →
  *"Call `start_session` to mint a fresh `session_id`, then reissue the
  call you just made with the new ID."*

When state forks (e.g. `query` returned a recoverable error vs. fatal
error), `next_moves` should differ accordingly. The agent should never
need to consult `DESIGN.md` mid-session to know what call to make next.

This is a design contract, not a convenience: changing it changes how
the agent reasons about the server. If you find yourself omitting
`next_moves`, surface that and update this file rather than letting it
drift.

## Workspace layout

The repo is a Cargo workspace (root `Cargo.toml` is `[workspace]` with
shared `[workspace.dependencies]`). Three members:

- `crates/typedb-mcp-core/` — the library kernel. Owns `config`,
  `core`, `envelope`, `error`, `extensions`, `handler`, `session`,
  `tools`, `typedb`. Public re-exports in `lib.rs`. The ten raw tools
  live in `tools::*` as free generic `fn`s over `H: HasTypeDbCore`
  (not closures — see "rmcp gotchas" below) and are assembled by
  `tools::raw_tools_router::<H>(RawToolsConfig)`. `handler.rs` is an
  82-line thin wrapper that mounts that router on `TypeDbMcp`, i.e.
  the binary dogfoods the public API.
- `crates/typedb-mcp/` — the binary. Just `main.rs`; behaviour
  unchanged from before the split.
- `crates/example-semantic-mcp/` — worked consumer demonstrating the
  library extension API.

## Library extension API

`typedb-mcp-core` is consumable as a library by other MCP servers that
want TypeDB safety semantics plus semantic tools of their own. See
`DESIGN.md` §11 for the authoritative surface and stability guarantees.
Headline types: `TypeDbCore` (kernel bundle: `connect`, `start_session`,
`resolve`, `spawn_reaper`); `HasTypeDbCore` trait (consumers implement
on their handler to plug into the generic raw router); `SessionHandle`
with `with_read_tx` / `with_write_tx` / `with_schema_tx` /
`with_current_tx` (closures are `AsyncFnOnce` so `&tx` lifetime
propagates), `extensions[_mut]`, `ok` / `err` / `state_err` envelope
helpers; `TxOutcome::{Commit, Rollback}` for write/schema closures;
`tools::raw_tools_router::<H>` with `with_prefix(...)` /
`without([...])` knobs; per-session typemap `Extensions`; and the full
`envelope` module (`AgentEnvelope`, `ErrorPayload`, `NextMoves`,
`ENVELOPE_VERSION = 1`, `next_moves` catalogue,
`envelope_state_error[_no_session]`, `extract_codes`,
`explain_query_error`).

## rmcp gotchas (worth not relearning)

- `ToolRouter<S>::merge` requires identical `S`, so the raw tool set
  is exposed as a generic `raw_tools_router::<H>` (Option B) rather
  than a concrete router that consumers merge into their own.
- Closures cannot satisfy rmcp 1.7's `CallToolHandler` HRTB
  (`for<'a> FnOnce(&'a S, P) -> Pin<Box<dyn Future + Send + 'a>>`).
  The rmcp macro sidesteps this by emitting free `fn` items;
  hand-written generic routes in `tools::*` do the same. Do not
  rewrite them as closures.

## Boundaries

- Safe to edit: `crates/typedb-mcp-core/src/`,
  `crates/typedb-mcp-core/reference/`, `crates/typedb-mcp/src/`,
  `crates/example-semantic-mcp/src/`, `DESIGN.md`, `AGENTS.md`, root
  and per-crate `Cargo.toml`.
- The ten tools enumerated in `DESIGN.md` §7.0-§7.9 (`start_session` plus
  the nine TypeDB-facing tools) are the default agent-facing surface of the
  reference binary. The only built-in extra tools are the optional database
  admin tools in §7.10-§7.11, absent unless explicitly enabled by operator
  config. Adding any other tool is a design change, not an implementation
  change — update `DESIGN.md` first. (Library consumers may of course add
  their own semantic tools alongside.)
- The public surface re-exported from `typedb-mcp-core`'s `lib.rs` is
  the library contract. Breaking changes there are a `DESIGN.md` §11
  change first; see "Versioning and stability" in §11.6.

## Running and testing

- `.mcp.json` is gitignored (each developer constructs their own).
  `config.local.toml` is the committed per-developer wiring template that
  `.mcp.json` points at via `TYPEDB_MCP_CONFIG`.
- Tests live in `crates/typedb-mcp-core/src/error.rs::tests` (classifier
  units), `crates/typedb-mcp-core/tests/smoke_local.rs` +
  `smoke_integration.rs` (driver-level, gated on `TYPEDB_MCP_SMOKE=1`
  with a live TypeDB at `127.0.0.1:1729`), and
  `crates/typedb-mcp-core/tests/in_process.rs` (in-process MCP
  client↔server over a tokio duplex; also gated on
  `TYPEDB_MCP_SMOKE=1`).
- Run gated tests with `TYPEDB_MCP_SMOKE=1 cargo test` (from the
  workspace root; cargo picks up all members).

## Deploying a new image to edge-01

CI publishes `ghcr.io/tilmon-engineering/typedb-mcp:latest` on every
push to `main` via `.github/workflows`. The edge-01 Deployment is
configured with `imagePullPolicy: Always`, so a pod restart is
sufficient to pick up the new image. Roll it yourself:

```bash
# 1. Trigger a rolling restart — kubelet re-pulls :latest on the new pod.
kubectl --context admin@edge-01 -n typedb rollout restart deployment/typedb-mcp

# 2. Wait for the new ReplicaSet to become ready.
kubectl --context admin@edge-01 -n typedb rollout status deployment/typedb-mcp

# 3. Confirm the digest actually changed — this is the only authoritative
#    check that new bits are running. A successful rollout with the SAME
#    digest just means `:latest` on GHCR still points at the old manifest
#    (the push hadn't propagated yet).
kubectl --context admin@edge-01 -n typedb \
  get pod -l app.kubernetes.io/name=typedb-mcp \
  -o jsonpath='{.items[0].status.containerStatuses[0].imageID}{"\n"}'

# 4. Sanity-check startup.
kubectl --context admin@edge-01 -n typedb logs \
  -l app.kubernetes.io/name=typedb-mcp --tail=20
# Expect: "Streamable HTTP transport listening at /mcp addr=0.0.0.0:8001"
# and an "allowed_hosts=[...]" line including
# typedb-mcp.edge-01.tilmonengineering.com.
```

Notes:

- If the digest in step 3 is unchanged, GHCR hadn't propagated `:latest`
  yet — wait a few seconds and re-run steps 1-3.
- A benign PodSecurity "restricted" warning is emitted on every rollout
  (allowPrivilegeEscalation, drop ALL caps, runAsNonRoot,
  seccompProfile). It's `warn`-level only, not `enforce`, so it does
  not block the rollout. Silenceable by tightening the container's
  securityContext on the Deployment.
- The redeployment is part of the K_* DoD for any task touching the
  agent-facing surface. Don't mark such tasks `done` until the new
  digest is confirmed running and an end-to-end smoke against the live
  surface confirms the change.

## What is deliberately not here

- `write_once` / `schema_once` convenience tools (would break the safety
  thesis).
- Server-initiated notifications to the agent (clients route them to logs,
  not to the model's context).
- Idle warnings in the session block (no actionable value; see
  `DESIGN.md` §6).
