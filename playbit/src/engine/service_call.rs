//! One owned data envelope, with exact context/operation/result correlation.
use super::session_instance::Context as InstanceContext;
use crate::runtime::host::{DataCall, DataResult, HostResult};
use playbit_api::abi::syscall::Error as HostError;

/// Long-lived receives have an independent ordered lane below file tokens.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(crate) const CHANNEL_RECEIVE_BIT: u64 = 1 << 62;

/// Guest authority and trusted tool authority are established separately. Both
/// then use the same owned data contract and bounded service executor.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[serde(tag = "kind", content = "binding", rename_all = "kebab-case")]
pub enum Context {
    SessionInstance(InstanceContext),
    Tool { handle: crate::store::Id, incarnation: crate::store::Id },
}

/// Trusted tools choose a concrete binding before submitting data. Volume
/// binding resolves the current account or authorized joined session once.
#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[serde(tag = "kind", content = "binding", rename_all = "kebab-case")]
pub enum ToolScope {
    Account,
    Session(String),
    Volume(crate::store::Id),
    /// Browse with the account ACL, retaining a legacy Root session for AP reads.
    /// A Shared participant must select its session explicitly.
    BrowserVolume(crate::store::Id),
}
#[cfg(all(not(target_os = "ios"), feature = "process"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    SessionInstance(crate::store::Id),
    Tool(crate::store::Id),
}
impl From<InstanceContext> for Context {
    fn from(context: InstanceContext) -> Self {
        Self::SessionInstance(context)
    }
}
impl Context {
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn key(self) -> Key {
        match self {
            Self::SessionInstance(context) => Key::SessionInstance(context.handle),
            Self::Tool { handle, .. } => Key::Tool(handle),
        }
    }
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn handle(self) -> crate::store::Id {
        match self {
            Self::SessionInstance(context) => context.handle,
            Self::Tool { handle, .. } => handle,
        }
    }
}
impl ArchivedContext {
    pub(crate) fn matches(&self, expected: Context) -> bool {
        match (self, expected) {
            (Self::SessionInstance(actual), Context::SessionInstance(expected)) => {
                actual.instance.0 == *expected.instance.as_bytes()
                    && actual.handle.0 == *expected.handle.as_bytes()
                    && actual.incarnation.0 == *expected.incarnation.as_bytes()
                    && actual.owner_generation.to_native() == expected.owner_generation
            }
            (
                Self::Tool { handle, incarnation },
                Context::Tool { handle: id, incarnation: epoch },
            ) => handle.0 == *id.as_bytes() && incarnation.0 == *epoch.as_bytes(),
            _ => false,
        }
    }
}

#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Outcome {
    pub context: Context,
    pub volume: crate::store::Id,
    pub operation: u64,
    pub mutation: u64,
    pub kind: u32,
    pub revision: Option<(crate::store::Id, u64)>,
    pub status: i32,
    pub result: Option<DataResult>,
}
impl Outcome {
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    fn known_receipt(&self) -> bool {
        self.status == 0
            && self.mutation != 0
            && self.revision.is_some()
            && crate::runtime::host::DataKind::from_raw(self.kind).is_ok_and(|kind| {
                self.result.as_ref().is_some_and(|result| result.is_receipt(kind))
            })
    }
    pub fn take(
        self,
        context: impl Into<Context>,
        operation: u64,
        mutation: u64,
        call: &DataCall,
    ) -> HostResult<DataResult> {
        if self.context != context.into()
            || self.volume != crate::store::Id::from_bytes(call.volume)
            || self.operation != operation
            || self.mutation != mutation
            || self.kind != call.operation.kind() as u32
        {
            return Err(HostError::Internal);
        }
        if self.status == 0 {
            if !call.operation.network() {
                self.revision.ok_or(HostError::Internal)?;
            }
            let result = self.result.ok_or(HostError::Internal)?;
            call.validate_result(&result, Default::default())?;
            Ok(result)
        } else if self.result.is_some() || self.status == HostError::WouldBlock as i32 {
            Err(HostError::Internal)
        } else {
            Err(HostError::try_from(self.status).unwrap_or(HostError::Internal))
        }
    }
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
#[path = "service_executor.rs"]
pub(super) mod executor;

#[cfg(all(not(target_os = "ios"), feature = "process"))]
impl super::Engine {
    /// Delivery reads bounded session metadata and six automatic addresses,
    /// without probing scope files. KV cuts tolerate content changes, while
    /// every read fences authority. Never replay work or obscure a commit ack.
    pub(crate) fn fence_service(&self, outcome: &mut Outcome) {
        if outcome.known_receipt() {
            return;
        }
        let error = if !self.owns_service_context(outcome.context) {
            Some(HostError::StaleHandle)
        } else if outcome.mutation == 0
            && outcome.status == 0
            && !matches!(
                crate::runtime::host::DataKind::from_raw(outcome.kind),
                Ok(crate::runtime::host::DataKind::ChannelEvents
                    | crate::runtime::host::DataKind::ChannelCommand)
            )
        {
            let changed = match outcome.context {
                Context::SessionInstance(context) => {
                    self.session_instances.descriptor(context.instance).and_then(|instance| {
                        let volumes = instance_delivery_volumes(&self.services.store, &instance)?;
                        Ok(read_delivery_revision(
                            &self.services.store,
                            outcome.kind,
                            outcome.revision,
                            &volumes,
                        ))
                    })
                }
                Context::Tool { .. } => self
                    .tool_contexts
                    .delivery_volumes(outcome.context, outcome.volume)
                    .map(|volumes| {
                        read_delivery_revision(
                            &self.services.store,
                            outcome.kind,
                            outcome.revision,
                            &volumes,
                        )
                    })
                    .ok_or_else(|| "Tool context has lost its delivery scope".into()),
            };
            match changed {
                Ok((Some(_), None)) => None,
                _ => Some(HostError::Unavailable),
            }
        } else {
            None
        };
        if let Some(error) = error {
            outcome.status = error as i32;
            outcome.result = None;
        }
    }
    pub(crate) fn begin_service(
        &mut self,
        context: impl Into<Context>,
        operation: u64,
        mutation: u64,
        call: DataCall,
        peer: &std::os::unix::net::UnixStream,
    ) -> HostResult<executor::Ticket> {
        let context = context.into();
        let (binding, access, session) = match context {
            Context::SessionInstance(context) => {
                if call.operation.tool_only() {
                    return Err(HostError::Unsupported);
                }
                let instance = self
                    .session_instances
                    .authorize(context, peer)
                    .map_err(|_| HostError::StaleHandle)?;
                if call.operation.network() {
                    let requested = match &call.operation {
                        crate::runtime::host::DataOperation::ChannelEvents { instance }
                        | crate::runtime::host::DataOperation::ChannelCommand {
                            instance, ..
                        } => instance,
                        _ => return Err(HostError::Unsupported),
                    };
                    if requested != instance.id.as_bytes() {
                        return Err(HostError::AccessDenied);
                    }
                } else {
                    ceiling(
                        &instance,
                        crate::store::Id::from_bytes(call.volume),
                        call.operation.rights(),
                        automatic_instance_rights(
                            &self.services.store,
                            &instance,
                            crate::store::Id::from_bytes(call.volume),
                        )?,
                    )?;
                }
                let access = crate::store::Access::session_version(
                    instance.account,
                    &instance.session,
                    &instance.build,
                );
                let session = instance.session.clone();
                (executor::Binding::SessionInstance(instance), access, session)
            }
            Context::Tool { .. } => {
                let binding = self.tool_contexts.authorize(context, peer)?;
                if call.operation.network() {
                    return Err(HostError::Unsupported);
                }
                let access = binding.access();
                let session =
                    binding.session.as_ref().map_or_else(String::new, |session| session.id.clone());
                (executor::Binding::Tool(binding), access, session)
            }
        };
        let registry = &self.session_instances;
        let tools = &self.tool_contexts;
        self.service_executor.record_request(
            context,
            operation,
            mutation,
            &call,
            |key| match key {
                Key::SessionInstance(handle) => registry.owns_handle(handle),
                Key::Tool(handle) => tools.owns_handle(handle),
            },
        )?;
        let host = super::host::LocalHost {
            data: super::data_host::DataHost {
                store: self.services.store.clone(),
                access,
                changed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            blobs: self.network.blobs.clone(),
            session,
        };
        self.service_executor.begin(
            context,
            executor::Order { operation, mutation },
            call,
            host,
            binding,
            self.completion.wake.clone(),
        )
    }
    fn owns_service_context(&self, context: Context) -> bool {
        match context {
            Context::SessionInstance(context) => self.session_instances.owns_context(context),
            Context::Tool { .. } => self.tool_contexts.owns(context),
        }
    }
    pub(crate) fn maintain_service(&mut self) -> super::Result<()> {
        self.maintain_files()
            .map_err(|error| format!("File worker maintenance failed: {error:?}"))?;
        if self.service_executor.maintain()? {
            self.state_changed();
        }
        Ok(())
    }
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) fn read_delivery_revision(
    store: &crate::store::LocalStore,
    kind: u32,
    previous: Option<(crate::store::Id, u64)>,
    volumes: &[crate::store::Id],
) -> (Option<(crate::store::Id, u64)>, Option<u64>) {
    if content_snapshot_kind(kind) {
        store.delivery_snapshot_revision(previous, volumes)
    } else {
        store.delivery_scope_revision(previous, volumes)
    }
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
fn content_snapshot_kind(kind: u32) -> bool {
    use crate::runtime::host::DataKind;
    matches!(DataKind::from_raw(kind), Ok(DataKind::KvKeys | DataKind::KvRead))
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
fn ceiling(
    instance: &super::session_instance::SessionInstance,
    volume: crate::store::Id,
    rights: crate::runtime::host::VolumeRights,
    automatic: Option<crate::runtime::host::VolumeRights>,
) -> HostResult<()> {
    if let Some(automatic) = automatic {
        return if automatic.contains(rights) { Ok(()) } else { Err(HostError::AccessDenied) };
    }
    if volume == instance.root {
        return Ok(());
    }
    let grant = instance
        .extra_volumes
        .iter()
        .find(|grant| grant.volume == volume)
        .ok_or(HostError::AccessDenied)?;
    if super::session_context::personal_rights(grant.write).for_data().contains(rights) {
        Ok(())
    } else {
        Err(HostError::AccessDenied)
    }
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) fn automatic_instance_rights(
    store: &crate::store::LocalStore,
    instance: &super::session_instance::SessionInstance,
    volume: crate::store::Id,
) -> HostResult<Option<crate::runtime::host::VolumeRights>> {
    use crate::runtime::store_bridge::error;
    let session = store.session(&instance.session).map_err(error)?;
    let Some(addresses) = store.automatic_volume_addresses(&session).map_err(error)? else {
        return Ok(None);
    };
    if instance.account != store.account().id
        || session.resolved_build_id != instance.build
        || session.root_volume != instance.root
    {
        return Err(HostError::StaleHandle);
    }
    let Some(address) = addresses.iter().find(|address| address.volume == volume) else {
        return Ok(None);
    };
    super::session_context::automatic_rights(
        store,
        &session,
        address.role,
        volume,
        address.overridden,
    )
    .map(|rights| Some(rights.for_data()))
    .map_err(|error| match error.downcast::<crate::store::Error>() {
        Ok(error) => crate::runtime::store_bridge::error(*error),
        Err(_) => HostError::InvalidArgument,
    })
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) fn instance_delivery_volumes(
    store: &crate::store::LocalStore,
    instance: &super::session_instance::SessionInstance,
) -> super::Result<Vec<crate::store::Id>> {
    if instance.account != store.account().id {
        return Err("Session-instance account binding changed".into());
    }
    let session = store.session(&instance.session)?;
    if session.root_volume != instance.root {
        return Err("Session-instance root binding changed".into());
    }
    let mut volumes = instance.delivery_volumes();
    if let Some(addresses) = store.automatic_volume_addresses(&session)? {
        volumes.extend(addresses.iter().map(|address| address.volume));
        volumes.sort_unstable();
        volumes.dedup();
    }
    Ok(volumes)
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
fn check_binding(
    instance: &super::session_instance::SessionInstance,
    host: &super::host::LocalHost,
    volume: crate::store::Id,
    rights: crate::runtime::host::VolumeRights,
) -> HostResult<()> {
    use crate::runtime::store_bridge::error;
    let store = &host.data.store;
    if instance.account != store.account().id {
        return Err(HostError::StaleHandle);
    }
    let session = store.session(&instance.session).map_err(error)?;
    if session.resolved_build_id != instance.build || session.root_volume != instance.root {
        return Err(HostError::StaleHandle);
    }
    if store.session_denial(&instance.session).map_err(error)?.is_some() {
        return Err(HostError::AccessDenied);
    }
    let automatic = automatic_instance_rights(store, instance, volume)?;
    ceiling(instance, volume, rights, automatic)?;
    if automatic.is_some() {
        // Admission qualifies the exact lazy address. Only this storage worker
        // may open it, using the signed initializer and current root authority.
        store.materialize_automatic_access(&host.data.access, volume).map_err(error)?;
    } else if volume != instance.root {
        let selected =
            store.personal_volume_grants(&instance.session).map_err(error)?.unwrap_or_default();
        if !selected.iter().any(|grant| {
            grant.volume == volume
                && super::session_context::personal_rights(grant.write).for_data().contains(rights)
        }) {
            return Err(HostError::AccessDenied);
        }
    }
    store.admit_binding(&host.data.access, volume).map_err(error)?;
    Ok(())
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
fn check_result(
    host: &super::host::LocalHost,
    volume: crate::store::Id,
    entry: Option<[u8; 15]>,
    result: &DataResult,
) -> HostResult<()> {
    use crate::{
        runtime::store_bridge::error,
        store::{EntryId, volume_index::EntryAddress},
    };
    let store = &host.data.store;
    let access = &host.data.access;
    let id = match result {
        DataResult::EntryResolved(id) => {
            store.admit_catalog_read(access, volume).map_err(error)?;
            Some(*id)
        }
        _ => entry,
    };
    if let Some(id) = id {
        let address = EntryAddress { volume, entry: EntryId::from_bytes(id).map_err(error)? };
        match result {
            DataResult::File(_) | DataResult::FileMetadata { .. } => {
                store.blob_version(access, address, None).map(|_| ()).map_err(error)
            }
            _ => store.entry(access, volume, address.entry).map(|_| ()).map_err(error),
        }
    } else {
        Ok(())
    }
}

#[cfg(all(test, not(target_os = "ios"), feature = "process"))]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert isolated service admission and durable storage behavior."
)]
mod tests {
    use super::*;
    use crate::{
        engine::{Engine, Request, reply::Reply, session_instance::Admission},
        runtime::host::*,
        store::{Access, Id},
    };
    use std::{
        collections::BTreeMap,
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };

    struct Fixture {
        engine: Engine,
        admission: Admission,
        peer: UnixStream,
        _client: UnixStream,
        _directory: tempfile::TempDir,
        operation: u64,
        mutation: u64,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_replication(false)
        }
        fn with_replication(share_key_with_service: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = crate::engine::tests::object_volume(
                &engine.services.store,
                "Service fixture",
                share_key_with_service,
            );
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            Self::with_session(directory, engine, session)
        }
        fn automatic() -> Self {
            use crate::package::{AutomaticVolumeRole as Role, Package, VolumeInitializers};
            use crate::store::{BuildSelector, SessionCreation};
            let directory = tempfile::tempdir().unwrap();
            let engine = Engine::open(directory.path()).unwrap();
            let store = &engine.services.store;
            let initializers = VolumeInitializers::from_json(include_bytes!(
                "../../../guests/todo/playbit-volumes.json"
            ))
            .unwrap();
            let package = crate::package::signature::testing::signed(
                store,
                Package::create(
                    "automatic-service".into(),
                    "Automatic service".into(),
                    b"\0asm\x01\0\0\0".to_vec(),
                    BTreeMap::new(),
                    &initializers,
                )
                .unwrap(),
            )
            .unwrap();
            let app = package.identity().unwrap().clone();
            let build = package.id().unwrap();
            let builds = directory.path().join(crate::paths::BUILDS);
            std::fs::create_dir_all(&builds).unwrap();
            std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
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
            Self::with_session(directory, engine, session)
        }
        fn with_session(
            directory: tempfile::TempDir,
            mut engine: Engine,
            session: crate::store::Session,
        ) -> Self {
            let (peer, client) = UnixStream::pair().unwrap();
            let Reply::Instance(instance) = engine
                .dispatch_instance(
                    Request::InstanceReserve {
                        restore_host_mounts: false,
                        request: crate::store::Id::generate().unwrap(),
                        incarnation: engine.session_instances.incarnation(),
                        session: session.id,
                        extra_volumes: Box::default(),
                        host_mounts: Box::default(),
                    },
                    &peer,
                )
                .unwrap()
            else {
                panic!("instance reservation")
            };
            let Reply::InstanceAdmission(admission) = engine
                .dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer)
                .unwrap()
            else {
                panic!("instance claim")
            };
            Self {
                engine,
                admission,
                peer,
                _client: client,
                _directory: directory,
                operation: 0,
                mutation: 0,
            }
        }
        fn begin(&mut self, operation: DataOperation) -> (executor::Ticket, DataCall) {
            self.begin_on(self.admission.instance.root, operation)
        }
        fn begin_on(
            &mut self,
            volume: Id,
            operation: DataOperation,
        ) -> (executor::Ticket, DataCall) {
            self.operation += 1;
            if operation.is_mutation() {
                self.mutation += 1;
            }
            let call = DataCall { volume: *volume.as_bytes(), operation };
            let ticket = self
                .engine
                .begin_service(
                    self.admission.context,
                    self.operation,
                    if call.operation.is_mutation() { self.mutation } else { 0 },
                    call.clone(),
                    &self.peer,
                )
                .unwrap();
            (ticket, call)
        }
        fn finish(&mut self, ticket: executor::Ticket, call: &DataCall) -> HostResult<DataResult> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ticket.ready() {
                self.engine.maintain_service().unwrap();
                assert!(Instant::now() < deadline, "accepted storage work did not finish");
                std::thread::yield_now();
            }
            let operation = ticket.operation;
            let mutation = ticket.mutation;
            self.engine.service_executor.take(ticket).unwrap().take(
                self.admission.context,
                operation,
                mutation,
                call,
            )
        }
        fn run(&mut self, operation: DataOperation) -> HostResult<DataResult> {
            let (ticket, call) = self.begin(operation);
            self.finish(ticket, &call)
        }
        fn run_on(&mut self, volume: Id, operation: DataOperation) -> HostResult<DataResult> {
            let (ticket, call) = self.begin_on(volume, operation);
            self.finish(ticket, &call)
        }
        fn kv_entry(&self) -> crate::store::volume_index::EntryAddress {
            let store = &self.engine.services.store;
            let owner = Access::owner(store.account().id);
            let volume = self.admission.instance.root;
            let entry = store
                .create_database(
                    &owner,
                    volume,
                    "settings",
                    crate::store::volume_index::EntryKind::KvDb {},
                )
                .unwrap();
            let address = crate::store::volume_index::EntryAddress { volume, entry: entry.id };
            store.kv_put(&owner, address, b"selected", b"original snapshot").unwrap();
            address
        }
        fn retained(&mut self, ticket: executor::Ticket) -> Outcome {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ticket.ready() {
                self.engine.maintain_service().unwrap();
                assert!(Instant::now() < deadline, "retained read did not complete");
                std::thread::yield_now();
            }
            self.engine.service_executor.take(ticket).unwrap()
        }
        fn establish(&mut self) {
            assert_eq!(
                self.run(DataOperation::SchemaEstablish {
                    entry_id: [5; 15],
                    schema: Schema {
                        types: vec![TypeDeclaration {
                            id: [1; 16],
                            name: "Task".into(),
                            deprecated: false,
                            fields: vec![FieldDeclaration {
                                id: [2; 16],
                                name: "Title".into(),
                                value_type: ValueType::Text,
                                repeated: false,
                                deprecated: false
                            }],
                        }]
                    }
                }),
                Ok(DataResult::SchemaEstablished)
            );
        }
        fn create(&mut self) -> [u8; 16] {
            let DataResult::Created(id) = self
                .run(DataOperation::Create {
                    entry_id: [5; 15],
                    type_id: [1; 16],
                    fields: title("Created"),
                })
                .unwrap()
            else {
                panic!("creation result")
            };
            id
        }
    }
    fn title(text: &str) -> Vec<FieldChange> {
        vec![FieldChange { field_id: [2; 16], values: Some(vec![Value::Text(text.into())]) }]
    }

    fn qualify_selected_root(f: &Fixture) {
        use crate::store::{DatabaseId, DatabasePolicy, EntryId, ScopedSnapshot};
        let store = &f.engine.services.store;
        let volume = f.admission.instance.root;
        let entry = EntryId::from_bytes([5; 15]).unwrap();
        let access = Access::session_version(
            store.account().id,
            &f.admission.instance.session,
            &f.admission.instance.build,
        );
        let mut stage = store
            .prepare_received_entry(crate::store::volume_index::EntryAddress { volume, entry })
            .unwrap();
        let policy = store
            .with_replica(volume, DatabaseId::Index, |source| {
                DatabasePolicy::read(source, DatabaseId::Entry(entry))
            })
            .unwrap();
        for database in [DatabaseId::Index, DatabaseId::Revocations, DatabaseId::Entry(entry)] {
            store
                .with_replica(volume, database, |source| {
                    stage.with_database(database, |target| {
                        if database == DatabaseId::Entry(entry) {
                            crate::store::merge_replica(
                                &std::env::temp_dir(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            let selected = ScopedSnapshot::new(
                                source,
                                database,
                                &policy,
                                store.account().id,
                                access.build_id(),
                            )?
                            .project(&std::env::temp_dir())?;
                            if database == DatabaseId::Index {
                                crate::store::merge_index_replica(
                                    &std::env::temp_dir(),
                                    selected.database(),
                                    target,
                                    kvdb::Lww,
                                    |_| Ok(()),
                                )
                            } else {
                                crate::store::merge_replica(
                                    &std::env::temp_dir(),
                                    selected.database(),
                                    target,
                                    kvdb::Lww,
                                    |_| Ok(()),
                                )
                            }
                        }
                    })
                })
                .unwrap();
        }
        stage.finish_database(DatabaseId::Entry(entry)).unwrap();
        store
            .with_replica(volume, DatabaseId::Index, |source| {
                use crate::store::volume_index::namespace::{receipt, source::cut};
                let accepted = cut::accepted(&source.begin_read()?)?;
                stage.with_database(DatabaseId::Index, |target| {
                    receipt::retain_fixture(target, Some(entry), &accepted)
                })
            })
            .unwrap();
        stage.qualify_authority(); // Trusted, authenticated AP receipt fixture.
        store.adopt_received_volume(&access, &stage).unwrap();
    }

    #[test]
    fn selected_capture_admits_native_binding_and_service_without_catalog_access() {
        let mut f = Fixture::new();
        f.establish();
        let object = f.create();
        let root = f.admission.instance.root;
        let store = f.engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let sibling = store
            .create_database(
                &owner,
                root,
                "sibling",
                crate::store::volume_index::EntryKind::ObjectDb { schema: Default::default() },
            )
            .unwrap()
            .id;
        qualify_selected_root(&f);
        let session = store.session(&f.admission.instance.session).unwrap();
        f.engine.check_instance_build(&session, &f.admission.instance.build).unwrap();
        let (mut binding, _) = super::super::session_binding::Binding::prepare(
            &f.engine.services,
            &session.id,
            &crate::engine::tests::button_package(),
        )
        .unwrap();
        assert!(binding.refresh().is_ok());
        assert!(matches!(
            f.run(DataOperation::Read {
                entry_id: [5; 15],
                object_id: object,
                fields: vec![[2; 16]]
            }),
            Ok(DataResult::Object(_))
        ));
        assert_eq!(
            f.run(DataOperation::SchemaRead { entry_id: *sibling.as_bytes() }),
            Err(HostError::AccessDenied)
        );
        let access =
            Access::session_version(store.account().id, &session.id, &session.resolved_build_id);
        assert!(store.entries(&access, root, None, 64).is_err());
        assert!(store.resolve_entry(&access, root, "data").is_err());
        let (ticket, call) = f.begin(DataOperation::Read {
            entry_id: [5; 15],
            object_id: object,
            fields: vec![[2; 16]],
        });
        let retained = f.finish(ticket, &call).unwrap();
        let host = crate::engine::host::LocalHost {
            data: crate::engine::data_host::DataHost {
                store: store.clone(),
                access: access.clone(),
                changed: std::sync::Arc::default(),
            },
            blobs: f.engine.network.blobs.clone(),
            session: session.id.clone(),
        };
        store.invalidate_authority(&owner, root).unwrap();
        assert!(f.engine.check_instance_build(&session, &f.admission.instance.build).is_err());
        assert_eq!(
            check_result(&host, root, Some([5; 15]), &retained),
            Err(HostError::AccessDenied)
        );
        qualify_selected_root(&f);
        f.engine.check_instance_build(&session, &f.admission.instance.build).unwrap();
        assert!(store.entries(&access, root, None, 64).is_err());
        store.record_session_denial(&session, "revoked").unwrap();
        assert!(f.engine.check_instance_build(&session, &f.admission.instance.build).is_err());
        assert_eq!(
            check_result(&host, root, Some([5; 15]), &retained),
            Err(HostError::AccessDenied)
        );
    }
    #[test]
    fn offline_bloblog_append_stays_uncommitted_after_tool_close_and_checks_guest_grants() {
        let mut f = Fixture::new();
        let volume = f.engine.services.store.create_volume("Accepted log", true).unwrap().id;
        let context = f
            .engine
            .tool_contexts
            .open(&f.engine.services.store, ToolScope::Volume(volume), &f.peer)
            .unwrap();
        let log = f
            .engine
            .services
            .store
            .create_bloblog(
                &Access::owner(f.engine.services.store.account().id),
                volume,
                "activity",
            )
            .unwrap()
            .id;
        let call = DataCall {
            volume: *volume.as_bytes(),
            operation: DataOperation::BloblogAppend {
                append_id: [1; 16],
                log: *log.as_bytes(),
                blobs: vec![b"Accepted once".to_vec()],
            },
        };
        assert!(matches!(
            f.engine.begin_service(f.admission.context, 1, 1, call.clone(), &f.peer),
            Err(HostError::AccessDenied)
        ));
        let ticket = f.engine.begin_service(context, 1, 1, call, &f.peer).unwrap();
        f.engine.tool_contexts.close(context, &f.peer).unwrap();
        drop(ticket);
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.engine.service_executor.pending() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let store = &f.engine.services.store;
        let page =
            store.bloblog_get(&Access::owner(store.account().id), volume, log, None).unwrap();
        assert!(page.entries.is_empty());
    }

    #[test]
    fn closing_channels_delivers_the_cancelled_receive_and_preserves_bloblog_reads() {
        let mut f = Fixture::with_replication(true);
        let store = f.engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let volume = f.admission.instance.root;
        let log = store.create_bloblog(&owner, volume, "activity").unwrap().id;
        store.bloblog_put(&owner, volume, log, &[b"saved".to_vec()]).unwrap();
        let (read, read_call) =
            f.begin(DataOperation::BloblogRead { log: *log.as_bytes(), cursor: None });
        let context = f.admission.context;
        let call = DataCall {
            volume: [0; 16],
            operation: DataOperation::ChannelEvents { instance: *context.instance.as_bytes() },
        };
        let receive = f
            .engine
            .begin_service(context, CHANNEL_RECEIVE_BIT | 1, 0, call.clone(), &f.peer)
            .unwrap();

        // Close before either worker starts. Both callers still own their tickets.
        f.engine.dispatch_instance(Request::InstanceChannelsClose { context }, &f.peer).unwrap();
        assert_eq!(f.finish(receive, &call), Err(HostError::Closed));
        let DataResult::BloblogPage(page) = f.finish(read, &read_call).unwrap() else {
            panic!("expected saved entries")
        };
        assert_eq!(page.entries[0].data, b"saved");
    }

    #[test]
    fn guest_bloblog_uses_authorized_storage_and_scoped_cursors() {
        // Offline appends fail; verified cached entries remain readable with scoped cursors.
        let mut f = Fixture::with_replication(true);
        let store = f.engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let volume = f.admission.instance.root;
        let log = store.create_bloblog(&owner, volume, "activity").unwrap().id;
        let other = store.create_bloblog(&owner, volume, "other").unwrap().id;
        let append = DataOperation::BloblogAppend {
            append_id: [1; 16],
            log: *log.as_bytes(),
            blobs: vec![vec![0, 255, 128], vec![]],
        };
        assert_eq!(f.run(append), Err(HostError::Unavailable));
        let cursor = store.bloblog_put(&owner, volume, log, &[vec![0, 255, 128], vec![]]).unwrap();
        let DataResult::BloblogPage(page) =
            f.run(DataOperation::BloblogRead { log: *log.as_bytes(), cursor: None }).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            page.entries.iter().map(|entry| entry.data.as_slice()).collect::<Vec<_>>(),
            [&[0, 255, 128][..], &[]]
        );
        assert_ne!(page.cursor, cursor);
        let after = page.cursor;
        assert_eq!(
            f.run(DataOperation::BloblogRead { log: *log.as_bytes(), cursor: Some(after) }),
            Err(HostError::Unavailable)
        );
        let DataResult::BloblogPage(page) = f
            .run(DataOperation::BloblogRead { log: *log.as_bytes(), cursor: Some(cursor.clone()) })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(page.entries.len(), 2);
        assert_eq!(
            f.run(DataOperation::BloblogRead { log: *other.as_bytes(), cursor: Some(cursor) }),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(
            f.run(DataOperation::BloblogAppend {
                append_id: [1; 16],
                log: [5; 15],
                blobs: vec![vec![1]]
            }),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(store.bloblog_get(&owner, volume, log, None).unwrap().entries.len(), 2);

        let mut f = Fixture::automatic();
        let store = f.engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let volume = f.admission.instance.root;
        assert_eq!(
            f.run(DataOperation::ResolveEntry { name: "activity".into() }),
            Err(HostError::NotFound)
        );
        let log = store.create_bloblog(&owner, volume, "activity").unwrap().id;
        let read = DataOperation::BloblogRead { log: *log.as_bytes(), cursor: None };
        assert_eq!(f.run(read.clone()), Err(HostError::Unavailable));

        let entries = crate::store::bloblog::append_entries(
            store.account().id,
            volume,
            log,
            Id::generate().unwrap(),
            &[b"cached".to_vec()],
        )
        .unwrap();
        store.bloblog_receive(&owner, volume, log, &entries).unwrap();
        let DataResult::BloblogPage(page) = f.run(read).unwrap() else {
            panic!("cached automatic-volume page")
        };
        assert_eq!(page.entries, entries);
        assert_eq!(
            f.run(DataOperation::BloblogRead { log: *log.as_bytes(), cursor: Some(page.cursor) }),
            Err(HostError::Unavailable)
        );
    }
    fn held_tool_read(
        f: &mut Fixture,
        session: Option<String>,
        volume: Id,
    ) -> (Context, DataCall, Outcome) {
        let context = f
            .engine
            .tool_contexts
            .open(
                &f.engine.services.store,
                session.map_or(ToolScope::Account, ToolScope::Session),
                &f.peer,
            )
            .unwrap();
        let entry = f
            .engine
            .services
            .store
            .resolve_entry(&Access::owner(f.engine.services.store.account().id), volume, "data")
            .unwrap()
            .id;
        let call = DataCall {
            volume: *volume.as_bytes(),
            operation: DataOperation::SchemaRead { entry_id: *entry.as_bytes() },
        };
        let ticket = f.engine.begin_service(context, 1, 0, call.clone(), &f.peer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ticket.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let outcome = f.engine.service_executor.take(ticket).unwrap();
        assert_eq!(outcome.status, 0);
        (context, call, outcome)
    }
    #[test]
    fn tool_read_delivery_keeps_unrelated_commits_and_rejects_target_changes() {
        let mut f = Fixture::new();
        let root = f.admission.instance.root;
        let other = crate::engine::tests::object_volume(
            &f.engine.services.store,
            "Unrelated tool scope",
            false,
        )
        .id;
        let (context, call, mut outcome) = held_tool_read(&mut f, None, root);
        let access = Access::owner(f.admission.instance.account);
        let schema = crate::store::object_contract::Schema {
            types: BTreeMap::from([(
                Id::from_bytes([8; 16]),
                crate::store::object_contract::TypeDeclaration {
                    name: "Other".into(),
                    deprecated: false,
                    fields: Default::default(),
                },
            )]),
        };
        f.engine
            .services
            .store
            .establish_schema(&access, crate::engine::tests::object_address(other), &schema)
            .unwrap();
        assert_ne!(outcome.revision, f.engine.services.store.delivery_revision());
        f.engine.fence_service(&mut outcome);
        assert!(outcome.take(context, 1, 0, &call).is_ok());
        f.engine.tool_contexts.close(context, &f.peer).unwrap();
        let (context, call, mut outcome) = held_tool_read(&mut f, None, root);
        f.engine
            .services
            .store
            .establish_schema(&access, crate::engine::tests::object_address(root), &schema)
            .unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(outcome.take(context, 1, 0, &call), Err(HostError::Unavailable));
    }
    #[test]
    fn a_bound_session_root_remains_in_a_tool_reads_delivery_scope() {
        let mut f = Fixture::new();
        let session = f.admission.instance.session.clone();
        let volume = f.admission.instance.root;
        let (context, call, mut outcome) = held_tool_read(&mut f, Some(session), volume);
        f.engine
            .services
            .store
            .set_permissions(
                &Access::owner(f.admission.instance.account),
                f.admission.instance.root,
                crate::store::object_contract::Acl::default(),
            )
            .unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(outcome.take(context, 1, 0, &call), Err(HostError::Unavailable));
    }
    #[test]
    fn a_result_from_a_different_volume_fails_archive_and_owned_correlation() {
        let mut f = Fixture::new();
        let root = f.admission.instance.root;
        let (context, call, mut outcome) = held_tool_read(&mut f, None, root);
        outcome.volume = Id::from_bytes([99; 16]);
        let reply = Reply::Syscall(outcome);
        let bytes =
            crate::engine::ipc::codec::encode(&reply, crate::engine::ipc::codec::MAX_ARCHIVE)
                .unwrap();
        let archived =
            crate::engine::ipc::codec::access::<crate::engine::reply::ArchivedReply>(&bytes)
                .unwrap();
        assert!(!archived.matches_request(&Request::Syscall {
            context,
            operation: 1,
            mutation: 0,
            call: Box::new(call.clone())
        }));
        let Reply::Syscall(outcome) = reply else { panic!("syscall outcome") };
        assert_eq!(outcome.take(context, 1, 0, &call), Err(HostError::Internal));
    }
    #[test]
    fn accepted_tool_commit_survives_context_close_and_lost_delivery() {
        let mut f = Fixture::new();
        let volume = *f.admission.instance.root.as_bytes();
        let entry = f
            .engine
            .services
            .store
            .create_blob(
                &Access::owner(f.admission.instance.account),
                f.admission.instance.root,
                "file.bin",
                b"",
            )
            .unwrap()
            .id;
        let start = DataCall {
            volume,
            operation: DataOperation::FileTransferStart {
                entry_id: *entry.as_bytes(),
                total_bytes: 3,
            },
        };
        assert!(matches!(
            f.engine.begin_service(f.admission.context, 3, 3, start.clone(), &f.peer),
            Err(HostError::Unsupported)
        ));
        let context = f
            .engine
            .tool_contexts
            .open(&f.engine.services.store, ToolScope::Account, &f.peer)
            .unwrap();
        let execute = |f: &mut Fixture, order: u64, call: DataCall| {
            let ticket =
                f.engine.begin_service(context, order, order, call.clone(), &f.peer).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ticket.ready() {
                f.engine.maintain_service().unwrap();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            f.engine
                .service_executor
                .take(ticket)
                .unwrap()
                .take(context, order, order, &call)
                .unwrap()
        };
        let DataResult::FileTransfer { transfer } = execute(&mut f, 1, start) else {
            panic!("transfer")
        };
        execute(
            &mut f,
            2,
            DataCall {
                volume,
                operation: DataOperation::FileTransferChunk {
                    transfer,
                    offset: 0,
                    bytes: b"abc".to_vec(),
                },
            },
        );
        let call = DataCall { volume, operation: DataOperation::FileTransferCommit { transfer } };
        let ticket = f.engine.begin_service(context, 3, 3, call, &f.peer).unwrap();
        f.engine.tool_contexts.close(context, &f.peer).unwrap();
        drop(ticket); // Delivery is lost before the accepted write even starts.
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.engine.service_executor.pending() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            f.engine
                .services
                .store
                .read_blob_entry(
                    &Access::owner(f.admission.instance.account),
                    Id::from_bytes(volume),
                    entry
                )
                .unwrap(),
            b"abc"
        );
        assert!(!f.engine.tool_contexts.owns(context));
    }
    #[test]
    fn automatic_shared_first_use_is_scoped_and_its_reads_are_fenced() {
        use crate::package::AutomaticVolumeRole as Role;
        use crate::store::{BuildSelector, SessionCreation, volume_index::EntryAddress};
        let mut f = Fixture::automatic();
        assert!(f.admission.instance.extra_volumes.is_empty());
        let store = f.engine.services.store.clone();
        let session = store.session(&f.admission.instance.session).unwrap();
        let shared =
            store.automatic_volume_id(&session.app, Some(&session.id), Role::Session).unwrap();
        assert!(matches!(store.volume(shared), Err(crate::store::Error::NotFound(_))));
        let DataResult::EntryResolved(entry) =
            f.run_on(shared, DataOperation::ResolveEntry { name: "data".into() }).unwrap()
        else {
            panic!("lazy Shared entry")
        };
        assert_eq!(store.volume(shared).unwrap().name, "Shared");
        let DataResult::Created(object) = f
            .run_on(
                shared,
                DataOperation::Create {
                    entry_id: entry,
                    type_id: [1; 16],
                    fields: title("Created through automatic Shared"),
                },
            )
            .unwrap()
        else {
            panic!("created object")
        };
        let query = DataOperation::Query {
            entry_id: entry,
            type_id: [1; 16],
            after: None,
            limit: 64,
            projection: Some(vec![[2; 16]]),
        };
        let DataResult::Page(page) = f.run_on(shared, query.clone()).unwrap() else {
            panic!("Shared page")
        };
        assert!(page.objects.contains(&object));

        let other = store
            .create_session_binding(SessionCreation {
                app: session.app.clone(),
                selector: BuildSelector::Exact { build_id: session.resolved_build_id.clone() },
                resolved_build_id: session.resolved_build_id.clone(),
                root_volume: session.root_volume,
                user_volume: None,
                sync_shared: true,
            })
            .unwrap();
        let stranger =
            store.automatic_volume_id(&other.app, Some(&other.id), Role::Session).unwrap();
        let denied = DataCall {
            volume: *stranger.as_bytes(),
            operation: DataOperation::ResolveEntry { name: "data".into() },
        };
        assert!(matches!(
            f.engine.begin_service(f.admission.context, f.operation + 1, 0, denied, &f.peer),
            Err(HostError::AccessDenied)
        ));
        assert!(matches!(store.volume(stranger), Err(crate::store::Error::NotFound(_))));

        let (ticket, call) = f.begin_on(shared, query.clone());
        let mut outcome = f.retained(ticket);
        assert_eq!(outcome.status, 0);
        store
            .create_object(
                &Access::owner(store.account().id),
                EntryAddress {
                    volume: shared,
                    entry: crate::store::EntryId::from_bytes(entry).unwrap(),
                },
                Id::from_bytes([1; 16]),
            )
            .unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(
            outcome.take(f.admission.context, f.operation, 0, &call),
            Err(HostError::Unavailable)
        );

        let previous = crate::package::Package::open(
            &store.profile_path().join(crate::paths::BUILDS).join(&session.resolved_build_id),
        )
        .unwrap();
        let next = crate::package::signature::testing::signed(
            &store,
            crate::package::Package::create(
                previous.app_id().into(),
                "Updated automatic service".into(),
                previous.wasm().unwrap(),
                Default::default(),
                previous.initializers(),
            )
            .unwrap(),
        )
        .unwrap();
        let build = next.id().unwrap();
        std::fs::write(
            store.profile_path().join(crate::paths::BUILDS).join(&build),
            next.to_bytes().unwrap(),
        )
        .unwrap();
        let (ticket, call) = f.begin_on(shared, query);
        let mut outcome = f.retained(ticket);
        assert_eq!(outcome.status, 0);
        let before = store.delivery_revision();
        store.select_development_build(&session, &build).unwrap();
        assert_ne!(store.delivery_revision(), before);
        f.engine.fence_service(&mut outcome);
        assert_eq!(
            outcome.take(f.admission.context, f.operation, 0, &call),
            Err(HostError::Unavailable)
        );
        let selected = store.session(&session.id).unwrap();
        let before = store.delivery_revision();
        store.select_development_build(&selected, &build).unwrap();
        assert_eq!(store.delivery_revision(), before);
    }
    #[test]
    fn independent_service_results_preserve_admitted_mutation_order() {
        let mut f = Fixture::new();
        f.establish();
        let object_id = f.create();
        let (first, first_call) =
            f.begin(DataOperation::Edit { entry_id: [5; 15], object_id, fields: title("First") });
        let (second, second_call) =
            f.begin(DataOperation::Edit { entry_id: [5; 15], object_id, fields: title("Second") });
        assert_eq!(f.finish(second, &second_call), Ok(DataResult::Edited));
        assert_eq!(f.finish(first, &first_call), Ok(DataResult::Edited));
        let DataResult::Object(object) = f
            .run(DataOperation::Read { entry_id: [5; 15], object_id, fields: vec![[2; 16]] })
            .unwrap()
        else {
            panic!("read result")
        };
        assert_eq!(object.fields, title("Second"));
        let DataResult::Page(page) = f
            .run(DataOperation::Query {
                entry_id: [5; 15],
                type_id: [1; 16],
                after: None,
                limit: 64,
                projection: Some(vec![[2; 16]]),
            })
            .unwrap()
        else {
            panic!("page result")
        };
        assert_eq!(page.objects, vec![object_id]);
        assert_eq!(page.records.unwrap()[0].fields, title("Second"));
        assert!(!f.engine.service_executor.pending());
    }
    #[test]
    fn abandoned_delivery_commits_once_and_shutdown_retains_accepted_work() {
        let mut f = Fixture::new();
        f.establish();
        let (ticket, call) = f.begin(DataOperation::Create {
            entry_id: [5; 15],
            type_id: [1; 16],
            fields: title("Accepted"),
        });
        let operation = ticket.operation;
        let mutation = ticket.mutation;
        drop(ticket);
        assert!(f.engine.service_executor.pending());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f.engine.poll_shutdown().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(matches!(
            f.engine.begin_service(f.admission.context, operation, mutation, call, &f.peer),
            Err(HostError::StaleHandle)
        ));
        let page = f
            .engine
            .services
            .store
            .query_objects(
                &crate::store::Access::owner(f.engine.services.store.account().id),
                crate::engine::tests::object_address(f.admission.instance.root),
                crate::store::Id::from_bytes([1; 16]),
                None,
                8,
            )
            .unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(
            page.objects[0].fields[&crate::store::Id::from_bytes([2; 16])].values,
            Some(vec![crate::store::object_contract::Value::Text("Accepted".into())])
        );
    }
    #[test]
    fn rejected_mutation_has_no_partial_object_and_does_not_revoke_context() {
        let mut f = Fixture::new();
        f.establish();
        assert_eq!(
            f.run(DataOperation::Create {
                entry_id: [5; 15],
                type_id: [1; 16],
                fields: vec![FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![Value::Boolean(true)])
                }]
            }),
            Err(HostError::InvalidArgument)
        );
        let DataResult::Page(page) = f
            .run(DataOperation::Query {
                entry_id: [5; 15],
                type_id: [1; 16],
                after: None,
                limit: 8,
                projection: None,
            })
            .unwrap()
        else {
            panic!("page result")
        };
        assert!(page.objects.is_empty());
        f.create();
    }
    #[test]
    fn denied_extra_volume_is_contained_and_stale_context_is_rejected() {
        let mut f = Fixture::new();
        let call = DataCall {
            volume: [99; 16],
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        assert!(matches!(
            f.engine.begin_service(f.admission.context, 1, 0, call, &f.peer),
            Err(HostError::AccessDenied)
        ));
        f.operation = 1;
        f.establish();
        let mut stale = f.admission.context;
        stale.owner_generation += 1;
        let call = DataCall {
            volume: *f.admission.instance.root.as_bytes(),
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        assert!(matches!(
            f.engine.begin_service(stale, 3, 0, call, &f.peer),
            Err(HostError::StaleHandle)
        ));
        assert!(matches!(
            f.run(DataOperation::SchemaRead { entry_id: [5; 15] }),
            Ok(DataResult::Schema(_))
        ));
    }
    #[test]
    fn revocation_before_worker_admission_prevents_a_durable_creation() {
        let mut f = Fixture::new();
        f.establish();
        let (ticket, call) = f.begin(DataOperation::Create {
            entry_id: [5; 15],
            type_id: [1; 16],
            fields: title("Denied"),
        });
        f.engine
            .services
            .store
            .record_session_denial(
                &f.engine.services.store.session(&f.admission.instance.session).unwrap(),
                "Access was revoked",
            )
            .unwrap();
        assert_eq!(f.finish(ticket, &call), Err(HostError::AccessDenied));
        let page = f
            .engine
            .services
            .store
            .query_objects(
                &crate::store::Access::owner(f.engine.services.store.account().id),
                crate::engine::tests::object_address(f.admission.instance.root),
                crate::store::Id::from_bytes([1; 16]),
                None,
                64,
            )
            .unwrap();
        assert!(page.objects.is_empty());
    }
    #[test]
    fn retained_reads_and_identity_pages_recheck_the_databases_current_acl() {
        use crate::store::{Access, Id, object_contract::Acl};
        let mut f = Fixture::new();
        f.establish();
        let object_id = f.create();
        let root = f.admission.instance.root;
        let store = f.engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let reader = Id::from_bytes([88; 16]);
        store.grant_volume(&owner, root, reader, false).unwrap();
        let host = crate::engine::host::LocalHost {
            data: crate::engine::data_host::DataHost {
                store: store.clone(),
                access: Access::owner(reader),
                changed: std::sync::Arc::default(),
            },
            blobs: f.engine.network.blobs.clone(),
            session: f.admission.instance.session.clone(),
        };
        let object = f
            .run(DataOperation::Read { entry_id: [5; 15], object_id, fields: vec![[2; 16]] })
            .unwrap();
        let page = f
            .run(DataOperation::Query {
                entry_id: [5; 15],
                type_id: [1; 16],
                after: None,
                limit: 64,
                projection: None,
            })
            .unwrap();
        assert_eq!(check_result(&host, root, Some([5; 15]), &object), Ok(()));
        assert_eq!(check_result(&host, root, Some([5; 15]), &page), Ok(()));
        let before = store.delivery_revision().unwrap();
        store
            .set_entry_permissions(
                &owner,
                root,
                crate::store::EntryId::from_bytes([5; 15]).unwrap(),
                crate::store::object_contract::EntryAcl::Replace(Acl::default()),
            )
            .unwrap();
        assert_ne!(store.delivery_revision().unwrap(), before);
        // Catalog access remains while this database and its records are denied.
        assert!(store.admit_catalog_read(&Access::owner(reader), root).is_ok());
        assert_eq!(check_result(&host, root, Some([5; 15]), &object), Err(HostError::AccessDenied));
        assert_eq!(check_result(&host, root, Some([5; 15]), &page), Err(HostError::AccessDenied));
    }
    #[test]
    fn policy_change_after_worker_completion_discards_read_without_replaying_it() {
        let mut f = Fixture::new();
        let (ticket, call) = f.begin(DataOperation::SchemaRead { entry_id: [5; 15] });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ticket.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut outcome = f.engine.service_executor.take(ticket).unwrap();
        assert_eq!(outcome.status, 0);
        f.engine
            .services
            .store
            .record_session_denial(
                &f.engine.services.store.session(&f.admission.instance.session).unwrap(),
                "Removed",
            )
            .unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(outcome.take(f.admission.context, 1, 0, &call), Err(HostError::Unavailable));
        assert!(f.engine.session_instances.owns_context(f.admission.context));
        assert!(matches!(
            f.engine.begin_service(f.admission.context, 1, 0, call, &f.peer),
            Err(HostError::StaleHandle)
        ));
    }
    #[test]
    fn retired_guest_context_keeps_a_committed_receipt_and_discards_read_plaintext() {
        // Retire the logical owner after storage completes but before delivery.
        // Only the original correlated write receipt may cross that retirement.
        for mutation in [false, true] {
            let mut f = Fixture::new();
            f.establish();
            let operation = if mutation {
                DataOperation::Create {
                    entry_id: [5; 15],
                    type_id: [1; 16],
                    fields: title("Committed once"),
                }
            } else {
                DataOperation::SchemaRead { entry_id: [5; 15] }
            };
            let (ticket, call) = f.begin(operation);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ticket.ready() {
                f.engine.maintain_service().unwrap();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let mut outcome = f.engine.service_executor.take(ticket).unwrap();
            assert_eq!(outcome.status, 0);
            f.engine.session_instances.owner_exited(f.admission.instance.id).unwrap();
            f.engine.fence_service(&mut outcome);
            let result = outcome.take(f.admission.context, 2, u64::from(mutation) * 2, &call);
            if mutation {
                assert!(matches!(result, Ok(DataResult::Created(_))));
                let page = f
                    .engine
                    .services
                    .store
                    .query_objects(
                        &Access::owner(f.admission.instance.account),
                        crate::engine::tests::object_address(f.admission.instance.root),
                        Id::from_bytes([1; 16]),
                        None,
                        64,
                    )
                    .unwrap();
                assert_eq!(page.objects.len(), 1);
            } else {
                assert_eq!(result, Err(HostError::StaleHandle));
            }
            assert!(matches!(
                f.engine.begin_service(
                    f.admission.context,
                    3,
                    if mutation { 3 } else { 0 },
                    call,
                    &f.peer,
                ),
                Err(HostError::StaleHandle)
            ));
        }
    }
    #[test]
    fn closed_tool_context_preserves_its_exact_successful_schema_receipt() {
        // Closing a tool releases authority, not the outcome of accepted work.
        let mut f = Fixture::new();
        let context = f
            .engine
            .tool_contexts
            .open(&f.engine.services.store, ToolScope::Account, &f.peer)
            .unwrap();
        let call = DataCall {
            volume: *f.admission.instance.root.as_bytes(),
            operation: DataOperation::SchemaEstablish {
                entry_id: [5; 15],
                schema: Default::default(),
            },
        };
        let ticket = f.engine.begin_service(context, 1, 1, call.clone(), &f.peer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ticket.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut outcome = f.engine.service_executor.take(ticket).unwrap();
        f.engine.tool_contexts.close(context, &f.peer).unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(outcome.take(context, 1, 1, &call), Ok(DataResult::SchemaEstablished));
        assert!(matches!(
            f.engine.begin_service(context, 2, 2, call, &f.peer),
            Err(HostError::StaleHandle)
        ));
    }
    #[test]
    fn unrelated_volume_commit_after_worker_completion_preserves_the_read() {
        let mut f = Fixture::new();
        let other =
            crate::engine::tests::object_volume(&f.engine.services.store, "Unrelated read", false);
        let (ticket, call) = f.begin(DataOperation::SchemaRead { entry_id: [5; 15] });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ticket.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut outcome = f.engine.service_executor.take(ticket).unwrap();
        assert_eq!(outcome.status, 0);
        let owner = Access::owner(f.engine.services.store.account().id);
        f.engine
            .services
            .store
            .establish_schema(
                &owner,
                crate::engine::tests::object_address(other.id),
                &crate::store::object_contract::Schema {
                    types: BTreeMap::from([(
                        Id::from_bytes([91; 16]),
                        crate::store::object_contract::TypeDeclaration {
                            name: "Unrelated".into(),
                            deprecated: false,
                            fields: Default::default(),
                        },
                    )]),
                },
            )
            .unwrap();
        assert_ne!(outcome.revision, f.engine.services.store.delivery_revision());
        f.engine.fence_service(&mut outcome);
        assert!(outcome.take(f.admission.context, 1, 0, &call).is_ok());
    }

    #[test]
    fn retained_kv_cuts_survive_backing_writes_without_replaying_a_read() {
        // Hold each real worker result across selected and sibling content
        // writes. Socket delivery retains exactly its earlier cut and bytes.
        for keys in [false, true] {
            let mut f = Fixture::new();
            let address = f.kv_entry();
            let operation = if keys {
                DataOperation::KvKeys {
                    entry_id: *address.entry.as_bytes(),
                    after: None,
                    root: None,
                    limit: 64,
                }
            } else {
                DataOperation::KvRead {
                    entry_id: *address.entry.as_bytes(),
                    key: b"selected".to_vec(),
                    offset: 0,
                    limit: 64,
                    root: None,
                }
            };
            let (ticket, call) = f.begin(operation);
            let mut outcome = f.retained(ticket);
            assert_eq!(outcome.status, 0);
            let expected = outcome.result.clone().unwrap();
            let store = &f.engine.services.store;
            let owner = Access::owner(store.account().id);
            store.kv_put(&owner, address, b"other", b"independent key").unwrap();
            store.kv_put(&owner, address, b"selected", b"later content").unwrap();
            assert!(store.delivery_scope_revision(outcome.revision, &[address.volume]).1.is_some());
            assert_eq!(
                store.delivery_snapshot_revision(outcome.revision, &[address.volume]).1,
                None
            );
            f.engine.fence_service(&mut outcome);
            assert_eq!(outcome.take(f.admission.context, 1, 0, &call), Ok(expected));
            assert_eq!(f.operation, 1, "no replacement read was admitted");
        }
    }

    #[test]
    fn retained_kv_plaintext_is_retired_by_every_authority_boundary() {
        // A held successful chunk must not cross an ACL, retirement, negative
        // authority or bound-session denial, even though content cuts can survive.
        for boundary in 0..4 {
            let mut f = Fixture::new();
            let address = f.kv_entry();
            let (ticket, call) = f.begin(DataOperation::KvRead {
                entry_id: *address.entry.as_bytes(),
                key: b"selected".to_vec(),
                offset: 0,
                limit: 64,
                root: None,
            });
            let mut outcome = f.retained(ticket);
            assert_eq!(outcome.status, 0);
            let store = &f.engine.services.store;
            let owner = Access::owner(store.account().id);
            match boundary {
                0 => store
                    .set_entry_permissions(
                        &owner,
                        address.volume,
                        address.entry,
                        crate::store::object_contract::EntryAcl::Replace(Default::default()),
                    )
                    .unwrap(),
                1 => store.remove_entry(&owner, address.volume, address.entry).unwrap(),
                2 => store.invalidate_authority(&owner, address.volume).unwrap(),
                3 => {
                    store
                        .record_session_denial(
                            &store.session(&f.admission.instance.session).unwrap(),
                            "Removed",
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            f.engine.fence_service(&mut outcome);
            assert_eq!(outcome.take(f.admission.context, 1, 0, &call), Err(HostError::Unavailable));
        }
    }

    #[test]
    fn non_kv_reads_still_fence_same_volume_content_changes() {
        // This increment relaxes only root-bound KV reads, not other contracts.
        let mut f = Fixture::new();
        let address = f.kv_entry();
        let (ticket, call) = f.begin(DataOperation::SchemaRead { entry_id: [5; 15] });
        let mut outcome = f.retained(ticket);
        assert_eq!(outcome.status, 0);
        let store = &f.engine.services.store;
        store.kv_put(&Access::owner(store.account().id), address, b"other", b"changed").unwrap();
        f.engine.fence_service(&mut outcome);
        assert_eq!(outcome.take(f.admission.context, 1, 0, &call), Err(HostError::Unavailable));
    }
    #[test]
    fn context_count_saturation_is_explicit_and_abandoned_reads_release_credit() {
        let mut f = Fixture::new();
        let mut tickets = Vec::new();
        for _ in 0..64 {
            tickets.push(
                f.begin(DataOperation::Query {
                    entry_id: [5; 15],
                    type_id: [1; 16],
                    after: None,
                    limit: 1,
                    projection: None,
                })
                .0,
            );
        }
        let call = DataCall {
            volume: *f.admission.instance.root.as_bytes(),
            operation: DataOperation::Query {
                entry_id: [5; 15],
                type_id: [1; 16],
                after: None,
                limit: 1,
                projection: None,
            },
        };
        assert!(matches!(
            f.engine.begin_service(f.admission.context, 65, 0, call, &f.peer),
            Err(HostError::LimitExceeded)
        ));
        drop(tickets);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f.engine.poll_shutdown().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // The rejected operation ID is consumed; no automatic retry is admitted.
        f.operation = 65;
        assert!(matches!(
            f.run(DataOperation::SchemaRead { entry_id: [5; 15] }),
            Ok(DataResult::Schema(_))
        ));
    }
}
