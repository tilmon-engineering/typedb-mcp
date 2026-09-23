#!/usr/bin/env bash
# Run the live core suites against disposable pinned TypeDB CE releases.
# Usage: scripts/compatibility_matrix.sh [3.12.0 3.13.0]
set -Eeuo pipefail

if [[ "${TYPEDB_MCP_SMOKE:-}" != 1 ]]; then
  echo "error: compatibility matrix requires TYPEDB_MCP_SMOKE=1" >&2
  exit 2
fi
runtime="${TYPEDB_CONTAINER_RUNTIME:-}"
if [[ -z "$runtime" ]]; then
  if command -v podman >/dev/null 2>&1; then runtime=podman
  elif command -v docker >/dev/null 2>&1; then runtime=docker
  else
    echo "error: no podman or docker found; install a container runtime or set TYPEDB_CONTAINER_RUNTIME" >&2
    exit 127
  fi
fi
command -v "$runtime" >/dev/null 2>&1 || { echo "error: container runtime '$runtime' is unavailable" >&2; exit 127; }

if (( $# )); then versions=("$@"); else versions=(3.12.0 3.13.0); fi
base_name="typedb-mcp-matrix-$$"
ids=()
cleanup() {
  local id
  for id in "${ids[@]:-}"; do
    "$runtime" rm -f "$id" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT INT TERM

for version in "${versions[@]}"; do
  [[ "$version" =~ ^3\.(12|13)\.0$ ]] || { echo "error: unsupported matrix version '$version' (expected 3.12.0 or 3.13.0)" >&2; exit 2; }
  name="${base_name}-${version//./-}"
  # Publish the fixed internal gRPC port on an OS-assigned loopback port.
  # Fully-qualified registry path: podman refuses short names without a TTY.
  id=$("$runtime" run -d --name "$name" -p 127.0.0.1::1729 "docker.io/typedb/typedb:$version") || {
    echo "error: failed to start TypeDB CE $version; check '$runtime' and image availability" >&2; exit 1;
  }
  ids+=("$id")
  port=$("$runtime" port "$id" 1729/tcp 2>/dev/null | sed -nE 's#^.*:([0-9]+)(/tcp)?$#\1#p' | head -n1)
  [[ -n "$port" ]] || { echo "error: runtime did not report a loopback port for $name" >&2; exit 1; }
  address="127.0.0.1:$port"
  echo "== TypeDB CE $version ($address) =="
  ready=0
  readiness_test="readiness_connects"
  if ! grep -q "async fn ${readiness_test}" crates/typedb-mcp-core/tests/smoke_local.rs; then
    readiness_test="common_helpers_compile"
  fi
  for _ in {1..60}; do
    if TYPEDB_MCP_TEST_ADDRESS="$address" TYPEDB_MCP_TEST_USERNAME="${TYPEDB_MCP_TEST_USERNAME:-admin}" TYPEDB_MCP_TEST_PASSWORD="${TYPEDB_MCP_TEST_PASSWORD:-password}" \
      cargo test -p typedb-mcp-core --test smoke_local "$readiness_test" -- --exact --nocapture >/dev/null 2>&1; then ready=1; break; fi
    sleep 2
  done
  if (( ! ready )); then
    echo "error: TypeDB CE $version did not become authenticated-ready at $address" >&2
    "$runtime" logs "$id" >&2 || true
    exit 1
  fi
  export TYPEDB_MCP_TEST_ADDRESS="$address"
  export TYPEDB_MCP_TEST_USERNAME="${TYPEDB_MCP_TEST_USERNAME:-admin}"
  export TYPEDB_MCP_TEST_PASSWORD="${TYPEDB_MCP_TEST_PASSWORD:-password}"
  # Workspace tests include the core tests; avoid running the suite twice.
  cargo test --workspace --tests -- --nocapture
  "$runtime" rm -f "$id" >/dev/null
  ids=("${ids[@]/$id}")
done
