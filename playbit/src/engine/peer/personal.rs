//! An authenticated owner and recipient exchange committed deltas over one opaque relay.
//! Subscribe before snapshot capture. An overrun closes the stream and requires a fresh cut.
use super::*;
use crate::store::{
    DatabaseId, EntryId,
    change_feed::{CommittedChange, CommittedChangeKind},
    volume_index::BlobVersion,
};
use std::{collections::BTreeMap, sync::Mutex};
use tokio::sync::{broadcast, mpsc, watch};
mod preparation;
use preparation::PreparationWaits;
pub(in crate::engine) use preparation::{OWNER_BUSY, PreparationBusy};

#[derive(Clone)]
enum Record {
    Change(Arc<CommittedChange>),
    FileAvailable(Arc<BlobVersion>),
    Gap,
}
const FEED_SLOTS: usize = 8;
const ACTIVE_VOLUMES: usize = 256;

#[derive(Default)]
pub(in crate::engine) struct PrivateFeeds(Mutex<BTreeMap<Id, broadcast::Sender<Record>>>);
impl PrivateFeeds {
    fn subscribe(&self, volume: Id) -> Result<broadcast::Receiver<Record>> {
        let mut feeds = self.0.lock().expect("private volume feed lock");
        feeds.retain(|_, feed| feed.receiver_count() != 0);
        if feeds.len() >= ACTIVE_VOLUMES && !feeds.contains_key(&volume) {
            return Err(SUBSCRIPTION_BUSY.into());
        }
        Ok(feeds.entry(volume).or_insert_with(|| broadcast::channel(FEED_SLOTS).0).subscribe())
    }
    pub(in crate::engine) fn publish(&self, record: &Arc<CommittedChange>) {
        if matches!(record.kind(), CommittedChangeKind::Database(_))
            && let Some(feed) =
                self.0.lock().expect("private volume feed lock").get(&record.volume())
        {
            let _ = feed.send(Record::Change(record.clone()));
        }
    }
    pub(super) fn file_available(&self, version: &BlobVersion) {
        if let Some(feed) =
            self.0.lock().expect("private volume feed lock").get(&version.address.volume)
        {
            let _ = feed.send(Record::FileAvailable(Arc::new(version.clone())));
        }
    }
    pub(in crate::engine) fn gap(&self) {
        for feed in self.0.lock().expect("private volume feed lock").values() {
            let _ = feed.send(Record::Gap);
        }
    }
}

/// A cursor scans at most one file per automatic network turn. A fresh edit
/// may sort before that cursor, so retain one rescan until this pass finishes.
pub(in crate::engine) struct FileWork {
    pub(in crate::engine) volume: Id,
    pub(in crate::engine) after: Option<(EntryId, EntryId)>,
    rescan: bool,
}

#[derive(Default)]
pub(in crate::engine) struct FileNotices(Mutex<std::collections::VecDeque<FileWork>>);

impl FileNotices {
    pub(in crate::engine) fn schedule(
        &self,
        volume: Id,
        wake: &crate::engine::completion::Wake,
    ) -> Result<()> {
        self.enqueue(FileWork { volume, after: None, rescan: false }, wake)
    }

    pub(in crate::engine) fn resume(
        &self,
        mut work: FileWork,
        next: Option<(EntryId, EntryId)>,
        wake: &crate::engine::completion::Wake,
    ) -> Result<()> {
        if next.is_none() && !work.rescan {
            return Ok(());
        }
        work.after = next;
        work.rescan &= next.is_some();
        self.enqueue(work, wake)
    }

    fn enqueue(&self, work: FileWork, wake: &crate::engine::completion::Wake) -> Result<()> {
        let mut volumes = self.0.lock().expect("private file notices");
        if let Some(queued) = volumes.iter_mut().find(|queued| queued.volume == work.volume) {
            // Only the network owner advances cursors. Merge a fresh declaration
            // with a continuation in either arrival order.
            queued.after = queued.after.or(work.after);
            queued.rescan = queued.after.is_some();
            return Ok(());
        }
        if volumes.len() == ACTIVE_VOLUMES {
            return Err(SUBSCRIPTION_BUSY.into());
        }
        volumes.push_back(work);
        drop(volumes);
        wake.notify();
        Ok(())
    }

    pub(in crate::engine) fn ready(&self) -> bool {
        !self.0.lock().expect("private file notices").is_empty()
    }

    pub(in crate::engine) fn take(&self) -> Option<FileWork> {
        self.0.lock().expect("private file notices").pop_front()
    }
}

#[derive(Clone)]
pub(in crate::engine) struct LiveUpdates {
    pub feeds: Arc<PrivateFeeds>,
    pub dirty: Arc<std::sync::atomic::AtomicBool>,
    pub wake: Arc<crate::engine::completion::Wake>,
}

pub(in crate::engine) struct Subscription {
    stream: ApStream,
    event: PeerEvent,
    store: Arc<LocalStore>,
    changes: broadcast::Receiver<Record>,
    key: [u8; 32],
    control: Checkpoint,
    incoming: mpsc::Receiver<(u8, Id)>,
    acknowledged: watch::Sender<Option<Id>>,
}

// The writer owns the encryption counter and sends both deltas and control frames
// on the same ordered stream. A checkpoint certifies the owner's preceding index
// applies before either an explicit sync or a live file transfer can proceed.
const CHECKPOINT: u8 = 6;
const FILE_AVAILABLE: u8 = 11;
// Reserve the null checkpoint ID for opt-in. Older owners echo this valid probe
// unchanged. Older readers never opt in to the additional live frame.
const FILE_HINT_PROBE: Id = Id::from_bytes([0; 16]);

#[derive(Clone)]
pub(in crate::engine) struct Checkpoint {
    commands: mpsc::Sender<(u8, Id)>,
    acknowledged: watch::Receiver<Option<Id>>,
    timeout: Duration,
    preparation: Arc<PreparationWaits>,
}

impl Checkpoint {
    pub(in crate::engine) fn wait_for_preparation(&self, busy: PreparationBusy) {
        self.preparation.arm(busy);
    }

    pub(in crate::engine) async fn wait(&self) -> Result<()> {
        let nonce = Id::generate()?;
        if nonce == FILE_HINT_PROBE {
            return Err("Private peer checkpoint generated a reserved nonce".into());
        }
        let mut acknowledged = self.acknowledged.clone();
        tokio::time::timeout(self.timeout, async {
            self.commands
                .send((CHECKPOINT, nonce))
                .await
                .map_err(|_| "private peer disconnected")?;
            acknowledged
                .wait_for(|value| *value == Some(nonce))
                .await
                .map_err(|_| "private peer disconnected")?;
            Ok::<_, &'static str>(())
        })
        .await
        .map_err(|_| "private peer checkpoint timed out")??;
        Ok(())
    }
}

struct LiveWriter<'a> {
    stream: crate::network::ap::ApSendStream,
    cipher: Cipher,
    store: &'a Arc<LocalStore>,
    volume: Id,
    remote: Id,
    files: Option<&'a FileNotices>,
    file_hints: bool,
    wake: &'a crate::engine::completion::Wake,
}

impl LiveWriter<'_> {
    async fn frame(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        let frame = crate::network::ap::Frame::new(
            crate::network::ap::frame_type::DIRECT,
            0,
            [],
            crate::network::ap::operation::SERVICE_DESTINATION,
            self.cipher.seal(kind, payload)?,
        )?;
        self.stream.send(&frame).await?;
        Ok(())
    }

    async fn change(&mut self, record: Record) -> Result<()> {
        let record = match record {
            Record::Change(record) => record,
            Record::Gap => return Err("Private volume publication gap".into()),
            Record::FileAvailable(version) => return self.send_file_hint(version).await,
        };
        let CommittedChangeKind::Database(database) = record.kind() else { return Ok(()) };
        let store = self.store.clone();
        let (volume, remote) = (self.volume, self.remote);
        let writer_index = self.files.is_some() && database == DatabaseId::Index;
        let credit = live_metadata_slots().acquire_owned().await?;
        let projected = tokio::task::spawn_blocking(move || {
            let _credit = credit;
            let bytes = store.private_invitation_delta(volume, database, remote, record.bytes())?;
            let files = writer_index
                && !crate::store::volume_index::published_blob_versions(volume, &bytes)?.is_empty();
            Ok((bytes, files))
        })
        .await?;
        let (bytes, files) = match projected {
            Ok(publication) => publication,
            Err(crate::store::Error::AccessDenied("entry permission")) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut payload = Vec::with_capacity(DatabaseId::WIRE_BYTES + bytes.len());
        payload.extend_from_slice(&database.to_bytes());
        payload.extend_from_slice(&bytes);
        self.frame(DATA, &payload).await?;
        if files && let Some(notices) = self.files {
            notices.schedule(volume, self.wake)?;
        }
        Ok(())
    }

    async fn send_file_hint(&mut self, version: Arc<BlobVersion>) -> Result<()> {
        if !self.file_hints || self.files.is_some() {
            return Ok(());
        }
        let store = self.store.clone();
        let access = Access::private_volume_peer(self.remote, self.remote);
        let (address, revision) = (version.address, version.revision);
        let credit = live_metadata_slots().acquire_owned().await?;
        let allowed = tokio::task::spawn_blocking(move || {
            let _credit = credit;
            store.blob_version(&access, address, None).map(|current| current == *version)
        })
        .await?;
        match allowed {
            Ok(true) => {}
            Ok(false)
            | Err(
                crate::store::Error::AccessDenied(_)
                | crate::store::Error::NotFound(_)
                | crate::store::Error::ParticipantRevoked { .. },
            ) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let mut payload = [0; 30];
        payload[..15].copy_from_slice(address.entry.as_bytes());
        payload[15..].copy_from_slice(revision.as_bytes());
        self.frame(FILE_AVAILABLE, &payload).await
    }

    async fn run(
        mut self,
        mut changes: broadcast::Receiver<Record>,
        mut commands: mpsc::Receiver<(u8, Id)>,
        preparation: &PreparationWaits,
    ) -> Result<()> {
        if self.files.is_some() {
            self.frame(CHECKPOINT, FILE_HINT_PROBE.as_bytes()).await?;
        }
        loop {
            tokio::select! {
                () = preparation.owner_probe(), if self.files.is_some() => {
                    self.frame(CHECKPOINT, &[]).await?;
                }
                () = preparation.local_ready(self.store.file_preparation_available()) => {
                    if let Some(notices) = self.files {
                        notices.schedule(self.volume, self.wake)?;
                    } else {
                        self.frame(RECEIPT, &[]).await?;
                    }
                }
                command = commands.recv() => {
                    let (kind, nonce) = command.ok_or("private peer control closed")?;
                    if kind == RECEIPT && nonce == FILE_HINT_PROBE {
                        self.file_hints = true;
                    }
                    if kind == CHECKPOINT {
                        // The publisher drained the committed FIFO before enqueuing this
                        // command. Cover the current prefix; later edits cannot extend it.
                        let pending = changes.len();
                        if pending > FEED_SLOTS {
                            return Err("Private volume publication gap".into());
                        }
                        for _ in 0..pending {
                            self.change(changes.recv().await?).await?;
                        }
                    }
                    self.frame(kind, nonce.as_bytes()).await?;
                }
                record = changes.recv() => self.change(record?).await?,
            }
        }
    }
}

impl Subscription {
    fn new(
        stream: ApStream,
        event: PeerEvent,
        store: Arc<LocalStore>,
        changes: broadcast::Receiver<Record>,
        key: [u8; 32],
    ) -> Self {
        let (commands, incoming) = mpsc::channel(1);
        let (acknowledged, received) = watch::channel(None);
        let control = Checkpoint {
            commands,
            acknowledged: received,
            timeout: stream.response_timeout(),
            preparation: Arc::default(),
        };
        Self { stream, event, store, changes, key, control, incoming, acknowledged }
    }

    pub(in crate::engine) fn checkpoint(&self) -> Checkpoint {
        self.control.clone()
    }

    pub(in crate::engine) async fn run(
        self,
        dirty: Arc<std::sync::atomic::AtomicBool>,
        wake: Arc<crate::engine::completion::Wake>,
        files: Option<Arc<FileNotices>>,
    ) -> Result<()> {
        let Self { stream, event, store, changes, key, control, incoming, acknowledged } = self;
        let requester = files.is_some();
        let remote = Id::parse(if requester {
            &event.responder_account_id
        } else {
            &event.requester_account_id
        })?;
        let volume = Id::parse(&event.volume_id)?;
        let (send, mut receive) = stream.split();
        let writer = LiveWriter {
            stream: send,
            cipher: Cipher::new(key, &event, requester)?,
            store: &store,
            volume,
            remote,
            files: files.as_deref(),
            file_hints: false,
            wake: &wake,
        };
        let mut decrypt = Cipher::new(key, &event, requester)?;
        let read = async {
            loop {
                let frame = receive
                    .receive_with_body_limit(
                        crate::store::MAX_REPLICA_DELTA_BYTES + DatabaseId::WIRE_BYTES + 29,
                    )
                    .await?;
                let (kind, bytes) = decrypt.open(crate::network::ap::service::data(frame)?)?;
                match kind {
                    // The empty probe follows an authenticated OWNER_BUSY
                    // refusal. Ordinary checkpoints always carry a 16-byte ID.
                    CHECKPOINT if !requester && bytes.is_empty() => {
                        control.wait_for_preparation(PreparationBusy::Local);
                        continue;
                    }
                    CHECKPOINT if !requester => {
                        // All preceding records completed their durable applies on this reader.
                        let nonce = Id::from_bytes(bytes.as_slice().try_into()?);
                        control.commands.send((RECEIPT, nonce)).await?;
                        continue;
                    }
                    RECEIPT if requester && bytes.is_empty() => {
                        if control.preparation.owner_ready()
                            && let Some(notices) = &files
                        {
                            notices.schedule(volume, &wake)?;
                        }
                        continue;
                    }
                    RECEIPT if requester => {
                        let nonce = Id::from_bytes(bytes.as_slice().try_into()?);
                        if nonce == FILE_HINT_PROBE {
                            store.private_file_available(volume, None);
                            dirty.store(true, std::sync::atomic::Ordering::Release);
                            wake.notify();
                        } else {
                            acknowledged.send_replace(Some(nonce));
                        }
                        continue;
                    }
                    FILE_AVAILABLE if requester && bytes.len() == 30 => {
                        let entry = EntryId::from_bytes(bytes[..15].try_into()?)?;
                        let revision = EntryId::from_bytes(bytes[15..].try_into()?)?;
                        store.private_file_available(volume, Some((entry, revision)));
                        dirty.store(true, std::sync::atomic::Ordering::Release);
                        wake.notify();
                        continue;
                    }
                    DATA if bytes.len() >= DatabaseId::WIRE_BYTES => {}
                    _ => return Err("Invalid private volume live frame".into()),
                }
                let database = DatabaseId::from_bytes(bytes[..DatabaseId::WIRE_BYTES].try_into()?)?;
                let store = store.clone();
                let credit = live_metadata_slots().acquire_owned().await?;
                let (changed, permissions) = tokio::task::spawn_blocking(move || {
                    let _credit = credit;
                    let delta = &bytes[DatabaseId::WIRE_BYTES..];
                    let changed =
                        store.apply_private_invitation_delta(volume, database, remote, delta)?;
                    // An echoed ACL can already match our local edit. Its arrival
                    // still establishes that the owner applied that policy.
                    let permissions = requester
                        && database == DatabaseId::Index
                        && crate::store::volume_index::changes_permissions(delta)?;
                    crate::store::Result::Ok((changed, permissions))
                })
                .await??;
                if permissions && let Some(notices) = &files {
                    notices.schedule(volume, &wake)?;
                }
                if changed || permissions {
                    dirty.store(true, std::sync::atomic::Ordering::Release);
                    wake.notify();
                }
            }
        };
        tokio::select! {
            result = writer.run(changes, incoming, &control.preparation) => result,
            result = read => result,
        }
    }
}

pub(in crate::engine) async fn prepare_subscription(
    connection: &Arc<ApConnection>,
    store: Arc<LocalStore>,
    volume: Id,
    feeds: &PrivateFeeds,
) -> Result<Subscription> {
    let changes = feeds.subscribe(volume)?;
    let request =
        ServiceRequest::VolumePeerSubscribe { operation_id: Id::generate()?.to_string(), volume };
    let mut stream = wire::begin(connection, &request).await?;
    let response = wire::response(&mut stream, request.operation_id()).await?;
    let event: PeerEvent =
        serde_json::from_value(response.data.ok_or("Missing personal volume peer offer")?)?;
    validate_volume_event(&event, &store, volume, DatabaseId::Index, SyncDirection::Pull)?;
    if !event.live {
        return Err("Peer did not authorize a live volume subscription".into());
    }
    let key = volume_key(&store, volume, DatabaseId::Index)?;
    // Readiness comes from the owner after acquiring a feed cursor.
    // It is domain-separated from the two live cipher counters.
    let mut cipher = Cipher::new(handshake_key(key), &event, true)?;
    ready(&mut stream, &mut cipher).await?;
    Ok(Subscription::new(stream, event, store, changes, key))
}

fn handshake_key(key: [u8; 32]) -> [u8; 32] {
    blake3::derive_key("app.playbit.personal-volume-live.ready.v1", &key)
}

pub(super) async fn offer_live(
    connection: Arc<ApConnection>,
    store: Arc<LocalStore>,
    volume: Id,
    event: PeerEvent,
    updates: LiveUpdates,
) -> Result<()> {
    let account = Id::parse(&event.requester_account_id)?;
    if !store
        .volume_invitation_policy(volume)?
        .is_some_and(|policy| policy.recipients.contains(&account))
    {
        return Err("Personal volume grant was withdrawn".into());
    }
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(Semaphore::new(32)));
    let request = ServiceRequest::PeerReply {
        operation_id: Id::generate()?.to_string(),
        request_id: event.request_id.clone(),
    };
    let mut stream = wire::begin(&connection, &request).await?;
    wire::response(&mut stream, request.operation_id()).await?;
    let key = volume_key(&store, volume, DatabaseId::Index)?;
    let mut cipher = Cipher::new(handshake_key(key), &event, false)?;
    let reservation: Result<_> = slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| SUBSCRIPTION_BUSY.into())
        .and_then(|slot| updates.feeds.subscribe(volume).map(|changes| (slot, changes)));
    let (slot, changes) = match reservation {
        Ok(reservation) => reservation,
        Err(error) => {
            let failure = crate::store::Error::System(error.to_string());
            send(&mut stream, &mut cipher, ERROR, &failure::encode(&failure)?).await?;
            stream.finish()?;
            return Ok(());
        }
    };
    send(&mut stream, &mut cipher, READY, &[]).await?;
    tokio::spawn(async move {
        let _slot = slot;
        let subscription = Subscription::new(stream, event, store, changes, key);
        if let Err(error) = subscription.run(updates.dirty, updates.wake, None).await {
            crate::diagnostic!("Private volume live subscription ended: {error}");
        }
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "The test controls the checkpoint and receipt channels.")]
mod tests {
    use super::*;

    #[test]
    fn automatic_file_turn_yields_and_revisits_edits_before_its_cursor() {
        let notices = FileNotices::default();
        let completion = crate::engine::completion::Completion::host(|| {});
        let wake = &completion.wake;
        let first = Id::generate().unwrap();
        let second = Id::generate().unwrap();
        let cursor = (EntryId::generate().unwrap(), EntryId::generate().unwrap());
        notices.schedule(first, wake).unwrap();
        let started = notices.take().unwrap();
        notices.schedule(second, wake).unwrap();
        // A second first-volume edit arrives while its first upload is blocked.
        notices.schedule(first, wake).unwrap();
        notices.resume(started, Some(cursor), wake).unwrap();
        assert_eq!(notices.take().unwrap().volume, second);
        let continued = notices.take().unwrap();
        assert_eq!((continued.volume, continued.after), (first, Some(cursor)));
        notices.resume(continued, None, wake).unwrap();
        let restarted = notices.take().unwrap();
        assert_eq!((restarted.volume, restarted.after), (first, None));
        notices.resume(restarted, None, wake).unwrap();
        assert!(!notices.ready());
    }

    #[tokio::test]
    async fn private_sync_requires_its_own_receipt_and_reports_disconnect() {
        let (commands, mut reader) = mpsc::channel(1);
        let (peer, acknowledged) = watch::channel(None);
        let checkpoint = Checkpoint {
            commands,
            acknowledged,
            timeout: Duration::from_secs(5),
            preparation: Arc::default(),
        };
        // An armed file wait neither occupies the command slot nor supplies
        // the receipt needed for an ordinary metadata checkpoint.
        checkpoint.wait_for_preparation(PreparationBusy::Local);
        checkpoint.wait_for_preparation(PreparationBusy::Owner);
        let preparation = Arc::downgrade(&checkpoint.preparation);
        let first = checkpoint.clone();
        let wait = tokio::spawn(async move { first.wait().await });
        let (kind, nonce) = reader.recv().await.unwrap();
        assert_eq!(kind, CHECKPOINT);
        peer.send_replace(Some(nonce));
        wait.await.unwrap().unwrap();

        let wait = tokio::spawn(async move { checkpoint.wait().await });
        let (_, next_nonce) = reader.recv().await.unwrap();
        assert_ne!(next_nonce, nonce);
        drop(peer);
        assert_eq!(wait.await.unwrap().unwrap_err().to_string(), "private peer disconnected");
        assert!(preparation.upgrade().is_none());
    }
}
