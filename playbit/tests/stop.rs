#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
// A stop signal must end the actual engine process orderly, whatever it is
// doing: idle, or with its AP connection held inside a background worker.
use playbit::engine::{
    Engine, Request, ipc,
    reply::{ArchivedReply, ArchivedTransferKind},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

struct EngineProcess(Child);
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn spawn(command: &mut Command, profile: &Path) -> (EngineProcess, BufReader<UnixStream>) {
    let (output, child_output) = UnixStream::pair().unwrap();
    output.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let child_output: OwnedFd = child_output.into();
    let engine = EngineProcess(
        command.env("PLAYBIT_PROFILE", profile).stdout(Stdio::from(child_output)).spawn().unwrap(),
    );
    (engine, BufReader::new(output))
}
/// The first output line containing `needle`.
fn line_with(output: &mut BufReader<UnixStream>, needle: &str) -> String {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(output.read_line(&mut line).unwrap() > 0, "engine output ended before {needle:?}");
        if line.contains(needle) {
            return line;
        }
    }
}
#[allow(unsafe_code, reason = "Signal only the child process owned and retained by this test.")]
fn signal_stop(engine: &EngineProcess) {
    assert_eq!(unsafe { libc::kill(engine.0.id() as libc::pid_t, libc::SIGTERM) }, 0);
}
fn wait_for_exit(engine: &mut EngineProcess) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = engine.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "engine did not exit after SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn terminate(engine: &mut EngineProcess) -> ExitStatus {
    signal_stop(engine);
    wait_for_exit(engine)
}
const READY: &str = "{\"ready\":true}";

#[test]
fn a_stop_signal_ends_an_idle_engine_orderly() {
    let profile = tempfile::tempdir().unwrap();
    let (mut engine, mut output) =
        spawn(&mut Command::new(env!("CARGO_BIN_EXE_playbit-engine")), profile.path());
    line_with(&mut output, READY);
    assert!(matches!(
        ipc::call_archive(profile.path(), &Request::Status).unwrap().archived(),
        ArchivedReply::Status(_)
    ));
    // Idle: the owner loop is parked in a wait with no timeout, so the signal
    // itself must wake it. A bare kill would report the signal, not a status.
    let status = terminate(&mut engine);
    assert_eq!(status.code(), Some(0), "{status}");
}

#[test]
#[allow(unsafe_code, reason = "The isolated child inherits the dispatch-worker signal mask.")]
fn a_stop_signal_ends_an_engine_launched_with_stop_signals_blocked() {
    use std::os::unix::process::CommandExt;
    let profile = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_playbit-engine"));
    // SAFETY: this only changes the child mask using async-signal-safe calls.
    unsafe {
        command.pre_exec(|| {
            let mut signals = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGTERM);
            libc::sigaddset(&mut signals, libc::SIGINT);
            if libc::sigprocmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (mut engine, mut output) = spawn(&mut command, profile.path());
    line_with(&mut output, READY);
    assert_eq!(terminate(&mut engine).code(), Some(0));
}

// The engine, as the real binary runs it, except enrolled and pointed at
// its own loopback AP: a real QUIC server that authenticates and then never
// answers, so the first owner turn hands `Remote` to a synchronizing worker
// that stays inside an AP exchange. The AP's observations reach the test
// through this process's stdout.
#[test]
#[ignore = "child-process fixture invoked by the busy stop test"]
fn connected_engine_fixture() {
    std::env::var_os("PLAYBIT_TEST_CONNECTED_SERVICE").expect("fixture only");
    playbit::engine::profile::initialize().expect("isolated fixture channel");
    let profile = playbit::engine::profile_path();
    let (seed, account) = {
        let mut engine = Engine::open(&profile).unwrap();
        // Authentication alone can finish synchronizing an empty profile.
        // An unpublished volume guarantees an actual AP exchange to hold.
        engine
            .dispatch_reply(Request::CreateVolume {
                share_key_with_service: true,
                name: "Stop fixture".into(),
            })
            .unwrap();
        (
            engine.services.store.account_seed().unwrap(),
            *engine.services.store.account().id.as_bytes(),
        )
    };
    let mut identity =
        playbit::network::ap::ClientIdentity::from_seed("stop@test.invalid", "Test", seed).unwrap();
    identity.auth_token = account.iter().copied().chain([0; 56]).collect();
    identity.instance_id = playbit::store::Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![1];
    identity.save(&profile.join("instance.json")).unwrap();
    let mut engine = Engine::open(&profile).unwrap();
    let ap = playbit::network::ap::testing::silent_authenticating_server();
    engine.remote.as_mut().unwrap().set_test_transport(ap.config);
    std::thread::spawn(move || {
        if ap.request_held.recv().is_ok() {
            println!("ap-request-held");
        }
    });
    playbit::engine::process::serve(&profile, engine).unwrap();
    println!("close-observed:{}", ap.closed.recv_timeout(Duration::from_secs(5)).unwrap_or(false));
}

/// The replacing instance's SIGTERM arrives whenever it arrives, including
/// while a background worker owns `Remote` inside an AP exchange. Dropping
/// the Engine alone flushes nothing then; the engine has to get `Remote` back
/// first. The AP reports the close before the process is gone.
#[test]
fn a_stop_signal_during_held_ap_work_flushes_the_close_before_exit() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "connected_engine_fixture", "--ignored", "--nocapture"])
        .env("PLAYBIT_TEST_CONNECTED_SERVICE", "1");
    let (mut engine, mut output) = spawn(&mut command, &profile);
    line_with(&mut output, READY);
    line_with(&mut output, "ap-request-held");
    let transfer = ipc::call_archive(&profile, &Request::TransferStatus).unwrap();
    let ArchivedReply::Transfer { transfer } = transfer.archived() else {
        panic!("expected transfer status")
    };
    assert!(matches!(transfer.kind, ArchivedTransferKind::Running));
    let status = terminate(&mut engine);
    assert_eq!(line_with(&mut output, "close-observed:").trim(), "close-observed:true");
    assert_eq!(status.code(), Some(0), "{status}");
}

/// The credentials the fixture's AP issues for the sign-in code. Both
/// processes use the same profile account, so the parent can check on disk exactly
/// what the AP handed out.
fn issued(account: [u8; 16]) -> playbit::network::ap::testing::IssuedCredentials {
    playbit::network::ap::testing::IssuedCredentials {
        // 72 bytes, the first 16 of which are the account id.
        auth_token: account.into_iter().chain((16..72).map(|byte| byte + 1)).collect(),
        instance_id: playbit::store::Id::from_bytes([9; 16]).to_string().into_bytes(),
        ca_certificate: vec![2],
        instance_certificate: vec![3],
    }
}

// The engine as the real binary runs it, on a fresh unenrolled profile,
// pointed at its own loopback AP. That AP answers the email request at once
// and then holds the answer to the sign-in code, the exchange that issues
// durable credentials. The parent drives enrollment over the engine socket
// and releases the held answer by writing a line to this process's stdin.
#[test]
#[ignore = "child-process fixture invoked by the enrollment stop test"]
fn enrolling_engine_fixture() {
    std::env::var_os("PLAYBIT_TEST_ENROLLING_SERVICE").expect("fixture only");
    playbit::engine::profile::initialize().expect("isolated fixture channel");
    let profile = playbit::engine::profile_path();
    let mut engine = Engine::open(&profile).unwrap();
    let (config, issuing, gate) = playbit::network::ap::testing::held_registration_server(issued(
        *engine.services.store.account().id.as_bytes(),
    ));
    engine.remote.as_mut().unwrap().set_test_transport(config);
    std::thread::spawn(move || {
        issuing.recv().unwrap();
        println!("credentials-held");
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
        gate.open();
    });
    playbit::engine::process::serve(&profile, engine).unwrap();
}

/// A stop signal can arrive while the AP is issuing this device's credentials.
/// That exchange is not retried and not repeated: exiting under it loses
/// credentials the account already owns. The engine must decline to exit for
/// as long as the exchange lasts, and the replacing instance's own timeout is
/// what reports it as busy. Here the AP holds its answer past the shutdown
/// bound, and the engine is still running afterwards; once the answer is
/// released it saves the credentials and exits orderly.
#[test]
fn a_stop_signal_waits_for_an_enrollment_that_is_issuing_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let account = *Engine::open(&profile).unwrap().services.store.account().id.as_bytes();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "enrolling_engine_fixture", "--ignored", "--nocapture"])
        .env("PLAYBIT_TEST_ENROLLING_SERVICE", "1");
    let (release, child_input) = UnixStream::pair().unwrap();
    let (notices, child_notices) = UnixStream::pair().unwrap();
    notices.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    command
        .stdin(Stdio::from(OwnedFd::from(child_input)))
        .stderr(Stdio::from(OwnedFd::from(child_notices)));
    let (mut engine, mut output) = spawn(&mut command, &profile);
    // The parent's own copies of the child's ends live in the command. Let
    // them go, so an engine that exits early ends these streams instead of
    // leaving a read waiting for output that can no longer come.
    drop(command);
    let mut notices = BufReader::new(notices);
    line_with(&mut output, READY);

    let sent =
        ipc::call_archive(&profile, &Request::RequestCode { email: "stop@test.invalid".into() })
            .unwrap();
    assert!(matches!(sent.archived(), ArchivedReply::CodeSent { code_sent: true }));
    // The code submission never returns: its answer is held at the AP.
    let signing_in = std::thread::spawn({
        let profile = profile.clone();
        move || {
            let _ = ipc::call_archive(&profile, &Request::SignIn { code: "123456".into() });
        }
    });
    line_with(&mut output, "credentials-held");

    signal_stop(&engine);
    // The engine says it is not exiting; it only says so once its bounded
    // shutdown attempt has passed with the exchange still in flight.
    line_with(&mut notices, "Stop deferred");
    assert!(
        engine.0.try_wait().unwrap().is_none(),
        "the engine exited under an unfinished enrollment"
    );

    (&release).write_all(b"go\n").unwrap();
    let status = wait_for_exit(&mut engine);
    assert_eq!(status.code(), Some(0), "{status}");
    let identity =
        playbit::network::ap::ClientIdentity::load(&profile.join("instance.json")).unwrap();
    assert_eq!(
        identity.auth_token,
        issued(account).auth_token,
        "the issued credentials must be on disk"
    );
    assert_eq!(identity.instance_certificate, issued(account).instance_certificate);
    let engine = Engine::open(&profile).unwrap();
    assert_eq!(
        &engine.services.store.account().id.as_bytes()[..],
        &issued(account).auth_token[..16],
        "the account the AP issued must be bound locally"
    );
    signing_in.join().unwrap();
}
