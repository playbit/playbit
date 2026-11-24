//! Private database synchronization through opaque streams on the existing AP.
//! Keys derive from the volume database key and the complete authenticated peer
//! event, separately for each direction. Random 96-bit nonces and authenticated
//! counters prevent nonce dependence on AP state, tampering, and stream replay.
//! Metadata admission and the merge commit share the LocalStore operation lock.
//! Senders prepare an owned kvdb view before READY; receivers wait for READY
//! outside their writer lock, so opposite-direction transfers cannot deadlock.
//! A final encrypted receipt follows the receiver's durable database commit.
//! Pull destinations stay detached until the whole-volume coordinator admits
//! them. AP relay completion alone is never a private durability acknowledgment.
// W7 coverage decision: initiating transfer errors return to Remote; response
// errors terminate/log that response, with encrypted ERROR when possible. Tests
// cover authenticated scope/order and offer validation; per-call RNG/transport
// variants are not individually injected. A completed store merge is never
// rolled back or repeated merely because its receipt was lost.
use crate::engine::Result;
use crate::network::ap::{
    ApConnection, ApStream,
    operation::{PeerEvent, ServiceRequest, SyncDirection},
    service_client as wire,
};
use crate::store::{
    Access, Database, DurableRevision, Id, LocalStore, ReplicaReader, ReplicaReceipt, Session,
    detached_replica, object_contract::Permission, outbound_replica,
};
use kvdb::{Lww, SyncReceiver, SyncSender, sync::SyncStatus};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::{Handle, Runtime},
    sync::Semaphore,
    task::JoinHandle,
};
mod failure;
pub(super) use failure::VerifiedDenial;

const CHUNK: usize = 256 * 1024 - 29;
const DATA: u8 = 1;
const FINISHED: u8 = 2;
const RECEIPT: u8 = 3;
const ERROR: u8 = 4;
const READY: u8 = 5;
const IDLE: Duration = Duration::from_secs(30);
const MAX_RESPONSES: usize = 2;

// Shared by all session listeners in this process. The blocking job owns its
// permit through its final durable receipt, even if its listener is canceled.
fn response_slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_RESPONSES))).clone()
}

struct Cipher {
    send: LessSafeKey,
    receive: LessSafeKey,
    sent: u64,
    received: u64,
}
impl Cipher {
    fn new(database_key: [u8; 32], event: &PeerEvent, requester: bool) -> Result<Self> {
        let mut context = database_key.to_vec();
        context.extend_from_slice(&serde_json::to_vec(event)?);
        let mut first = context.clone();
        first.extend_from_slice(b"requester-to-responder");
        context.extend_from_slice(b"responder-to-requester");
        let first = blake3::derive_key("app.playbit.private-database-transfer.v1", &first);
        let second = blake3::derive_key("app.playbit.private-database-transfer.v1", &context);
        let key = |key: &[u8]| -> Result<LessSafeKey> {
            Ok(LessSafeKey::new(
                UnboundKey::new(&aead::AES_256_GCM, key)
                    .map_err(|_| "invalid peer encryption key")?,
            ))
        };
        let (send, receive) = if requester { (first, second) } else { (second, first) };
        Ok(Self { send: key(&send)?, receive: key(&receive)?, sent: 0, received: 0 })
    }
    fn seal(&mut self, kind: u8, data: &[u8]) -> Result<Vec<u8>> {
        let next = self.sent.checked_add(1).ok_or("peer encryption counter exhausted")?;
        let mut encrypted = vec![kind];
        encrypted.extend_from_slice(data);
        // Random per-message nonces keep confidentiality independent of AP-chosen
        // request IDs. Counters authenticate ordering, including retransmitted frames.
        let mut nonce = [0u8; 12];
        SystemRandom::new().fill(&mut nonce).map_err(|_| "peer nonce generation failed")?;
        self.send
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.sent.to_le_bytes()),
                &mut encrypted,
            )
            .map_err(|_| "peer encryption failed")?;
        self.sent = next;
        let mut framed = nonce.to_vec();
        framed.extend_from_slice(&encrypted);
        Ok(framed)
    }
    fn open(&mut self, mut encrypted: Vec<u8>) -> Result<(u8, Vec<u8>)> {
        let next = self.received.checked_add(1).ok_or("peer encryption counter exhausted")?;
        let frame_len = encrypted.len();
        if frame_len < 29 {
            return Err("truncated private peer message".into());
        }
        let nonce: [u8; 12] = encrypted[..12]
            .try_into()
            .expect("checked fixed-width field has the destination width");
        let plain = self
            .receive
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.received.to_le_bytes()),
                &mut encrypted[12..],
            )
            .map_err(|_| {
                format!(
                    "private peer message authentication failed at frame {} ({} bytes)",
                    self.received, frame_len
                )
            })?;
        let (&kind, data) = plain.split_first().ok_or("empty private peer message")?;
        let data = data.to_vec();
        self.received = next;
        Ok((kind, data))
    }
}
async fn send(stream: &mut ApStream, cipher: &mut Cipher, kind: u8, data: &[u8]) -> Result<()> {
    wire::send_data(stream, &cipher.seal(kind, data)?).await
}
async fn send_data(stream: &mut ApStream, cipher: &mut Cipher, data: &[u8]) -> Result<()> {
    for chunk in data.chunks(CHUNK) {
        send(stream, cipher, DATA, chunk).await?;
    }
    Ok(())
}
async fn receive(stream: &mut ApStream, cipher: &mut Cipher) -> Result<(u8, Vec<u8>)> {
    let encrypted = tokio::time::timeout(IDLE, wire::receive_data(stream)).await??;
    let (kind, data) = cipher.open(encrypted)?;
    if kind == ERROR {
        return Err(failure::Failure::decode(&data)?.into());
    }
    Ok((kind, data))
}
fn error(error: impl std::fmt::Display) -> crate::store::Error {
    crate::store::Error::System(error.to_string())
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Offer a retained, locally initialized session. Abort the returned task when
/// disconnecting; all reply streams use this same AP connection.
pub async fn listen(
    connection: Arc<ApConnection>,
    store: Arc<LocalStore>,
    session: Session,
) -> Result<JoinHandle<()>> {
    if session.shared_volume.is_some() {
        return Err("Shared volumes require hosted replication".into());
    }
    let own = Access::session(store.account().id, &session.id);
    store.authorize_replication(&own, session.replica_volume(), Permission::Read)?;
    let request = ServiceRequest::PeerListen {
        operation_id: Id::generate()?.to_string(),
        session_id: session.id.clone(),
        build_id: session.resolved_build_id.clone(),
        proof: store.session_proof(&session.id, store.account().id)?,
    };
    let mut stream = wire::begin(&connection, &request).await?;
    wire::response(&mut stream, request.operation_id()).await?;
    let runtime = Handle::current();
    let responses = response_slots();
    Ok(tokio::spawn(async move {
        loop {
            let event = match wire::receive_data(&mut stream)
                .await
                .and_then(|bytes| offered_event(&bytes, &session.id, store.account().id))
            {
                Ok(event) => event,
                Err(error) => {
                    // This task owns the offered stream, not a local input. End
                    // this offer; a future explicit sync may install a new one.
                    // The requesting peer receives stream failure/unavailability.
                    crate::diagnostic!("Private database offer ended for {}: {error}", session.id);
                    break;
                }
            };
            let connection = connection.clone();
            let store = store.clone();
            let runtime = runtime.clone();
            let credit = responses
                .clone()
                .acquire_owned()
                .await
                .expect("private response admission is never closed");
            tokio::task::spawn_blocking(move || {
                let _credit = credit;
                if let Err(error) = respond(&runtime, &connection, &store, &event) {
                    crate::diagnostic!("Private database transfer ended: {error}");
                }
            });
        }
    }))
}

fn offered_event(bytes: &[u8], session: &str, account: Id) -> Result<PeerEvent> {
    let event: PeerEvent = serde_json::from_slice(bytes)?;
    if event.session_id != session || event.responder_account_id != account.to_string() {
        return Err("private peer offer scope differs".into());
    }
    Ok(event)
}

/// Receive authenticated private bytes into a caller-owned ReceivedVolume
/// database. The caller reserves the bootstrap job and supplies the matching
/// volume/database destination. READY and encrypted receipt remain mandatory;
/// no live store merge or session admission occurs here. The coordinator must
/// freshly authorize and adopt the complete volume before treating it as ready.
pub fn pull_into(
    runtime: &Runtime,
    connection: &ApConnection,
    store: &LocalStore,
    session: &Session,
    volume: Id,
    database: crate::store::DatabaseId,
    target: &Database,
) -> Result<ReplicaReceipt> {
    let key = validate_target(store, volume, database, target)?;
    let (mut stream, mut cipher, event) = begin_request(
        runtime,
        connection,
        store,
        session,
        volume,
        database,
        SyncDirection::Pull,
        key,
    )?;
    let result = runtime.block_on(async {
        ready(&mut stream, &mut cipher).await?;
        pull(
            &store.profile_path().join(crate::paths::STAGING),
            target,
            key,
            &mut stream,
            &mut cipher,
        )
        .await?;
        let mut received = receipt(&mut stream, &mut cipher).await?;
        if target.meta().hash_key != key {
            let sender = SyncSender::new(target)?;
            if super::remote::world::wire_root(&sender, key).await? != received.durable.root {
                return Err("credentialed peer receipt differs from delivered World data".into());
            }
            received.durable.root = sender.root_hash()?;
        }
        Ok(received)
    });
    failure::qualify(result, store, session, &event)
}

/// Publish the current authorized view and wait for the encrypted receiver's
/// durable receipt. Run outside the runtime's async context.
pub fn push(
    runtime: &Runtime,
    connection: &ApConnection,
    store: &LocalStore,
    session: &Session,
    volume: Id,
    database: crate::store::DatabaseId,
) -> Result<DurableRevision> {
    let key = volume_key(store, volume, database)?;
    let (mut stream, mut cipher, event) = begin_request(
        runtime,
        connection,
        store,
        session,
        volume,
        database,
        SyncDirection::Push,
        key,
    )?;
    let projected = store.with_authorized_replica(
        &Access::session_version(store.account().id, &session.id, &session.resolved_build_id),
        volume,
        database,
        None,
        |db, policy| {
            outbound_replica(
                &store.profile_path().join(crate::paths::STAGING),
                db,
                policy,
                ReplicaReader::Participant(Id::parse(&event.responder_account_id)?),
            )
        },
    )?;
    runtime.block_on(async {
        send(&mut stream, &mut cipher, READY, &[]).await?;
        send_replica(projected.database(), key, &mut stream, &mut cipher).await?;
        receipt(&mut stream, &mut cipher).await.map(|receipt| receipt.durable)
    })
}

fn volume_key(
    store: &LocalStore,
    volume: Id,
    database: crate::store::DatabaseId,
) -> Result<[u8; 32]> {
    Ok(store.replica_hash_key(volume, database)?)
}
fn validate_target(
    store: &LocalStore,
    volume: Id,
    database: crate::store::DatabaseId,
    target: &Database,
) -> Result<[u8; 32]> {
    let key = volume_key(store, volume, database)?;
    let meta = target.meta();
    if !meta.syncable
        || (meta.hash_key != key
            && (!store.uses_world_projection(volume)?
                || meta.hash_key
                    != database
                        .hash_key(&crate::store::DatabaseId::world_projection_key(volume), volume)))
    {
        return Err("private peer target volume key mismatch".into());
    }
    Ok(key)
}
#[allow(
    clippy::too_many_arguments,
    reason = "Keep the explicit boundary inputs together without introducing a one-use parameter object."
)]
fn begin_request(
    runtime: &Runtime,
    connection: &ApConnection,
    store: &LocalStore,
    session: &Session,
    volume: Id,
    database: crate::store::DatabaseId,
    direction: SyncDirection,
    database_key: [u8; 32],
) -> Result<(ApStream, Cipher, PeerEvent)> {
    if session.shared_volume.is_some() {
        return Err("Shared volumes require hosted replication".into());
    }
    let request = ServiceRequest::PeerRequest {
        operation_id: Id::generate()?.to_string(),
        session_id: session.id.clone(),
        build_id: session.resolved_build_id.clone(),
        proof: store.session_proof(&session.id, store.account().id)?,
        volume_id: volume.to_string(),
        database_id: database,
        direction,
    };
    let mut stream = runtime.block_on(wire::begin(connection, &request))?;
    let response = runtime.block_on(wire::response(&mut stream, request.operation_id()))?;
    let event: PeerEvent =
        serde_json::from_value(response.data.ok_or("missing peer transfer scope")?)?;
    validate_event(&event, store.account().id, session, volume, database, direction)?;
    let cipher = Cipher::new(database_key, &event, true)?;
    Ok((stream, cipher, event))
}

fn validate_event(
    event: &PeerEvent,
    account: Id,
    session: &Session,
    volume: Id,
    database: crate::store::DatabaseId,
    direction: SyncDirection,
) -> Result<()> {
    if event.requester_account_id != account.to_string()
        || event.session_id != session.id
        || event.build_id != session.resolved_build_id
        || event.volume_id != volume.to_string()
        || event.database_id != database
        || event.direction != direction
        || Id::parse(&event.request_id).is_err()
        || Id::parse(&event.responder_account_id).is_err()
    {
        return Err("AP peer transfer scope mismatch".into());
    }
    Ok(())
}

async fn ready(stream: &mut ApStream, cipher: &mut Cipher) -> Result<()> {
    let (kind, data) = receive(stream, cipher).await?;
    if kind != READY || !data.is_empty() {
        return Err("private peer sender is not ready".into());
    }
    Ok(())
}
async fn receipt(stream: &mut ApStream, cipher: &mut Cipher) -> Result<ReplicaReceipt> {
    let (kind, data) = receive(stream, cipher).await?;
    if kind != RECEIPT {
        return Err("missing peer synchronization receipt".into());
    }
    let receipt = serde_json::from_slice(&data)?;
    stream.finish()?;
    Ok(receipt)
}

fn respond(
    runtime: &Handle,
    connection: &ApConnection,
    store: &LocalStore,
    event: &PeerEvent,
) -> Result<()> {
    let account = Id::parse(&event.requester_account_id)?;
    let volume = Id::parse(&event.volume_id)?;
    let request = ServiceRequest::PeerReply {
        operation_id: Id::generate()?.to_string(),
        request_id: event.request_id.clone(),
    };
    let mut stream = runtime.block_on(wire::begin(connection, &request))?;
    runtime.block_on(wire::response(&mut stream, request.operation_id()))?;
    // Only the trusted runtime obtains the database key; it never crosses AP.
    let key = volume_key(store, volume, event.database_id)?;
    // A participant may have the catalog record without a local backing yet.
    // Open its local replica before the existing per-request authorization and transfer.
    // The local tree's hash key carries no peer encryption authority.
    store.with_replica(volume, event.database_id, |_| Ok(()))?;
    let mut cipher = Cipher::new(key, event, false)?;
    let access = Access::session_version(account, &event.session_id, &event.build_id);
    let result = match event.direction {
        SyncDirection::Pull => respond_pull(
            runtime,
            store,
            &access,
            volume,
            event.database_id,
            key,
            account,
            &mut stream,
            &mut cipher,
        ),
        SyncDirection::Push => {
            runtime.block_on(ready(&mut stream, &mut cipher))?;
            respond_push(
                runtime,
                store,
                &access,
                volume,
                event.database_id,
                key,
                account,
                &mut stream,
                &mut cipher,
            )
        }
    };
    runtime.block_on(async {
        match result {
            Ok(receipt) => {
                send(&mut stream, &mut cipher, RECEIPT, &serde_json::to_vec(&receipt)?).await?
            }
            Err(error) => send(&mut stream, &mut cipher, ERROR, &failure::encode(&error)?).await?,
        }
        stream.finish()?;
        Ok(())
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit transport and current participant admission inputs together."
)]
fn respond_pull(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    volume: Id,
    database: crate::store::DatabaseId,
    key: [u8; 32],
    account: Id,
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> crate::store::Result<ReplicaReceipt> {
    let (projected, mut receipt) = authorized_snapshot(store, access, volume, database, account)?;
    runtime
        .block_on(async {
            send(stream, cipher, READY, &[]).await?;
            receipt.durable.root = send_replica(projected.database(), key, stream, cipher).await?;
            Ok::<ReplicaReceipt, Box<dyn std::error::Error + Send + Sync>>(receipt)
        })
        .map_err(error)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit transport and current participant admission inputs together."
)]
fn respond_push(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    volume: Id,
    database: crate::store::DatabaseId,
    key: [u8; 32],
    account: Id,
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> crate::store::Result<ReplicaReceipt> {
    let staged = store.with_authorized_replica(access, volume, database, None, |db, _| {
        detached_replica(&store.profile_path().join(crate::paths::STAGING), db)
    })?;
    runtime
        .block_on(pull(
            &store.profile_path().join(crate::paths::STAGING),
            staged.database(),
            key,
            stream,
            cipher,
        ))
        .map_err(error)?;
    store.merge_authorized_replica(access, volume, database, staged.database())?;
    let (projected, mut receipt) = authorized_snapshot(store, access, volume, database, account)?;
    receipt.durable.root = runtime
        .block_on(async {
            let sender = SyncSender::new(projected.database())?;
            if projected.database().meta().hash_key == key {
                return Ok(sender.root_hash()?);
            }
            super::remote::world::wire_root(&sender, key).await
        })
        .map_err(error)?;
    Ok(receipt)
}

/// Capture the authorized bytes, clock and policy at one cut. Finish the receipt
/// in the credentialed wire context from that same immutable view.
fn authorized_snapshot(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    database: crate::store::DatabaseId,
    account: Id,
) -> crate::store::Result<(crate::store::OutboundReplica, ReplicaReceipt)> {
    store.with_authorized_replica(access, volume, database, Some(Permission::Read), |db, policy| {
        let projected = outbound_replica(
            &store.profile_path().join(crate::paths::STAGING),
            db,
            policy,
            ReplicaReader::Participant(account),
        )?;
        let receipt = ReplicaReceipt {
            namespace: crate::store::volume_index::namespace::receipt::Receipt::Unavailable,
            source: store.source_snapshot_token(volume)?,
            durable: DurableRevision {
                // Filled from the exact authorized outgoing snapshot.
                root: None,
                revision: db.clock().as_u64(),
                policy_revision: policy.volume.revision(),
            },
        };
        Ok((projected, receipt))
    })
}

async fn send_replica(
    db: &Database,
    key: [u8; 32],
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> Result<crate::store::ReplicaRoot> {
    let mut sender = SyncSender::new(db)?;
    if db.meta().hash_key != key {
        let root = super::remote::world::wire_root(&sender, key).await?;
        sender.use_projection(key, root)?;
    }
    let mut done = false;
    loop {
        let (kind, input) = receive(stream, cipher).await?;
        if kind == FINISHED {
            if !done || !input.is_empty() {
                return Err("premature private peer completion".into());
            }
            return Ok(sender.root_hash()?);
        }
        if kind != DATA {
            return Err("invalid private database message".into());
        }
        done = sender.process(&input)? == SyncStatus::Done;
        let mut output = vec![0; CHUNK];
        loop {
            let Some(length) = sender.try_write_output(&mut output)? else {
                tokio::task::yield_now().await;
                continue;
            };
            if length == 0 {
                break;
            }
            send_data(stream, cipher, &output[..length]).await?;
        }
    }
}
async fn pull(
    scratch: &std::path::Path,
    db: &Database,
    key: [u8; 32],
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> Result<()> {
    let mut output = Vec::new();
    let staging = crate::store::sync_staging(scratch)?;
    let mut receiver = if db.meta().hash_key == key {
        SyncReceiver::new(db, Lww, staging, now(), &mut output)?
    } else {
        SyncReceiver::new_projection(db, key, Lww, staging, now(), &mut output)?
    };
    loop {
        send_data(stream, cipher, &output).await?;
        output.clear();
        let (kind, input) = receive(stream, cipher).await?;
        if kind != DATA {
            return Err("invalid private database message".into());
        }
        let done = receiver.process(&input, &mut output)? == SyncStatus::Done;
        if done {
            send_data(stream, cipher, &output).await?;
            receiver.finish()?;
            send(stream, cipher, FINISHED, &[]).await?;
            return Ok(());
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    fn event() -> PeerEvent {
        PeerEvent {
            build_id: crate::identifier::encode(&[0xab; 32]),
            request_id: crate::identifier::encode(&[0x11; 16]),
            requester_account_id: crate::identifier::encode(&[0x22; 16]),
            responder_account_id: crate::identifier::encode(&[0x33; 16]),
            session_id: crate::identifier::encode(&[0x44; 32]),
            volume_id: crate::identifier::encode(&[0x55; 16]),
            database_id: crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
            ),
            direction: SyncDirection::Pull,
        }
    }
    #[test]
    fn malformed_or_foreign_offer_never_reaches_a_response_worker() {
        let valid = event();
        let bytes = serde_json::to_vec(&valid).unwrap();
        let account = Id::parse(&valid.responder_account_id).unwrap();
        assert!(offered_event(b"invalid JSON", &valid.session_id, account).is_err());
        assert!(offered_event(&bytes, &crate::identifier::encode(&[0xff; 32]), account).is_err());
        assert!(offered_event(&bytes, &valid.session_id, Id::from_bytes([9; 16])).is_err());
        let accepted = offered_event(&bytes, &valid.session_id, account).unwrap();
        assert_eq!(accepted.request_id, valid.request_id);
    }

    #[test]
    fn ciphertext_is_bound_to_transfer_scope_direction_and_order() {
        let event = event();
        let mut requester = Cipher::new([7; 32], &event, true).unwrap();
        let mut responder = Cipher::new([7; 32], &event, false).unwrap();
        let first = requester.seal(DATA, b"private database text").unwrap();
        assert!(!first.windows(8).any(|part| part == b"database"));
        assert_eq!(
            responder.open(first.clone()).unwrap(),
            (DATA, b"private database text".to_vec())
        );
        assert!(responder.open(first.clone()).is_err());
        assert!(requester.open(first.clone()).is_err());
        let mut wrong_key = Cipher::new([8; 32], &event, false).unwrap();
        assert!(wrong_key.open(first.clone()).is_err());
        let mut wrong_event = event.clone();
        wrong_event.build_id = crate::identifier::encode(&[0xcd; 32]);
        assert!(Cipher::new([7; 32], &wrong_event, false).unwrap().open(first.clone()).is_err());
        let mut wrong_event = event.clone();
        wrong_event.database_id = crate::store::DatabaseId::Revocations;
        assert!(Cipher::new([7; 32], &wrong_event, false).unwrap().open(first.clone()).is_err());
        let mut corrupt = first;
        corrupt[0] ^= 1;
        assert!(Cipher::new([7; 32], &event, false).unwrap().open(corrupt).is_err());
        let reply = responder.seal(RECEIPT, b"durable").unwrap();
        assert_eq!(requester.open(reply).unwrap(), (RECEIPT, b"durable".to_vec()));
        let large = requester.seal(DATA, &vec![7; CHUNK]).unwrap();
        assert_eq!(large.len(), 256 * 1024, "one ciphertext must fit one AP data frame");
        assert_eq!(responder.open(large).unwrap().1, vec![7; CHUNK]);
    }
    #[test]
    fn scoped_peer_reply_cannot_redirect_a_staged_pull() {
        let event = event();
        let account = Id::parse(&event.requester_account_id).unwrap();
        let volume = Id::parse(&event.volume_id).unwrap();
        let session = Session {
            id: event.session_id.clone(),
            selector: crate::store::BuildSelector::Exact { build_id: event.build_id.clone() },
            resolved_build_id: event.build_id.clone(),
            root_volume: volume,
            shared_volume: None,
            app: crate::package::AppIdentity { publisher: account, app_id: "test".into() },

            creator: account,
            update_authority: crate::store::UpdateAuthority::Creator,
        };
        validate_event(&event, account, &session, volume, event.database_id, SyncDirection::Pull)
            .unwrap();
        for field in 0..8 {
            let mut changed = event.clone();
            match field {
                0 => changed.requester_account_id = crate::identifier::encode(&[0x99; 16]),
                1 => changed.session_id = crate::identifier::encode(&[0x99; 32]),
                2 => changed.build_id = crate::identifier::encode(&[0x99; 32]),
                3 => changed.volume_id = crate::identifier::encode(&[0x99; 16]),
                4 => changed.database_id = crate::store::DatabaseId::Index,
                5 => changed.direction = SyncDirection::Push,
                6 => changed.request_id = "invalid".into(),
                7 => changed.responder_account_id = "invalid".into(),
                _ => unreachable!(),
            }
            assert!(
                validate_event(
                    &changed,
                    account,
                    &session,
                    volume,
                    event.database_id,
                    SyncDirection::Pull
                )
                .is_err()
            );
        }
    }

    #[test]
    fn staged_pull_binding_never_creates_or_changes_live_named_data() {
        use crate::store::{
            DatabaseId, EntryId,
            volume_index::{self, DatabaseProtocol, EntryInfo, EntryKind},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Private fixture", false).unwrap();
        let received = store.prepare_received_volume(volume.id).unwrap();
        store
            .with_replica(volume.id, DatabaseId::Index, |source| {
                received.with_database(DatabaseId::Index, |target| {
                    crate::store::merge_index_replica(
                        &std::env::temp_dir(),
                        source,
                        target,
                        Lww,
                        |_| Ok(()),
                    )
                })
            })
            .unwrap();
        let entry = EntryId::from_bytes([3; 15]).unwrap();
        let database = DatabaseId::Entry(entry);
        received
            .with_database(DatabaseId::Index, |index| {
                let mut tx = index.begin_write(now())?;
                volume_index::create(
                    &mut tx,
                    entry,
                    &EntryInfo {
                        namespace: Default::default(),
                        origin: crate::store::volume_index::namespace::Origin {
                            installation: crate::store::Id::from_bytes([42; 16]),
                            label: None,
                        },
                        initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                        created_revision: EntryId::generate()?,
                        name: "received".into(),
                        kind: EntryKind::KvDb {},
                        protocol: Some(DatabaseProtocol::Kvdb),
                    },
                )?;
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        let live = directory.path().join(volume.id.to_string()).join(database.file_name());
        let before = store.replica_root(volume.id, DatabaseId::Index).unwrap();
        received
            .with_database(database, |target| {
                assert_eq!(
                    validate_target(&store, volume.id, database, target).unwrap(),
                    target.meta().hash_key
                );
                assert!(!live.exists(), "deriving a peer key cannot initialize the live backing");
                let mut tx = target.begin_write(now())?;
                tx.put(b"fixture", b"received bytes stay detached")?;
                tx.commit()?;
                let other = store.create_volume("Unrelated", false)?;
                assert!(validate_target(&store, other.id, database, target).is_err());
                assert!(
                    validate_target(
                        &store,
                        volume.id,
                        DatabaseId::Entry(EntryId::from_bytes([4; 15]).unwrap()),
                        target
                    )
                    .is_err()
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(store.replica_root(volume.id, DatabaseId::Index).unwrap(), before);
        assert!(!live.exists());
    }
}
