//! Shared account services, authoritative storage and replication.
//! Interactive execution belongs to the renderer process; this owner validates
//! packages and delivers bounded management/data completions.
use super::*;

pub(crate) type ServiceObserver = Arc<dyn Fn(&[replication::ReplicaNotice]) + Send + Sync>;

pub struct Engine {
    channel: crate::release_channel::ReleaseChannel,
    pub(super) gauges: super::gauges::Gauges,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(super) session_instances: session_instance::registry::Registry,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(super) purging_app: Option<crate::package::AppIdentity>,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(super) tool_contexts: tool_context::Contexts,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(super) service_executor: service_call::executor::Executor,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(super) file_service: Option<file_service::State>,
    pub(super) changes: ChangeToken,
    #[cfg(all(target_os = "macos", feature = "process"))]
    pub(super) provider_revision: Option<(Id, u64)>,
    // One coalesced notification batch; never a retained mutation history.
    pub services: Services,
    pub(super) runtime: Runtime,
    // Explicit in-process UI owners publish their bounded live session set.
    pub(crate) embedded_sessions: Vec<String>,
    pub remote: Option<remote::Remote>,
    pub(crate) network: remote::NetworkHandles,
    pub(crate) background: Option<background::Transfer>,
    pub(super) synchronization: Option<remote::Synchronization>,
    pub(super) invitation: Option<invitation::Invitation>,
    pub(crate) bootstrap_pending: bool,
    pub(super) pending_join: Option<String>,
    pub(super) network_request_waiter: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(super) network_response: Option<std::result::Result<reply::Reply, String>>,
    pub(crate) completion: Completion,
    pub(crate) transfer_result: reply::TransferStatus,
    pub(crate) availability_cache: String,
    pub(crate) account_ready_cache: bool,
    pub(crate) updates: BTreeMap<String, UpdateStatus>,
    pub(crate) service_observer: Option<ServiceObserver>,
}
impl Engine {
    /// Exercise the native hosts' closed-profile decision from real-AP integration tests.
    #[cfg(all(unix, feature = "test-support"))]
    pub fn resolve_replacement_for_test(path: &Path, replace: bool) -> Result<()> {
        super::sign_out::resolve_replacement(path, replace)
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_testing(path.as_ref(), None)
    }
    pub fn open_testing(
        path: impl AsRef<Path>,
        credentials: crate::network::ap::hello::TestingCredentials,
    ) -> Result<Self> {
        Self::open_with_testing(path.as_ref(), Some(credentials))
    }
    pub(super) fn open_with_testing(
        path: &Path,
        testing: Option<crate::network::ap::hello::TestingCredentials>,
    ) -> Result<Self> {
        Self::open_with_completion(path, testing, Completion::unix()?)
    }
    /// Open with completion delivery through the embedding host's event loop.
    /// The callback may run on any worker thread and must only schedule a later
    /// owner turn calling `complete_background`: no blocking, panics or reentry.
    /// Calls are coalesced until collection. Dropping this Engine disables the
    /// callback and waits for an in-flight callback before releasing its captures.
    /// Connected services still use their own internal networking/store adapters.
    pub fn open_with_wake(
        path: impl AsRef<Path>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_with_completion(path.as_ref(), None, Completion::host(wake))
    }
    fn open_with_completion(
        path: &Path,
        testing: Option<crate::network::ap::hello::TestingCredentials>,
        completion: Completion,
    ) -> Result<Self> {
        let _timing = crate::trace::Span::new("engine.open");
        let path = path.to_path_buf();
        #[cfg(target_os = "ios")]
        super::sign_out::resume(&path)?;
        let mut services = Services::open_unix(&path, testing.as_ref())?;
        let runtime = Runtime::with_backend(if cfg!(target_os = "ios") {
            ExecutionBackend::Pulley
        } else {
            ExecutionBackend::Native
        })?;
        let runtime = match super::compiled_cache::Cache::open(&path) {
            Ok(cache) => runtime.with_cache(Arc::new(cache)),
            Err(error) => {
                eprintln!("Compiled module cache unavailable: {error}");
                runtime
            }
        };
        let mut remote = remote::Remote::open(&path, completion.wake.clone())?;
        let store =
            Arc::get_mut(&mut services.store).expect("newly opened store is exclusively owned");
        remote.restore_profile_binding(store)?;
        let wake = completion.wake.clone();
        store.set_session_activity_wake(move || wake.notify());
        let store = &services.store;
        if let Some(testing) = testing {
            remote.configure_testing(store, testing)?;
        }
        remote.set_offline(store.work_offline()?);
        if remote.enrolled() && !remote.offline() {
            remote.begin_connection();
        }
        let network = remote.network_handles();
        let availability_cache = if remote.offline() {
            "Offline mode; local data is retained"
        } else if remote.enrolled() {
            "Waiting to synchronize"
        } else {
            "Offline; local data is retained"
        }
        .into();
        let changes = ChangeToken { generation: Id::generate()?, sequence: 0 };
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        let session_instances =
            session_instance::registry::Registry::open(&path, changes.generation)?;
        services.resume_app_data_removals();
        Ok(Self {
            channel: remote.channel(),
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            session_instances,
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            purging_app: None,
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            tool_contexts: tool_context::Contexts::new(changes.generation),
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            service_executor: Default::default(),
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            file_service: None,
            changes,
            #[cfg(all(target_os = "macos", feature = "process"))]
            provider_revision: services.store.delivery_provider_revision(None, None).0,
            bootstrap_pending: remote.enrolled() && !remote.offline(),
            pending_join: None,
            remote: Some(remote),
            network,
            background: None,
            synchronization: None,
            invitation: None,
            network_request_waiter: None,
            network_response: None,
            completion,
            transfer_result: reply::TransferStatus::default(),
            availability_cache,
            account_ready_cache: false,
            updates: BTreeMap::new(),
            service_observer: None,
            services,
            runtime,
            gauges: super::gauges::Gauges::new(),
            embedded_sessions: vec![],
        })
    }
    pub fn change_token(&self) -> ChangeToken {
        self.changes
    }
    pub const fn channel(&self) -> crate::release_channel::ReleaseChannel {
        self.channel
    }

    pub fn blob_transport(&self) -> blob::BlobTransport {
        self.network.blobs.clone()
    }

    pub(super) fn active_sessions(&self) -> Vec<String> {
        let mut sessions = self.embedded_sessions.clone();
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        sessions.extend(self.session_instances.active_sessions());
        sessions.sort_unstable();
        sessions.dedup();
        sessions
    }

    pub(crate) fn state_changed(&mut self) {
        self.changes.sequence =
            self.changes.sequence.checked_add(1).expect("local change sequence exhausted");
        crate::trace::mark("engine.state_changed");
        self.sample_gauges();
    }
    pub fn create_session(&mut self, package: Package, root: Id) -> Result<Session> {
        self.runtime.validate_guest(&package.wasm()?)?;
        self.services.create_session(package, root)
    }
    /// The shareable link of a retained session: its seed, freshly encoded.
    pub fn invitation(&self, id: &str) -> Result<String> {
        Ok(self.services.store.invitation(id, self.channel)?)
    }
    pub fn availability(&self) -> Result<String> {
        let _timing = crate::trace::Span::new("engine.availability");
        if self.network.connection_status.offline() {
            return Ok("Offline mode; local data is retained".into());
        }
        if self.background.as_ref().is_some_and(|transfer| transfer.diagnostic_read) {
            // A catalog query temporarily borrows Remote; it does not start
            // synchronization. Its completion checks returned connection health.
            return Ok(self.availability_cache.clone());
        }
        if self.background.is_some() {
            return Ok(if self.transfer_result.kind == reply::TransferKind::Canceling {
                "Canceling synchronization"
            } else {
                "Synchronizing…"
            }
            .into());
        }
        if self.bootstrap_pending {
            return Ok("Waiting to synchronize".into());
        }
        Ok(self
            .remote
            .as_ref()
            .map_or("Disconnected; local data is retained", |remote| {
                remote.availability(&self.services.store)
            })
            .into())
    }
    pub fn synchronize(&mut self) -> Result<()> {
        if self.background_pending() {
            return Err(
                "A background operation is in progress; synchronization did not start".into()
            );
        }
        self.bootstrap_pending = false;
        self.remote.as_mut().ok_or("A network operation is in progress")?.sync(&self.services.store)
    }
    pub fn join(&mut self, invitation: &str) -> Result<Session> {
        let seed = crate::store::parse_session_link(invitation, self.channel)?;
        let id = crate::store::session_id(&seed)?;
        let local_session = match self.services.store.session(&id) {
            Ok(session) => {
                if let Some(reason) = self.services.store.session_denial(&id)? {
                    return Err(reason.into());
                }
                let local = self.services.store.is_local_session(&id)?;
                if self.session_data_ready(&session, local)? {
                    self.services.store.admit_binding(
                        &Access::session_version(
                            self.services.store.account().id,
                            &id,
                            &session.resolved_build_id,
                        ),
                        self.session_admission_volume(&session)?,
                    )?;
                    self.services.store.retain_joined_session(&id)?;
                    return Ok(session);
                }
                local.then_some(session)
            }
            Err(crate::store::Error::NotFound("local record")) => None,
            Err(error) => return Err(error.into()),
        };
        if self.background_pending() {
            return Err("A background operation is in progress; join did not start".into());
        }
        let preview = self.services.store.preview_invitation(invitation, self.channel)?;
        let session = if let Some(local) = local_session {
            local
        } else {
            self.receive_pending_invitation(&preview.session)?;
            self.services.store.import_invitation(&preview.session)?
        };
        let resolution =
            self.remote.as_mut().ok_or("A network operation is in progress")?.bootstrap_current(
                &self.services.store,
                &session.id,
                &self.services.path.join(crate::paths::BUILDS),
            )?;
        let prepared = updates::prepare_candidate(
            &self.runtime,
            &self.services.store,
            &session.id,
            &self.services.path.join(crate::paths::BUILDS),
            resolution,
        )?;
        self.finish_update(&session.id, Ok(prepared));
        if let Some(update) = self.updates.get(&session.id) {
            return Err(update.message.clone().into());
        }
        let retained = self.services.store.session(&session.id)?;
        self.services.store.admit_binding(
            &Access::session_version(
                self.services.store.account().id,
                &retained.id,
                &retained.resolved_build_id,
            ),
            self.session_admission_volume(&retained)?,
        )?;
        self.services.store.complete_invitation(&session.id)?;
        self.stop_invitation_watch(&session.id);
        Ok(self.services.store.session(&session.id)?)
    }
    pub fn dispatch_reply(&mut self, request: Request) -> Result<reply::Reply> {
        let _timing = crate::trace::Span::new("engine.dispatch");
        // A failed mutating operation may already have persisted part of its
        // work. Invalidate after either outcome. Snapshot reads must not
        // unconditionally invalidate, or a watcher refreshing them would loop.
        let changes = !matches!(
            &request,
            Request::UiZoom { .. }
                | Request::BrowserDrafts { .. }
                | Request::LocalProtocol
                | Request::WaitChanges { .. }
                | Request::Status
                | Request::TransferStatus
                | Request::Invitation { .. }
                | Request::PersonalVolumes { .. }
                | Request::AppVolumes { .. }
                | Request::AppRemovalImpact { .. }
                | Request::AutomaticVolumes { .. }
                | Request::Entries { .. }
                | Request::DirectoryEntries { .. }
                | Request::VolumeDetails { .. }
                | Request::EntryDetails { .. }
                | Request::EntryRevisions { .. }
                | Request::ReadObject { .. }
                | Request::Objects { .. }
                | Request::RecoveryStatus
        );
        let result = self.dispatch_inner(request);
        if changes {
            self.state_changed();
        }
        self.sample_gauges();
        result
    }
    pub(super) fn dispatch_inner(&mut self, request: Request) -> Result<reply::Reply> {
        let access = Access::owner(self.services.store.account().id);
        Ok(match request {
            Request::WorkOffline { enabled } => {
                self.set_work_offline(enabled)?;
                reply::Reply::WorkOffline { enabled }
            }
            Request::UiZoom { root, scale } => {
                let scale = match scale {
                    Some(scale) => {
                        self.services.store.set_ui_zoom(root, scale)?;
                        scale
                    }
                    None => self.services.store.ui_zoom(root)?,
                };
                reply::Reply::UiZoom { root, scale }
            }
            Request::BrowserDrafts { account, operation } => reply::Reply::BrowserDrafts {
                account,
                result: self.services.store.browser_drafts(account, operation)?,
            },
            Request::FileCall { .. } => {
                return Err("File calls require authenticated IPC admission".into());
            }
            Request::WaitChanges { .. } => {
                return Err("change waits require local IPC admission".into());
            }
            #[rustfmt::skip]
            Request::Syscall { .. } | Request::ToolContextOpen { .. }
            | Request::ToolContextClose { .. } | Request::InstanceEvents { .. }
            | Request::InstanceInspect { .. } | Request::InstanceStopReason { .. } | Request::InstanceList { .. }
            | Request::InstanceConfigurations { .. } | Request::InstanceConfiguration { .. } | Request::InstanceRestore { .. } | Request::InstanceMounts { .. }
            | Request::InstanceLaunchFailed { .. } | Request::InstanceReserve { .. }
            | Request::InstanceChannelsAck { .. } | Request::InstanceChannelsClose { .. } | Request::InstanceStartup { .. } | Request::InstanceShow { .. } | Request::InstanceCompile { .. } | Request::InstanceCompiled { .. } | Request::InstanceClaim { .. } | Request::InstanceReconstruct { .. } => {
                return Err(
                    "Session-instance operations require authenticated process admission".into()
                );
            }
            #[rustfmt::skip]
            Request::RecoveryStatus
            | Request::PrepareRecovery | Request::SaveRecovery
            | Request::RecoverCredentials { .. } => {
                return Err("recovery operations require asynchronous owner admission".into());
            }
            #[rustfmt::skip]
            request @ (Request::StartSync | Request::StartJoin { .. } | Request::PrepareSession { .. }
            | Request::RetrySessionInfo { .. } | Request::TransferStatus | Request::CancelTransfer
            | Request::PersonalVolumes { .. } | Request::SelectPersonalVolumes { .. } | Request::AddPersonalVolumes { .. }
            | Request::CheckUpdates { .. } | Request::CacheBudget { .. }
            | Request::SetDisplayName { .. } | Request::AcceptAgreement { .. } | Request::Sync
            | Request::Disconnect) => self.dispatch_owner_control(request)?,
            #[rustfmt::skip]
            Request::AppRemovalImpact { .. } | Request::RemoveApp { .. } => {
                return Err("App removal requires asynchronous owner admission".into());
            }
            #[rustfmt::skip]
            Request::GrantVolume { .. }
            | Request::PublishApp { .. } | Request::ListApps { .. }
            | Request::SetAppEnabled { .. } | Request::DeleteApp { .. }
            | Request::GetAppDisplay { .. } | Request::CreateAppSession { .. }
            | Request::UpdateSession { .. } | Request::PrepareApp { .. } | Request::SignPackage { .. } | Request::AuthenticatePackage { .. } | Request::CreateSession { .. }
            | Request::CreateBuildSession { .. } => {
                return Err("developer operations require asynchronous owner admission".into());
            }
            #[rustfmt::skip]
            Request::DuplicateVolume { .. } | Request::DuplicateEntry { .. } | Request::Usage { .. } | Request::PayloadSize { .. } | Request::FileCreate { .. } | Request::FileCreateIn { .. } | Request::FileRead { .. }
            | Request::FileWrite { .. } | Request::RecoverEntry { .. } | Request::RejectedEntries { .. } => {
                return Err("diagnostic operations require asynchronous owner admission".into());
            }
            #[rustfmt::skip]
            Request::SavedAccount | Request::RetryAccountKeyBackup | Request::PrepareSignIn | Request::RequestCode { .. } | Request::SignIn { .. }
            | Request::SignInWithKey { .. } => {
                return Err("enrollment requires asynchronous owner admission".into());
            }
            #[rustfmt::skip]
            request @ (Request::Join { .. } | Request::PreviewInvitation { .. } | Request::PreviewDesktopInvitation { .. } | Request::DesktopLaunch | Request::RetainDesktopInvitation { .. }
            | Request::WatchInvitation { .. } | Request::ResumeInvitation { .. }
            | Request::PauseInvitation { .. } | Request::Provision { .. }
            | Request::Permissions { .. } | Request::Revoke { .. }) =>
                self.dispatch_owner_sharing(request, &access)?,
            #[rustfmt::skip]
            request @ (Request::LocalProtocol | Request::Status | Request::Entries { .. } | Request::DirectoryEntries { .. } | Request::VolumeDetails { .. }
            | Request::EntryDetails { .. } | Request::EntryRevisions { .. }
            | Request::CreateDatabase { .. } | Request::CreateDatabaseIn { .. } | Request::RenameEntry { .. } | Request::RenameEntryComponent { .. } | Request::KvRename { .. }
            | Request::RemoveEntry { .. } | Request::EntryPermissions { .. }
            | Request::DeleteVolume { .. } | Request::LabelVolume { .. } | Request::CreateVolume { .. }
            | Request::InitializeRoot { .. } | Request::InitializeApp { .. } | Request::AppVolumes { .. } | Request::AutomaticVolumes { .. } | Request::OpenAppVolume { .. } | Request::OpenAutomaticVolume { .. } | Request::SessionReference { .. } | Request::BloblogCreate { .. } | Request::BloblogCreateIn { .. }
            | Request::Schema { .. } | Request::CreateObject { .. }
            | Request::ReadObject { .. } | Request::EditObject { .. }
            | Request::RemoveObject { .. } | Request::Objects { .. }
            | Request::OrderSessions { .. } | Request::ForgetSession { .. }
            | Request::Invitation { .. }) => self.dispatch_local(request)?,
        })
    }

    fn dispatch_owner_control(&mut self, request: Request) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::StartSync => self.start_transfer(None)?,
            Request::StartJoin { session } => self.start_transfer(Some(session))?,
            Request::PrepareSession { session } => self.prepare_session(&session)?,
            Request::RetrySessionInfo { session } => {
                let retained = self.services.store.session(&session)?;
                self.network.builds.retry(&retained.resolved_build_id);
                Reply::Status(Box::new(self.status_snapshot()?))
            }
            Request::TransferStatus => Reply::Transfer { transfer: self.transfer_status() },
            Request::CancelTransfer => {
                self.cancel_transfer()?;
                Reply::Transfer { transfer: self.transfer_status() }
            }
            Request::PersonalVolumes { session: version } => Reply::PersonalVolumes {
                volumes: self.services.store.personal_volumes()?,
                grants: self.services.store.personal_volume_grants(&version)?,
                session: version,
            },
            Request::SelectPersonalVolumes { session: version, grants } => {
                self.select_personal_volumes(&version, grants)?;
                Reply::PersonalVolumesSelected { personal_volumes_selected: true }
            }
            Request::AddPersonalVolumes { session, grants } => {
                self.add_personal_volumes(&session, grants)?;
                Reply::PersonalVolumesSelected { personal_volumes_selected: true }
            }
            Request::CheckUpdates { session } => self.start_update_check(session)?,
            Request::CacheBudget { bytes } => {
                let retained_bytes = self.services.store.set_cache_budget(bytes)?;
                Reply::CacheBudget { retained_bytes }
            }
            Request::AcceptAgreement { account, digest } => {
                if !self.enrolled() {
                    return Err("Sign in before accepting the user agreement".into());
                }
                self.services.store.accept_agreement(account, digest)?;
                if !self.network.connection_status.offline()
                    && self.remote.as_ref().is_none_or(|remote| !remote.sync_connected())
                {
                    self.bootstrap_pending = true;
                }
                self.network.report_acceptance(self.services.store.clone());
                Reply::AgreementAccepted
            }
            Request::SetDisplayName { name } => {
                if self.services.store.display_name()?.is_none()
                    && !self.network.account_initialized()
                {
                    return Err("Account data is still loading; the name was not changed".into());
                }
                if self.enrolled() && self.services.store.agreement_required()? {
                    return Err("Accept the user agreement before choosing a display name".into());
                }
                self.services.store.set_display_name(&name)?;
                self.network.report_onboarding(self.services.store.clone());
                Reply::DisplayName { display_name: name }
            }
            Request::Sync => {
                self.synchronize()?;
                self.availability_cache = self.availability()?;
                Reply::Availability { availability: self.availability_cache.clone() }
            }
            Request::Disconnect => {
                self.disconnect_network()?;
                Reply::Disconnected { disconnected: true }
            }
            _ => unreachable!("owner dispatch routes only control operations here"),
        })
    }

    fn dispatch_owner_sharing(
        &mut self,
        request: Request,
        access: &Access,
    ) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::Join { invitation } => Reply::Session(self.join(&invitation)?),
            Request::DesktopLaunch => {
                let invitations = self.desktop_launch_invitations()?;
                Reply::Status(Box::new(self.status_with_invitations(invitations)?))
            }
            Request::PreviewDesktopInvitation { invitation } => {
                Reply::InvitationPreview(self.preview_desktop_invitation(&invitation)?)
            }
            Request::RetainDesktopInvitation { session } => {
                self.retain_desktop_invitation(&session)?
            }
            Request::PreviewInvitation { invitation } => Reply::InvitationPreview(
                self.services.store.preview_invitation(&invitation, self.channel)?,
            ),
            Request::WatchInvitation { session } => self.watch_invitation(session, false)?,
            Request::ResumeInvitation { session } => self.watch_invitation(session, true)?,
            Request::PauseInvitation { session } => self.pause_invitation(&session)?,
            Request::Provision { volume, version } => {
                self.services.store.provision_version(access, volume, &version)?;
                Reply::Provisioned { provisioned: version }
            }
            Request::Permissions { volume, acl } => {
                self.services.store.set_permissions(access, volume, acl)?;
                Reply::Permissions { permissions_updated: true }
            }
            Request::Revoke { volume, account, reason } => {
                self.services.store.revoke(access, volume, account, &reason)?;
                Reply::Revoked { revoked: account }
            }
            _ => unreachable!("owner dispatch routes only sharing operations here"),
        })
    }
    /// Local data and app operations, after request routing selects this owner.
    pub(super) fn status_snapshot(&mut self) -> Result<reply::Status> {
        self.status_with_invitations(self.services.store.pending_invitations()?)
    }
    fn status_with_invitations(
        &mut self,
        invitations: Vec<crate::store::InvitationPreview>,
    ) -> Result<reply::Status> {
        self.network.report_onboarding(self.services.store.clone());
        let sessions = self.services.store.retained_sessions()?;
        self.network.refresh_builds(self.services.store.clone(), self.completion.wake.clone())?;
        let volumes = self.services.store.volumes()?;
        let automatic_volume_ids = self.services.store.automatic_volume_ids(&volumes)?;
        let invitation_status = self.invitation_status();
        let desktop_invitation = match &invitation_status {
            Some(selected) if selected.state != reply::InvitationState::Paused => {
                self.services.store.resolved_desktop_invitation(&selected.session)?
            }
            _ => None,
        };
        let status = reply::Status {
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            startup: self.session_instances.startup_progress(),
            #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
            startup: Vec::new(),
            ap_connection: self.network.connection_status.snapshot(),
            ui_zoom: self.services.store.ui_zoom(None)?,
            account: self.services.store.account().clone(),
            account_key_backup: self.services.store.account_key_backup()?,
            volumes,
            automatic_volume_ids,
            examples: crate::examples::EXAMPLES
                .iter()
                .map(|example| reply::ExampleInfo {
                    id: example.id.into(),
                    name: example.name.into(),
                    description: example.description.into(),
                    invitation: example.invitation.into(),
                    source: example.source.into(),
                })
                .collect(),
            account_profile: reply::AccountProfile {
                agreement_required: self.enrolled() && self.services.store.agreement_required()?,
                replacement_required: self.replacement_required(),
                display_name: self.services.store.display_name()?,
                unlocked: self
                    .services
                    .store
                    .recovery_state()?
                    .is_some_and(|state| state.acknowledged),
                initialized: self.network.account_initialized(),
            },
            session_references: self.services.store.session_references()?,
            imported_apps: self.services.store.imported_apps()?,
            session_activity: self.services.store.session_activity(&sessions)?,
            sessions,
            // Ready is published after the name commit. Observe it first so
            // a completion cannot overtake an older name snapshot.
            builds: self.network.builds.states(),
            app_names: self.services.store.session_app_names()?,
            recent_sessions: self.services.store.recent_sessions()?,
            updates: self.updates.clone(),
            invitations,
            desktop_invitation,
            enrolled: self.enrolled(),
            email: self.email(),
            invitation_status,
            recovery: recovery::local_status(&self.services.store)?,
            transfer: self.transfer_status(),
            pending: self.services.store.pending_operations()?,
            availability: self.availability()?,
        };
        // Worker health can change between completion collection and this read.
        // Track the value actually exposed so returning to the previous health
        // still invalidates observers on the next completion.
        if self.availability_cache != status.availability {
            self.availability_cache = status.availability.clone();
            self.state_changed();
        }
        Ok(status)
    }

    pub(super) fn dispatch_local(&mut self, request: Request) -> Result<reply::Reply> {
        use reply::Reply;
        let access = request.management_access(&self.services.store)?;
        Ok(match request {
            #[rustfmt::skip]
            Request::Syscall { .. } | Request::InstanceEvents { .. }
            | Request::InstanceInspect { .. } | Request::InstanceStopReason { .. } | Request::InstanceLaunchFailed { .. }
            | Request::InstanceReserve { .. } | Request::InstanceChannelsAck { .. } | Request::InstanceChannelsClose { .. } | Request::InstanceStartup { .. } | Request::InstanceShow { .. } | Request::InstanceCompile { .. } | Request::InstanceCompiled { .. } | Request::InstanceClaim { .. }
            | Request::InstanceConfigurations { .. } | Request::InstanceConfiguration { .. } | Request::InstanceRestore { .. } | Request::InstanceMounts { .. }
            | Request::InstanceReconstruct { .. } => {
                return Err(
                    "Session-instance operations require authenticated process admission".into()
                );
            }

            Request::LocalProtocol => {
                Reply::Protocol { protocol: ipc::LOCAL_PROTOCOL, version: crate::VERSION.into() }
            }

            Request::Status => Reply::Status(Box::new(self.status_snapshot()?)),
            Request::SessionReference { session } => Reply::SessionReference {
                reference: self.services.store.session_reference(&session)?,
            },
            Request::AppVolumes { app } => {
                Reply::AutomaticVolumes { slots: self.services.store.app_volumes(&app)? }
            }
            Request::AutomaticVolumes { session } => {
                Reply::AutomaticVolumes { slots: self.services.store.automatic_volumes(&session)? }
            }
            Request::OpenAppVolume { app, role } => {
                Reply::Volume(self.services.store.open_app_volume(&app, role)?)
            }
            Request::OpenAutomaticVolume { session, role } => {
                Reply::Volume(self.services.store.open_automatic_volume(&session, role)?)
            }
            Request::InitializeApp { app_id, name, declaration } => {
                Reply::Volume(super::developer::source_app_descriptor(
                    &self.services.store,
                    app_id,
                    &name,
                    &declaration,
                )?)
            }

            #[rustfmt::skip]
            request @ (Request::Entries { .. } | Request::DirectoryEntries { .. } | Request::VolumeDetails { .. } | Request::EntryDetails { .. }
            | Request::EntryRevisions { .. } | Request::CreateDatabase { .. } | Request::CreateDatabaseIn { .. }
            | Request::RenameEntry { .. } | Request::RenameEntryComponent { .. } | Request::KvRename { .. } | Request::RemoveEntry { .. }
            | Request::EntryPermissions { .. } | Request::BloblogCreate { .. } | Request::BloblogCreateIn { .. }) => {
                self.dispatch_local_catalog(request, &access)?
            },

            #[rustfmt::skip]
            request @ (Request::DeleteVolume { .. } | Request::LabelVolume { .. } | Request::CreateVolume { .. }
            | Request::InitializeRoot { .. }) => {
                self.dispatch_local_volumes(request, &access)?
            },

            #[rustfmt::skip]
            request @ (Request::Schema { .. } | Request::CreateObject { .. }
            | Request::ReadObject { .. } | Request::EditObject { .. }
            | Request::RemoveObject { .. } | Request::Objects { .. }) => {
                self.dispatch_local_objects(request, &access)?
            },

            #[rustfmt::skip]
            request @ (Request::OrderSessions { .. } | Request::ForgetSession { .. }
            | Request::Invitation { .. }) => {
                self.dispatch_local_sessions(request)?
            },

            _ => unreachable!("dispatch_inner routes only local operations here"),
        })
    }

    fn dispatch_local_catalog(
        &mut self,
        request: Request,
        access: &Access,
    ) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::Entries { volume, after, limit, .. } => Reply::Entries {
                volume,
                after,
                page: self.services.store.entries(access, volume, after, limit as usize)?,
            },
            Request::DirectoryEntries { volume, target, cut, after, limit, .. } => {
                Reply::DirectoryEntries {
                    volume,
                    target,
                    cut_requested: cut,
                    after,
                    listing: self.services.store.directory_entries(
                        access,
                        volume,
                        target,
                        cut,
                        after,
                        limit as usize,
                        crate::ui::collation::compare,
                    )?,
                }
            }
            Request::VolumeDetails { volume, .. } => Reply::VolumeDetails {
                details: self.services.store.volume_details(access, volume)?,
            },
            Request::EntryDetails { volume, entry, .. } => Reply::EntryDetails {
                volume,
                details: self.services.store.entry_details(access, volume, entry)?,
            },
            Request::EntryRevisions { address, after, limit, .. } => Reply::EntryRevisions {
                address,
                after,
                page: Box::new(self.services.store.entry_revisions(
                    access,
                    address,
                    after,
                    limit as usize,
                )?),
            },
            Request::CreateDatabase { volume, name, kind, .. } => Reply::VolumeEntry {
                volume,
                entry: self.services.store.create_database(access, volume, &name, *kind)?,
            },
            Request::CreateDatabaseIn { volume, parent, name, kind, .. } => Reply::VolumeEntry {
                volume,
                entry: self
                    .services
                    .store
                    .create_database_in(access, volume, parent, &name, *kind)?,
            },
            Request::RenameEntry { volume, entry, name, .. } => {
                self.services.store.rename_entry(access, volume, entry, &name)?;
                Reply::EntryChanged { volume, entry }
            }
            Request::RenameEntryComponent { volume, entry, name, .. } => {
                self.services.store.rename_entry_component(access, volume, entry, &name)?;
                Reply::EntryChanged { volume, entry }
            }
            Request::KvRename { mutation, .. } => {
                let KvRenameMutation { address, old_key, new_key, expected, value } = *mutation;
                let root = self
                    .services
                    .store
                    .kv_rename(access, address, &old_key, &new_key, expected, &value)?;
                Reply::KvRenamed {
                    receipt: reply::KvRenameReceipt { address, old_key, new_key, root },
                }
            }
            Request::RemoveEntry { volume, entry, .. } => {
                self.services.store.remove_entry(access, volume, entry)?;
                Reply::EntryChanged { volume, entry }
            }
            Request::EntryPermissions {
                volume, entry, permissions, retained_permissions, ..
            } => {
                self.services.store.set_entry_permissions_with_retained(
                    access,
                    volume,
                    entry,
                    permissions,
                    retained_permissions,
                )?;
                Reply::EntryPermissions { volume, entry }
            }
            Request::BloblogCreate { volume, name, .. } => {
                let access = if access.session_id().is_some() {
                    access.clone()
                } else {
                    bloblog::access(&self.services.store, volume)?
                };
                Reply::VolumeEntry {
                    volume,
                    entry: self.services.store.create_bloblog(&access, volume, &name)?,
                }
            }
            Request::BloblogCreateIn { volume, parent, name, .. } => {
                let access = if access.session_id().is_some() {
                    access.clone()
                } else {
                    bloblog::access(&self.services.store, volume)?
                };
                Reply::VolumeEntry {
                    volume,
                    entry: self.services.store.create_bloblog_in(&access, volume, parent, &name)?,
                }
            }
            _ => unreachable!("owner dispatch routes only catalog operations here"),
        })
    }

    fn dispatch_local_objects(
        &mut self,
        request: Request,
        access: &Access,
    ) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::Schema { database, schema, .. } => {
                self.services.store.establish_schema(access, database, &schema)?;
                Reply::Schema { database, ok: true }
            }
            Request::CreateObject { database, type_id, fields, .. } => {
                Reply::ObjectCreated(reply::ObjectResult {
                    database,
                    object: self
                        .services
                        .store
                        .create_object_with_fields(access, database, type_id, &fields)?,
                })
            }
            Request::ReadObject { database, object, .. } => Reply::Object(reply::ObjectResult {
                database,
                object: self.services.store.read_object(access, database, object)?,
            }),
            Request::EditObject { database, object, fields, .. } => {
                Reply::Object(reply::ObjectResult {
                    database,
                    object: self.services.store.edit_fields(access, database, object, &fields)?,
                })
            }
            Request::RemoveObject { database, object, .. } => {
                self.services.store.remove_object(access, database, object)?;
                Reply::ObjectRemoved { database, object }
            }
            Request::Objects { database, type_id, after, limit, .. } => {
                Reply::Objects(reply::ObjectPageResult {
                    database,
                    type_id,
                    after,
                    page: self.services.store.query_objects(
                        access,
                        database,
                        type_id,
                        after,
                        limit as usize,
                    )?,
                })
            }
            _ => unreachable!("owner dispatch routes only objects operations here"),
        })
    }

    fn dispatch_local_volumes(
        &mut self,
        request: Request,
        access: &Access,
    ) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::LabelVolume { volume, label } => {
                let renamed = self.services.store.label_volume(access, volume, &label)?;
                Reply::Volume(renamed)
            }
            Request::DeleteVolume { volume } => {
                self.services.store.delete_volume(volume)?;
                Reply::VolumeDeleted { volume }
            }
            Request::CreateVolume { name, share_key_with_service } => {
                Reply::Volume(self.services.store.create_volume(&name, share_key_with_service)?)
            }
            Request::InitializeRoot { volume, name, declaration } => Reply::Volume(
                self.services.store.initialize_root(volume, &name, true, &declaration)?,
            ),
            _ => unreachable!("owner dispatch routes only volumes operations here"),
        })
    }

    fn dispatch_local_sessions(&mut self, request: Request) -> Result<reply::Reply> {
        use reply::Reply;
        Ok(match request {
            Request::OrderSessions { sessions } => {
                self.services.store.order_sessions(&sessions)?;
                Reply::OrderSessions { ordered: true }
            }
            Request::ForgetSession { session } => {
                // Cancel only this session's pending open. No shared
                // capability, data or account record is removed.
                self.services.store.session(&session)?;
                self.cancel_session_open(&session)?;
                self.services.store.forget_session(&session)?;
                self.stop_invitation_watch(&session);
                self.updates.remove(&session);
                let active = self.active_sessions();
                if let Some(remote) = self.remote.as_mut() {
                    remote.retire_selection_watches(&active);
                }
                Reply::ForgetSession { forgotten: session }
            }
            Request::Invitation { session } => Reply::Invitation {
                invitation: self.invitation(&session)?,
                availability: self.availability()?,
            },
            _ => unreachable!("owner dispatch routes only sessions operations here"),
        })
    }
}

impl Request {
    /// Only the explicitly opened Shared scope supplies group membership.
    /// LocalStore checks the retained account, session and current policy on each operation.
    pub(crate) fn selected_shared_volume(&self) -> Option<(Id, &[u8; 32])> {
        let (volume, session) = match self {
            Self::Entries { volume, session, .. }
            | Self::DirectoryEntries { volume, session, .. }
            | Self::VolumeDetails { volume, session }
            | Self::EntryDetails { volume, session, .. }
            | Self::CreateDatabase { volume, session, .. }
            | Self::CreateDatabaseIn { volume, session, .. }
            | Self::BloblogCreate { volume, session, .. }
            | Self::BloblogCreateIn { volume, session, .. }
            | Self::RenameEntry { volume, session, .. }
            | Self::RenameEntryComponent { volume, session, .. }
            | Self::RemoveEntry { volume, session, .. }
            | Self::EntryPermissions { volume, session, .. }
            | Self::FileCreate { volume, session, .. }
            | Self::FileCreateIn { volume, session, .. }
            | Self::PayloadSize { volume, session, .. } => (*volume, session),
            Self::EntryRevisions { address, session, .. }
            | Self::Schema { database: address, session, .. }
            | Self::CreateObject { database: address, session, .. }
            | Self::ReadObject { database: address, session, .. }
            | Self::EditObject { database: address, session, .. }
            | Self::RemoveObject { database: address, session, .. }
            | Self::Objects { database: address, session, .. } => (address.volume, session),
            Self::KvRename { mutation, session } => (mutation.address.volume, session),
            _ => return None,
        };
        Some((volume, session.as_deref()?))
    }

    pub(super) fn management_access(&self, store: &LocalStore) -> Result<Access> {
        let Some((volume, id)) = self.selected_shared_volume() else {
            return Ok(Access::owner(store.account().id));
        };
        let session = store.session(&crate::identifier::encode(id))?;
        if session.shared_volume != Some(volume) {
            return Err("Selected session does not bind this Shared volume".into());
        }
        Ok(Access::session_version(store.account().id, session.id, session.resolved_build_id))
    }
}
