//! Reconcile owner grants and receive the authenticated account's direct volume offers.
use super::*;
use crate::network::ap::{
    ApStream,
    operation::{
        VolumeInvitationEvent as Event, VolumeInvitationOffer, volume_invitation_key_message,
    },
    service,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, broadcast, mpsc};

const RETRIES: u8 = 6;
fn retry_delay(attempt: u8) -> Duration {
    Duration::from_millis(250 << attempt)
}

struct PrivateStream {
    task: tokio::task::JoinHandle<()>,
    ended: Arc<AtomicBool>,
    checkpoint: crate::engine::peer::personal::Checkpoint,
}

enum Notice {
    Reconcile(Id),
    PeerAvailable(Id),
}

pub(super) struct Watch {
    private: BTreeMap<Id, PrivateStream>,
    pub(super) rescan: Arc<Notify>,
    connection: Arc<ApConnection>,
    task: tokio::task::JoinHandle<()>,
    pending: mpsc::Receiver<Notice>,
    enqueue: mpsc::Sender<Notice>,
    files: Arc<crate::engine::peer::personal::FileNotices>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
        for stream in self.private.values() {
            stream.task.abort();
        }
    }
}

impl Remote {
    pub(super) fn start_volume_invitations(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        if self.scope.read().provider() {
            return Ok(());
        }
        let Some(connection) = self.connection.clone() else { return Ok(()) };
        if let Some(watch) = &self.volume_invitation_watch
            && Arc::ptr_eq(&watch.connection, &connection)
            && !watch.task.is_finished()
        {
            return Ok(());
        }
        let Some(state) = store.recovery_state()?.filter(|s| s.acknowledged && s.revision != 0)
        else {
            return Ok(());
        };
        self.volume_invitation_watch = None;
        let public_key = crate::store::account_key::volume_invitation_public_key(
            store.account().id,
            &state.secret,
        )?;
        let signature = crate::store::account_key::sign(
            &state.secret,
            &volume_invitation_key_message(store.account().id, &public_key),
        )?;
        let (enqueue, pending) = mpsc::channel(crate::engine::publisher::VOLUME_LIMIT);
        let rescan = Arc::new(Notify::new());
        let inbox = Inbox {
            rescan: rescan.clone(),
            feeds: self
                .live
                .as_ref()
                .ok_or("live replication unavailable")?
                .publisher()
                .private_feeds(),
            store: store.clone(),
            connection: connection.clone(),
            wake: self.completion.clone(),
            dirty: self.volume_invitations_changed.clone(),
            enqueue: enqueue.clone(),
        };
        let task = self.runtime.spawn(async move {
            if let Err(error) = inbox.run(public_key, signature).await {
                crate::diagnostic!("Volume invitation delivery interrupted: {error}");
            }
        });
        self.volume_invitation_watch = Some(Watch {
            connection,
            task,
            pending,
            enqueue,
            rescan,
            files: Arc::default(),
            private: BTreeMap::new(),
        });
        Ok(())
    }
    pub(super) fn sync_direct_volume(&mut self, store: &Arc<LocalStore>, volume: Id) -> Result<()> {
        if self.has_live_volume(store, volume)? && !self.initializations.tracked(volume) {
            return Ok(());
        }
        let result = self.capture_direct_volume(store, volume).and_then(|pending| {
            if let Some(database) = pending {
                return Err(sync_failure::database(
                    store,
                    &Access::owner(store.account().id),
                    volume,
                    database,
                    "waiting for initial database synchronization",
                ));
            }
            Ok(())
        });
        self.complete_volume_capture(store, volume, result, true)
    }

    pub(super) fn capture_direct_volume(
        &mut self,
        store: &Arc<LocalStore>,
        volume: Id,
    ) -> Result<Option<crate::store::DatabaseId>> {
        if !store.has_direct_volume_grant(volume)? {
            return Err("Personal volume invitation was withdrawn".into());
        }
        let descriptor = store.volume(volume)?;
        let connection = self.connection.as_ref().ok_or("AP disconnected")?.clone();
        let access = Access::owner(store.account().id);
        let capture = |remote: &Self| -> Result<ReceivedVolume> {
            let received = remote.receive_volume(store, None, volume)?;
            store.adopt_received_revocations(&access, &received)?;
            store.adopt_received_volume(&access, &received)?;
            remote.acknowledge_received(store, &access, &received)?;
            Ok(received)
        };
        if !descriptor.share_key_with_service {
            let watch = self
                .volume_invitation_watch
                .as_mut()
                .ok_or("Volume invitation stream is unavailable")?;
            watch.private.retain(|_, stream| !stream.ended.load(Ordering::Acquire));
            if watch.private.contains_key(&volume) && !self.initializations.tracked(volume) {
                return Ok(None);
            }
            if let Some(stream) = watch.private.remove(&volume) {
                stream.task.abort();
            }
            if watch.private.len() >= crate::engine::publisher::VOLUME_LIMIT {
                return Err(crate::engine::peer::SUBSCRIPTION_BUSY.into());
            }
            let feeds = self
                .live
                .as_ref()
                .ok_or("live replication unavailable")?
                .publisher()
                .private_feeds();
            let subscription = self.runtime.block_on(crate::engine::peer::prepare_subscription(
                &connection,
                store.clone(),
                volume,
                &feeds,
            ))?;
            let received = capture(self)?;
            self.push_volume(store, None, volume)?;
            let watch = self
                .volume_invitation_watch
                .as_mut()
                .expect("subscription is scoped to this watch");
            let enqueue = watch.enqueue.clone();
            let files = watch.files.clone();
            let wake = self.completion.clone();
            let dirty = self.volume_invitations_changed.clone();
            let ended = Arc::new(AtomicBool::new(false));
            let finished = ended.clone();
            let checkpoint = subscription.checkpoint();
            let task = self.runtime.spawn(async move {
                let result = subscription.run(dirty, wake.clone(), Some(files)).await;
                finished.store(true, Ordering::Release);
                if let Err(error) = result {
                    crate::diagnostic!("Private volume subscription ended for {volume}: {error}");
                    // This stream already completed bootstrap. A subsequent loss is an
                    // explicit gap. If the owner is now offline, PeerAvailable wakes it.
                    if enqueue.send(Notice::Reconcile(volume)).await.is_ok() {
                        wake.notify();
                    }
                }
            });
            watch.private.insert(volume, PrivateStream { task, ended, checkpoint });
            // The live cursor covers new commits. Replay retained bodies whose
            // original commit notifications predate this connection or process.
            watch.files.schedule(volume, &self.completion)?;
            return Ok(received.pending_backing);
        }
        // Register the live stream before capturing the source. Publications during
        // the capture are retained until the local replica has installed that cut.
        let mut prepared = self.runtime.block_on(
            self.live.as_mut().ok_or("live replication unavailable")?.begin(
                connection,
                crate::engine::replication::ReplicaScope::Volume { volume, session: None },
            ),
        )?;
        self.runtime.block_on(
            self.live.as_mut().expect("live replication initialized").subscribe(&mut prepared),
        )?;
        let received = capture(self)?;
        self.finish_captured_live(store, None, volume, prepared)?;
        Ok(received.pending_backing)
    }

    pub(super) fn retire_direct_grant(&mut self, volume: Id) -> Result<()> {
        if let Some(watch) = &mut self.volume_invitation_watch
            && let Some(stream) = watch.private.remove(&volume)
        {
            stream.task.abort();
        }
        self.initializations.forget(volume);
        if let Some(live) = &mut self.live {
            self.runtime.block_on(live.retire_direct_grant(volume))?;
        }
        Ok(())
    }

    pub(super) fn peer_invitations_ready(&self) -> bool {
        self.volume_invitation_watch
            .as_ref()
            .is_some_and(|watch| !watch.pending.is_empty() || watch.files.ready())
    }

    /// Upload at most one file per turn. Report when this turn drains the
    /// scheduled file work so cached account availability can be refreshed.
    pub(super) fn upload_ready_private_files(&self, store: &LocalStore) -> Result<bool> {
        let Some(watch) = &self.volume_invitation_watch else { return Ok(false) };
        let Some(work) = watch.files.take() else { return Ok(false) };
        let volume = work.volume;
        if watch.files.ready() {
            self.completion.notify();
        }
        if self.private_invitation_connected(volume)
            && self.scope.read().admits(volume)
            && store.has_direct_volume_grant(volume)?
        {
            let page = store.next_private_file_upload(volume, work.after)?;
            let result = self
                .upload_private_page(store, volume, page.versions)
                .map_err(|error| sync_failure::volume(store, volume, error))?;
            watch.files.resume(work, page.next, &self.completion)?;
            if let Some(error) = result {
                return Err(sync_failure::volume(store, volume, error));
            }
        }
        Ok(!watch.files.ready())
    }

    pub(super) fn private_invitation_connected(&self, volume: Id) -> bool {
        self.volume_invitation_watch
            .as_ref()
            .and_then(|watch| watch.private.get(&volume))
            .is_some_and(|stream| !stream.ended.load(Ordering::Acquire))
    }

    pub(super) fn private_checkpoint(
        &self,
        volume: Id,
    ) -> Result<&crate::engine::peer::personal::Checkpoint> {
        self.volume_invitation_watch
            .as_ref()
            .and_then(|watch| watch.private.get(&volume))
            .map(|stream| &stream.checkpoint)
            .ok_or_else(|| "waiting for an authorized peer".into())
    }

    pub(super) fn checkpoint_direct_volume(&self, volume: Id) -> Result<()> {
        let checkpoint = self.private_checkpoint(volume)?;
        let publisher = self.live.as_ref().ok_or("live replication unavailable")?.publisher();
        self.runtime.block_on(async {
            publisher.drain_private_changes().await?;
            checkpoint.wait().await
        })
    }

    pub(super) fn take_ready_peer_invitations(&mut self, limit: usize) -> Result<BTreeSet<Id>> {
        let mut ready = BTreeSet::new();
        if let Some(watch) = &mut self.volume_invitation_watch {
            for _ in 0..limit {
                let Ok(notice) = watch.pending.try_recv() else { break };
                let volume = match notice {
                    Notice::Reconcile(id) | Notice::PeerAvailable(id) => id,
                };
                if watch.private.get(&volume).is_some_and(|s| !s.ended.load(Ordering::Acquire)) {
                    // A fresh inbox snapshot can replace a lost file wake while the
                    // authorized catalog stream stays connected across the gap.
                    watch.files.schedule(volume, &self.completion)?;
                    if matches!(notice, Notice::PeerAvailable(_)) {
                        continue;
                    }
                }
                ready.insert(volume);
            }
        }
        Ok(ready)
    }
}

struct Inbox {
    rescan: Arc<Notify>,
    feeds: Arc<crate::engine::peer::PrivateFeeds>,
    store: Arc<LocalStore>,
    connection: Arc<ApConnection>,
    wake: Arc<Wake>,
    dirty: Arc<AtomicBool>,
    enqueue: mpsc::Sender<Notice>,
}

impl Inbox {
    async fn run(&self, public_key: [u8; 32], signature: [u8; 64]) -> Result<()> {
        let mut failures = 0;
        loop {
            let start = Instant::now();
            let error = match self.watch(public_key, signature).await {
                Ok(false) => return Ok(()), // Older AP; ordinary replication remains available.
                Ok(true) => {
                    failures = 0;
                    continue;
                } // Explicit AP gap.
                Err(error) => error,
            };
            if start.elapsed() > Duration::from_secs(30) {
                failures = 0;
            }
            if self.connection.is_closed() || failures == RETRIES {
                return Err(error);
            }
            crate::diagnostic!("Reopening volume invitation stream: {error}");
            tokio::time::sleep(retry_delay(failures)).await;
            failures += 1;
        }
    }

    async fn watch(&self, public_key: [u8; 32], signature: [u8; 64]) -> Result<bool> {
        if !self
            .connection
            .describe()
            .await?
            .features
            .iter()
            .any(|f| f == service::VOLUME_INVITATIONS)
        {
            return Ok(false);
        }
        wire::request(
            &self.connection,
            &ServiceRequest::RegisterVolumeInvitationKey {
                operation_id: Id::generate()?.to_string(),
                public_key,
                signature: signature.into(),
            },
        )
        .await?;
        let policy_changes = self.store.volume_invitation_policy_changes();
        let request =
            ServiceRequest::WatchVolumeInvitations { operation_id: Id::generate()?.to_string() };
        let mut stream = wire::begin(&self.connection, &request).await?;
        wire::response(&mut stream, request.operation_id()).await?;
        tokio::select! {
            result = self.receive(stream) => result,
            result = self.publish(policy_changes) => result,
        }?;
        Ok(true)
    }

    async fn receive(&self, mut stream: ApStream) -> Result<()> {
        let mut snapshot = true;
        loop {
            let bytes = service::data(stream.receive_with_body_limit(8192).await?)?;
            let store = self.store.clone();
            let changed = match serde_json::from_slice::<Event>(&bytes)? {
                Event::Upsert { item } => {
                    let volume = item.volume;
                    let result = tokio::task::spawn_blocking(move || {
                        let envelope = crate::store::parse_envelope_text(&item.envelope)?;
                        store.apply_volume_invitation(
                            item.owner,
                            item.volume,
                            item.clock,
                            item.share_key_with_service,
                            &envelope,
                            item.owner_name.as_deref(),
                        )
                    })
                    .await?;
                    match result {
                        Ok(changed) => {
                            // Reconcile retained grants and replay their pending files on a gap.
                            if snapshot || changed {
                                self.enqueue.send(Notice::Reconcile(volume)).await?;
                            }
                            snapshot || changed
                        }
                        Err(
                            crate::store::Error::InvalidInput(_)
                            | crate::store::Error::AccessDenied(
                                "invalid volume invitation ciphertext",
                            ),
                        ) => {
                            crate::diagnostic!("Rejected invalid personal volume invitation");
                            false
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Event::Remove { volume, clock } => {
                    let changed = tokio::task::spawn_blocking(move || {
                        store.withdraw_volume_invitation(volume, clock)
                    })
                    .await??;
                    if changed {
                        self.enqueue.send(Notice::Reconcile(volume)).await?;
                    }
                    changed
                }
                Event::KeyReady => {
                    self.rescan.notify_one();
                    false
                }
                Event::PeerAvailable { volume } => {
                    self.enqueue.send(Notice::PeerAvailable(volume)).await?;
                    true
                }
                Event::Ready => {
                    snapshot = false;
                    false
                }
                Event::Gap => return Ok(()),
            };
            if changed {
                self.dirty.store(true, Ordering::Release);
                self.wake.notify();
            }
        }
    }

    async fn publish(&self, mut changes: broadcast::Receiver<Id>) -> Result<()> {
        let mut retries = BTreeMap::<Id, (tokio::time::Instant, u8)>::new();
        let mut listeners = PrivateListeners::default();
        let mut selected = None;
        loop {
            let store = self.store.clone();
            let ids = if let Some(volume) = selected {
                vec![volume]
            } else {
                tokio::task::spawn_blocking(move || store.volume_invitation_publications())
                    .await??
            };
            for id in ids {
                let attempt = retries.remove(&id).map_or(0, |(_, attempt)| attempt);
                if let Err(error) = self.reconcile(id, &mut listeners).await {
                    if self.connection.is_closed() {
                        return Err(error);
                    }
                    crate::diagnostic!(
                        "Could not synchronize volume invitations for {id}: {error}"
                    );
                    if attempt < RETRIES && retries.len() < crate::engine::publisher::VOLUME_LIMIT {
                        retries.insert(
                            id,
                            (tokio::time::Instant::now() + retry_delay(attempt), attempt + 1),
                        );
                    }
                }
            }
            let next = retries
                .iter()
                .min_by_key(|(_, (deadline, _))| *deadline)
                .map(|(id, (deadline, _))| (*id, *deadline));
            selected = tokio::select! {
                signal = changes.recv() => match signal {
                    Ok(id) => Some(id),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                () = self.rescan.notified() => None,
                id = async {
                    if let Some((id, at)) = next { tokio::time::sleep_until(at).await; id }
                    else { std::future::pending().await }
                } => Some(id),
            };
        }
    }

    async fn reconcile(&self, volume: Id, listeners: &mut PrivateListeners) -> Result<()> {
        use crate::store::VolumeInvitationPublication;
        let store = self.store.clone();
        let publication =
            tokio::task::spawn_blocking(move || store.volume_invitation_publication(volume))
                .await??;
        let (clock, recipients, descriptor) = match publication {
            Some(VolumeInvitationPublication::Policy(policy)) => {
                (policy.clock, policy.recipients, Some(policy.volume))
            }
            Some(VolumeInvitationPublication::Withdrawal) => {
                (VolumeInvitationPublication::WITHDRAWAL_CLOCK, Vec::new(), None)
            }
            None => {
                listeners.retire(volume);
                return Ok(());
            }
        };
        wire::request(
            &self.connection,
            &ServiceRequest::PutVolumeInvitationPolicy {
                operation_id: Id::generate()?.to_string(),
                volume,
                clock,
                recipients: recipients.clone(),
            },
        )
        .await?;
        let mut wanted: BTreeSet<Id> = recipients.into_iter().collect();
        let private =
            !wanted.is_empty() && descriptor.as_ref().is_some_and(|v| !v.share_key_with_service);
        if !private {
            listeners.retire(volume);
        }

        self.reconcile_existing_offers(volume, clock, &mut wanted).await?;
        let Some(descriptor) = descriptor else {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                store.acknowledge_volume_invitation_withdrawal(volume, clock)
            })
            .await??;
            return Ok(());
        };
        let mut first_error = None;
        for recipient in wanted {
            if let Err(error) = self.offer(&descriptor, recipient, clock).await {
                if self.connection.is_closed() {
                    return Err(error);
                }
                first_error.get_or_insert(error);
            }
        }
        // Discovery remains available while the owner's peer transport is unavailable.
        if private {
            listeners.listen(self, volume).await?;
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn reconcile_existing_offers(
        &self,
        volume: Id,
        clock: u64,
        wanted: &mut BTreeSet<Id>,
    ) -> Result<()> {
        let mut after = None;
        loop {
            let response = wire::request(
                &self.connection,
                &ServiceRequest::ListVolumeInvitationRecipients {
                    operation_id: Id::generate()?.to_string(),
                    volume,
                    after,
                },
            )
            .await?;
            let page: Recipients =
                serde_json::from_value(response.data.ok_or("missing invitation recipient page")?)?;
            if page.recipients.len() > 32 || page.next.is_some() && page.next == after {
                return Err("Invalid invitation recipient page".into());
            }
            for (recipient, offered) in page.recipients {
                if wanted.contains(&recipient) {
                    if offered >= clock {
                        wanted.remove(&recipient);
                    }
                } else if offered <= clock {
                    self.put(volume, recipient, clock, None).await?;
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        Ok(())
    }

    async fn offer(
        &self,
        descriptor: &crate::store::Volume,
        recipient: Id,
        clock: u64,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct RecipientKey {
            public_key: Option<[u8; 32]>,
        }
        let volume = descriptor.id;
        let response = wire::request(
            &self.connection,
            &ServiceRequest::GetVolumeInvitationKey {
                operation_id: Id::generate()?.to_string(),
                volume,
                recipient,
            },
        )
        .await?;
        let key: RecipientKey =
            serde_json::from_value(response.data.ok_or("missing volume invitation key response")?)?;
        let Some(key) = key.public_key else { return Ok(()) };
        let store = self.store.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            store.seal_volume_invitation(volume, recipient, clock, &key)
        })
        .await??;
        self.put(
            volume,
            recipient,
            clock,
            Some(VolumeInvitationOffer {
                share_key_with_service: descriptor.share_key_with_service,
                envelope: crate::store::envelope_text(&bytes),
            }),
        )
        .await
    }

    async fn put(
        &self,
        volume: Id,
        recipient: Id,
        clock: u64,
        invitation: Option<VolumeInvitationOffer>,
    ) -> Result<()> {
        wire::request(
            &self.connection,
            &ServiceRequest::PutVolumeInvitation {
                operation_id: Id::generate()?.to_string(),
                volume,
                recipient,
                clock,
                invitation,
            },
        )
        .await?;
        Ok(())
    }
}
#[derive(Deserialize)]
struct Recipients {
    recipients: Vec<(Id, u64)>,
    next: Option<Id>,
}

#[derive(Default)]
struct PrivateListeners(BTreeMap<Id, tokio::task::JoinHandle<()>>);
impl PrivateListeners {
    fn retire(&mut self, volume: Id) {
        if let Some(task) = self.0.remove(&volume) {
            task.abort();
        }
    }
    async fn listen(&mut self, inbox: &Inbox, volume: Id) -> Result<()> {
        if self.0.get(&volume).is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        if self.0.len() >= 256 && !self.0.contains_key(&volume) {
            return Err("Private volume listener limit reached".into());
        }
        let task = crate::engine::peer::listen_volume(
            inbox.connection.clone(),
            inbox.store.clone(),
            volume,
            crate::engine::peer::LiveUpdates {
                feeds: inbox.feeds.clone(),
                dirty: inbox.dirty.clone(),
                wake: inbox.wake.clone(),
            },
        )
        .await?;
        self.0.insert(volume, task);
        Ok(())
    }
}
impl Drop for PrivateListeners {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}
