#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
// Exercise the actual Mac launcher implementation against the packaged engine
// binary without linking the GUI or using a substitute readiness protocol.
use playbit::engine::launch;
use playbit::identifier;

use playbit::engine::{
    Request, ipc,
    reply::{Reply, Status},
};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    os::{
        fd::OwnedFd,
        unix::{fs::OpenOptionsExt, net::UnixStream},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
#[cfg(target_os = "macos")]
use std::{io::Write, time::Instant};

struct EngineProcess(Child);
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_playbit-engine"))
}

fn status(profile: &Path) -> Box<Status> {
    let Reply::Status(status) =
        ipc::call_archive(profile, &Request::Status).unwrap().into_owned().unwrap()
    else {
        panic!("expected status")
    };
    status
}

#[test]
fn fresh_warm_and_stale_endpoint_launches_use_actual_engine_readiness() {
    // Readiness and canonical account identity survive a fresh, warm and stale endpoint.
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let engine = EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap());
    assert_eq!(executable().file_name().unwrap(), "playbit-engine");
    assert!(profile.join("control/engine.sock").exists());
    assert!(profile.join("logs/engine.log").is_file());
    let first = status(&profile);
    assert_eq!(
        identifier::decode::<16>(&first.account.id.to_string()).unwrap(),
        *first.account.id.as_bytes()
    );
    // A ready endpoint does not require a second executable or child process.
    assert!(launch::ensure(&profile, Path::new("/missing-engine")).unwrap().is_none());
    drop(engine);
    assert!(profile.join("control/engine.sock").exists());
    let _restarted = EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap());
    let second = status(&profile);
    assert_eq!(first.account.id, second.account.id);
}

#[cfg(target_os = "macos")]
#[test]
#[allow(unsafe_code, reason = "Signal only the retained isolated helper child.")]
fn privileged_fixture_process_and_cli_bind_a_custom_profile_and_reject_defaults() {
    // Real engine/CLI processes with outbound networking denied must admit only
    // a custom Testing fixture and reject default profiles before opening data.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let home = directory.path().join("home");
    let profile = directory.path().join("fixture");
    fs::create_dir(&home).unwrap();
    let credentials = directory.path().join("credentials.json");
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(&credentials).unwrap();
    write!(
        file,
        "{}",
        serde_json::json!({
            "key": identifier::encode(&[1; 16]), "namespace": identifier::encode(&[2; 16])
        })
    )
    .unwrap();
    let policy = "(version 1) (allow default) (deny network-outbound (remote ip \"*:*\"))";
    let (ready, output) = UnixStream::pair().unwrap();
    ready.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut engine = EngineProcess(
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", policy])
            .arg(executable())
            .env("HOME", &home)
            .env("PLAYBIT_PROFILE", &profile)
            .env("PLAYBIT_CHANNEL", "testing")
            .env("PLAYBIT_TESTING_CREDENTIALS", &credentials)
            .stdout(Stdio::from(OwnedFd::from(output)))
            .spawn()
            .unwrap(),
    );
    let mut notice = String::new();
    BufReader::new(ready).read_line(&mut notice).unwrap();
    assert_eq!(notice, "{\"ready\":true}\n");
    let run = |profile: &Path| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", policy])
            .arg(env!("CARGO_BIN_EXE_pb"))
            .arg("status")
            .env("HOME", &home)
            .env("PLAYBIT_PROFILE", profile)
            .env("PLAYBIT_CHANNEL", "testing")
            .env("PLAYBIT_TESTING_CREDENTIALS", &credentials)
            .output()
            .unwrap()
    };
    let result = run(&profile);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let status: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(status["ap_connection"]["hostname"], "ap.testing.playbit.app");
    assert_eq!(fs::read_to_string(profile.join("channel")).unwrap(), "testing");
    assert_eq!(
        fs::read_to_string(profile.join("testing-namespace")).unwrap(),
        identifier::encode(&[2; 16])
    );
    // SAFETY: the child handle retains exactly the process started above.
    assert_eq!(unsafe { libc::kill(engine.0.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = engine.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(0));
            break;
        }
        assert!(Instant::now() < deadline, "isolated helper did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    for (name, error) in
        [(".playbit", "reserved for Public"), (".playbit-testing", "custom isolated profile")]
    {
        let default = home.join(name);
        let result = run(&default);
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(error), "profile {name}: {stderr}");
        assert!(!default.join("profile.kvdb").exists());
    }
}

#[test]
fn a_held_launcher_lock_fails_explicitly_without_mutating_a_fresh_store() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    fs::create_dir(&profile).unwrap();
    fs::create_dir(profile.join("control")).unwrap();
    let lock_path = profile.join("control/launch.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .unwrap();
    lock.lock().unwrap();
    let error = launch::ensure(&profile, executable()).unwrap_err();
    assert!(error.to_string().contains("already starting"));
    assert!(!profile.join("profile.kvdb").exists());
    // Concurrent tests can fork while this descriptor is open. Explicitly
    // release the fixture lock, including any child copy awaiting exec.
    lock.unlock().unwrap();
    drop(lock);
    let _engine = EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap());
}

#[test]
fn closing_the_launch_receiver_does_not_terminate_the_engine() {
    // A closed readiness receiver preserves the running engine and its canonical identity.
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let (ready, output) = UnixStream::pair().unwrap();
    drop(ready);
    let output: OwnedFd = output.into();
    let (diagnostics, errors) = UnixStream::pair().unwrap();
    diagnostics.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let errors: OwnedFd = errors.into();
    let mut engine = EngineProcess(
        Command::new(executable())
            .env("PLAYBIT_PROFILE", &profile)
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            .spawn()
            .unwrap(),
    );
    // Confirm bind before actual request handling and the single startup release record.
    // The stdout receiver was closed before initialization, so its write must fail safely.
    let mut diagnostics = BufReader::new(diagnostics);
    let mut line = String::new();
    diagnostics.read_line(&mut line).unwrap();
    assert!(line.contains("] Playbit local engine:"));
    let log = std::fs::read_to_string(profile.join("logs/engine.log")).unwrap();
    assert_eq!(log.matches(playbit::VERSION).count(), 1);
    let account = status(&profile).account.id;
    assert_eq!(identifier::decode::<16>(&account.to_string()).unwrap(), *account.as_bytes());
    assert!(engine.0.try_wait().unwrap().is_none());
}

#[test]
fn an_unrelated_endpoint_is_not_replaced_and_profile_is_untouched() {
    // An unrelated process must retain its endpoint and data regardless of its reply.
    // Send only through the rejection boundary, then require the client to close.
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        thread,
    };
    for case in [
        "compatible",
        "wrong protocol",
        "wrong version",
        "wrong reply",
        "invalid archive",
        "invalid prefix",
        "short length",
        "oversized length",
    ] {
        let reply = if case == "wrong reply" {
            Reply::Error { error: "unrelated service".into() }
        } else {
            Reply::Protocol {
                protocol: ipc::LOCAL_PROTOCOL + u32::from(case == "wrong protocol"),
                version: if case == "wrong version" {
                    "different version"
                } else {
                    playbit::VERSION
                }
                .into(),
            }
        };
        let archive = ipc::codec::encode(&reply, 512 - ipc::codec::PREFIX_LEN).unwrap();
        let mut response = Vec::new();
        ipc::codec::write(&mut response, &archive).unwrap();
        match case {
            "invalid archive" => response[ipc::codec::HEADER_LEN..].fill(0xff),
            "invalid prefix" => {
                response[4] ^= 1;
                response.truncate(ipc::codec::HEADER_LEN);
            }
            "short length" => {
                response = ((ipc::codec::PREFIX_LEN - 1) as u32).to_le_bytes().to_vec();
            }
            "oversized length" => response = 513_u32.to_le_bytes().to_vec(),
            _ => {}
        }
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path();
        fs::write(profile.join("retained-data"), b"do not change").unwrap();
        std::fs::create_dir_all(profile.join("control")).unwrap();
        let listener = UnixListener::bind(profile.join("control/engine.sock")).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request: Request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut stream).unwrap(),
            )
            .unwrap();
            assert!(matches!(request, Request::LocalProtocol));
            // Fragment every frame: correctness must not depend on writes coalescing
            // or the server completing another write before the client rejects it.
            for byte in response {
                stream.write_all(&[byte]).unwrap();
            }
            assert_eq!(stream.read(&mut [0]).unwrap(), 0, "{case}");
            listener
        });
        let error = launch::ensure(profile, executable()).unwrap_err();
        assert!(error.to_string().contains("not a separate Playbit engine"), "{case}: {error}");
        let listener = server.join().unwrap();
        assert_eq!(fs::read(profile.join("retained-data")).unwrap(), b"do not change");
        assert_eq!(fs::read_dir(profile).unwrap().count(), 2);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}

#[test]
fn a_stalled_protocol_check_has_a_failure_deadline() {
    use std::{io::Read, os::unix::net::UnixListener, thread};
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("control")).unwrap();
    let listener = UnixListener::bind(directory.path().join("control/engine.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(matches!(
            rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut stream).unwrap()
            )
            .unwrap(),
            Request::LocalProtocol
        ));
        // Wait for the client to close at its deadline, not a timed fake reply.
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let mut stream = UnixStream::connect(directory.path().join("control/engine.sock")).unwrap();
    assert!(ipc::check_protocol(&mut stream).unwrap_err().to_string().contains("did not respond"));
    drop(stream);
    server.join().unwrap();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

// The fixture is a separate OS process, named like the actual helper, with its
// own executable identity. It uses the real durable store and dispatch path;
// only its launch protocol can emulate the obsolete helper.
#[test]
#[ignore = "child-process fixture invoked by replacement tests"]
fn obsolete_engine_fixture() {
    use std::{io::Write, os::unix::net::UnixListener};
    let mode = std::env::var("PLAYBIT_TEST_OBSOLETE_SERVICE").expect("fixture only");
    playbit::engine::profile::initialize().expect("isolated fixture channel");
    let profile = playbit::engine::profile_path();
    let mut engine = playbit::engine::Engine::open(&profile).unwrap();
    std::fs::create_dir_all(profile.join("control")).unwrap();
    let listener = UnixListener::bind(profile.join("control/engine.sock")).unwrap();
    std::io::stdout().write_all(ipc::ENGINE_READY).unwrap();
    std::io::stdout().flush().unwrap();
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        ipc::verify_peer(&stream).unwrap();
        let Ok(bytes) = ipc::read(&mut stream) else { continue };
        let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
        let response = if mode == "obsolete" && matches!(request, Request::LocalProtocol) {
            playbit::engine::reply::Reply::Protocol {
                protocol: ipc::LOCAL_PROTOCOL - 1,
                version: "old package".into(),
            }
        } else {
            engine.dispatch_reply(request).unwrap()
        };
        ipc::write(&mut stream, &ipc::codec::encode(&response, ipc::codec::MAX_ARCHIVE).unwrap())
            .unwrap();
    }
}

fn obsolete_engine(directory: &Path, profile: &Path, mode: &str) -> EngineProcess {
    let fixture = directory.join("playbit-engine");
    fs::copy(std::env::current_exe().unwrap(), &fixture).unwrap();
    // Complete the copied harness's first-launch OS assessment before fixture setup.
    // The readiness deadline still bounds actual store and endpoint initialization.
    let assessment = Command::new(&fixture).arg("--list").output().unwrap();
    assert!(assessment.status.success(), "{}", String::from_utf8_lossy(&assessment.stderr));
    let (ready, output) = UnixStream::pair().unwrap();
    ready.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let output: OwnedFd = output.into();
    let mut command = if mode == "ignore-term" {
        let mut command = Command::new("/bin/sh");
        // Ignored dispositions survive exec; the actual connected process is
        // still the fixture executable, not the shell.
        command.args(["-c", "trap '' TERM; exec \"$@\"", "fixture"]).arg(&fixture);
        command
    } else {
        Command::new(fixture)
    };
    let engine = EngineProcess(
        command
            .args(["--exact", "obsolete_engine_fixture", "--ignored", "--nocapture"])
            .env("PLAYBIT_PROFILE", profile)
            .env("PLAYBIT_TEST_OBSOLETE_SERVICE", mode)
            .stdout(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    for line in BufReader::new(ready).lines() {
        if line.unwrap().contains("{\"ready\":true}") {
            return engine;
        }
    }
    panic!("fixture exited before readiness");
}

#[test]
fn a_different_engine_is_replaced_with_committed_data_retained() {
    for mode in ["obsolete", "compatible"] {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let mut old = obsolete_engine(directory.path(), &profile, mode);
        let before = status(&profile);
        let Reply::Volume(volume) = ipc::call_archive(
            &profile,
            &Request::CreateVolume {
                share_key_with_service: true,
                name: "Keep this volume".into(),
            },
        )
        .unwrap()
        .into_owned()
        .unwrap() else {
            panic!("expected volume")
        };
        let retained = status(&profile);
        // Another profile must not be touched by this profile's handoff.
        let other_profile = directory.path().join("other-profile");
        let mut other =
            EngineProcess(launch::ensure(&other_profile, executable()).unwrap().unwrap());
        // A bad installation must not terminate the retained engine.
        let missing = directory.path().join("missing-engine");
        let result = launch::ensure(&profile, &missing);
        if mode == "obsolete" {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_none());
        }
        assert!(old.0.try_wait().unwrap().is_none());
        let current = if mode == "compatible" {
            let barrier = std::sync::Barrier::new(4);
            let mut children = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            launch::ensure(&profile, executable())
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .filter_map(|worker| match worker.join().unwrap() {
                        Ok(child) => child.map(EngineProcess),
                        Err(error) => {
                            assert!(error.to_string().contains("already starting"), "{error}");
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            });
            assert_eq!(children.len(), 1);
            children.pop().unwrap()
        } else {
            EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap())
        };
        assert!(!old.0.wait().unwrap().success());
        assert_ne!(old.0.id(), current.0.id());
        assert!(other.0.try_wait().unwrap().is_none());
        let after = status(&profile);
        assert_eq!(before.account.id, after.account.id);
        assert_eq!(retained.volumes, after.volumes);
        assert!(after.volumes.iter().any(|item| item.id == volume.id));
        assert!(launch::ensure(&profile, executable()).unwrap().is_none());
    }
}

#[test]
fn a_engine_that_cannot_exit_is_left_with_its_data_and_no_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let mut old = obsolete_engine(directory.path(), &profile, "ignore-term");
    let retained = fs::read(profile.join("profile.kvdb")).unwrap();
    // This child ignores SIGTERM: the exit deadline must fail without
    // escalating to SIGKILL or spawning a competing store owner.
    let error = launch::ensure(&profile, executable()).unwrap_err();
    assert!(error.to_string().contains("did not stop"), "{error}");
    assert!(old.0.try_wait().unwrap().is_none());
    assert_eq!(fs::read(profile.join("profile.kvdb")).unwrap(), retained);
    assert!(!profile.join("logs/engine.log").exists());
}

#[test]
fn launcher_does_not_follow_log_or_lock_symlinks() {
    use std::os::unix::fs::symlink;
    for suffix in ["launch.lock", "engine.log"] {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        fs::create_dir(&profile).unwrap();
        let target = directory.path().join("retained");
        fs::write(&target, b"unchanged").unwrap();
        let subdirectory = if suffix == "launch.lock" { "control" } else { "logs" };
        fs::create_dir(profile.join(subdirectory)).unwrap();
        symlink(&target, profile.join(subdirectory).join(suffix)).unwrap();
        assert!(launch::ensure(&profile, executable()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        assert!(!profile.join("control/engine.sock").exists());
    }
}

#[cfg(all(target_os = "macos", feature = "desktop"))]
#[test]
fn flat_packaged_clients_share_the_only_engine_helper() {
    // Copy real executables into the package layout, then exercise both clients
    // against its one engine. No client app bundles or symlinks are present.
    use std::io::{Read, Write};
    let directory = tempfile::tempdir().unwrap();
    let contents = directory.path().join("Playbit.app/Contents");
    let macos = contents.join("MacOS");
    fs::create_dir_all(&macos).unwrap();
    let pb = macos.join("pb");
    let mcp = macos.join("playbit-mcp");
    fs::copy(env!("CARGO_BIN_EXE_pb"), &pb).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_playbit-mcp"), &mcp).unwrap();
    // The containing app also hosts the background drive coordinator. This
    // fixture checks client routing; packaged workflow checks its Swift payload.
    assert!(
        Command::new("clang")
            .args(["-std=c17", "-Oz"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("platform/macos/launcher.c"))
            .arg("-o")
            .arg(macos.join("Playbit"))
            .status()
            .unwrap()
            .success()
    );
    let helper = launch::executable_for(&pb).unwrap();
    fs::create_dir_all(helper.parent().unwrap()).unwrap();
    fs::copy(executable(), &helper).unwrap();
    // Assess this fresh package copy before exercising the launcher's unchanged
    // readiness deadline. First-launch OS validation can dominate fixture setup.
    let (mut ready, output) = UnixStream::pair().unwrap();
    ready.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let output: OwnedFd = output.into();
    let assessment = EngineProcess(
        Command::new(&helper)
            .env("PLAYBIT_PROFILE", directory.path().join("assess"))
            .stdout(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let mut greeting = [0; ipc::ENGINE_READY.len()];
    ready.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, ipc::ENGINE_READY);
    drop(assessment);
    let profile = directory.path().join("profile");
    let _engine = EngineProcess(launch::ensure(&profile, &helper).unwrap().unwrap());
    let result = Command::new(&pb).arg("status").env("PLAYBIT_PROFILE", &profile).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let cli: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let mut client = Command::new(&mcp)
        .env("PLAYBIT_PROFILE", &profile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    client
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
              \"params\":{\"name\":\"playbit\",\"arguments\":{\"op\":\"status\"}}}\n",
        )
        .unwrap();
    let result = client.wait_with_output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let reply: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_ne!(reply["result"]["isError"], true);
    let mcp: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(cli["account"]["id"], mcp["account"]["id"]);
    assert!(!cli["account"]["id"].is_null());
    assert!(launch::ensure(&profile, &helper).unwrap().is_none());
}

#[test]
fn socket_or_profile_removal_stops_actual_engine_and_allows_reopen() {
    // Exercise the real helper's native endpoint watch and full teardown, with
    // a second profile proving that retirement remains local to its owner.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let other_profile = directory.path().join("other");
    let mut other = EngineProcess(launch::ensure(&other_profile, executable()).unwrap().unwrap());
    let other_account = status(&other_profile).account.id;
    for remove_profile in [false, true] {
        let profile = directory.path().join(if remove_profile { "deleted" } else { "unlinked" });
        let mut engine = EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap());
        let account = status(&profile).account.id;
        if remove_profile {
            fs::remove_dir_all(&profile).unwrap();
        } else {
            fs::remove_file(profile.join("control/engine.sock")).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(exit) = engine.0.try_wait().unwrap() {
                assert!(exit.success(), "engine failed after endpoint removal: {exit}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "engine did not stop after endpoint removal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _reopened = EngineProcess(launch::ensure(&profile, executable()).unwrap().unwrap());
        let reopened_account = status(&profile).account.id;
        if !remove_profile {
            assert_eq!(reopened_account, account);
        }
        assert!(other.0.try_wait().unwrap().is_none());
        assert_eq!(status(&other_profile).account.id, other_account);
    }
}
