# typedb-mcp

A safety-focused [Model Context Protocol](https://modelcontextprotocol.io) server exposing a TypeDB 3.12+ database through the official Rust `typedb-driver` (gRPC) and `rmcp` SDK. This is an independent Rust/gRPC implementation, not a drop-in replacement for the upstream Python/HTTP server.

## Support and tool surface

The supported server floor is TypeDB **3.12.0**. The checked-in Rust driver is exactly **3.12.3**. The compatibility matrix covers TypeDB 3.12.0; TypeDB 3.13.0 is accepted with status `unverified` because it is outside the verified matrix. Stable versions at or above the floor may be reported unverified; prerelease or unparsable versions fail startup rather than arming an unverified metadata contract.

The reference binary has twelve default tools:

`start_session`, `list_databases`, `get_schema`, `open_read`, `open_write`, `open_schema`, `query`, `checkpoint`, `commit`, `rollback`, `read_once`, `server_info`.

Call `start_session` first; every other tool requires its returned `session_id`. The schema-read gate and one-transaction-per-session lifecycle remain mandatory. `create_database` and `delete_database` are optional admin tools, disabled by default. `export_database` and `import_database` are optional migration tools, disabled by default and available only through an explicitly stdio-authorized handler.

`start_session` returns a server-issued session ID, database list, and bundled TypeQL reference. `server_info` is bounded and read-only: it reports build/driver/upstream version policy, enabled tools for that transport, and a timed connectivity observation. Configured addresses and replica topology are omitted unless `server.expose_connection_details = true`; credentials, secret environment variable names, and CA filesystem paths are never returned. It is not a comprehensive health check or a promise of failover success.

## Configuration

The binary loads TOML with `Config::load_from_path`. Set `TYPEDB_MCP_CONFIG` to the config file path; otherwise it attempts `config.toml` in its current working directory. A local stdio harness should use an **absolute** config path because its child may have a foreign working directory:

```bash
TYPEDB_MCP_CONFIG=/home/operator/typedb-mcp/config.local.toml \
  /home/operator/typedb-mcp/target/release/typedb-mcp
```

The executable path and config path above are examples only; do not put credentials in command lines or committed files. Credentials should use environment-variable names in TOML and be supplied only to the child process. See `config.example.toml` and `config.smoke.toml`.

The only address configuration forms are `typedb.address`, a non-empty `typedb.addresses` list, or a non-empty `typedb.address_translation` map. Exactly one must be present. `tls_root_ca_path` requires `tls_enabled = true` and must be an absolute path. `request_timeout_s` is a positive **unary driver request** bound; it is not an in-transaction `query` or `commit` deadline. `primary_failover_retries` configures the upstream driver option; this project adds no automatic application retries, especially not for mutations. Migration uses a separately built driver with retries forced to zero.

Defaults include: stdio enabled; HTTP absent/disabled; read/write/schema idle timeouts 600/60/60 seconds; session TTL 3600 seconds; result cap 500; admin, migration, and connection-detail disclosure disabled; migration schema cap 16 MiB; migration destination free-space reserve 256 MiB; shutdown grace 30 seconds. Both transports may be enabled together. HTTP uses `/mcp` and should retain a restrictive `allowed_hosts` policy. The existing create/delete policy is unchanged: both are absent unless `enable_database_admin_tools = true`, and delete requires exact confirmation.

## Local stdio harness

Stdio is a local child process. Use an absolute executable and absolute `TYPEDB_MCP_CONFIG`; do not rely on cwd, `~`, shell expansion, URLs, or relative paths. The child runs as the operator’s stdio account and its filesystem namespace is authoritative. Co-location of a client, MCP process, and TypeDB container does **not** imply shared mounts; a container child namespace must actually have the files mounted at the paths supplied to tools. Migration remains default-off:

```toml
[server]
listen_stdio = true
# listen_http omitted
# enable_database_migration_tools = false
```

On stdio EOF the reference binary shuts down the whole process. Logs go to stderr; stdout is reserved for MCP protocol frames. HTTP live checks must use only normal tools and explicitly exclude file migration tools.

## Database migration (stdio only, opt-in)

Set `server.enable_database_migration_tools = true` in a stdio config. The routes are `export_database` and `import_database`; they are not listed or callable over HTTP, including mixed-transport sessions. File paths are absolute native paths only: no relative, cwd, tilde, drive-relative/root-relative Windows, URL, or expansion notation. Paths must be distinct regular files as applicable; export destinations must not exist (including dangling symlinks), and there is no overwrite option.

Export requires a prior `get_schema` for the source and writes schema/data through private staging directories on each destination filesystem. The preflight free-space check enforces only the configured 256 MiB reserve by default. This is **headroom, not proof that the export will fit**: no source-size estimate is assumed. Results include resolved paths, byte sizes, and streaming SHA-256 checksums. Publication is two-file and not atomic; a second-publication failure is reported as partial with precise surviving paths. The server never deletes a path whose ownership is uncertain.

Import validates UTF-8 schema size (16 MiB default), file identity, complete length-delimited framing, sizes, and checksums, then copies inputs into private staging before invoking TypeDB. It requires `confirm_database` to exactly equal `database` (byte-for-byte string equality) and the target database must not exist. Import is new-target-only; it never overwrites, deletes, retries, or recreates a target. After a successful import, call `get_schema` before normal transactions because the target schema gate is invalidated for live sessions.

Cancellation, driver failure, disk-full errors, malformed framing, and publication failures are reported honestly. An import that may have reached TypeDB is `unknown_or_partial`; do not assume rollback or atomic crash recovery. Responses include checksums/sizes where known. The worker owns reservations and cleanup until completion even if the caller disconnects. Ordinary failures clean only owned staging files; a process crash can leave staging files for operator inspection and cleanup. External TypeDB clients are outside this process-local coordination, so they may race; the server never auto-deletes a possibly externally created target.

These are client export/import paths, not TypeDB’s server data directory. Do not copy, edit, or restore TypeDB’s internal data directory as if it were a migration file. For upgrades and backups, follow the TypeDB release documentation and your tested backup/restore runbook; this server does not schedule backups or claim crash atomicity.

## Security and transports

Both stdio and Streamable HTTP can reach the same TypeDB connection and ordinary transaction tools, so apply least privilege and network controls to both. Stdio additionally grants explicitly opted-in local filesystem migration authority to the trusted child account. HTTP never receives that authority, even if a session ID originated over stdio. Configure HTTP Host-header allowlisting and place it behind appropriate authentication/network policy; do not expose an unprotected listener. Connection-detail disclosure is off by default and never includes credentials or CA paths.

The TypeDB endpoint is gRPC (normally port 1729), not the upstream Python server’s HTTP port 8000. A historical upstream redirect fix, where relevant, was TypeScript-only; it is not a Rust driver claim.

## Running and testing

```bash
cargo run --release
# or, with an explicit file:
TYPEDB_MCP_CONFIG=/absolute/path/config.example.toml \
  cargo run --release -p typedb-mcp
```

For disposable live compatibility testing, use the runner (not production resources):

```bash
TYPEDB_MCP_SMOKE=1 bash scripts/compatibility_matrix.sh
# optionally: ... compatibility_matrix.sh 3.12.0 3.13.0
```

The runner uses podman or docker, dynamically allocated loopback ports, authenticated readiness checks, and removes only its own containers. If neither runtime is available it fails with an actionable error; it does not silently pass.

See [`DESIGN.md`](DESIGN.md) for the state machine and [`RELEASE.md`](RELEASE.md) for the reproducible release gate and authorized deployment workflow.

## License

Dual-licensed under MIT or Apache-2.0; see `LICENSE-MIT` and `LICENSE-APACHE`.
