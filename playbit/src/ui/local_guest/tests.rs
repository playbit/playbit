//! Actual TODO Wasm with UI-owned execution and shared authoritative storage.
#![allow(
    clippy::unwrap_used,
    reason = "Tests own isolated stores and finite observation deadlines."
)]
use super::*;
use crate::engine::Engine;
use crate::store::{Access, PersonalVolumeGrant, object_contract::*};
use playbit_api::ui::CheckState;
use std::{collections::BTreeMap, sync::mpsc};

struct Fixture {
    apps: Apps,
    engine: Engine,
    wake: Arc<dyn Fn() + Send + Sync>,
    receive: mpsc::Receiver<()>,
    session: String,
    object: Id,
    database: crate::store::volume_index::EntryAddress,
    ids: [Id; 3],
    _directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = send.try_send(());
        });
        let engine_wake = wake.clone();
        let engine = Engine::open_with_wake(directory.path(), move || engine_wake()).unwrap();
        let store = &engine.services.store;
        let initializers = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap();
        let declaration = initializers.for_role(crate::package::AutomaticVolumeRole::Session);
        let crate::store::InitialDatabaseKind::ObjectDb { objects, .. } =
            &declaration.databases[0].kind
        else {
            panic!("TODO database")
        };
        let object = objects[0].id;
        let package = crate::package::signature::testing::signed(
            store,
            Package::create(
                "test".into(),
                "TODO".into(),
                playbit_test_fixtures::Guest::Todo.wasm().to_vec(),
                BTreeMap::new(),
                &initializers,
            )
            .unwrap(),
        )
        .unwrap();
        let app = package.identity().unwrap().clone();
        let build = package.id().unwrap();
        let builds = engine.services.profile_path().join(crate::paths::BUILDS);
        std::fs::create_dir_all(&builds).unwrap();
        std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
        store.remember_imported_app(&package).unwrap();
        let root =
            app.volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None).unwrap();
        let create = || {
            store
                .create_session_binding(crate::store::SessionCreation {
                    app: app.clone(),
                    selector: crate::store::BuildSelector::Exact { build_id: build.clone() },
                    resolved_build_id: build.clone(),
                    root_volume: root,
                    user_volume: None,
                    sync_shared: true,
                })
                .unwrap()
                .id
        };
        let session = create();
        let independent = create();
        let root = store
            .open_automatic_volume(&session, crate::package::AutomaticVolumeRole::Session)
            .unwrap()
            .id;
        let unrelated = store
            .open_automatic_volume(&independent, crate::package::AutomaticVolumeRole::Session)
            .unwrap()
            .id;
        assert_ne!(root, unrelated, "separate sessions have independent Shared data");
        let database = crate::store::volume_index::EntryAddress {
            volume: root,
            entry: declaration.databases[0].id,
        };
        let mut apps = Apps::default();
        let ids = [
            apps.open(
                &engine.services,
                &session,
                engine.remote.as_ref().unwrap().blob_transport(),
                wake.clone(),
            )
            .unwrap(),
            apps.open(
                &engine.services,
                &session,
                engine.remote.as_ref().unwrap().blob_transport(),
                wake.clone(),
            )
            .unwrap(),
            apps.open(
                &engine.services,
                &independent,
                engine.remote.as_ref().unwrap().blob_transport(),
                wake.clone(),
            )
            .unwrap(),
        ];
        assert_ne!(ids[0], ids[1], "one session may have independent instances");
        let mut fixture = Self {
            apps,
            engine,
            wake,
            receive,
            session,
            object,
            database,
            ids,
            _directory: directory,
        };
        fixture.settle();
        fixture
    }
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.engine.complete_background().unwrap();
            self.apps.advance(true);
            if self.apps.settled() {
                return;
            }
            assert!(Instant::now() < deadline, "local owners failed to quiesce");
            if self.apps.wake_after(Instant::now()) != Some(Duration::ZERO) {
                self.receive
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
            }
        }
    }
    fn input(&mut self, index: usize, kind: EventKind, target: u32, data: &[u8]) {
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
        self.apps
            .input(self.ids[index], Event { object_id: 0, kind, target, data: data.to_vec() })
            .unwrap();
        self.apps.advance(true);
    }
    fn rows(&self, index: usize) -> Vec<(u32, String, CheckState)> {
        fn visit(view: &ViewNode, rows: &mut Vec<(u32, String, CheckState)>) {
            match view {
                ViewNode::Checkbox { id, label, state, .. } => {
                    rows.push((*id, label.clone(), *state))
                }
                ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => {
                    visit(child, rows)
                }
                ViewNode::Row(children) | ViewNode::Column(children) | ViewNode::List(children) => {
                    for child in children {
                        visit(child, rows);
                    }
                }
                _ => {}
            }
        }
        let mut rows = vec![];
        visit(self.apps.scene(self.ids[index]).unwrap(), &mut rows);
        rows
    }
    fn done(&self) -> bool {
        let store = &self.engine.services.store;
        let row = store
            .read_object(&Access::owner(store.account().id), self.database, self.object)
            .unwrap();
        row.fields[&Id::from_bytes([3; 16])].values.as_ref().unwrap() == &vec![Value::Boolean(true)]
    }
    fn draft(&self, index: usize) -> &str {
        fn find(view: &ViewNode) -> Option<&str> {
            match view {
                ViewNode::TextField { id: 1, value, .. } => Some(value),
                ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => find(child),
                ViewNode::Row(children) | ViewNode::Column(children) | ViewNode::List(children) => {
                    children.iter().find_map(find)
                }
                _ => None,
            }
        }
        find(self.apps.scene(self.ids[index]).unwrap()).unwrap()
    }
}
#[test]
fn completed_edit_refreshes_only_shared_readers_without_engine_execution() {
    let mut f = Fixture::new();
    let unrelated = f.apps.scene(f.ids[2]).unwrap().clone();
    let target = f.rows(0)[0].0;
    f.input(0, EventKind::Click, target, &[]);
    f.settle();
    assert!(f.done());
    assert_eq!(f.rows(1)[0].2, CheckState::On);
    assert_eq!(f.apps.scene(f.ids[2]).unwrap(), &unrelated);
}
#[test]
fn external_commit_refreshes_shared_readers_from_scoped_history_without_ap_notices() {
    let mut f = Fixture::new();
    f.input(1, EventKind::TextChanged, 1, b"Retained independent draft");
    f.settle();
    let generations = f.ids.map(|id| f.apps.generation(id).unwrap());
    let unrelated = f.apps.scene(f.ids[2]).unwrap().clone();
    let store = &f.engine.services.store;
    store
        .edit_fields(
            &Access::owner(store.account().id),
            f.database,
            f.object,
            &BTreeMap::from([(Id::from_bytes([3; 16]), Some(vec![Value::Boolean(true)]))]),
        )
        .unwrap();
    // No guest input, live AP notice or synthesized DataChanged event. An
    // explicit storage/management commit uses the same bounded delivery history
    // as an initialization/reconnect merge, on the next actual owner turn.
    f.settle();
    assert_eq!(f.rows(0)[0].2, CheckState::On);
    assert_eq!(f.rows(1)[0].2, CheckState::On);
    assert_eq!(f.draft(1), "Retained independent draft");
    assert_eq!(f.apps.scene(f.ids[2]).unwrap(), &unrelated);
    assert_eq!(f.ids.map(|id| f.apps.generation(id).unwrap()), generations);
    for _ in 0..8 {
        f.apps.advance(true);
    }
    assert!(f.done(), "observing the committed scene cannot replay an input");
}
#[test]
fn consent_changes_preserve_instance_draft_and_data() {
    let mut f = Fixture::new();
    let generation = f.apps.app(f.ids[0]).unwrap().guest.generation();
    f.input(0, EventKind::TextChanged, 1, b"Private unsubmitted draft");
    let volume = f.engine.services.store.create_volume("Notes", false).unwrap();
    f.engine
        .dispatch_reply(crate::engine::Request::SelectPersonalVolumes {
            session: f.session.clone(),
            grants: vec![PersonalVolumeGrant { volume: volume.id, write: false }],
        })
        .unwrap();
    f.settle();
    assert_eq!(f.apps.app(f.ids[0]).unwrap().guest.generation(), generation);
    assert!(!f.apps.app(f.ids[0]).unwrap().reads.contains(&volume.id));
    assert!(!f.apps.app(f.ids[0]).unwrap().writes.contains(&volume.id));
    assert_eq!(f.draft(0), "Private unsubmitted draft");
    let newly_admitted = f
        .apps
        .open(
            &f.engine.services,
            &f.session,
            f.engine.remote.as_ref().unwrap().blob_transport(),
            f.wake.clone(),
        )
        .unwrap();
    assert!(f.apps.app(newly_admitted).unwrap().reads.contains(&volume.id));
    assert!(!f.apps.app(newly_admitted).unwrap().writes.contains(&volume.id));
    f.input(0, EventKind::Click, 2, &[]);
    f.settle();
    assert!(f.rows(1).iter().any(|(_, title, _)| title == "Private unsubmitted draft"));
}
#[test]
fn notice_during_accepted_input_preserves_independent_instance_state() {
    let mut f = Fixture::new();
    f.apps
        .input(
            f.ids[1],
            Event {
                object_id: 0,
                kind: EventKind::TextChanged,
                target: 1,
                data: playbit_api::abi::text::TextInput::encode("Independent draft", &[])
                    .expect("fixture edit"),
            },
        )
        .unwrap();
    let target = f.rows(0)[0].0;
    f.input(0, EventKind::Click, target, &[]);
    f.settle();
    assert_eq!(f.draft(1), "Independent draft");
    assert_eq!(f.rows(1)[0].2, CheckState::On);
    assert_eq!(f.rows(2)[0].2, CheckState::Off);
}
#[test]
fn discarded_scene_delivery_does_not_replay_a_durable_edit() {
    let mut f = Fixture::new();
    let target = f.rows(0)[0].0;
    f.input(0, EventKind::Click, target, &[]);
    // No scene reply is observed while the accepted operation completes.
    f.settle();
    assert!(f.done());
    assert_eq!(f.rows(1)[0].2, CheckState::On);
    for _ in 0..8 {
        f.apps.advance(true);
    }
    assert!(f.done());
    assert_eq!(
        f.apps
            .logs(f.ids[0])
            .unwrap()
            .iter()
            .filter(|line| line.as_str() == "TODO changed")
            .count(),
        1
    );
}
#[test]
fn revoked_delivery_after_commit_keeps_independent_session_data_unchanged() {
    let mut f = Fixture::new();
    let independent = f.apps.scene(f.ids[2]).unwrap().clone();
    f.settle();
    let target = f.rows(0)[0].0;
    f.input(0, EventKind::Click, target, &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.apps.app(f.ids[0]).unwrap().inbox.waiting() {
        f.receive.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    }
    assert!(f.done(), "the mutation is durable before delivery is revoked");
    f.engine
        .services
        .store
        .record_session_denial(
            &f.engine.services.store.session(&f.session).unwrap(),
            "delivery revoked",
        )
        .unwrap();
    f.settle();
    assert!(f.apps.scene(f.ids[0]).is_err());
    assert!(f.apps.scene(f.ids[1]).is_err(), "both revoked session instances refuse delivery");
    assert_eq!(f.rows(2)[0].2, CheckState::Off);
    assert_eq!(f.apps.scene(f.ids[2]).unwrap(), &independent);
    assert!(f.done(), "late delivery denial cannot undo the accepted Shared write");
}
#[test]
fn instance_capacity_is_independent_of_shared_session_identity() {
    let mut f = Fixture::new();
    let extra = f
        .apps
        .open(
            &f.engine.services,
            &f.session,
            f.engine.remote.as_ref().unwrap().blob_transport(),
            f.wake.clone(),
        )
        .unwrap();
    assert_ne!(extra, f.ids[0]);
    assert!(
        f.apps
            .open(
                &f.engine.services,
                &f.session,
                f.engine.remote.as_ref().unwrap().blob_transport(),
                f.wake.clone()
            )
            .is_err()
    );
    f.settle();
    f.apps.close(extra).unwrap();
    assert!(
        f.apps
            .open(
                &f.engine.services,
                &f.session,
                f.engine.remote.as_ref().unwrap().blob_transport(),
                f.wake.clone()
            )
            .is_ok()
    );
    f.settle();
}
#[test]
fn deactivation_drains_an_accepted_save_and_its_readback() {
    let mut f = Fixture::new();
    f.input(0, EventKind::TextChanged, 1, b"Saved before close");
    f.apps
        .input(f.ids[0], Event { object_id: 0, kind: EventKind::Click, target: 2, data: vec![] })
        .unwrap();
    f.apps.deactivate(f.ids[0]).unwrap();
    assert!(
        f.apps
            .input(
                f.ids[0],
                Event {
                    object_id: 0,
                    kind: EventKind::TextChanged,
                    target: 1,
                    data: playbit_api::abi::text::TextInput::encode("Rejected", &[])
                        .expect("fixture edit")
                }
            )
            .is_err()
    );
    f.settle();
    assert!(!f.apps.drain_pending(f.ids[0]).unwrap());
    assert!(f.rows(1).iter().any(|(_, title, _)| title == "Saved before close"));
    f.apps.close(f.ids[0]).unwrap();
}
