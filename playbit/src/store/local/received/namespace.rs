//! A qualified namespace plan is captured before source merge and sealed with
//! the publication intent. Replay consumes that plan, never a mixed source cut.
use super::*;
use std::io::{Read, Write};
use volume_index::namespace::source::cut::{self, Sources};
use volume_index::namespace::{
    self as ns, Context,
    observations::{self, Ledger, Observation, Status, Witness},
};

const FILE: &str = ".namespace-plan";
const MAGIC: [u8; 4] = *b"NSP\x01";
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: usize = MAX_BYTES + MAX_PAYLOAD_BYTES + ns::MAX_FACTS * 4;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const ROW_BYTES: usize = 15 + 15 + 32 + 8 + 1;

struct Plan {
    dictionary: Arc<[EntryId]>,
    rows: Vec<Observation>,
}
fn admit_size(rows: usize, ids: usize) -> Result<usize> {
    if rows > ns::MAX_FACTS || ids >= ns::MAX_FACTS || rows > ids {
        return Err(Error::LimitExceeded);
    }
    let encoded = 12 + ids * 15 + rows * (ROW_BYTES + ids.div_ceil(8));
    // Account for encoded bytes and typed rows together, including vector growth,
    // row-map bookkeeping during capture and shared dictionary ownership.
    let heap = encoded + ids * 15 + 32 + rows * (512 + ids.div_ceil(64) * 8);
    if heap > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(encoded)
}
fn add_ids(ids: &mut BTreeSet<EntryId>, source: &Witness) -> Result<()> {
    for id in std::iter::once(source.id()?).chain(source.observed.iter().copied()) {
        ids.insert(id);
        if ids.len() >= ns::MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
    }
    Ok(())
}
impl Plan {
    fn write(&self, stage: &ReceivedVolume) -> Result<()> {
        let bytes = self.encode()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage._directory.0.join(FILE))
            .map_err(io)?;
        file.write_all(&bytes).map_err(io)?;
        drop(bytes);
        write_payloads(&mut file, stage, self)?;
        file.sync_all().map_err(io)
    }
    fn heap_bytes(&self) -> usize {
        self.dictionary.len() * 15
            + 32
            + self.rows.len() * (512 + self.dictionary.len().div_ceil(64) * 8)
    }
    fn encode(&self) -> Result<Vec<u8>> {
        let length = admit_size(self.rows.len(), self.dictionary.len())?;
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&(self.dictionary.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        for id in self.dictionary.iter() {
            bytes.extend_from_slice(id.as_bytes());
        }
        for row in &self.rows {
            let source = &row.source;
            bytes.extend_from_slice(source.entry.as_bytes());
            bytes.extend_from_slice(source.revision.as_bytes());
            bytes.extend_from_slice(&source.source_digest);
            bytes.extend_from_slice(&source.clock.to_le_bytes());
            let status = match row.status {
                Status::Confirmed => 0,
                Status::Excluded => 1,
                _ => return Err(Error::InvalidInput("namespace publication plan status")),
            };
            let assignment = row.assignment.map_or(0, ns::Assignments::bits);
            if (row.status == Status::Confirmed) != row.assignment.is_some() {
                return Err(Error::InvalidInput("namespace plan assignment qualification"));
            }
            bytes.push(status | (u8::from(source.birth) << 1) | (assignment << 2));
            let start = bytes.len();
            bytes.resize(start + self.dictionary.len().div_ceil(8), 0);
            for id in source.observed.iter() {
                let index = self
                    .dictionary
                    .binary_search(id)
                    .map_err(|_| Error::corrupt("namespace plan dictionary omitted an edge"))?;
                bytes[start + index / 8] |= 1 << (index % 8);
            }
        }
        assert_eq!(bytes.len(), length, "namespace plan fixed-width framing");
        Ok(bytes)
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut offset = 0;
        if take::<4>(bytes, &mut offset)? != MAGIC {
            return Err(Error::corrupt("namespace plan header"));
        }
        let ids = u32::from_le_bytes(take(bytes, &mut offset)?) as usize;
        let count = u32::from_le_bytes(take(bytes, &mut offset)?) as usize;
        if admit_size(count, ids)? != bytes.len() {
            return Err(Error::corrupt("namespace plan length"));
        }
        let mut dictionary = Vec::with_capacity(ids);
        for _ in 0..ids {
            dictionary.push(EntryId::from_bytes(take(bytes, &mut offset)?)?);
        }
        ns::validate_context(&dictionary)?;
        let dictionary: Arc<[EntryId]> = dictionary.into();
        let mut rows = Vec::with_capacity(count);
        let mut prior = None;
        let mut used = BTreeSet::new();
        for _ in 0..count {
            let entry = EntryId::from_bytes(take(bytes, &mut offset)?)?;
            let revision = EntryId::from_bytes(take(bytes, &mut offset)?)?;
            let source_digest = take(bytes, &mut offset)?;
            let clock = u64::from_le_bytes(take(bytes, &mut offset)?);
            let [flags] = take(bytes, &mut offset)?;
            if flags > 15 || flags & 1 != 0 && flags >> 2 != 0 {
                return Err(Error::corrupt("namespace plan flags"));
            }
            let bits = &bytes[offset..offset + ids.div_ceil(8)];
            offset += bits.len();
            if !ids.is_multiple_of(8) && bits.last().is_some_and(|byte| byte >> (ids % 8) != 0) {
                return Err(Error::corrupt("namespace plan trailing bits"));
            }
            let mut observed = Context::from(
                dictionary
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| {
                        (bits[index / 8] & (1 << (index % 8)) != 0).then_some(*id)
                    })
                    .collect::<Vec<_>>(),
            );
            observed.compact(&dictionary)?;
            let source =
                Witness { entry, revision, source_digest, clock, birth: flags & 2 != 0, observed };
            let id = source.id()?;
            if prior.is_some_and(|prior| prior >= id) || dictionary.binary_search(&id).is_err() {
                return Err(Error::corrupt("namespace plan source order"));
            }
            prior = Some(id);
            add_ids(&mut used, &source)?;
            rows.push(Observation {
                assignment: if flags & 1 == 0 {
                    Some(ns::Assignments::from_bits(flags >> 2)?)
                } else {
                    None
                },
                source,
                status: if flags & 1 == 0 { Status::Confirmed } else { Status::Excluded },
            });
        }
        if !used.iter().eq(dictionary.iter()) {
            return Err(Error::corrupt("namespace plan dictionary differs from its rows"));
        }
        Ok(Self { dictionary, rows })
    }
}
fn take<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N]> {
    let end = offset.checked_add(N).ok_or(Error::LimitExceeded)?;
    let value = bytes.get(*offset..end).ok_or(Error::corrupt("truncated namespace plan"))?;
    *offset = end;
    Ok(value.try_into().expect("fixed-width namespace plan field"))
}
fn read(
    stage: &ReceivedVolume,
    mut emit: impl FnMut(&Observation, &[u8], usize) -> Result<()>,
) -> Result<Option<(Plan, [u8; 32])>> {
    let mut file = match fs::File::open(stage._directory.0.join(FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    if file.metadata().map_err(io)?.len() > MAX_FILE_BYTES as u64 {
        return Err(Error::LimitExceeded);
    }
    let mut header = [0; 12];
    file.read_exact(&mut header).map_err(io)?;
    let ids = u32::from_le_bytes(header[4..8].try_into().expect("fixed-width ID count")) as usize;
    let rows =
        u32::from_le_bytes(header[8..12].try_into().expect("fixed-width row count")) as usize;
    let mut bytes = vec![0; admit_size(rows, ids)?];
    bytes[..12].copy_from_slice(&header);
    file.read_exact(&mut bytes[12..]).map_err(io)?;
    let plan = Plan::decode(&bytes)?;
    let mut digest = blake3::Hasher::new();
    digest.update(&bytes);
    drop(bytes);
    let mut total = 0usize;
    for row in &plan.rows {
        if row.status != Status::Confirmed {
            continue;
        }
        let mut length = [0; 4];
        file.read_exact(&mut length).map_err(io)?;
        digest.update(&length);
        let length = u32::from_le_bytes(length) as usize;
        total = total.checked_add(length).ok_or(Error::LimitExceeded)?;
        if length > volume_index::MAX_ENTRY_RECORD_BYTES || total > MAX_PAYLOAD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).map_err(io)?;
        digest.update(&bytes);
        ns::payloads::validate(&row.source, &bytes, plan.heap_bytes())?;
        emit(row, &bytes, plan.heap_bytes())?;
    }
    if file.read(&mut [0]).map_err(io)? != 0 {
        return Err(Error::corrupt("namespace plan has trailing payloads"));
    }
    Ok(Some((plan, *digest.finalize().as_bytes())))
}

fn write_payloads(file: &mut fs::File, stage: &ReceivedVolume, plan: &Plan) -> Result<()> {
    let tx = stage.metadata.begin_read()?;
    let mut scan = ns::source::cut::Scan::materializing(plan.heap_bytes())?;
    let mut total = 0usize;
    for row in &plan.rows {
        if row.status != Status::Confirmed {
            continue;
        }
        let bytes = ns::payloads::capture(&tx, &row.source, &mut scan, plan.heap_bytes())?;
        total = total.checked_add(bytes.len()).ok_or(Error::LimitExceeded)?;
        if total > MAX_PAYLOAD_BYTES {
            return Err(Error::LimitExceeded);
        }
        file.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(io)?;
        file.write_all(&bytes).map_err(io)?;
    }
    Ok(())
}
fn visit_excluded(
    index: &Database,
    stage: &ReceivedVolume,
    policy: &MetadataPolicy,
    actor: DataActor,
    retained: usize,
    mut emit: impl FnMut(Witness) -> Result<usize>,
) -> Result<()> {
    let local = index.begin_read()?;
    let mut cursor = vec![volume_index::ENTRY_ID_BYTE];
    let mut exclusive = false;
    let mut entries = 0usize;
    let mut scan = ns::source::cut::Scan::materializing(retained)?;
    while let Some(key) = local.next_source_key(&cursor, exclusive)? {
        if key.first() != Some(&volume_index::ENTRY_ID_BYTE) {
            break;
        }
        let entry = volume_index::source_entry_id(&key)?;
        cursor = key.clone();
        exclusive = true;
        entries += 1;
        if entries > ns::MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        if stage.entry_scope.is_some_and(|scope| scope != entry) {
            continue;
        }
        let (clock, bytes) =
            scan.read(&local, &key)?.ok_or(Error::corrupt("namespace declaration is absent"))?;
        let declaration = ns::source::Declaration::decode(entry, clock, &bytes)?;
        drop(bytes);
        scan.retained = declaration.heap_bytes();
        volume_index::visit_rejected_sources(
            &local,
            &stage.metadata,
            policy,
            actor,
            entry,
            &mut scan,
            |key, clock, bytes, scan| {
                if let Some(decoded) = declaration.source(key, clock, bytes)?
                    && observations::is_provisional(
                        &local,
                        &decoded.witness,
                        declaration.heap_bytes() + bytes.len() + 4096,
                    )?
                {
                    scan.retain_materialized(emit(decoded.witness)?)?;
                }
                Ok(true)
            },
        )?;
        scan.retained = 0;
    }
    Ok(())
}

impl LocalStore {
    pub(super) fn prepare_namespace_plan(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        policy: &MetadataPolicy,
    ) -> Result<()> {
        if !stage.authority_qualified {
            return Ok(());
        }
        let confirmed = ns::receipt::capture(&stage.metadata.begin_read()?, stage.entry_scope)?;
        let index = self.db(stage.volume, DatabaseId::Index)?;
        let confirmed_ids =
            confirmed.iter().map(|row| row.source.id()).collect::<Result<BTreeSet<_>>>()?;
        let mut ids = BTreeSet::new();
        for row in &confirmed {
            add_ids(&mut ids, &row.source)?;
        }
        let confirmed_bytes = ns::MAX_FACTS * 15
            + 32
            + confirmed_ids.len() * 64
            + confirmed.iter().map(|row| 512 + row.source.observed.heap_bytes()).sum::<usize>();
        let actor = self.data_actor(access, policy.volume())?;
        visit_excluded(&index, stage, policy, actor, confirmed_bytes + ids.len() * 64, |source| {
            if !confirmed_ids.contains(&source.id()?) {
                add_ids(&mut ids, &source)?;
            }
            Ok(confirmed_bytes + ids.len() * 64)
        })?;
        let dictionary: Arc<[EntryId]> = ids.into_iter().collect::<Vec<_>>().into();
        let mut rows = BTreeMap::new();
        let base = dictionary.len() * 15 + 32 + confirmed_ids.len() * 64;
        let row_bytes = 512 + dictionary.len().div_ceil(64) * 8;
        let mut insert = |mut row: Observation| -> Result<usize> {
            row.source.observed.compact(&dictionary)?;
            let id = row.source.id()?;
            if let Some(prior) = rows.get(&id) {
                if prior != &row {
                    return Err(Error::corrupt("namespace plan contradicts accepted source"));
                }
                return Ok(base + rows.len() * row_bytes);
            }
            admit_size(rows.len() + 1, dictionary.len())?;
            rows.insert(id, row);
            Ok(base + rows.len() * row_bytes)
        };
        let mut retained = base;
        for row in confirmed {
            retained = insert(row)?;
        }
        visit_excluded(&index, stage, policy, actor, retained, |source| {
            // A selected projection can deliver an accepted head while omitting
            // its history copy. Both physical keys identify the same event.
            if !confirmed_ids.contains(&source.id()?) {
                retained =
                    insert(Observation { source, status: Status::Excluded, assignment: None })?;
            }
            Ok(retained)
        })?;
        drop(confirmed_ids);
        let plan = Plan { dictionary, rows: rows.into_values().collect() };
        plan.write(stage)
    }
    pub(super) fn preflight_namespace_plan(
        &self,
        stage: &ReceivedVolume,
        actor: DataActor,
    ) -> Result<Option<[u8; 32]>> {
        let index = self.db(stage.volume, DatabaseId::Index)?;
        let snapshot = index.begin_read()?;
        let mut usage = ns::payloads::Usage::load(&snapshot)?;
        let plan = read(stage, |row, bytes, retained| {
            usage.include(&snapshot, &row.source, bytes, retained)
        })?;
        if plan.is_some() != stage.authority_qualified {
            return Err(Error::corrupt("namespace plan qualification differs from publication"));
        }
        let digest = plan.as_ref().map(|(_, digest)| *digest);
        let mut candidate = Ledger::load_read(&snapshot)?;
        if let Some((plan, _)) = &plan {
            candidate.apply_receipt(&plan.rows)?;
        }
        drop(plan);
        candidate.validate_persistence()?;
        // The reconciled stage is the candidate source cut. Overlay the target's
        // other entries for a selected receipt; their resource usage and current
        // controls must participate in the same preflight as the selected entry.
        // This transaction is deliberately rolled back: no candidate metadata
        // becomes authoritative before the durable publication intent.
        let rejected = rejected::RejectedCut::read(&stage._directory.0)?;
        let history = if let Some(entry) = stage.entry_scope
            && rejected.is_none()
        {
            DatabasePolicy::read_source(&stage.metadata, DatabaseId::Entry(entry))?
                .metadata_readable(actor)?
        } else {
            true
        };
        // Rejection cuts contain at most 128 original identities (1920 bytes).
        let canonical = rejected.map_or_else(Vec::new, |cut| cut.entries);
        let mut tx = stage.metadata.begin_write(now_unix_secs())?;
        copy_namespace_baseline(&snapshot, &mut tx.local_indexes())?;
        candidate.write_changes(&Ledger::default(), &mut tx.local_indexes())?;
        drop(candidate);
        let copied = read(stage, |row, bytes, retained| {
            ns::payloads::retain(&mut tx.local_indexes(), row, bytes, retained)
        })?;
        if copied.as_ref().map(|(_, digest)| *digest) != digest {
            return Err(Error::corrupt("namespace payloads changed during preflight"));
        }
        drop(copied);
        let projection = cut::project(&CandidateSources {
            target: &snapshot,
            stage: &tx,
            entry: stage.entry_scope,
            history,
            canonical: &canonical,
        })?;
        projection.write_current(&mut tx.local_indexes())?;
        Ok(digest)
    }

    pub(super) fn apply_namespace_plan(
        &self,
        stage: &ReceivedVolume,
        expected: Option<[u8; 32]>,
    ) -> Result<()> {
        let index = self.db(stage.volume, DatabaseId::Index)?;
        let snapshot = index.begin_read()?;
        let mut usage = ns::payloads::Usage::load(&snapshot)?;
        let plan = read(stage, |row, bytes, retained| {
            usage.include(&snapshot, &row.source, bytes, retained)
        })?;
        if plan.as_ref().map(|(_, digest)| *digest) != expected {
            return Err(Error::corrupt("namespace plan differs from publication intent"));
        }
        let before = Ledger::load_read(&snapshot)?;
        drop(snapshot);
        let mut after = before.clone();
        if let Some((plan, _)) = &plan {
            after.apply_receipt(&plan.rows)?;
        }
        drop(plan);
        // The volume publication gate excludes local authoring through this
        // transaction. Root equality cannot skip replica-local observations.
        let mut tx = index.begin_write(now_unix_secs())?;
        after.write_changes(&before, &mut tx.local_indexes())?;
        drop(after);
        drop(before);
        let copied = read(stage, |row, bytes, retained| {
            ns::payloads::retain(&mut tx.local_indexes(), row, bytes, retained)
        })?;
        if copied.as_ref().map(|(_, digest)| *digest) != expected {
            return Err(Error::corrupt("namespace payloads changed during replay"));
        }
        drop(copied);
        cut::project(&tx)?.write_current(&mut tx.local_indexes())?;
        tx.commit()?;
        Ok(())
    }
}

// Only source reads use the reconciled stage. Local qualification is the exact
// candidate ledger/payload state prepared in its rollback-only transaction.
struct CandidateSources<'a> {
    target: &'a dyn Sources,
    stage: &'a dyn Sources,
    entry: Option<EntryId>,
    history: bool,
    canonical: &'a [EntryId],
}
impl Sources for CandidateSources<'_> {
    fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        let staged = self.stage.next_source_key(key, exclusive)?;
        Ok(match (staged, self.target.next_source_key(key, exclusive)?) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }
    fn source(&self, key: &[u8], limit: usize) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
        let entry = volume_index::source_entry_id(key)?;
        if self.entry.is_some_and(|selected| selected != entry)
            || !self.history && volume_index::is_entry_history(key)
        {
            return self.target.source(key, limit);
        }
        if self.canonical.binary_search(&entry).is_ok() {
            // Canonical replacement also removes target-only rejected sources.
            return self.stage.source(key, limit);
        }
        let local = self.target.source(key, limit)?;
        let Some(remote) = self.stage.source(key, limit)? else { return Ok(local) };
        let delta = kvdb::sync::Delta {
            key,
            local: local.as_ref().map_or(kvdb::sync::EntState::Missing, |(clock, bytes)| {
                kvdb::sync::EntState::Present { clock: *clock, value: bytes }
            }),
            remote: kvdb::sync::EntState::Present { clock: remote.0, value: &remote.1 },
        };
        // Preparation already validated immutable source compatibility. Compare
        // only bounded bytes here; Scan charges and decodes the selected source
        // against retained facts after the losing buffer has been released.
        Ok(match kvdb::Resolver::resolve(&mut kvdb::Lww, &delta) {
            kvdb::sync::Resolution::UseRemote => Some(remote),
            kvdb::sync::Resolution::KeepLocal => local,
        })
    }
    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.stage.next_local_key(key, exclusive)
    }
    fn local(&self, key: &[u8], limit: usize) -> Result<Option<Vec<u8>>> {
        self.stage.local(key, limit)
    }
}

/// Stream the target's replica-local namespace evidence into the rollback-only
/// stage. The namespace owns this prefix: one context, at most MAX_FACTS each of
/// observations/payloads/assignments/revision/body pointers, and MAX_ENTRIES current rows.
/// Retain only one bounded value at a time; these copies never enter source roots.
fn copy_namespace_baseline(
    source: &impl Sources,
    local: &mut kvdb::LocalIndexes<'_, '_, kvdb::redb::RedbStorage>,
) -> Result<()> {
    const PREFIX: &[u8] = b"namespace-";
    const MAX_ROWS: usize = 5 * ns::MAX_FACTS + ns::MAX_ENTRIES + 2;
    const MAX_BYTES: usize = 2 * MAX_PAYLOAD_BYTES
        + ns::MAX_FACTS * 512
        + ns::MAX_ENTRIES * 256
        + volume_index::MAX_ENTRY_RECORD_BYTES;
    let mut cursor = PREFIX.to_vec();
    let mut exclusive = false;
    let mut count = 0;
    while let Some(key) = local.next_local_key(&cursor, exclusive)? {
        if !key.starts_with(PREFIX) {
            break;
        }
        count += 1;
        if count > MAX_ROWS {
            return Err(Error::LimitExceeded);
        }
        local.del(&key)?;
        cursor = key;
        exclusive = true;
    }
    let mut cursor = PREFIX.to_vec();
    let mut exclusive = false;
    let mut count = 0;
    let mut total = 0usize;
    while let Some(key) = source.next_local_key(&cursor, exclusive)? {
        if !key.starts_with(PREFIX) {
            break;
        }
        count += 1;
        if count > MAX_ROWS {
            return Err(Error::LimitExceeded);
        }
        let bytes = source
            .local(&key, volume_index::MAX_ENTRY_RECORD_BYTES)?
            .ok_or(Error::corrupt("namespace baseline disappeared"))?;
        total = total.checked_add(key.len() + bytes.len()).ok_or(Error::LimitExceeded)?;
        if total > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        local.put(&key, &bytes)?;
        cursor = key;
        exclusive = true;
    }
    ns::payloads::Usage::validate_local(local)
}

impl ReceivedVolume {
    pub(super) fn verify_namespace_plan(&self, expected: Option<[u8; 32]>) -> Result<()> {
        if read(self, |_, _, _| Ok(()))?.as_ref().map(|(_, digest)| *digest) != expected {
            return Err(Error::corrupt("sealed namespace plan digest changed"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests check exact sealed-plan framing and replay inputs.")]
mod tests {
    use super::*;

    #[test]
    fn candidate_sources_preserve_filtered_history_and_bound_the_final_union() {
        use kvdb::{CreateOptions, Db, storage::mem::MemStorage};
        use volume_index::{EntryChange, EntryInfo, EntryKind, EntryRevision};
        let target = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let stage = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let wall = kvdb::clock::EPOCH_UNIX_SECS + 1000;
        let id = |number: u16| {
            let mut bytes = [0; 15];
            bytes[13..].copy_from_slice(&number.to_be_bytes());
            EntryId::from_bytes(bytes).unwrap()
        };
        let entry = id(1);
        let prefix = volume_index::entry_source_prefixes(entry, false)
            .into_iter()
            .find(|key| volume_index::is_entry_history(key))
            .unwrap();
        let history = |revision: EntryId| [prefix.as_slice(), revision.as_bytes()].concat();
        let origin = ns::Origin { installation: Id::from_bytes([1; 16]), label: None };
        let kind = EntryKind::Blob {
            len_bytes: 0,
            digest: [1; 32],
            encrypted_key: [2; 32],
            published_at_micros: 0,
        };
        let birth = record::encode(&EntryInfo {
            created_revision: id(2),
            name: "file".into(),
            namespace: ns::Birth::default(),
            origin: origin.clone(),
            kind: kind.clone(),
            protocol: None,
            initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
        })
        .unwrap();
        let mut tx = target.begin_write(wall).unwrap();
        tx.put(&volume_index::entry_key(entry), &birth).unwrap();
        let clock = tx.clock();
        tx.commit().unwrap();
        let declaration = ns::source::Declaration::decode(entry, clock, &birth).unwrap();
        let mut tx = stage.begin_write(wall).unwrap();
        tx.put_with_clock(&volume_index::entry_key(entry), &birth, clock).unwrap();
        tx.commit().unwrap();
        // Each source cut fits. Their disjoint parallel histories must be
        // charged together, not hidden by a stage-only selected-entry preview.
        for (db, first) in [(&target, 10u16), (&stage, 2010u16)] {
            let mut tx = db.begin_write(wall + 1).unwrap();
            for number in first..first + 1100 {
                let edit = EntryRevision {
                    revision: id(number),
                    creation: declaration.witness().source_digest,
                    origin_clock: clock.as_u64() + 1,
                    origin: origin.clone(),
                    observed: vec![declaration.witness().id().unwrap()],
                    change: EntryChange::Content { kind: kind.clone() },
                };
                tx.put_with_clock(
                    &history(edit.revision),
                    &record::encode(&edit).unwrap(),
                    kvdb::Clock::from_u64(edit.origin_clock),
                )
                .unwrap();
            }
            tx.commit().unwrap();
            cut::project(&db.begin_read().unwrap()).unwrap();
        }
        let target = target.begin_read().unwrap();
        let stage = stage.begin_read().unwrap();
        let mut candidate = CandidateSources {
            target: &target,
            stage: &stage,
            entry: Some(entry),
            history: false,
            canonical: &[],
        };
        let retained = history(id(10));
        let incoming = history(id(2010));
        assert_eq!(
            candidate.source(&retained, volume_index::MAX_ENTRY_RECORD_BYTES).unwrap(),
            target.source(&retained, volume_index::MAX_ENTRY_RECORD_BYTES).unwrap()
        );
        assert!(
            candidate.source(&incoming, volume_index::MAX_ENTRY_RECORD_BYTES).unwrap().is_none()
        );
        cut::project(&candidate).unwrap();
        candidate.history = true;
        assert!(matches!(cut::project(&candidate), Err(Error::LimitExceeded)));
        // Canonical rejection is replacement, including remote absence.
        candidate.canonical = std::slice::from_ref(&entry);
        assert!(
            candidate.source(&retained, volume_index::MAX_ENTRY_RECORD_BYTES).unwrap().is_none()
        );
        assert!(
            candidate.source(&incoming, volume_index::MAX_ENTRY_RECORD_BYTES).unwrap().is_some()
        );
        cut::project(&candidate).unwrap();
    }

    #[test]
    fn plan_decoding_rejects_malformed_framing_and_dictionary() {
        let source = Witness {
            entry: EntryId::from_bytes([1; 15]).unwrap(),
            revision: EntryId::from_bytes([2; 15]).unwrap(),
            source_digest: [3; 32],
            clock: 1,
            birth: false,
            observed: Vec::new().into(),
        };
        let plan = Plan {
            dictionary: vec![source.id().unwrap()].into(),
            rows: vec![Observation {
                source,
                status: Status::Confirmed,
                assignment: Some(ns::Assignments { name: false, parent: true }),
            }],
        };
        let encoded = plan.encode().unwrap();
        assert_eq!(Plan::decode(&encoded).unwrap().rows, plan.rows);
        for end in 0..encoded.len() {
            assert!(Plan::decode(&encoded[..end]).is_err());
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(Plan::decode(&trailing).is_err());
        for (offset, value) in [(0, b'X'), (12 + 15 + ROW_BYTES - 1, 16), (encoded.len() - 1, 0x80)]
        {
            let mut bytes = encoded.clone();
            bytes[offset] = value;
            assert!(Plan::decode(&bytes).is_err());
        }
        let mut wrong_id = encoded.clone();
        wrong_id[12..27].copy_from_slice(EntryId::from_bytes([4; 15]).unwrap().as_bytes());
        assert!(Plan::decode(&wrong_id).is_err());
        let mut huge = encoded;
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Plan::decode(&huge).is_err());
    }
}
