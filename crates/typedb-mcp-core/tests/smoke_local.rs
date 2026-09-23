//! Smoke test against a local TypeDB on 127.0.0.1:1729.
//!
//! Skipped unless the env var `TYPEDB_MCP_SMOKE=1` is set, since it needs a
//! running TypeDB. Run with:
//!
//!     TYPEDB_MCP_SMOKE=1 cargo test --test smoke_local -- --nocapture

use typedb_mcp_core::typedb::TxKind;
mod common;

fn enabled() -> bool {
    common::smoke_enabled()
}

#[tokio::test]
async fn readiness_connects() {
    // Readiness probe for the compatibility matrix: asserts only that a
    // connection authenticates and list_databases succeeds. A fresh
    // disposable CE container legitimately has zero databases, so this must
    // NOT require a non-empty list (see scripts/compatibility_matrix.sh).
    if !enabled() {
        eprintln!("skipping: set TYPEDB_MCP_SMOKE=1 to enable");
        return;
    }
    let client = common::connect().await.expect("connect");
    let names = client.list_databases().await.expect("list_databases");
    eprintln!("ready; databases: {names:?}");
}

#[tokio::test]
async fn list_databases_and_get_schema() {
    if !enabled() {
        eprintln!("skipping: set TYPEDB_MCP_SMOKE=1 to enable");
        return;
    }

    let client = common::connect().await.expect("connect");

    let names = client.list_databases().await.expect("list_databases");
    eprintln!("databases: {names:?}");

    // Self-contained: create a throwaway database instead of requiring the
    // caller to pre-create one (fresh disposable containers have none).
    let db = format!("smoke_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    client
        .create_database(&db)
        .await
        .expect("create scratch database");

    let schema = client.get_schema(&db).await.expect("get_schema");
    eprintln!(
        "schema for {db} (first 200 chars): {}",
        schema.chars().take(200).collect::<String>()
    );

    // Open a read tx and run a trivial query to confirm the live Transaction works.
    let tx = client
        .open_transaction(&db, TxKind::Read)
        .await
        .expect("open_transaction");
    let answer = tx
        .query("match $x isa $t; limit 1; fetch { \"type\": $t };")
        .await;
    eprintln!("query result: {:?}", answer.is_ok());
    tx.close().await.expect("close read tx");
    let _ = client.delete_database(&db).await;
}
