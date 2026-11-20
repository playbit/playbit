#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use std::{
    io::Write,
    process::{Command, Output, Stdio},
};

const LIMIT: usize = 4 * 1024 * 1024;
fn run(input: &[u8]) -> Output {
    let profile = tempfile::tempdir().unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_playbit-mcp"))
        .env("PLAYBIT_PROFILE", profile.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Err(error) = process.stdin.as_mut().unwrap().write_all(input) {
        // An oversized frame may close the connection before the producer
        // finishes writing. No subsequent request may then be dispatched.
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    drop(process.stdin.take());
    process.wait_with_output().unwrap()
}
#[test]
fn exact_limit_crlf_and_unterminated_final_rpc_work_in_the_real_process() {
    let mut input = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_vec();
    input.resize(LIMIT, b' ');
    input.extend_from_slice(b"\r\n");
    input.extend_from_slice(
        "{\"jsonrpc\":\"2.0\",\"id\":\"雪\",\"method\":\"tools/list\"}".as_bytes(),
    );
    let output = run(&input);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let rows: Vec<serde_json::Value> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[0]["result"]["serverInfo"]["name"], "playbit");
    assert_eq!(rows[0]["result"]["serverInfo"]["version"], playbit::VERSION);
    assert_eq!(rows[1]["id"], "雪");
    assert_eq!(rows[1]["result"]["tools"][0]["name"], "playbit");
}
#[test]
fn oversize_closes_before_dispatching_the_following_rpc() {
    let mut input = vec![b'x'; LIMIT + 1];
    input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n");
    let output = run(&input);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MCP message exceeds 4 MiB"));
}
#[test]
fn invalid_utf8_terminates_without_a_protocol_reply() {
    let output = run(&[0xff, b'\n']);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not UTF-8"));
}
