//! In-process MCP integration tests.
//!
//! These tests construct a `TypeDbMcp` server, connect a minimal client to
//! it via a `tokio::io::duplex` pair, and drive the raw tool surface end-to-end
//! through the MCP protocol. They verify the *agent-facing* contract — the
//! envelope shape (`session`, `next_moves`, `result`/`error`), the
//! schema-read gate, the full transaction lifecycle, and that recoverable
//! errors are correctly surfaced as `is_error` results with the right
//! `class`.
//!
//! Gated on `TYPEDB_MCP_SMOKE=1` because they require a live local TypeDB
//! on `127.0.0.1:1729`.

use std::sync::Arc;

use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ClientInfo, GetPromptRequestParams,
        PromptMessageContent,
    },
};
use typedb_mcp_core::{
    config::{Config, Credentials, LoggingConfig, ServerConfig, TypeDbConfig},
    handler::TypeDbMcp,
    language_reference::{
        TYPEQL_LANGUAGE_REFERENCE, TYPEQL_LANGUAGE_REFERENCE_SHA256,
        TYPEQL_LANGUAGE_REFERENCE_SOURCE,
    },
    session::SessionStore,
    tools,
    typedb::TypeDbClient,
};

fn enabled() -> bool {
    std::env::var("TYPEDB_MCP_SMOKE").as_deref() == Ok("1")
}

fn test_config() -> Config {
    Config {
        server: ServerConfig {
            idle_timeout_read_s: 600,
            idle_timeout_write_s: 60,
            idle_timeout_schema_s: 60,
            session_ttl_s: 3600,
            result_cap: 500,
            listen_stdio: true,
            listen_http: None,
            enable_database_admin_tools: false,
            allowed_hosts: None,
        },
        typedb: TypeDbConfig {
            address: "127.0.0.1:1729".into(),
            credentials: Credentials::Inline {
                username: "admin".into(),
                password: "password".into(),
            },
            tls_enabled: false,
        },
        logging: LoggingConfig::default(),
    }
}

/// Minimal client that says "I'm a client" and otherwise defers to defaults.
#[derive(Debug, Clone, Default)]
struct DummyClient;
impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

/// Plumbing: build a connected `(server_handle, client)` pair sharing one
/// fresh TypeDbMcp. Returns the spawned server task handle and the running
/// client service (which exposes `.call_tool`).
///
/// Drop the client to close the connection; awaiting `server_handle` will
/// then unblock.
async fn connected_pair() -> (
    tokio::task::JoinHandle<anyhow::Result<()>>,
    rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
    Arc<SessionStore>,
) {
    connected_pair_with_config(test_config()).await
}

async fn connected_pair_with_config(
    config: Config,
) -> (
    tokio::task::JoinHandle<anyhow::Result<()>>,
    rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
    Arc<SessionStore>,
) {
    let config = Arc::new(config);
    let typedb = Arc::new(
        TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
            .await
            .expect("local TypeDB"),
    );
    let sessions = SessionStore::new();
    let handler = TypeDbMcp::new(config, typedb, sessions.clone());

    // Two unidirectional channels: client → server (writes / reads) and
    // server → client (writes / reads).
    let (client_write, server_read) = tokio::io::duplex(64 * 1024);
    let (server_write, client_read) = tokio::io::duplex(64 * 1024);
    let server_transport = (server_read, server_write);
    let client_transport = (client_read, client_write);

    let server_handle = tokio::spawn(async move {
        let svc = handler.serve(server_transport).await?;
        svc.waiting().await?;
        anyhow::Ok(())
    });

    let client = DummyClient
        .serve(client_transport)
        .await
        .expect("client init");

    (server_handle, client, sessions)
}

/// Extract the JSON envelope text from a CallToolResult — every envelope
/// is a single text content block carrying serialized JSON.
fn envelope(result: &CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text content");
    serde_json::from_str(&text).expect("envelope is JSON")
}

fn is_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
    name: &str,
    args: serde_json::Value,
) -> CallToolResult {
    let params = if args.is_null() {
        CallToolRequestParams::new(name.to_owned())
    } else {
        CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().expect("args is object").clone())
    };
    client.call_tool(params).await.expect("call_tool transport")
}

/// Mint a fresh session_id and return it. Tests should call this once at
/// the top of each scenario, then pass the returned id into every other
/// tool call's args.
async fn mint_sid(client: &rmcp::service::RunningService<rmcp::RoleClient, DummyClient>) -> String {
    let result = call(client, "start_session", serde_json::Value::Null).await;
    let env = envelope(&result);
    env["result"]["session_id"]
        .as_str()
        .expect("start_session response has result.session_id")
        .to_owned()
}

/// Merge a session_id into a JSON object literal (or build one if Null).
fn with_sid(sid: &str, mut args: serde_json::Value) -> serde_json::Value {
    if args.is_null() {
        args = serde_json::json!({});
    }
    let map = args.as_object_mut().expect("args is object");
    map.insert("session_id".into(), serde_json::Value::String(sid.into()));
    args
}

// ---------- 0. reference tool surface ----------

#[tokio::test]
async fn mcp_default_tool_surface_is_exactly_raw_names_all() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;

    let tools_result = client.list_tools(None).await.expect("list_tools");
    let mut actual = tools_result
        .tools
        .iter()
        .map(|tool| tool.name.as_ref().to_owned())
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = tools::names::ALL
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);

    drop(client);
    let _ = server_handle.await;
}

#[tokio::test]
async fn mcp_admin_enabled_tool_surface_includes_admin_names() {
    if !enabled() {
        return;
    }
    let mut config = test_config();
    config.server.enable_database_admin_tools = true;
    let (server_handle, client, _sessions) = connected_pair_with_config(config).await;

    let tools_result = client.list_tools(None).await.expect("list_tools");
    let mut actual = tools_result
        .tools
        .iter()
        .map(|tool| tool.name.as_ref().to_owned())
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = tools::names::ALL
        .iter()
        .chain(tools::names::ADMIN_ALL.iter())
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);

    drop(client);
    let _ = server_handle.await;
}

#[tokio::test]
async fn mcp_admin_tools_require_valid_session_and_delete_confirmation() {
    if !enabled() {
        return;
    }
    let mut config = test_config();
    config.server.enable_database_admin_tools = true;
    let (server_handle, client, _sessions) = connected_pair_with_config(config).await;

    let result = call(
        &client,
        "create_database",
        serde_json::json!({
            "session_id": "not-a-real-session",
            "database": "mcp_admin_invalid_session"
        }),
    )
    .await;
    assert!(is_error(&result));
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "SESSION_UNKNOWN");

    let sid = mint_sid(&client).await;
    let result = call(
        &client,
        "delete_database",
        with_sid(
            &sid,
            serde_json::json!({
                "database": "mcp_admin_confirm_guard",
                "confirm_database": "different_name"
            }),
        ),
    )
    .await;
    assert!(is_error(&result));
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "CONFIRMATION_REQUIRED");

    drop(client);
    let _ = server_handle.await;
}

#[tokio::test]
async fn mcp_admin_tools_reject_current_session_open_transaction() {
    if !enabled() {
        return;
    }
    let mut config = test_config();
    config.server.enable_database_admin_tools = true;
    let (server_handle, client, _sessions) = connected_pair_with_config(config).await;
    let sid = mint_sid(&client).await;
    let db = format!("mcp_admin_open_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let other_db = format!("mcp_admin_other_{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let result = call(
        &client,
        "create_database",
        with_sid(&sid, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));

    let result = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));
    let result = call(
        &client,
        "open_schema",
        with_sid(&sid, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));

    let result = call(
        &client,
        "create_database",
        with_sid(&sid, serde_json::json!({ "database": other_db })),
    )
    .await;
    assert!(is_error(&result));
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "TX_ALREADY_OPEN");

    let result = call(
        &client,
        "delete_database",
        with_sid(
            &sid,
            serde_json::json!({ "database": db, "confirm_database": db }),
        ),
    )
    .await;
    assert!(is_error(&result));
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "TX_ALREADY_OPEN");

    let _ = call(&client, "rollback", with_sid(&sid, serde_json::Value::Null)).await;
    let result = call(
        &client,
        "delete_database",
        with_sid(
            &sid,
            serde_json::json!({ "database": db, "confirm_database": db }),
        ),
    )
    .await;
    assert!(!is_error(&result));

    drop(client);
    let _ = server_handle.await;
}

#[tokio::test]
async fn mcp_delete_database_rejects_other_session_open_transaction_on_target() {
    if !enabled() {
        return;
    }
    let mut config = test_config();
    config.server.enable_database_admin_tools = true;
    let (server_handle, client, _sessions) = connected_pair_with_config(config).await;
    let sid_open = mint_sid(&client).await;
    let sid_delete = mint_sid(&client).await;
    let db = format!("mcp_admin_cross_{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let result = call(
        &client,
        "create_database",
        with_sid(&sid_open, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));
    let result = call(
        &client,
        "get_schema",
        with_sid(&sid_open, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));
    let result = call(
        &client,
        "open_schema",
        with_sid(&sid_open, serde_json::json!({ "database": db })),
    )
    .await;
    assert!(!is_error(&result));

    let result = call(
        &client,
        "delete_database",
        with_sid(
            &sid_delete,
            serde_json::json!({ "database": db, "confirm_database": db }),
        ),
    )
    .await;
    assert!(is_error(&result));
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "TX_ALREADY_OPEN");

    let _ = call(
        &client,
        "rollback",
        with_sid(&sid_open, serde_json::Value::Null),
    )
    .await;
    let result = call(
        &client,
        "delete_database",
        with_sid(
            &sid_delete,
            serde_json::json!({ "database": db, "confirm_database": db }),
        ),
    )
    .await;
    assert!(!is_error(&result));

    drop(client);
    let _ = server_handle.await;
}

// ---------- 1. list_databases returns proper envelope ----------

#[tokio::test]
async fn mcp_list_databases_envelope() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let start = call(&client, "start_session", serde_json::Value::Null).await;
    assert!(!is_error(&start));
    let start_env = envelope(&start);
    let sid = start_env["result"]["session_id"]
        .as_str()
        .expect("start_session response has result.session_id")
        .to_owned();
    assert_eq!(
        start_env["result"]["language_reference"]["content"],
        TYPEQL_LANGUAGE_REFERENCE
    );
    assert_eq!(
        start_env["result"]["language_reference"]["source"],
        TYPEQL_LANGUAGE_REFERENCE_SOURCE
    );
    assert_eq!(
        start_env["result"]["language_reference"]["sha256"],
        TYPEQL_LANGUAGE_REFERENCE_SHA256
    );

    let result = call(
        &client,
        "list_databases",
        with_sid(&sid, serde_json::Value::Null),
    )
    .await;
    assert!(!is_error(&result));
    let env = envelope(&result);
    assert!(env.get("session").is_some(), "session block present");
    assert!(env.get("next_moves").is_some(), "next_moves present");
    let moves = env["next_moves"].as_array().expect("array");
    assert!(!moves.is_empty(), "next_moves not empty");
    assert!(
        moves[0].as_str().unwrap().contains("get_schema"),
        "first next move mentions get_schema: {moves:?}"
    );
    assert!(
        env["result"]["databases"].is_array(),
        "databases list present"
    );
    assert!(
        env["result"].get("language_reference").is_none(),
        "list_databases does not return the bundled language reference"
    );

    let prompt = client
        .get_prompt(GetPromptRequestParams::new("typeql-language-reference"))
        .await
        .expect("language reference prompt");
    let prompt_text = match &prompt.messages.first().expect("prompt message").content {
        PromptMessageContent::Text { text } => text,
        other => panic!("expected text prompt, got {other:?}"),
    };
    assert_eq!(prompt_text, TYPEQL_LANGUAGE_REFERENCE);

    drop(client);
    let _ = server_handle.await;
}

// ---------- 2. schema gate blocks open_read until get_schema is called ----------

#[tokio::test]
async fn mcp_schema_gate_blocks_open_without_get_schema() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let result = call(
        &client,
        "open_read",
        with_sid(
            &sid,
            serde_json::json!({"database": "nonexistent_for_gate_test"}),
        ),
    )
    .await;
    assert!(
        is_error(&result),
        "open without get_schema should be an error"
    );
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "SCHEMA_NOT_READ");
    assert_eq!(env["error"]["retriable_in_same_tx"], false);
    let moves = env["next_moves"].as_array().expect("array");
    assert!(
        moves
            .iter()
            .any(|m| m.as_str().unwrap().contains("get_schema")),
        "next_moves directs to get_schema: {moves:?}"
    );

    drop(client);
    let _ = server_handle.await;
}

// ---------- 3. Full write lifecycle through MCP ----------

#[tokio::test]
async fn mcp_full_write_lifecycle() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    // Set up a fresh database out-of-band so this default-surface lifecycle
    // test does not depend on the optional admin tools being enabled.
    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_in_process_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();

    // Pre-create a schema (the MCP tool only exposes get_schema, not define).
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query(
            "define
               attribute name, value string;
               entity widget, owns name @card(1..1);",
        )
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // 1. get_schema — clears the gate
    let r = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(!is_error(&r), "get_schema should succeed");
    let env = envelope(&r);
    assert!(env["result"]["schema"].as_str().unwrap().contains("widget"));
    assert!(
        env["next_moves"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.as_str().unwrap().contains("open_write")),
        "next_moves points to open_*"
    );

    // 2. open_write
    let r = call(
        &client,
        "open_write",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(!is_error(&r));
    let env = envelope(&r);
    assert_eq!(env["session"]["transaction"]["kind"], "write");

    // 3. query insert
    let r = call(
        &client,
        "query",
        with_sid(
            &sid,
            serde_json::json!({"query": "insert $w isa widget, has name \"first\";"}),
        ),
    )
    .await;
    assert!(!is_error(&r), "insert should succeed: {r:?}");

    // 4. query verify (read inside the write tx)
    let r = call(
        &client,
        "query",
        with_sid(&sid, serde_json::json!({"query": "match $w isa widget, has name $n; fetch { \"name\": $n };"})),
    )
    .await;
    assert!(!is_error(&r));
    let env = envelope(&r);
    let answers = env["result"]["answers"].as_array().expect("answers array");
    assert_eq!(answers.len(), 1, "one widget visible inside the write tx");
    assert_eq!(answers[0]["name"], "first");

    // 5. commit
    let r = call(&client, "commit", with_sid(&sid, serde_json::Value::Null)).await;
    assert!(!is_error(&r), "commit should succeed: {r:?}");
    let env = envelope(&r);
    assert_eq!(env["result"]["committed"], true);
    assert!(
        env["session"]["transaction"].is_null(),
        "tx cleared on commit"
    );

    // 6. read_once verifies persistence
    let r = call(
        &client,
        "read_once",
        with_sid(&sid, serde_json::json!({"database": db, "query": "match $w isa widget, has name $n; fetch { \"name\": $n };"})),
    )
    .await;
    assert!(!is_error(&r), "read_once should succeed: {r:?}");
    let env = envelope(&r);
    let answers = env["result"]["answers"].as_array().expect("answers");
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["name"], "first");

    // Cleanup
    setup.delete_database(&db).await.unwrap();

    drop(client);
    let _ = server_handle.await;
}

// ---------- 3b. Second open_* while a tx is held returns TX_ALREADY_OPEN ----------

#[tokio::test]
async fn mcp_second_open_returns_tx_already_open() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!(
        "mcp_already_open_{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    let _ = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    let r = call(
        &client,
        "open_write",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(!is_error(&r), "first open_write succeeds");
    let first_tx_id = envelope(&r)["session"]["transaction"]["id"]
        .as_str()
        .expect("tx id present")
        .to_owned();

    // Now try to open a schema tx without committing or rolling back.
    let r = call(
        &client,
        "open_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(
        is_error(&r),
        "second open_* must be an error, not silent replacement"
    );
    let env = envelope(&r);
    assert_eq!(env["error"]["class"], "TX_ALREADY_OPEN");
    // TX_ALREADY_OPEN means the *original* tx survived untouched — the field
    // reflects "a tx is alive and continuable," not "this call retries."
    assert_eq!(env["error"]["retriable_in_same_tx"], true);
    // The original write tx must still be the live one.
    assert_eq!(env["session"]["transaction"]["id"], first_tx_id);
    assert_eq!(env["session"]["transaction"]["kind"], "write");

    let _ = call(&client, "rollback", with_sid(&sid, serde_json::Value::Null)).await;
    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}

// ---------- 3c. Parallel read_once on the same session serializes cleanly ----------

// Regression: two read_once calls dispatched concurrently on the same session
// must serialize at the handler boundary rather than racing on the server.
// Before the lock-scope fix, the per-session lock was held only across the
// precondition checks; both calls passed the `tx.is_some()` gate and opened
// transactions back-to-back on the server, with one's `read_once`-internal
// rollback colliding with the other's open and producing a TSV13
// (`TRANSIENT_CONFLICT`) on one of the two responses. Holding the lock
// across the full body (open → query → rollback) makes a second call wait
// for the first to finish, so neither call surfaces TSV13.
#[tokio::test]
async fn mcp_parallel_read_once_serializes() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!(
        "mcp_parallel_read_{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // Arm the schema-read gate.
    let _ = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;

    // Fire two read_once calls in parallel on the same session.
    let q = serde_json::json!({
        "database": db,
        "query": "match $w isa widget, has name $n; fetch { \"name\": $n };",
    });
    let (r1, r2) = tokio::join!(
        call(&client, "read_once", with_sid(&sid, q.clone())),
        call(&client, "read_once", with_sid(&sid, q.clone())),
    );

    // Both must succeed without TSV13. Either ordering is fine; what matters
    // is that neither surfaces TRANSIENT_CONFLICT (the symptom of the race).
    for (label, r) in [("first", &r1), ("second", &r2)] {
        let env = envelope(r);
        if is_error(r) {
            panic!("{label} parallel read_once errored — race not serialized: {env}");
        }
        // Sanity-check the read returned answers (empty array is fine — schema
        // is loaded, table is empty).
        assert!(
            env["result"]["answers"].is_array(),
            "{label}: answers array"
        );
    }

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}

// ---------- 3d. K_00000053 regression: read_once must NOT emit TSV3 ----------
//
// TypeDB 3.x rejects an explicit `Rollback` on a read transaction with
// `[TSV3] Read transactions cannot be rolled back, since they never contain
// writes.` The K_00000053 fix routes read-tx release through
// `Transaction::close()` instead of `.rollback()` so the server never
// receives a Rollback request for a read tx. The TSV3 was emitted as a
// swallowed `tracing::warn!` — the agent-facing response was successful, but
// the log line revealed the bug. This test captures the WARN-level tracing
// output for the duration of a `read_once` and asserts no TSV3 surfaces.
//
// If this ever fails, someone has reverted the close()-vs-rollback() rule.
// See DESIGN.md §5.0.1.

#[derive(Clone, Default)]
struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureSink;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureSink(self.0.clone())
    }
}

struct CaptureSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn mcp_read_once_does_not_emit_tsv3() {
    if !enabled() {
        return;
    }

    // Install a per-thread tracing subscriber that captures WARN+ into an
    // in-memory buffer for the duration of this test. `#[tokio::test]`
    // defaults to a current-thread runtime, so the spawned server task
    // shares this thread and inherits the subscriber.
    let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer = CaptureWriter(captured.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_no_tsv3_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    let _ = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;

    // Run several read_once calls sequentially — each opens and releases a
    // read tx. If any release goes through `rollback()` instead of `close()`,
    // the server rejects with TSV3 and the handler emits a swallowed warn.
    for _ in 0..5 {
        let r = call(
            &client,
            "read_once",
            with_sid(
                &sid,
                serde_json::json!({
                    "database": db,
                    "query": "match $w isa widget, has name $n; fetch { \"name\": $n };",
                }),
            ),
        )
        .await;
        assert!(!is_error(&r), "read_once succeeded: {r:?}");
    }

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;

    // Inspect captured logs. TSV3 is the canary; the swallowed warn message
    // is the additional signal that someone reverted the wire op.
    let logs = String::from_utf8(captured.lock().unwrap().clone()).expect("captured logs are utf8");
    assert!(
        !logs.contains("TSV3"),
        "K_00000053 regression: TSV3 emitted during read_once — read tx is \
         being released via rollback() instead of close(). Captured logs:\n{logs}"
    );
    assert!(
        !logs.contains("read_once close returned an error"),
        "read_once close path errored — unexpected. Captured logs:\n{logs}"
    );
    assert!(
        !logs.contains("read_once rollback returned an error"),
        "read_once still calls rollback on a read tx (old K_00000053 bug path). \
         Captured logs:\n{logs}"
    );
}

// ---------- 3e. read_once must drain the answer stream before close ----------
//
// Regression: do_read_once used to call `tx.close()` between `tx.query()`
// and `query_answer_to_json()`. The driver's QueryAnswer is a lazy gRPC
// stream tied to the live transaction, so draining after close aborts with
// TSV13 ("execution interrupted by a concurrent transaction close") — but
// only when the result set is large enough that the driver must pull
// batches beyond the one prefetched with the query response. Tiny one-row
// results (like the other tests here) pass either way, which is how the
// bug escaped CI while failing 100% of the time against real databases.
// This test inserts enough rows to force multi-batch streaming and asserts
// the rows actually come back.

#[tokio::test]
async fn mcp_read_once_returns_full_multibatch_result() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!(
        "mcp_read_once_drain_{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // 400 rows: comfortably under the test result_cap of 500, but well past
    // any single prefetch batch. Pad the names so the payload isn't trivial.
    const ROWS: usize = 400;
    let write_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Write)
        .await
        .unwrap();
    let mut insert = String::from("insert\n");
    for i in 0..ROWS {
        insert.push_str(&format!(
            "$w{i} isa widget, has name \"widget-{i:04}-{}\";\n",
            "x".repeat(64)
        ));
    }
    write_tx.query(&insert).await.unwrap();
    write_tx.commit().await.unwrap();

    let r = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(!is_error(&r), "get_schema should succeed: {r:?}");

    let r = call(
        &client,
        "read_once",
        with_sid(
            &sid,
            serde_json::json!({
                "database": db,
                "query": "match $w isa widget, has name $n; fetch { \"name\": $n };",
            }),
        ),
    )
    .await;
    assert!(
        !is_error(&r),
        "read_once must not abort mid-stream (TSV13 = tx closed before the \
         answer stream was drained): {r:?}"
    );
    let env = envelope(&r);
    let answers = env["result"]["answers"].as_array().expect("answers array");
    assert_eq!(
        answers.len(),
        ROWS,
        "read_once must return every row, not just the prefetched batch"
    );

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}

// ---------- 4. Parse error preserves the transaction (recoverable) ----------

#[tokio::test]
async fn mcp_parse_error_keeps_tx_open() {
    if !enabled() {
        return;
    }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_parse_err_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp_core::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // Walk through MCP to open a write tx
    let _ = call(
        &client,
        "get_schema",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    let r = call(
        &client,
        "open_write",
        with_sid(&sid, serde_json::json!({"database": db})),
    )
    .await;
    assert!(!is_error(&r));

    // Bad query
    let r = call(
        &client,
        "query",
        with_sid(&sid, serde_json::json!({"query": "not valid typeql"})),
    )
    .await;
    assert!(is_error(&r), "syntax error should be an error result");
    let env = envelope(&r);
    assert_eq!(env["error"]["class"], "PARSE_ERROR");
    assert_eq!(env["error"]["retriable_in_same_tx"], true);
    // Tx must still be reflected as open in the session block.
    assert_eq!(env["session"]["transaction"]["kind"], "write");
    let moves = env["next_moves"].as_array().unwrap();
    assert!(
        moves
            .iter()
            .any(|m| m.as_str().unwrap().contains("still open")),
        "next_moves tells agent the tx is still open: {moves:?}"
    );

    // Recover: same tx, good query.
    let r = call(
        &client,
        "query",
        with_sid(
            &sid,
            serde_json::json!({"query": "insert $w isa widget, has name \"recovered\";"}),
        ),
    )
    .await;
    assert!(!is_error(&r), "post-parse-error insert should succeed");
    let _ = call(&client, "rollback", with_sid(&sid, serde_json::Value::Null)).await;

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}
