//! Authenticated transport for engine-owned File resources. A guest selects a
//! retained launch mount by index; it never supplies a host root pathname.
use super::session_instance::Context;
use crate::runtime::host::{
    HostResult,
    files::{Call, Output},
};
use playbit_api::abi::syscall::Error;

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum Request {
    BundleRoot,
    HostRoot { mount: crate::store::Id },
    VolumeRoot { volume: crate::store::Id },
    Operation(Call),
    ListClose { list: u64 },
}
impl Request {
    pub fn kind(&self) -> u32 {
        use crate::runtime::host::files::Action;
        match self {
            Self::BundleRoot => 10,
            Self::HostRoot { .. } => 0,
            Self::VolumeRoot { .. } => 9,
            Self::Operation(call) => match call.action {
                Action::Open { .. } => 1,
                Action::Read { .. } => 2,
                Action::Write { .. } => 3,
                Action::Status => 4,
                Action::Close { .. } => 5,
                Action::ListOpen => 6,
                Action::ListNext { .. } => 7,
            },
            Self::ListClose { .. } => 8,
        }
    }
}
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Outcome {
    pub context: Context,
    pub operation: u64,
    pub kind: u32,
    pub status: i32,
    pub revision: Option<(crate::store::Id, u64)>,
    pub output: Option<Output>,
}
impl Outcome {
    pub fn result(
        context: Context,
        operation: u64,
        kind: u32,
        result: HostResult<Output>,
        revision: Option<(crate::store::Id, u64)>,
    ) -> Self {
        let (status, output) = match result {
            Ok(output) => (0, Some(output)),
            Err(error) => (error as i32, None),
        };
        Self { context, operation, kind, status, output, revision }
    }
    pub fn take(self, context: Context, operation: u64, request: &Request) -> HostResult<Output> {
        if self.context != context || self.operation != operation || self.kind != request.kind() {
            return Err(Error::Internal);
        }
        if self.status != 0 {
            if self.output.is_some() || self.status == Error::WouldBlock as i32 {
                return Err(Error::Internal);
            }
            return Err(Error::try_from(self.status).map_err(|_| Error::Internal)?);
        }
        let output = self.output.ok_or(Error::Internal)?;
        match request {
            Request::Operation(call) => call.validate_output(&output)?,
            Request::BundleRoot | Request::HostRoot { .. } | Request::VolumeRoot { .. } if !matches!(output, Output::File { handle } if handle != 0) =>
            {
                return Err(Error::Internal);
            }
            Request::ListClose { .. }
                if !matches!(
                    output,
                    Output::Close {
                        error: 0,
                        publication: crate::runtime::host::files::NOT_PUBLISHED,
                        recovery: None
                    }
                ) =>
            {
                return Err(Error::Internal);
            }
            _ => {}
        }
        Ok(output)
    }
}
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod native {
    use super::*;
    use crate::{
        engine::files::{
            authority::Authority, executor, host::PrivateObjects, private_storage::Managed,
        },
        store::Id,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        os::unix::net::UnixStream,
        sync::Arc,
    };
    #[derive(Default)]
    struct Sequence {
        highest: u64,
        seen: BTreeSet<u64>,
    }
    impl Sequence {
        fn admit(&mut self, operation: u64) -> HostResult<()> {
            // A bounded replay window permits transport reordering and gaps
            // from requests rejected before a context could acquire credit.
            // Forgotten IDs are too old to execute again; gaps never stall
            // every later operation after resource pressure has cleared.
            if operation == 0
                || self.highest.checked_sub(operation).is_some_and(|age| age >= 64)
                || self.seen.contains(&operation)
            {
                return Err(Error::StaleHandle);
            }
            self.highest = self.highest.max(operation);
            self.seen.retain(|prior| self.highest - prior < 64);
            self.seen.insert(operation);
            Ok(())
        }
    }
    enum Work {
        Operation(executor::Ticket),
        Retirement(executor::Retirement),
    }
    pub(crate) struct Ticket {
        pub context: Context,
        pub operation: u64,
        pub kind: u32,
        inner: Work,
    }
    impl Ticket {
        pub fn ready(&self) -> bool {
            match &self.inner {
                Work::Operation(ticket) => ticket.ready(),
                Work::Retirement(ticket) => ticket.ready(),
            }
        }
    }

    fn authorize_request(
        store: &crate::store::LocalStore,
        instance: &super::super::session_instance::SessionInstance,
        request: &Request,
        cleanup: bool,
        retained: Option<&crate::store::Session>,
    ) -> HostResult<()> {
        if !cleanup {
            let session = instance_session(store, instance)?;
            let draining = matches!(
                request,
                Request::Operation(Call {
                    action: crate::runtime::host::files::Action::Write { .. }
                        | crate::runtime::host::files::Action::Close { .. },
                    ..
                })
            );
            if session.resolved_build_id != instance.build && !draining {
                return Err(Error::StaleHandle);
            }
            check_grant(store, instance, &session, if draining { retained } else { None })?;
        }
        Ok(())
    }

    impl State {
        fn open_host(
            &mut self,
            context: Context,
            instance: &super::super::session_instance::SessionInstance,
            mount: Id,
        ) -> HostResult<Work> {
            let root = instance
                .host_mounts
                .iter()
                .find(|root| root.id == mount)
                .ok_or(Error::InvalidArgument)?;
            let authority = self
                .grants
                .entry((context.handle, GrantKey::Host(mount)))
                .or_insert_with(|| Arc::new(Authority::new(root.write)))
                .clone();
            Ok(Work::Operation(self.executor.root(
                context.handle,
                executor::Root {
                    path: root.path(),
                    private: self.private.clone(),
                    authority,
                    placement: executor::PlacementSource::Managed(self.placement.clone()),
                },
            )?))
        }

        fn open_volume(
            &mut self,
            store: &Arc<crate::store::LocalStore>,
            instance: &super::super::session_instance::SessionInstance,
            context: Context,
            volume: Id,
            root: (bool, Id, crate::engine::blob::BlobTransport),
        ) -> HostResult<Work> {
            let (write, epoch, transport) = root;
            let authority = self
                .grants
                .entry((context.handle, GrantKey::Volume(volume, write, epoch)))
                .and_modify(|authority| {
                    if authority.check(false).is_err() {
                        // A newly admitted root has fresh authority;
                        // retained objects still hold the revoked Arc.
                        *authority = Arc::new(Authority::new(write));
                    }
                })
                .or_insert_with(|| Arc::new(Authority::new(write)))
                .clone();
            Ok(Work::Operation(
                self.executor.volume_root(
                    context.handle,
                    executor::VolumeRoot {
                        store: store.clone(),
                        access: crate::store::Access::session_version(
                            instance.account,
                            &instance.session,
                            &instance.build,
                        )
                        .with_personal_incarnation(volume, epoch),
                        volume,
                        session: Some(instance.session.clone()),
                        transport,
                        authority,
                        placement: executor::PlacementSource::Managed(self.placement.clone()),
                    },
                )?,
            ))
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum GrantKey {
        Host(Id),
        Bundle,
        Volume(Id, bool, Id),
    }
    struct Binding {
        instance: super::super::session_instance::SessionInstance,
        session: crate::store::Session,
    }
    pub(crate) struct State {
        executor: executor::Executor,
        private: Arc<PrivateObjects>,
        placement: Arc<Managed>,
        sequences: BTreeMap<Id, Sequence>,
        bundles: BTreeMap<Id, Arc<executor::Bundle>>,
        grants: BTreeMap<(Id, GrantKey), Arc<Authority>>,
        bindings: BTreeMap<Id, Binding>,
        checked_revision: Option<(Id, u64)>,
        #[cfg(test)]
        pub(crate) volume_grant_checks: usize,
    }
    impl State {
        fn new(
            wake: Arc<super::super::completion::Wake>,
            profile: std::path::PathBuf,
        ) -> std::io::Result<Self> {
            let private = Arc::new(PrivateObjects::default());
            super::super::compiled_cache::protect(&profile, &private)?;
            let placement = Arc::new(Managed::new(profile, private.clone()));
            Ok(Self {
                executor: executor::Executor::new(move || wake.notify())?,
                private,
                placement,
                sequences: BTreeMap::new(),
                bundles: BTreeMap::new(),
                grants: BTreeMap::new(),
                bindings: BTreeMap::new(),
                checked_revision: None,
                #[cfg(test)]
                volume_grant_checks: 0,
            })
        }
        pub fn reserve_reconstruction(&mut self, prior: Context, next: Id) -> HostResult<bool> {
            if !self.sequences.contains_key(&prior.handle) {
                return Ok(false);
            }
            if self.sequences.contains_key(&next) {
                return Err(Error::Conflict);
            }
            assert!(
                self.sequences.len() <= 32,
                "one temporary successor is serialized with ownership commit"
            );
            self.executor.reserve_context(next)?;
            self.sequences.insert(next, Sequence::default());
            Ok(true)
        }
        pub fn forget_context(&mut self, owner: Id) {
            self.executor.retire(owner);
            self.sequences.remove(&owner);
            self.bundles.remove(&owner);
            self.bindings.remove(&owner);
            self.grants.retain(|(handle, _), _| *handle != owner);
        }
        pub fn prepare_reconstruction(&mut self, context: Context) -> HostResult<bool> {
            if self.executor.owner_pending(context.handle) {
                return Ok(false);
            }
            self.executor.retire(context.handle);
            self.executor.maintain()?;
            Ok(!self.executor.owner_retained(context.handle))
        }
        pub fn pending(&self) -> bool {
            self.executor.pending()
        }
        pub fn take(&mut self, ticket: &Ticket, revision: Option<(Id, u64)>) -> Option<Outcome> {
            let result = match &ticket.inner {
                Work::Operation(ticket) => self.executor.take(ticket),
                Work::Retirement(ticket) if ticket.ready() => Ok(Output::Close {
                    error: 0,
                    publication: crate::runtime::host::files::NOT_PUBLISHED,
                    recovery: None,
                }),
                Work::Retirement(_) => Err(Error::WouldBlock),
            };
            if matches!(result, Err(Error::WouldBlock)) {
                return None;
            }
            Some(Outcome::result(ticket.context, ticket.operation, ticket.kind, result, revision))
        }
        pub fn maintain(
            &mut self,
            registry: &super::super::session_instance::registry::Registry,
        ) -> HostResult<()> {
            let retired = self
                .sequences
                .keys()
                .filter(|handle| !registry.owns_handle(**handle))
                .copied()
                .collect::<Vec<_>>();
            for handle in retired {
                self.executor.retire(handle);
                // Retirement suppresses delivery, but original authority still
                // governs accepted writes/closes. Keep watching explicit policy
                // revocation until that work has actually finished.
                if !self.executor.owner_pending(handle) {
                    self.sequences.remove(&handle);
                    self.bundles.remove(&handle);
                    self.bindings.remove(&handle);
                    self.grants.retain(|(owner, _), _| *owner != handle);
                }
            }
            self.executor.maintain()?;
            self.grants.retain(|_, authority| {
                authority.check(false).is_ok() || Arc::strong_count(authority) > 1
            });
            Ok(())
        }
    }
    impl super::super::Engine {
        pub(crate) fn begin_file(
            &mut self,
            context: Context,
            operation: u64,
            request: Request,
            peer: &UnixStream,
        ) -> HostResult<Ticket> {
            let instance =
                self.session_instances.authorize(context, peer).map_err(|_| Error::StaleHandle)?;
            self.maintain_files()?;
            let cleanup = matches!(request, Request::ListClose { .. })
                || matches!(&request, Request::Operation(Call {
                    action: crate::runtime::host::files::Action::Close { flags }, ..
                }) if *flags == playbit_api::abi::files::FILE_CLOSE_DISCARD);
            let retained = self
                .file_service
                .as_ref()
                .and_then(|files| files.bindings.get(&context.handle))
                .map(|binding| &binding.session);
            authorize_request(&self.services.store, &instance, &request, cleanup, retained)?;
            if self.file_service.is_none() {
                if cleanup {
                    return Err(Error::StaleHandle);
                }
                self.file_service = Some(
                    State::new(self.completion.wake.clone(), self.services.path.clone())
                        .map_err(|_| Error::Unavailable)?,
                );
            }
            let volume_root = if let Request::VolumeRoot { volume } = &request {
                let roots = self.instance_volumes(context).map_err(|_| Error::AccessDenied)?;
                let root = roots
                    .iter()
                    .find(|root| {
                        root.id == *volume.as_bytes()
                            && root.backing == playbit_api::abi::data::VOLUME_PLAYBIT
                    })
                    .ok_or(Error::AccessDenied)?;
                Some((
                    root.rights.contains(crate::runtime::host::VolumeRights::WRITE),
                    Id::from_bytes(root.source_epoch),
                    self.network.blobs.clone(),
                ))
            } else {
                None
            };
            let state = self.file_service.as_mut().expect("file service initialized above");
            state.maintain(&self.session_instances)?;
            if !state.sequences.contains_key(&context.handle) && state.sequences.len() == 32 {
                return Err(Error::LimitExceeded);
            }
            state.sequences.entry(context.handle).or_default().admit(operation)?;
            if let std::collections::btree_map::Entry::Vacant(entry) =
                state.bindings.entry(context.handle)
            {
                let session = instance_session(&self.services.store, &instance)?;
                entry.insert(Binding { instance: instance.clone(), session });
            }
            let kind = request.kind();
            let inner = match request {
                Request::BundleRoot => {
                    let bundle = state
                        .bundles
                        .entry(context.handle)
                        .or_insert_with(|| {
                            Arc::new(executor::Bundle::new(
                                self.services.path.join(crate::paths::BUILDS).join(&instance.build),
                                instance.build.clone(),
                            ))
                        })
                        .clone();
                    let authority = state
                        .grants
                        .entry((context.handle, GrantKey::Bundle))
                        .or_insert_with(|| Arc::new(Authority::new(false)))
                        .clone();
                    Work::Operation(state.executor.bundle_root(
                        context.handle,
                        bundle,
                        authority,
                    )?)
                }
                Request::HostRoot { mount } => state.open_host(context, &instance, mount)?,
                Request::VolumeRoot { volume } => state.open_volume(
                    &self.services.store,
                    &instance,
                    context,
                    volume,
                    volume_root.expect("volume root was authorized"),
                )?,
                Request::Operation(call) if matches!(call.action, crate::runtime::host::files::Action::Close { flags } if flags == playbit_api::abi::files::FILE_CLOSE_DISCARD) => {
                    Work::Retirement(state.executor.discard(context.handle, call.file)?)
                }
                Request::Operation(call) => {
                    Work::Operation(state.executor.begin(context.handle, call)?)
                }
                Request::ListClose { list } => {
                    Work::Retirement(state.executor.close_list(context.handle, list)?)
                }
            };
            Ok(Ticket { context, operation, kind, inner })
        }
        /// Revalidate after observed authority changes. Object database and
        /// bloblog payload edits cannot change binding authority; index,
        /// revocation and local policy changes still require a fresh check.
        pub(crate) fn maintain_files(&mut self) -> HostResult<()> {
            let Some(files) = self.file_service.as_ref() else { return Ok(()) };
            let revision = self.services.store.delivery_revision();
            let checks = if revision.is_none() || revision != files.checked_revision {
                files
                    .bindings
                    .iter()
                    .map(|(handle, binding)| {
                        let scope = match super::super::service_call::instance_delivery_volumes(
                            &self.services.store,
                            &binding.instance,
                        ) {
                            Ok(scope) => scope,
                            Err(_) => return (*handle, Err(Error::AccessDenied)),
                        };
                        let (scope_revision, changed) = self
                            .services
                            .store
                            .delivery_snapshot_revision(files.checked_revision, &scope);
                        if scope_revision.is_some() && changed.is_none() {
                            return (*handle, Ok(()));
                        }
                        let result = scope_revision
                            .ok_or(Error::Unavailable)
                            .and_then(|_| instance_session(&self.services.store, &binding.instance))
                            .and_then(|session| {
                                check_grant(
                                    &self.services.store,
                                    &binding.instance,
                                    &session,
                                    Some(&binding.session),
                                )
                            });
                        (*handle, result)
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let files = self.file_service.as_mut().expect("checked file service");
            for (owner, result) in checks {
                if result.is_err() {
                    // Explicit authority loss differs from a build reload or
                    // owner exit. Accepted closes retain truthful outcomes.
                    for ((handle, _), grant) in &files.grants {
                        if *handle == owner {
                            grant.revoke().map_err(|_| Error::Internal)?;
                        }
                    }
                }
            }
            if revision.is_none() || revision != files.checked_revision {
                for ((owner, key), authority) in &files.grants {
                    let GrantKey::Volume(volume, write, epoch) = key else { continue };
                    let binding = files.bindings.get(owner).ok_or(Error::Internal)?;
                    // The owner pass already validates this exact root binding
                    // and revokes every owner grant on failure. Other volumes
                    // have additional dependencies and need their own check.
                    if *volume == binding.instance.root
                        && !self
                            .services
                            .store
                            .has_automatic_scopes(&binding.instance.session)
                            .map_err(|_| Error::AccessDenied)?
                    {
                        continue;
                    }
                    // Unrelated commits and backing-content edits cannot change
                    // this grant. Missing history and global policy changes recheck;
                    // a closed feed fails closed even after a live outer read.
                    let replica = self
                        .services
                        .store
                        .session(&binding.instance.session)
                        .map_err(|_| Error::AccessDenied)?
                        .replica_volume();
                    let (scope_revision, changed) = self
                        .services
                        .store
                        .delivery_snapshot_revision(files.checked_revision, &[replica, *volume]);
                    if scope_revision.is_some() && changed.is_none() {
                        continue;
                    }
                    #[cfg(test)]
                    {
                        files.volume_grant_checks += 1;
                    }
                    let result = scope_revision.ok_or(Error::Unavailable).and_then(|_| {
                        check_volume_grant(&self.services.store, binding, *volume, *write, *epoch)
                    });
                    if result.is_err() {
                        authority.revoke().map_err(|_| Error::Internal)?;
                    }
                }
            }
            files.checked_revision = revision;
            files.maintain(&self.session_instances)
        }
        pub(crate) fn reconcile_host_mounts(&mut self, instance: Id) -> HostResult<()> {
            let current = self.session_instances.descriptor(instance);
            let Some(files) = self.file_service.as_mut() else { return Ok(()) };
            for ((owner, key), authority) in &files.grants {
                let GrantKey::Host(mount) = key else { continue };
                if files.bindings.get(owner).is_some_and(|binding| binding.instance.id == instance)
                    && !current.as_ref().is_ok_and(|binding| {
                        binding.host_mounts.iter().any(|root| root.id == *mount)
                    })
                {
                    authority.revoke().map_err(|_| Error::Internal)?;
                }
            }
            Ok(())
        }
        pub(crate) fn file_pending(&self) -> bool {
            self.file_service.as_ref().is_some_and(State::pending)
        }
    }
    fn check_volume_grant(
        store: &crate::store::LocalStore,
        binding: &Binding,
        volume: Id,
        write: bool,
        epoch: Id,
    ) -> HostResult<()> {
        let instance = &binding.instance;
        let access = crate::store::Access::session_version(
            instance.account,
            &instance.session,
            &instance.build,
        );
        let current = instance_session(store, instance)?;
        let automatic = store
            .automatic_volume_addresses(&current)
            .map_err(|_| Error::AccessDenied)?
            .and_then(|addresses| addresses.into_iter().find(|address| address.volume == volume));
        if let Some(address) = automatic {
            store
                .admit_file_binding(&access, &binding.session, instance.root)
                .map_err(|_| Error::AccessDenied)?;
            let write_now = automatic_file_write(
                store,
                &access,
                &binding.session,
                address.role,
                volume,
                address.overridden,
            )?;
            if epoch != Id::from_bytes([0; 16]) || write_now != write {
                return Err(Error::AccessDenied);
            }
            // The File worker owns first materialization. Its pending root has
            // no Index yet; current root authority and the exact lazy address
            // qualify this grant until that worker checks the opened scope.
            match store.volume(volume) {
                Ok(_) => store
                    .admit_file_binding(&access, &binding.session, volume)
                    .map_err(|_| Error::AccessDenied)?,
                Err(crate::store::Error::NotFound(_)) => {}
                Err(_) => return Err(Error::AccessDenied),
            }
            return Ok(());
        }
        store
            .admit_file_binding(&access, &binding.session, volume)
            .map_err(|_| Error::AccessDenied)?;
        if volume != instance.root {
            let ceiling = instance
                .extra_volumes
                .iter()
                .find(|grant| grant.volume == volume)
                .ok_or(Error::AccessDenied)?;
            let current = store
                .personal_volume_selections(&instance.session)
                .map_err(|_| Error::AccessDenied)?
                .unwrap_or_default();
            if !current.iter().any(|grant| {
                grant.volume == volume
                    && grant.incarnation == epoch
                    && (grant.write && ceiling.write) == write
            }) {
                return Err(Error::AccessDenied);
            }
        }
        Ok(())
    }
    fn automatic_file_write(
        store: &crate::store::LocalStore,
        access: &crate::store::Access,
        retained: &crate::store::Session,
        role: crate::package::AutomaticVolumeRole,
        volume: Id,
        overridden: bool,
    ) -> HostResult<bool> {
        let account = store.account().id;
        if overridden {
            use crate::store::object_contract::{Permission, Principal};
            let policy = store
                .file_binding_policy(access, retained, volume)
                .map_err(|_| Error::AccessDenied)?;
            return Ok(policy.volume().owner == account
                || policy
                    .default_permissions()
                    .permits(Some(Principal::Human(account)), Permission::Write));
        }
        if role != crate::package::AutomaticVolumeRole::Root {
            return Ok(true);
        }
        match store.volume(volume) {
            Ok(volume) => Ok(volume.owner == account),
            Err(crate::store::Error::NotFound(_)) => Ok(retained.app.publisher == account),
            Err(_) => Err(Error::AccessDenied),
        }
    }
    fn check_grant(
        store: &crate::store::LocalStore,
        instance: &super::super::session_instance::SessionInstance,
        session: &crate::store::Session,
        retained: Option<&crate::store::Session>,
    ) -> HostResult<()> {
        if store.session_denial(&session.id).map_err(|_| Error::AccessDenied)?.is_some() {
            return Err(Error::AccessDenied);
        }
        // The immutable host mount belongs to the original admitted build.
        // Selecting another build stops guest turns, but cannot revoke accepted
        // writes/closes. Current participant/root authority still applies.
        let access =
            crate::store::Access::session_version(store.account().id, &session.id, &instance.build);
        match retained {
            Some(retained) => store.admit_file_binding(&access, retained, instance.root),
            None => store.admit_binding(&access, instance.root),
        }
        .map_err(|_| Error::AccessDenied)
    }
    fn instance_session(
        store: &crate::store::LocalStore,
        instance: &super::super::session_instance::SessionInstance,
    ) -> HostResult<crate::store::Session> {
        if instance.account != store.account().id {
            return Err(Error::AccessDenied);
        }
        let session = store.session(&instance.session).map_err(|_| Error::AccessDenied)?;
        if session.root_volume != instance.root {
            return Err(Error::AccessDenied);
        }
        Ok(session)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn replay_window_accepts_gaps_and_reordering_without_replaying_forgotten_ids() {
            let mut sequence = Sequence::default();
            assert_eq!(sequence.admit(0), Err(Error::StaleHandle));
            assert_eq!(sequence.admit(2), Ok(()));
            assert_eq!(sequence.admit(1), Ok(()));
            assert_eq!(sequence.admit(2), Err(Error::StaleHandle));
            assert_eq!(sequence.admit(130), Ok(()));
            assert_eq!(sequence.admit(67), Ok(()));
            assert_eq!(sequence.admit(66), Err(Error::StaleHandle));
            assert_eq!(sequence.admit(1), Err(Error::StaleHandle));
            assert_eq!(sequence.admit(u64::MAX), Ok(()));
            assert_eq!(sequence.admit(u64::MAX - 63), Ok(()));
            assert_eq!(sequence.admit(u64::MAX - 64), Err(Error::StaleHandle));
            assert_eq!(sequence.admit(u64::MAX), Err(Error::StaleHandle));
            assert!(sequence.seen.len() <= 64);
        }
    }
}
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(crate) use native::{State, Ticket};
