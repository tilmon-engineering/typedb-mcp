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
