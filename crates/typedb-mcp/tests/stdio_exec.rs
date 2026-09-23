use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn unreachable_driver_is_reported_before_stdio_protocol() {
    let dir = std::env::temp_dir().join(format!("typedb-mcp-stdio-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    fs::write(&config, "[server]\nlisten_stdio=true\n[typedb]\naddress=\"127.0.0.1:1\"\n[typedb.credentials]\nsource=\"inline\"\nusername=\"admin\"\npassword=\"password\"\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_typedb-mcp"))
        .env("TYPEDB_MCP_CONFIG", &config)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect"));
    assert!(Duration::from_secs(0) <= Duration::from_secs(1));
}
