# typedb-mcp — Design

Last verified: 2026-05-22

A Model Context Protocol server, written in Rust, that exposes a TypeDB 3.x
database to an LLM agent through a connection-bound transaction model. The
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

Per MCP session, the server holds:

```
SessionState {
  schema_seen: HashMap<DatabaseName, ()>,   // databases whose schema was read this session
  tx: Option<OpenTx>,
}

OpenTx {
  id: TypeDbTxId,                  // opaque ID from POST /v1/transactions/open
  database: DatabaseName,
  kind: TxKind,                    // Read | Write | Schema
  state: TxState,                  // Open | Dead
  last_activity: Instant,
}
```

State invariants:

1. **At most one transaction per session.** Attempting to open a second one
   while one is already open returns `TX_ALREADY_OPEN`.
2. **Schema must be read before opening a transaction.** Each
   `open_read | open_write | open_schema` checks `schema_seen` for the target
   database. `read_once` enforces the same gate.
3. **`schema_seen` is cleared for `db` whenever a `schema` transaction
   committed against `db` succeeds.** The agent's mental model of the schema
   is now stale by its own action; force a re-read before any further work.
4. **Idle reaper.** A background task scans sessions and rolls back any
   transaction whose `last_activity` is older than `idle_timeout_s` (default
   60s, configurable). The session keeps existing — only the transaction is
   reaped. The agent learns about the reaping by getting an error on its
   next call (no push notifications; see §2).

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
              └─ idle > idle_timeout_s   -> tx None (reaper)
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
2. **Specific TSV codes** — `TSV12`/`TSV9`/`TSV8`/`TSV2`/`TSV7`. The
   `TSV*` family is heterogeneous (commit-fatal, recoverable, dead-tx
   marker, etc. all in one prefix), so it must be matched code-by-code.
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
  database: agents | null
  transaction:                  # null if no tx open
    id: tx_a8c1                 # short opaque correlation id (not the TypeDB UUID)
    kind: write                 # read | write | schema
    state: open                 # open is the only externally observable state
  schema_seen_for: [agents]     # databases whose schema this session has read
next_moves:
  - "Call `get_schema(database=<name>)` for any database before querying it."
result: ...                     # or
error:
  class: WRITE_FAILED
  message: "..."                # already lifecycle-aware, see §5
  typedb_codes: [CNT6, DVL7, COW5, WEX1, PEX6, QEX14, TSV11, HSR16]
  retriable_in_same_tx: false   # convenience flag the agent can act on without parsing prose
```

`next_moves` is intentionally not a single string — multiple valid next
operations exist at most states, and the agent should see all of them.
The catalogue lives in CLAUDE.md so it can evolve without re-versioning
this design document.

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

Nine tools.

### 7.1 `list_databases`

- **Params**: none
- **Returns**: `{ databases: [{name}, ...] }`
- **Annotation**: read-only, idempotent
- **Gate**: none
- **Errors**: `UPSTREAM_UNAVAILABLE`

### 7.2 `get_schema`

- **Params**: `database: string`
- **Returns**: `{ schema: "<full TypeQL define text>" }`
- **Annotation**: read-only, idempotent
- **Gate**: none. **Sets** `schema_seen[database]`.
- **Errors**: `UNKNOWN_DATABASE`, `UPSTREAM_UNAVAILABLE`
- **Description (agent-facing)**: "Returns the full TypeQL `define` source for
  a database. You **must** call this before opening any transaction on the
  database. TypeQL 3.x differs materially from 2.x; do not write queries from
  prior assumptions about the schema."

### 7.3 `open_read`

- **Params**: `database: string`
- **Returns**: session block with new tx
- **Annotation**: read-only
- **Gate**: `tx is None AND schema_seen[database]`
- **Errors**: `SCHEMA_NOT_READ`, `TX_ALREADY_OPEN`, `UNKNOWN_DATABASE`,
  `UPSTREAM_UNAVAILABLE`

### 7.4 `open_write`

- **Params**: `database: string`
- **Returns**: session block with new tx
- **Annotation**: **destructive on commit** (mutations only land on commit)
- **Gate**: same as `open_read`
- **Errors**: same as `open_read`

### 7.5 `open_schema`

- **Params**: `database: string`
- **Returns**: session block with new tx
- **Annotation**: **destructive, schema-level** — clients should escalate
  confirmation
- **Gate**: same as `open_read`
- **Errors**: same as `open_read`

### 7.6 `query`

- **Params**: `query: string` (TypeQL)
- **Returns**: `{ answer_type, answers, warning }` + session block
- **Annotation**: behaviour depends on the open transaction's kind. Tool
  description must say so explicitly.
- **Gate**: tx is `Open`
- **Errors**: `NO_TX_OPEN`, `WRONG_TX_TYPE`, `PARSE_ERROR`, `TYPE_ERROR`,
  `WRITE_FAILED`, `RESULT_LIMIT_EXCEEDED`, `TIMEOUT`,
  `IDLE_TIMEOUT` (if tx was reaped between calls)
- **Result cap**: if the server-side cap (default 500, configurable) is
  reached, the answers are **discarded** and `RESULT_LIMIT_EXCEEDED` is
  returned with guidance to paginate.

### 7.7 `commit`

- **Params**: none
- **Returns**: session block with `tx: null`
- **Annotation**: finalize
- **Gate**: tx is `Open` and `kind != Read`
- **Errors**: `NO_TX_OPEN`, `TX_IS_READ`, `COMMIT_FAILED`,
  `UPSTREAM_UNAVAILABLE`
- **Side effect**: if the tx was a `schema` tx, `schema_seen[database]` is
  cleared on successful commit.

### 7.8 `rollback`

- **Params**: none
- **Returns**: session block with `tx: null`
- **Annotation**: cleanup
- **Gate**: none — forgiving
- **Behaviour**: if no tx is open, returns success with a short ack
  ("No transaction was open; nothing to roll back."). The safe direction.

### 7.9 `read_once`

- **Params**: `database: string`, `query: string`
- **Returns**: `{ answer_type, answers, warning }` + session block (tx still
  null afterward)
- **Annotation**: read-only, idempotent
- **Gate**: `tx is None AND schema_seen[database]`
- **Behaviour**: internally `open_read -> query -> close`. The transaction is
  not exposed to the agent.
- **Errors**: `SCHEMA_NOT_READ`, `TX_ALREADY_OPEN` (if a tx is already open in
  this session), `PARSE_ERROR`, `TYPE_ERROR`, `UNKNOWN_DATABASE`,
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
idle_timeout_s = 60
result_cap     = 500
listen_stdio   = true
listen_http    = "127.0.0.1:8765"     # null to disable

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
- `write_once` / `schema_once` convenience tools (deliberately omitted).
- Per-agent role-based access control. Access is governed by the credentials
  the operator gives the MCP server.
- Streaming results. All query responses are batched and capped.

---

## 11. Open work tracked against this design

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
