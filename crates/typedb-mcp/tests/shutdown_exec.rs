#[test]
fn shutdown_probe_forces_noncooperative_worker() {
    let exe = env!("CARGO_BIN_EXE_shutdown_probe");
    let start = std::time::Instant::now();
    let output = std::process::Command::new(exe)
        .env("TYPEDB_MCP_SHUTDOWN_GRACE", "1")
        .output()
        .expect("spawn shutdown probe");
    assert_eq!(output.status.code(), Some(1));
    assert!(start.elapsed() <= std::time::Duration::from_secs(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("shutdown grace expired")
            && stderr.contains("uncertain")
            && stderr.contains("forcing close"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"));
}
