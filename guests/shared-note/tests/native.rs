use playbit::engine::{
    Engine,
    data_host::DataHost,
    in_process_host::{InProcessHost, Inbox},
};
use playbit::package::Package;
use playbit::runtime::{ExecutionBackend, Runtime, syscall::*};
use playbit::store::{Access, Id as StoreId};
use playbit_test_fixtures::Guest;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

fn context(shared: [u8; 16], root: [u8; 16]) -> GuestContext {
    GuestContext {
        app_name: "Shared note fixture".into(),
        app_build: None,
        identity: Identity {
            display_name: String::new(),
            account_id: [5; 16],
            device_id: [6; 16],
            instance_id: [7; 16],
            is_service: false,
            session_id: None,
        },
        volumes: vec![
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: shared,
                name: "Root".into(),
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: false,
                role: playbit_api::context::VolumeRole::Session as u32,
            },
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: root,
                name: "Fixture Root".into(),
                tags: vec![],
                rights: VolumeRights::READ,
                is_root: true,
                role: 0,
            },
        ],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: vec![],
    }
}
struct Owner {
    guest: GuestInstance,
    inbox: Inbox,
    wake: mpsc::Receiver<()>,
    view: Vec<u8>,
}
impl Owner {
    fn open(engine: &Engine, session: &str) -> Self {
        let session = engine.services.store.session(session).unwrap();
        let (send, wake) = mpsc::sync_channel(1);
        let (mut host, inbox) = InProcessHost::local(
            DataHost {
                store: engine.services.store.clone(),
                access: Access::session_version(
                    engine.services.store.account().id,
                    &session.id,
                    &session.resolved_build_id,
                ),
                changed: Arc::new(AtomicBool::new(false)),
            },
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        let root = engine.services.store.create_volume("Adapter fixture Root", true).unwrap();
        let mut grants =
            engine.services.store.personal_volume_grants(&session.id).unwrap().unwrap_or_default();
        grants.push(playbit::store::PersonalVolumeGrant { volume: root.id, write: false });
        engine.services.store.select_personal_volumes(&session.id, grants).unwrap();
        let mut context = context(*session.root_volume.as_bytes(), *root.id.as_bytes());
        context.volumes = host
            .enable_files(
                &engine.services,
                Access::session_version(
                    engine.services.store.account().id,
                    &session.id,
                    &session.resolved_build_id,
                ),
                Some(session.id.clone()),
                None,
                &context.volumes,
            )
            .unwrap();
        let guest = Runtime::new()
            .unwrap()
            .create_guest(Guest::SharedNote.wasm(), context, Box::new(host))
            .unwrap();
        let mut owner = Self { guest, inbox, wake, view: vec![] };
        owner.settle();
        owner
    }
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            for (generation, token, creation) in self.inbox.ready() {
                assert!(!creation);
                self.guest.notify_data(generation, token).unwrap();
            }
            if self.guest.has_ready_work(Instant::now()) {
                let frame = self.guest.pump().unwrap();
                if let Some(view) = frame.view_bytes() {
                    self.view = view.to_vec();
                }
            }
            if self.inbox.idle() && !self.guest.has_ready_work(Instant::now()) {
                return;
            }
            assert!(Instant::now() < deadline, "accepted file work did not settle");
            if !self.inbox.has_ready() && !self.guest.has_ready_work(Instant::now()) {
                self.wake.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
        }
    }
    fn input(&mut self, kind: EventKind, data: &[u8]) {
        let encoded;
        let data = if kind == EventKind::TextChanged {
            encoded = playbit_api::abi::text::TextInput::encode(
                core::str::from_utf8(data).expect("fixture text"),
                &[],
            )
            .expect("fixture edit");
            encoded.as_slice()
        } else {
            data
        };
        self.guest
            .queue_event(
                self.guest.generation(),
                Event { object_id: 0, kind, target: 1, data: data.to_vec() },
            )
            .unwrap();
    }
}
#[test]
fn compiled_shared_note_reads_writes_drains_and_reopens_local_persistent_files() {
    for seeded in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut engine = Engine::open(directory.path()).unwrap();
        let access = Access::owner(engine.services.store.account().id);
        let mut declaration = playbit::package::read_volume_initializers(source)
            .unwrap()
            .for_role(playbit::package::AutomaticVolumeRole::Session);
        declaration.blobs[0].bytes = b"Native greeting".to_vec();
        if !seeded {
            declaration.blobs.clear();
        }
        let root = engine
            .services
            .store
            .initialize_root(StoreId::generate().unwrap(), "Shared Note test", false, &declaration)
            .unwrap();

        let package = Package::new(
            "shared-note".into(),
            "Shared Note".into(),
            Guest::SharedNote.wasm().to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        let package =
            playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
        let session = engine.create_session(package, root.id).unwrap();
        let mut owner = Owner::open(&engine, &session.id);
        if seeded {
            assert!(owner.view.windows(15).any(|bytes| bytes == b"Native greeting"));
        } else {
            assert!(owner.view.windows(7).any(|bytes| bytes == b"0 bytes"));
            assert!(!owner.view.windows(8).any(|bytes| bytes == b"NotFound"));
        }
        owner.input(EventKind::Submit, b"Saved native note");
        owner.guest.pump().unwrap();
        owner.guest.deactivate();
        owner.settle();
        assert!(!owner.guest.drain_pending());
        assert!(!owner.guest.drain_failed());
        let entry =
            engine.services.store.file_entry(&access, root.id, None, "note.txt").unwrap().id;
        assert_eq!(
            engine.services.store.read_blob_entry(&access, root.id, entry).unwrap(),
            b"Saved native note"
        );
        assert!(owner.view.windows(8).any(|bytes| bytes == b"17 bytes"));
        assert!(
            engine
                .services
                .store
                .pending_entry_blobs(&access, root.id, None)
                .unwrap()
                .versions
                .iter()
                .any(|reference| reference.len_bytes == 17)
        );
        drop(owner);
        drop(engine);
        let reopened = Engine::open(directory.path()).unwrap();
        let restored = Owner::open(&reopened, &session.id);
        assert!(restored.view.windows(17).any(|bytes| bytes == b"Saved native note"));
    }
}

use playbit::runtime::host::files::{
    Action, Call, Output, Root,
    capacity::{Capacity, Kind, Retention},
};
use playbit_api::abi::files::*;
use std::sync::Mutex;
#[derive(Default)]
struct Records {
    capacity: Arc<Capacity>,
    root: Option<Root>,
    calls: Vec<Arc<Call>>,
    results: BTreeMap<u64, HostResult<Arc<Output>>>,
    retained: BTreeMap<u64, Retention>,
    released: Vec<(u64, bool)>,
    discarded: Vec<u64>,
}
struct Controlled(Arc<Mutex<Records>>);
impl Host for Controlled {
    fn file_capacity(&self) -> Arc<Capacity> {
        self.0.lock().unwrap().capacity.clone()
    }
    fn file_roots(&mut self, volumes: &[AvailableVolume]) -> HostResult<Vec<Root>> {
        assert_eq!(volumes.len(), 2);
        let mut records = self.0.lock().unwrap();
        if records.root.is_none() {
            records.root = Some(Root {
                replaces: None,
                backing: volumes[0].backing,
                volume: volumes[0].id,
                grant: volumes[0].id,
                engine: 10,
                writable: true,
                credit: records.capacity.one(Kind::File)?,
            });
        }
        Ok(vec![
            records.root.as_ref().unwrap().clone(),
            Root {
                replaces: None,
                backing: volumes[1].backing,
                volume: volumes[1].id,
                grant: volumes[1].id,
                engine: 11,
                writable: false,
                credit: records.capacity.one(Kind::File)?,
            },
        ])
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::FileOpenBegin,
            Opcode::FileOpenTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
            Opcode::FileStatusBegin,
            Opcode::FileStatusTake,
            Opcode::FileCloseBegin,
            Opcode::FileCloseTake,
            Opcode::OperationDispose,
        ]
    }
    fn begin_file(&mut self, _: u64, call: Arc<Call>, retention: Retention) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.calls.push(call);
        let token = (1 << 63) | records.calls.len() as u64;
        records.retained.insert(token, retention);
        Ok(token)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<Output>> {
        self.0.lock().unwrap().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_file(&mut self, token: u64, claimed: bool) {
        let mut records = self.0.lock().unwrap();
        records.results.remove(&token);
        records.retained.remove(&token);
        records.released.push((token, claimed));
    }
    fn discard_file(&mut self, handle: u64, list: bool, _: Retention) -> HostResult<()> {
        assert!(!list);
        self.0.lock().unwrap().discarded.push(handle);
        Ok(())
    }
}
struct Fixture {
    guest: GuestInstance,
    records: Arc<Mutex<Records>>,
    view: playbit::runtime::ViewNode,
    rendered: bool,
}
impl Fixture {
    fn new(backend: ExecutionBackend) -> Self {
        let records = Arc::new(Mutex::new(Records::default()));
        let mut guest = Runtime::with_backend(backend)
            .unwrap()
            .create_guest(
                Guest::SharedNote.wasm(),
                context([1; 16], [2; 16]),
                Box::new(Controlled(records.clone())),
            )
            .unwrap();
        let frame = guest.pump().unwrap();
        let view = playbit::runtime::decode_view(frame.view_bytes().unwrap()).unwrap();
        Self { guest, records, view, rendered: true }
    }
    fn pump(&mut self) {
        let frame = self.guest.pump().unwrap();
        self.rendered = frame.view_bytes().is_some();
        if let Some(view) = frame.view_bytes() {
            self.view = playbit::runtime::decode_view(view).unwrap();
        }
    }
    fn input(&mut self, kind: EventKind, target: u32, data: &[u8]) {
        let encoded;
        let data = if kind == EventKind::TextChanged {
            encoded = playbit_api::abi::text::TextInput::encode(
                core::str::from_utf8(data).expect("fixture text"),
                &[],
            )
            .expect("fixture edit");
            encoded.as_slice()
        } else {
            data
        };
        self.guest
            .queue_event(
                self.guest.generation(),
                Event { object_id: 0, kind, target, data: data.to_vec() },
            )
            .unwrap();
        self.pump();
    }
    fn ready(&mut self, index: u64, result: HostResult<Output>) {
        let token = (1 << 63) | index;
        self.records.lock().unwrap().results.insert(token, result.map(Arc::new));
        self.guest.notify_data(self.guest.generation(), token).unwrap();
        self.pump();
    }
    fn open(&mut self, index: u64, mode: u32) {
        assert_eq!(
            *self.records.lock().unwrap().calls[index as usize - 1],
            Call { file: 10, action: Action::Open { path: b"note.txt".to_vec(), mode } },
        );
        self.ready(index, Ok(Output::File { handle: 20 + index }));
    }
    fn load(&mut self, first: u64, text: &str) {
        self.open(first, FILE_READ);
        self.ready(first + 1, Ok(Output::Read { bytes: text.as_bytes().to_vec() }));
        self.ready(first + 2, Ok(Output::Read { bytes: vec![] }));
    }
    fn draft(&self) -> &str {
        fn find(view: &playbit::runtime::ViewNode) -> Option<&str> {
            match view {
                playbit::runtime::ViewNode::TextField { id: 1, value, .. } => Some(value),
                playbit::runtime::ViewNode::Theme { child, .. } => find(child),
                playbit::runtime::ViewNode::Column(children) => children.iter().find_map(find),
                _ => None,
            }
        }
        find(&self.view).unwrap()
    }
    fn text(&self, text: &str) -> bool {
        fn find(view: &playbit::runtime::ViewNode, text: &str) -> bool {
            match view {
                playbit::runtime::ViewNode::Text { text: value, .. } => value == text,
                playbit::runtime::ViewNode::Theme { child, .. } => find(child, text),
                playbit::runtime::ViewNode::Column(children) => {
                    children.iter().any(|child| find(child, text))
                }
                _ => false,
            }
        }
        find(&self.view, text)
    }
}
#[test]
fn missing_note_is_empty_and_preserves_typing_during_initial_load() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut fixture = Fixture::new(backend);
        fixture.input(EventKind::TextChanged, 1, b"First draft");
        fixture.ready(1, Err(HostError::NotFound));
        assert_eq!(fixture.draft(), "First draft");
        assert!(fixture.text("0 bytes · Shared file"));
        assert!(!fixture.text("Could not open or save the note: NotFound"));
        fixture.input(EventKind::Submit, 1, b"First draft");
        fixture.open(2, FILE_WRITE);
        fixture
            .ready(4, Ok(Output::Close { error: 0, publication: FILE_PUBLISHED, recovery: None }));
        // Absence after a successful publication is a readback failure, not an empty note.
        fixture.ready(5, Err(HostError::NotFound));
        assert_eq!(fixture.draft(), "First draft");
        assert!(fixture.text("Could not open or save the note: NotFound"));
    }
}
#[test]
fn delayed_read_and_save_keep_subsequent_typing_and_never_replay_write() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut fixture = Fixture::new(backend);
        assert!(fixture.text("Loading note…"));
        fixture.input(EventKind::TextChanged, 1, b"Private while loading");
        fixture.load(1, "Original");
        assert_eq!(fixture.draft(), "Private while loading");
        assert!(fixture.text("Original"));
        fixture.input(EventKind::Submit, 1, b"Accepted save");
        assert!(fixture.text("Saving note…"));
        fixture.open(4, FILE_WRITE);
        assert!(!fixture.rendered, "opening the file does not change Saving");
        assert_eq!(
            *fixture.records.lock().unwrap().calls[4],
            Call {
                file: 24,
                action: Action::Write { offset: 0, bytes: b"Accepted save".to_vec() }
            },
        );
        assert_eq!(fixture.records.lock().unwrap().calls[5].action, Action::Close { flags: 0 });
        assert!(fixture.records.lock().unwrap().released.contains(&((1 << 63) | 5, false)));
        // Deliver only Close: its ordered receipt also covers the uncollected write.
        fixture.input(EventKind::TextChanged, 1, b"Later private draft");
        assert!(fixture.text("Saving note…"));
        fixture
            .ready(6, Ok(Output::Close { error: 0, publication: FILE_PUBLISHED, recovery: None }));
        assert!(!fixture.rendered, "publication keeps Saving visible until readback completes");
        assert!(fixture.text("Saving note…"));
        fixture.load(7, "Actual stored text");
        assert_eq!(fixture.draft(), "Later private draft");
        assert!(fixture.text("Actual stored text"));
        assert_eq!(fixture.records.lock().unwrap().calls.len(), 9);
        fixture.input(EventKind::DataChanged, 0, &[]);
        fixture.load(10, "Collaborator edit");
        assert_eq!(fixture.draft(), "Later private draft");
        assert!(fixture.text("Collaborator edit"));
        assert_eq!(
            fixture
                .records
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|call| matches!(call.action, Action::Write { .. }))
                .count(),
            1
        );
    }
}
#[test]
fn save_readback_covers_prior_changes_but_retains_changes_during_the_read() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut fixture = Fixture::new(backend);
        fixture.load(1, "Original");
        fixture.input(EventKind::Submit, 1, b"Saved");
        fixture.open(4, FILE_WRITE);
        fixture.input(EventKind::DataChanged, 0, &[]);
        assert!(!fixture.rendered, "a deferred refresh leaves Saving unchanged");
        fixture
            .ready(6, Ok(Output::Close { error: 0, publication: FILE_PUBLISHED, recovery: None }));
        fixture.load(7, "Saved");
        assert!(fixture.text("Saved"));
        assert_eq!(fixture.records.lock().unwrap().calls.len(), 9);
        fixture.input(EventKind::DataChanged, 0, &[]);
        assert!(fixture.rendered, "starting a refresh must show Loading");
        fixture.input(EventKind::DataChanged, 0, &[]);
        assert!(!fixture.rendered, "coalescing a refresh leaves Loading unchanged");
        fixture.load(10, "First change");
        assert_eq!(fixture.records.lock().unwrap().calls.len(), 13);
        assert!(fixture.rendered, "readback must render even when another read starts");
        assert!(fixture.text("First change"));
        assert!(fixture.text("Loading note…"));
        fixture.load(13, "Second change");
        assert!(fixture.text("Second change"));
        assert_eq!(fixture.records.lock().unwrap().calls.len(), 15);
    }
}
#[test]
fn failed_file_keeps_loaded_note_and_draft_until_explicit_reload() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut fixture = Fixture::new(backend);
        fixture.load(1, "Original");
        fixture.input(EventKind::Submit, 1, b"Retained draft");
        fixture.open(4, FILE_WRITE);
        fixture.ready(
            6,
            Ok(Output::Close {
                error: HostError::Unavailable as i32,
                publication: FILE_NOT_PUBLISHED,
                recovery: None,
            }),
        );
        assert!(
            fixture.text("File unavailable. Reconnect and choose Reload to read current data.")
        );
        assert_eq!(fixture.draft(), "Retained draft");
        assert!(fixture.text("Original"));
        let count = fixture.records.lock().unwrap().calls.len();
        fixture.pump();
        assert_eq!(fixture.records.lock().unwrap().calls.len(), count);
        fixture.input(EventKind::Click, 3, &[]);
        fixture.load(7, "Committed externally");
        assert_eq!(fixture.draft(), "Retained draft");
        assert!(fixture.text("Committed externally"));
    }
}

#[test]
fn note_reads_short_chunks_through_eof_and_rejects_oversized_content() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut fixture = Fixture::new(backend);
        fixture.open(1, FILE_READ);
        fixture.ready(2, Ok(Output::Read { bytes: vec![0xe2] }));
        assert_eq!(
            *fixture.records.lock().unwrap().calls[2],
            Call { file: 21, action: Action::Read { offset: 1, length: 8192 } },
        );
        fixture.ready(3, Ok(Output::Read { bytes: vec![0x82, 0xac] }));
        assert!(fixture.text("Loading note…"));
        fixture.ready(4, Ok(Output::Read { bytes: vec![] }));
        assert_eq!(fixture.draft(), "€");
        assert!(fixture.records.lock().unwrap().discarded.contains(&21));
        fixture.input(EventKind::DataChanged, 0, &[]);
        fixture.open(5, FILE_READ);
        fixture.ready(6, Ok(Output::Read { bytes: vec![b'x'; 8192] }));
        assert_eq!(
            *fixture.records.lock().unwrap().calls[6],
            Call { file: 25, action: Action::Read { offset: 8192, length: 1 } },
        );
        fixture.ready(7, Ok(Output::Read { bytes: vec![b'x'] }));
        assert_eq!(fixture.draft(), "€");
        assert!(fixture.text("€"));
        assert!(fixture.text("This note can contain up to 8,192 UTF-8 bytes."));
        assert!(fixture.records.lock().unwrap().discarded.contains(&25));
        fixture.input(EventKind::Click, 3, &[]);
        fixture.load(8, &"y".repeat(8192));
        assert_eq!(fixture.draft().len(), 8192);
        assert!(fixture.text("8192 bytes · Shared file"));

        let oversized = "🦀".repeat(2049);
        let calls = fixture.records.lock().unwrap().calls.len();
        fixture.input(EventKind::TextChanged, 1, oversized.as_bytes());
        assert_eq!(fixture.draft(), oversized);
        fixture.input(EventKind::Submit, 1, oversized.as_bytes());
        fixture.input(EventKind::Click, 2, &[]);
        assert_eq!(fixture.records.lock().unwrap().calls.len(), calls);
        assert!(fixture.text("This note can contain up to 8,192 UTF-8 bytes."));
        fixture.input(EventKind::Submit, 1, b"Recovered");
        assert_eq!(fixture.draft(), "Recovered");
        assert!(fixture.text("Saving note…"));
        fixture.open(11, FILE_WRITE);
    }
}
