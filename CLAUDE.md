# typedb-mcp

Last verified: 2026-05-23 (added: OST positioning, rollout procedure;
updated: per-session locking serializes tx work, see `src/handler.rs`)

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

## Boundaries

- Safe to edit: `src/`, `DESIGN.md`, `CLAUDE.md`, `Cargo.toml`.
- The ten tools enumerated in `DESIGN.md` §7 (`start_session` plus the
  nine TypeDB-facing tools) are the entire agent-facing surface. Adding
  an eleventh tool is a design change, not an implementation change —
  update `DESIGN.md` first.

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
