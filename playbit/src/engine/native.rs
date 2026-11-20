//! Local native apps bound to one engine's owned volumes and host event loop.
//! Callbacks have one serialized owner; data I/O runs on bounded storage lanes.
//! Completion readiness wakes the caller without a polling timer or VM.
use super::{Services, data_host::DataHost};
use crate::{
    runtime::{
        Frame, RuntimeError, ViewNode,
        host::*,
        native::NativeInstance,
        syscall::{Event, EventKind, HostError, HostResult, validate_input},
    },
    store::{
        Access, Id as StoreId,
        change_feed::{ChangeBatch, FeedStatus},
    },
};
use playbit_api::abi::operations::App;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

mod recovery;
use super::session_binding as session;

pub const MAX_APPS: usize = 4;
struct Running {
    session_identity: Option<[u8; 32]>,
    store: Arc<crate::store::LocalStore>,
    observed_revision: Option<(StoreId, u64)>,
    read_volumes: Vec<StoreId>,
    writable_volumes: Vec<StoreId>,
    session: Option<session::Binding>,
    changed: Arc<AtomicBool>,
    blocked: Option<HostError>,
    instance: NativeInstance,
    inbox: super::in_process_host::Inbox,
    draining: bool,
    scene: Option<ViewNode>,
    dirty: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Step {
    pub ready: bool,
    pub wake_after: Option<Duration>,
}
pub struct Apps {
    running: Vec<Running>,
    observed: Option<FeedStatus>,
    running_gauge: crate::trace::Gauge,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Apps {
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            running: Vec::with_capacity(MAX_APPS),
            observed: None,
            wake,
            running_gauge: crate::trace::Gauge::new("native.running", 0),
        }
    }
    /// Bind an app to a concrete owned root. Initialization starts on advance.
    ///
    /// # Safety
    /// Callback/user-data lifetimes and cooperative execution obey playbit/sys/abi.h.
    /// The table and data remain valid until detach/clear/drop destroys the app.
    #[allow(unsafe_code, reason = "The host explicitly owns the borrowed C app lifecycle.")]
    pub unsafe fn attach(
        &mut self,
        services: &Services,
        root: StoreId,
        app: App,
    ) -> HostResult<u64> {
        // SAFETY: forwards the caller's unchanged callback/lifetime contract.
        unsafe { self.attach_in_role(services, root, app, 0) }
    }
    /// Explicit role metadata for adapter fixtures using an already-owned volume.
    /// This does not derive, initialize or authorize an automatic scope.
    ///
    /// # Safety
    /// The callback lifetime and cooperative execution contract is the same as attach.
    #[cfg(feature = "test-support")]
    #[allow(unsafe_code, reason = "Fixture callbacks obey the same owned attach lifecycle.")]
    pub unsafe fn attach_fixture_role(
        &mut self,
        services: &Services,
        root: StoreId,
        app: App,
        role: playbit_api::abi::data::VolumeRole,
    ) -> HostResult<u64> {
        // SAFETY: forwards the caller's unchanged callback/lifetime contract.
        unsafe { self.attach_in_role(services, root, app, role as u32) }
    }
    #[allow(unsafe_code, reason = "The caller owns callback code and data through teardown.")]
    unsafe fn attach_in_role(
        &mut self,
        services: &Services,
        root: StoreId,
        app: App,
        role: u32,
    ) -> HostResult<u64> {
        if self.running.len() == MAX_APPS {
            return Err(HostError::LimitExceeded);
        }
        let volume = services.store.volume(root).map_err(crate::runtime::store_bridge::error)?;
        let account = services.store.account().id;
        if volume.owner != account {
            return Err(HostError::AccessDenied);
        }
        let instance_id = StoreId::generate().map_err(crate::runtime::store_bridge::error)?;
        let mut context = GuestContext {
            app_name: "Native app".into(),
            app_build: None,
            identity: Identity {
                account_id: *account.as_bytes(),
                is_service: false,
                device_id: *account.as_bytes(),
                instance_id: *instance_id.as_bytes(),
                session_id: None,
            },
            volumes: vec![AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: *root.as_bytes(),
                name: volume.name,
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: role == 0 || role == 1,
                role,
            }],
            startup_arguments: BTreeMap::new(),
            trace_catalog: None,
            required_capabilities: BTreeSet::new(),
            participants: vec![],
        };
        #[cfg(feature = "test-support")]
        if role == playbit_api::context::VolumeRole::Session as u32 {
            let root = services
                .store
                .create_volume("Adapter fixture Root", false)
                .map_err(crate::runtime::store_bridge::error)?;
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
        let changed = Arc::new(AtomicBool::new(false));
        let backend = DataHost {
            store: services.store.clone(),
            access: Access::owner(account),
            changed: changed.clone(),
        };
        let store = services.store.clone();
        let (mut host, inbox) = super::in_process_host::InProcessHost::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            super::in_process_host::scoped_revision(store, None),
            self.wake.clone(),
        )
        .map_err(|_| HostError::Unavailable)?;
        context.volumes =
            host.enable_files(services, Access::owner(account), None, None, &context.volumes)?;
        let host = Box::new(host);
        // SAFETY: the caller retains callback code/data and owns this thread through teardown.
        let session_identity = context.identity.session_id;
        let instance = unsafe { NativeInstance::new(context, host, app) }.map_err(runtime_error)?;
        let generation = instance.generation();
        assert!(
            !self.running.iter().any(|app| app.instance.generation() == generation),
            "native generations are unique"
        );
        self.running.push(Running {
            session_identity,
            store: services.store.clone(),
            observed_revision: services.store.delivery_scope_revision(None, &[root]).0,
            read_volumes: vec![root],
            writable_volumes: vec![root],
            session: None,
            changed,
            blocked: None,
            instance,
            inbox,
            draining: false,
            scene: None,
            dirty: false,
        });
        self.running_gauge.set(self.running.len() as u64);
        Ok(generation)
    }
    /// Bind trusted native callbacks to a retained session and its exact package.
    /// The host guarantees these callbacks implement this package's app. Package
    /// resources and session grants use the same context records as Wasm.
    ///
    /// # Safety
    /// Callback code/user data obey attach's ownership contract through teardown.
    #[cfg(feature = "connected-engine")]
    #[allow(unsafe_code, reason = "The host owns trusted native callbacks through detach.")]
    pub unsafe fn attach_session(
        &mut self,
        services: &Services,
        session: &str,
        package: crate::package::Package,
        app: App,
        blobs: super::blob::BlobTransport,
    ) -> HostResult<u64> {
        if self.running.len() == MAX_APPS {
            return Err(HostError::LimitExceeded);
        }
        let (binding, mut context) = session::Binding::prepare(services, session, &package)?;
        let (read_volumes, writable_volumes) = scopes(&context.volumes);
        let changed = Arc::new(AtomicBool::new(false));
        let backend = super::host::LocalHost {
            data: DataHost {
                store: services.store.clone(),
                access: binding.access.clone(),
                changed: changed.clone(),
            },
            blobs: blobs.clone(),
            session: session.into(),
        };
        let store = services.store.clone();
        let (mut host, inbox) = super::in_process_host::InProcessHost::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            super::in_process_host::scoped_revision(store, Some(binding.delivery_volumes())),
            self.wake.clone(),
        )
        .map_err(|_| HostError::Unavailable)?;
        context.volumes = host.enable_files(
            services,
            binding.access.clone(),
            Some(session.into()),
            Some(blobs),
            &context.volumes,
        )?;
        let host = Box::new(host);
        // SAFETY: the caller retains callbacks and state until detach/clear/drop.
        let session_identity = context.identity.session_id;
        let instance = unsafe { NativeInstance::new(context, host, app) }.map_err(runtime_error)?;
        let generation = instance.generation();
        assert!(!self.running.iter().any(|app| app.instance.generation() == generation));
        self.running.push(Running {
            session_identity,
            store: services.store.clone(),
            observed_revision: services.store.delivery_scope_revision(None, &read_volumes).0,
            read_volumes,
            writable_volumes,
            session: Some(binding),
            changed,
            blocked: None,
            instance,
            inbox,
            draining: false,
            scene: None,
            dirty: false,
        });
        self.running_gauge.set(self.running.len() as u64);
        Ok(generation)
    }
    pub fn instance_id(&self, generation: u64) -> HostResult<[u8; 16]> {
        Ok(self.running[self.index(generation)?].instance.instance_id())
    }
    pub fn session_identity(&self, generation: u64) -> HostResult<Option<[u8; 32]>> {
        Ok(self.running[self.index(generation)?].session_identity)
    }
    fn index(&self, generation: u64) -> HostResult<usize> {
        self.running
            .iter()
            .position(|app| app.instance.generation() == generation)
            .ok_or(HostError::StaleHandle)
    }
    pub fn detach(&mut self, generation: u64) -> HostResult<()> {
        let index = self.index(generation)?;
        self.running.remove(index);
        self.running_gauge.set(self.running.len() as u64);
        Ok(())
    }
    /// Destroy every initialized app before releasing the services that own it.
    pub fn clear(&mut self) {
        self.running.clear();
        self.running_gauge.set(0);
    }
    pub fn input(&mut self, generation: u64, event: Event) -> HostResult<()> {
        let index = self.index(generation)?;
        let app = &mut self.running[index];
        if let Some(error) = app.blocked {
            return Err(error);
        }
        if event.kind != EventKind::SurfaceChanged {
            validate_input(app.scene.as_ref().ok_or(HostError::Conflict)?, &event)?;
        }
        // Surface geometry is checked by the runtime independently of a scene.
        app.instance.queue_event(generation, event)
    }
    /// Coalesce actual committed changes; gaps require a fresh app read as well
    /// as the host's own recovery. This never pumps an app inside a store callback.
    pub fn committed(&mut self, batch: &ChangeBatch) {
        let recovery = recovery::required(self.observed, batch.status);
        self.observed = Some(batch.status);
        for app in &mut self.running {
            if !app.instance.is_halted()
                && app.blocked.is_none()
                && (recovery
                    || batch
                        .records
                        .iter()
                        .any(|change| app.read_volumes.contains(&change.volume())))
            {
                app.dirty = true;
            }
        }
    }
    /// Coalesce bounded replica notices when the AP publisher owns the store's
    /// change feed. Account notices also refresh consent and session authority.
    #[cfg(feature = "connected-engine")]
    pub fn received(&mut self, notices: &[super::connected::ReplicaNotice]) -> HostResult<()> {
        if notices.len() > super::replication::NOTICE_LIMIT {
            return Err(HostError::LimitExceeded);
        }
        use super::connected::ReplicaSource;
        for app in &mut self.running {
            if app.instance.is_halted() || app.blocked.is_some() {
                continue;
            }
            app.dirty |= notices.iter().any(|notice| match notice.source {
                ReplicaSource::Account => true,
                ReplicaSource::Volume(volume) => app.read_volumes.contains(&volume),
                ReplicaSource::Entry(address) => app.read_volumes.contains(&address.volume),
            });
        }
        Ok(())
    }
    /// Content readiness is independent of AP subscription health. Both success
    /// and terminal failure let a waiting app retry its range once.
    /// Output borrows only this turn's frame. Retain the semantic scene after
    /// delivery, without another tree clone or persistent encoded-frame buffer.
    pub fn advance(&mut self, generation: u64, output: impl FnOnce(&Frame)) -> HostResult<()> {
        let index = self.index(generation)?;
        let (before, current) = self.running.split_at_mut(index);
        let (app, after) = current.split_first_mut().expect("index names an attached app");
        if let Some(error) = app.blocked {
            return Err(error);
        }
        if let Some(binding) = &mut app.session {
            match binding.refresh().and_then(|volumes| {
                if let Some(volumes) = volumes {
                    let volumes = app.inbox.prepare_files(&volumes)?;
                    let (read, writable) = scopes(&volumes);
                    if app.instance.replace_volumes(volumes)? {
                        app.dirty = true;
                    }
                    app.read_volumes = read;
                    app.writable_volumes = writable;
                }
                Ok(())
            }) {
                Ok(()) => {}
                Err(error) => {
                    app.blocked = Some(error);
                    return Err(error);
                }
            }
        }
        if let Some(volumes) = app.inbox.retry_file_roots()? {
            let (read, writable) = scopes(&volumes);
            app.dirty |= app.instance.replace_volumes(volumes)?;
            app.read_volumes = read;
            app.writable_volumes = writable;
        }
        // Bootstrap/reconnect merges update scoped history without publishing
        // an outbound change batch. Observe on actual owner turns as well as
        // explicit live notices; the AP publisher retains its sole consumer.
        let (revision, changed) =
            app.store.delivery_scope_revision(app.observed_revision, &app.read_volumes);
        if revision.is_none() {
            app.blocked = Some(HostError::Unavailable);
            return Err(HostError::Unavailable);
        }
        app.observed_revision = revision;
        app.dirty |= changed.is_some();
        if app.dirty && !app.draining {
            match app.instance.queue_event(
                generation,
                Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
            ) {
                Ok(()) => {
                    app.dirty = false;
                }
                // Let the app drain its admitted input, then notify on the next turn.
                Err(HostError::LimitExceeded) => {}
                Err(error) => return Err(error),
            }
        }
        for (generation, token, creation) in app.inbox.ready() {
            let result = if creation {
                app.instance.notify_creation(generation, token)
            } else {
                app.instance.notify_data(generation, token)
            };

            match result {
                Ok(()) => {}
                Err(HostError::LimitExceeded) => app.inbox.defer(token),
                Err(error) => return Err(error),
            }
        }
        let result = app.instance.pump();
        // A failing callback may have committed data. Invalidate shared readers
        // before returning its failure; never wait for AP to echo a local write.
        if app.changed.swap(false, Ordering::AcqRel) {
            for other in before.iter_mut().chain(after) {
                if (app.session.is_some() || other.session.is_some())
                    && !other.instance.is_halted()
                    && other.blocked.is_none()
                    && app.writable_volumes.iter().any(|id| other.read_volumes.contains(id))
                {
                    other.dirty = true;
                }
            }
        }
        let frame = result.map_err(runtime_error)?;
        output(&frame);
        if let Some(scene) = frame.into_view() {
            app.scene = Some(scene);
        }
        Ok(())
    }
    pub fn deactivate(&mut self, generation: u64) -> HostResult<()> {
        let index = self.index(generation)?;
        let app = &mut self.running[index];
        app.draining = true;
        app.instance.deactivate();
        Ok(())
    }
    pub fn drain_pending(&self, generation: u64) -> HostResult<bool> {
        let app = &self.running[self.index(generation)?];
        // I/O may finish between these observations. Check waiting first so
        // that a newly completed operation remains visible to has_ready.
        if app.instance.drain_failed()
            || app.blocked.is_some()
            || (app.draining
                && app.instance.drain_pending()
                && !app.instance.has_ready_work(Instant::now())
                && !app.inbox.waiting()
                && !app.inbox.has_ready())
        {
            return Err(HostError::Unavailable);
        }
        Ok(app.instance.drain_pending() || !app.inbox.idle())
    }
    pub fn scene(&self, generation: u64) -> HostResult<Option<&ViewNode>> {
        Ok(self.running[self.index(generation)?].scene.as_ref())
    }
    pub fn step(&self, generation: u64, now: Instant) -> HostResult<Step> {
        let app = &self.running[self.index(generation)?];
        let (revision, changed) =
            app.store.delivery_scope_revision(app.observed_revision, &app.read_volumes);
        Ok(Step {
            ready: app.blocked.is_none()
                && !app.instance.is_halted()
                && (((app.dirty || changed.is_some() || revision.is_none()) && !app.draining)
                    || app.inbox.has_ready()
                    || app.instance.has_ready_work(now)),
            wake_after: if app.blocked.is_none() { app.instance.wake_after(now) } else { None },
        })
    }
}
fn scopes(volumes: &[AvailableVolume]) -> (Vec<StoreId>, Vec<StoreId>) {
    assert!(volumes.len() <= playbit_api::abi::syscall::MAX_VOLUMES);
    let read = volumes
        .iter()
        .filter(|volume| volume.backing == playbit_api::abi::data::VOLUME_PLAYBIT)
        .map(|volume| StoreId::from_bytes(volume.id))
        .collect();
    let writable = volumes
        .iter()
        .filter(|volume| volume.rights.0 & VolumeRights::WRITE.0 != 0)
        .map(|volume| StoreId::from_bytes(volume.id))
        .collect();
    (read, writable)
}

fn runtime_error(error: RuntimeError) -> HostError {
    match error {
        RuntimeError::NativeApp(status) => {
            HostError::try_from(status).unwrap_or(HostError::Internal)
        }
        RuntimeError::GuestHalted => HostError::Conflict,
        _ => HostError::Internal,
    }
}

#[cfg(test)]
mod tests;
