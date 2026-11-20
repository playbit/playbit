//! Real socket peers exercise profile isolation, termination and exclusive reset.
#![allow(clippy::unwrap_used, reason = "Tests assert successful isolated process fixtures.")]
use super::*;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
};

struct Engine {
    child: Child,
    output: BufReader<UnixStream>,
    profile: PathBuf,
    _directory: tempfile::TempDir,
}
impl Engine {
    fn start(stubborn: bool) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let executable = directory.path().join("playbit-engine");
        // Keep OS-authenticated peers small: copying and launching the full test
        // executable can exceed readiness bounds during concurrent workspace tests.
        let source = directory.path().join("owner.c");
        fs::write(&source, include_str!("owner.c")).unwrap();
        assert!(
            Command::new("cc")
                .args(["-std=c17", "-Wall", "-Wextra", "-Werror", "-O2"])
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        let profile = directory.path().join("profile");
        drop(crate::store::LocalStore::open(&profile).unwrap());
        let (ready, output) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
        let output: OwnedFd = output.into();
        let child = Command::new(executable)
            .arg(&profile)
            .arg(if stubborn { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .spawn()
            .unwrap();
        let output = BufReader::new(ready);
        let mut engine = Self { child, output, profile, _directory: directory };
        engine.wait_message("engine-fixture-ready");
        engine
    }
    fn wait_message(&mut self, message: &str) {
        let mut line = String::new();
        assert_ne!(self.output.read_line(&mut line).unwrap(), 0, "fixture exited");
        assert_eq!(line.trim(), message);
    }
    fn peer(&self) -> Peer {
        let socket = UnixStream::connect(self.profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        Peer::capture(&socket).unwrap()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn resetting_one_profile_leaves_another_running_engine_and_its_data_untouched() {
    // Discover identical executables through two independent profile sockets.
    let mut selected = Engine::start(false);
    let mut other = Engine::start(false);
    let other_account = fs::read(other.profile.join(crate::paths::PROFILE_DB)).unwrap();
    super::super::sign_out::reset_profile(&selected.profile, None).unwrap();
    assert_eq!(selected.child.wait().unwrap().signal(), Some(libc::SIGTERM));
    assert!(other.child.try_wait().unwrap().is_none());
    other.peer().require_engine().unwrap();
    assert_eq!(fs::read(other.profile.join(crate::paths::PROFILE_DB)).unwrap(), other_account);
    assert!(crate::store::LocalStore::open(&selected.profile).is_ok());
}

#[test]
fn graceful_timeout_retains_data_but_explicit_reset_kills_the_same_peer() {
    // First preserve a stubborn owner, then exercise the actual 30-second reset
    // deadline while another launcher tries to recover that same profile.
    let mut engine = Engine::start(true);
    let mut other = Engine::start(false);
    let other_account = fs::read(other.profile.join(crate::paths::PROFILE_DB)).unwrap();
    let before = fs::read(engine.profile.join(crate::paths::PROFILE_DB)).unwrap();
    let error = engine.peer().stop(Duration::from_millis(20)).unwrap_err();
    assert!(error.to_string().contains("local data is retained"));
    engine.wait_message("engine-fixture-term");
    assert!(engine.child.try_wait().unwrap().is_none());
    assert_eq!(fs::read(engine.profile.join(crate::paths::PROFILE_DB)).unwrap(), before);
    let profile = engine.profile.clone();
    let started = Instant::now();
    let reset = std::thread::spawn(move || super::super::sign_out::reset_profile(&profile, None));
    engine.wait_message("engine-fixture-term");
    let executable = engine._directory.path().join("playbit-engine");
    assert!(super::super::launch::recover(&engine.profile, &executable).is_err());
    assert!(engine.child.try_wait().unwrap().is_none());
    reset.join().unwrap().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert_eq!(engine.child.wait().unwrap().signal(), Some(libc::SIGKILL));
    assert!(crate::store::LocalStore::open(&engine.profile).is_ok());
    assert!(other.child.try_wait().unwrap().is_none());
    assert_eq!(fs::read(other.profile.join(crate::paths::PROFILE_DB)).unwrap(), other_account);
}

#[test]
fn unrelated_socket_peer_is_never_signaled() {
    // The same-user test runner owns the endpoint, rather than an engine process.
    let (client, _server) = UnixStream::pair().unwrap();
    assert!(Peer::capture(&client).unwrap().stop_for_reset().is_err());
}
