//! Durable replay inputs for one accepted publication per volume. An intent is
//! a prior commit decision, not another authorization or authoritative catalog.
use super::*;
use std::sync::atomic::AtomicU8;

const PREFIX: &[u8] = b"received-publication/";
const MANIFEST: &str = ".publication.kvdb";
const BACKING_PREFIX: [u8; 1] = [1];

pub(super) struct ReceivedDirectory(pub PathBuf, AtomicU8);
impl ReceivedDirectory {
    pub(super) fn new(path: PathBuf) -> Self {
        Self(path, AtomicU8::new(0))
    }
    fn retain(&self, retained: bool) {
        self.1.store(u8::from(retained), Ordering::Release);
    }
    fn retain_rejected(&self) {
        self.1.store(2, Ordering::Release);
    }
}
impl Drop for ReceivedDirectory {
    fn drop(&mut self) {
        let retained = self.1.load(Ordering::Acquire);
        if retained == 0 {
            let _ = fs::remove_dir_all(&self.0);
        } else if retained == 2
            && let Ok(files) = fs::read_dir(&self.0)
        {
            // Database handles drop before this directory. Only the rejected
            // cut remains reachable; successful replay inputs are obsolete.
            for file in files.flatten() {
                if file.file_name() != "rejected" && file.file_name() != "rejected.json" {
                    let _ = fs::remove_file(file.path());
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingPublication {
    operation: Id,
    volume: Id,
    account: Id,
    access: Access,
    authority_generation: u64,
    authority_entry_generation: u64,
    authority_qualified: bool,
    entry_scope: Option<EntryId>,
    rejected_digest: Option<[u8; 32]>,
    pub(super) namespace_digest: Option<[u8; 32]>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backing {
    root: ReplicaRoot,
    clock: u64,
    hash_key: [u8; 32],
}
fn key(volume: Id) -> Vec<u8> {
    let mut key = Vec::with_capacity(PREFIX.len() + 16);
    key.extend_from_slice(PREFIX);
    key.extend_from_slice(volume.as_bytes());
    key
}
fn backing_key(database: DatabaseId) -> [u8; 17] {
    let mut key = [0; 17];
    key[0] = BACKING_PREFIX[0];
    key[1..].copy_from_slice(&database.to_bytes());
    key
}

impl ReceivedVolume {
    #[cfg(test)]
    pub(in crate::store::local) fn staged_directory(&self) -> &Path {
        &self._directory.0
    }
    fn record_backing(&self, manifest: &Database, database: DatabaseId) -> Result<()> {
        if !self._directory.0.join(database.file_name()).is_file() {
            return Err(Error::NotFound("received publication backing"));
        }
        self.with_database(database, |source| {
            let snapshot = source.begin_read()?;
            let backing = Backing {
                root: snapshot.root_hash()?,
                clock: snapshot.clock()?.as_u64(),
                hash_key: source.meta().hash_key,
            };
            put(manifest, &backing_key(database), &backing)?;
            let file = fs::File::open(self._directory.0.join(database.file_name())).map_err(io)?;
            rustix::io::retry_on_intr(|| rustix::fs::fsync(&file))
                .map_err(|error| io(error.into()))?;
            Ok(())
        })
    }

    fn seal(&self, pending: &PendingPublication, actor: DataActor) -> Result<()> {
        let path = self._directory.0.join(MANIFEST);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        let manifest = create_db(&path, None, STAGED_CACHE_BYTES)?;
        self.record_backing(&manifest, DatabaseId::Index)?;
        self.record_backing(&manifest, DatabaseId::Revocations)?;
        let mut after = None;
        loop {
            let page = self.database_page(after)?;
            for database in page.databases {
                let DatabaseId::Entry(id) = database else { unreachable!("named stage page") };
                let policy = DatabasePolicy::read_source(&self.metadata, database)?;
                if policy.authorize_reader(ReplicaReader::SessionParticipant(actor)).is_ok()
                    && volume_index::IndexSnapshot::new(self.metadata.begin_read()?)
                        .source_backing_ready(id)?
                {
                    self.record_backing(&manifest, database)?;
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        put(&manifest, b"intent", pending)?;
        drop(manifest);
        let file = fs::File::open(&path).map_err(io)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(|error| io(error.into()))?;
        sync_directory(&self._directory.0)?;
        let parent = self._directory.0.parent().expect("private receive directory has a parent");
        sync_directory(parent)?;
        sync_directory(parent.parent().expect("receive stages are nested under the profile"))
    }

    fn manifest(&self, pending: &PendingPublication) -> Result<Database> {
        let manifest = open_replica(&self._directory.0.join(MANIFEST), STAGED_CACHE_BYTES)?;
        if get::<PendingPublication>(&manifest, b"intent")?.as_ref() != Some(pending) {
            return Err(Error::corrupt(
                "staged publication intent differs from the pending intent",
            ));
        }
        Ok(manifest)
    }

    pub(super) fn verify_publication(&self, pending: &PendingPublication) -> Result<()> {
        self.verify_namespace_plan(pending.namespace_digest)?;
        let rejected = rejected::RejectedCut::read(&self._directory.0)?;
        if rejected.as_ref().map(encode).transpose()?.map(|bytes| *blake3::hash(&bytes).as_bytes())
            != pending.rejected_digest
        {
            return Err(Error::corrupt("rejected cut digest differs from the pending publication"));
        }
        if let Some(cut) = rejected {
            cut.verify(&self._directory.0)?;
        }
        let manifest = self.manifest(pending)?;
        let mut after = None;
        loop {
            let page = backing_page(&manifest, after)?;
            for database in &page.0 {
                if !self._directory.0.join(database.file_name()).is_file() {
                    return Err(Error::NotFound("received publication backing"));
                }
                let backing = required::<Backing>(&manifest, backing_key(*database))?;
                self.with_database(*database, |source| {
                    let snapshot = source.begin_read()?;
                    if snapshot.root_hash()? != backing.root
                        || snapshot.clock()?.as_u64() != backing.clock
                        || source.meta().hash_key != backing.hash_key
                        || backing.hash_key != database.hash_key(&self.database_key, self.volume)
                    {
                        return Err(Error::corrupt(
                            "staged database root, clock or hash key differs from its publication",
                        ));
                    }
                    Ok(())
                })?;
            }
            let Some(next) = page.1 else { break };
            after = Some(next);
        }
        for database in [DatabaseId::Index, DatabaseId::Revocations] {
            required::<Backing>(&manifest, backing_key(database))?;
        }
        Ok(())
    }

    pub(super) fn published_database_page(
        &self,
        pending: &PendingPublication,
        after: Option<DatabaseId>,
    ) -> Result<(Vec<DatabaseId>, Option<DatabaseId>)> {
        backing_page(&self.manifest(pending)?, after)
    }
}

fn backing_page(
    manifest: &Database,
    after: Option<DatabaseId>,
) -> Result<(Vec<DatabaseId>, Option<DatabaseId>)> {
    let start = after.map_or_else(|| BACKING_PREFIX.to_vec(), |after| backing_key(after).to_vec());
    let tx = manifest.begin_read()?;
    let mut page = Vec::with_capacity(64);
    for result in tx.iter_from(&start) {
        let (key, value) = result?;
        if !key.starts_with(&BACKING_PREFIX) {
            break;
        }
        if key.len() != 17 || value.len() > 1024 {
            return Err(Error::corrupt("pending publication marker has an invalid size"));
        }
        let database =
            DatabaseId::from_bytes(key[1..].try_into().expect("checked database width"))?;
        if Some(database) == after {
            continue;
        }
        if page.len() == 64 {
            let next = page.last().copied().expect("a full page has a final database");
            return Ok((page, Some(next)));
        }
        page.push(database);
    }
    Ok((page, None))
}

impl LocalStore {
    #[cfg(test)]
    pub(in crate::store::local) fn has_pending_publication(&self, volume: Id) -> Result<bool> {
        Ok(self.pending_publication(volume)?.is_some())
    }
    pub(super) fn pending_publication(&self, volume: Id) -> Result<Option<PendingPublication>> {
        get(&self.metadata, &key(volume))
    }

    pub(super) fn begin_received_publication(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        gate: &VolumeOperation,
    ) -> Result<PendingPublication> {
        let actor = self.data_actor(access, &self.volume(stage.volume)?)?;
        let pending = PendingPublication {
            operation: stage.operation,
            volume: stage.volume,
            account: self.account.id,
            access: access.clone(),
            authority_generation: stage.authority_generation,
            authority_entry_generation: stage.authority_entry_generation,
            authority_qualified: stage.authority_qualified,
            entry_scope: stage.entry_scope,
            namespace_digest: self.preflight_namespace_plan(stage, actor)?,
            rejected_digest: rejected::RejectedCut::read(&stage._directory.0)?
                .as_ref()
                .map(encode)
                .transpose()?
                .map(|bytes| *blake3::hash(&bytes).as_bytes()),
        };
        stage.seal(&pending, actor)?;
        let _profile = self.lock_profile()?;
        if self.pending_publication(stage.volume)?.is_some() {
            return Err(Error::LocalContention("volume already has a pending publication"));
        }
        // A failed commit may have crossed its durable boundary. Retain both
        // inputs and the fence before attempting it; a retry/reopen resolves
        // the persistent marker rather than assuming the write rolled back.
        stage._directory.retain(true);
        gate.fence_publication(true);
        put(&self.metadata, &key(stage.volume), &pending)?;
        Ok(pending)
    }

    pub(super) fn check_publication_binding(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        pending: &PendingPublication,
    ) -> Result<()> {
        if pending.operation != stage.operation
            || pending.volume != stage.volume
            || pending.account != self.account.id
            || &pending.access != access
            || pending.entry_scope != stage.entry_scope
            || pending.authority_generation != stage.authority_generation
            || pending.authority_entry_generation != stage.authority_entry_generation
            || pending.authority_qualified != stage.authority_qualified
            || pending.operation == Id::from_bytes([0; 16])
        {
            return Err(Error::AccessDenied("received publication binding"));
        }
        if stage.database_key != self.credentials(stage.volume)?.database_key {
            return Err(Error::AccessDenied("staged database credentials changed"));
        }
        stage.verify_publication(pending)
    }

    pub(super) fn finish_received_publication(
        &self,
        stage: &ReceivedVolume,
        pending: &PendingPublication,
        gate: &VolumeOperation,
    ) -> Result<()> {
        let _profile = self.lock_profile()?;
        if self.pending_publication(stage.volume)?.as_ref() != Some(pending) {
            return Err(Error::corrupt("pending publication changed before commit"));
        }
        if let Some(entry) = pending.entry_scope {
            self.restrict_catalog_locked(stage.volume, entry)?;
        }
        if pending.authority_qualified {
            self.clear_authority_locked(
                stage.volume,
                pending.authority_generation,
                pending.entry_scope.map(|entry| (entry, pending.authority_entry_generation)),
            )?;
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(&key(stage.volume))?;
        tx.commit()?;
        gate.fence_publication(false);
        if pending.rejected_digest.is_some() {
            stage._directory.retain_rejected();
        } else {
            stage._directory.retain(false);
        }
        Ok(())
    }

    pub(in crate::store::local) fn recover_received_publications(&self) -> Result<()> {
        let mut after = PREFIX.to_vec();
        let mut count = 0;
        loop {
            let next = {
                let tx = self.metadata.begin_read()?;
                let mut iter = tx.iter_from(&after);
                match iter.next().transpose()? {
                    Some((key, bytes)) if key.starts_with(PREFIX) => {
                        if key.len() != PREFIX.len() + 16 || bytes.len() > 2048 {
                            return Err(Error::corrupt(
                                "pending publication record has an invalid size",
                            ));
                        }
                        let pending: PendingPublication = decode(&bytes)?;
                        if key.as_ref() != self::key(pending.volume) {
                            return Err(Error::corrupt(
                                "pending publication volume differs from its key",
                            ));
                        }
                        Some(pending)
                    }
                    _ => None,
                }
            };
            let Some(pending) = next else {
                return Ok(());
            };
            count += 1;
            if count > MAX_VOLUME_OPERATIONS {
                return Err(Error::LimitExceeded);
            }
            after = key(pending.volume);
            let stage = self.reopen_received_publication(&pending)?;
            let gate = self.volume_operation(pending.volume)?;
            gate.fence_publication(true);
            self.adopt_received_volume(&pending.access, &stage)?;
        }
    }

    fn reopen_received_publication(&self, pending: &PendingPublication) -> Result<ReceivedVolume> {
        let credit = ReceiveCredit::enter(&self.received)?;
        let directory = ReceivedDirectory::new(
            self.path.join(crate::paths::RECEIVED).join(pending.operation.to_string()),
        );
        directory.retain(true);
        let credentials = self.credentials(pending.volume)?;
        let metadata =
            open_replica(&directory.0.join(DatabaseId::Index.file_name()), STAGED_CACHE_BYTES)?;
        let revocations = open_replica(
            &directory.0.join(DatabaseId::Revocations.file_name()),
            STAGED_CACHE_BYTES,
        )?;
        Ok(ReceivedVolume {
            operation: pending.operation,
            volume: pending.volume,
            identity: self.volume(pending.volume)?,
            database_key: credentials.database_key,
            authority_generation: pending.authority_generation,
            authority_entry_generation: pending.authority_entry_generation,
            authority_qualified: pending.authority_qualified,
            entry_scope: pending.entry_scope,
            metadata,
            revocations,
            _directory: directory,
            _credit: credit,
        })
    }
}
