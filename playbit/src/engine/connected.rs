//! Account and replica services without an app executor or process transport.
//!
//! Network methods block: run this owner on a network thread, independently of
//! UI and local app turns. The wake callback schedules a later owner turn; it
//! must not block, panic or reenter this owner. Local apps share the store and
//! continue committing while this owner waits for remote work.
use super::{
    Result, Services,
    cancellation::Canceled,
    completion::Completion,
    recovery,
    remote::{NetworkHandles, Remote},
};
use crate::network::ap::{ApConnection, ClientConfig, hello::TestingCredentials};
use crate::store::Id;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub use super::replication::{ReplicaNotice, ReplicaSource, ReplicaState};

/// Permanent shutdown signal. It wakes a pending authentication/replication wait
/// and closes its current transport. Enrollment may already have issued durable
/// credentials: the host must collect the admitted call before dropping/joining.
#[derive(Clone)]
pub struct Stop {
    canceled: Arc<Canceled>,
    connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
}
impl Stop {
    pub fn request(&self) {
        self.canceled.raise();
        if let Some(connection) =
            self.connection.lock().expect("connection mutex is not poisoned").as_ref()
        {
            connection.shutdown();
        }
    }
}

/// The app owner drains replica invalidations independently of network calls.
/// Use exactly one consumer: draining transfers the existing bounded notices.
/// The Connected wake callback must schedule both this consumer and the network
/// owner. Retire the app owner before dropping Connected.
pub struct Notifications {
    received: Arc<super::replication::ReplicaNotices>,
    wake: Arc<super::completion::Wake>,
}
impl Notifications {
    pub fn drain(&self) -> Vec<ReplicaNotice> {
        // Rearm before reading: a receive racing the drain schedules another
        // turn even while Connected is blocked in a separate network operation.
        self.wake.rearm_host();
        self.received.drain()
    }
}

/// A drain can acknowledge a pre-commit marker only if no local mutation
/// overlaps its source capture. Epochs detect calls which finish during a drain.
#[cfg(feature = "file-provider")]
#[derive(Default)]
pub(crate) struct ProviderMutations {
    active: std::sync::atomic::AtomicUsize,
    epoch: std::sync::atomic::AtomicU64,
}
#[cfg(feature = "file-provider")]
impl ProviderMutations {
    pub(crate) fn begin(&self) -> ProviderMutation<'_> {
        self.active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.advance();
        ProviderMutation(self)
    }
    fn advance(&self) {
        let before = self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(before < u64::MAX, "provider mutation epochs cannot wrap");
    }
    pub(crate) fn idle_epoch(&self) -> Option<u64> {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::SeqCst);
        (self.active.load(std::sync::atomic::Ordering::SeqCst) == 0).then_some(epoch)
    }
    pub(crate) fn unchanged(&self, before: Option<u64>) -> bool {
        before.is_some() && self.idle_epoch() == before
    }
}
#[cfg(feature = "file-provider")]
pub(crate) struct ProviderMutation<'a>(&'a ProviderMutations);
#[cfg(feature = "file-provider")]
impl Drop for ProviderMutation<'_> {
    fn drop(&mut self) {
        self.0.advance();
        let before = self.0.active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        assert!(before > 0, "a provider mutation owns its admission count");
    }
}

pub struct Connected {
    /// Local operations remain independent of remote I/O. The AP publisher owns
    /// this store's sole committed-change receiver; hosts observe network notices
    /// and their own local commits instead of taking that receiver.
    pub services: Services,
    remote: Remote,
    network: NetworkHandles,
    stop: Stop,
    completion: Completion,
    #[cfg(feature = "file-provider")]
    pub(crate) provider_mutations: Arc<ProviderMutations>,
}
impl Connected {
    pub fn channel(&self) -> crate::release_channel::ReleaseChannel {
        self.remote.channel()
    }
    /// Open local state and the network owner without making a network request.
    /// AP completion uses the host callback. The existing publisher uses an
    /// internal Unix socket pair for store readiness; no IPC listener is opened.
    pub fn open(path: impl AsRef<Path>, wake: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::open_inner(path.as_ref(), None, wake)
    }
    pub fn open_testing(
        path: impl AsRef<Path>,
        credentials: TestingCredentials,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_inner(path.as_ref(), Some(credentials), wake)
    }
    fn open_inner(
        path: &Path,
        testing: Option<TestingCredentials>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        #[cfg(unix)]
        super::sign_out::resume(path)?;
        let completion = Completion::host(wake);
        let mut services = Services::open_unix(path, testing.as_ref())?;
        let mut remote = Remote::open(path, completion.wake.clone())?;
        remote.restore_profile_binding(
            Arc::get_mut(&mut services.store).expect("newly opened store is exclusively owned"),
        )?;
        if let Some(testing) = testing {
            remote.configure_testing(&services.store, testing)?;
        }
        remote.set_offline(services.store.work_offline()?);
        let stop =
            Stop { canceled: Arc::new(Canceled::default()), connection: remote.cancel_handle() };
        remote.set_cancellation(Some(stop.canceled.clone()));
        let network = remote.network_handles();
        services.resume_app_data_removals();
        Ok(Self {
            services,
            remote,
            network,
            stop,
            completion,
            #[cfg(feature = "file-provider")]
            provider_mutations: Default::default(),
        })
    }
    pub fn notifications(&self) -> Notifications {
        Notifications {
            received: self.network.received.clone(),
            wake: self.completion.wake.clone(),
        }
    }
    pub fn stop_handle(&self) -> Stop {
        self.stop.clone()
    }
    fn active(&self) -> Result<()> {
        if self.stop.canceled.raised() { Err("connected engine is stopped".into()) } else { Ok(()) }
    }
    /// Explicit transport injection for private integration tests.
    pub fn set_test_transport(&mut self, config: ClientConfig) -> Result<()> {
        self.active()?;
        self.remote.set_test_transport(config);
        Ok(())
    }
    pub fn enrolled(&self) -> bool {
        self.remote.enrolled()
    }
    pub fn email(&self) -> &str {
        self.remote.email()
    }
    pub fn prepare_sign_in(&mut self) -> Result<()> {
        self.active()?;
        self.remote.prepare_sign_in()
    }
    pub fn request_code(&mut self, email: &str) -> Result<()> {
        self.active()?;
        if !crate::network::ap::operation::valid_account_email(email) {
            return Err("invalid email address".into());
        }
        self.remote.request_code(&self.services.store, email)
    }
    /// Bind a fresh profile before an app retains the store. A failed backup
    /// read is reported separately and never undoes successful enrollment.
    pub fn sign_in(&mut self, code: &str) -> Result<super::reply::SignInReply> {
        self.active()?;
        if !crate::network::ap::registration::valid_email_code(code.as_bytes()) {
            return Err("enter the six-digit email sign-in code".into());
        }
        let account = self.remote.finish_code(&self.services.store, code)?;
        if self.remote.replacement_required() {
            return Ok(super::reply::SignInReply {
                replacement_required: true,
                signed_in: false,
                recovery: None,
                recovery_error: None,
            });
        }
        if account != self.services.store.account().id {
            self.services.bind_authenticated_account(account)?;
        }
        if let Some(recorded) = self.remote.sign_in_agreement() {
            self.services.store.set_agreement_required(!recorded)?;
        }
        let mut response = super::reply::SignInReply {
            replacement_required: false,
            signed_in: true,
            recovery: None,
            recovery_error: None,
        };
        match self.remote.credential_vault(&self.services.store).and_then(|mut vault| {
            recovery::stage_recovery_key(&self.services.store, account, &mut vault)?;
            recovery::after_sign_in(&self.services.store, vault)
        }) {
            Ok(state) => response.recovery = Some(state),
            Err(error) => response.recovery_error = Some(error.to_string()),
        }
        Ok(response)
    }
    /// Same key proof and durable profile binding as the process engine.
    pub fn sign_in_with_key(
        &mut self,
        email: &str,
        key: &str,
    ) -> Result<super::reply::SignInReply> {
        self.active()?;
        let email = email.trim().to_ascii_lowercase();
        if !crate::network::ap::operation::valid_account_email(&email) {
            return Err("invalid email address".into());
        }
        let secret = crate::store::account_key::parse(key)?;
        let account = self.remote.sign_in_with_key(&self.services.store, &email, &secret, None)?;
        if self.remote.replacement_required() {
            return Ok(super::reply::SignInReply {
                replacement_required: true,
                signed_in: false,
                recovery: None,
                recovery_error: None,
            });
        }
        if account != self.services.store.account().id {
            self.services.bind_authenticated_account(account)?;
        }
        if let Some(recorded) = self.remote.sign_in_agreement() {
            self.services.store.set_agreement_required(!recorded)?;
        }
        Ok(super::reply::SignInReply {
            replacement_required: false,
            signed_in: true,
            recovery: None,
            recovery_error: None,
        })
    }
    /// A provider is a distinct device replica authorized by the containing
    /// app's protected account handoff. Never adopt a different account returned
    /// by authentication, and finish binding before retaining local store owners.
    #[cfg(feature = "file-provider")]
    pub(crate) fn enroll_provider(
        &mut self,
        account: crate::store::Id,
        email: &str,
        secret: &[u8; 16],
    ) -> Result<()> {
        self.active()?;
        if !self.remote.scope.read().provider() {
            return Err("File Provider requires a provider replica".into());
        }
        if !self.enrolled() {
            let authenticated =
                self.remote.sign_in_with_key(&self.services.store, email, secret, Some(account))?;
            if self.remote.replacement_required() {
                return Err("File Provider profile belongs to another account".into());
            }
            self.services.bind_authenticated_account(authenticated)?;
        }
        if self.services.store.account().id != account || self.email() != email {
            return Err("File Provider account is no longer signed in".into());
        }
        if self.services.store.recovery_state()?.is_none() {
            self.restore_credentials(secret)?;
        }
        Ok(())
    }

    /// Explicit retry runs on the host's existing account worker.
    #[cfg(target_vendor = "apple")]
    pub fn retry_account_key_backup(&mut self) -> Result<crate::store::AccountKeyBackup> {
        self.active()?;
        self.remote.retry_account_key_backup(&self.services.store)?;
        Ok(self.services.store.account_key_backup()?)
    }
    #[cfg(target_vendor = "apple")]
    pub fn saved_account(&self) -> Result<Option<super::reply::SavedAccount>> {
        self.active()?;
        self.remote.saved_account(&self.services.store)
    }
    /// Initialize just the account stream. Session admission still initializes
    /// its own required volumes before allowing native callbacks.
    pub fn initialize_account(&mut self) -> Result<()> {
        self.active()?;
        self.remote.initialize_account(&self.services.store)?;
        if !self.remote.account_initialized() {
            return Err(
                "Account information is not ready. Finish saving the account key and retry.".into(),
            );
        }
        Ok(())
    }
    pub fn account_profile(&self) -> Result<super::reply::AccountProfile> {
        Ok(super::reply::AccountProfile {
            agreement_required: self.remote.enrolled()
                && self.services.store.agreement_required()?,
            replacement_required: self.remote.replacement_required(),
            display_name: self.services.store.display_name()?,
            unlocked: self.services.store.recovery_state()?.is_some_and(|s| s.acknowledged),
            initialized: self.remote.account_initialized(),
        })
    }
    pub fn accept_agreement(&self, account: Id, digest: [u8; 32]) -> Result<()> {
        self.active()?;
        if !self.remote.enrolled() {
            return Err("Sign in before accepting the user agreement".into());
        }
        self.services.store.accept_agreement(account, digest)?;
        self.network.report_acceptance(self.services.store.clone());
        Ok(())
    }
    pub fn set_display_name(&self, name: &str) -> Result<()> {
        self.active()?;
        if !self.account_initialized() && self.services.store.display_name()?.is_none() {
            return Err("Account data is still loading; the name was not changed".into());
        }
        if self.remote.enrolled() && self.services.store.agreement_required()? {
            return Err("Accept the user agreement before choosing a display name".into());
        }
        self.services.store.set_display_name(name)?;
        Ok(())
    }
    pub fn recovery_status(&mut self) -> Result<super::reply::RecoveryStatus> {
        self.active()?;
        recovery::status(&mut self.remote, &self.services.store)
    }
    pub fn prepare_recovery(&mut self) -> Result<super::reply::PreparedRecovery> {
        self.active()?;
        recovery::prepare(&mut self.remote, &self.services.store)
    }
    /// Explicitly acknowledge that the user saved the prepared account key.
    pub fn save_recovery(&mut self) -> Result<super::reply::RecoveryStatus> {
        self.active()?;
        recovery::save(&mut self.remote, &self.services.store)
    }
    pub fn restore_credentials(
        &mut self,
        secret: &[u8; 16],
    ) -> Result<super::reply::RecoveryStatus> {
        self.active()?;
        recovery::restore_credentials(
            &mut self.remote,
            &self.services.store,
            secret,
            Some(&self.stop.canceled),
        )
    }
    /// Retained package trust is local; first use admits the publisher through
    /// this owner's existing authenticated AP connection when needed.
    #[cfg(all(target_os = "macos", feature = "desktop-ui", feature = "native-runtime"))]
    pub(crate) fn authenticate_native_package(
        &mut self,
        package: crate::package::Package,
    ) -> Result<crate::package::Package> {
        self.active()?;
        self.remote.authenticate_public_package(&self.services.store, package)
    }
    /// Join the session implemented by a statically compiled native app. Native
    /// callbacks cannot follow a changed app publication or a different build.
    pub fn join_native(
        &mut self,
        invitation: &str,
        package: &crate::package::Package,
    ) -> Result<crate::store::Session> {
        use crate::store::{Access, BuildSelector};
        self.active()?;
        let build = package.id()?;
        let seed = crate::store::parse_session_link(invitation, self.channel())?;
        let local = crate::store::session_id(&seed)?;
        if self.services.store.is_local_session(&local)? {
            let package = self
                .services
                .store
                .authenticate_package(package.clone(), crate::store::now_unix_secs())?;
            let session = self.services.store.session(&local)?;
            if !matches!(&session.selector, BuildSelector::Exact { build_id } if build_id == &build)
                || session.resolved_build_id != build
                || &session.app != package.identity()?
            {
                return Err("This session does not use this exact native app build".into());
            }
            self.services.store.admit_binding(
                &Access::session_version(self.services.store.account().id, &local, &build),
                session.root_volume,
            )?;
            self.services.store.retain_joined_session(&session.id)?;
            return Ok(session);
        }
        let preview = self.services.store.preview_invitation(invitation, self.channel())?;
        if preview.received.is_none() {
            let envelope = self.remote.read_invitation(&self.services.store, &preview.session)?;
            self.services.store.receive_invitation(&preview.session, &envelope)?;
        }
        let session = self.services.store.import_invitation(&preview.session)?;
        let resolution = self.remote.resolve_session(&self.services.store, &session.id)?;
        let selector: BuildSelector = serde_json::from_value(resolution["selector"].clone())?;
        if !matches!(selector, BuildSelector::Exact { ref build_id } if build_id == &build)
            || resolution["resolved_build_id"].as_str() != Some(&build)
            || session.resolved_build_id != build
        {
            return Err("This session does not use this exact native app build".into());
        }
        self.synchronize()?;
        self.services.store.authorize_replication(
            &Access::session_version(self.services.store.account().id, &session.id, &build),
            session.replica_volume(),
            crate::store::object_contract::Permission::Read,
        )?;
        self.services.store.complete_invitation(&session.id)?;
        Ok(session)
    }
    /// Explicit initialization or user-requested recovery, never a freshness poll.
    pub fn synchronize(&mut self) -> Result<()> {
        self.active()?;
        let result = self.remote.sync(&self.services.store);
        self.remote.sample_gauges();
        result
    }
    /// Initialize and keep one known entry live without catalog admission.
    pub fn synchronize_entry(
        &mut self,
        address: crate::store::volume_index::EntryAddress,
        session: Option<&crate::store::Session>,
    ) -> Result<()> {
        self.active()?;
        self.remote.synchronize_entry(&self.services.store, address, session)
    }
    pub fn availability(&self) -> &str {
        self.remote.availability(&self.services.store)
    }
    pub fn account_initialized(&self) -> bool {
        self.remote.account_initialized()
    }
    pub fn blob_transport(&self) -> super::blob::BlobTransport {
        self.network.blobs.clone()
    }
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_connection_generation(&self) -> Option<u64> {
        self.remote.provider_connection_generation()
    }
    /// Publication work after a local provider reply, on its network lane.
    #[cfg(feature = "file-provider")]
    pub(crate) fn flush_provider(&mut self) -> Result<()> {
        self.active()?;
        self.remote.flush_provider(&self.services.store, &self.provider_mutations)
    }
    /// Set before enrollment/live workers exist, so empty admission cannot be
    /// mistaken for a full client or initialize app sessions.
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_only(&mut self) {
        *self.remote.scope.write() = super::remote::ClientScope::Provider(Vec::new());
    }
    #[cfg(feature = "file-provider")]
    pub(crate) fn select_provider_volumes(&mut self, volumes: &[crate::store::Id]) -> Result<()> {
        self.active()?;
        self.remote.select_provider_volumes(&self.services.store, volumes)
    }
    #[cfg(feature = "file-provider")]
    pub(crate) fn prepare_provider_copy(
        &mut self,
        destination: crate::store::Id,
        source: crate::store::volume_index::EntryAddress,
    ) -> Result<()> {
        let volumes = if destination == source.volume {
            vec![destination]
        } else {
            vec![destination, source.volume]
        };
        self.select_provider_volumes(&volumes)?;
        if !self.services.store.personal_volumes()?.iter().any(|volume| volume.id == source.volume)
        {
            return Err("database copy source is not an owned personal volume".into());
        }
        if !self.network.blobs.provider_connected()
            && self
                .services
                .store
                .entry_details(
                    &crate::store::Access::owner(self.services.store.account().id),
                    source.volume,
                    source.entry,
                )
                .is_ok_and(|details| details.readable && details.backing_ready)
        {
            return Ok(());
        }
        let prepared = self.remote.synchronize_entry(&self.services.store, source, None);
        let retired = self.remote.finish_provider_copy(source);
        if let Err(error) = retired {
            self.stop.request();
            return Err(error);
        }
        prepared
    }
    pub fn progress(&self) -> Option<String> {
        if self.remote.offline() {
            return None;
        }
        self.network.progress.snapshot().map(|progress| progress.message())
    }
    pub fn disconnect(&mut self) {
        self.remote.disconnect();
    }
    /// Remember connection intent in this profile; local operations stay usable.
    pub fn set_work_offline(&mut self, enabled: bool) -> Result<()> {
        self.active()?;
        if self.remote.offline() == enabled {
            return Ok(());
        }
        self.services.store.set_work_offline(enabled)?;
        self.remote.set_offline(enabled);
        if !enabled && self.remote.enrolled() {
            self.remote.sync(&self.services.store)?;
        }
        Ok(())
    }
    /// Drain bounded state notifications. Actual durable edits are delivered by
    /// the store's change feed; these notices also report gaps and revocation.
    pub fn take_notices(&self) -> Vec<ReplicaNotice> {
        self.network.received.drain()
    }

    /// Return the current retry deadline even after a failed advance. Hosts must
    /// retain errors and schedule this deadline; an error does not mean no retry.
    pub fn wake_after(&self, now: Instant) -> Option<Duration> {
        if self.stop.canceled.raised() { None } else { self.remote.wake_after(now) }
    }
    /// Clear coalesced readiness, then drive one ready recovery operation.
    /// Return whether an operation completed; an idle wake does not clear a
    /// previous operation failure. Call on readiness or a retry deadline only.
    pub fn advance(&mut self, now: Instant) -> Result<bool> {
        let result = self.advance_ready(now);
        self.remote.sample_gauges();
        result
    }
    fn advance_ready(&mut self, now: Instant) -> Result<bool> {
        self.active()?;
        self.completion.drain()?;
        self.network.progress.take_changed();
        if self.remote.offline() {
            return Ok(false);
        }
        #[cfg(target_vendor = "apple")]
        if self.remote.account_backup_pending(&self.services.store)? {
            let state = self.services.store.recovery_state()?.ok_or("account key is missing")?;
            self.remote.remember_account(&self.services.store, &state.secret)?;
            return Ok(true);
        }
        if self.remote.bootstrap_retry_ready(now) {
            self.remote.sync(&self.services.store)?;
        } else if self.remote.reconnect_ready(now) {
            self.remote.reconnect(&self.services.store)?;
        } else if self.remote.needs_gap_recovery() {
            self.remote.recover_gaps(&self.services.store)?;
        } else if self.remote.needs_initialization() {
            self.remote.initialize_volumes(&self.services.store)?;
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn remembered_work_offline_keeps_connected_local_services_available() {
        let directory = tempfile::tempdir().unwrap();
        {
            let store = crate::engine::LocalStore::open(directory.path()).unwrap();
            store.set_work_offline(true).unwrap();
        }
        let mut connected = Connected::open(directory.path(), || {}).unwrap();
        assert!(connected.remote.offline());
        assert!(!connected.advance(Instant::now()).unwrap());
        assert!(connected.wake_after(Instant::now()).is_none());
        let volume = connected.services.store.create_volume("Local", false).unwrap();
        assert_eq!(connected.services.store.volume(volume.id).unwrap().name, "Local");
        connected.set_work_offline(false).unwrap();
        assert!(!connected.remote.offline());
        assert!(!connected.services.store.work_offline().unwrap());
    }

    #[test]
    fn local_services_and_completion_retire_independently() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let mut connected = Connected::open(directory.path(), move || {
            called.fetch_add(1, Ordering::AcqRel);
        })
        .unwrap();
        assert!(!connected.enrolled());
        assert!(connected.email().is_empty());
        assert_eq!(connected.availability(), "Offline; local data is retained");
        connected.advance(Instant::now()).unwrap();
        calls.store(0, Ordering::Release);
        let root = connected.services.store.create_volume("Local", true).unwrap();
        let retained_wake = connected.completion.wake.clone();
        retained_wake.notify();
        retained_wake.notify();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        connected.advance(Instant::now()).unwrap();
        assert!(connected.wake_after(Instant::now()).is_none());
        assert!(connected.take_notices().is_empty());
        let store = connected.services.store.clone();
        drop(connected);
        let before = calls.load(Ordering::Acquire);
        retained_wake.notify();
        store.create_volume("Retained host", true).unwrap();
        assert_eq!(calls.load(Ordering::Acquire), before, "retired owner cannot call the host");
        assert_eq!(store.volume(root.id).unwrap().name, "Local");
    }

    #[test]
    fn app_notifications_rearm_without_a_network_owner_turn() {
        // A network request can remain blocked while an app consumes notices.
        // The second receive must still schedule the app independently.
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let connected = Connected::open(directory.path(), move || {
            called.fetch_add(1, Ordering::AcqRel);
        })
        .unwrap();
        let notifications = connected.notifications();
        for epoch in [[1; 16], [2; 16]] {
            connected.network.received.test_state(
                ReplicaSource::Account,
                epoch,
                ReplicaState::Live,
            );
            connected.completion.wake.notify();
            let notices = notifications.drain();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].epoch, epoch);
        }
        assert_eq!(calls.load(Ordering::Acquire), 2);
        drop(connected);
        assert!(notifications.drain().is_empty());
    }

    #[test]
    fn invalid_enrollment_and_permanent_stop_preserve_local_identity() {
        let directory = tempfile::tempdir().unwrap();
        let mut connected = Connected::open(directory.path(), || {}).unwrap();
        let account = connected.services.store.account().id;
        for email in ["", "missing-domain", "a @example.com"] {
            assert!(connected.request_code(email).is_err());
        }
        for code in [String::new(), "x".repeat(65)] {
            assert!(connected.sign_in(&code).is_err());
        }
        assert_eq!(connected.services.store.account().id, account);
        assert!(connected.email().is_empty());
        connected.stop_handle().request();
        assert!(connected.prepare_sign_in().is_err());
        assert!(connected.request_code("native@example.com").is_err());
        assert!(connected.sign_in("123456").is_err());
        assert!(connected.synchronize().is_err());
        assert!(connected.advance(Instant::now()).is_err());
        assert!(connected.wake_after(Instant::now()).is_none());
        drop(connected);
        let reopened = Connected::open(directory.path(), || {}).unwrap();
        assert!(!reopened.enrolled());
        assert_eq!(reopened.services.store.account().id, account);
    }
}
