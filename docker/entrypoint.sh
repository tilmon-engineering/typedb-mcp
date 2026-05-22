#!/bin/sh
# Render a config.toml from CLI flags / env, then exec typedb-mcp.
# Mirrors the official typedb/typedb-mcp container's flag surface so this
# image can be a drop-in replacement at the MCP transport (port 8001 /mcp).
#
# Caveat: --typedb-address here is the TypeDB *gRPC* endpoint (default port
# 1729), not the HTTP endpoint (8000) that the official Python image takes.
# A leading http:// or https:// is stripped for convenience, but the port
# must point at gRPC.
set -eu

TYPEDB_ADDRESS="${TYPEDB_ADDRESS:-127.0.0.1:1729}"
TYPEDB_USERNAME="${TYPEDB_USERNAME:-admin}"
TYPEDB_PASSWORD="${TYPEDB_PASSWORD:-password}"
TYPEDB_TLS="${TYPEDB_TLS:-false}"
LISTEN_HTTP="${LISTEN_HTTP:-0.0.0.0:8001}"

while [ $# -gt 0 ]; do
    case "$1" in
        --typedb-address)  TYPEDB_ADDRESS="$2";  shift 2 ;;
        --typedb-username) TYPEDB_USERNAME="$2"; shift 2 ;;
        --typedb-password) TYPEDB_PASSWORD="$2"; shift 2 ;;
        --typedb-tls)      TYPEDB_TLS="$2";      shift 2 ;;
        --listen-http)     LISTEN_HTTP="$2";     shift 2 ;;
        --help|-h)
            cat <<'USAGE'
typedb-mcp container

Flags:
  --typedb-address  HOST:PORT      TypeDB gRPC endpoint (default 127.0.0.1:1729)
  --typedb-username NAME           (default admin)
  --typedb-password PASS           (default password)
  --typedb-tls      true|false     (default false)
  --listen-http     HOST:PORT      MCP HTTP bind (default 0.0.0.0:8001)

Equivalent env vars: TYPEDB_ADDRESS, TYPEDB_USERNAME, TYPEDB_PASSWORD,
TYPEDB_TLS, LISTEN_HTTP. MCP endpoint is served at /mcp.
USAGE
            exit 0
            ;;
        *) echo "entrypoint: unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Defensive scheme strip so a copy-pasted http://host:port doesn't break us.
TYPEDB_ADDRESS="${TYPEDB_ADDRESS#http://}"
TYPEDB_ADDRESS="${TYPEDB_ADDRESS#https://}"
TYPEDB_ADDRESS="${TYPEDB_ADDRESS%/}"

export TYPEDB_USER="$TYPEDB_USERNAME"
export TYPEDB_PASS="$TYPEDB_PASSWORD"

CONFIG_PATH="${TYPEDB_MCP_CONFIG:-/etc/typedb-mcp/config.toml}"
mkdir -p "$(dirname "$CONFIG_PATH")"
cat >"$CONFIG_PATH" <<EOF
[server]
listen_stdio = false
listen_http  = "$LISTEN_HTTP"

[typedb]
address     = "$TYPEDB_ADDRESS"
tls_enabled = $TYPEDB_TLS

[typedb.credentials]
source       = "env"
username_var = "TYPEDB_USER"
password_var = "TYPEDB_PASS"
EOF

export TYPEDB_MCP_CONFIG="$CONFIG_PATH"
exec /usr/local/bin/typedb-mcp
