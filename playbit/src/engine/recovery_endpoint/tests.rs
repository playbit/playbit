//! Native peers prove that recovery retires only the selected profile's engine.
#![allow(clippy::unwrap_used, reason = "Tests assert successful isolated process fixtures.")]
use super::*;
use std::{
    io::{BufRead, BufReader},
    os::{fd::OwnedFd, unix::process::ExitStatusExt},
    process::{Child, Command, Stdio},
};

const OWNER: &str = r#"
#define _POSIX_C_SOURCE 200809L
#define _DARWIN_C_SOURCE 1
#define _DEFAULT_SOURCE 1
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 3) return 1;
    int lock = open(argv[2], O_RDWR);
    if (lock < 0 || flock(lock, LOCK_EX | LOCK_NB) != 0) return 2;
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    int length = snprintf(address.sun_path, sizeof(address.sun_path), "%s", argv[1]);
    if (length < 0 || (size_t)length >= sizeof(address.sun_path)) return 3;
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        chmod(argv[1], 0600) != 0 || listen(listener, 16) != 0) return 4;
    puts("engine-fixture-ready");
    if (fflush(stdout) != 0) return 5;
    char byte;
    while (read(STDIN_FILENO, &byte, 1) < 0) {
        if (errno != EINTR) return 6;
    }
    return 0;
}
"#;

struct Fixture {
    directory: tempfile::TempDir,
    executable: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let executable = directory.path().join("playbit-engine");
        let source = directory.path().join("owner.c");
        fs::write(&source, OWNER).unwrap();
        assert!(
            Command::new("cc")
                .args(["-std=c17", "-Wall", "-Wextra", "-Werror", "-O2"])
                .arg(source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        Self { directory, executable }
    }
    fn start(&self, name: &str, channel: ReleaseChannel) -> Owner {
        let profile = self.directory.path().join(name);
        let selection =
            profile::Selection::open(self.directory.path(), Some(&profile), Some(channel)).unwrap();
        let profile = selection.path.clone();
        fs::create_dir_all(profile.join("control")).unwrap();
        // Exercise the same private directory and lifetime lock as production,
        // then let the separate process own that lock and recovery listener.
        drop(Endpoint::reserve(&profile).unwrap());
        let recovery = path(&profile).unwrap();
        let (ready, output) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let child = Command::new(&self.executable)
            .arg(&recovery)
            .arg(recovery.with_extension("lock"))
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(output)))
            .spawn()
            .unwrap();
        let owner = Owner { child, profile, recovery, _selection: selection };
        let mut line = String::new();
        BufReader::new(ready).read_line(&mut line).unwrap();
        assert_eq!(line, "engine-fixture-ready\n");
        owner
    }
}
struct Owner {
    child: Child,
    profile: PathBuf,
    recovery: PathBuf,
    _selection: profile::Selection,
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.recovery);
        let _ = fs::remove_file(self.recovery.with_extension("lock"));
    }
}

#[test]
fn lifetime_lease_prevents_opening_replacement_storage() {
    // Even after deleting all profile data, admission must reject a second
    // owner before invoking the callback that opens or creates its store.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let profile = directory.path().join("profile");
    fs::create_dir(&profile).unwrap();
    let owner = Endpoint::reserve(&profile).unwrap();
    // A concurrent fork retains this file description until exec.
    let inherited = owner.lease.try_clone().unwrap();
    fs::remove_dir(&profile).unwrap();

    let error = crate::engine::process::serve_with_ready(
        &profile,
        || panic!("a live owner must exclude replacement store initialization"),
        || Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("A Playbit engine is still running"), "{error}");
    assert!(!profile.exists());

    let lock = owner.path.with_extension("lock");
    drop(owner);
    drop(Endpoint::reserve(&profile).unwrap());
    drop(inherited);
    fs::remove_file(lock).unwrap();
}

#[test]
fn missing_profile_retires_authenticated_orphan_and_preserves_other_engine() {
    // A healthy profile survives reopening; deleting it leaves only the stable
    // endpoint, which must still authenticate and stop that exact process.
    let fixture = Fixture::new();
    let mut selected = fixture.start("selected", ReleaseChannel::Testing);
    let mut other = fixture.start("other", ReleaseChannel::Testing);
    let _main = UnixListener::bind(selected.profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
    fs::write(other.profile.join("retained-data"), b"unchanged").unwrap();
    retire_if_stale(&selected.profile, ReleaseChannel::Testing).unwrap();
    assert!(selected.child.try_wait().unwrap().is_none());
    fs::remove_dir_all(&selected.profile).unwrap();
    retire_if_stale(&selected.profile, ReleaseChannel::Testing).unwrap();
    assert_eq!(selected.child.wait().unwrap().signal(), Some(libc::SIGTERM));
    assert!(other.child.try_wait().unwrap().is_none());
    assert_eq!(fs::read(other.profile.join("retained-data")).unwrap(), b"unchanged");
    let socket = UnixStream::connect(&other.recovery).unwrap();
    Peer::capture(&socket).unwrap().require_engine().unwrap();
}

#[test]
fn deleted_personal_profile_retires_only_its_remembered_owners_engine() {
    let fixture = Fixture::new();
    let home = fixture.directory.path().canonicalize().unwrap();
    let mut owner = fixture.start("Personal", ReleaseChannel::Julia);
    let alias =
        home.with_file_name(home.file_name().unwrap().to_ascii_uppercase()).join("PERSONAL");
    let requested = if cfg!(target_os = "macos") && alias.is_dir() {
        assert_eq!(path(&alias).unwrap(), owner.recovery);
        alias
    } else {
        owner.profile.clone()
    };
    fs::remove_dir_all(&owner.profile).unwrap();

    for other in [ReleaseChannel::Public, ReleaseChannel::Testing, ReleaseChannel::Edward] {
        assert!(profile::recover_gui_profile(&home, &requested, other).is_err());
        assert!(profile::Selection::open(&home, Some(&requested), Some(other)).is_err());
        assert!(owner.child.try_wait().unwrap().is_none());
        assert!(!owner.profile.exists());
    }

    profile::recover_gui_profile(&home, &requested, ReleaseChannel::Julia).unwrap();
    assert_eq!(owner.child.wait().unwrap().signal(), Some(libc::SIGTERM));
    let reopened = profile::Selection::open(&home, Some(&requested), None).unwrap();
    assert_eq!(reopened.channel, ReleaseChannel::Julia);
    assert_eq!(fs::read(owner.profile.join("channel")).unwrap(), b"julia");
}

#[test]
fn wrong_channel_retires_same_profile_engine_without_changing_data() {
    // Channel mismatch is also a stale binding, even while the original primary
    // socket still exists. Retirement must leave persisted channel/data intact.
    let fixture = Fixture::new();
    let mut selected = fixture.start("selected", ReleaseChannel::Testing);
    let _main = UnixListener::bind(selected.profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
    fs::write(selected.profile.join("retained-data"), b"unchanged").unwrap();
    retire_if_stale(&selected.profile, ReleaseChannel::Public).unwrap();
    assert_eq!(selected.child.wait().unwrap().signal(), Some(libc::SIGTERM));
    assert_eq!(fs::read(selected.profile.join("channel")).unwrap(), b"testing");
    assert_eq!(fs::read(selected.profile.join("retained-data")).unwrap(), b"unchanged");
}

#[test]
fn recovery_rejects_non_engine_peer_without_removing_its_endpoint() {
    // The test runner owns this same-user socket, so successful connection is
    // insufficient authority to signal a process or replace its endpoint.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let profile = directory.path().join("profile");
    let mut endpoint = Endpoint::reserve(&profile).unwrap();
    endpoint.listen().unwrap();
    let error = retire_if_stale(&profile, ReleaseChannel::Testing).unwrap_err();
    assert!(error.to_string().contains("not a separate Playbit engine"), "{error}");
    assert!(UnixStream::connect(&endpoint.path).is_ok());
    let lock = endpoint.path.with_extension("lock");
    drop(endpoint);
    fs::remove_file(lock).unwrap();
}
