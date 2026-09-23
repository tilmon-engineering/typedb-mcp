//! Live compatibility probes run by the pinned CE matrix.
mod common;

use typedb_mcp_core::{
    error::{ErrorClass, classify_typedb_error},
    typedb::{TxKind, query_answer_to_json},
};

#[tokio::test]
async fn annotation_round_trip() {
    if !common::smoke_enabled() {
        return;
    }
    let client = common::connect().await.expect("connect");
    let db = common::unique_database("compat_annotation");
    client.create_database(&db).await.expect("create");
    let tx = client
        .open_transaction(&db, TxKind::Schema)
        .await
        .expect("schema tx");
    tx.query("define attribute marker @doc(\"matrix annotation\"), value string; entity annotated, owns marker;").await.expect("define annotation");
    tx.commit().await.expect("commit");
    let schema = client.get_schema(&db).await.expect("schema");
    assert!(
        schema.contains("matrix annotation"),
        "annotation missing from schema: {schema}"
    );
    client.delete_database(&db).await.expect("cleanup");
}

#[test]
fn classifier_lifecycle_matrix_uses_only_established_classes() {
    assert!(classify_typedb_error("TSV9", "[TSV9] wrong transaction").retriable_in_same_tx());
    assert_eq!(
        classify_typedb_error("TSV7", "[TSV7] invalid syntax"),
        ErrorClass::ParseError
    );
    assert_eq!(
        classify_typedb_error("INF2", "[INF2] unknown type"),
        ErrorClass::TypeError
    );
    assert!(!classify_typedb_error("WEX1", "[WEX1] write failed").retriable_in_same_tx());
    assert!(!classify_typedb_error("DCT3", "[DCT3] commit failed").retriable_in_same_tx());
    // Ambiguous/new server codes intentionally remain unclassified; do not
    // assert lifecycle semantics for them without a live empirical fixture.
    assert_eq!(
        classify_typedb_error("NEW999", "[NEW999] unknown"),
        ErrorClass::Unclassified
    );
}

#[tokio::test]
async fn streaming_read_is_drained_before_transaction_close() {
    if !common::smoke_enabled() {
        return;
    }
    let client = common::connect().await.expect("connect");
    let db = common::unique_database("compat_stream");
    client.create_database(&db).await.expect("create");
    let tx = client
        .open_transaction(&db, TxKind::Schema)
        .await
        .expect("schema");
    tx.query("define attribute value, value string; entity item, owns value;")
        .await
        .expect("define");
    tx.commit().await.expect("schema commit");
    let tx = client
        .open_transaction(&db, TxKind::Write)
        .await
        .expect("write");
    tx.query("insert $x isa item, has value \"drain\";")
        .await
        .expect("insert");
    tx.commit().await.expect("write commit");
    let tx = client
        .open_transaction(&db, TxKind::Read)
        .await
        .expect("read");
    let answer = tx
        .query("match $x isa item; fetch { \"value\": $x.value }; ")
        .await
        .expect("stream");
    let json = query_answer_to_json(answer, 10)
        .await
        .expect("drain stream");
    assert!(
        json.answers
            .as_array()
            .is_some_and(|answers| !answers.is_empty())
    );
    // Live 2026-09-10 (CE 3.12.0): releasing a READ transaction via
    // rollback() is rejected with [TSV3] "Read transactions cannot be rolled
    // back". Reads are released with close() — exactly the DESIGN.md §3
    // reaper distinction; this probe pins that contract end-to-end.
    tx.close().await.expect("close after drain");
    client.delete_database(&db).await.expect("cleanup");
}

#[tokio::test]
async fn write_expression_rejection_probe_for_313() {
    if !common::smoke_enabled() {
        return;
    }
    let client = common::connect().await.expect("connect");
    // Live evidence 2026-09-10 (CE 3.12.0): an unsupported write expression
    // does NOT produce a clean server rejection there — the server closes
    // the connection ([CXN06] ServerConnectionIsClosedUnexpectedly). The
    // strict rejection assertion is therefore 3.13-only and opt-in.
    let version = client.startup_version().version.clone();
    if !version.starts_with("3.13") {
        eprintln!("skipping 3.13 rejection assertion against server {version}");
        return;
    }
    if std::env::var("TYPEDB_MCP_TEST_EXPECT_WRITE_EXPR_REJECT").as_deref() != Ok("1") {
        eprintln!("skipping: TYPEDB_MCP_TEST_EXPECT_WRITE_EXPR_REJECT!=1");
        return;
    }
    let db = common::unique_database("compat_expression");
    client.create_database(&db).await.expect("create");
    let tx = client
        .open_transaction(&db, TxKind::Schema)
        .await
        .expect("schema");
    tx.query("define attribute value, value integer; entity counter, owns value;")
        .await
        .expect("define");
    tx.commit().await.expect("schema commit");
    let tx = client
        .open_transaction(&db, TxKind::Write)
        .await
        .expect("write");
    let result = tx.query("insert $c isa counter, has value 1 + 2;").await;
    eprintln!("write-expression response: {result:?}");
    let error = result.expect_err("3.13 write expression should be rejected");
    assert!(
        !error.message().is_empty(),
        "rejection must explain the failure"
    );
    let _ = tx.rollback().await;
    // Cleanup must never fail the probe: the server may be in an awkward
    // state after a rejected write.
    let _ = client.delete_database(&db).await;
}
