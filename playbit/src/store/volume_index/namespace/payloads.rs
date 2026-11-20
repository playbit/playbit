//! Exact accepted source bytes retained independently of mutable register heads.
//! A selected receipt can omit history; its canonical payload must survive a
//! later unqualified head and receipt replay. These rows never enter source roots.
use super::{Error, MAX_FACTS, MAX_SOURCE_BYTES, Result, observations, source};
use crate::store::volume_index as index;
use kvdb::{LocalIndexes, ReadTxn, Storage};

pub(crate) const PREFIX: &[u8] = b"namespace-confirmed-payload/";
const USAGE: &[u8] = b"namespace-confirmed-payload-usage";
const ASSIGNMENT: &[u8] = b"namespace-confirmed-assignment/";
// At most two pointers per retained source, bounded by MAX_FACTS alongside
// payload accounting: a revision selector and an exact body identity.
const REVISION: &[u8] = b"namespace-confirmed-revision/";
const REFERENCE_BYTES: usize = 8 + 15;
const BLOB: &[u8] = b"namespace-confirmed-blob/";

// The volume is the enclosing index database. No field of the captured body
// identity is omitted; publication time does not identify bytes or authority.
fn blob_key(
    entry: super::EntryId,
    revision: super::EntryId,
    kind: &index::EntryKind,
) -> Option<Vec<u8>> {
    let index::EntryKind::Blob { len_bytes, digest, encrypted_key, .. } = kind else { return None };
    Some(
        [
            BLOB,
            entry.as_bytes(),
            revision.as_bytes(),
            &len_bytes.to_le_bytes(),
            digest,
            encrypted_key,
        ]
        .concat(),
    )
}

fn revision_key(entry: super::EntryId, revision: super::EntryId) -> Vec<u8> {
    [REVISION, entry.as_bytes(), revision.as_bytes()].concat()
}
fn reference(source: &observations::Witness) -> Result<[u8; REFERENCE_BYTES]> {
    let mut bytes = [0; REFERENCE_BYTES];
    bytes[..8].copy_from_slice(&source.clock.to_le_bytes());
    bytes[8..].copy_from_slice(source.id()?.as_bytes());
    Ok(bytes)
}

fn key(source: &observations::Witness) -> Result<Vec<u8>> {
    Ok([PREFIX, source.entry.as_bytes(), source.id()?.as_bytes()].concat())
}

/// Validate exact canonical bytes against a previously qualified witness.
/// This checks binding, not authority: only receipt qualification may confirm it.
pub(crate) fn validate(
    source: &observations::Witness,
    bytes: &[u8],
    retained: usize,
) -> Result<()> {
    if bytes.len() > index::MAX_ENTRY_RECORD_BYTES
        || crate::store::catalog::index_record_charge(bytes)?
            .checked_add(bytes.len() + 16 * 1024)
            .and_then(|bytes| bytes.checked_add(retained))
            .is_none_or(|bytes| bytes > 2 * 1024 * 1024 + 512 * 1024)
    {
        return Err(Error::LimitExceeded);
    }
    let matches = if source.birth {
        source::Declaration::decode(source.entry, kvdb::Clock::from_u64(source.clock), bytes)?
            .witness()
            == *source
    } else {
        let revision = index::revision(bytes)?;
        !matches!(
            revision.change,
            index::EntryChange::Created { .. } | index::EntryChange::Schema { .. }
        ) && revision.revision == source.revision
            && revision.origin_clock == source.clock
            && blake3::hash(bytes).as_bytes() == &source.source_digest
            && revision.observed.iter().eq(source.observed.iter())
    };
    if !matches {
        return Err(Error::corrupt("accepted namespace payload differs from witness"));
    }
    Ok(())
}

/// Copy a physically delivered source while its authenticated cut is pinned.
/// Selected snapshots may carry only a head, so history is not required here.
pub(crate) fn capture(
    tx: &ReadTxn<'_, impl Storage>,
    source: &observations::Witness,
    scan: &mut source::cut::Scan,
    retained: usize,
) -> Result<Vec<u8>> {
    let mut keys = vec![if source.birth {
        index::entry_key(source.entry).to_vec()
    } else {
        index::revision_key(source.entry, source.revision).to_vec()
    }];
    if !source.birth {
        for tag in [index::NAME, index::CONTENT, index::PARENT, index::RETIRED] {
            keys.push(index::meta_key(tag, source.entry).to_vec());
        }
    }
    for key in keys {
        let Some((clock, bytes)) = scan.read(tx, &key)? else { continue };
        if clock.as_u64() != source.clock {
            continue;
        }
        let digest = if source.birth {
            index::creation_digest(source.entry, clock, &bytes)
        } else {
            *blake3::hash(&bytes).as_bytes()
        };
        if digest == source.source_digest {
            validate(source, &bytes, retained)?;
            return Ok(bytes);
        }
    }
    Err(Error::corrupt("accepted namespace payload is absent before merge"))
}

/// Persist only qualified payloads: recipient confirmation commits with its
/// ledger; AP canonical admission commits with its authorized source merge.
/// Exact replay is idempotent; exclusion can never erase the accepted baseline.
pub(crate) fn retain(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
    observation: &observations::Observation,
    bytes: &[u8],
    retained: usize,
) -> Result<()> {
    retain_new(local, observation, bytes, retained).map(|_| ())
}

/// The first acceptance is also a publication event: a previously Pending
/// source may become accepted when another entry's dependencies arrive.
pub(crate) fn retain_new(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
    observation: &observations::Observation,
    bytes: &[u8],
    retained: usize,
) -> Result<bool> {
    if observation.status != observations::Status::Confirmed {
        return Err(Error::InvalidInput("accepted payload requires confirmation"));
    }
    validate(&observation.source, bytes, retained)?;
    let source = &observation.source;
    let key = key(source)?;
    let assignment_key = assignment_key(source.entry, source.id()?);
    let assignment =
        observation.assignment.ok_or(Error::InvalidInput("confirmation lacks assignment"))?;
    let exists = match local.get(&key)? {
        Some(prior) if prior.as_ref() == bytes => true,
        Some(_) => return Err(Error::corrupt("accepted namespace payload changed")),
        None => false,
    };
    let updates = reference_updates(
        |key, limit| bounded_copy(local.get(key)?, limit),
        source,
        bytes,
        retained,
        exists,
    )?;
    if exists {
        if local.get(&assignment_key)?.as_deref() != Some(&[assignment.bits()][..]) {
            return Err(Error::corrupt("accepted namespace assignment changed"));
        }
        return Ok(false);
    }
    if local.get(&assignment_key)?.is_some() {
        return Err(Error::corrupt("accepted namespace assignment lacks payload"));
    }
    let mut usage = Usage::decode(local.get(USAGE)?.as_deref())?;
    usage.add(bytes.len())?;
    for key in updates {
        local.put(&key, &reference(source)?)?;
    }
    local.put(&key, bytes)?;
    local.put(&assignment_key, &[assignment.bits()])?;
    local.put(USAGE, &usage.encode())?;
    Ok(true)
}

fn assignment_key(entry: super::EntryId, event: super::EntryId) -> Vec<u8> {
    [ASSIGNMENT, entry.as_bytes(), event.as_bytes()].concat()
}

/// Bounded lookup of immutable full-cut acceptance. The exact payload must still
/// match; neither a source head nor an opaque dependency supplies this authority.
pub(crate) fn assignment(
    tx: &impl source::cut::Sources,
    witness: &observations::Witness,
    bytes: &[u8],
) -> Result<Option<super::Assignments>> {
    let Some(retained) = tx.local(&key(witness)?, index::MAX_ENTRY_RECORD_BYTES)? else {
        return Ok(None);
    };
    if retained != bytes {
        return Err(Error::corrupt("accepted namespace payload changed"));
    }
    let key = assignment_key(witness.entry, witness.id()?);
    let value = tx.local(&key, 1)?.ok_or(Error::corrupt("accepted assignment is missing"))?;
    let [bits] = value.as_slice() else {
        return Err(Error::corrupt("invalid accepted assignment"));
    };
    Ok(Some(super::Assignments::from_bits(*bits)?))
}

/// Preflight retained bytes before the durable publication intent is admitted.
/// A plan has unique event IDs, so each absent payload consumes capacity once.
pub(crate) struct Usage {
    count: usize,
    bytes: usize,
}
impl Usage {
    fn decode(bytes: Option<&[u8]>) -> Result<Self> {
        let Some(bytes) = bytes else { return Ok(Self { count: 0, bytes: 0 }) };
        if bytes.len() != 8 {
            return Err(Error::corrupt("accepted namespace payload usage"));
        }
        let value = Self {
            count: u32::from_le_bytes(bytes[..4].try_into().expect("fixed-width payload count"))
                as usize,
            bytes: u32::from_le_bytes(bytes[4..].try_into().expect("fixed-width payload bytes"))
                as usize,
        };
        if value.count == 0
            || value.count > MAX_FACTS
            || value.bytes > MAX_SOURCE_BYTES
            || value.bytes < value.count
        {
            return Err(Error::corrupt("accepted namespace payload usage exceeds bounds"));
        }
        Ok(value)
    }
    pub(crate) fn validate_local(local: &LocalIndexes<'_, '_, impl Storage>) -> Result<()> {
        let usage = local.get(USAGE)?;
        if usage.is_none()
            && local.next_local_key(PREFIX, false)?.is_some_and(|key| key.starts_with(PREFIX))
        {
            return Err(Error::corrupt("accepted namespace payload usage is missing"));
        }
        Self::decode(usage.as_deref())?;
        Ok(())
    }
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn load(tx: &ReadTxn<'_, impl Storage>) -> Result<Self> {
        let usage = tx.local_get(USAGE)?;
        if usage.is_none()
            && let Some(row) = tx.local_iter_from(PREFIX)?.next()
            && row?.0.starts_with(PREFIX)
        {
            return Err(Error::corrupt("accepted namespace payload usage is missing"));
        }
        Self::decode(usage.as_deref())
    }
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn include(
        &mut self,
        tx: &ReadTxn<'_, impl Storage>,
        source: &observations::Witness,
        bytes: &[u8],
        retained: usize,
    ) -> Result<()> {
        let exists = match tx.local_get(&key(source)?)? {
            Some(prior) if prior.as_ref() == bytes => true,
            Some(_) => return Err(Error::corrupt("accepted namespace payload changed")),
            None => false,
        };
        reference_updates(
            |key, limit| bounded_copy(tx.local_get(key)?, limit),
            source,
            bytes,
            retained,
            exists,
        )?;
        if exists { Ok(()) } else { self.add(bytes.len()) }
    }

    fn add(&mut self, bytes: usize) -> Result<()> {
        if self.count >= MAX_FACTS || bytes > MAX_SOURCE_BYTES - self.bytes {
            return Err(Error::LimitExceeded);
        }
        self.count += 1;
        self.bytes += bytes;
        Ok(())
    }
    fn encode(&self) -> [u8; 8] {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&(self.count as u32).to_le_bytes());
        bytes[4..].copy_from_slice(&(self.bytes as u32).to_le_bytes());
        bytes
    }
}

/// Read an exact retained acceptance payload. The caller still owns current ACL
/// admission; retaining bytes never grants access to the original entry identity.
pub fn read(
    tx: &ReadTxn<'_, impl Storage>,
    source: &observations::Witness,
) -> Result<Option<Vec<u8>>> {
    let Some(bytes) = tx.local_get(&key(source)?)? else { return Ok(None) };
    validate(source, &bytes, source.observed.heap_bytes() + 512)?;
    Ok(Some(bytes.to_vec()))
}

/// Resolve a retained non-birth source with two bounded point reads. This is
/// immutable history access, not current-head selection or an ACL grant.
pub(crate) fn revision(
    tx: &ReadTxn<'_, impl Storage>,
    entry: super::EntryId,
    revision: super::EntryId,
) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
    read_revision(|key, limit| bounded_copy(tx.local_get(key)?, limit), entry, revision, 0)
}

/// An exact accepted birth is bound to its source clock and complete canonical
/// declaration. Merely finding the current raw declaration supplies no evidence.
pub(crate) fn birth(
    tx: &ReadTxn<'_, impl Storage>,
    source: &observations::Witness,
    canonical: &[u8],
) -> Result<bool> {
    if !source.birth {
        return Err(Error::InvalidInput("accepted birth requires a birth witness"));
    }
    let Some(bytes) = tx.local_get(&key(source)?)? else { return Ok(false) };
    if bytes.as_ref() != canonical {
        return Err(Error::corrupt("accepted birth differs from its source"));
    }
    Ok(true)
}

/// Match an exact captured content body independently of the revision selector.
pub(crate) fn blob(
    tx: &ReadTxn<'_, impl Storage>,
    version: &index::BlobVersion,
) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
    let kind = index::EntryKind::Blob {
        len_bytes: version.len_bytes,
        digest: version.digest,
        encrypted_key: version.encrypted_key,
        published_at_micros: 0,
    };
    let key = blob_key(version.address.entry, version.revision, &kind)
        .expect("blob kind has a captured-body key");
    read_reference(
        |key, limit| bounded_copy(tx.local_get(key)?, limit),
        &key,
        version.address.entry,
        version.revision,
        0,
    )
}

// At most two fixed pointers per payload (revision selector and exact body),
// hence at most 2*MAX_FACTS rows. Both are checked before publication intent;
// replay cannot fill missing bindings or bypass corrupt referenced payloads.
fn reference_updates(
    get: impl Fn(&[u8], usize) -> Result<Option<Vec<u8>>>,
    source: &observations::Witness,
    bytes: &[u8],
    retained: usize,
    exists: bool,
) -> Result<Vec<Vec<u8>>> {
    if source.birth {
        return Ok(Vec::new());
    }
    let mut keys = vec![revision_key(source.entry, source.revision)];
    if let index::EntryChange::Content { kind } = index::revision(bytes)?.change
        && let Some(key) = blob_key(source.entry, source.revision, &kind)
    {
        keys.push(key);
    }
    let mut updates = Vec::with_capacity(2);
    for key in keys {
        let prior =
            read_reference(&get, &key, source.entry, source.revision, retained + bytes.len())?;
        let replace = prior.as_ref().is_none_or(|(clock, prior)| {
            (clock.as_u64(), prior.as_slice()) < (source.clock, bytes)
        });
        if exists && replace {
            return Err(Error::corrupt("accepted namespace revision binding changed"));
        }
        if replace {
            updates.push(key);
        }
    }
    Ok(updates)
}

fn bounded_copy(bytes: Option<impl AsRef<[u8]>>, limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(bytes) = bytes else { return Ok(None) };
    if bytes.as_ref().len() > limit {
        return Err(Error::LimitExceeded);
    }
    Ok(Some(bytes.as_ref().to_vec()))
}

fn read_revision(
    get: impl Fn(&[u8], usize) -> Result<Option<Vec<u8>>>,
    entry: super::EntryId,
    revision: super::EntryId,
    retained: usize,
) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
    read_reference(get, &revision_key(entry, revision), entry, revision, retained)
}

fn read_reference(
    get: impl Fn(&[u8], usize) -> Result<Option<Vec<u8>>>,
    key: &[u8],
    entry: super::EntryId,
    revision: super::EntryId,
    retained: usize,
) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
    let Some(binding) = get(key, REFERENCE_BYTES)? else {
        return Ok(None);
    };
    if binding.len() != REFERENCE_BYTES {
        return Err(Error::corrupt("accepted revision binding length"));
    }
    let clock = u64::from_le_bytes(binding[..8].try_into().expect("fixed-width source clock"));
    let event = super::EntryId::from_bytes(binding[8..].try_into().expect("fixed-width event ID"))?;
    let bytes =
        get(&[PREFIX, entry.as_bytes(), event.as_bytes()].concat(), index::MAX_ENTRY_RECORD_BYTES)?
            .ok_or(Error::corrupt("accepted revision payload is missing"))?;
    if bytes.len() > index::MAX_ENTRY_RECORD_BYTES
        || crate::store::catalog::index_record_charge(&bytes)?
            .checked_add(bytes.len() + 16 * 1024)
            .and_then(|bytes| bytes.checked_add(retained))
            .is_none_or(|bytes| bytes > 2 * 1024 * 1024 + 512 * 1024)
    {
        return Err(Error::LimitExceeded);
    }
    let record = index::revision(&bytes)?;
    if clock == 0
        || record.origin_clock != clock
        || record.revision != revision
        || matches!(
            record.change,
            index::EntryChange::Created { .. } | index::EntryChange::Schema { .. }
        )
        || super::event_id(entry, revision, *blake3::hash(&bytes).as_bytes())? != event
    {
        return Err(Error::corrupt("accepted revision payload differs from its binding"));
    }
    if key.starts_with(BLOB) {
        let index::EntryChange::Content { kind } = record.change else {
            return Err(Error::corrupt("captured blob points to non-content source"));
        };
        if blob_key(entry, revision, &kind).as_deref() != Some(key) {
            return Err(Error::corrupt("captured blob differs from accepted source"));
        }
    }
    Ok(Some((kvdb::Clock::from_u64(clock), bytes)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests inject exact retained-payload boundaries.")]
mod tests {
    use super::super::{Birth, EntryId, Id, Origin};
    use super::*;
    use crate::store::object_contract::EntryAcl;
    use kvdb::{CreateOptions, Db, storage::mem::MemStorage};

    #[test]
    fn confirmation_and_capacity_preflight_preserve_exact_replay() {
        // Raw/provisional bytes do not become accepted, capacity is checked
        // before writing, and an exact replay still fits a completely full store.
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let bytes = index::EntryInfo {
            namespace: Birth::default(),
            origin: Origin { installation: Id::from_bytes([1; 16]), label: None },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::from_bytes([2; 15]).unwrap(),
            name: "directory".into(),
            kind: index::EntryKind::Directory {},
            protocol: None,
        }
        .canonical()
        .unwrap();
        let source = source::Declaration::decode(
            EntryId::from_bytes([1; 15]).unwrap(),
            kvdb::Clock::from_u64(1),
            &bytes,
        )
        .unwrap()
        .witness();
        let mut observation = observations::Observation {
            source,
            status: observations::Status::Provisional,
            assignment: None,
        };
        let mut tx = db.begin_write(0).unwrap();
        assert!(retain(&mut tx.local_indexes(), &observation, &bytes, 0).is_err());
        observation.status = observations::Status::Confirmed;
        observation.assignment = Some(super::super::Assignments::default());
        retain(&mut tx.local_indexes(), &observation, &bytes, 0).unwrap();
        tx.commit().unwrap();
        let snapshot = db.begin_read().unwrap();
        let mut usage = Usage::load(&snapshot).unwrap();
        usage.include(&snapshot, &observation.source, &bytes, 0).unwrap();
        assert_eq!((usage.count, usage.bytes), (1, bytes.len()));
        usage.bytes = MAX_SOURCE_BYTES;
        usage.include(&snapshot, &observation.source, &bytes, 0).unwrap();
        let mut other = observation.source.clone();
        other.entry = EntryId::from_bytes([3; 15]).unwrap();
        assert!(matches!(usage.include(&snapshot, &other, &bytes, 0), Err(Error::LimitExceeded)));
        assert!(usage.include(&snapshot, &observation.source, b"changed", 0).is_err());
        assert_eq!(read(&snapshot, &observation.source).unwrap(), Some(bytes));
        assert!(validate(&other, b"{}", 0).is_err());
        drop(snapshot);
        let mut tx = db.begin_write(0).unwrap();
        tx.local_indexes().del(USAGE).unwrap();
        tx.commit().unwrap();
        assert!(Usage::load(&db.begin_read().unwrap()).is_err());
    }
    #[test]
    fn retained_revision_selection_converges_and_rejects_damaged_pointers() {
        // Two qualified events may share a revision key. Select exactly as the
        // replicated history register does, while retaining both exact sources.
        let entry = EntryId::from_bytes([1; 15]).unwrap();
        let revision_id = EntryId::from_bytes([2; 15]).unwrap();
        let sources = [(2, 1), (2, 2), (3, 3)].map(|(clock, byte)| {
            let bytes = index::EntryRevision {
                revision: revision_id,
                creation: [7; 32],
                origin_clock: clock,
                origin: Origin { installation: Id::from_bytes([1; 16]), label: None },
                observed: vec![],
                change: index::EntryChange::Content {
                    kind: index::EntryKind::Blob {
                        len_bytes: 8,
                        digest: [byte; 32],
                        encrypted_key: [byte + 1; 32],
                        published_at_micros: 0,
                    },
                },
            }
            .canonical()
            .unwrap();
            let source = observations::Witness {
                entry,
                revision: revision_id,
                clock,
                birth: false,
                source_digest: *blake3::hash(&bytes).as_bytes(),
                observed: vec![].into(),
            };
            let row = observations::Observation {
                source,
                status: observations::Status::Confirmed,
                assignment: Some(super::super::Assignments::default()),
            };
            (row, bytes)
        });
        for order in [[0, 1, 2], [2, 1, 0]] {
            let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
            let snapshot = db.begin_read().unwrap();
            let mut usage = Usage::load(&snapshot).unwrap();
            for i in order {
                usage.include(&snapshot, &sources[i].0.source, &sources[i].1, 0).unwrap();
            }
            drop(snapshot);
            let mut tx = db.begin_write(0).unwrap();
            for (position, i) in order.into_iter().enumerate() {
                let mut local = tx.local_indexes();
                retain(&mut local, &sources[i].0, &sources[i].1, 0).unwrap();
                let winner = order[..=position]
                    .iter()
                    .map(|i| &sources[*i])
                    .max_by_key(|(row, bytes)| (row.source.clock, bytes))
                    .unwrap();
                assert_eq!(
                    read_revision(
                        |key, limit| bounded_copy(local.get(key)?, limit),
                        entry,
                        revision_id,
                        0
                    )
                    .unwrap(),
                    Some((kvdb::Clock::from_u64(winner.0.source.clock), winner.1.clone()))
                );
            }
            tx.commit().unwrap();
            let snapshot = db.begin_read().unwrap();
            assert_eq!(
                revision(&snapshot, entry, revision_id).unwrap(),
                Some((kvdb::Clock::from_u64(3), sources[2].1.clone()))
            );
            let mut usage = Usage::load(&snapshot).unwrap();
            for (row, bytes) in &sources {
                assert_eq!(
                    read(&snapshot, &row.source).unwrap().as_deref(),
                    Some(bytes.as_slice())
                );
                usage.include(&snapshot, &row.source, bytes, 0).unwrap();
                let index::EntryChange::Content { kind } = index::revision(bytes).unwrap().change
                else {
                    panic!()
                };
                let pointer = blob_key(entry, revision_id, &kind).unwrap();
                assert_eq!(
                    read_reference(
                        |key, limit| bounded_copy(snapshot.local_get(key)?, limit),
                        &pointer,
                        entry,
                        revision_id,
                        0
                    )
                    .unwrap(),
                    Some((kvdb::Clock::from_u64(row.source.clock), bytes.clone()))
                );
            }
            drop(snapshot);
            let index::EntryChange::Content { kind } =
                index::revision(&sources[0].1).unwrap().change
            else {
                panic!()
            };
            let pointer = blob_key(entry, revision_id, &kind).unwrap();
            let mut tx = db.begin_write(0).unwrap();
            tx.local_indexes().put(&pointer, &reference(&sources[1].0.source).unwrap()).unwrap();
            tx.commit().unwrap();
            let snapshot = db.begin_read().unwrap();
            assert!(
                read_reference(
                    |key, limit| bounded_copy(snapshot.local_get(key)?, limit),
                    &pointer,
                    entry,
                    revision_id,
                    0
                )
                .is_err()
            );
            assert!(
                Usage::load(&snapshot)
                    .unwrap()
                    .include(&snapshot, &sources[0].0.source, &sources[0].1, 0)
                    .is_err()
            );
            drop(snapshot);
            let mut tx = db.begin_write(0).unwrap();
            tx.local_indexes().put(&pointer, &reference(&sources[0].0.source).unwrap()).unwrap();
            tx.local_indexes().del(&key(&sources[2].0.source).unwrap()).unwrap();
            tx.commit().unwrap();
            let snapshot = db.begin_read().unwrap();
            assert!(revision(&snapshot, entry, revision_id).is_err());
            assert!(
                Usage::load(&snapshot)
                    .unwrap()
                    .include(&snapshot, &sources[0].0.source, &sources[0].1, 0)
                    .is_err()
            );
        }
    }
}
