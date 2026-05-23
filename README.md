# typedb-mcp

A safety-focused [Model Context Protocol](https://modelcontextprotocol.io)
server that exposes a [TypeDB 3.11+](https://typedb.com) database to an LLM
agent. Written in Rust, built on the official `typedb-driver` (gRPC) and
the `rmcp` SDK.

This is an independent reimplementation, in Rust on gRPC, of the
official [`typedb/typedb-mcp`](https://github.com/typedb/typedb-mcp)
Python (HTTP) server. The transport URL (`/mcp` on the configured port)
and the broad shape of the tool surface match upstream's intent, but
**this is not a drop-in replacement**:

1. The tool surface diverges in shape: this server adds `start_session`
   as the entry point, and **every other tool requires a `session_id`
   argument** (see `DESIGN.md` §3 and §7). Existing clients written
   against the upstream Python server will need to thread `session_id`
   through every call.
2. The lifecycle is gated: schema must be read before a transaction can
   be opened on a database, only one transaction is open at a time, and
   the response envelope is structured (`session`, `next_moves`,
   `result` / `error`).
3. The TypeDB endpoint is gRPC (default `:1729`), not the HTTP API
   (`:8000`) the upstream Python server uses.

See [`DESIGN.md`](DESIGN.md) for the full contract.

## What's different from the upstream server

- **Server-issued sessions are the state-machine root.** Call
  `start_session` first; pass the returned `session_id` to every other
  tool. Sessions outlive the MCP transport session (a deliberate choice
  — see `DESIGN.md` §3 for the LiteLLM-gateway interop story that drove
  it). Default TTL is 60 minutes of inactivity, refreshed on every
  resolve.
- **Connection-bound transaction model.** Within a session, the agent
  must call `get_schema` before opening a transaction, and every
  response carries `next_moves` telling it what calls are valid next.
  The state machine is small, but it is explicit.
- **Lifecycle-aware errors.** Every error tells the agent whether the
  open transaction is still alive (`error.retriable_in_same_tx`). The
  classification was probed empirically against TypeDB CE 3.10.4 (HTTP)
  and 3.11.1 (gRPC); see `DESIGN.md` §5.
- **gRPC, not HTTP.** We use the official `typedb-driver` crate, which
  speaks gRPC on port `1729`. The upstream Python server speaks HTTP on
  port `8000`. Point the container at the TypeDB **gRPC** endpoint.

> **TypeDB version requirement.** This server requires **TypeDB 3.11.0
> or newer**. Earlier 3.x releases are not supported — deployment
> against 3.10.x and below has been observed to fail at the driver
> layer.

The MCP-facing surface is ten tools served over Streamable HTTP at
`/mcp` (or stdio for local clients):
`start_session`, `list_databases`, `get_schema`, `open_read`,
`open_write`, `open_schema`, `query`, `commit`, `rollback`, `read_once`.

## Theory: agent affordances are user affordances

The design of this server rests on one observation: **the things that make
a tool easier for a human to use correctly are the same things that make
it easier for an agent to use correctly**. The two populations have
different failure modes — humans get bored, agents hallucinate — but they
fail for a shared underlying reason, which is that *both have to resolve
indirection to act, and both have a finite budget for doing so*.

### Indirection resolution

Consider two errors:

> Specified database does not exist.

versus

> Specified database `agnts` does not exist. Available databases: `agents`,
> `ost`, `scratch`.

The first error tells the caller that something is wrong. To recover from
it, the caller has to do additional work: figure out which database was
specified, list the databases that *do* exist, compare the two, guess at
the intended name, and retry. Every one of those steps is a hop of
indirection — a separate question the caller has to answer before it can
make progress.

The second error collapses all of those hops into the response itself.
The caller does not have to ask "what databases exist?" because that
question is already answered. It does not have to ask "what did I
specify?" because the verbatim input is quoted. The recovery path is
one read, not five.

A human reading the first error is mildly inconvenienced. An agent
reading the first error has to spend tokens on a multi-turn investigation
that ends, often, in a fabricated database name. The cost differential
between the two error messages is small for the human and large for the
agent — but the *direction* of the cost is the same. Better is better
for both.

### Attention stacking

A related cost is what we'll call *attention stacking*: the implicit
dimensional context that conversation participants are expected to
silently track on each other's behalf.

Consider a sentence like "the Publisher API is slow." Real systems have
many dimensions along which "the Publisher API" might vary — preprod or
prod, region, internal or external endpoint, current release or the
canary, this tenant or that one. The sentence fixes none of those
dimensions explicitly. To act on it, every reader has to consult a
running mental model of the conversation so far and decide which
dimensions are pinned, which are still variable, and which were pinned
several turns ago and might have drifted since. That running model is
the stack; using it is the attention cost.

Humans pay this cost reasonably well over short conversations, with
people they know, in domains they're current on. The cost rises sharply
when any of those conditions weaken — new participant, long thread,
unfamiliar subsystem. Agents pay this cost on every single turn, with
no continuity beyond the literal text in their context, and the failure
mode when the stack mis-resolves is not "ask a clarifying question" but
"confidently act on the wrong referent."

Pronouns are the densest case of the same phenomenon. "It calls the API
and then it returns the result, which it then passes to the handler"
has three `it`s with three different referents; resolving each one
requires the reader to walk back through prior context and pick the
right antecedent. Drop the pronouns and the indirection drops with
them.

The mitigation is mechanical and slightly tedious: when writing for a
system that involves multiple actors, environments, or objects, fully
qualify the reference every time. Not "the Publisher API" but "the
preprod Publisher API in `us-east-1`". Not "it returns the result" but
"`fetch_user` returns the `User` record". The prose gets longer. The
attention budget drops to near zero. For anything an agent will read,
and for any technical conversation that crosses more than two
participants or more than ten minutes, that is the right trade.

### How those principles show up in this server

- **`next_moves` on every response.** The agent is stateless between
  tool calls beyond what the response carries. Rather than expect the
  agent to remember the state machine, every response re-teaches the
  immediate horizon — the literal tool names that are valid next, given
  the session's current state. The agent does not have to resolve "what
  comes after a successful `open_write`?" because the response already
  answered it.

- **Lifecycle-aware errors.** Every error answers, in the same envelope,
  the question "is my transaction still alive?" via a structured
  `retriable_in_same_tx` boolean and a prose sentence. The agent does
  not have to cross-reference the error class against a table in
  `DESIGN.md` to decide whether to retry or to open a fresh transaction.

- **Schema-read gate, enforced and explained.** The server refuses to
  open a transaction on a database whose schema the session has not yet
  read. That refusal is enforced in code and *also* documented in the
  tool descriptions the agent sees. The constraint is not a hidden
  precondition that surfaces as a confusing error; it is a stated rule
  with a named recovery path.

- **Plain, fully-qualified prose in tool descriptions.** Tool
  descriptions name the tools they reference (`open_read`, `commit`)
  with literal backticked names rather than "the read tool" or "the
  finalization step". This costs a handful of tokens and removes a
  category of agent mis-resolution entirely.

None of this is novel. It is the same set of techniques a careful
technical writer would apply to documentation for a human audience. The
claim of this server is just that those techniques are not optional when
the reader is an LLM, because the LLM has no way to ask a follow-up
question on its own behalf — every ambiguity it encounters either gets
resolved by additional tool calls (expensive) or papered over by
fabrication (worse).

The shorthand: **build for the agent the way you would build for a
careful but tired human, and both will do better work**.

## Running the container

Published to GitHub Container Registry:
`ghcr.io/tilmon-engineering/typedb-mcp`.

```bash
docker run -p 8001:8001 ghcr.io/tilmon-engineering/typedb-mcp:latest \
    --typedb-address host.docker.internal:1729 \
    --typedb-username admin \
    --typedb-password password
```

On Linux add `--add-host=host.docker.internal:host-gateway` to reach a
TypeDB running on the host.

### Flags and env vars

| Flag                | Env var            | Default              | Notes                          |
| ------------------- | ------------------ | -------------------- | ------------------------------ |
| `--typedb-address`  | `TYPEDB_ADDRESS`   | `127.0.0.1:1729`     | TypeDB **gRPC** endpoint.      |
| `--typedb-username` | `TYPEDB_USERNAME`  | `admin`              |                                |
| `--typedb-password` | `TYPEDB_PASSWORD`  | `password`           |                                |
| `--typedb-tls`      | `TYPEDB_TLS`       | `false`              | `true` for TLS-fronted TypeDB. |
| `--listen-http`     | `LISTEN_HTTP`      | `0.0.0.0:8001`       | MCP served at `/mcp`.          |

A leading `http://` on `--typedb-address` is stripped for compatibility
with copy-pasted upstream commands, but the port must still point at gRPC.

### `config.toml` extras

A handful of knobs are only reachable via the config file (`config.toml`
in CWD, or `TYPEDB_MCP_CONFIG=/path/to/config.toml`):

```toml
[server]
session_ttl_s         = 3600          # SessionStore entry TTL (default 60 min)
# Per-kind tx-idle reaper timeouts. Reads can hold for a long agent
# turn cheaply (no uncommitted state, no blocking); writes/schema
# stay aggressive (they hold state, schema blocks readers).
idle_timeout_read_s   = 600           # default 600 s
idle_timeout_write_s  = 60            # default  60 s
idle_timeout_schema_s = 60            # default  60 s
result_cap            = 500           # max answers per query response

# Streamable HTTP Host-header allowlist. Omit to keep rmcp's loopback
# default (localhost, 127.0.0.1, ::1) — fine for local stdio-style use.
# Extend for Kubernetes Service DNS / Ingress hostnames:
# allowed_hosts = ["typedb-mcp.typedb.svc:8001", "typedb-mcp.example.com"]
# Or `[]` to disable the check entirely (only if upstream network
# isolation is enforced); logs WARN on startup if you do.
```

### Kubernetes / behind an Ingress

By default the Streamable HTTP transport only accepts requests whose
`Host` header is `localhost`, `127.0.0.1`, or `::1` (rmcp's
DNS-rebinding defense). In-cluster Service DNS and Ingress hostnames
are rejected with a `403 Forbidden: Host header is not allowed`. Set
`server.allowed_hosts` in `config.toml` to extend the allowlist; see
the snippet above.

### Wiring an MCP client

Point your client at `http://<host>:8001/mcp`. For Cursor:

```json
{
  "mcpServers": {
    "typedb": { "url": "http://localhost:8001/mcp" }
  }
}
```

### Tool flow (agent's-eye view)

The expected call sequence for a write-and-verify task:

```
start_session()
  -> { session_id: "abc…", databases: [...] }

get_schema(session_id, database)
  -> arms the schema-read gate for that database

open_write(session_id, database)
  -> transaction is now open

query(session_id, query="insert ...")
  -> insert lands inside the open tx; not yet persisted

commit(session_id)
  -> writes durable

read_once(session_id, database, query="match ...")
  -> verify
```

Any tool other than `start_session` returns `SESSION_UNKNOWN` or
`SESSION_EXPIRED` if `session_id` does not resolve; the response's
`next_moves` directs the agent to call `start_session` and reissue.

## Running from source

```bash
cargo run --release
```

Reads `config.toml` from the working directory, or whatever
`TYPEDB_MCP_CONFIG` points at. See `config.example.toml`.

The default config enables the stdio transport, which is what local MCP
clients (e.g. Claude Code) expect. To run the HTTP transport instead,
uncomment `listen_http` in your config or use the container.

## Tests

Unit tests for the error classifier run with `cargo test`. The smoke
and in-process integration tests need a live TypeDB at `127.0.0.1:1729`
and are gated:

```bash
TYPEDB_MCP_SMOKE=1 cargo test
```

## License

MIT OR Apache-2.0.
