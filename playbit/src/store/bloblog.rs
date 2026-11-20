//! Opaque append-only logs. Entry identity travels; arrival clocks are local.
//! AP preserves supplied payload bytes. Clients/peers can optionally encrypt
//! stored entry values through `Log::encryption_key`. Wire entries are unchanged;
//! callers own payload confidentiality and cryptographic authenticity over AP.
//!
//! A cursor belongs to a volume, log and replica incarnation. A different peer
//! replays from the beginning, so an arbitrarily delayed offline edit cannot be
//! hidden behind another peer's clock. No clock-skew or delivery-delay assumption.
use crate::store::{EntryId, Error, Id, Result};
use serde::{Deserialize, Serialize};

#[cfg(any(target_arch = "wasm32", feature = "local-store"))]
mod storage;
#[cfg(any(target_arch = "wasm32", feature = "local-store"))]
pub use storage::Log;

pub use playbit_api::abi::bloblog::{MAX_BLOB_BYTES, MAX_BLOBS, MAX_BYTES};
pub const INLINE_BYTES: usize = 2048;

/// Canonical metadata is independent of the connection and request correlation ID.
pub fn append_entries(
    account: Id,
    volume: Id,
    log: EntryId,
    append_id: Id,
    blobs: &[Vec<u8>],
) -> Result<Vec<Entry>> {
    validate_blobs(blobs)?;
    let mut scope = blake3::Hasher::new_derive_key("app.playbit.bloblog.append.v1");
    scope.update(account.as_bytes());
    scope.update(volume.as_bytes());
    scope.update(log.as_bytes());
    scope.update(append_id.as_bytes());
    let mut entries = Vec::with_capacity(blobs.len());
    for (index, data) in blobs.iter().enumerate() {
        let mut hash = scope.clone();
        hash.update(&(index as u32).to_le_bytes());
        let nonce = Id::from_bytes(hash.finalize().as_bytes()[..16].try_into().expect("ID width"));
        // Arrival clocks provide ordering; the canonical origin clock is fixed.
        let clock = 1;
        entries.push(Entry {
            log,
            id: identity(log, clock, nonce, data),
            clock,
            nonce,
            data: data.clone(),
        });
    }
    Ok(entries)
}

pub fn append_identity(entries: &[Entry]) -> Result<[u8; 32]> {
    playbit_api::abi::bloblog::validate_lengths(entries.iter().map(|entry| entry.data.len()))
        .map_err(|_| Error::LimitExceeded)?;
    let mut hash = blake3::Hasher::new_derive_key("app.playbit.bloblog.payload.v1");
    hash.update(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        hash.update(&(entry.data.len() as u32).to_le_bytes());
        hash.update(&entry.data);
    }
    Ok(*hash.finalize().as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Entry {
    pub log: EntryId,
    pub id: Id,
    pub clock: u64,
    pub nonce: Id,
    pub data: Vec<u8>,
}
impl Entry {
    /// Recovery creates a new log domain, preserving source payload and origin.
    /// The resulting append needs its own first durable hosted publication.
    #[cfg(any(target_arch = "wasm32", feature = "local-store"))]
    pub(crate) fn recover_into(mut self, log: EntryId) -> Self {
        self.log = log;
        self.id = identity(log, self.clock, self.nonce, &self.data);
        self
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[cfg_attr(feature = "app-host", rkyv(bytecheck(verify)))]
pub struct Cursor(String);
impl Cursor {
    pub fn parse(text: &str) -> Result<Self> {
        Self::validate(text)?;
        Ok(Self(text.into()))
    }
    pub(crate) fn validate(text: &str) -> Result<()> {
        Self::fields(text).map(|_| ())
    }
    fn fields(text: &str) -> Result<(Id, EntryId, u64, u64, Id)> {
        if text.len() > playbit_api::abi::bloblog::MAX_CURSOR_BYTES {
            return Err(Error::InvalidInput("invalid bloblog cursor"));
        }
        let fields = text.split('/').collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "1" {
            return Err(Error::InvalidInput("invalid bloblog cursor version or fields"));
        }
        let replica = crate::identifier::decode::<8>(fields[3]).map_err(Error::InvalidInput)?;
        let clock: u64 =
            fields[4].parse().map_err(|_| Error::InvalidInput("invalid cursor clock"))?;
        if clock.to_string() != fields[4] {
            return Err(Error::InvalidInput("invalid cursor clock"));
        }
        Ok((
            Id::parse(fields[1])?,
            EntryId::parse(fields[2])?,
            u64::from_be_bytes(replica),
            clock,
            Id::parse(fields[5])?,
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[cfg(any(
        target_arch = "wasm32",
        feature = "presentation",
        feature = "wasm-runtime",
        feature = "native-runtime",
        feature = "connected-engine",
        feature = "desktop-ui"
    ))]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.0.capacity()
    }
}

#[cfg(any(
    target_arch = "wasm32",
    feature = "wasm-runtime",
    feature = "native-runtime",
    feature = "desktop-ui"
))]
impl Cursor {
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    pub(crate) fn matches_log(&self, volume: Id, log: EntryId) -> bool {
        Self::fields(self.as_str()).is_ok_and(|(v, l, ..)| v == volume && l == log)
    }
}

#[cfg(feature = "app-host")]
impl ArchivedCursor {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
// A cursor arrives over the wire (AP JSON pages, replicated bloblog events) and is
// then persisted verbatim as a durable checkpoint. Validate on the way in so a
// malformed cursor is rejected here instead of wedging sync once it is stored.
impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Cursor::parse(&text).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Page {
    pub entries: Vec<Entry>,
    pub cursor: Cursor,
    pub more: bool,
}

/// Source qualification is independent of append identity and grants no access
/// or AP durability. A complete volume bootstrap checks every page against its cut.
#[derive(Debug, Serialize, Deserialize)]
#[serde(try_from = "ReplicaPageWire")]
pub struct ReplicaPage {
    pub page: Page,
    pub source: crate::store::SnapshotToken,
    pub policy_revision: u64,
    pub database_clock: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaPageWire {
    page: Page,
    source: crate::store::SnapshotToken,
    policy_revision: u64,
    database_clock: u64,
}
impl TryFrom<ReplicaPageWire> for ReplicaPage {
    type Error = Error;
    fn try_from(wire: ReplicaPageWire) -> Result<Self> {
        crate::store::SnapshotCut::new(wire.source, wire.policy_revision)?;
        Ok(Self {
            page: wire.page,
            source: wire.source,
            policy_revision: wire.policy_revision,
            database_clock: wire.database_clock,
        })
    }
}
impl ReplicaPage {
    pub fn cut(&self) -> Result<crate::store::SnapshotCut> {
        crate::store::SnapshotCut::new(self.source, self.policy_revision)
    }
}

pub fn validate_blobs(blobs: &[Vec<u8>]) -> Result<()> {
    if blobs.is_empty()
        || blobs.len() > MAX_BLOBS
        || blobs.iter().any(|b| b.len() > MAX_BLOB_BYTES)
        || 4 + blobs.iter().map(|b| 59 + b.len()).sum::<usize>() > MAX_BYTES
    {
        return Err(Error::InvalidInput(
            "bloblog batch requires 1–128 blobs, at most 32 KiB each and 64 KiB encoded",
        ));
    }
    Ok(())
}
pub fn encode(entries: &[Entry]) -> Result<Vec<u8>> {
    if entries.len() > MAX_BLOBS {
        return Err(Error::LimitExceeded);
    }
    let size = entries
        .iter()
        .try_fold(4usize, |size, entry| size.checked_add(59)?.checked_add(entry.data.len()))
        .filter(|size| *size <= MAX_BYTES)
        .ok_or(Error::LimitExceeded)?;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        if entry.data.len() > MAX_BLOB_BYTES || entry.clock == 0 || entry.clock > u64::MAX - 2 {
            return Err(Error::InvalidInput("invalid bloblog entry"));
        }
        out.extend_from_slice(entry.log.as_bytes());
        out.extend_from_slice(entry.id.as_bytes());
        out.extend_from_slice(&entry.clock.to_be_bytes());
        if entry.id != identity(entry.log, entry.clock, entry.nonce, &entry.data) {
            return Err(Error::corrupt("bloblog entry digest does not match its identity"));
        }
        out.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.nonce.as_bytes());
        out.extend_from_slice(&entry.data);
    }
    Ok(out)
}
pub fn decode(mut bytes: &[u8]) -> Result<Vec<Entry>> {
    if bytes.len() < 4 || bytes.len() > MAX_BYTES {
        return Err(Error::InvalidInput("invalid bloblog batch size"));
    }
    let count = u32::from_le_bytes(
        bytes[..4].try_into().expect("checked fixed-width field has the destination width"),
    ) as usize;
    bytes = &bytes[4..];
    if count > MAX_BLOBS {
        return Err(Error::LimitExceeded);
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.len() < 59 {
            return Err(Error::corrupt("bloblog entry header is truncated"));
        }
        let length = u32::from_le_bytes(
            bytes[39..43].try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        let clock = u64::from_be_bytes(
            bytes[31..39].try_into().expect("checked fixed-width field has the destination width"),
        );
        if length > MAX_BLOB_BYTES
            || bytes.len() < 59 + length
            || clock == 0
            || clock > u64::MAX - 2
        {
            return Err(Error::corrupt("bloblog entry has an invalid length or clock"));
        }
        entries.push(Entry {
            log: EntryId::from_bytes(
                bytes[..15]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            )?,
            id: Id::from_bytes(
                bytes[15..31]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            ),
            clock,
            nonce: Id::from_bytes(
                bytes[43..59]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            ),
            data: bytes[59..59 + length].to_vec(),
        });
        bytes = &bytes[59 + length..];
    }
    if !bytes.is_empty() {
        return Err(Error::corrupt("bloblog has trailing bytes"));
    }
    for e in &entries {
        if e.id != identity(e.log, e.clock, e.nonce, &e.data) {
            return Err(Error::corrupt("bloblog entry digest does not match its identity"));
        }
    }
    Ok(entries)
}

fn identity(log: EntryId, clock: u64, nonce: Id, data: &[u8]) -> Id {
    let mut hash = blake3::Hasher::new_derive_key("app.playbit.bloblog.identity.v1");
    hash.update(log.as_bytes());
    hash.update(&clock.to_be_bytes());
    hash.update(nonce.as_bytes());
    hash.update(data);
    Id::from_bytes(
        hash.finalize().as_bytes()[..16]
            .try_into()
            .expect("checked fixed-width field has the destination width"),
    )
}

#[cfg(test)]
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::storage::entry_key;
    use super::*;
    use crate::store::{Access, LocalStore, create_replica};
    use crate::store::{Database, now_unix_secs};
    use kvdb::Clock;
    #[cfg(feature = "app-host")]
    #[test]
    fn archived_cursor_rejects_invalid_contents_before_deserialization() {
        use crate::engine::{Request, ipc::codec};
        for text in [
            String::new(),
            "01".into(),
            "02".to_owned() + &"0".repeat(126),
            "01".to_owned() + &"g".repeat(126),
            "01".to_owned() + &"0".repeat(126),
        ] {
            let valid = Cursor::parse(&text).is_ok();
            // Construct an invalid value inside its defining module to model
            // a hostile but structurally well-formed incoming archive.
            let request = Request::Syscall {
                context: crate::engine::service_call::Context::Tool {
                    handle: id(3),
                    incarnation: id(4),
                },
                operation: 1,
                mutation: 0,
                call: Box::new(crate::runtime::host::DataCall {
                    volume: *id(1).as_bytes(),
                    operation: crate::runtime::host::DataOperation::BloblogRead {
                        log: [2; 15],
                        cursor: Some(Cursor(text)),
                    },
                }),
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            assert_eq!(codec::access::<crate::engine::ArchivedRequest>(&bytes).is_ok(), valid);
            assert_eq!(
                rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).is_ok(),
                valid
            );
        }
    }
    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }
    fn lid(n: u8) -> EntryId {
        EntryId::from_bytes([n; 15]).unwrap()
    }
    fn database(dir: &std::path::Path, name: &str) -> Database {
        create_replica(&dir.join(name), [7; 32], 512 * 1024).unwrap()
    }
    // Bounded validation must inspect every record, including corrupt tails
    // beyond its first page, and preserve the existing arrival/pending layout.
    #[test]
    fn paged_validation_covers_every_record_and_rejects_a_corrupt_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "paged");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        log.put(&vec![vec![7]; 40]).unwrap();
        let mut after = None;
        let mut pages = 0;
        loop {
            after = log.validate_page(after.as_deref(), 64).unwrap();
            pages += 1;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(pages, 2);
        let mut tx = db.begin_write(log.now).unwrap();
        tx.put(b"z/corrupt-tail", b"invalid").unwrap();
        tx.commit().unwrap();
        let after = log.validate_page(None, 64).unwrap().unwrap();
        assert!(log.validate_page(Some(&after), 64).is_err());
        assert!(log.validate_page(None, 0).is_err());
        assert!(log.validate_page(None, 65).is_err());
    }
    // Wire pages must carry exact uint64 source evidence. Old, unknown or
    // invalid identity shapes cannot qualify a volume cut.
    #[test]
    fn replica_page_requires_complete_exact_source_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "source");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        let receipt = ReplicaPage {
            page: log.get(None).unwrap(),
            source: crate::store::SnapshotToken { generation: id(3), epoch: u64::MAX },
            policy_revision: 1,
            database_clock: u64::MAX,
        };
        let encoded = serde_json::to_value(receipt).unwrap();
        let decoded: ReplicaPage = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.source.epoch, u64::MAX);
        assert_eq!(decoded.database_clock, u64::MAX);
        for case in 0..4 {
            let mut wire = encoded.clone();
            match case {
                0 => {
                    wire.as_object_mut().unwrap().remove("source");
                }
                1 => wire["grant"] = serde_json::json!(true),
                2 => wire["source"]["generation"] = serde_json::json!(id(0)),
                3 => wire["policy_revision"] = serde_json::json!(0),
                _ => unreachable!(),
            }
            assert!(serde_json::from_value::<ReplicaPage>(wire).is_err());
        }
    }

    // A detached publication cannot accept orphaned arrival/pending pointers
    // or records for a different log. Walk storage with bounded row state.
    #[test]
    fn log_validation_rejects_unknown_and_inconsistent_storage() {
        for case in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let db = database(dir.path(), "log");
            let log = Log {
                database: &db,
                volume: id(1),
                entry: lid(2),
                encryption_key: None,
                now: crate::store::now_unix_secs(),
            };
            let (_, entries) = log.put(&[b"entry".to_vec()]).unwrap();
            log.validate().unwrap();
            let entry = &entries[0];
            let mut tx = db.begin_write(now_unix_secs()).unwrap();
            let key = entry_key(entry);
            match case {
                0 => tx.put(b"unknown", b"value").unwrap(),
                1 => {
                    tx.del(key.as_bytes()).unwrap();
                }
                2 => {
                    tx.put(format!("p/{}/{}", lid(3), entry.id).as_bytes(), key.as_bytes()).unwrap()
                }
                3 => tx
                    .put(format!("l/{}/{:016x}/{}", lid(2), 0, entry.id).as_bytes(), key.as_bytes())
                    .unwrap(),
                _ => unreachable!(),
            }
            tx.commit().unwrap();
            assert!(log.validate().is_err());
        }
    }
    #[test]
    fn committed_cache_paginates_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let volume;
        let cursor;
        let first;
        let second;
        {
            let store = LocalStore::open(dir.path()).unwrap();
            volume = store.create_volume("Bloblogs", true).unwrap().id;
            let access = Access::owner(store.account().id);
            first = store.create_bloblog(&access, volume, "edits").unwrap().id;
            second = store.create_bloblog(&access, volume, "other").unwrap().id;
            let initial = store.bloblog_get(&access, volume, first, None).unwrap();
            assert!(initial.entries.is_empty());
            cursor = initial.cursor;
            for n in 0..3 {
                let blobs = vec![vec![n; 100]; 128];
                store.bloblog_put(&access, volume, first, &blobs).unwrap();
            }
            store.bloblog_put(&access, volume, second, &[vec![], vec![0, 255, 0]]).unwrap();
            assert!(store.bloblog_get(&access, volume, second, Some(&cursor)).is_err());
            store.rename_entry(&access, volume, first, "renamed/edits").unwrap();
            assert!(store.bloblog_pending(&access, volume, first).unwrap().is_empty());
        }
        let store = LocalStore::open(dir.path()).unwrap();
        let access = Access::owner(store.account().id);
        let mut next = cursor;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = store.bloblog_get(&access, volume, first, Some(&next)).unwrap();
            assert_eq!(page.entries.len(), 128);
            for entry in page.entries {
                assert!(seen.insert(entry.id));
            }
            next = page.cursor;
            if !page.more {
                break;
            }
        }
        assert_eq!(seen.len(), 384);
        assert!(store.bloblog_get(&access, volume, first, Some(&next)).unwrap().entries.is_empty());
        let other = store.bloblog_get(&access, volume, second, None).unwrap();
        assert_eq!(other.entries.len(), 2);
        assert!(other.entries.iter().any(|e| e.data.is_empty()));
    }
    #[test]
    fn delayed_entries_peer_switch_duplicates_and_clock_rollback_cannot_skip_data() {
        let dir = tempfile::tempdir().unwrap();
        let a = database(dir.path(), "a");
        let b = database(dir.path(), "b");
        let a = Log {
            database: &a,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        let b = Log {
            database: &b,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        let (_, old) = a.put(&[b"offline".to_vec()]).unwrap();
        let mut future = old[0].clone();
        future.clock = Clock::now(now_unix_secs() + 3600).as_u64();
        future.data = b"future".to_vec();
        future.id = identity(future.log, future.clock, future.nonce, &future.data);
        b.apply(&[future.clone()], false).unwrap();
        let before = b.get(None).unwrap().cursor;
        b.apply(&old, false).unwrap();
        let late = b.get(Some(&before)).unwrap();
        assert_eq!(late.entries, old);
        assert!(!b.apply(&old, false).unwrap());
        assert!(b.get(Some(&late.cursor)).unwrap().entries.is_empty());
        let own = b.put(&[b"after rollback".to_vec()]).unwrap();
        assert!(own.1[0].clock > future.clock);
        assert_eq!(b.get(Some(&own.0)).unwrap().entries, own.1);
        // Cursor originated on B: A replays its own complete log, even though
        // B's observed clock is an hour ahead of A's offline clock.
        assert_eq!(a.get(Some(&late.cursor)).unwrap().entries, old);
        assert!(Log { volume: id(3), ..a }.get(Some(&late.cursor)).is_err());
        assert!(Log { entry: lid(3), ..b }.get(Some(&late.cursor)).is_err());
    }

    #[test]
    fn detached_pending_discard_is_bounded_and_preserves_accepted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "pending");
        let log =
            Log { database: &db, volume: id(1), entry: lid(2), encryption_key: None, now: 1000 };
        let mut pending = Vec::new();
        for byte in 0..65 {
            pending.extend(log.put(&[vec![byte]]).unwrap().1);
        }
        let (_, accepted) = log.put(&[b"already acknowledged".to_vec()]).unwrap();
        log.acknowledge(&accepted).unwrap();
        assert_eq!(log.discard_pending_page().unwrap(), (64, false));
        log.validate().unwrap();
        assert_eq!(log.discard_pending_page().unwrap(), (1, true));
        assert_eq!(log.discard_pending_page().unwrap(), (0, true));
        assert!(log.pending().unwrap().is_empty());
        assert_eq!(log.get(None).unwrap().entries, accepted);
        // AP may already have accepted an offline append whose ack was lost.
        log.apply(&pending[..1], false).unwrap();
        log.apply(&pending[..1], false).unwrap();
        assert_eq!(log.get(None).unwrap().entries.len(), 2);
        assert!(log.pending().unwrap().is_empty());
        log.validate().unwrap();
    }
    #[test]
    fn opaque_storage_preserves_payloads_and_immutable_wire_identity() {
        let dir = tempfile::tempdir().unwrap();
        // The caller creates ciphertext. Bloblog only sees these bytes.
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &[42; 32]).unwrap(),
        );
        let mut ciphertext = b"caller-owned secret".to_vec();
        key.seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key([1; 12]),
            ring::aead::Aad::empty(),
            &mut ciphertext,
        )
        .unwrap();
        let blobs = vec![
            b"opaque edit".to_vec(),
            vec![],
            vec![0, 255, 0],
            ciphertext.clone(),
            vec![0xAB; MAX_BLOB_BYTES],
        ];
        let entries;
        {
            let db = database(dir.path(), "opaque");
            let log = Log {
                database: &db,
                volume: id(1),
                entry: lid(2),
                encryption_key: None,
                now: crate::store::now_unix_secs(),
            };
            entries = log.put(&blobs).unwrap().1;
            for (entry, payload) in entries.iter().zip(&blobs) {
                assert_eq!(&entry.data, payload);
                let record = entry_key(entry);
                let read = db.begin_read().unwrap();
                let bytes = read.get(record.as_bytes()).unwrap().unwrap();
                assert_eq!(&*bytes, encode(std::slice::from_ref(entry)).unwrap());
                assert_eq!(log.open(&record, &bytes).unwrap(), *entry);
            }
            let entry = &entries[0];
            let record = entry_key(entry);
            let wire = log.seal(entry).unwrap();
            assert!(Log { entry: lid(3), ..log }.seal(entry).is_err());
            assert!(Log { entry: lid(3), ..log }.open(&record, &wire).is_err());
            assert!(log.open("e/other", &wire).is_err());
            assert!(log.open(&record, &encode(&[]).unwrap()).is_err());
            assert!(log.open(&record, &encode(&entries[..2]).unwrap()).is_err());
            for end in 0..wire.len() {
                assert!(decode(&wire[..end]).is_err());
            }
            let mut forged = wire;
            *forged.last_mut().unwrap() ^= 1;
            assert!(decode(&forged).is_err());
            let mut altered = entry.clone();
            altered.data.push(1);
            assert!(log.apply(&[altered], false).is_err());
            assert!(!log.apply(&entries, true).unwrap());
            log.validate().unwrap();
        }
        let db = database(dir.path(), "opaque");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        assert_eq!(log.get(None).unwrap().entries, entries);
        let pending = log.pending().unwrap();
        assert_eq!(pending.len(), entries.len());
        assert!(entries.iter().all(|entry| pending.contains(entry)));
        let mut stored = log.get(None).unwrap().entries[3].data.clone();
        assert_eq!(stored, ciphertext);
        let plain = key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key([1; 12]),
                ring::aead::Aad::empty(),
                &mut stored,
            )
            .unwrap();
        assert_eq!(plain, b"caller-owned secret");
    }
    #[test]
    fn encrypted_storage_binding_and_immutable_wire_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "encrypted");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: Some([42; 32]),
            now: crate::store::now_unix_secs(),
        };
        let (_, entries) = log.put(&[b"unique secret plaintext".to_vec()]).unwrap();
        let entry = &entries[0];
        let key = entry_key(entry);
        let bytes = db.begin_read().unwrap().get(key.as_bytes()).unwrap().unwrap().to_vec();
        assert!(!bytes.windows(entry.data.len()).any(|w| w == entry.data));
        assert_eq!(log.open(&key, &bytes).unwrap(), *entry);
        assert_eq!(log.seal(entry).unwrap(), log.seal(entry).unwrap());
        assert!(Log { encryption_key: Some([43; 32]), ..log }.open(&key, &bytes).is_err());
        assert!(Log { volume: id(3), ..log }.open(&key, &bytes).is_err());
        assert!(Log { entry: lid(3), ..log }.open(&key, &bytes).is_err());
        assert!(log.open("e/other", &bytes).is_err());
        for end in 0..28 {
            assert!(log.open(&key, &bytes[..end]).is_err());
        }
        let mut damaged = bytes.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(log.open(&key, &damaged).is_err());
        let wire = encode(&entries).unwrap();
        assert_eq!(decode(&wire).unwrap(), entries);
        // Peers can use different local storage keys without changing entries.
        let peer_db = database(dir.path(), "peer");
        let peer = Log { database: &peer_db, encryption_key: Some([43; 32]), ..log };
        peer.apply(&decode(&wire).unwrap(), false).unwrap();
        assert_eq!(peer.get(None).unwrap().entries, entries);
        assert_ne!(peer.seal(entry).unwrap(), bytes);
        peer.validate().unwrap();
        for end in 0..wire.len() {
            assert!(decode(&wire[..end]).is_err());
        }
        let mut forged = wire.clone();
        *forged.last_mut().unwrap() ^= 1;
        assert!(decode(&forged).is_err());
        let mut altered = entry.clone();
        altered.data.push(1);
        assert!(log.apply(&[altered], false).is_err());
        assert_eq!(log.get(None).unwrap().entries, entries);
        let now = log.now;
        drop(db);
        let db = database(dir.path(), "encrypted");
        let reopened = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: Some([42; 32]),
            now,
        };
        assert_eq!(reopened.get(None).unwrap().entries, entries);
        assert_eq!(reopened.pending().unwrap(), entries);
        reopened.validate().unwrap();
    }
    #[test]
    fn bounds_empty_blobs_and_cursor_validation() {
        assert!(validate_blobs(&[]).is_err());
        assert!(validate_blobs(&vec![vec![]; 129]).is_err());
        assert!(validate_blobs(&[vec![0; MAX_BLOB_BYTES + 1]]).is_err());
        assert!(validate_blobs(&vec![vec![0; MAX_BLOB_BYTES]; 2]).is_err());
        assert!(validate_blobs(&[vec![]]).is_ok());
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "bounds");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        log.put(&[vec![1; MAX_BLOB_BYTES]]).unwrap();
        log.put(&[vec![2; MAX_BLOB_BYTES]]).unwrap();
        let page = log.get(None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.more);
        assert_eq!(Cursor::parse(page.cursor.as_str()).unwrap(), page.cursor);
        assert!(Log { entry: lid(3), ..log }.get(Some(&page.cursor)).is_err());
        assert!(Cursor::parse("").is_err());
        let mut invalid = page.cursor.to_string();
        invalid.replace_range(..2, "02");
        assert!(Cursor::parse(&invalid).is_err());
        assert!(log.get(Some(&Cursor("bad".into()))).is_err());
    }
    #[test]
    fn cursor_deserialize_rejects_malformed_input() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "cursor-deserialize");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        let (cursor, _) = log.put(&[vec![1]]).unwrap();
        let valid = serde_json::to_string(&cursor).unwrap();
        assert_eq!(serde_json::from_str::<Cursor>(&valid).unwrap(), cursor);
        // A page pulled from AP or a peer is untrusted until parsed: reject a
        // malformed cursor here instead of storing it as a durable checkpoint.
        assert!(serde_json::from_str::<Cursor>("\"bad\"").is_err());
        assert!(serde_json::from_str::<Cursor>("\"\"").is_err());
    }
    #[test]
    fn get_rejects_index_key_with_truncated_arrival_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), "short-suffix");
        let log = Log {
            database: &db,
            volume: id(1),
            entry: lid(2),
            encryption_key: None,
            now: crate::store::now_unix_secs(),
        };
        let (_, entries) = log.put(&[b"payload".to_vec()]).unwrap();
        let reference = entry_key(&entries[0]);
        // Same prefix as a real index entry, but the arrival-clock suffix is
        // shorter than the 16 hex digits `get` expects: this must not panic.
        let bad_key = format!("l/{}/zzz", log.entry);
        let mut tx = db.begin_write(now_unix_secs()).unwrap();
        tx.put(bad_key.as_bytes(), reference.as_bytes()).unwrap();
        tx.commit().unwrap();
        assert!(matches!(log.get(None), Err(Error::Corrupt(_))));
    }
}
