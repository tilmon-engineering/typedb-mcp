//! TypeDB driver wrapper. Uses the official `typedb-driver` crate (gRPC).
//!
//! Why the driver and not raw HTTP: type safety on the wire (Concept rows,
//! schema strings), and upstream-maintained protocol updates. The empirical
//! error stack we rely on in `crate::error` is preserved verbatim by the
//! driver — `Error::Server(ServerError)` carries the `[CNT6] → [WEX1] → …`
//! chain via its `Display`/`message()`.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use serde::Serialize;
use typedb_driver::{
    Addresses, Credentials, DriverOptions, DriverTlsConfig, TransactionType, TypeDBDriver,
    answer::QueryAnswer,
};

use crate::config::ConnectionSettings;
use crate::connection::{self, ServerVersionInfo};
use crate::error::InternalError;
use crate::migration::MigrationBackend;
use std::path::Path;

/// Our internal kind enum, kept distinct from the driver's `TransactionType`
/// so it stays serializable in the agent envelope and stable across driver
/// upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TxKind {
    Read,
    Write,
    Schema,
}

impl From<TxKind> for TransactionType {
    fn from(k: TxKind) -> Self {
        match k {
            TxKind::Read => TransactionType::Read,
            TxKind::Write => TransactionType::Write,
            TxKind::Schema => TransactionType::Schema,
        }
    }
}

/// Re-export for callers that need to hold a driver `Transaction` directly
/// (e.g. inside [`crate::session::OpenTx`]).
pub use typedb_driver::transaction::Transaction as DriverTransaction;

/// Wraps an open driver connection.
#[derive(Debug)]
pub struct TypeDbClient {
    driver: Arc<TypeDBDriver>,
    startup_version: ServerVersionInfo,
}

impl TypeDbClient {
    /// Connect to TypeDB.
    ///
    /// `address` is in `host:port` form (e.g. `127.0.0.1:1729`).
    #[allow(clippy::result_large_err)]
    pub async fn connect(
        address: &str,
        username: &str,
        password: &str,
        tls_enabled: bool,
    ) -> Result<Self, InternalError> {
        let addresses = Addresses::try_from_address_str(address).map_err(InternalError::from)?;
        let credentials = Credentials::new(username, password);
        let tls = if tls_enabled {
            DriverTlsConfig::enabled_with_native_root_ca()
        } else {
            DriverTlsConfig::disabled()
        };
        let options = DriverOptions::new(tls);
        let driver = TypeDBDriver::new(addresses, credentials, options)
            .await
            .map_err(InternalError::from)?;
        #[allow(clippy::result_large_err)]
        let startup_version = driver
            .server_version()
            .await
            .map_err(InternalError::from)
            .and_then(|v| connection::server_version_info(v.distribution(), v.version()))?;
        Ok(Self {
            driver: Arc::new(driver),
            startup_version,
        })
    }

    /// Connect using validated settings and verify the server version before returning.
    #[allow(clippy::result_large_err)]
    pub async fn connect_with_settings(
        settings: &crate::config::ConnectionSettings,
        username: &str,
        password: &str,
    ) -> Result<Self, InternalError> {
        let driver = connection::connect(settings, username, password).await?;
        let v = driver.server_version().await.map_err(InternalError::from)?;
        let startup_version = connection::server_version_info(v.distribution(), v.version())?;
        Ok(Self {
            driver: Arc::new(driver),
            startup_version,
        })
    }

    pub fn startup_version(&self) -> &ServerVersionInfo {
        &self.startup_version
    }

    /// Bounded, read-only connection diagnostics. The deadline bounds waiting for the
    /// response; dropping a future does not promise cancellation of an underlying RPC.
    #[allow(clippy::result_large_err)]
    pub async fn diagnostics(
        &self,
        deadline: Duration,
        expose_topology: bool,
    ) -> Result<crate::diagnostics::DiagnosticReport, InternalError> {
        crate::diagnostics::collect(
            &self.driver,
            self.startup_version.clone(),
            deadline,
            expose_topology,
        )
        .await
    }

    /// List database names.
    #[allow(clippy::result_large_err)]
    pub async fn list_databases(&self) -> Result<Vec<String>, InternalError> {
        let dbs = self
            .driver
            .databases()
            .all()
            .await
            .map_err(InternalError::from)?;
        Ok(dbs.into_iter().map(|d| d.name().to_owned()).collect())
    }

    /// Fetch the full TypeQL `define` source for a database.
    #[allow(clippy::result_large_err)]
    pub async fn get_schema(&self, name: &str) -> Result<String, InternalError> {
        let db = self
            .driver
            .databases()
            .get(name)
            .await
            .map_err(InternalError::from)?;
        db.schema().await.map_err(InternalError::from)
    }

    /// Open a new transaction. The returned value is the live driver
    /// `Transaction` — the caller is responsible for storing it for the
    /// lifetime of the agent session.
    #[allow(clippy::result_large_err)]
    pub async fn open_transaction(
        &self,
        database: &str,
        kind: TxKind,
    ) -> Result<DriverTransaction, InternalError> {
        self.driver
            .transaction(database, kind.into())
            .await
            .map_err(InternalError::from)
    }

    /// Force-close the driver (used at shutdown).
    pub fn force_close(&self) {
        let _ = self.driver.force_close();
    }

    /// Create a database. Used by tests and optional admin MCP tools; the
    /// admin tools are disabled by default and must be operator-enabled.
    #[allow(clippy::result_large_err)]
    pub async fn create_database(&self, name: &str) -> Result<(), InternalError> {
        self.driver
            .databases()
            .create(name)
            .await
            .map_err(InternalError::from)
    }

    /// Delete a database. Used by tests and optional admin MCP tools; the
    /// admin tools are disabled by default and require explicit confirmation.
    #[allow(clippy::result_large_err)]
    pub async fn delete_database(&self, name: &str) -> Result<(), InternalError> {
        let db = self
            .driver
            .databases()
            .get(name)
            .await
            .map_err(InternalError::from)?;
        db.delete().await.map_err(InternalError::from)
    }
}

/// Production synchronous adapter used exclusively by the dedicated migration
/// worker. It owns a private Tokio runtime and a SEPARATELY constructed
/// migration driver (primary_failover_retries=0) that lives entirely on that
/// runtime. TypeDB's gRPC channels are runtime-affine: sharing the main
/// client's driver from a foreign runtime kills the worker mid-job (observed
/// live 2026-09-10), so the migration driver must never be the shared one.
/// Construct on a plain thread (the worker start qualifies) — never inside a
/// Tokio runtime context.
pub struct TypeDbMigrationBackend {
    rt: Arc<tokio::runtime::Runtime>,
    driver: Arc<TypeDBDriver>,
}
impl TypeDbMigrationBackend {
    pub fn connect(
        settings: &ConnectionSettings,
        username: &str,
        password: &str,
    ) -> Result<Self, String> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?,
        );
        let driver = rt
            .block_on(connection::connect_migration(settings, username, password))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            rt,
            driver: Arc::new(driver),
        })
    }
    fn block<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, typedb_driver::Error>>,
    ) -> Result<T, String> {
        self.rt.block_on(fut).map_err(|e| e.to_string())
    }
}
impl MigrationBackend for TypeDbMigrationBackend {
    fn export_database(&self, database: &str, schema: &Path, data: &Path) -> Result<(), String> {
        let db = self.block(self.driver.databases().get(database))?;
        self.block(db.export_to_file(schema, data))
    }
    fn import_database(&self, database: &str, schema: &Path, data: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(schema).map_err(|e| e.to_string())?;
        self.block(
            self.driver
                .databases()
                .import_from_file(database.to_owned(), content, data),
        )
    }
    fn database_exists(&self, database: &str) -> Result<bool, String> {
        Ok(self.block(self.driver.databases().get(database)).is_ok())
    }
}

// -------------------------------------------------------------------------
// QueryAnswer → JSON
//
// The agent wants something it can read; the driver gives us a streaming
// answer envelope. We materialize it eagerly here (subject to a result
// cap that the handler enforces). Streams that exceed the cap are
// truncated and an error is surfaced.
// -------------------------------------------------------------------------

/// Materialize a [`QueryAnswer`] into a JSON value capped at `max_answers`.
///
/// Returns `Ok((json, count))` on success and `Err(InternalError)` if the
/// stream itself errored. The handler is responsible for translating
/// `count > max_answers` into a `RESULT_LIMIT_EXCEEDED` envelope.
#[allow(clippy::result_large_err)]
pub async fn query_answer_to_json(
    answer: QueryAnswer,
    max_answers: usize,
) -> Result<QueryAnswerJson, InternalError> {
    let query_type = format!("{:?}", answer.get_query_type()).to_lowercase();
    match answer {
        QueryAnswer::Ok(_) => Ok(QueryAnswerJson {
            query_type,
            answer_type: "ok".into(),
            answers: serde_json::Value::Null,
            truncated: false,
        }),
        QueryAnswer::ConceptRowStream(_header, mut stream) => {
            let mut rows = Vec::new();
            let mut truncated = false;
            // We pull one past the cap so we can detect overflow without
            // changing the size guarantees of the returned vector.
            while let Some(row) = stream.next().await {
                let row = row.map_err(InternalError::from)?;
                if rows.len() >= max_answers {
                    truncated = true;
                    break;
                }
                rows.push(concept_row_to_json(&row));
            }
            Ok(QueryAnswerJson {
                query_type,
                answer_type: "conceptRows".into(),
                answers: serde_json::Value::Array(rows),
                truncated,
            })
        }
        QueryAnswer::ConceptDocumentStream(_header, mut stream) => {
            let mut docs = Vec::new();
            let mut truncated = false;
            while let Some(doc) = stream.next().await {
                let doc = doc.map_err(InternalError::from)?;
                if docs.len() >= max_answers {
                    truncated = true;
                    break;
                }
                docs.push(driver_json_to_serde(doc.into_json()));
            }
            Ok(QueryAnswerJson {
                query_type,
                answer_type: "conceptDocuments".into(),
                answers: serde_json::Value::Array(docs),
                truncated,
            })
        }
    }
}

#[derive(Debug)]
pub struct QueryAnswerJson {
    pub query_type: String,
    pub answer_type: String,
    pub answers: serde_json::Value,
    pub truncated: bool,
}

impl QueryAnswerJson {
    pub fn into_value(self) -> serde_json::Value {
        serde_json::json!({
            "queryType": self.query_type,
            "answerType": self.answer_type,
            "answers": self.answers,
        })
    }
}

fn concept_row_to_json(row: &typedb_driver::answer::ConceptRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for name in row.get_column_names() {
        let cell = match row.get(name) {
            Ok(Some(concept)) => concept_to_json(concept),
            _ => serde_json::Value::Null,
        };
        obj.insert(name.clone(), cell);
    }
    serde_json::Value::Object(obj)
}

/// Render a [`Concept`] as a structured JSON object. The shape is stable
/// across kinds and is intended to be agent-readable:
///
/// - Entity / Relation: `{ "kind", "type", "iid" }`
/// - Attribute:          `{ "kind", "type", "valueType", "value" }`
/// - Value:              `{ "kind", "valueType", "value" }`
/// - *Type concepts:     `{ "kind", "label" }`
fn concept_to_json(concept: &typedb_driver::concept::Concept) -> serde_json::Value {
    use typedb_driver::concept::Concept;
    match concept {
        Concept::Entity(e) => {
            let type_label = e.type_().map(|t| t.label().to_owned());
            serde_json::json!({
                "kind": "entity",
                "type": type_label,
                "iid": format!("{}", e.iid),
            })
        }
        Concept::Relation(r) => {
            let type_label = r.type_().map(|t| t.label().to_owned());
            serde_json::json!({
                "kind": "relation",
                "type": type_label,
                "iid": format!("{}", r.iid),
            })
        }
        Concept::Attribute(a) => {
            let type_label = a.type_().map(|t| t.label().to_owned());
            serde_json::json!({
                "kind": "attribute",
                "type": type_label,
                "valueType": a.value.get_type_name(),
                "value": value_to_json(&a.value),
            })
        }
        Concept::Value(v) => {
            serde_json::json!({
                "kind": "value",
                "valueType": v.get_type_name(),
                "value": value_to_json(v),
            })
        }
        Concept::EntityType(t) => {
            serde_json::json!({ "kind": "entityType", "label": t.label() })
        }
        Concept::RelationType(t) => {
            serde_json::json!({ "kind": "relationType", "label": t.label() })
        }
        Concept::RoleType(t) => {
            serde_json::json!({ "kind": "roleType", "label": t.label() })
        }
        Concept::AttributeType(t) => {
            let value_type = t.value_type().map(|vt| vt.name().to_owned());
            serde_json::json!({
                "kind": "attributeType",
                "label": t.label(),
                "valueType": value_type,
            })
        }
    }
}

/// Render a [`Value`] as a JSON primitive (or stringified form where JSON
/// has no native type — decimals, dates, durations, structs).
///
/// Strings render through `Value::Display` so dates use ISO `YYYY-MM-DD`,
/// datetimes use `%FT%T%.9f`, and decimals/durations use the driver's
/// canonical forms. The string-typed variants are never quoted.
fn value_to_json(v: &typedb_driver::concept::Value) -> serde_json::Value {
    use typedb_driver::concept::Value as V;
    match v {
        V::Boolean(b) => serde_json::Value::Bool(*b),
        V::Integer(i) => serde_json::Value::Number((*i).into()),
        V::Double(d) => serde_json::Number::from_f64(*d)
            .map(serde_json::Value::Number)
            // JSON cannot represent NaN/±Inf — stringify so the agent
            // still gets *something* meaningful.
            .unwrap_or_else(|| serde_json::Value::String(format!("{d}"))),
        V::String(s) => serde_json::Value::String(s.clone()),
        V::Decimal(d) => serde_json::Value::String(format!("{d}")),
        V::Date(d) => serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
        V::Datetime(dt) => serde_json::Value::String(dt.format("%FT%T%.9f").to_string()),
        V::DatetimeTZ(dt) => serde_json::Value::String(format!("{dt}")),
        V::Duration(d) => serde_json::Value::String(format!("{d}")),
        V::Struct(s, type_name) => {
            let mut fields = serde_json::Map::new();
            for (k, maybe_v) in s.fields() {
                fields.insert(
                    k.clone(),
                    maybe_v
                        .as_ref()
                        .map(value_to_json)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            serde_json::json!({ "structType": type_name, "fields": fields })
        }
    }
}

fn driver_json_to_serde(j: typedb_driver::answer::JSON) -> serde_json::Value {
    use typedb_driver::answer::JSON as J;
    match j {
        J::Null => serde_json::Value::Null,
        J::Boolean(b) => serde_json::Value::Bool(b),
        J::Number(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        J::String(s) => serde_json::Value::String(s.into_owned()),
        J::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(driver_json_to_serde).collect())
        }
        J::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k.into_owned(), driver_json_to_serde(v)))
                .collect(),
        ),
    }
}

/// Convenience: a small request timeout helper preserved from the prior
/// HTTP design. Currently unused by the driver wrapper (driver manages its
/// own connection timeouts via DriverOptions), kept here as a placeholder
/// so config still compiles.
#[allow(dead_code)]
pub fn placeholder_timeout(_d: Duration) {}
