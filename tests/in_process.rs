//! In-process MCP integration tests.
//!
//! These tests construct a `TypeDbMcp` server, connect a minimal client to
//! it via a `tokio::io::duplex` pair, and drive the eight tools end-to-end
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
    model::{CallToolRequestParams, CallToolResult, ClientInfo},
};
use typedb_mcp::{
    config::{Config, Credentials, LoggingConfig, ServerConfig, TypeDbConfig},
    handler::TypeDbMcp,
    session::SessionStore,
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
    fn get_info(&self) -> ClientInfo { ClientInfo::default() }
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
    let config = Arc::new(test_config());
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
async fn mint_sid(
    client: &rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
) -> String {
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

// ---------- 1. list_databases returns proper envelope ----------

#[tokio::test]
async fn mcp_list_databases_envelope() {
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let result = call(&client, "list_databases", with_sid(&sid, serde_json::Value::Null)).await;
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
    assert!(env["result"]["databases"].is_array(), "databases list present");

    drop(client);
    let _ = server_handle.await;
}

// ---------- 2. schema gate blocks open_read until get_schema is called ----------

#[tokio::test]
async fn mcp_schema_gate_blocks_open_without_get_schema() {
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let result = call(
        &client,
        "open_read",
        with_sid(&sid, serde_json::json!({"database": "nonexistent_for_gate_test"})),
    )
    .await;
    assert!(is_error(&result), "open without get_schema should be an error");
    let env = envelope(&result);
    assert_eq!(env["error"]["class"], "SCHEMA_NOT_READ");
    assert_eq!(env["error"]["retriable_in_same_tx"], false);
    let moves = env["next_moves"].as_array().expect("array");
    assert!(
        moves.iter().any(|m| m.as_str().unwrap().contains("get_schema")),
        "next_moves directs to get_schema: {moves:?}"
    );

    drop(client);
    let _ = server_handle.await;
}

// ---------- 3. Full write lifecycle through MCP ----------

#[tokio::test]
async fn mcp_full_write_lifecycle() {
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    // Set up a fresh database out-of-band so we don't need a `create_database`
    // tool. The MCP server doesn't expose database creation by design.
    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_in_process_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();

    // Pre-create a schema (the MCP tool only exposes get_schema, not define).
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp::typedb::TxKind::Schema)
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
    let r = call(&client, "get_schema", with_sid(&sid, serde_json::json!({"database": db}))).await;
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
    let r = call(&client, "open_write", with_sid(&sid, serde_json::json!({"database": db}))).await;
    assert!(!is_error(&r));
    let env = envelope(&r);
    assert_eq!(env["session"]["transaction"]["kind"], "write");

    // 3. query insert
    let r = call(
        &client,
        "query",
        with_sid(&sid, serde_json::json!({"query": "insert $w isa widget, has name \"first\";"})),
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
    assert!(env["session"]["transaction"].is_null(), "tx cleared on commit");

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
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_already_open_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    let _ = call(&client, "get_schema", with_sid(&sid, serde_json::json!({"database": db}))).await;
    let r = call(&client, "open_write", with_sid(&sid, serde_json::json!({"database": db}))).await;
    assert!(!is_error(&r), "first open_write succeeds");
    let first_tx_id = envelope(&r)["session"]["transaction"]["id"]
        .as_str()
        .expect("tx id present")
        .to_owned();

    // Now try to open a schema tx without committing or rolling back.
    let r = call(&client, "open_schema", with_sid(&sid, serde_json::json!({"database": db}))).await;
    assert!(is_error(&r), "second open_* must be an error, not silent replacement");
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
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_parallel_read_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // Arm the schema-read gate.
    let _ = call(&client, "get_schema", with_sid(&sid, serde_json::json!({"database": db}))).await;

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
            panic!(
                "{label} parallel read_once errored — race not serialized: {env}"
            );
        }
        // Sanity-check the read returned answers (empty array is fine — schema
        // is loaded, table is empty).
        assert!(env["result"]["answers"].is_array(), "{label}: answers array");
    }

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}

// ---------- 4. Parse error preserves the transaction (recoverable) ----------

#[tokio::test]
async fn mcp_parse_error_keeps_tx_open() {
    if !enabled() { return; }
    let (server_handle, client, _sessions) = connected_pair().await;
    let sid = mint_sid(&client).await;

    let setup = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .unwrap();
    let db = format!("mcp_parse_err_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    setup.create_database(&db).await.unwrap();
    let schema_tx = setup
        .open_transaction(&db, typedb_mcp::typedb::TxKind::Schema)
        .await
        .unwrap();
    schema_tx
        .query("define attribute name, value string; entity widget, owns name @card(1..1);")
        .await
        .unwrap();
    schema_tx.commit().await.unwrap();

    // Walk through MCP to open a write tx
    let _ = call(&client, "get_schema", with_sid(&sid, serde_json::json!({"database": db}))).await;
    let r = call(&client, "open_write", with_sid(&sid, serde_json::json!({"database": db}))).await;
    assert!(!is_error(&r));

    // Bad query
    let r = call(&client, "query", with_sid(&sid, serde_json::json!({"query": "not valid typeql"}))).await;
    assert!(is_error(&r), "syntax error should be an error result");
    let env = envelope(&r);
    assert_eq!(env["error"]["class"], "PARSE_ERROR");
    assert_eq!(env["error"]["retriable_in_same_tx"], true);
    // Tx must still be reflected as open in the session block.
    assert_eq!(env["session"]["transaction"]["kind"], "write");
    let moves = env["next_moves"].as_array().unwrap();
    assert!(
        moves.iter().any(|m| m.as_str().unwrap().contains("still open")),
        "next_moves tells agent the tx is still open: {moves:?}"
    );

    // Recover: same tx, good query.
    let r = call(
        &client,
        "query",
        with_sid(&sid, serde_json::json!({"query": "insert $w isa widget, has name \"recovered\";"})),
    )
    .await;
    assert!(!is_error(&r), "post-parse-error insert should succeed");
    let _ = call(&client, "rollback", with_sid(&sid, serde_json::Value::Null)).await;

    setup.delete_database(&db).await.unwrap();
    drop(client);
    let _ = server_handle.await;
}
