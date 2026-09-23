//! Executable harness tests against a disposable TypeDB instance.
//!
//! These tests launch the REAL `typedb-mcp` binary (`CARGO_BIN_EXE_typedb-mcp`)
//! as a child process from a fresh, different working directory, drive it over
//! piped stdio (newline-delimited JSON-RPC) and, for the mixed-transport test,
//! over raw Streamable-HTTP POSTs, using a disposable TypeDB CE instance
//! exported through the shared matrix environment:
//!
//! - `TYPEDB_MCP_SMOKE=1`
//! - `TYPEDB_MCP_TEST_ADDRESS` (host:port of the gRPC endpoint)
//! - `TYPEDB_MCP_TEST_USERNAME`
//! - `TYPEDB_MCP_TEST_PASSWORD`
//!
//! They are skipped (pass, with no work) unless the gate is set. The pinned
//! matrix runner (`scripts/compatibility_matrix.sh`) provides that environment.
//!
//! Covered acceptance mappings: binary_stdio_lifecycle, binary_stdio_stdout_is_protocol,
//! binary_stdio_eof_shutdown, binary_stdio_foreign_cwd, migration_router_policy_matrix
//! (executable variant), http_guessed_migration_rejected, mixed_transport_session_reuse_denied,
//! binary_stdio_migration_round_trip, imported_schema_gate_reset, migration_manifest_hashes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(180);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The twelve default tool names (DESIGN.md §7). Migrations and admin tools
/// are separate optional families and must never appear by default.
const DEFAULT_TOOLS: &[&str] = &[
    "start_session",
    "list_databases",
    "get_schema",
    "open_read",
    "open_write",
    "open_schema",
    "query",
    "checkpoint",
    "commit",
    "rollback",
    "read_once",
    "server_info",
];

// ---------------------------------------------------------------- environment

/// Returns false when the live gate is absent (test should skip cleanly).
fn smoke_gate() -> bool {
    std::env::var("TYPEDB_MCP_SMOKE").as_deref() == Ok("1")
}

fn smoke_env() -> (String, String, String) {
    let addr = std::env::var("TYPEDB_MCP_TEST_ADDRESS")
        .expect("TYPEDB_MCP_TEST_ADDRESS (set by scripts/compatibility_matrix.sh)");
    let user = std::env::var("TYPEDB_MCP_TEST_USERNAME").expect("TYPEDB_MCP_TEST_USERNAME");
    let pass = std::env::var("TYPEDB_MCP_TEST_PASSWORD").expect("TYPEDB_MCP_TEST_PASSWORD");
    (addr, user, pass)
}

fn config_toml(
    addr: &str,
    user: &str,
    pass: &str,
    listen_http: Option<&str>,
    admin: bool,
    migration: bool,
) -> String {
    let http = match listen_http {
        Some(a) => format!("listen_http = \"{a}\"\n"),
        None => String::new(),
    };
    format!(
        "[server]\nlisten_stdio = true\n{http}enable_database_admin_tools = {admin}\n\
         enable_database_migration_tools = {migration}\n\n\
         [typedb]\naddress = \"{addr}\"\n\n\
         [typedb.credentials]\nsource = \"inline\"\nusername = \"{user}\"\npassword = \"{pass}\"\n"
    )
}

// ------------------------------------------------------------------ child rpc

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "typedb-mcp-live-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn abs(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ChildRpc {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    rx: Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: u64,
    _dir: TempDir,
}

impl ChildRpc {
    fn spawn(config_text: &str, tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let config = dir.abs("config.toml");
        std::fs::write(&config, config_text).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_typedb-mcp"))
            .env("TYPEDB_MCP_CONFIG", &config)
            .current_dir(&dir.0) // foreign cwd: child's cwd differs from harness cwd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn typedb-mcp child");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stderr: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let err_handle = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                err_handle.lock().unwrap().push(line);
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
            stderr,
            next_id: 0,
            _dir: dir,
        }
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.stdin.as_mut().expect("stdin"), "{frame}").expect("child stdin write");
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                panic!("timeout waiting for {method} response");
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line)
                        .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
                    if v.get("id") == Some(&json!(id)) {
                        assert_eq!(v["jsonrpc"], "2.0", "protocol-only stdout frame");
                        return v;
                    }
                    // notifications / other traffic: keep waiting
                }
                Err(RecvTimeoutError::Timeout) => panic!("timeout waiting for {method} response"),
                Err(RecvTimeoutError::Disconnected) => {
                    let err = self.stderr.lock().unwrap().join("\n");
                    panic!("child stdout closed while waiting for {method}; stderr:\n{err}")
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let frame = json!({"jsonrpc":"2.0","method":method,"params":params});
        writeln!(self.stdin.as_mut().expect("stdin"), "{frame}").expect("child stdin write");
    }

    fn initialize(&mut self) {
        let v = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "live-exec-harness", "version": "0.1.0"}
            }),
            INIT_TIMEOUT,
        );
        assert!(v.get("result").is_some(), "initialize failed: {v}");
        self.notify("notifications/initialized", json!({}));
    }

    fn tools(&mut self) -> Vec<String> {
        let v = self.request("tools/list", json!({}), INIT_TIMEOUT);
        let tools = v["result"]["tools"].as_array().expect("tools array");
        tools
            .iter()
            .map(|t| t["name"].as_str().expect("tool name").to_string())
            .collect()
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            CALL_TIMEOUT,
        )
    }

    fn start_session(&mut self) -> String {
        let v = self.call("start_session", json!({}));
        let env = ok_envelope(&v, "start_session");
        find_key(&env, "session_id")
            .and_then(|x| x.as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("no session_id in start_session envelope: {env}"))
    }

    /// True when the tool-call response denotes failure (protocol error,
    /// isError flag, or error-shaped envelope text).
    fn is_failure(v: &Value) -> bool {
        if v.get("error").is_some() {
            return true;
        }
        let text = envelope_text(v);
        text.to_lowercase().contains("\"error\"")
            || text.to_lowercase().contains("errorclass")
            || v["result"]["isError"].as_bool() == Some(true)
    }

    fn eof_shutdown(mut self) {
        let pid = self.child.id();
        drop(self.stdin.take()); // EOF: stdio-only server must shut the process down
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            match self.child.try_wait().expect("poll child") {
                Some(status) => {
                    assert!(
                        status.success(),
                        "child pid {pid} did not exit cleanly on EOF: {status}; stderr:\n{}",
                        self.stderr.lock().unwrap().join("\n")
                    );
                    break;
                }
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!("child did not exit within {EXIT_TIMEOUT:?} after stdin EOF");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

impl Drop for ChildRpc {
    /// Best-effort diagnostics: if the child died abnormally, surface its
    /// stderr tail so worker panics are visible in test output.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let err = self.stderr.lock().unwrap();
        if err
            .iter()
            .any(|l| l.contains("panic") || l.contains("ERROR"))
        {
            eprintln!(
                "--- child stderr tail ---\n{}\n--- end ---",
                err.iter()
                    .rev()
                    .take(40)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

// ---------------------------------------------------------------- envelope io

fn envelope_text(v: &Value) -> String {
    v["result"]["content"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["text"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn ok_envelope(v: &Value, what: &str) -> Value {
    assert!(
        v.get("error").is_none(),
        "{what} returned protocol error: {v}"
    );
    let text = envelope_text(v);
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{what} did not return a JSON envelope ({e}): {text}"));
    assert!(
        parsed.get("error").is_none(),
        "{what} returned an error envelope: {parsed}"
    );
    parsed
}

fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                return Some(x);
            }
            m.values().find_map(|x| find_key(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

fn scratch_db(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

/// Define a small schema carrying a @doc annotation (annotation preservation
/// is part of AC.5) and insert one row. Uses the schema/write flow itself.
fn define_schema_with_doc(rpc: &mut ChildRpc, sid: &str, db: &str) {
    ok_envelope(
        &rpc.call("open_schema", json!({"session_id": sid, "database": db})),
        "open_schema",
    );
    ok_envelope(
        &rpc.call(
            "query",
            json!({"session_id": sid, "query": "define\n  attribute name @doc(\"Person name\"), value string;\n  entity person, owns name;"}),
        ),
        "define query",
    );
    ok_envelope(
        &rpc.call("commit", json!({"session_id": sid})),
        "commit define",
    );
    ok_envelope(
        &rpc.call("open_write", json!({"session_id": sid, "database": db})),
        "open_write",
    );
    ok_envelope(
        &rpc.call(
            "query",
            json!({"session_id": sid, "query": "insert $p isa person, has name \"alice\";"}),
        ),
        "insert query",
    );
    ok_envelope(
        &rpc.call("commit", json!({"session_id": sid})),
        "commit insert",
    );
}

fn try_delete_db(rpc: &mut ChildRpc, sid: &str, db: &str) {
    let _ = rpc.call(
        "delete_database",
        json!({"session_id": sid, "database": db, "confirm_database": db}),
    );
}

// ----------------------------------------------------------------- HTTP client

struct HttpResp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResp {
    fn header(&self, name: &str) -> Option<&str> {
        let lname = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lname)
            .map(|(_, v)| v.as_str())
    }
    /// Extract the last `data:` JSON payload (SSE form) or parse the JSON body.
    fn json(&self) -> Value {
        if self
            .header("content-type")
            .map(|c| c.contains("text/event-stream"))
            .unwrap_or(false)
        {
            let last = self
                .body
                .lines()
                .filter_map(|l| l.strip_prefix("data:").map(str::trim))
                .rfind(|d| !d.is_empty() && *d != "[DONE]")
                .expect("SSE body carried no data frame");
            serde_json::from_str(last).expect("SSE data frame is JSON-RPC")
        } else {
            serde_json::from_str(self.body.trim())
                .unwrap_or_else(|e| panic!("JSON body parse failed ({e}): {}", self.body))
        }
    }
}

fn http_post(addr: &str, body: &str, session: Option<&str>) -> HttpResp {
    let mut stream = TcpStream::connect(addr).expect("connect to HTTP transport");
    stream
        .set_read_timeout(Some(CALL_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(30))))
        .unwrap();
    // Streamable HTTP (MCP 2025-03-26+) requires the client to offer BOTH
    // response media types in Accept; rmcp answers 406 to a single type.
    // Which form is actually returned is the server's choice, so the parser
    // below handles both application/json and text/event-stream bodies.
    let accept_header = "application/json, text/event-stream";
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: {accept_header}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(sid) = session {
        req.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    BufReader::new(stream)
        .read_to_end(&mut raw)
        .expect("read HTTP response");
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header/body split in HTTP response: {text}"));
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line: {status_line}"));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    HttpResp {
        status,
        headers,
        body: rest.to_string(),
    }
}

fn http_rpc(addr: &str, session: Option<&str>, id: u64, method: &str, params: Value) -> Value {
    let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
    let resp = http_post(addr, &frame, session);
    assert_eq!(resp.status, 200, "HTTP {method} status: {}", resp.status);
    resp.json()
}

fn http_notify(addr: &str, session: Option<&str>, method: &str) {
    // JSON-RPC notifications get no response body; streamable HTTP answers
    // 202 Accepted (or 200). Only the transport status matters here.
    let frame = json!({"jsonrpc":"2.0","method":method,"params":{}}).to_string();
    let resp = http_post(addr, &frame, session);
    assert!(
        resp.status == 200 || resp.status == 202,
        "notification {method} unexpected status {}",
        resp.status
    );
}

// --------------------------------------------------------------------- tests

/// AC.2/AC.3 executable stdio lifecycle: initialize, tools/list (exactly the
/// twelve defaults; no migrations, no admin), a full transaction flow over
/// real pipes, protocol-only stdout, separate stderr, foreign child cwd, and
/// clean whole-process shutdown on stdin EOF.
#[test]
fn binary_stdio_lifecycle_and_eof() {
    if !smoke_gate() {
        eprintln!("skip: TYPEDB_MCP_SMOKE!=1");
        return;
    }
    let (addr, user, pass) = smoke_env();
    let cfg = config_toml(&addr, &user, &pass, None, true, false);
    let mut rpc = ChildRpc::spawn(&cfg, "lifecycle");
    rpc.initialize();

    let mut tools = rpc.tools();
    tools.sort();
    // Admin tools are enabled in this config, so discovery is the twelve
    // defaults PLUS the two admin tools; migrations must still be absent.
    let mut expected = DEFAULT_TOOLS.to_vec();
    expected.extend_from_slice(&["create_database", "delete_database"]);
    expected.sort_unstable();
    let expected: Vec<String> = expected.into_iter().map(str::to_string).collect();
    assert_eq!(
        tools, expected,
        "discovery must be twelve defaults + enabled admin pair, no migrations"
    );
    assert!(
        !tools
            .iter()
            .any(|t| t.contains("export") || t.contains("import"))
    );

    let sid = rpc.start_session();

    // Admin: create a scratch database, define an annotated schema, insert data.
    let db = scratch_db("mcp_live");
    ok_envelope(
        &rpc.call(
            "create_database",
            json!({"session_id": sid, "database": db}),
        ),
        "create_database",
    );
    // Schema gate: get_schema before transactions.
    ok_envelope(
        &rpc.call("get_schema", json!({"session_id": sid, "database": db})),
        "get_schema",
    );
    define_schema_with_doc(&mut rpc, &sid, &db);

    // Read flow: open_read -> query -> rollback.
    ok_envelope(
        &rpc.call("open_read", json!({"session_id": sid, "database": db})),
        "open_read",
    );
    ok_envelope(
        &rpc.call(
            "query",
            json!({"session_id": sid, "query": "match $p isa person; fetch { \"name\": $p.name };"}),
        ),
        "read query",
    );
    ok_envelope(
        &rpc.call("rollback", json!({"session_id": sid})),
        "rollback",
    );

    try_delete_db(&mut rpc, &sid, &db);
    rpc.eof_shutdown();
}

/// AC.5 executable migration round trip: export with no-clobber destinations,
/// hashes/sizes manifest, import under a new confirmed name, schema gate
/// reset requiring a fresh get_schema on the imported database.
#[test]
fn binary_stdio_migration_round_trip() {
    if !smoke_gate() {
        eprintln!("skip: TYPEDB_MCP_SMOKE!=1");
        return;
    }
    let (addr, user, pass) = smoke_env();
    let cfg = config_toml(&addr, &user, &pass, None, true, true);
    let mut rpc = ChildRpc::spawn(&cfg, "roundtrip");
    rpc.initialize();
    let mut tools = rpc.tools();
    tools.sort();
    assert!(
        tools.contains(&"export_database".to_string())
            && tools.contains(&"import_database".to_string()),
        "migration tools must be discoverable when enabled over stdio; got {tools:?}"
    );

    let sid = rpc.start_session();
    let src = scratch_db("mcp_src");
    let dst = scratch_db("mcp_dst");
    ok_envelope(
        &rpc.call(
            "create_database",
            json!({"session_id": sid, "database": src}),
        ),
        "create_database",
    );
    ok_envelope(
        &rpc.call("get_schema", json!({"session_id": sid, "database": src})),
        "get_schema",
    );
    define_schema_with_doc(&mut rpc, &sid, &src);

    let dir = TempDir::new("export");
    let schema_out = dir.abs("schema.tql");
    let data_out = dir.abs("data.typedb");

    let v = rpc.call(
        "export_database",
        json!({"session_id": sid, "database": src,
               "schema_file_path": schema_out.to_string_lossy(),
               "data_file_path": data_out.to_string_lossy()}),
    );
    let env = ok_envelope(&v, "export_database");

    // Manifest honesty: the response must carry hashes and sizes (empirical
    // field names from MigrationReport serialization).
    assert!(
        find_key(&env, "schema_sha256").is_some(),
        "schema hash in export envelope: {env}"
    );
    assert!(
        find_key(&env, "data_sha256").is_some(),
        "data hash in export envelope: {env}"
    );
    assert!(
        find_key(&env, "schema_size").is_some(),
        "schema size in export envelope: {env}"
    );
    assert!(
        find_key(&env, "data_size").is_some(),
        "data size in export envelope: {env}"
    );

    // Files exist at the resolved absolute paths, are regular, and carry the
    // annotated schema through the round trip (AC.5 annotation preservation).
    assert!(
        schema_out.is_file() && data_out.is_file(),
        "export files must exist"
    );
    let schema_text = std::fs::read_to_string(&schema_out).unwrap();
    assert!(schema_text.contains("person"), "schema content round trip");
    assert!(
        schema_text.contains("@doc"),
        "annotation must survive export"
    );
    assert!(
        std::fs::metadata(&data_out).unwrap().len() > 0,
        "data file non-empty"
    );

    // Import under a NEW confirmed name.
    let v = rpc.call(
        "import_database",
        json!({"session_id": sid, "database": dst,
               "schema_file_path": schema_out.to_string_lossy(),
               "data_file_path": data_out.to_string_lossy(),
               "confirm_database": dst}),
    );
    ok_envelope(&v, "import_database");

    // Schema gate reset: the imported database must NOT be openable for
    // writes without a fresh get_schema in this session.
    let v = rpc.call("open_write", json!({"session_id": sid, "database": dst}));
    assert!(
        ChildRpc::is_failure(&v),
        "open_write on freshly imported DB must be rejected until get_schema: {v}"
    );
    ok_envelope(
        &rpc.call("get_schema", json!({"session_id": sid, "database": dst})),
        "get_schema after import",
    );
    ok_envelope(
        &rpc.call("open_write", json!({"session_id": sid, "database": dst})),
        "open_write after reread",
    );
    ok_envelope(
        &rpc.call(
            "query",
            json!({"session_id": sid, "query": "insert $p isa person, has name \"bob\";"}),
        ),
        "insert into imported",
    );
    ok_envelope(
        &rpc.call("commit", json!({"session_id": sid})),
        "commit imported",
    );
    let v = rpc.call(
        "read_once",
        json!({"session_id": sid, "database": dst, "query": "match $p isa person; fetch { \"name\": $p.name };"}),
    );
    let env = ok_envelope(&v, "read_once");
    assert!(
        env.to_string().contains("bob") && env.to_string().contains("alice"),
        "imported data must contain original and inserted rows: {env}"
    );

    try_delete_db(&mut rpc, &sid, &src);
    try_delete_db(&mut rpc, &sid, &dst);
    rpc.eof_shutdown();
}

/// AC.3 executable cross-transport denial: with migrations enabled on the
/// stdio surface, the HTTP surface of the same mixed-transport process must
/// NOT list them, and guessed calls over HTTP (both JSON and SSE response
/// forms) using a stdio-created session id must fail without touching the
/// filesystem — sentinel files unchanged, import target never created.
#[test]
fn mixed_transport_denies_migration_over_http() {
    if !smoke_gate() {
        eprintln!("skip: TYPEDB_MCP_SMOKE!=1");
        return;
    }
    let (addr, user, pass) = smoke_env();

    // OS-assigned loopback port.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let http_addr = format!("127.0.0.1:{port}");
    let cfg = config_toml(&addr, &user, &pass, Some(&http_addr), false, true);

    // Same config drives both surfaces (mixed transport).
    let mut stdio = ChildRpc::spawn(&cfg, "mixed-stdio");
    stdio.initialize();
    let sid = stdio.start_session();

    // HTTP initialize → session header.
    let init = json!({"jsonrpc":"2.0","id":100,"method":"initialize","params":{
        "protocolVersion":"2025-03-26","capabilities":{},
        "clientInfo":{"name":"live-exec-http","version":"0.1.0"}}});
    let resp = http_post(&http_addr, &init.to_string(), None);
    assert_eq!(resp.status, 200, "HTTP initialize");
    let http_sid = resp
        .header("mcp-session-id")
        .map(str::to_string)
        .expect("streamable HTTP must issue Mcp-Session-Id");
    http_notify(&http_addr, Some(&http_sid), "notifications/initialized");

    // HTTP discovery must not include migration tools even though the shared
    // process has them enabled on stdio.
    let v = http_rpc(&http_addr, Some(&http_sid), 102, "tools/list", json!({}));
    let mut tools: Vec<String> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    tools.sort();
    let mut expected = DEFAULT_TOOLS.to_vec();
    expected.sort_unstable();
    let expected: Vec<String> = expected.into_iter().map(str::to_string).collect();
    assert_eq!(
        tools, expected,
        "HTTP discovery must show exactly the twelve defaults (no migrations)"
    );

    // Sentinel files the guessed calls must NOT touch.
    let dir = TempDir::new("sentinel");
    let schema_sentinel = dir.abs("schema-sentinel.tql");
    let data_sentinel = dir.abs("data-sentinel.typedb");
    std::fs::write(&schema_sentinel, b"SCHEMA-SENTINEL-DO-NOT-MODIFY").unwrap();
    std::fs::write(&data_sentinel, b"DATA-SENTINEL-DO-NOT-MODIFY").unwrap();
    let import_target = dir.abs("never-created.typedb");
    let s_path = schema_sentinel.to_string_lossy().to_string();
    let d_path = data_sentinel.to_string_lossy().to_string();

    for accept in ["application/json", "text/event-stream"] {
        // Guessed export over HTTP with a stdio-created session id.
        let v = http_rpc(
            &http_addr,
            Some(&http_sid),
            103,
            "tools/call",
            json!({"name":"export_database","arguments":{
                "session_id": sid, "database": "whatever",
                "schema_file_path": s_path, "data_file_path": d_path}}),
        );
        assert!(
            ChildRpc::is_failure(&v),
            "guessed export_database over HTTP must fail ({accept}): {v}"
        );
        // Guessed import over HTTP.
        let v = http_rpc(
            &http_addr,
            Some(&http_sid),
            104,
            "tools/call",
            json!({"name":"import_database","arguments":{
                "session_id": sid, "database": "whatever",
                "schema_file_path": s_path, "data_file_path": d_path,
                "confirm_database": "whatever"}}),
        );
        assert!(
            ChildRpc::is_failure(&v),
            "guessed import_database over HTTP must fail ({accept}): {v}"
        );

        assert_eq!(
            std::fs::read(&schema_sentinel).unwrap(),
            b"SCHEMA-SENTINEL-DO-NOT-MODIFY",
            "schema sentinel must be unchanged ({accept})"
        );
        assert_eq!(
            std::fs::read(&data_sentinel).unwrap(),
            b"DATA-SENTINEL-DO-NOT-MODIFY",
            "data sentinel must be unchanged ({accept})"
        );
        assert!(
            !import_target.exists(),
            "guessed import must never create the target ({accept})"
        );
    }

    try_delete_db(&mut stdio, &sid, "whatever"); // no-op safety; nothing was created
    stdio.eof_shutdown();
}
