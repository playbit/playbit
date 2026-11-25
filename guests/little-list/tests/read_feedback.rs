//! Exercise the app's actual callbacks with controlled docdb host completions.
#![allow(unsafe_code)]
#[path = "../src/lib.rs"]
mod native;
#[allow(
    clippy::duplicate_mod,
    reason = "The fixture builds independent input with the guest schema."
)]
#[path = "../src/schema.rs"]
mod schema;

use playbit::{
    runtime::{
        host::{DataCall, DataKind, DataOperation, DataResult},
        native::NativeInstance,
        syscall::{
            AvailableVolume, Event, EventKind, GuestContext, Host, HostError, HostResult, Identity,
            Opcode, VolumeRights,
        },
    },
    store::{EntryId, Id, bloblog},
};
use playbit_api::docdb::{Doc, RNG, SyncHelper, UUID};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Records {
    calls: Vec<DataKind>,
    cursors: Vec<Option<bloblog::Cursor>>,
    results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
    deny_begin: Option<DataKind>,
}
struct Controlled(Arc<Mutex<Records>>);
impl Host for Controlled {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::EntryResolveBegin,
            Opcode::EntryResolveTake,
            Opcode::BloblogReadBegin,
            Opcode::BloblogReadTake,
            Opcode::BloblogAppendBegin,
            Opcode::BloblogAppendTake,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        let kind = call.operation.kind();
        records.calls.push(kind);
        if let DataOperation::BloblogRead { cursor, .. } = &call.operation {
            records.cursors.push(cursor.clone());
        }
        if records.deny_begin == Some(kind) {
            return Err(HostError::AccessDenied);
        }
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.0.lock().unwrap().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().results.remove(&token);
    }
}
struct Fixture {
    guest: NativeInstance,
    records: Arc<Mutex<Records>>,
    view: Vec<u8>,
}
impl Fixture {
    fn new(deny_begin: Option<DataKind>) -> Self {
        let records = Arc::new(Mutex::new(Records { deny_begin, ..Default::default() }));
        let context = GuestContext {
            app_name: "Little List".into(),
            app_build: None,
            identity: Identity {
                display_name: String::new(),
                account_id: [5; 16],
                device_id: [6; 16],
                instance_id: [7; 16],
                is_service: false,
                session_id: None,
            },
            volumes: vec![AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [4; 16],
                name: "Root".into(),
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: true,
                role: 0,
            }],
            startup_arguments: BTreeMap::new(),
            trace_catalog: None,
            required_capabilities: Default::default(),
            participants: vec![],
        };
        // The included guest owns these callbacks for the fixture's lifetime.
        let guest = unsafe {
            NativeInstance::new(
                context,
                Box::new(Controlled(records.clone())),
                playbit_api::native_app(),
            )
            .unwrap()
        };
        let mut fixture = Self { guest, records, view: vec![] };
        fixture.pump();
        fixture
    }
    fn resolved() -> Self {
        let mut fixture = Self::new(None);
        fixture.ready(1, Ok(DataResult::EntryResolved([1; 15])));
        assert_eq!(fixture.calls(), vec![DataKind::ResolveEntry, DataKind::BloblogRead]);
        fixture
    }
    fn loaded() -> Self {
        let mut fixture = Self::resolved();
        fixture.ready(2, Ok(page(None, false, 0)));
        fixture
    }
    fn pump(&mut self) {
        if let Some(view) = self.guest.pump().unwrap().view_bytes() {
            self.view = view.to_vec();
        }
    }
    fn input(&mut self, kind: EventKind, target: u32, text: &str) {
        let data = if kind == EventKind::TextChanged {
            playbit_api::abi::text::TextInput::encode(text, &[]).unwrap()
        } else {
            text.as_bytes().to_vec()
        };
        self.guest
            .queue_event(self.guest.generation(), Event { object_id: 0, kind, target, data })
            .unwrap();
        self.pump();
    }
    fn changed(&mut self) {
        self.input(EventKind::DataChanged, 0, "");
    }
    fn ready(&mut self, token: u64, result: HostResult<DataResult>) {
        self.records.lock().unwrap().results.insert(token, result.map(Arc::new));
        self.guest.notify_data(self.guest.generation(), token).unwrap();
        self.pump();
    }
    fn calls(&self) -> Vec<DataKind> {
        self.records.lock().unwrap().calls.clone()
    }
    fn cursors(&self) -> Vec<Option<bloblog::Cursor>> {
        self.records.lock().unwrap().cursors.clone()
    }
    fn text(&self, text: &str) -> bool {
        self.view.windows(text.len()).any(|bytes| bytes == text.as_bytes())
    }
}
struct Random(u8);
impl RNG for Random {
    fn secure_random_bytes(&mut self, bytes: *mut u8, length: usize) {
        unsafe { std::slice::from_raw_parts_mut(bytes, length) }.fill(self.0);
        self.0 += 1;
    }
}
fn page(title: Option<&str>, more: bool, seed: u8) -> DataResult {
    let log = EntryId::from_bytes([1; 15]).unwrap();
    let mut entries = vec![];
    if let Some(title) = title {
        let mut doc = Doc::default();
        let mut random = Random(seed);
        let item = UUID::generate(&mut random);
        schema::StateRoot::root().items().append(&mut doc, item);
        schema::Item::from_link(item).text().set(&mut doc, title);
        let data = SyncHelper::default().query(&mut doc, &mut random).unwrap();
        entries = bloblog::append_entries(
            Id::from_bytes([5; 16]),
            Id::from_bytes([4; 16]),
            log,
            Id::from_bytes([seed; 16]),
            &[data],
        )
        .unwrap();
    }
    let cursor = bloblog::Cursor::parse(&format!(
        "1/{}/{}/{}/{}/{}",
        Id::from_bytes([4; 16]),
        log,
        playbit::identifier::encode(&[0; 8]),
        u64::from(seed) + 1,
        Id::from_bytes([0; 16])
    ))
    .unwrap();
    DataResult::BloblogPage(bloblog::Page { entries, cursor, more })
}

#[test]
fn resolve_failures_use_read_feedback_and_resolution_starts_reading() {
    let denied = Fixture::new(Some(DataKind::ResolveEntry));
    assert!(denied.text("Couldn't read your list: This list can't be read right now."));
    assert!(!denied.text("Couldn't update the app:"));
    let mut app = Fixture::new(None);
    app.ready(1, Err(HostError::WouldBlock));
    assert_eq!(app.calls(), vec![DataKind::ResolveEntry]);
    app.ready(1, Err(HostError::AccessDenied));
    assert!(app.text("Couldn't read your list: This list can't be read right now."));
    assert!(!app.text("Couldn't update the app:"));
    let mut app = Fixture::resolved();
    assert_eq!(app.cursors(), vec![None]);
    app.ready(2, Ok(page(Some("Initial task"), false, 1)));
    assert!(app.text("Initial task"));
    assert_eq!(app.calls().len(), 2);
}

#[test]
fn read_feedback_survives_pending_partial_and_failed_pages_until_final_success() {
    let mut app = Fixture::loaded();
    app.records.lock().unwrap().deny_begin = Some(DataKind::BloblogRead);
    app.changed();
    assert!(app.text("This list can't be read right now."));
    app.records.lock().unwrap().deny_begin = None;
    app.changed();
    app.ready(4, Err(HostError::WouldBlock));
    assert!(app.text("This list can't be read right now."));
    app.input(EventKind::DataChanged, 0, "");
    app.changed();
    assert_eq!(app.calls().len(), 4);
    let partial = page(Some("Partial marker"), true, 10);
    let DataResult::BloblogPage(ref partial_page) = partial else { panic!("page") };
    let cursor = partial_page.cursor.clone();
    app.ready(4, Ok(partial));
    assert!(app.text("Partial marker"));
    assert!(app.text("This list can't be read right now."));
    assert_eq!(app.cursors().last(), Some(&Some(cursor.clone())));
    app.ready(5, Err(HostError::NotFound));
    assert!(app.text("Couldn't read your list: NotFound"));
    assert_eq!(app.calls().len(), 5, "failure consumes the pending refresh");
    app.changed();
    assert_eq!(app.cursors().last(), Some(&Some(cursor)));
    app.ready(6, Ok(page(None, false, 20)));
    assert!(app.text("Partial marker"));
    assert!(!app.text("Couldn't read your list:"));
    assert_eq!(
        app.calls(),
        vec![
            DataKind::ResolveEntry,
            DataKind::BloblogRead,
            DataKind::BloblogRead,
            DataKind::BloblogRead,
            DataKind::BloblogRead,
            DataKind::BloblogRead,
        ]
    );
}

#[test]
fn completed_read_denial_recovers_without_clearing_append_error_or_draft() {
    let mut app = Fixture::loaded();
    app.input(EventKind::TextChanged, 1, "Unsaved task");
    app.input(EventKind::Submit, 1, "Unsaved task");
    app.ready(3, Err(HostError::AccessDenied));
    assert!(app.text("Couldn't update the app: AccessDenied"));
    assert!(app.text("Unsaved task"));
    app.changed();
    app.ready(4, Err(HostError::AccessDenied));
    assert!(app.text("This list can't be read right now."));
    assert!(app.text("Couldn't update the app: AccessDenied"));
    app.input(EventKind::DataChanged, 0, "");
    app.ready(5, Ok(page(Some("Recovered marker"), false, 40)));
    assert!(app.text("Recovered marker"));
    assert!(!app.text("Couldn't read your list:"));
    assert!(app.text("Couldn't update the app: AccessDenied"));
    assert!(app.text("Unsaved task"));
    assert_eq!(
        app.calls(),
        vec![
            DataKind::ResolveEntry,
            DataKind::BloblogRead,
            DataKind::BloblogAppend,
            DataKind::BloblogRead,
            DataKind::BloblogRead,
        ]
    );
}

#[test]
fn continuation_begin_denial_keeps_feedback_and_recovers_on_data_changed() {
    let mut app = Fixture::resolved();
    app.ready(2, Err(HostError::NotFound));
    app.changed();
    app.records.lock().unwrap().deny_begin = Some(DataKind::BloblogRead);
    app.ready(3, Ok(page(Some("Partial marker"), true, 60)));
    assert!(app.text("This list can't be read right now."));
    app.records.lock().unwrap().deny_begin = None;
    app.changed();
    app.ready(5, Ok(page(None, false, 70)));
    assert!(!app.text("Couldn't read your list:"));
    assert!(app.text("Partial marker"));
    assert_eq!(app.calls().len(), 5);
}

#[test]
fn changes_before_resolution_and_during_pages_coalesce_without_losing_refresh() {
    let mut app = Fixture::new(None);
    app.input(EventKind::DataChanged, 0, "");
    app.input(EventKind::DataChanged, 0, "");
    assert_eq!(app.calls(), vec![DataKind::ResolveEntry]);
    app.ready(1, Ok(DataResult::EntryResolved([1; 15])));
    assert_eq!(app.calls().len(), 2);
    for _ in 0..3 {
        app.input(EventKind::DataChanged, 0, "");
    }
    assert_eq!(app.calls().len(), 2);
    app.ready(2, Ok(page(Some("First page"), true, 10)));
    assert!(app.text("First page"));
    assert_eq!(app.calls().len(), 3);
    let last = page(Some("Second page"), false, 20);
    let DataResult::BloblogPage(ref last_page) = last else { panic!("page") };
    let cursor = last_page.cursor.clone();
    app.ready(3, Ok(last));
    assert!(app.text("Second page"));
    assert_eq!(app.calls().len(), 4, "one follow-up after all pages");
    assert_eq!(app.cursors().last(), Some(&Some(cursor)));
    let empty = page(None, false, 30);
    let DataResult::BloblogPage(ref empty_page) = empty else { panic!("page") };
    let cursor = empty_page.cursor.clone();
    app.ready(4, Ok(empty));
    assert_eq!(app.calls().len(), 4, "caught-up reads stay idle");
    app.input(EventKind::DataChanged, 0, "");
    assert_eq!(app.cursors().last(), Some(&Some(cursor)));
    app.ready(5, Ok(page(Some("New task"), false, 40)));
    assert!(app.text("New task"));
    assert_eq!(app.calls().len(), 5);
}

#[test]
fn replayed_docdb_entries_do_not_duplicate_tasks() {
    let mut app = Fixture::resolved();
    app.ready(2, Ok(page(Some("Replay marker"), false, 50)));
    app.input(EventKind::DataChanged, 0, "");
    app.ready(3, Ok(page(Some("Replay marker"), false, 50)));
    let count = app.view.windows(b"Replay marker".len()).filter(|s| *s == b"Replay marker").count();
    assert_eq!(count, 1);
}
