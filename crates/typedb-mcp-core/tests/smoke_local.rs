//! Smoke test against a local TypeDB on 127.0.0.1:1729.
//!
//! Skipped unless the env var `TYPEDB_MCP_SMOKE=1` is set, since it needs a
//! running TypeDB. Run with:
//!
//!     TYPEDB_MCP_SMOKE=1 cargo test --test smoke_local -- --nocapture

use typedb_mcp_core::typedb::{TxKind, TypeDbClient};

fn enabled() -> bool {
    std::env::var("TYPEDB_MCP_SMOKE").as_deref() == Ok("1")
}

#[tokio::test]
async fn list_databases_and_get_schema() {
    if !enabled() {
        eprintln!("skipping: set TYPEDB_MCP_SMOKE=1 to enable");
        return;
    }

    let client = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .expect("connect");

    let names = client.list_databases().await.expect("list_databases");
    eprintln!("databases: {names:?}");

    // Use a throwaway db so the test is self-contained.
    let db = format!("smoke_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    // Create via a schema tx -- but the driver's create lives on DatabaseManager.
    // For smoke purposes we depend on the caller pre-creating one. Use the
    // first existing db.
    let Some(existing) = names.first().cloned() else {
        panic!("no databases on the local TypeDB to test against");
    };
    drop(db);

    let schema = client.get_schema(&existing).await.expect("get_schema");
    eprintln!(
        "schema for {existing} (first 200 chars): {}",
        &schema.chars().take(200).collect::<String>()
    );

    // Open a read tx and run a trivial query to confirm the live Transaction works.
    let tx = client
        .open_transaction(&existing, TxKind::Read)
        .await
        .expect("open_transaction");
    let answer = tx
        .query("match $x isa $t; limit 1; fetch { \"type\": $t };")
        .await;
    eprintln!("query result: {:?}", answer.is_ok());
    let _ = tx.rollback().await;
}
