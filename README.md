# typedb-mcp

A safety-focused [Model Context Protocol](https://modelcontextprotocol.io)
server that exposes a [TypeDB 3.x](https://typedb.com) database to an LLM
agent. Written in Rust, built on the official `typedb-driver` (gRPC) and
the `rmcp` SDK.

This is an independent reimplementation of the official
[`typedb/typedb-mcp`](https://github.com/typedb/typedb-mcp) Python server.
At the MCP transport layer it is a drop-in replacement — same port, same
URL path, same tool surface — but the design has a tighter safety thesis.
See [`DESIGN.md`](DESIGN.md) for the full contract.

## What's different from the upstream server

- **Connection-bound transaction model.** The agent must call `get_schema`
  before opening a transaction, and every response carries `next_moves`
  telling it what calls are valid next. The state machine is small, but
  it is explicit.
- **Lifecycle-aware errors.** Every error tells the agent whether the
  open transaction is still alive (`error.retriable_in_same_tx`). The
  classification was probed empirically against TypeDB CE 3.10.4 (HTTP)
  and 3.11.1 (gRPC); see `DESIGN.md` §5.
- **gRPC, not HTTP.** We use the official `typedb-driver` crate, which
  speaks gRPC on port `1729`. The upstream Python server speaks HTTP on
  port `8000`. This is the one place the "drop-in" framing breaks: point
  the container at the TypeDB **gRPC** endpoint.

The MCP-facing surface (nine tools, served over Streamable HTTP at
`/mcp`) matches upstream's intent. The wire-level behaviour is
intentionally stricter.

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

The same principle applies to nested wrappers:

> An error occurred while handling a request: an error occurred while
> performing operation Y on object Z: an error occurred in subsystem W:
> ...

Each `an error occurred while` is a frame the reader has to push onto a
mental stack before they reach the actual claim. By the time the reader
hits the leaf, they have to pop frames back up to relate the leaf to the
original call. For a human this is fatiguing; for an agent it is
fatiguing *and* lossy, because the model has to hold the stack in working
context while doing the rest of its reasoning.

The fix is the same in both cases: state the operation and the object
once, plainly, at the top of the message, and put the leaf cause inline
rather than wrapping it. A response should read top-down, not
inside-out.

### Pronouns and un-qualified references

A related failure mode is excessive pronouns and ambiguous referents in
documentation, prompts, and inter-system messages. "It calls the API and
then it returns the result, which it then passes to the handler" contains
three `it`s with three different referents. A human reader can usually
disambiguate from context with effort. An agent often cannot, and silent
mis-resolution shows up downstream as confidently wrong code.

The mitigation is mechanical: when writing for a system that involves
multiple actors and objects, rewrite sentences to use the
fully-qualified noun every time. The prose gets longer; the indirection
budget drops to zero. That is the right trade for anything an agent will
read.

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

### Wiring an MCP client

Point your client at `http://<host>:8001/mcp`. For Cursor:

```json
{
  "mcpServers": {
    "typedb": { "url": "http://localhost:8001/mcp" }
  }
}
```

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
