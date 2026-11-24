//! Guest-package tests own execution explicitly beside durable engine services.
#![allow(clippy::unwrap_used, reason = "Tests own isolated stores and finite completion waits.")]
use playbit::{
    engine::{
        Engine,
        data_host::DataHost,
        in_process_host::{InProcessHost, Inbox},
    },
    package::Package,
    runtime::{Runtime, syscall::*},
    store::{Access, Id, Session},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

pub struct WasmOwner {
    pub session: Session,
    guest: GuestInstance,
    inbox: Inbox,
    wake: mpsc::Receiver<()>,
    view: Vec<u8>,
}
impl WasmOwner {
    /// Adapter fixture mapping over the already-owned ordinary volume; not automatic-scope admission.
    pub fn open_with_role(engine: &Engine, session: &str, role: u32) -> Self {
        let record = engine.services.store.session(session).unwrap();
        let package = Package::from_bytes(
            &std::fs::read(
                engine.services.profile_path().join("builds").join(&record.resolved_build_id),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(package.id().unwrap(), record.resolved_build_id);
        let account = *engine.services.store.account().id.as_bytes();
        let session_id = playbit::identifier::decode::<32>(&record.id).unwrap();
        let mut context = GuestContext {
            app_name: package.name().to_owned(),
            app_build: Some(playbit::identifier::decode::<32>(&record.resolved_build_id).unwrap()),
            identity: Identity {
                display_name: String::new(),
                account_id: account,
                device_id: account,
                is_service: false,
                instance_id: *Id::generate().unwrap().as_bytes(),
                session_id: Some(session_id),
            },
            volumes: vec![AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: *record.root_volume.as_bytes(),
                name: "Root".into(),
                tags: vec!["root".into()],
                rights: VolumeRights::ALL,
                is_root: role == 0 || role == 1,
                role,
            }],
            startup_arguments: BTreeMap::new(),
            trace_catalog: package.trace().cloned(),
            required_capabilities: BTreeSet::new(),
            participants: vec![],
        };
        if role == playbit_api::context::VolumeRole::Session as u32 {
            let root = engine.services.store.create_volume("Adapter fixture Root", true).unwrap();
            let mut grants =
                engine.services.store.personal_volume_grants(session).unwrap().unwrap_or_default();
            grants.push(playbit::store::PersonalVolumeGrant { volume: root.id, write: false });
            engine.services.store.select_personal_volumes(session, grants).unwrap();
            context.volumes.push(AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: *root.id.as_bytes(),
                name: root.name,
                tags: vec!["root".into()],
                rights: VolumeRights::READ,
                is_root: true,
                role: 0,
            });
        }
        let (send, wake) = mpsc::sync_channel(1);
        let (host, inbox) = InProcessHost::local(
            DataHost {
                store: engine.services.store.clone(),
                access: Access::session_version(
                    engine.services.store.account().id,
                    &record.id,
                    &record.resolved_build_id,
                ),
                changed: Arc::new(AtomicBool::new(false)),
            },
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        let guest = Runtime::new()
            .unwrap()
            .create_guest(&package.wasm().unwrap(), context, Box::new(host))
            .unwrap();
        let mut owner = Self { session: record, guest, inbox, wake, view: vec![] };
        owner.settle();
        owner
    }
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            for (generation, token, creation) in self.inbox.ready() {
                if creation {
                    self.guest.notify_creation(generation, token)
                } else {
                    self.guest.notify_data(generation, token)
                }
                .unwrap();
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
            assert!(Instant::now() < deadline, "test owner did not complete accepted work");
            if !self.inbox.has_ready() && !self.guest.has_ready_work(Instant::now()) {
                self.wake.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
        }
    }
    pub fn input(&mut self, event: Event) -> HostResult<()> {
        let view =
            playbit::runtime::decode_view(&self.view).map_err(|()| HostError::InvalidArgument)?;
        validate_input(&view, &event)?;
        self.guest.queue_event(self.guest.generation(), event)?;
        self.settle();
        Ok(())
    }
    #[allow(dead_code, reason = "Only TODO fixtures signal their explicit external commits.")]
    pub fn data_changed(&mut self) {
        self.guest
            .queue_event(
                self.guest.generation(),
                Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
            )
            .unwrap();
        self.settle();
    }
    pub fn scene(&mut self) -> &[u8] {
        self.settle();
        &self.view
    }
    pub fn quit(&mut self) {
        self.guest.deactivate();
        self.settle();
        assert!(!self.guest.drain_pending());
    }
}
