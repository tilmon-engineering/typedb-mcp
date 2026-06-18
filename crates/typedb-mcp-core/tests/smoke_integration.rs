//! Deeper integration smoke: exercises the data-path safety contract
//! end-to-end against a local TypeDB.
//!
//! Each test creates and tears down its own database, so failed runs
//! shouldn't pollute the server. Run with:
//!
//!     TYPEDB_MCP_SMOKE=1 cargo test --test smoke_integration -- --nocapture
//!
//! The tests *verify the design contract from DESIGN.md §5*:
//!   • parse / type / wrong-tx errors leave the transaction OPEN
//!   • write-pipeline errors ABORT the transaction (next call → IdleTimeout)
//!   • commit-time errors close the transaction
//!   • result cap discards and surfaces RESULT_LIMIT_EXCEEDED
//!
//! This is the closest empirical check we have that the driver-based
//! error classifier matches the live TypeDB behavior.

use typedb_mcp_core::{
    error::ErrorClass,
    typedb::{TxKind, TypeDbClient, query_answer_to_json},
};

fn enabled() -> bool {
    std::env::var("TYPEDB_MCP_SMOKE").as_deref() == Ok("1")
}

async fn fresh_client_and_db(prefix: &str) -> (TypeDbClient, String) {
    let client = TypeDbClient::connect("127.0.0.1:1729", "admin", "password", false)
        .await
        .expect("connect");
    let db = format!("{prefix}_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    client.create_database(&db).await.expect("create");
    (client, db)
}

async fn define_basic_schema(client: &TypeDbClient, db: &str) {
    let tx = client
        .open_transaction(db, TxKind::Schema)
        .await
        .expect("open schema tx");
    let _ = tx
        .query(
            "define
              attribute name, value string;
              attribute age, value integer;
              attribute email, value string @regex(\"^.+@.+\\\\..+$\");
              entity person, owns name @card(1..1), owns age, owns email @card(0..3);",
        )
        .await
        .expect("define");
    tx.commit().await.expect("commit schema");
}

// ---------- 1. happy path: write → commit → read back ----------

#[tokio::test]
async fn write_commit_read_cycle() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_write").await;
    define_basic_schema(&client, &db).await;

    // write
    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    tx.query("insert $p isa person, has name \"Alice\";")
        .await
        .expect("insert ok");
    tx.commit().await.expect("commit ok");

    // read back
    let tx = client.open_transaction(&db, TxKind::Read).await.unwrap();
    let answer = tx
        .query("match $p isa person, has name $n; fetch { \"name\": $n };")
        .await
        .expect("read ok");
    let json = query_answer_to_json(answer, 500)
        .await
        .expect("materialize");
    let v = json.into_value();
    eprintln!("read-back: {v}");
    assert!(
        v.get("answers")
            .and_then(|a| a.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    );
    let _ = tx.rollback().await;

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 2. parse error survives the tx ----------

#[tokio::test]
async fn parse_error_leaves_tx_open() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_parse").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    let bad = tx.query("this is not valid typeql").await;
    let bad_err = bad.expect_err("parse should fail");
    let class = typedb_mcp_core::error::classify_driver_error(&bad_err);
    eprintln!("parse-error class: {class:?}");
    assert_eq!(class, ErrorClass::ParseError);
    assert!(
        class.retriable_in_same_tx(),
        "DESIGN.md §5 says parse errors survive"
    );

    // Tx still usable.
    tx.query("insert $p isa person, has name \"Bob\";")
        .await
        .expect("post-parse insert should succeed");
    tx.commit().await.expect("commit");

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 3. type-inference error survives the tx ----------

#[tokio::test]
async fn type_error_leaves_tx_open() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_type").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    let bad = tx
        .query("match $x isa nonexistent_type; fetch { $x.* };")
        .await;
    let class = typedb_mcp_core::error::classify_driver_error(&bad.expect_err("type err"));
    eprintln!("type-error class: {class:?}");
    assert_eq!(class, ErrorClass::TypeError);
    assert!(class.retriable_in_same_tx());

    // Tx still usable.
    tx.query("insert $p isa person, has name \"Eve\";")
        .await
        .expect("post-type-err insert");
    tx.commit().await.expect("commit");

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 4. write-pipeline error ABORTS the tx ----------

#[tokio::test]
async fn write_pipeline_error_aborts_tx() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_write_fail").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    let bad = tx
        .query("insert $p isa person, has name \"Bob\", has email \"not-an-email\";")
        .await;
    let class = typedb_mcp_core::error::classify_driver_error(&bad.expect_err("regex violation"));
    eprintln!("write-fail class: {class:?}");
    assert_eq!(class, ErrorClass::WriteFailed);
    assert!(
        !class.retriable_in_same_tx(),
        "DESIGN.md §5 says write-pipeline errors are fatal"
    );

    // Tx should be dead. What does the driver actually report?
    let follow = tx.query("insert $p isa person, has name \"Carol\";").await;
    let follow_err = follow.expect_err("tx must be dead");
    eprintln!("follow-up err (Display): {follow_err}");
    eprintln!("follow-up err (Debug):   {follow_err:?}");
    eprintln!("follow-up err code:      {}", follow_err.code());
    eprintln!("follow-up err message:   {}", follow_err.message());
    let follow_class = typedb_mcp_core::error::classify_driver_error(&follow_err);
    eprintln!("follow-up class:         {follow_class:?}");
    // Either variant is acceptable from a *safety* perspective: both signal
    // "the tx is gone, open a new one." The classifier just needs to map
    // them to a class whose `retriable_in_same_tx` is false.
    assert!(
        !follow_class.retriable_in_same_tx(),
        "follow-up must be classified as fatal (got {follow_class:?})"
    );

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 4b. write-pipeline abort DISCARDS prior writes in same tx ----------

// K_00000052(2): empirical proof for the `next_moves` line that says every
// write already submitted in the aborted tx is discarded. Insert a valid
// person, then trigger a regex-violating insert in the same tx, then open a
// fresh tx and assert the first person is NOT found.
//
// No integration fixture for TSV13 (the post-commit concurrent-rollback
// transient): it's a driver-internal race we can't deterministically trigger
// from a single-client smoke test. The classifier and `next_moves` shape are
// covered by unit tests in `src/error.rs` and `src/handler.rs`; pretending we
// have a live fixture for a race we can't provoke would be worse than not
// having one.
#[tokio::test]
async fn write_pipeline_abort_discards_prior_writes() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_write_discard").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    tx.query("insert $p isa person, has name \"Dora\";")
        .await
        .expect("prior insert accepted by the tx");
    let bad = tx
        .query("insert $p isa person, has name \"Eli\", has email \"not-an-email\";")
        .await;
    let class = typedb_mcp_core::error::classify_driver_error(&bad.expect_err("regex violation"));
    assert_eq!(class, ErrorClass::WriteFailed);

    // The aborted tx is gone; the question is whether Dora survived. She must not.
    let tx = client.open_transaction(&db, TxKind::Read).await.unwrap();
    let answer = tx
        .query("match $p isa person, has name \"Dora\"; fetch { \"name\": $p.name };")
        .await
        .expect("read ok");
    let json = query_answer_to_json(answer, 500)
        .await
        .expect("materialize");
    let v = json.into_value();
    let empty = v
        .get("answers")
        .and_then(|a| a.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false);
    assert!(
        empty,
        "Dora must have been discarded with the aborted tx; got {v}"
    );
    let _ = tx.rollback().await;

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 5. commit-time cardinality error ----------

#[tokio::test]
async fn commit_time_cardinality_fails_at_commit() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_commit_fail").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    // Insert a person *without* name — violates @card(1..1), but only at commit.
    tx.query("insert $p isa person, has age 30;")
        .await
        .expect("insert accepted");
    let commit_err = tx.commit().await.expect_err("commit must fail");
    let class = typedb_mcp_core::error::classify_driver_error(&commit_err);
    eprintln!("commit-fail class: {class:?}");
    assert_eq!(class, ErrorClass::CommitFailed);

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 6. wrong-tx-type rejection survives ----------

#[tokio::test]
async fn wrong_tx_type_is_recoverable() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_wrong_tx").await;
    define_basic_schema(&client, &db).await;

    let tx = client.open_transaction(&db, TxKind::Read).await.unwrap();
    let bad = tx.query("insert $p isa person, has name \"Frank\";").await;
    let class = typedb_mcp_core::error::classify_driver_error(&bad.expect_err("write under read"));
    eprintln!("wrong-tx class: {class:?}");
    assert_eq!(class, ErrorClass::WrongTxType);
    assert!(class.retriable_in_same_tx());

    // Read tx still usable.
    let ok = tx.query("match $p isa person; fetch { $p.* };").await;
    assert!(ok.is_ok(), "read tx should survive wrong-tx-type rejection");
    let _ = tx.rollback().await;

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 7b. concept_row JSON is structured ----------

#[tokio::test]
async fn concept_row_emits_structured_json() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_row").await;
    define_basic_schema(&client, &db).await;

    // Insert with mixed concept kinds in the projection.
    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    tx.query("insert $p isa person, has name \"Alice\", has age 33;")
        .await
        .expect("insert");
    tx.commit().await.expect("commit");

    // `match ... ;` (no `fetch`) returns a ConceptRowStream — that's the path
    // our concept_row_to_json handles.
    let tx = client.open_transaction(&db, TxKind::Read).await.unwrap();
    let answer = tx
        .query("match $p isa person, has name $n, has age $a;")
        .await
        .expect("row query");
    let json = query_answer_to_json(answer, 500)
        .await
        .expect("materialize");
    let _ = tx.rollback().await;
    let answer_type = json.answer_type.clone();
    let answers = json.answers.clone();
    eprintln!(
        "conceptRow JSON:\n{}",
        serde_json::to_string_pretty(&json.into_value()).unwrap()
    );
    assert_eq!(answer_type, "conceptRows");
    let answers = answers.as_array().expect("array");
    let row = answers
        .first()
        .expect("one row")
        .as_object()
        .expect("object row");

    // Person concept — should be an Entity with a type label.
    let person = row
        .get("p")
        .expect("p column")
        .as_object()
        .expect("p is object");
    assert_eq!(person.get("kind").and_then(|v| v.as_str()), Some("entity"));
    assert_eq!(person.get("type").and_then(|v| v.as_str()), Some("person"));
    assert!(
        person
            .get("iid")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("0x"))
    );

    // Name attribute — string value rendered as JSON string.
    let name = row
        .get("n")
        .expect("n column")
        .as_object()
        .expect("n is object");
    assert_eq!(name.get("kind").and_then(|v| v.as_str()), Some("attribute"));
    assert_eq!(name.get("type").and_then(|v| v.as_str()), Some("name"));
    assert_eq!(
        name.get("valueType").and_then(|v| v.as_str()),
        Some("string")
    );
    assert_eq!(name.get("value").and_then(|v| v.as_str()), Some("Alice"));

    // Age attribute — integer value rendered as JSON number.
    let age = row
        .get("a")
        .expect("a column")
        .as_object()
        .expect("a is object");
    assert_eq!(age.get("kind").and_then(|v| v.as_str()), Some("attribute"));
    assert_eq!(age.get("type").and_then(|v| v.as_str()), Some("age"));
    assert_eq!(
        age.get("valueType").and_then(|v| v.as_str()),
        Some("integer")
    );
    assert_eq!(age.get("value").and_then(|v| v.as_i64()), Some(33));

    client.delete_database(&db).await.expect("cleanup");
}

// ---------- 7. result cap discards over-cap streams ----------

#[tokio::test]
async fn result_cap_truncates_and_signals() {
    if !enabled() {
        return;
    }
    let (client, db) = fresh_client_and_db("smoke_cap").await;

    // Lightweight schema: just an entity with name.
    let tx = client.open_transaction(&db, TxKind::Schema).await.unwrap();
    tx.query(
        "define
           attribute name, value string;
           entity widget, owns name @card(1..1);",
    )
    .await
    .expect("define");
    tx.commit().await.expect("commit schema");

    // Insert 10 widgets.
    let tx = client.open_transaction(&db, TxKind::Write).await.unwrap();
    let mut q = String::from("insert ");
    for i in 0..10 {
        q.push_str(&format!("$w{i} isa widget, has name \"w{i:02}\"; "));
    }
    tx.query(&q).await.expect("bulk insert");
    tx.commit().await.expect("commit widgets");

    // Query with cap = 3.
    let tx = client.open_transaction(&db, TxKind::Read).await.unwrap();
    let answer = tx
        .query("match $w isa widget, has name $n; fetch { \"name\": $n };")
        .await
        .expect("read");
    let json = query_answer_to_json(answer, 3).await.expect("materialize");
    eprintln!(
        "truncated={} answers_len={}",
        json.truncated,
        json.answers.as_array().map(|a| a.len()).unwrap_or(0)
    );
    assert!(json.truncated, "10 widgets > cap of 3 should truncate");
    assert_eq!(
        json.answers.as_array().map(|a| a.len()).unwrap_or(0),
        3,
        "exactly cap rows"
    );
    let _ = tx.rollback().await;

    client.delete_database(&db).await.expect("cleanup");
}
