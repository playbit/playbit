//! An authenticated owner and recipient exchange committed deltas over one opaque relay.
//! Subscribe before snapshot capture. An overrun closes the stream and requires a fresh cut.
use super::*;
use crate::store::{
    DatabaseId,
    change_feed::{CommittedChange, CommittedChangeKind},
};
use std::{collections::BTreeMap, sync::Mutex};
use tokio::sync::{broadcast, mpsc, watch};

type Record = Option<Arc<CommittedChange>>;
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
            let _ = feed.send(Some(record.clone()));
        }
    }
    pub(in crate::engine) fn gap(&self) {
        for feed in self.0.lock().expect("private volume feed lock").values() {
            let _ = feed.send(None);
        }
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
    requester: bool,
    key: [u8; 32],
    control: Checkpoint,
    incoming: mpsc::Receiver<(u8, Id)>,
    acknowledged: watch::Sender<Option<Id>>,
}

// The writer owns the encryption counter and sends both deltas and control frames
// on the same ordered stream. Only an explicit sync requests a durable receipt.
const CHECKPOINT: u8 = 6;

#[derive(Clone)]
pub(in crate::engine) struct Checkpoint {
    commands: mpsc::Sender<(u8, Id)>,
    acknowledged: watch::Receiver<Option<Id>>,
    timeout: Duration,
}

impl Checkpoint {
    pub(in crate::engine) async fn wait(&self) -> Result<()> {
        let nonce = Id::generate()?;
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
        let record = record.ok_or("Private volume publication gap")?;
        let CommittedChangeKind::Database(database) = record.kind() else { return Ok(()) };
        let store = self.store.clone();
        let (volume, remote) = (self.volume, self.remote);
        let credit = response_slots().acquire_owned().await?;
        let projected = tokio::task::spawn_blocking(move || {
            let _credit = credit;
            store.private_invitation_delta(volume, database, remote, record.bytes())
        })
        .await?;
        let bytes = match projected {
            Ok(bytes) => bytes,
            Err(crate::store::Error::AccessDenied("entry permission")) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut payload = Vec::with_capacity(DatabaseId::WIRE_BYTES + bytes.len());
        payload.extend_from_slice(&database.to_bytes());
        payload.extend_from_slice(&bytes);
        self.frame(DATA, &payload).await
    }

    async fn run(
        mut self,
        mut changes: broadcast::Receiver<Record>,
        mut commands: mpsc::Receiver<(u8, Id)>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let (kind, nonce) = command.ok_or("private peer control closed")?;
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
        requester: bool,
        key: [u8; 32],
    ) -> Self {
        let (commands, incoming) = mpsc::channel(1);
        let (acknowledged, replies) = watch::channel(None);
        let control =
            Checkpoint { commands, acknowledged: replies, timeout: stream.response_timeout() };
        Self { stream, event, store, changes, requester, key, control, incoming, acknowledged }
    }

    pub(in crate::engine) fn checkpoint(&self) -> Checkpoint {
        self.control.clone()
    }

    pub(in crate::engine) async fn run(
        self,
        dirty: Arc<std::sync::atomic::AtomicBool>,
        wake: Arc<crate::engine::completion::Wake>,
    ) -> Result<()> {
        let Self { stream, event, store, changes, requester, key, control, incoming, acknowledged } =
            self;
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
                    CHECKPOINT if !requester => {
                        // All preceding records completed their durable applies on this reader.
                        let nonce = Id::from_bytes(bytes.as_slice().try_into()?);
                        control.commands.send((RECEIPT, nonce)).await?;
                        continue;
                    }
                    RECEIPT if requester => {
                        acknowledged
                            .send_replace(Some(Id::from_bytes(bytes.as_slice().try_into()?)));
                        continue;
                    }
                    DATA if bytes.len() >= DatabaseId::WIRE_BYTES => {}
                    _ => return Err("Invalid private volume live frame".into()),
                }
                let database = DatabaseId::from_bytes(bytes[..DatabaseId::WIRE_BYTES].try_into()?)?;
                let store = store.clone();
                let credit = response_slots().acquire_owned().await?;
                if tokio::task::spawn_blocking(move || {
                    let _credit = credit;
                    store.apply_private_invitation_delta(
                        volume,
                        database,
                        remote,
                        &bytes[DatabaseId::WIRE_BYTES..],
                    )
                })
                .await??
                {
                    dirty.store(true, std::sync::atomic::Ordering::Release);
                    wake.notify();
                }
            }
        };
        tokio::select! {result = writer.run(changes, incoming) => result, result = read => result}
    }
}

pub(in crate::engine) async fn prepare_subscription(
    connection: &ApConnection,
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
    Ok(Subscription::new(stream, event, store, changes, true, key))
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
        let subscription = Subscription::new(stream, event, store, changes, false, key);
        if let Err(error) = subscription.run(updates.dirty, updates.wake).await {
            crate::diagnostic!("Private volume live subscription ended: {error}");
        }
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "The test controls the checkpoint and receipt channels.")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_sync_requires_its_own_receipt_and_reports_disconnect() {
        let (commands, mut reader) = mpsc::channel(1);
        let (peer, acknowledged) = watch::channel(None);
        let checkpoint = Checkpoint { commands, acknowledged, timeout: Duration::from_secs(5) };
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
    }
}
