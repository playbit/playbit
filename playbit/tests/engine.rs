#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::engine::{Request, ipc, reply::Reply};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Seek, Write},
    os::unix::net::UnixListener,
    process::{Child, Command, Stdio},
    thread,
};

struct Client {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
    errors: std::fs::File,
    _directory: tempfile::TempDir,
}
impl Client {
    // Deliberately omit the sibling helper: discovery must not need it, and an
    // already-running same-user endpoint must remain usable without it.
    fn new(profile: &std::path::Path) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("playbit-mcp");
        std::fs::copy(env!("CARGO_BIN_EXE_playbit-mcp"), &executable).unwrap();
        let errors = tempfile::tempfile().unwrap();
        let mut child = Command::new(executable)
            .env("PLAYBIT_PROFILE", profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(errors.try_clone().unwrap())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, output, errors, _directory: directory }
    }
    fn rpc(&mut self, method: &str, arguments: Value) -> Value {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":1, "method":method,
            "params":{"name":"playbit", "arguments":arguments}})
        )
        .unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        if self.output.read_line(&mut line).unwrap() == 0 {
            // EOF can precede process exit. Give it a bounded chance to report
            // its actual status, then retire only this test's owned child.
            let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= until {
                    self.child.kill().unwrap();
                    break self.child.wait().unwrap();
                }
                thread::sleep(std::time::Duration::from_millis(10));
            };
            // A regular file cannot wait for an inherited stderr pipe to close.
            self.errors.rewind().unwrap();
            let mut errors = Vec::new();
            Read::take(&mut self.errors, 64 * 1024).read_to_end(&mut errors).unwrap();
            panic!(
                "MCP child closed stdout before {method} reply ({status}); stderr:\n{}",
                String::from_utf8_lossy(&errors)
            );
        }
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 1);
        response["result"].clone()
    }
    fn close(mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn pb_store_failures_explain_the_cause_and_preserve_the_profile() {
    // Exercise the real pb/helper boundary for more than one command and damage kind.
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    // Admit the profile normally so each damaged catalog reaches its intended check.
    drop(playbit::store::LocalStore::open(&profile).unwrap());
    let format = std::fs::read(profile.join("control/profile-format.json")).unwrap();
    let catalog = profile.join(playbit::paths::PROFILE_DB);
    for fixture in ["damaged header", "incompatible version", "missing account"] {
        if fixture == "damaged header" {
            std::fs::write(&catalog, b"damaged").unwrap();
        } else {
            std::fs::remove_file(&catalog).unwrap();
            let database =
                playbit::store::create_replica(&catalog, [7; 32], 2 * 1024 * 1024).unwrap();
            let mut tx = database.begin_write(1).unwrap();
            tx.put(b"logical-format", &format).unwrap();
            tx.put(b"account-secret", &[9; 32]).unwrap();
            tx.commit().unwrap();
        }
        if fixture == "incompatible version" {
            let mut bytes = std::fs::read(&catalog).unwrap();
            // redb stores the format version first in each 128-byte transaction header.
            // Both slots must require an upgrade, regardless of the active slot.
            bytes[64] = 2;
            bytes[192] = 2;
            std::fs::write(&catalog, bytes).unwrap();
        }
        for command in [["session", "list"], ["volume", "list"]] {
            let result = Command::new(env!("CARGO_BIN_EXE_pb"))
                .args(command)
                .env("PLAYBIT_PROFILE", &profile)
                .output()
                .unwrap();
            assert_eq!(result.status.code(), Some(1));
            let error = String::from_utf8(result.stderr).unwrap();
            assert!(error.contains(&catalog.display().to_string()), "{error}");
            assert!(error.contains(&profile.display().to_string()), "{error}");
            assert!(error.contains("back up"), "{error}");
            assert!(error.contains("have not synced"), "{error}");
            assert!(error.contains("PLAYBIT_PROFILE="), "{error}");
            assert!(error.contains("Engine log:"), "{error}");
            if fixture == "damaged header" {
                assert!(
                    error.contains("invalid or truncated") || error.contains("database backend"),
                    "{error}"
                );
                assert_eq!(std::fs::read(&catalog).unwrap(), b"damaged");
            } else if fixture == "incompatible version" {
                assert!(error.contains("database backend format version 2"), "{error}");
                assert!(error.contains("incompatible database version"), "{error}");
                let bytes = std::fs::read(&catalog).unwrap();
                assert_eq!((bytes[64], bytes[192]), (2, 2));
            } else {
                assert!(error.contains("account record is missing"), "{error}");
            }
        }
    }
    let database = playbit::store::open_replica(&catalog, 2 * 1024 * 1024).unwrap();
    let tx = database.begin_read().unwrap();
    assert!(tx.get(b"account").unwrap().is_none());
    assert_eq!(tx.get(b"account-secret").unwrap().unwrap().as_ref(), &[9; 32]);
}

#[test]
fn discovery_and_invalid_operations_have_no_startup_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let mut client = Client::new(&profile);
    assert_eq!(client.rpc("initialize", Value::Null)["serverInfo"]["name"], "playbit");
    assert_eq!(client.rpc("tools/list", Value::Null)["tools"][0]["name"], "playbit");
    assert_eq!(client.rpc("tools/call", json!({"op":"not-an-operation"}))["isError"], true);
    assert!(!profile.exists());
    let response = client.rpc("tools/call", json!({"op":"status"}));
    assert_eq!(response["isError"], true);
    assert!(response["content"][0]["text"].as_str().unwrap().contains("Reinstall Playbit"));
    assert!(!profile.join("profile.kvdb").exists(), "missing helper must not initialize identity");
    assert!(profile.join("logs/engine.log").is_file(), "retain startup diagnostics");
    client.close();
}

#[test]
fn running_endpoint_needs_no_helper_and_a_lost_reply_is_not_replayed() {
    let directory = tempfile::tempdir().unwrap();
    // Advertise a running endpoint only after fixture initialization. Store
    // setup must not race the client's bounded protocol compatibility probe.
    let storage = tempfile::tempdir().unwrap();
    let mut engine = playbit::engine::Engine::open(storage.path()).unwrap();
    std::fs::create_dir_all(directory.path().join("control")).unwrap();
    let listener = UnixListener::bind(directory.path().join("control/engine.sock")).unwrap();
    let server = thread::spawn(move || {
        for attempt in 0..2 {
            // Admission checks protocol compatibility before the operation.
            let (mut probe, _) = listener.accept().unwrap();
            let request: Request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut probe).unwrap(),
            )
            .unwrap();
            assert!(matches!(request, Request::LocalProtocol));
            ipc::write(
                &mut probe,
                &ipc::codec::encode(
                    &Reply::Protocol {
                        protocol: ipc::LOCAL_PROTOCOL,
                        version: playbit::VERSION.into(),
                    },
                    504,
                )
                .unwrap(),
            )
            .unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut stream).unwrap(),
            )
            .unwrap();
            if attempt == 0 {
                assert!(matches!(
                    request,
                    Request::CreateVolume { share_key_with_service: true, .. }
                ));
                engine.dispatch_reply(request).unwrap();
                // Model a committed mutation whose response is lost.
            } else {
                assert!(matches!(request, Request::Status), "the mutation must never be replayed");
                let response = engine.dispatch_reply(request).unwrap();
                ipc::write(
                    &mut stream,
                    &ipc::codec::encode(&response, ipc::codec::MAX_ARCHIVE).unwrap(),
                )
                .unwrap();
            }
        }
    });
    let mut client = Client::new(directory.path());
    let response = client.rpc("tools/call", json!({"op":"create-volume", "name":"once"}));
    assert_eq!(response["isError"], true);
    let response = client.rpc("tools/call", json!({"op":"status"}));
    assert!(response.get("isError").is_none());
    let state: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(state["volumes"][0]["name"], "once");
    client.close();
    server.join().unwrap();
}

#[test]
fn maximum_ipc_response_preserves_nested_json_and_followup_discovery() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("control")).unwrap();
    let listener = UnixListener::bind(directory.path().join("control/engine.sock")).unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut engine = playbit::engine::Engine::open(storage.path()).unwrap();
    let mut response = engine.dispatch_reply(Request::Status).unwrap();
    let Reply::Status(status) = &mut response else { panic!("status reply") };
    status.availability.clear();
    let empty = ipc::codec::encode(&response, ipc::codec::MAX_ARCHIVE).unwrap().len();
    let Reply::Status(status) = &mut response else { panic!("status reply") };
    status.availability = "\n".repeat(ipc::codec::MAX_ARCHIVE - empty);
    let payload = ipc::codec::encode(&response, ipc::codec::MAX_ARCHIVE).unwrap();
    assert_eq!(payload.len(), ipc::codec::MAX_ARCHIVE);
    let expected = serde_json::to_value(response).unwrap();
    let server = thread::spawn(move || {
        let (mut probe, _) = listener.accept().unwrap();
        let request: Request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
            &ipc::read(&mut probe).unwrap(),
        )
        .unwrap();
        assert!(matches!(request, Request::LocalProtocol));
        ipc::write(
            &mut probe,
            &ipc::codec::encode(
                &Reply::Protocol {
                    protocol: ipc::LOCAL_PROTOCOL,
                    version: playbit::VERSION.into(),
                },
                504,
            )
            .unwrap(),
        )
        .unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
            &ipc::read(&mut stream).unwrap(),
        )
        .unwrap();
        assert!(matches!(request, Request::Status));
        ipc::write(&mut stream, &payload).unwrap();
    });
    let mut client = Client::new(directory.path());
    let result = client.rpc("tools/call", json!({"op":"status"}));
    assert!(result.get("isError").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        expected
    );
    assert_eq!(client.rpc("initialize", Value::Null)["serverInfo"]["name"], "playbit");
    client.close();
    server.join().unwrap();
}
