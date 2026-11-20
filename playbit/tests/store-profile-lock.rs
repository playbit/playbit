//! Real forked-process profile ownership regression on Unix hosts.
#![cfg(all(unix, feature = "local-store"))]
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
#![allow(unsafe_code, reason = "Child after fork uses only async-signal-safe libc operations.")]
use playbit::store::{Error, LocalStore};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    time::Duration,
};

#[test]
fn retired_profile_releases_ownership_while_a_forked_child_waits() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let account = store.account().id;
    let volume = store.create_volume("Retained through owner retirement", false).unwrap();
    let (mut ready_parent, ready_child) = UnixStream::pair().unwrap();
    let (mut release_parent, release_child) = UnixStream::pair().unwrap();
    ready_parent.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (ready_fd, release_fd) = (ready_child.as_raw_fd(), release_child.as_raw_fd());
    let (parent_ready_fd, parent_release_fd) =
        (ready_parent.as_raw_fd(), release_parent.as_raw_fd());
    // SAFETY: after fork the child only calls async-signal-safe libc operations
    // on preexisting descriptors, then _exit; no Rust destructor or allocator.
    let child = unsafe {
        let child = libc::fork();
        if child == 0 {
            libc::alarm(5);
            libc::close(parent_ready_fd);
            libc::close(parent_release_fd);
            let mut byte = 1u8;
            if libc::write(ready_fd, (&byte as *const u8).cast(), 1) != 1 {
                libc::_exit(2);
            }
            let received = libc::read(release_fd, (&mut byte as *mut u8).cast(), 1);
            libc::_exit(if received == 1 { 0 } else { 3 });
        }
        child
    };
    assert!(child > 0, "fork failed: {}", std::io::Error::last_os_error());
    drop(ready_child);
    drop(release_child);
    let ready = ready_parent.read_exact(&mut [0]);
    let live_owner_rejected =
        matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_)));
    drop(store);
    let reopened = LocalStore::open(directory.path());
    let released = release_parent.write_all(&[1]);
    let mut status = 0;
    // SAFETY: child is our positive fork result and status is writable.
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    ready.unwrap();
    released.unwrap();
    assert_eq!(status, 0, "child exited unsuccessfully");
    assert!(live_owner_rejected);
    let reopened = reopened.unwrap();
    assert_eq!(reopened.account().id, account);
    assert_eq!(reopened.volume(volume.id).unwrap().name, volume.name);
    assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
}

#[test]
#[ignore = "operator-only optimized isolated profile open/drop components"]
#[allow(clippy::assertions_on_constants, reason = "Reject unoptimized timing at runtime.")]
fn profile_owner_lifetime_cost() {
    assert!(!cfg!(debug_assertions), "use --release");
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let account = store.account().id;
    drop(store);
    let mut opened = Vec::with_capacity(100);
    let mut closed = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = std::time::Instant::now();
        let store = LocalStore::open(directory.path()).unwrap();
        opened.push(start.elapsed().as_nanos());
        assert_eq!(store.account().id, account);
        let start = std::time::Instant::now();
        drop(store);
        closed.push(start.elapsed().as_nanos());
    }
    println!("PROFILE_LIFETIME_COST {{\"open_ns\":{opened:?},\"drop_ns\":{closed:?}}}");
}
