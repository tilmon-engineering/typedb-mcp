# typedb-mcp — Design

Last verified: 2026-05-24 (added: hybrid library+application positioning,
§3a per-session extension state, §6.1 envelope stability, §7 tool-surface
preface on library composition, §11 library extension API, §10 library
prohibition note)

A Model Context Protocol server, written in Rust, that exposes a TypeDB
3.11+ database to an LLM agent through a connection-bound transaction
model. TypeDB 3.10.x and earlier are not supported — deployment against
pre-3.11 servers fails at the driver layer. The
design exists to **stop the agent from corrupting data through accidental
mistypes, runaway queries, or careless commits** — while keeping the
interaction surface small and explicit enough that a competent agent can use
it without ceremony.

This document is the source of truth for the design. Implementation should
match what is written here, and changes to behaviour should be reflected here
first.

---

## 1. Thesis

The agent must be forced to **look at the data it is about to write before
committing**. Everything in the design follows from that.

Concretely:

- All writes happen inside an explicit transaction the agent owns.
- The agent must read the schema for a database before opening a transaction
  on it.
- There is no `write_once` convenience tool. There is a `read_once` tool,
  because reading is safe and common.
- Result sets are capped and the agent is taught to paginate with
  `offset; limit;`.
- Errors carry **lifecycle-aware language** so the agent always knows whether
  the transaction it was holding is still alive.

The MCP server is the safety layer. TypeDB itself provides transaction
isolation, schema-enforced typing, atomic commit, and per-query error
reporting, but it does not provide query-cost estimates, result caps,
dry-run mode, or per-user role gating. The MCP layer adds those, plus the
agent-facing state machine.

### Hybrid library + application

typedb-mcp ships in two shapes: a **server binary** with the ten tools
enumerated in §7, and a **library crate** that exposes the same
connection, session, transaction, and envelope machinery for other MCP
servers to embed. A consuming server may mount the ten raw tools
verbatim, prefix them, omit some, or add semantic tools of its own —
but any TypeDB-backed tool it adds **must** go through the library's
transaction helpers, so the safety thesis (this section), the
schema-read gate (§3.5), the single-tx invariant (§3.4), the
schema-commit clear rule (§3.6), and the response envelope (§6) hold
uniformly across raw and semantic tools. The library API is described
in §11; it is the authoritative integration surface.

---

## 2. Transport & runtime

- **Language**: Rust (stable), async on Tokio.
- **MCP library**: `rmcp`, the official Anthropic Model Context Protocol Rust
  SDK. Both stdio and Streamable HTTP transports are supported behind a single
  tool-handler abstraction; the design assumes both are exposed by default.
- **Session model**: one MCP session corresponds to one optional open TypeDB
  transaction. `rmcp` provides `SessionId` via `RequestContext.extensions`;
  the server keeps a map of `SessionId -> SessionState`.
- **TypeDB driver**: the server speaks to TypeDB through the **official
  `typedb-driver` crate (gRPC)**. The driver gives us a typed surface
  (`Transaction`, `QueryAnswer`, `Concept` …) and is maintained by the
  TypeDB team, so protocol changes land in the driver, not our code. The
  empirical error stack we rely on in §5 is preserved verbatim by the
  driver — `Error::Server(ServerError)` carries the `[CNT6] → [WEX1] → …`
  chain via `Display`/`message()`, and `Error::Connection(TransactionIsClosed*)`
  is the analogue of HTTP's `TSV12`. The cluster must expose TypeDB's
  gRPC port (default `1729`) for the MCP server to reach it.
- **No server-initiated notifications.** MCP's `notifications/*` channels exist
  but client implementations route them to logs, not to the agent's context.
  All agent-facing state is delivered in the **session block** of normal tool
  responses. See §6.

---

## 3. Session state

### Why sessions are explicit, not transport-derived

The server's safety thesis depends on cross-call state (schema-read gate,
single-tx invariant, transactional lifecycle). MCP's Streamable HTTP
transport defines a session via the `Mcp-Session-Id` header, but in
practice some prominent clients (notably LiteLLM's MCP gateway, observed
on edge-01 2026-05-22) treat each tool call as a fresh `initialize` →
call → `DELETE /mcp` cycle. Under that pattern the transport session
exists for only one tool call and our cross-call state is never
reachable. The schema gate fires on every `open_*` because the prior
`get_schema` happened in a transport-session that has already been torn
down.

We therefore decouple from the transport. The server issues its **own**
session identifier via the `start_session` tool, and **every other tool
requires the agent to pass that identifier in as a `session_id`
parameter**. Whether the transport session survives between calls is
irrelevant: the `session_id` argument is what reconstitutes our state.

This is a load-bearing design choice. It changes the agent's first move
on entry from "call `list_databases` or `get_schema`" to "call
`start_session`."

### Per-session data

Per server session, the server holds:

```
SessionState {
  id: SessionId,                            // UUID v4, returned by start_session
  schema_seen: HashSet<DatabaseName>,       // databases whose schema was read this session
  tx: Option<OpenTx>,                       // at most one open tx per session
  expires_at: Instant,                      // absolute deadline; refreshed on each tool call
}

OpenTx {
  id: TypeDbTxId,                  // opaque ID from POST /v1/transactions/open
  database: DatabaseName,
  kind: TxKind,                    // Read | Write | Schema
  state: TxState,                  // Open | Dead
  last_activity: Instant,
}
```

### State invariants

1. **Sessions are minted only by `start_session`.** No tool other than
   `start_session` ever creates or accepts an unknown `session_id`.
   Calling any other tool with a `session_id` the server does not know
   returns `SESSION_UNKNOWN`. The agent's recovery is to call
   `start_session` (which yields a fresh ID) and reissue.
2. **Sessions expire on inactivity.** `expires_at` is initialized to
   `now + session_ttl_s` (default 3600s) at `start_session`, and refreshed
   to `now + session_ttl_s` on every subsequent tool call that
   successfully resolves the session. A tool call whose `session_id`
   resolves to an *expired* session returns `SESSION_EXPIRED` and the
   stored session is purged. The agent must call `start_session` to get
   a new one.
3. **Transport teardown does not touch session state.** A DELETE on the
   MCP transport session (or a stdio disconnect) does not destroy any
   `SessionState` — only the TTL reaper does. The transport and
   application session lifecycles are deliberately independent.
4. **At most one transaction per session.** Attempting to open a second
   one while one is already open returns `TX_ALREADY_OPEN`.
5. **Schema must be read before opening a transaction.** Each
   `open_read | open_write | open_schema` checks `schema_seen` for the
   target database. `read_once` enforces the same gate.
6. **`schema_seen` is cleared for `db` whenever a `schema` transaction
   committed against `db` succeeds.** The agent's mental model of the
   schema is now stale by its own action; force a re-read before any
   further work.
7. **Idle reaper for open transactions.** A background task scans
   sessions and releases any transaction whose `last_activity` is older
   than its per-kind idle timeout. The timeouts are asymmetric because
   the costs are asymmetric: read transactions hold no uncommitted state
   and don't block other transactions, so they're cheap to keep alive
   across a long agent turn (`idle_timeout_read_s`, default 600s);
   write and schema transactions hold uncommitted state and (for schema)
   block other readers/writers, so they stay aggressive
   (`idle_timeout_{write,schema}_s`, default 60s each). All three are
   configurable and distinct from `session_ttl_s`. The session keeps
   existing — only the transaction is reaped. The agent learns about
   the reaping by getting an error on its next call (no push
   notifications; see §2). The release mechanism depends on tx kind:
   read transactions are released via `Transaction::close()` (a
   client-side gRPC-stream teardown that the server acks before the
   call returns); write and schema transactions are released via
   `Transaction::rollback()` (which discards uncommitted writes). See
   §5 for why this distinction matters.
8. **Session reaper for whole sessions.** The same background task purges
   `SessionState` whose `expires_at` is in the past. If such a session
   held an open tx, that tx is released as part of the purge — a
   session purge is the strongest form of cleanup.

### 3a. Per-session extension state (library consumers)

Library consumers building semantic tools on top of the kernel often
need per-session state of their own (current focus entity, accepted
disclaimers, a small query cache scoped to the agent's conversation).
The kernel exposes a typemap-style `Extensions` slot on `SessionState`
for this. Each consumer crate stashes its own concrete type:

```rust
// inside a consumer tool body
session.extensions_mut(|ext| {
    ext.get_or_insert_with::<MyFocus, _>(MyFocus::default).note(entity_id);
}).await;
```

Rules:

1. Extensions live and die with `SessionState`; the reaper purges them
   alongside any open tx when the session is expired or removed.
2. Access is mediated by the same per-session `Mutex` that protects
   `schema_seen` and `tx`; consumers cannot hold an extension reference
   across `await` points except inside the closure passed to
   `extensions`/`extensions_mut`.
3. Extensions never affect the schema-read gate, the single-tx
   invariant, or the envelope shape. They are agent-invisible unless a
   consumer's tool chooses to surface them in `result`.

The kernel does not gossip extension data into `SessionSnapshot`. The
agent's view of session state is exactly the §6 envelope and nothing
more; consumer additions stay in consumer-emitted `result`/`error`.

---

## 4. Transaction lifecycle

```
                ┌──────────────────────────────────────┐
                │             SESSION                  │
                │  schema_seen: { db -> () }           │
                │  tx: None | OpenTx{db, kind, state}  │
                └──────────────────────────────────────┘

   list_databases / get_schema  (no gate, any time)
                              │
                              ▼
   get_schema(db)  sets  schema_seen[db]
                              │
                              ▼
   open_read(db) / open_write(db) / open_schema(db)
       requires: tx is None AND schema_seen[db]
                              │
                              ▼
                            OPEN
              ├─ query (recoverable err: stays OPEN)
              ├─ query (write-pipeline err: -> DEAD)
              ├─ commit:
              │     ├─ ok      -> tx None
              │     │           if was schema: clear schema_seen[db]
              │     └─ commit-time err -> tx None (DEAD path)
              ├─ rollback                -> tx None
              │     (wire op: `close()` for read tx — TypeDB rejects
              │      explicit `Rollback` on a read tx with [TSV3];
              │      `rollback()` for write/schema. Agent verb unchanged.)
              └─ idle > idle_timeout_<kind>_s -> tx None (reaper, same
                                                wire-op rule as above)
```

The DEAD state is observable but not persistent: as soon as the agent issues
any further tool call, the server cleans up the dead tx (TypeDB has already
discarded it) and returns an error that tells the agent the tx is gone. There
is no agent-visible "DEAD" — the agent sees "tx is None" on the next response.

---

## 5. Error semantics (empirically verified)

TypeDB 3.x's error behaviour was probed against
`https://typedb.edge-01.tilmonengineering.com` (TypeDB CE 3.10.4). Findings:

| Error class                              | Terminal code signature                                       | Tx survives? |
| ---------------------------------------- | ------------------------------------------------------------- | ------------ |
| Syntax (parse failure)                   | `TQL03 -> TSV7 -> HSR16`                                      | **YES**      |
| Type inference / unknown type            | `INF2 -> QUA1 -> QEX8 -> TSV11 -> HSR16`                      | **YES**      |
| Wrong tx kind for query                  | `TSV9` (write under read) / `TSV8` (schema under write)       | **YES**      |
| Write-execution (regex, @values, etc.)   | `... -> WEX1 -> PEX6 -> QEX14 -> TSV11 -> HSR16`              | **NO**       |
| Commit-time (cardinality, deferred)      | `... -> COW5 -> DCT3 -> TSV5 -> HSR18`                        | **NO**       |
| Operation on already-dead tx             | `TSV12 "no open transaction" -> HSR16`                        | n/a          |
| Concurrent-rollback transient (post-commit race) | `TSV13 "Execution interrupted by a concurrent transaction rollback" -> HSR16` | n/a (retry the call) |
| Explicit `Rollback` sent against a read tx       | `TSV3 "Read transactions cannot be rolled back, since they never contain writes."` | n/a (driver-side bug if seen) |
| Server-issued `session_id` not recognized | (server-internal: SessionStore lookup miss)                  | n/a          |
| Server-issued `session_id` past TTL       | (server-internal: SessionStore lookup miss after purge)      | n/a          |

The discriminator between **recoverable** and **fatal** is the presence of the
`WEX1 / PEX6 / QEX14` chain (write pipeline) in the TypeDB error stack, or any
of the commit-time codes (`DCT3 / TSV5 / HSR18`). The MCP server parses the
error stack and classifies the error before sending it to the agent.

**Transport difference on follow-up calls to a dead tx.** Two shapes
were observed empirically and the handler tolerates either:

- **HTTP API** (probed 2026-05-22, TypeDB 3.10.4): a follow-up call returns
  `[TSV12] "Operation failed: no open transaction"`.
- **gRPC driver** (verified 2026-05-22 against TypeDB CE 3.11.1 via
  `typedb-driver` 3.11.1, see `tests/smoke_integration.rs`): a follow-up
  call **replays the original error stack** verbatim (e.g. the same
  `[CNT6] → [WEX1] → …` chain). The classifier returns the original
  fatal class (`WriteFailed` etc.), not `IdleTimeout`.

Both shapes are safe: `retriable_in_same_tx` is false for either, and the
handler clears `state.tx = None` on the first fatal class. The agent's
*next* call therefore goes through the handler's `NoTxOpen` branch — it
never sees a duplicate copy of the original error. The classifier's job
is correct fatality classification; the handler's job is to ensure the
agent's view of "your tx is dead" is delivered once, cleanly.

Mapping into agent-facing error classes:

| MCP error class           | When                                              | Tx after  |
| ------------------------- | ------------------------------------------------- | --------- |
| `SCHEMA_NOT_READ`         | `open_*` / `read_once` without prior `get_schema` | unchanged |
| `TX_ALREADY_OPEN`         | second `open_*` while one is held                 | unchanged |
| `NO_TX_OPEN`              | `query` / `commit` with no tx                     | none      |
| `TX_IS_READ`              | `commit` on a read tx (`TSV2`)                    | unchanged |
| `WRONG_TX_TYPE`           | data query on read tx, schema query on write tx   | **open**  |
| `PARSE_ERROR`             | TypeQL syntax failure                             | **open**  |
| `TYPE_ERROR`              | type-inference failure (unknown type, bad shape)  | **open**  |
| `WRITE_FAILED`            | write-pipeline error                              | **gone**  |
| `COMMIT_FAILED`           | commit-time error (markers `DCT3`/`TSV5`/`SRV19`/`HSR18`) | **gone**  |
| `RESULT_LIMIT_EXCEEDED`   | server-side cap hit                               | **open**  |
| `TIMEOUT`                 | server-side query timeout                         | **gone**  |
| `UNKNOWN_DATABASE`        | database not present (TypeDB `SRV3`)              | unchanged |
| `IDLE_TIMEOUT`            | tx was reaped before this call                    | none      |
| `TRANSIENT_CONFLICT`      | concurrent-rollback transient (`TSV13`)           | none (retry the call) |
| `UPSTREAM_UNAVAILABLE`    | TypeDB unreachable / auth failure                 | session-level |
| `UNCLASSIFIED`            | TypeDB returned a code we don't recognize         | **gone**  |

Each error response **must** include:

1. What the server thinks the state is (database, tx id/kind/state).
2. What the agent tried to do.
3. Whether the transaction is still alive after this error.
4. What the valid next moves are.

Example — recoverable:

> `TYPE_ERROR`: Type label `nonexistent_thing` not found near column 14.
> Your `write` transaction on `agents` is still open. Fix the query and
> retry, or `rollback` to start over.

Example — fatal:

> `WRITE_FAILED`: Constraint `@regex("^.+@.+\..+$")` violated on attribute
> `email`. Your transaction has been aborted by TypeDB because the write
> entered the data pipeline before failing. Open a new `write` transaction
> to continue.

### 5.0.1 Closing a read transaction

The agent-facing `rollback` tool is the universal "I'm done, discard"
verb. Under the hood it picks the correct wire op based on tx kind:

- **Write / schema tx** — send the driver's `Transaction::rollback()`,
  which sends a server-side `Rollback` request that discards
  uncommitted writes. This is the right op here.
- **Read tx** — send the driver's `Transaction::close()`, which signals
  the gRPC stream shutdown and awaits the server's ack before
  returning. We do **not** send `Rollback`, because TypeDB 3.x rejects
  it with `[TSV3] Read transactions cannot be rolled back, since they
  never contain writes.` Worse, the rejected request leaves the
  server-side tx lingering until the stream tears down at its own
  pace — and that lingering tx is the most likely source of the
  `TSV13` (`TRANSIENT_CONFLICT`) bursts observed on rapid reads. The
  `close()` path is synchronous: the server's view of the tx is gone
  before the next call can be issued, so no collision.

The same rule applies to all internal release paths: `read_once`,
the explicit `rollback` tool, and both reaper paths (idle-tx and
expired-session purge). See `OpenTx::release()` in `src/session.rs`.

**Release only after the answer stream is fully materialized.** The
driver's `QueryAnswer` is a lazy gRPC stream tied to the live
transaction, not data. Any release (`close()` included) tears the
stream down: draining after release aborts — server-side `TSV13`
under driver 3.11.1, client-side `CXN07` under 3.11.5. `read_once`
shipped with this ordering inverted (query → close → drain) and
failed deterministically on any result set larger than the driver's
prefetch batch; the regression test
`mcp_read_once_returns_full_multibatch_result` pins the corrected
order.

### 5.1 Classifier strategy

TypeDB defines ~70 error-code prefixes and ~300 individual codes via a
`typedb_error!` macro scattered across ~40 `error.rs` files in the
[`typedb/typedb`](https://github.com/typedb/typedb) repo. There is **no
single canonical list** — to be complete you would have to scan the whole
tree at build time. We deliberately do not.

Instead, `classify_typedb_error` (in `src/error.rs`) uses a four-layer
strategy, matched in order:

1. **Structural markers in the stack** — `WEX1`/`PEX6`/`QEX14` decide
   "write pipeline torn down → `WRITE_FAILED`"; `DCT3`/`TSV5`/`HSR18`/
   `SRV19` decide "commit failed → `COMMIT_FAILED`". These markers
   discriminate *fatality* regardless of which constraint (`CNT*`,
   `DVL*`, etc.) actually fired.
2. **Specific TSV codes** — `TSV12`/`TSV13`/`TSV9`/`TSV8`/`TSV2`/`TSV7`. The
   `TSV*` family is heterogeneous (commit-fatal, recoverable, dead-tx
   marker, benign post-commit transient, etc. all in one prefix), so it
   must be matched code-by-code. `TSV13` ("Execution interrupted by a
   concurrent transaction rollback") is mapped to `TRANSIENT_CONFLICT`:
   the agent is told to reissue the failing call as-is, since no state
   was damaged.
3. **Family prefixes** — whole families with one agent-facing class:
   `[INF*` and `[QUA*` → `TYPE_ERROR` (recoverable); `[TQL*` → `PARSE_ERROR`
   (recoverable); `[CNT*` and `[DVL*` → `WRITE_FAILED` (constraint or
   validation failed outside a pipeline context).
4. **Top-code fallback** for codes that don't show up as stack markers:
   `TXN1`/`TXN2` → `TIMEOUT`; `SRV3` → `UNKNOWN_DATABASE`; `AUT2`/`HSR9`
   → `UPSTREAM_UNAVAILABLE`. Anything else → `UNCLASSIFIED`.

The fallback is `UNCLASSIFIED`, **not** `UPSTREAM_UNAVAILABLE`. The
latter promises "retry once, TypeDB was unreachable" — actively
misleading for an unrecognized code from a newer TypeDB release.
`UNCLASSIFIED` instead carries the verbatim driver message and
bracketed code list to the agent, tells it the transaction is gone,
and asks the human operator to teach the classifier the new code.

This means **the classifier degrades safely**: new TypeDB codes don't
silently become bad advice. They become a labeled "I don't know" that
the agent can surface and a human can fix in one place (`src/error.rs`).

---

## 6. Response envelope

Every tool response carries three things: a `session` block of observed
state, a `next_moves` list of one-line usage hints for the current state,
and either a `result` (on success) or an `error` (on failure).

The `next_moves` field is the in-band lifecycle teacher: it re-teaches the
immediate horizon of the state machine on every call so the agent never
has to remember what the valid next operation is. See CLAUDE.md for the
full per-tool catalogue of hints and the rationale.

```yaml
session:
  session_id: 6ee0c8b9-...      # the server-issued ID this snapshot belongs to;
                                # omitted entirely on SESSION_UNKNOWN/SESSION_EXPIRED
                                # errors where no session was resolved
  database: agents | null
  transaction:                  # null if no tx open
    id: tx_a8c1                 # short opaque correlation id (not the TypeDB UUID)
    kind: write                 # read | write | schema
    state: open                 # open is the only externally observable state
  schema_seen_for: [agents]     # databases whose schema this session has read
next_moves:
  - "Call `get_schema(session_id=..., database=<name>)` for any database before querying it."
result: ...                     # or
error:
  class: WRITE_FAILED           # or SESSION_UNKNOWN / SESSION_EXPIRED, etc.
  message: "..."                # already lifecycle-aware, see §5
  typedb_codes: [CNT6, DVL7, COW5, WEX1, PEX6, QEX14, TSV11, HSR16]
  retriable_in_same_tx: false   # convenience flag the agent can act on without parsing prose
```

The `start_session` response is the one tool whose `result` block also
carries the session_id explicitly (alongside `expires_in_seconds` and
`databases`), since it's the canonical way the agent learns the ID it
needs to thread through subsequent calls. Every other tool's response
puts the ID in `session.session_id` only.

`next_moves` is intentionally not a single string — multiple valid next
operations exist at most states, and the agent should see all of them.
The catalogue lives in CLAUDE.md so it can evolve without re-versioning
this design document.

### 6.1 Envelope stability (library consumers)

The envelope above is the public agent-facing contract, not an
implementation detail. Library consumers MUST:

1. Emit the same outer shape: `session` + `next_moves` + exactly one of
   `result` or `error`. The kernel's `ok` / `err` / `state_err` helpers
   are the only sanctioned constructors; consumers should not
   hand-build `Content::text` envelopes.
2. Use `ErrorClass` values from this crate. The enum is closed and
   versioned alongside this document. Novel semantic-level errors that
   do not fit an existing class use `UNCLASSIFIED` with a tool-specific
   `message` and `next_moves`.
3. Build `next_moves` with the `NextMoves` builder (`default_for(class)`
   plus tool-specific `.extended([...])` lines) so the prose register
   stays consistent across raw and semantic tools.

The envelope JSON shape is tagged with `ENVELOPE_VERSION` (currently 1).
Any breaking change to the field set, the `ErrorClass` enum, or the
session block requires a version bump and a DESIGN.md edit.

Notes:

- We do **not** emit `idle_s`, `auto_rollback_in_s`, or warning strings. They
  cost tokens on every response and the agent has no actionable use for them:
  if it is reading them, it has just issued a query, which resets the idle
  timer. The agent learns about the reaper only when it next calls into a
  reaped tx.
- `typedb_codes` are surfaced raw for debugging and for sophisticated agents
  that want to introspect; they are not the primary signal.
- `retriable_in_same_tx` is the boolean form of §5's "Tx after" column.
  Precise meaning: **`true` iff a transaction remains open and continuable
  after this error**. It does *not* mean "you can retry the failing call
  as-is" — for `TX_IS_READ` and `TX_ALREADY_OPEN` the field is `true`
  because the underlying tx is alive (consult `next_moves` for what to do
  with it), even though the specific call that failed will keep failing
  the same way until you change something. `false` covers both "no tx
  existed" (`NO_TX_OPEN`, `SCHEMA_NOT_READ`, `UNKNOWN_DATABASE`, …) and
  "the tx was torn down" (`WRITE_FAILED`, `COMMIT_FAILED`, `TIMEOUT`,
  `IDLE_TIMEOUT`). Mapping: `unchanged` + `open` → `true`; `none` + `gone`
  + `session-level` → `false`.

---

## 7. Tool surface

Ten tools. The first, `start_session`, mints the `session_id` that every
other tool requires (see §3). All non-`start_session` tools take
`session_id: string` as a required argument and return `SESSION_UNKNOWN`
or `SESSION_EXPIRED` if it does not resolve.

The ten tools enumerated here are the defaults the **binary** mounts.
Library consumers (§11) may prefix raw tool names, omit individual
tools, or expose only a subset — subject to the safety thesis (§1): no
consumer-added tool may compose into a `write_once` / `schema_once`
shape, and any tool that opens a transaction must do so through the
kernel's `with_*_tx` helpers so the schema-read gate and single-tx
invariant remain enforced.

### 7.0 `start_session`

- **Params**: none
- **Returns**: `{ session_id: string, expires_in_seconds: integer,
  databases: [{name}, ...] }`. `expires_in_seconds` is the configured
  TTL (per-call resolution refreshes it); reporting it relative rather
  than as an absolute timestamp avoids any clock-skew confusion at the
  agent. The database list is returned for
  convenience — it is the same content `list_databases` returns and makes
  the post-session-start tool selection one call shorter for the common
  case.
- **Annotation**: read-only, **state-creating**
- **Gate**: none
- **Errors**: `UPSTREAM_UNAVAILABLE` (cannot fetch the database list;
  `session_id` is still issued in that case so the agent can retry the
  database fetch via `list_databases` without re-minting)
- **`next_moves`**: same response-envelope contract as every other tool;
  carries one-line reminders that the agent must pass `session_id` to
  every subsequent call, and that calling `get_schema(database)` for any
  database is the required next step before opening a transaction on it.
- **Description (agent-facing)**: "Mint a new server-side session and
  return its `session_id`. Pass the `session_id` to every other tool.
  Sessions expire after a configured period of inactivity (default 60
  minutes); on `SESSION_EXPIRED` or `SESSION_UNKNOWN`, call this again
  for a fresh ID."

### 7.1 `list_databases`

- **Params**: `session_id: string`
- **Returns**: `{ databases: [{name}, ...] }`
- **Annotation**: read-only, idempotent
- **Gate**: valid session
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `UPSTREAM_UNAVAILABLE`

### 7.2 `get_schema`

- **Params**: `session_id: string`, `database: string`
- **Returns**: `{ schema: "<full TypeQL define text>" }`
- **Annotation**: read-only, idempotent
- **Gate**: valid session. **Sets** `schema_seen[database]` on that
  session.
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `UNKNOWN_DATABASE`,
  `UPSTREAM_UNAVAILABLE`
- **Description (agent-facing)**: "Returns the full TypeQL `define` source for
  a database. You **must** call this before opening any transaction on the
  database. TypeQL 3.x differs materially from 2.x; do not write queries from
  prior assumptions about the schema."

### 7.3 `open_read`

- **Params**: `session_id: string`, `database: string`
- **Returns**: session block with new tx
- **Annotation**: read-only
- **Gate**: valid session AND `tx is None AND schema_seen[database]`
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `SCHEMA_NOT_READ`,
  `TX_ALREADY_OPEN`, `UNKNOWN_DATABASE`, `UPSTREAM_UNAVAILABLE`

### 7.4 `open_write`

- **Params**: `session_id: string`, `database: string`
- **Returns**: session block with new tx
- **Annotation**: **destructive on commit** (mutations only land on commit)
- **Gate**: same as `open_read`
- **Errors**: same as `open_read`

### 7.5 `open_schema`

- **Params**: `session_id: string`, `database: string`
- **Returns**: session block with new tx
- **Annotation**: **destructive, schema-level** — clients should escalate
  confirmation
- **Gate**: same as `open_read`
- **Errors**: same as `open_read`

### 7.6 `query`

- **Params**: `session_id: string`, `query: string` (TypeQL)
- **Returns**: `{ answer_type, answers, warning }` + session block
- **Annotation**: behaviour depends on the open transaction's kind. Tool
  description must say so explicitly.
- **Gate**: valid session AND tx is `Open`
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `NO_TX_OPEN`,
  `WRONG_TX_TYPE`, `PARSE_ERROR`, `TYPE_ERROR`, `WRITE_FAILED`,
  `RESULT_LIMIT_EXCEEDED`, `TIMEOUT`,
  `IDLE_TIMEOUT` (if tx was reaped between calls)
- **Result cap**: if the server-side cap (default 500, configurable) is
  reached, the answers are **discarded** and `RESULT_LIMIT_EXCEEDED` is
  returned with guidance to paginate.

### 7.7 `commit`

- **Params**: `session_id: string`
- **Returns**: session block with `tx: null`
- **Annotation**: finalize
- **Gate**: valid session AND tx is `Open` and `kind != Read`
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `NO_TX_OPEN`,
  `TX_IS_READ`, `COMMIT_FAILED`, `UPSTREAM_UNAVAILABLE`
- **Side effect**: if the tx was a `schema` tx, `schema_seen[database]` is
  cleared on successful commit.

### 7.8 `rollback`

- **Params**: `session_id: string`
- **Returns**: session block with `tx: null`
- **Annotation**: cleanup
- **Gate**: valid session — otherwise forgiving
- **Behaviour**: if the session is valid and no tx is open, returns
  success with a short ack ("No transaction was open; nothing to roll
  back."). The safe direction. `SESSION_UNKNOWN`/`SESSION_EXPIRED` still
  error, since there is no session in which to "do nothing."
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`

### 7.9 `read_once`

- **Params**: `session_id: string`, `database: string`, `query: string`
- **Returns**: `{ answer_type, answers, warning }` + session block (tx still
  null afterward)
- **Annotation**: read-only, idempotent
- **Gate**: valid session AND `tx is None AND schema_seen[database]`
- **Behaviour**: internally `open_read -> query -> close`. The transaction is
  not exposed to the agent.
- **Errors**: `SESSION_UNKNOWN`, `SESSION_EXPIRED`, `SCHEMA_NOT_READ`,
  `TX_ALREADY_OPEN` (if a tx is already open in this session),
  `PARSE_ERROR`, `TYPE_ERROR`, `UNKNOWN_DATABASE`,
  `RESULT_LIMIT_EXCEEDED`, `TIMEOUT`, `UPSTREAM_UNAVAILABLE`
- **Why no `write_once`**: deliberate. The safety thesis of this server is
  that the agent must look at the data before committing a write. A one-shot
  write would undo that.

---

## 8. Result cap and pagination guidance

`query` and `read_once` enforce a hard cap (default 500 answers, configurable).
When hit, the result is discarded and the error message instructs the agent:

> `RESULT_LIMIT_EXCEEDED`: Query returned more than 500 answers (cap). The
> result has been discarded — TypeDB does not paginate after the fact.
> Re-issue the query with `sort`, `offset`, and `limit` clauses to fetch in
> chunks. The canonical form is:
>
> ```
> match $x isa <type>, has <key> $k;
> sort $k asc;
> offset N;
> limit M;
> fetch { ... };
> ```
>
> **`offset` must come before `limit`** — pipeline stages execute in textual
> order. Without `sort`, page boundaries are not stable across calls.

The canonical form was verified empirically against TypeDB 3.10.4 on
2026-05-22. Order matters: `limit 3; offset 3;` returns zero rows
(`limit` truncates to 3, then `offset` skips 3 of 3); `offset 3; limit 3;`
returns rows 4-6.

---

## 9. Configuration (operator)

```toml
[server]
# Per-kind tx idle timeouts. Asymmetric because the costs are
# asymmetric: read tx hold no uncommitted state and don't block other
# tx, so they get a long leash for slow agent turns; write/schema hold
# state (schema also blocks readers) so they stay aggressive.
idle_timeout_read_s   = 600    # default 600s (10 min)
idle_timeout_write_s  = 60     # default 60s
idle_timeout_schema_s = 60     # default 60s
session_ttl_s         = 3600   # session-level: TTL on the SessionStore entry, refreshed on every call
result_cap            = 500
listen_stdio          = true
listen_http           = "127.0.0.1:8765"     # null to disable

[typedb]
address     = "127.0.0.1:1729"                  # gRPC host:port; no scheme
tls_enabled = false                             # true for TLS-fronted production servers
credentials = { source = "env", username_var = "TYPEDB_USER", password_var = "TYPEDB_PASS" }

[logging]
audit_log_path = "/var/log/teng-typedb-mcp/audit.jsonl"
# audit log fields: ts, session_id, tool, params (TypeQL verbatim),
#                   result_summary { size | error_class | typedb_codes }, tx_id
```

Audit logging is **operator-only**. The agent has no tool to read it.

The driver manages its own connection lifecycle (gRPC HTTP/2 with auth
handled internally). The operator does not need to think about token
refresh — that is the driver's responsibility.

---

## 10. Out of scope (for now)

- Server-initiated notifications / idle warnings (see §2).
- Nested transactions, savepoints, multiple concurrent transactions per
  session.
- Query-cost estimation or dry-run mode (TypeDB does not expose these).
- Result pagination as a first-class feature (the agent paginates with
  `offset; limit;`).
- `write_once` / `schema_once` convenience tools (deliberately omitted —
  this prohibition applies to both the binary's tool surface *and* the
  library API in §11; the kernel exposes no helper that composes into
  a one-shot write).
- Per-agent role-based access control. Access is governed by the credentials
  the operator gives the MCP server.
- Streaming results. All query responses are batched and capped.

---

## 11. Library extension API

This section is the contract for crate consumers who want to embed
typedb-mcp's connection, session, and transaction machinery into their
own MCP server alongside semantic tools of their own design. The binary
in this repository (`src/main.rs`) is itself a consumer of this API and
is the reference for "how to wire it up."

### 11.1 What the library provides

Three layers, named for the role they play:

1. **The kernel** — `TypeDbCore`. A cheaply-cloneable `Arc` bundle of
   the TypeDB driver client, the `SessionStore`, and the operator
   config. One per process.
2. **The session handle** — `SessionHandle<'_>`. The result of
   `core.resolve(session_id)`. Carries the resolved `SessionId` plus
   the per-session `Arc<Mutex<SessionState>>`. All transaction work and
   envelope emission happens through methods on this handle so the
   per-session lock is acquired with the right window every time.
3. **The raw tool set** — `RawToolSet`. A builder over the ten tools
   in §7 that consumers can mount into their `ServerHandler`, with
   knobs for prefixing names, omitting individual tools, and (for the
   binary) being mounted whole.

### 11.2 What a consumer's semantic tool body looks like

The intended shape, in full:

```rust
#[tool(description = "Find a customer by external id.")]
async fn find_customer(
    &self,
    Parameters(p): Parameters<MyParams>,
) -> Result<CallToolResult, McpError> {
    let session = match self.core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return Ok(env),   // SESSION_UNKNOWN/EXPIRED envelope
    };
    Ok(session.with_read_tx(
        "crm",
        NextMoves::default_for_semantic("find_customer"),
        |tx| async move {
            let answer = tx.query(&typeql).await.map_err(InternalError::Driver)?;
            // ... materialize answer ...
            Ok(serde_json::json!({ "customer": ... }))
        },
    ).await)
}
```

The body has no direct access to `SessionState`, no direct construction
of `OpenTx`, no direct call to `Transaction::rollback`/`commit`/`close`,
and no hand-built `CallToolResult`. Every invariant from §3 and §5 is
enforced by the helper.

### 11.3 The kernel API (sketch)

```rust
pub struct TypeDbCore {
    pub config: Arc<Config>,
    pub typedb: Arc<TypeDbClient>,
    pub sessions: Arc<SessionStore>,
}

impl TypeDbCore {
    pub async fn connect(cfg: CoreConfig) -> Result<Arc<Self>, InternalError>;
    pub fn spawn_reaper(self: &Arc<Self>) -> tokio::task::JoinHandle<()>;

    pub async fn start_session(&self) -> SessionId;

    /// Resolve agent-supplied session_id. On miss/expiry, returns a
    /// ready CallToolResult carrying the canonical envelope.
    pub async fn resolve(&self, sid: &str)
        -> Result<SessionHandle<'_>, CallToolResult>;
}

pub struct SessionHandle<'a> { /* &core, SessionId, Arc<Mutex<...>> */ }

impl SessionHandle<'_> {
    pub async fn snapshot(&self) -> SessionSnapshot;

    pub async fn extensions<F, R>(&self, f: F) -> R
        where F: AsyncFnOnce(&Extensions) -> R;
    pub async fn extensions_mut<F, R>(&self, f: F) -> R
        where F: AsyncFnOnce(&mut Extensions) -> R;

    // --- the load-bearing transaction helpers (§3 + §5 invariants) ---

    pub async fn with_read_tx<F, T>(
        &self, database: &str, hints: NextMoves, f: F,
    ) -> CallToolResult
        where F: AsyncFnOnce(&DriverTransaction) -> Result<T, InternalError>,
              T: Serialize;

    pub async fn with_write_tx<F, T>(
        &self, database: &str, hints: NextMoves, f: F,
    ) -> CallToolResult
        where F: AsyncFnOnce(&DriverTransaction) -> Result<TxOutcome<T>, InternalError>,
              T: Serialize;

    pub async fn with_schema_tx<F, T>(...)  -> CallToolResult;

    /// Borrow the agent's currently-open tx (e.g. for a `query`-style
    /// semantic tool). Emits NO_TX_OPEN envelope if none is open.
    pub async fn with_current_tx<F, T>(
        &self, hints: NextMoves, f: F,
    ) -> CallToolResult
        where F: AsyncFnOnce(&DriverTransaction, TxKind, &str)
                    -> Result<T, InternalError>,
              T: Serialize;

    // --- envelope helpers (canonical §6 shape) ---

    pub fn ok(&self, result: impl Serialize, hints: NextMoves) -> CallToolResult;
    pub fn err(&self, err: InternalError, explanation: &str, hints: NextMoves)
        -> CallToolResult;
    pub fn state_err(&self, class: ErrorClass, msg: &str, hints: NextMoves)
        -> CallToolResult;
}

pub enum TxOutcome<T> {
    Commit(T),    // kernel issues commit(); on failure, COMMIT_FAILED envelope
    Rollback(T),  // kernel issues the kind-correct release (§5.0.1)
}
```

`with_*_tx` enforces:

- Schema-read gate (`SCHEMA_NOT_READ` if the database's schema has not
  been read in this session).
- Single-tx invariant (`TX_ALREADY_OPEN` if a tx is already open).
- Per-session lock held across precondition checks, network open, the
  closure body, and the kind-correct release (§3 reasoning about
  TSV13 races; the lock window choice is load-bearing).
- Kind-correct release (`close()` for read, `rollback()` for
  write/schema, per §5.0.1).
- Error classification through `classify_typedb_error`, with the
  resulting `ErrorClass` driving `retriable_in_same_tx` and whether
  `state.tx` is cleared.
- Envelope construction via `ok` / `err`.

### 11.4 Mounting the raw tool set

The binary's wiring (in `src/main.rs`) becomes one of:

```rust
// All ten tools, default names.
let raw = RawToolSet::all(core.clone()).into_router();

// Or selectively:
let raw = RawToolSet::all(core.clone())
    .with_prefix("tdb_")
    .without(["read_once"])   // consumer is shadowing it
    .into_router();
```

The router returned has a `Self` type that depends on the composition
strategy (see §11.5). The binary's existing `TypeDbMcp` handler stays
as a thin wrapper around `RawToolSet` so the binary's behaviour does
not change.

### 11.5 Composing raw tools into a consumer's handler

`rmcp` 1.7's `ToolRouter<S>` can only be merged when both routers
share the same `S`. This forces a composition choice the library MUST
document (currently an open question being resolved in the
implementation PR):

- **Option A — embed-our-struct.** Consumer's handler holds a
  `TypeDbMcp` field and uses rmcp's multi-router `+` feature to
  combine routers, with their semantic tools defined on `TypeDbMcp`
  via an extension trait. Simple but couples the consumer's tools to
  our struct type.
- **Option B — generic raw-tools router.** The library hand-writes
  `pub fn raw_tools_router<H>(accessor: fn(&H) -> &Arc<TypeDbCore>)
   -> ToolRouter<H>` without the `#[tool_router]` macro. The
  consumer's handler is their own type; we generate routes whose
  closures reach the kernel through `accessor`. Most ergonomic,
  highest implementation cost.
- **Option C — dispatch delegation.** The consumer implements
  `ServerHandler::call_tool` and delegates name-prefixed tool calls
  to an internal `TypeDbMcp`. Avoids router-merge entirely. Reasonable
  for consumers with very few semantic tools.

The chosen option is recorded here once the implementation lands; the
others remain available to consumers with unusual needs.

### 11.6 Versioning and stability

- The kernel API (this section), the envelope shape (§6.1), and the
  `ErrorClass` enum are SemVer-stable across patch releases of the
  library crate. Breaking changes require a minor-version bump and a
  DESIGN.md edit.
- `rmcp` is a public dependency; the supported version range is
  pinned in the library's `Cargo.toml` and bumped intentionally.
- The binary depends on the library at the matching version; the two
  are released in lockstep.

### 11.7 Out of scope for the library (specifically)

- No `with_write_once` / `with_schema_once` helper, ever. This is the
  §1 thesis applied to the library API: a consumer who needs to write
  exposes `open_write` (raw) plus a semantic tool that operates inside
  the open tx via `with_current_tx`, or accepts that their semantic
  mutation tool is a guided template the agent invokes inside an
  explicit `open_write` → `query` → `commit` flow.
- No public `OpenTx` or `DriverTransaction` outside a `with_*_tx`
  closure. The lock window, `last_activity` bookkeeping, and
  close-vs-rollback selection (§5.0.1) stay kernel-side.
- No consumer extension of `ErrorClass`. The enum is closed; novel
  errors use `UNCLASSIFIED`.
- No per-process "extensions" slot on `TypeDbCore` (only per-session).
  Consumers that need process-global state hold it on their own
  handler struct alongside the `Arc<TypeDbCore>`.

---

## 12. Open work tracked against this design

- Empirical: verify TypeDB behaviour under concurrent sessions hitting the
  same database (issue #6146 hints at concurrent-request error confusion).
- Empirical: verify that closing a session in `rmcp` (stdio EOF, HTTP session
  expiry) reliably triggers our session-state cleanup so we do not leak
  transactions on the TypeDB side. The idle reaper backstops this, but the
  prompt-cleanup path on session close has not been exercised end-to-end.
- Decide: whether to surface per-query `warning` from TypeDB to the agent
  inside `result`, or merge it into the error envelope.
- Audit logging: the operator config field exists; the writer is not
  implemented.
