#![allow(
    clippy::unwrap_used,
    reason = "Tests drive real filesystem operations through event-driven worker completion."
)]
use super::super::staging::Area;
use super::*;

#[test]
#[cfg(all(feature = "process", not(target_os = "ios")))]
fn bundle_enumerations_share_the_verified_descriptor() {
    // Replacing a published pathname cannot change an admitted generation.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("app.pbapp");
    let package = crate::package::Package::new(
        "test".into(),
        "First".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        Default::default(),
    )
    .unwrap();
    std::fs::write(&path, package.to_bytes().unwrap()).unwrap();
    let source = Bundle::new(path.clone(), package.id().unwrap());
    let first = source.get().unwrap();
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, package.renamed("Second").unwrap().to_bytes().unwrap()).unwrap();
    std::fs::rename(replacement, path).unwrap();
    let second = source.get().unwrap();
    assert_eq!(first.digest(), second.digest());
    assert_eq!(second.name(), "First");
}
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    time::Duration,
};

struct Fixture {
    executor: Executor,
    notifications: mpsc::Receiver<()>,
    directory: tempfile::TempDir,
    _private: tempfile::TempDir,
    owner: Id,
    authority: Arc<Authority>,
    root: u64,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let file = Arc::new(File::open(staging.path()).unwrap());
        super::super::privacy::restrict_new(&file).unwrap();
        let private = Arc::new(PrivateObjects::default());
        let placement = Arc::new(Placement {
            snapshots: None,
            staging: Some(Area::new(file, private.clone()).unwrap()),
        });
        let (send, notifications) = mpsc::sync_channel(1);
        let mut executor = Executor::new(move || {
            let _ = send.try_send(());
        })
        .unwrap();
        let owner = Id::from_bytes([1; 16]);
        let authority = Arc::new(Authority::new(true));
        let ticket = executor
            .root(
                owner,
                Root {
                    path: directory.path().canonicalize().unwrap(),
                    private,
                    placement: PlacementSource::Ready(placement),
                    authority: authority.clone(),
                },
            )
            .unwrap();
        let mut fixture = Self {
            executor,
            notifications,
            directory,
            _private: staging,
            owner,
            authority,
            root: 0,
        };
        let Output::File { handle } = fixture.finish(&ticket).unwrap() else { panic!("root file") };
        fixture.root = handle;
        fixture
    }
    fn finish(&mut self, ticket: &Ticket) -> HostResult<Output> {
        loop {
            self.executor.maintain().unwrap();
            if ticket.ready() {
                return self.executor.take(ticket);
            }
            self.notifications
                .recv_timeout(Duration::from_secs(5))
                .expect("worker completion notification");
        }
    }
    fn begin(&mut self, file: u64, action: Action) -> Ticket {
        self.executor.begin(self.owner, Call { file, action }).unwrap()
    }
    fn open(&mut self, path: &[u8], mode: u32) -> u64 {
        let ticket = self.begin(self.root, Action::Open { path: path.to_vec(), mode });
        let Output::File { handle } = self.finish(&ticket).unwrap() else { panic!("opened file") };
        handle
    }
    fn drain(&mut self) {
        while self.executor.pending() {
            self.executor.maintain().unwrap();
            if self.executor.pending() {
                self.notifications
                    .recv_timeout(Duration::from_secs(5))
                    .expect("retiring worker completion");
            }
        }
    }
}

#[test]
fn completion_metadata_exhaustion_does_not_retain_file_quota() {
    // Reject opens at the real completion-table metadata boundary. All writer
    // quota must remain available without a worker wake or maintenance turn.
    let mut f = Fixture::new();
    f.executor.accepted = crate::capability::Table::new(crate::capability::Budget::new(0));
    for _ in 0..8 {
        assert!(matches!(
            f.executor.begin(
                f.owner,
                Call {
                    file: f.root,
                    action: Action::Open { path: b"rejected".to_vec(), mode: FILE_WRITE }
                }
            ),
            Err(Error::LimitExceeded)
        ));
    }
    f.executor.accepted = crate::capability::Table::new(crate::capability::Budget::backend());
    let tickets = (0..8)
        .map(|index| {
            f.begin(
                f.root,
                Action::Open { path: format!("writer-{index}").into_bytes(), mode: FILE_WRITE },
            )
        })
        .collect::<Vec<_>>();
    for ticket in tickets {
        assert!(matches!(f.finish(&ticket).unwrap(), Output::File { .. }));
    }
    assert!(!f.directory.path().join("rejected").exists());
}

#[test]
fn result_admission_failure_releases_completion_reservation() {
    // Fill real writer quota, then reject more opens with only one or two
    // completion slots available. A source-only status call must still fit.
    let mut f = Fixture::new();
    for index in 0..8 {
        f.open(format!("writer-{index}").as_bytes(), FILE_WRITE);
    }
    f.executor.accepted = crate::capability::Table::new(crate::capability::Budget::new(64));
    for _ in 0..4 {
        assert!(matches!(
            f.executor.begin(
                f.owner,
                Call {
                    file: f.root,
                    action: Action::Open { path: b"rejected".to_vec(), mode: FILE_WRITE }
                }
            ),
            Err(Error::LimitExceeded)
        ));
    }
    let status = f.begin(f.root, Action::Status);
    assert!(matches!(f.finish(&status).unwrap(), Output::Status { .. }));
}

#[test]
fn reused_completion_slots_preserve_accepted_write_order() {
    // Keep an earlier write in flight while recycling an earlier table index.
    // A later write must wait for that accepted operation to finish.
    let mut f = Fixture::new();
    let writer = f.open(b"ordered", FILE_WRITE);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let source = f.executor.slots.lookup(&writer).unwrap().cell.clone();
    *source.after_execute.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }));
    drop(source);
    let status = f.begin(f.root, Action::Status);
    let first = f.begin(writer, Action::Write { offset: 0, bytes: b"first".to_vec() });
    f.executor.maintain().unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    f.finish(&status).unwrap();
    let second = f.begin(writer, Action::Write { offset: 0, bytes: b"final".to_vec() });
    let close = f.begin(writer, Action::Close { flags: 0 });
    f.executor.maintain().unwrap();
    let overtook = f.executor.accepted.lookup(&second.id).unwrap().started;
    release.send(()).unwrap();
    f.finish(&first).unwrap();
    f.finish(&second).unwrap();
    assert!(matches!(
        f.finish(&close).unwrap(),
        Output::Close { publication: crate::runtime::host::files::PUBLISHED, .. }
    ));
    assert!(!overtook, "a reused slot cannot overtake an earlier write");
    assert_eq!(fs::read(f.directory.path().join("ordered")).unwrap(), b"final");
}

#[test]
fn close_barrier_keeps_abandoned_writes_and_parent_close_does_not_close_child() {
    let mut f = Fixture::new();
    let open = f.begin(f.root, Action::Open { path: b"nested/new".to_vec(), mode: FILE_WRITE });
    let parent_close = f.begin(f.root, Action::Close { flags: 0 });
    let Output::File { handle: writer } = f.finish(&open).unwrap() else { panic!("writer") };
    f.finish(&parent_close).unwrap();
    for bytes in [b"first".as_slice(), b"SECOND", b"final!"] {
        // Disposing delivery must not omit any accepted bytes from the close.
        drop(f.begin(writer, Action::Write { offset: 0, bytes: bytes.to_vec() }));
    }
    assert!(matches!(
        f.executor.begin(
            f.owner,
            Call {
                file: writer,
                action: Action::Close { flags: FILE_CLOSE_SYNC | FILE_CLOSE_DISCARD }
            }
        ),
        Err(Error::InvalidArgument)
    ));
    let status = f.begin(writer, Action::Status);
    let close = f.begin(writer, Action::Close { flags: 0 });
    assert!(matches!(
        f.executor.begin(
            f.owner,
            Call { file: writer, action: Action::Write { offset: 0, bytes: vec![9] } }
        ),
        Err(Error::Closed)
    ));
    assert_eq!(
        f.finish(&status).unwrap(),
        Output::Status {
            kind: FILE_KIND_FILE,
            flags: FILE_SIZE_VALID,
            size: 6,
            modification_time: 0
        }
    );
    assert_eq!(
        f.finish(&close).unwrap(),
        Output::Close {
            error: 0,
            publication: crate::runtime::host::files::PUBLISHED,
            recovery: None
        }
    );
    assert_eq!(fs::read(f.directory.path().join("nested/new")).unwrap(), b"final!");
    assert!(matches!(f.executor.take(&close), Err(Error::StaleHandle)));
}

#[test]
fn exclusive_failure_returns_private_recovery_and_revocation_withholds_its_handle() {
    let mut f = Fixture::new();
    let writer = f.open(b"new", FILE_WRITE | FILE_EXCLUSIVE);
    let write = f.begin(writer, Action::Write { offset: 2, bytes: b"saved".to_vec() });
    f.finish(&write).unwrap();
    fs::write(f.directory.path().join("new"), b"external").unwrap();
    let close = f.begin(writer, Action::Close { flags: 0 });
    let Output::Close { error, publication, recovery: Some(recovery) } = f.finish(&close).unwrap()
    else {
        panic!("recovery")
    };
    assert_eq!(error, Error::Conflict as i32);
    assert_eq!(publication, crate::runtime::host::files::NOT_PUBLISHED);
    let read = f.begin(recovery, Action::Read { offset: 0, length: 32 });
    assert_eq!(f.finish(&read).unwrap(), Output::Read { bytes: b"\0\0saved".to_vec() });
    let status = f.begin(recovery, Action::Status);
    assert_eq!(
        f.finish(&status).unwrap(),
        Output::Status {
            kind: FILE_KIND_FILE,
            flags: FILE_SIZE_VALID | FILE_CONTENTS_IMMUTABLE,
            size: 7,
            modification_time: 0
        }
    );
    assert_eq!(fs::read(f.directory.path().join("new")).unwrap(), b"external");
    let writer = f.open(b"other", FILE_WRITE | FILE_EXCLUSIVE);
    let write = f.begin(writer, Action::Write { offset: 0, bytes: b"private".to_vec() });
    f.finish(&write).unwrap();
    fs::write(f.directory.path().join("other"), b"occupied").unwrap();
    let close = f.begin(writer, Action::Close { flags: 0 });
    f.drain();
    f.authority.revoke().unwrap();
    assert_eq!(
        f.finish(&close).unwrap(),
        Output::Close {
            error: Error::Conflict as i32,
            publication: crate::runtime::host::files::NOT_PUBLISHED,
            recovery: None
        }
    );
    assert!(matches!(
        f.executor
            .begin(f.owner, Call { file: recovery, action: Action::Read { offset: 0, length: 1 } }),
        Err(Error::AccessDenied)
    ));
}

#[test]
fn list_close_retires_ready_or_pending_next_and_identity_is_scoped_to_owner() {
    let mut f = Fixture::new();
    fs::write(f.directory.path().join("file"), b"data").unwrap();
    let open = f.begin(f.root, Action::ListOpen);
    let Output::List { handle: list } = f.finish(&open).unwrap() else { panic!("list") };
    let parent = f.begin(f.root, Action::Close { flags: 0 });
    f.finish(&parent).unwrap();
    let first = f.begin(list, Action::ListNext { index: 0 });
    assert!(matches!(
        f.executor.begin(f.owner, Call { file: list, action: Action::ListNext { index: 0 } }),
        Err(Error::Busy)
    ));
    assert!(matches!(
        f.executor.begin(
            Id::from_bytes([2; 16]),
            Call { file: list, action: Action::ListNext { index: 0 } }
        ),
        Err(Error::StaleHandle)
    ));
    let retirement = f.executor.close_list(f.owner, list).unwrap();
    assert!(!retirement.ready());
    assert!(first.ready());
    assert_eq!(f.executor.take(&first), Err(Error::Closed));
    f.drain();
    assert_eq!(f.executor.take(&first), Err(Error::StaleHandle));
}

#[test]
fn teardown_and_engine_shutdown_finish_accepted_closes_but_discard_other_writers() {
    let mut f = Fixture::new();
    let saved = f.open(b"saved", FILE_WRITE);
    let discarded = f.open(b"discarded", FILE_WRITE);
    drop(f.begin(saved, Action::Write { offset: 0, bytes: b"before".to_vec() }));
    drop(f.begin(saved, Action::Write { offset: 0, bytes: b"latest".to_vec() }));
    drop(f.begin(saved, Action::Close { flags: 0 }));
    drop(f.begin(discarded, Action::Write { offset: 0, bytes: b"never saved".to_vec() }));
    f.executor.retire(f.owner);
    f.drain();
    assert_eq!(fs::read(f.directory.path().join("saved")).unwrap(), b"latest");
    assert!(!f.directory.path().join("discarded").exists());
    // Shutdown must preserve ordering even before any queued operation starts.
    let mut f = Fixture::new();
    let writer = f.open(b"shutdown", FILE_WRITE);
    for value in 0..32 {
        drop(f.begin(writer, Action::Write { offset: 0, bytes: vec![value] }));
    }
    drop(f.begin(writer, Action::Close { flags: 0 }));
    drop(f.executor);
    assert_eq!(fs::read(f.directory.path().join("shutdown")).unwrap(), vec![31]);
}

#[test]
fn sibling_staging_is_hidden_across_grants_and_discard_keeps_created_parents() {
    let mut f = Fixture::new();
    let private = Arc::new(PrivateObjects::default());
    let root = f
        .executor
        .root(
            f.owner,
            Root {
                path: f.directory.path().canonicalize().unwrap(),
                private: private.clone(),
                authority: f.authority.clone(),
                placement: PlacementSource::Ready(Arc::new(Placement::default())),
            },
        )
        .unwrap();
    let Output::File { handle } = f.finish(&root).unwrap() else { panic!("root") };
    f.root = handle;
    let writer = f.open(b"nested/new", FILE_WRITE);
    let write = f.begin(writer, Action::Write { offset: 0, bytes: b"private".to_vec() });
    f.finish(&write).unwrap();
    let staged = fs::read_dir(f.directory.path().join("nested"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    use std::os::unix::ffi::OsStrExt;
    let other = Directory::root(
        &f.directory.path().canonicalize().unwrap(),
        private,
        Arc::new(Authority::new(true)),
    )
    .unwrap();
    let nested = other.directory(b"nested").unwrap();
    assert!(nested.list().unwrap().is_empty());
    assert!(nested.reader(staged.as_bytes()).is_err());
    let close = f.begin(writer, Action::Close { flags: FILE_CLOSE_DISCARD });
    f.finish(&close).unwrap();
    f.drain();
    assert!(f.directory.path().join("nested").is_dir());
    assert_eq!(fs::read_dir(f.directory.path().join("nested")).unwrap().count(), 0);
    let writer = f.open(b"nested/new", FILE_WRITE);
    let close = f.begin(writer, Action::Close { flags: 0 });
    f.finish(&close).unwrap();
    assert_eq!(fs::read(f.directory.path().join("nested/new")).unwrap(), b"");
}

#[test]
fn retirement_keeps_the_context_lease_until_resource_credit_is_released() {
    let mut f = Fixture::new();
    let pin = f.executor.slots.lookup(&f.root).unwrap().cell.credit.clone();
    f.executor.retire(f.owner);
    f.drain();
    assert!(f.executor.owner_retained(f.owner));
    drop(pin);
    // A worker may still hold the queued release after it left the owner queue.
    loop {
        f.executor.maintain().unwrap();
        if !f.executor.owner_retained(f.owner) {
            break;
        }
        f.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert!(!f.executor.budget.retained(f.owner));
}

#[test]
fn discard_of_a_sealed_writer_waits_for_accepted_publication_instead_of_replacing_it() {
    let mut f = Fixture::new();
    let writer = f.open(b"accepted", FILE_WRITE);
    drop(f.begin(writer, Action::Write { offset: 0, bytes: b"saved".to_vec() }));
    let close = f.begin(writer, Action::Close { flags: 0 });
    let retirement = f.executor.discard(f.owner, writer).unwrap();
    assert!(!retirement.ready());
    assert!(matches!(f.finish(&close).unwrap(), Output::Close { error: 0, recovery: None, .. }));
    assert!(retirement.ready());
    assert_eq!(fs::read(f.directory.path().join("accepted")).unwrap(), b"saved");
}

#[test]
fn disposal_needs_no_operation_slot_and_preserves_accepted_writes() {
    let mut f = Fixture::new();
    let writer = f.open(b"discarded", FILE_WRITE);
    let pending: Vec<_> = (0..64)
        .map(|offset| f.begin(writer, Action::Write { offset, bytes: vec![offset as u8] }))
        .collect();
    assert!(matches!(
        f.executor.begin(f.owner, Call { file: writer, action: Action::Status }),
        Err(Error::LimitExceeded)
    ));
    let retirement = f.executor.discard(f.owner, writer).unwrap();
    assert!(!retirement.ready());
    assert!(matches!(
        f.executor.begin(f.owner, Call { file: writer, action: Action::Status }),
        Err(Error::StaleHandle)
    ));
    for ticket in pending {
        assert!(matches!(f.finish(&ticket), Ok(Output::Written)));
    }
    f.drain();
    assert!(retirement.ready());
    assert!(!f.directory.path().join("discarded").exists());
}

struct VolumeFixture {
    files: Fixture,
    store: Arc<crate::store::LocalStore>,
    access: crate::store::Access,
    volume: Id,
    _profile: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
}
impl VolumeFixture {
    fn new() -> Self {
        let mut files = Fixture::new();
        let close = files.begin(files.root, Action::Close { flags: 0 });
        files.finish(&close).unwrap();
        let profile = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::LocalStore::open(profile.path()).unwrap());
        let access = crate::store::Access::owner(store.account().id);
        let volume = store.create_volume("Worker Files", false).unwrap().id;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let transport = crate::engine::blob::BlobTransport::new_local(
            runtime.handle().clone(),
            Arc::new(Default::default()),
        );
        let private = Arc::new(PrivateObjects::default());
        let placement = Arc::new(Placement {
            snapshots: None,
            staging: Some(
                Area::new(Arc::new(File::open(files._private.path()).unwrap()), private).unwrap(),
            ),
        });
        let root = files
            .executor
            .volume_root(
                files.owner,
                VolumeRoot {
                    store: store.clone(),
                    access: access.clone(),
                    volume,
                    session: None,
                    transport,
                    authority: files.authority.clone(),
                    placement: PlacementSource::Ready(placement),
                },
            )
            .unwrap();
        let Output::File { handle } = files.finish(&root).unwrap() else { panic!("volume root") };
        files.root = handle;
        Self { files, store, access, volume, _profile: profile, _runtime: runtime }
    }
}

#[test]
fn volume_preparation_coalesces_and_keeps_credit_through_retirement() {
    // Substitute only the two-slot mailbox to deterministically fill it, then
    // hand an accepted preparation to the real pool after retiring its owner.
    let mut f = VolumeFixture::new();
    let fixture = tempfile::tempdir().unwrap();
    let managed = Arc::new(super::super::private_storage::Managed::new(
        fixture.path().to_owned(),
        Arc::new(PrivateObjects::default()),
    ));
    let root = VolumeRoot {
        store: f.store.clone(),
        access: f.access.clone(),
        volume: f.volume,
        session: None,
        transport: crate::engine::blob::BlobTransport::local(),
        authority: f.files.authority.clone(),
        placement: PlacementSource::Managed(managed.clone()),
    };
    let (send, receive) = mpsc::sync_channel(2);
    let original = f.files.executor.pool.send.replace(send).unwrap();
    let cell = f.files.executor.slots.lookup(&f.files.root).unwrap().cell.clone();
    for _ in 0..2 {
        assert!(
            f.files
                .executor
                .pool
                .send
                .as_ref()
                .unwrap()
                .try_send(Job::Release(cell.clone()))
                .is_ok()
        );
    }
    f.files.executor.prepare_volume_write(f.files.owner, &root);
    assert!(!managed.preparation_started.load(Ordering::Acquire));
    // A rejected job must release its operation credit immediately.
    let credits = (0..64)
        .map(|_| f.files.executor.budget.operation(f.files.owner, 0).unwrap())
        .collect::<Vec<_>>();
    drop(credits);
    drop(receive.try_recv().unwrap());
    drop(receive.try_recv().unwrap());
    drop(cell);
    f.files.executor.prepare_volume_write(f.files.owner, &root);
    f.files.executor.prepare_volume_write(f.files.owner, &root);
    assert!(managed.preparation_started.load(Ordering::Acquire));
    let job = receive.try_recv().unwrap();
    assert!(matches!(job, Job::Prepare { .. }));
    assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
    f.files.executor.pool.send = Some(original);
    f.files.executor.retire(f.files.owner);
    f.files.drain();
    assert!(f.files.executor.owner_retained(f.files.owner), "preparation owns its credit");
    assert!(f.files.executor.pool.send.as_ref().unwrap().try_send(job).is_ok());
    while f.files.executor.owner_retained(f.files.owner) {
        f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
        f.files.executor.maintain().unwrap();
    }
    drop(root);
    drop(f);
    drop(managed);
}

#[test]
fn volume_preparation_failure_preserves_reads_and_writer_retries() {
    // Exercise both native memory-only root admission and worker root admission
    // with unusable private placement, then repair it before a normal write.
    for native in [false, true] {
        let mut f = VolumeFixture::new();
        f.store.create_blob(&f.access, f.volume, "file", b"readable").unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let obstruction = fixture.path().join("file-io");
        fs::write(&obstruction, b"not a directory").unwrap();
        let managed = Arc::new(super::super::private_storage::Managed::new(
            fixture.path().to_owned(),
            Arc::new(PrivateObjects::default()),
        ));
        let root = VolumeRoot {
            store: f.store.clone(),
            access: f.access.clone(),
            volume: f.volume,
            session: None,
            transport: crate::engine::blob::BlobTransport::local(),
            authority: f.files.authority.clone(),
            placement: PlacementSource::Managed(managed.clone()),
        };
        f.files.root = if native {
            f.files.executor.admit_volume_root(f.files.owner, root).unwrap()
        } else {
            let ticket = f.files.executor.volume_root(f.files.owner, root).unwrap();
            let Output::File { handle } = f.files.finish(&ticket).unwrap() else { panic!("root") };
            handle
        };
        assert!(managed.preparation_started.load(Ordering::Acquire));
        // The queued preparation is the only extra Managed owner. Its final
        // reference retires before the existing worker completion notification.
        while Arc::strong_count(&managed) > 2 {
            f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let reader = f.files.open(b"file", FILE_READ);
        let read = f.files.begin(reader, Action::Read { offset: 0, length: 8 });
        assert_eq!(f.files.finish(&read).unwrap(), Output::Read { bytes: b"readable".to_vec() });
        let write =
            f.files.begin(f.files.root, Action::Open { path: b"file".to_vec(), mode: FILE_WRITE });
        assert!(f.files.finish(&write).is_err());
        fs::remove_file(obstruction).unwrap();
        let writer = f.files.open(b"file", FILE_WRITE);
        let write =
            f.files.begin(writer, Action::Write { offset: 0, bytes: b"recovered".to_vec() });
        f.files.finish(&write).unwrap();
        let close = f.files.begin(writer, Action::Close { flags: 0 });
        assert!(matches!(f.files.finish(&close).unwrap(), Output::Close { error: 0, .. }));
        drop(f);
        drop(managed);
    }
}

#[test]
fn volume_worker_close_preserves_accepted_writes_and_pinned_reader_revision() {
    let mut f = VolumeFixture::new();
    let original = f.store.create_blob(&f.access, f.volume, "folder/file", b"original").unwrap();
    let reader = f.files.open(b"folder/file", FILE_READ);
    let writer = f.files.open(b"folder/file", FILE_WRITE);
    drop(f.files.begin(writer, Action::Write { offset: 3, bytes: b"last".to_vec() }));
    drop(f.files.begin(writer, Action::Write { offset: 0, bytes: b"new".to_vec() }));
    let close = f.files.begin(writer, Action::Close { flags: FILE_CLOSE_SYNC });
    assert_eq!(
        f.files.finish(&close).unwrap(),
        Output::Close {
            error: 0,
            publication: crate::runtime::host::files::PUBLISHED,
            recovery: None,
        }
    );
    assert_eq!(f.store.read_blob_entry(&f.access, f.volume, original.id).unwrap(), b"newlast");
    let read = f.files.begin(reader, Action::Read { offset: 0, length: 32 });
    assert_eq!(f.files.finish(&read).unwrap(), Output::Read { bytes: b"original".to_vec() });
    let status = f.files.begin(reader, Action::Status);
    let Output::Status { flags, size, modification_time, .. } = f.files.finish(&status).unwrap()
    else {
        panic!("status")
    };
    assert_eq!(size, 8);
    assert_eq!(flags, FILE_SIZE_VALID | FILE_MODIFICATION_TIME_VALID | FILE_CONTENTS_IMMUTABLE);
    assert!(modification_time > 0);
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            original.id,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    let read = f.files.begin(reader, Action::Read { offset: 0, length: 0 });
    assert_eq!(f.files.finish(&read), Err(Error::AccessDenied));
    let close = f.files.begin(reader, Action::Close { flags: 0 });
    assert!(f.files.finish(&close).is_ok(), "revocation does not prevent cleanup");
}

#[test]
fn volume_directory_deletion_keeps_captured_list_and_complete_recovery_until_revocation() {
    let mut f = VolumeFixture::new();
    f.store.create_blob(&f.access, f.volume, "folder/seed", b"seed").unwrap();
    let parent = f.store.resolve_entry(&f.access, f.volume, "folder").unwrap().id;
    let directory = f.files.open(b"folder", FILE_DIRECTORY);
    let list = f.files.begin(directory, Action::ListOpen);
    let Output::List { handle: list } = f.files.finish(&list).unwrap() else { panic!("list") };
    let writer = f.files.open(b"folder/new", FILE_WRITE);
    let write = f.files.begin(writer, Action::Write { offset: 0, bytes: b"complete".to_vec() });
    f.files.finish(&write).unwrap();
    f.store.remove_entry(&f.access, f.volume, parent).unwrap();
    let next = f.files.begin(list, Action::ListNext { index: 0 });
    assert_eq!(
        f.files.finish(&next).unwrap(),
        Output::Entry { kind: FILE_KIND_FILE, name: b"seed".to_vec() }
    );
    let open = f.files.begin(directory, Action::Open { path: b"seed".to_vec(), mode: FILE_READ });
    assert_eq!(f.files.finish(&open), Err(Error::DestinationDeleted));
    let close = f.files.begin(writer, Action::Close { flags: 0 });
    let Output::Close { error, publication, recovery: Some(recovery) } =
        f.files.finish(&close).unwrap()
    else {
        panic!("complete recovery")
    };
    assert_eq!(error, Error::DestinationDeleted as i32);
    assert_eq!(publication, crate::runtime::host::files::NOT_PUBLISHED);
    let read = f.files.begin(recovery, Action::Read { offset: 0, length: 32 });
    assert_eq!(f.files.finish(&read).unwrap(), Output::Read { bytes: b"complete".to_vec() });
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            parent,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    let read = f.files.begin(recovery, Action::Read { offset: 0, length: 32 });
    assert_eq!(f.files.finish(&read), Err(Error::AccessDenied));
    let next = f.files.begin(list, Action::ListNext { index: 0 });
    assert_eq!(f.files.finish(&next), Err(Error::AccessDenied));
    let close = f.files.begin(directory, Action::Close { flags: 0 });
    assert!(f.files.finish(&close).is_ok());
}

#[test]
fn volume_ready_bytes_and_recovery_are_fenced_before_take() {
    let mut f = VolumeFixture::new();
    let entry = f.store.create_blob(&f.access, f.volume, "file", b"private").unwrap();
    let reader = f.files.open(b"file", FILE_READ);
    let read = f.files.begin(reader, Action::Read { offset: 0, length: 32 });
    loop {
        f.files.executor.maintain().unwrap();
        if read.ready() {
            break;
        }
        f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let status = f.files.begin(reader, Action::Status);
    let open_list = f.files.begin(f.files.root, Action::ListOpen);
    let Output::List { handle: list } = f.files.finish(&open_list).unwrap() else {
        panic!("captured root listing")
    };
    let next = f.files.begin(list, Action::ListNext { index: 0 });
    loop {
        f.files.executor.maintain().unwrap();
        if status.ready() && next.ready() {
            break;
        }
        f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            entry.id,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    for ticket in [&read, &status, &next] {
        assert_eq!(f.files.executor.take(ticket), Err(Error::Unavailable));
    }

    let writer = f.files.open(b"new", FILE_WRITE | FILE_EXCLUSIVE);
    let write = f.files.begin(writer, Action::Write { offset: 0, bytes: b"recovery".to_vec() });
    f.files.finish(&write).unwrap();
    f.store.create_blob(&f.access, f.volume, "new", b"occupied").unwrap();
    let close = f.files.begin(writer, Action::Close { flags: 0 });
    loop {
        f.files.executor.maintain().unwrap();
        if close.ready() {
            break;
        }
        f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    // Even a broader policy-history change must conservatively withhold a
    // prepared recovery capability while retaining the known close outcome.
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            entry.id,
            crate::store::object_contract::EntryAcl::Inherit,
        )
        .unwrap();
    assert_eq!(
        f.files.executor.take(&close),
        Err(Error::WouldBlock),
        "revoked recovery retires before the known receipt is consumed"
    );
    assert_eq!(
        f.files.finish(&close).unwrap(),
        Output::Close {
            error: Error::Conflict as i32,
            publication: crate::runtime::host::files::NOT_PUBLISHED,
            recovery: None,
        }
    );
}

#[test]
fn accepted_store_write_denial_poisoning_survives_permission_restoration() {
    let mut f = VolumeFixture::new();
    let entry = f.store.create_blob(&f.access, f.volume, "file", b"original").unwrap();
    let writer = f.files.open(b"file", FILE_WRITE);
    let write = f.files.begin(writer, Action::Write { offset: 0, bytes: b"prefix".to_vec() });
    f.files.finish(&write).unwrap();
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            entry.id,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    let denied = f.files.begin(writer, Action::Write { offset: 6, bytes: b"tail".to_vec() });
    assert_eq!(f.files.finish(&denied), Err(Error::AccessDenied));
    f.store
        .set_entry_permissions(
            &f.access,
            f.volume,
            entry.id,
            crate::store::object_contract::EntryAcl::Inherit,
        )
        .unwrap();
    let close = f.files.begin(writer, Action::Close { flags: 0 });
    let Output::Close { error, publication, recovery } = f.files.finish(&close).unwrap() else {
        panic!("close receipt")
    };
    assert_ne!(error, 0);
    assert_eq!(publication, crate::runtime::host::files::NOT_PUBLISHED);
    assert_eq!(recovery, None);
    assert_eq!(f.store.read_blob_entry(&f.access, f.volume, entry.id).unwrap(), b"original");
}

#[test]
fn volume_policy_change_during_execution_revalidates_without_replay() {
    let mut f = VolumeFixture::new();
    let entry = f.store.create_blob(&f.access, f.volume, "file", b"private").unwrap();
    let reader = f.files.open(b"file", FILE_READ);
    let store = f.store.clone();
    let access = f.access.clone();
    let volume = f.volume;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let source = f.files.executor.slots.lookup(&reader).unwrap().cell.clone();
    *source.after_execute.lock().unwrap() = Some(Box::new(move || {
        observed.fetch_add(1, Ordering::Relaxed);
        store
            .set_entry_permissions(
                &access,
                volume,
                entry.id,
                crate::store::object_contract::EntryAcl::Replace(Default::default()),
            )
            .unwrap();
    }));
    let read = f.files.begin(reader, Action::Read { offset: 0, length: 32 });
    // The worker must reject during fallback authorization, before the owner
    // could instead report an expired ready-result fence as Unavailable.
    assert_eq!(f.files.finish(&read), Err(Error::AccessDenied));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn volume_open_revalidates_changed_permissions_before_handle_delivery() {
    // Revoke the exact opened entry both during execution and after the worker
    // finishes. Neither a reused check nor a queued result may deliver a handle.
    for mode in [FILE_READ, FILE_WRITE, FILE_DIRECTORY] {
        for after_ready in [false, true] {
            let mut f = VolumeFixture::new();
            let entry =
                f.store.create_blob(&f.access, f.volume, "folder/file", b"private").unwrap();
            let (path, target) = if mode == FILE_DIRECTORY {
                (
                    b"folder".to_vec(),
                    f.store.resolve_entry(&f.access, f.volume, "folder").unwrap().id,
                )
            } else {
                (b"folder/file".to_vec(), entry.id)
            };
            let store = f.store.clone();
            let access = f.access.clone();
            let volume = f.volume;
            let revoke = move || {
                store
                    .set_entry_permissions(
                        &access,
                        volume,
                        target,
                        crate::store::object_contract::EntryAcl::Replace(Default::default()),
                    )
                    .unwrap();
            };
            if after_ready {
                let ticket = f.files.begin(f.files.root, Action::Open { path, mode });
                loop {
                    f.files.executor.maintain().unwrap();
                    if ticket.ready() {
                        break;
                    }
                    f.files.notifications.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                revoke();
                assert_eq!(f.files.executor.take(&ticket), Err(Error::Unavailable));
            } else {
                let source = f.files.executor.slots.lookup(&f.files.root).unwrap().cell.clone();
                *source.after_execute.lock().unwrap() = Some(Box::new(revoke));
                let ticket = f.files.begin(f.files.root, Action::Open { path, mode });
                assert_eq!(f.files.finish(&ticket), Err(Error::AccessDenied));
            }
        }
    }
}

#[test]
fn volume_open_changed_namespace_preserves_reader_but_rejects_deleted_directory() {
    // The same deletion between execution and delivery invalidates an opened
    // directory, while the already captured reader retains its immutable bytes.
    for mode in [FILE_READ, FILE_DIRECTORY] {
        let mut f = VolumeFixture::new();
        let entry = f.store.create_blob(&f.access, f.volume, "folder/file", b"captured").unwrap();
        let directory = f.store.resolve_entry(&f.access, f.volume, "folder").unwrap().id;
        let store = f.store.clone();
        let access = f.access.clone();
        let volume = f.volume;
        let target = if mode == FILE_READ { entry.id } else { directory };
        let source = f.files.executor.slots.lookup(&f.files.root).unwrap().cell.clone();
        *source.after_execute.lock().unwrap() = Some(Box::new(move || {
            store.remove_entry(&access, volume, target).unwrap();
        }));
        let path = if mode == FILE_READ { b"folder/file".to_vec() } else { b"folder".to_vec() };
        let ticket = f.files.begin(f.files.root, Action::Open { path, mode });
        if mode == FILE_DIRECTORY {
            assert_eq!(f.files.finish(&ticket), Err(Error::DestinationDeleted));
        } else {
            let Output::File { handle } = f.files.finish(&ticket).unwrap() else {
                panic!("reader")
            };
            let read = f.files.begin(handle, Action::Read { offset: 0, length: 32 });
            assert_eq!(
                f.files.finish(&read).unwrap(),
                Output::Read { bytes: b"captured".to_vec() }
            );
        }
    }
}

#[test]
#[cfg(feature = "ap-identity")]
fn automatic_file_aliases_stay_lazy_until_one_scope_is_used() {
    // Both native and process roots are metadata-only. The first real File
    // operation materializes its scope and still enforces store authority.
    for native in [false, true] {
        use crate::package::{AutomaticVolumeRole as Role, Package};
        use crate::store::{Access, BuildSelector, LocalStore, SessionCreation};
        let mut files = Fixture::new();
        let profile = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(profile.path()).unwrap());
        let package = crate::package::signature::testing::signed(
            &store,
            Package::new(
                "lazy-files".into(),
                "Lazy Files".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                Default::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let app = package.identity().unwrap().clone();
        let build = package.id().unwrap();
        let path = profile.path().join(crate::paths::BUILDS);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(&build), package.to_bytes().unwrap()).unwrap();
        store.remember_imported_app(&package).unwrap();
        let root = app.volume_id(Role::Root, None, None, None).unwrap();
        let session = store
            .create_session_binding(SessionCreation {
                app,
                selector: BuildSelector::Exact { build_id: build.clone() },
                resolved_build_id: build,
                root_volume: root,
                user_volume: None,
                sync_shared: true,
            })
            .unwrap();
        let access =
            Access::session_version(store.account().id, &session.id, &session.resolved_build_id);
        let slots = store.automatic_volumes(&session.id).unwrap();
        let placement =
            PlacementSource::Ready(Arc::new(Placement { snapshots: None, staging: None }));
        let mut user = None;
        for slot in &slots {
            assert!(matches!(store.volume(slot.volume), Err(crate::store::Error::NotFound(_))));
            let root = VolumeRoot {
                store: store.clone(),
                access: access.clone(),
                volume: slot.volume,
                session: Some(session.id.clone()),
                transport: crate::engine::blob::BlobTransport::local(),
                authority: files.authority.clone(),
                placement: placement.clone(),
            };
            let handle = if native {
                files.executor.admit_volume_root(files.owner, root).unwrap()
            } else {
                let ticket = files.executor.volume_root(files.owner, root).unwrap();
                let Output::File { handle } = files.finish(&ticket).unwrap() else {
                    panic!("root")
                };
                handle
            };
            if slot.role == Role::User {
                user = Some(handle);
            }
        }
        for slot in &slots {
            assert!(
                matches!(store.volume(slot.volume), Err(crate::store::Error::NotFound(_))),
                "registering aliases does not create data"
            );
        }
        let listing = files.begin(user.unwrap(), Action::ListOpen);
        assert!(matches!(files.finish(&listing), Ok(Output::List { .. })));
        for slot in &slots {
            if slot.role == Role::User {
                assert_eq!(store.volume(slot.volume).unwrap().owner, store.account().id);
            } else {
                assert!(matches!(store.volume(slot.volume), Err(crate::store::Error::NotFound(_))));
            }
        }
        let ordinary = store.create_volume("Ordinary", false).unwrap();
        for (account, allowed) in [(store.account().id, true), (Id::from_bytes([0x72; 16]), false)]
        {
            let root = VolumeRoot {
                store: store.clone(),
                access: Access::owner(account),
                volume: ordinary.id,
                session: None,
                transport: crate::engine::blob::BlobTransport::local(),
                authority: files.authority.clone(),
                placement: placement.clone(),
            };
            let handle = if native {
                files.executor.admit_volume_root(files.owner, root).unwrap()
            } else {
                let ticket = files.executor.volume_root(files.owner, root).unwrap();
                let Output::File { handle } = files.finish(&ticket).unwrap() else {
                    panic!("root")
                };
                handle
            };
            let status = files.begin(handle, Action::Status);
            if allowed {
                assert!(matches!(files.finish(&status), Ok(Output::Status { .. })));
            } else {
                assert_eq!(files.finish(&status), Err(Error::AccessDenied));
            }
        }
    }
}
