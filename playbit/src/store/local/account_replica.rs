//! Durable per-record account publication intent, committed with local state.
use super::recovery_catalog::RecoveryCatalog;
use super::*;
use crate::store::account::{Cipher, Record};
use kvdb::{Clock, WriteTxn};

const RECORD: &[u8] = b"account-record/";
const DIRTY: &[u8] = b"account-dirty/";
const CLOCK: &[u8] = b"account-clock";
const QUARANTINE: &[u8] = b"account-authority-quarantine";

pub(super) struct CachedCipher {
    account: Id,
    secret: [u8; 16],
    cipher: Arc<Cipher>,
}

impl Record {
    fn open(cipher: &Cipher, key: &[u8], clock: Clock, ciphertext: &[u8]) -> Result<Option<Self>> {
        let plain = cipher.open(key, clock, ciphertext)?;
        let decoded = crate::store::account::decode_plaintext(&plain)?;
        if cipher.record_key(decoded.logical_key.as_bytes())?.as_slice() != key {
            return Err(Error::InvalidInput("account record key does not match its contents"));
        }
        decoded.admit()
    }
}

/// One retained ciphertext; replay must use these bytes and originating clock.
/// Acknowledgment never removes a newer local record written during publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAccountRecord {
    pub key: [u8; 32],
    pub clock: Clock,
    pub ciphertext: Vec<u8>,
}
impl PendingAccountRecord {
    fn parts<'a>(key: &[u8], bytes: &'a [u8]) -> Result<(Clock, &'a [u8])> {
        let clock = Clock::from_u64(u64::from_le_bytes(
            bytes
                .get(..8)
                .ok_or(Error::corrupt("pending account record clock is truncated"))?
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ));
        let ciphertext = bytes
            .get(8..)
            .ok_or(Error::corrupt("pending account record ciphertext is truncated"))?;
        crate::store::account::validate_envelope(key, clock, ciphertext)?;
        Ok((clock, ciphertext))
    }
    fn decode(key: &[u8], bytes: &[u8]) -> Result<Self> {
        let (clock, ciphertext) = Self::parts(key, bytes)?;
        Ok(Self {
            key: key
                .try_into()
                .map_err(|_| Error::corrupt("account record key must be 32 bytes"))?,
            clock,
            ciphertext: ciphertext.to_vec(),
        })
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.ciphertext.len());
        bytes.extend_from_slice(&self.clock.as_u64().to_le_bytes());
        bytes.extend_from_slice(&self.ciphertext);
        bytes
    }
}
fn key(prefix: &[u8], opaque: &[u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 32);
    key.extend_from_slice(prefix);
    key.extend_from_slice(opaque);
    key
}

impl LocalStore {
    /// A failed account admission never deletes credentials or overwrites the
    /// replica. Retain the first authenticated unsupported record as evidence.
    /// This version has no newer authority features and cannot clear this fence.
    fn quarantine_account(&self, key: &[u8], clock: Clock, ciphertext: &[u8]) -> Result<()> {
        let _authority = self
            .personal_consent
            .write()
            .map_err(|_| Error::Internal("personal consent lock is poisoned"))?;
        self.account_authority_paused.store(true, std::sync::atomic::Ordering::Release);
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if !tx.has(QUARANTINE)? {
            let key: [u8; 32] =
                key.try_into().map_err(|_| Error::InvalidInput("invalid account record key"))?;
            let record = PendingAccountRecord { key, clock, ciphertext: ciphertext.to_vec() };
            let mut bytes = Vec::from(key);
            bytes.extend_from_slice(&record.encode());
            tx.put(QUARANTINE, &bytes)?;
            tx.commit()?;
            self.changes.observed_changed(None);
        }
        Ok(())
    }

    pub(super) fn require_account_authority(&self) -> Result<()> {
        self.check_account_authority()?;
        let snapshot = self.metadata.begin_read()?;
        if snapshot.has(QUARANTINE)? {
            self.account_authority_paused.store(true, std::sync::atomic::Ordering::Release);
            return Err(crate::store::account::unsupported());
        }
        Ok(())
    }

    /// Network owners recheck after storage admission/awaits without doing
    /// synchronous database I/O on their executor. No network wait holds a lock.
    pub(crate) fn check_account_authority(&self) -> Result<()> {
        if self.account_authority_paused.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::store::account::unsupported());
        }
        Ok(())
    }

    pub(super) fn lock_account_authority(&self) -> Result<std::sync::RwLockReadGuard<'_, ()>> {
        let guard = self
            .personal_consent
            .read()
            .map_err(|_| Error::Internal("personal consent lock is poisoned"))?;
        self.require_account_authority()?;
        Ok(guard)
    }

    fn open_account_record(
        &self,
        cipher: &Cipher,
        key: &[u8],
        clock: Clock,
        ciphertext: &[u8],
    ) -> Result<Option<Record>> {
        match Record::open(cipher, key, clock, ciphertext) {
            Err(error) if crate::store::account::is_unsupported(&error) => {
                self.quarantine_account(key, clock, ciphertext)?;
                Err(error)
            }
            result => result,
        }
    }

    fn lock_account_writer(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let _timing = crate::trace::Span::new("account.writer_admission");
        self.account_writer.lock().map_err(|_| Error::Internal("account writer lock is poisoned"))
    }
    fn unlocked_account_cipher(&self) -> Result<Arc<Cipher>> {
        let state = self
            .recovery_state()?
            .filter(|state| state.acknowledged && state.revision != 0)
            .ok_or(Error::AccessDenied("account secret is not saved"))?;
        self.cipher_for(&state.secret)
    }

    /// The ordinary encrypted account tree shares the volume backend cache.
    /// First creation is distinct from loss of an already initialized file.
    pub fn account_replica(&self) -> Result<Arc<Database>> {
        self.unlocked_account_cipher()?;
        let ready = get::<bool>(&self.metadata, b"account-replica-ready")?.unwrap_or(false);
        let hash_key = crate::store::account::hash_key(self.account.id);
        let database = self.cached_db(
            DatabaseKey::Account,
            &self.path.join(crate::paths::ACCOUNT_DB),
            (!ready).then_some(hash_key),
        )?;
        if database.meta().hash_key != hash_key {
            return Err(Error::AccessDenied("account replica binding"));
        }
        if !ready {
            let _profile = self.lock_profile()?;
            if !get::<bool>(&self.metadata, b"account-replica-ready")?.unwrap_or(false) {
                put(&self.metadata, b"account-replica-ready", &true)?;
            }
        }
        Ok(database)
    }

    /// AP registration/provisioning dependencies of one encrypted outbox record.
    /// The publisher checks these against completed initialization before staging
    /// the entry, so bootstrap cannot publish a not-yet-usable capability either.
    pub fn account_record_dependencies(
        &self,
        record: &PendingAccountRecord,
    ) -> Result<[Option<Id>; 2]> {
        self.require_account_authority()?;
        let cipher = self.unlocked_account_cipher()?;
        Ok(match Record::open(&cipher, &record.key, record.clock, &record.ciphertext)? {
            Some(Record::Volume { volume, .. } | Record::HostedVolume { volume, .. }) => {
                [(!self.volume_deleted(volume.id)?).then_some(volume.id), None]
            }
            Some(
                Record::Invitation { invitation, .. } | Record::HostedInvitation { invitation, .. },
            ) => [
                (!self.volume_deleted(invitation.session.replica_volume())?)
                    .then_some(invitation.session.replica_volume()),
                (!self.volume_deleted(invitation.session.replica_volume())?)
                    .then_some(invitation.session.replica_volume()),
            ],
            _ => [None, None],
        })
    }

    /// Stage durable local intent in the ordinary replica and return only its
    /// committed changeset. Replay keeps the original entry clock/ciphertext.
    /// None means a newer account-tree record must be adopted locally first.
    pub fn stage_account_record(&self, record: &PendingAccountRecord) -> Result<Option<Vec<u8>>> {
        self.require_account_authority()?;
        let _timing = crate::trace::Span::new("account.stage_record");
        let cipher = self.unlocked_account_cipher()?;
        self.open_account_record(&cipher, &record.key, record.clock, &record.ciphertext)?;
        let _authority = self.lock_account_authority()?;
        let database = self.account_replica()?;
        let replay = {
            let snapshot = database.begin_read()?;
            let order = snapshot.get_with_clock(&record.key)?.map(|(clock, value)| {
                (clock, value.as_ref()).cmp(&(record.clock, record.ciphertext.as_slice()))
            });
            match order {
                Some(std::cmp::Ordering::Greater) => return Ok(None),
                Some(std::cmp::Ordering::Equal) => Some(
                    snapshot
                        .delta_for_key(&record.key, crate::store::MAX_REPLICA_DELTA_BYTES)?
                        .ok_or(Error::corrupt("account record delta is missing"))?,
                ),
                _ => None,
            }
        };
        let delta = if let Some(delta) = replay {
            delta
        } else {
            let _writer = self.lock_account_writer()?;
            let mut tx = database
                .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
            let order = tx.get_with_clock(&record.key)?.map(|(clock, value)| {
                (clock, value.as_ref()).cmp(&(record.clock, record.ciphertext.as_slice()))
            });
            match order {
                Some(std::cmp::Ordering::Greater) => return Ok(None),
                Some(std::cmp::Ordering::Equal) => {
                    // The writer guard prevents a concurrent receive from replacing
                    // this entry while its committed snapshot is captured. No commit
                    // is needed for a replay of exactly the same value.
                    database
                        .begin_read()?
                        .delta_for_key(&record.key, crate::store::MAX_REPLICA_DELTA_BYTES)?
                        .ok_or(Error::corrupt("account record delta is missing"))?
                }
                _ => {
                    tx.put_with_clock(&record.key, &record.ciphertext, record.clock)?;
                    tx.commit_with_delta()?
                }
            }
        };
        match delta {
            kvdb::CommittedDelta::Bytes(bytes) => Ok(Some(bytes)),
            kvdb::CommittedDelta::PullRequired => Err(Error::LimitExceeded),
        }
    }

    /// Receive a complete bounded AP publication. Authenticate and decode every
    /// entry, including LWW losers, before any replica/profile mutation. The
    /// caller owns AP continuity and must mark failure/gap if adoption fails.
    pub fn receive_account_delta(&self, bytes: &[u8]) -> Result<bool> {
        self.require_account_authority()?;
        let _timing = crate::trace::Span::new("account.receive_delta");
        let cipher = self.unlocked_account_cipher()?;
        crate::store::account::inspect_delta(bytes, |key, clock, value| {
            self.validate_account_projection(self.open_account_record(&cipher, key, clock, value)?)
        })?;
        let database = self.account_replica()?;
        {
            let _authority = self.lock_account_authority()?;
            let _writer = self.lock_account_writer()?;
            let mut receiver = kvdb::DeltaReceiver::new(
                &database,
                kvdb::Lww,
                now_unix_secs(),
                crate::store::MAX_REPLICA_DELTA_BYTES,
            )?;
            receiver.process(bytes)?;
            receiver.finish()?;
        }
        let mut changed = false;
        // Only this frame's keys are projected, never the full account catalog.
        // Repeat even for an identical database receipt: a previous process may
        // have committed the tree and exited before its profile transaction.
        crate::store::account::inspect_delta(bytes, |key, _, _| {
            changed |= self.adopt_account_record(
                key.try_into()
                    .map_err(|_| Error::corrupt("account record key must be 32 bytes"))?,
            )?;
            Ok(())
        })?;
        Ok(changed)
    }

    fn inspect_account_snapshot<J: kvdb::SyncStaging>(
        &self,
        cipher: &Cipher,
        prepared: kvdb::sync::PreparedChanges<J>,
    ) -> Result<kvdb::sync::PreparedChanges<J>> {
        use kvdb::sync::SyncEntState;
        let mut inspection = prepared.inspect();
        let mut failure = None;
        while inspection
            .next(|key, state| {
                let (clock, value) = match state {
                    SyncEntState::Missing => return Ok(()),
                    SyncEntState::Present { clock, value } => (clock, value),
                    SyncEntState::Deleted { .. } => return Err(kvdb::Error::Protocol),
                };
                if value.len() > crate::store::account::MAX_ENVELOPE_BYTES {
                    return Err(kvdb::Error::ValueSize);
                }
                let mut bytes = vec![0; value.len()];
                value.read_exact_at(0, &mut bytes)?;
                self.open_account_record(cipher, key, clock, &bytes)
                    .and_then(|record| self.validate_account_projection(record))
                    .map_err(|error| {
                        failure = Some(error);
                        kvdb::Error::Protocol
                    })
            })
            .map_err(|error| failure.take().unwrap_or_else(|| error.into()))?
        {}
        Ok(inspection.finish()?)
    }

    /// Merge a completed bootstrap journal, authenticating every remote entry
    /// before committing. PreparedSync holds only a snapshot during transport.
    pub fn merge_account_snapshot<J: kvdb::SyncStaging>(
        &self,
        prepared: kvdb::sync::PreparedChanges<J>,
    ) -> Result<bool> {
        use kvdb::sync::{SyncEntState, SyncResolver};
        self.require_account_authority()?;
        let cipher = self.unlocked_account_cipher()?;
        let prepared = self.inspect_account_snapshot(&cipher, prepared)?;
        let database = self.account_replica()?;
        let mut validation_error = None;
        let resolver = |entry: &kvdb::sync::SyncDelta<'_>| {
            let (clock, value) = match entry.remote {
                SyncEntState::Missing => return Ok(kvdb::sync::Resolution::KeepLocal),
                SyncEntState::Present { clock, value } => (clock, value),
                SyncEntState::Deleted { .. } => return Err(kvdb::Error::Protocol),
            };
            if value.len() > crate::store::account::MAX_ENVELOPE_BYTES {
                return Err(kvdb::Error::ValueSize);
            }
            let mut bytes = vec![0; value.len()];
            value.read_exact_at(0, &mut bytes)?;
            Record::open(&cipher, entry.key, clock, &bytes)
                .and_then(|record| self.validate_account_projection(record))
                .map_err(|error| {
                    // The kvdb resolver cannot carry store errors. Retain the
                    // sanitized cause for the caller after the merge rolls back.
                    validation_error = Some(error);
                    kvdb::Error::Protocol
                })?;
            kvdb::Lww.resolve(entry)
        };
        {
            let _authority = self.lock_account_authority()?;
            let _writer = self.lock_account_writer()?;
            let merged = prepared
                .merge(&database, resolver, now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
                .map_err(|error| validation_error.unwrap_or_else(|| error.into()))?;
            merged.finish_with_delta()?;
        }
        self.reconcile_account_records()
    }

    /// Startup/reconnect/gap recovery only, after authenticating the received
    /// snapshot. Retain a read snapshot and one entry at a time; no writer or
    /// network wait spans the catalog. Live edits use receive_account_delta.
    pub fn reconcile_account_records(&self) -> Result<bool> {
        self.require_account_authority()?;
        let _timing = crate::trace::Span::new("account.reconcile_records");
        let cipher = self.unlocked_account_cipher()?;
        let database = self.account_replica()?;
        let snapshot = database.begin_read()?;
        // A malformed tail must not expose a successfully decoded prefix.
        for entry in snapshot.iter() {
            let (key, _) = entry?;
            let (clock, value) = snapshot
                .get_with_clock(&key)?
                .ok_or(Error::corrupt("account record or clock is missing"))?;
            self.validate_account_projection(
                self.open_account_record(&cipher, &key, clock, &value)?,
            )?;
        }
        let mut changed = false;
        for entry in snapshot.iter() {
            let (key, _) = entry?;
            changed |= self.adopt_account_record(
                key.as_ref()
                    .try_into()
                    .map_err(|_| Error::corrupt("account record key must be 32 bytes"))?,
            )?;
        }
        Ok(changed)
    }

    fn validate_account_projection(&self, record: Option<Record>) -> Result<()> {
        let _profile = self.lock_profile()?;
        let Some(record) = record else {
            return Ok(());
        };
        match record {
            Record::Volume { volume, credentials } => {
                if self.account_volume_requires_invitation(volume.id)? {
                    return Err(Error::AccessDenied("app scope credentials require an invitation"));
                }
                self.validate_incoming_catalog(&RecoveryCatalog::volume(
                    self.account.id,
                    volume,
                    credentials,
                ))
            }
            Record::Invitation { invitation, .. } => self.validate_incoming_catalog(
                &RecoveryCatalog::invitation(self.account.id, invitation)?,
            ),
            Record::HostedVolume { volume, credentials, scope } => self.validate_incoming_catalog(
                &RecoveryCatalog::volume(self.account.id, volume, credentials).with_scope(scope),
            ),
            Record::HostedInvitation { invitation, scope, .. } => self.validate_incoming_catalog(
                &RecoveryCatalog::invitation(self.account.id, invitation)?.with_scope(scope),
            ),
            Record::PersonalVolumes { grants, .. } => {
                let snapshot = self.metadata.begin_read()?;
                for grant in grants {
                    if let Some(bytes) =
                        snapshot.get(format!("volume/{}", grant.volume).as_bytes())?
                        && decode::<Volume>(&bytes)?.owner != self.account.id
                    {
                        return Err(Error::AccessDenied("personal volume owner"));
                    }
                }
                Ok(())
            }
            Record::DisplayName { .. } | Record::VolumeDeleted { .. } => Ok(()),
        }
    }

    // Commit presentation alongside capability adoption, after its source-clock fence.
    fn adopt_account_invitation(
        &self,
        invitation: Invitation,
        app_name: Option<String>,
        scope: Option<AppScope>,
        finish: impl FnOnce(&mut WriteTxn<'_, RedbStorage>) -> Result<bool>,
    ) -> Result<bool> {
        let id = invitation.session.id.clone();
        let volumes = [invitation.session.replica_volume()];
        let mut catalog = RecoveryCatalog::invitation(self.account.id, invitation)?;
        if let Some(scope) = scope {
            catalog = catalog.with_scope(scope);
        }
        Ok(self
            .import_credentials(
                || Ok(catalog),
                |tx| {
                    if !finish(tx)? {
                        return Ok(false);
                    }
                    for volume in volumes {
                        if tx.has(format!("deleted-volume/{volume}").as_bytes())? {
                            // Credential adoption intentionally omits deleted sessions.
                            // Settle the source clock without reviving presentation.
                            return Ok(true);
                        }
                    }
                    let key = format!("session/{id}");
                    let mut stored: StoredSession =
                        decode(&tx.get(key.as_bytes())?.ok_or(Error::NotFound("session"))?)?;
                    if app_name.is_some() {
                        stored.app_name = app_name;
                    }
                    tx.put(key.as_bytes(), &encode(&stored)?)?;
                    Ok(true)
                },
            )?
            .is_some())
    }

    /// Adopt one current account-tree entry, including after a crash between
    /// replica commit and profile adoption. Snapshot lifetime does not hold a
    /// writer. Original invitations/keys use the same checks as recovery; local
    /// session version adoption and device decisions remain untouched.
    pub fn adopt_account_record(&self, opaque: &[u8; 32]) -> Result<bool> {
        self.require_account_authority()?;
        let _timing = crate::trace::Span::new("account.adopt_record");
        let cipher = self.unlocked_account_cipher()?;
        let database = self.account_replica()?;
        let snapshot = database.begin_read()?;
        let (clock, ciphertext) =
            snapshot.get_with_clock(opaque)?.ok_or(Error::NotFound("account record"))?;
        let decoded = {
            let _validation = crate::trace::Span::new("account.adopt_validate");
            self.open_account_record(&cipher, opaque, clock, &ciphertext)?
        };
        let finish = |tx: &mut WriteTxn<'_, RedbStorage>| {
            Self::adopt_account_intent(tx, opaque, clock, &ciphertext)
        };
        let changed: Result<bool> = match decoded {
            None => self.adopt_optional_account_record(opaque, clock, &ciphertext),
            Some(Record::VolumeDeleted { volume }) => {
                self.adopt_account_volume_deletion(volume, finish)
            }
            Some(Record::Volume { volume, credentials }) => {
                if self.account_volume_requires_invitation(volume.id)? {
                    return Err(Error::AccessDenied("app scope credentials require an invitation"));
                }
                let catalog = RecoveryCatalog::volume(self.account.id, volume, credentials);
                Ok(self.import_credentials(|| Ok(catalog), finish)?.is_some())
            }
            Some(Record::Invitation { invitation, app_name }) => {
                self.adopt_account_invitation(invitation, app_name, None, finish)
            }
            Some(Record::HostedVolume { volume, credentials, scope }) => {
                let catalog =
                    RecoveryCatalog::volume(self.account.id, volume, credentials).with_scope(scope);
                Ok(self.import_credentials(|| Ok(catalog), finish)?.is_some())
            }
            Some(Record::HostedInvitation { invitation, app_name, scope }) => {
                self.adopt_account_invitation(invitation, app_name, Some(scope), finish)
            }
            Some(record) => self.adopt_account_projection(&record, finish),
        };
        let changed = changed?;
        if changed {
            self.changes.observed_changed(None);
        }
        Ok(changed)
    }

    fn adopt_account_volume_deletion(
        &self,
        volume: Id,
        finish: impl FnOnce(&mut WriteTxn<'_, RedbStorage>) -> Result<bool>,
    ) -> Result<bool> {
        let operations = self.operations(&[volume])?;
        let gate = self.volume_operation(volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if !finish(&mut tx)? {
            return Ok(false);
        }
        tx.put(format!("deleted-volume/{volume}").as_bytes(), &[])?;
        tx.commit()?;
        self.changes.observed_changed(Some(volume));
        Ok(true)
    }

    fn adopt_optional_account_record(
        &self,
        opaque: &[u8; 32],
        clock: Clock,
        ciphertext: &[u8],
    ) -> Result<bool> {
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let changed = Self::adopt_account_intent(&mut tx, opaque, clock, ciphertext)?;
        tx.commit()?;
        Ok(changed)
    }

    fn adopt_account_projection(
        &self,
        record: &Record,
        finish: impl FnOnce(&mut WriteTxn<'_, RedbStorage>) -> Result<bool>,
    ) -> Result<bool> {
        // Same order as local personal selection. No file preparation or
        // network wait holds this guard; an in-flight authorized local
        // operation finishes before an incoming withdrawal is adopted.
        let _consent = if matches!(record, Record::PersonalVolumes { .. }) {
            Some(
                self.personal_consent
                    .write()
                    .map_err(|_| Error::Internal("personal consent lock is poisoned"))?,
            )
        } else {
            None
        };
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if !finish(&mut tx)? {
            return Ok(false);
        }

        match record {
            Record::DisplayName { name } => {
                tx.put(b"settings/display-name", &encode(name)?)?;
            }
            Record::PersonalVolumes { session, grants } => {
                // A capability may arrive after its consent record. Keep
                // the exact decision; personal_grant still requires the
                // locally known volume and this account's ownership.
                for grant in grants {
                    if let Some(bytes) = tx.get(format!("volume/{}", grant.volume).as_bytes())?
                        && decode::<Volume>(&bytes)?.owner != self.account.id
                    {
                        return Err(Error::AccessDenied("personal volume owner"));
                    }
                }
                tx.put(format!("session-mounts/{session}").as_bytes(), &encode(grants)?)?;
            }
            _ => unreachable!(),
        }

        let _commit = crate::trace::Span::new("account.adopt_commit");
        tx.commit()?;
        // Publish the fence before releasing the consent/profile guard.
        self.changes.observed_changed(None);
        Ok(true)
    }

    /// Projection and the observed ciphertext/clock commit atomically. Equal
    /// local intent is already projected and stays dirty until its actual AP
    /// acknowledgment; replay of a staged-but-unsent tree cannot acknowledge it.
    fn adopt_account_intent(
        tx: &mut WriteTxn<'_, RedbStorage>,
        opaque: &[u8; 32],
        clock: Clock,
        ciphertext: &[u8],
    ) -> Result<bool> {
        let storage_key = key(RECORD, opaque);
        if let Some(bytes) = tx.get(&storage_key)?
            && PendingAccountRecord::parts(opaque, &bytes)? >= (clock, ciphertext)
        {
            return Ok(false);
        }
        let mut bytes = Vec::with_capacity(8 + ciphertext.len());
        bytes.extend_from_slice(&clock.as_u64().to_le_bytes());
        bytes.extend_from_slice(ciphertext);
        tx.put(&storage_key, &bytes)?;
        tx.del(&key(DIRTY, opaque))?;
        let previous =
            tx.get(CLOCK)?
                .map(|bytes| {
                    bytes.as_ref().try_into().map(u64::from_le_bytes).map_err(|_| {
                        Error::corrupt("account publication clock must be eight bytes")
                    })
                })
                .transpose()?
                .unwrap_or(0);
        if clock.as_u64() > previous {
            tx.put(CLOCK, &clock.as_u64().to_le_bytes())?;
        }
        Ok(true)
    }

    fn cipher_for(&self, secret: &[u8; 16]) -> Result<Arc<Cipher>> {
        let mut cached = self
            .account_cipher
            .lock()
            .map_err(|_| Error::Internal("account cipher lock is poisoned"))?;
        if let Some(cached) = cached
            .as_ref()
            .filter(|cached| cached.account == self.account.id && cached.secret == *secret)
        {
            return Ok(cached.cipher.clone());
        }
        let cipher = Arc::new(Cipher::new(self.account.id, secret)?);
        *cached = Some(CachedCipher {
            account: self.account.id,
            secret: *secret,
            cipher: cipher.clone(),
        });
        Ok(cipher)
    }

    /// Hosted World retains its own account record. Other automatic scopes stay
    /// local, or carry Shared credentials only in a feature-gated invitation.
    fn account_volume_requires_invitation(&self, id: Id) -> Result<bool> {
        if let Some(binding) = self.automatic_binding(id)? {
            return Ok(binding.role != crate::package::AutomaticVolumeRole::Root
                || !self.replication_supported(id)?);
        }
        Ok(scan::<StoredSession>(&self.metadata, "session/")?
            .iter()
            .any(|stored| stored.invitation.session.shared_volume == Some(id)))
    }

    /// Called with the existing profile mutation transaction. Unsaved/locked
    /// state remains local; explicit setup/recovery subsequently seeds it once.
    pub(super) fn queue_account_record(
        &self,
        tx: &mut WriteTxn<'_, RedbStorage>,
        record: &Record,
    ) -> Result<bool> {
        let excluded = match record {
            Record::Volume { volume, .. } => {
                let binding = tx
                    .get(format!("automatic-volume/{}", volume.id).as_bytes())?
                    .map(|bytes| decode::<automatic::AutomaticBinding>(&bytes))
                    .transpose()?;
                binding.is_some_and(|binding| {
                    binding.role != crate::package::AutomaticVolumeRole::Root
                        || !volume.share_key_with_service
                }) || self.account_volume_requires_invitation(volume.id)?
            }
            Record::VolumeDeleted { volume } => {
                // Deletion carries no credentials. Read the retained Shared
                // identity even after its availability fence has been applied.
                !self.replication_supported(*volume)?
            }
            _ => false,
        };
        if excluded {
            return Ok(false);
        }
        if let Record::PersonalVolumes { session, .. } = record
            && self
                .local_session(session)?
                .is_some_and(|local| local.session.shared_volume.is_none())
        {
            return Ok(false);
        }
        // Scope metadata and its capability arrive as one authoritative record.
        let hosted = match record {
            Record::Volume { volume, credentials } => tx
                .get(format!("automatic-world/{}", volume.id).as_bytes())?
                .map(|bytes| -> Result<Record> {
                    Ok(Record::HostedVolume {
                        volume: volume.clone(),
                        credentials: credentials.clone(),
                        scope: decode(&bytes)?,
                    })
                })
                .transpose()?,
            Record::Invitation { invitation, app_name } => tx
                .get(format!("automatic-session/{}", invitation.session.id).as_bytes())?
                .map(|bytes| -> Result<Record> {
                    Ok(Record::HostedInvitation {
                        invitation: invitation.clone(),
                        app_name: app_name.clone(),
                        scope: decode(&bytes)?,
                    })
                })
                .transpose()?,
            _ => None,
        };
        self.write_account_record_intent(tx, hosted.as_ref().unwrap_or(record))
    }

    fn write_account_record_intent(
        &self,
        tx: &mut WriteTxn<'_, RedbStorage>,
        record: &Record,
    ) -> Result<bool> {
        let _timing = crate::trace::Span::new("account.record_intent");
        self.require_account_authority()?;
        let Some(state) = self
            .transactional_recovery(tx)?
            .filter(|state| state.acknowledged && state.revision != 0)
        else {
            return Ok(false);
        };
        let cipher = self.cipher_for(&state.secret)?;
        let opaque = cipher.record_key(record.logical_key()?.as_bytes())?;
        let storage_key = key(RECORD, &opaque);
        let mut plain = record.encode()?;
        // Reasserting unchanged state is not a new originating event. This also
        // preserves stable bytes when recovery/bootstrap retries after a crash.
        if let Some(bytes) = tx.get(&storage_key)? {
            let previous = PendingAccountRecord::decode(&opaque, &bytes)?;
            let original = cipher.open(&opaque, previous.clock, &previous.ciphertext)?;
            plain = record.rewrite(&original)?;
            if original == plain {
                return Ok(false);
            }
        }
        let previous =
            tx.get(CLOCK)?
                .map(|bytes| {
                    let bytes: [u8; 8] = bytes.as_ref().try_into().map_err(|_| {
                        Error::corrupt("account publication clock must be eight bytes")
                    })?;
                    Ok::<_, Error>(Clock::from_u64(u64::from_le_bytes(bytes)))
                })
                .transpose()?
                .unwrap_or(Clock::ZERO);
        let clock = previous.incr(now_unix_secs())?;
        let record = PendingAccountRecord {
            key: opaque,
            clock,
            ciphertext: cipher.seal(&opaque, clock, &plain)?,
        };
        tx.put(&storage_key, &record.encode())?;
        tx.put(&key(DIRTY, &opaque), &clock.as_u64().to_le_bytes())?;
        tx.put(CLOCK, &clock.as_u64().to_le_bytes())?;
        Ok(true)
    }

    pub(super) fn committed_account_record(&self, changed: bool) {
        if changed {
            self.changes.account_changed();
        }
    }

    /// Read a single pending value, independent of the number of retained
    /// records. The read snapshot sees its marker, clock and ciphertext together.
    pub fn pending_account_record(
        &self,
        after: Option<[u8; 32]>,
    ) -> Result<Option<PendingAccountRecord>> {
        let snapshot = self.metadata.begin_read()?;
        let start = after.as_ref().map(|opaque| key(DIRTY, opaque));
        let mut entries = snapshot.iter_from(start.as_deref().unwrap_or(DIRTY));
        let mut next = entries.next().transpose()?;
        if next.as_ref().is_some_and(|(name, _)| start.as_deref() == Some(name.as_ref())) {
            next = entries.next().transpose()?;
        }
        let Some((marker, clock)) = next else {
            return Ok(None);
        };
        let Some(opaque) = marker.strip_prefix(DIRTY) else {
            return Ok(None);
        };
        let opaque: [u8; 32] = opaque
            .try_into()
            .map_err(|_| Error::corrupt("dirty account record key must be 32 bytes"))?;
        let bytes = snapshot
            .get(&key(RECORD, &opaque))?
            .ok_or(Error::corrupt("dirty account record is missing"))?;
        let record = PendingAccountRecord::decode(&opaque, &bytes)?;
        if clock.as_ref() != record.clock.as_u64().to_le_bytes() {
            return Err(Error::corrupt("dirty account marker clock differs from its record"));
        }
        Ok(Some(record))
    }

    /// The caller supplies the exact record whose remote publication completed.
    /// No catalog scan, data removal, or ciphertext regeneration occurs here.
    pub fn acknowledge_account_record(&self, record: &PendingAccountRecord) -> Result<bool> {
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let matching = tx.get(&key(RECORD, &record.key))?.is_some_and(|bytes| {
            bytes.get(..8) == Some(record.clock.as_u64().to_le_bytes().as_slice())
                && bytes.get(8..) == Some(record.ciphertext.as_slice())
        });
        if !matching || !tx.del(&key(DIRTY, &record.key))? {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// Initialize existing shared state once after explicit secret save/recovery.
    /// The source snapshot retains no writer lock; concurrent mutations already
    /// create their own records and win over this older bootstrap snapshot.
    pub fn seed_account_records(&self) -> Result<()> {
        let source = {
            let _profile = self.lock_profile()?;
            if get::<bool>(&self.metadata, b"account-seeded")?.unwrap_or(false) {
                return Ok(());
            }
            if self.recovery_state()?.is_none_or(|state| !state.acknowledged || state.revision == 0)
            {
                return Err(Error::AccessDenied("save or recover the account secret first"));
            }
            self.metadata.begin_read()?
        };
        for (prefix, kind) in [("volume/", 0), ("session/", 1), ("session-mounts/", 2)] {
            for entry in source.iter_from(prefix.as_bytes()) {
                let (name, bytes) = entry?;
                if !name.starts_with(prefix.as_bytes()) {
                    break;
                }
                if kind == 2 {
                    let session = std::str::from_utf8(&name[prefix.len()..])
                        .map_err(|_| Error::corrupt("personal volume session is not UTF-8"))?;
                    if self
                        .local_session(session)?
                        .is_some_and(|local| local.session.shared_volume.is_none())
                    {
                        continue;
                    }
                }
                let record = match kind {
                    0 => {
                        let volume: Volume = decode(&bytes)?;
                        if self.volume_deleted(volume.id)? {
                            Record::VolumeDeleted { volume: volume.id }
                        } else {
                            if !self.replication_supported(volume.id)?
                                || self.account_volume_requires_invitation(volume.id)?
                            {
                                continue;
                            }
                            let credentials = source
                                .get(format!("credentials/{}", volume.id).as_bytes())?
                                .ok_or(Error::corrupt("volume credentials record is missing"))?;
                            Record::Volume { volume, credentials: decode(&credentials)? }
                        }
                    }
                    1 => Record::Invitation {
                        app_name: decode::<StoredSession>(&bytes)?.app_name,
                        invitation: decode::<StoredSession>(&bytes)?.invitation,
                    },
                    _ => Record::PersonalVolumes {
                        session: std::str::from_utf8(&name[prefix.len()..])
                            .map_err(|_| Error::corrupt("personal volume session is not UTF-8"))?
                            .to_owned(),
                        grants: decode(&bytes)?,
                    },
                };
                self.seed_account_record(&record)?;
            }
        }
        if let Some(bytes) = source.get(b"settings/display-name")? {
            self.seed_account_record(&Record::DisplayName { name: decode(&bytes)? })?;
        }
        let _profile = self.lock_profile()?;
        put(&self.metadata, b"account-seeded", &true)
    }

    fn seed_account_record(&self, record: &Record) -> Result<()> {
        let _profile = self.lock_profile()?;
        let state = self
            .recovery_state()?
            .filter(|state| state.acknowledged && state.revision != 0)
            .ok_or(Error::AccessDenied("account secret is not saved"))?;
        let opaque =
            self.cipher_for(&state.secret)?.record_key(record.logical_key()?.as_bytes())?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if tx.get(&key(RECORD, &opaque))?.is_some() {
            return Ok(());
        }
        let changed = self.queue_account_record(&mut tx, record)?;
        tx.commit()?;
        self.committed_account_record(changed);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn unlock(store: &LocalStore, acknowledged: bool) {
        store
            .save_recovery_state(&RecoveryState {
                secret: [7; 16],
                acknowledged,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();
    }

    fn sealed(store: &LocalStore, record: Record, clock: Clock) -> PendingAccountRecord {
        let cipher = store.unlocked_account_cipher().unwrap();
        let key = cipher.record_key(record.logical_key().unwrap().as_bytes()).unwrap();
        PendingAccountRecord {
            key,
            clock,
            ciphertext: cipher.seal(&key, clock, &record.encode().unwrap()).unwrap(),
        }
    }

    #[test]
    fn display_name_is_durable_and_queues_one_validated_account_record() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        assert!(store.set_display_name("Robin").is_err());
        unlock(&store, true);
        store.set_display_name("Robin 雨 🌱").unwrap();
        let pending = store.pending_account_record(None).unwrap().unwrap();
        assert!(
            matches!(plaintext(&store, &pending), Record::DisplayName { name } if name == "Robin 雨 🌱")
        );
        for invalid in ["", " Robin", "Robin ", "Ro\nbin", "\0", &"x".repeat(257)] {
            assert!(store.set_display_name(invalid).is_err());
        }
        store.set_display_name("Robin 雨 🌱").unwrap();
        assert_eq!(store.pending_account_record(None).unwrap().unwrap(), pending);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.display_name().unwrap().as_deref(), Some("Robin 雨 🌱"));
        assert_eq!(store.pending_account_record(None).unwrap().unwrap(), pending);
    }

    #[test]
    fn incoming_display_name_survives_restart_without_creating_an_echo() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        unlock(&source, true);
        source.set_display_name("Robin").unwrap();
        let record = source.pending_account_record(None).unwrap().unwrap();
        let path = directory.path().join("target");
        let mut target = LocalStore::open(&path).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target, true);
        target.stage_account_record(&record).unwrap().unwrap();
        drop(target);
        let target = LocalStore::open(&path).unwrap();
        target.reconcile_account_records().unwrap();
        assert_eq!(target.display_name().unwrap().as_deref(), Some("Robin"));
        assert!(target.pending_account_record(None).unwrap().is_none());
        target.set_display_name("Robin updated").unwrap();
        let changed = target.pending_account_record(None).unwrap().unwrap();
        assert!(changed.clock > record.clock);
        source.stage_account_record(&changed).unwrap().unwrap();
        source.reconcile_account_records().unwrap();
        assert_eq!(source.display_name().unwrap(), target.display_name().unwrap());
        assert!(source.pending_account_record(None).unwrap().is_none());
    }

    #[test]
    #[ignore = "durable display-name save diagnostic; run alone with --release"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "Keep executable checks of the compile-time capacity contract in this regression test."
    )]
    fn benchmark_display_name_save() {
        assert!(!cfg!(debug_assertions));
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let names = ["Robin".to_owned(), "é".repeat(128)];
        let mut samples = Vec::with_capacity(1000);
        for index in 0..1100 {
            let name = &names[index % 2];
            let start = std::time::Instant::now();
            store.set_display_name(std::hint::black_box(name)).unwrap();
            let elapsed = start.elapsed().as_nanos();
            if index >= 100 {
                samples.push(elapsed);
            }
            assert_eq!(store.display_name().unwrap().as_deref(), Some(name.as_str()));
        }
        let record = store.pending_account_record(None).unwrap().unwrap();
        assert!(
            matches!(plaintext(&store, &record), Record::DisplayName { name } if name == names[1])
        );
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.display_name().unwrap().as_deref(), Some(names[1].as_str()));
        assert_eq!(store.pending_account_record(None).unwrap().unwrap(), record);
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        crate::diagnostic!(
            "display-name-save {}",
            serde_json::json!({
                "boundary":"LocalStore::set_display_name: validation, profile lock, metadata transaction, outbox intent, durable commit and notification; no GUI or network",
                "samples_ns":samples, "warmup":100, "name_bytes":[5,256],
                "p50_ns":sorted[499], "p99_ns":sorted[989], "max_ns":sorted[999],
                "p99_budget_ns":2_000_000
            })
        );
        assert!(sorted[989] < 2_000_000, "display-name local save p99 exceeds 2 ms");
    }

    #[test]
    fn account_adoption_replays_after_restart_without_local_registration_or_new_intent() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        unlock(&source, true);
        let volume = source.create_volume("Other device", true).unwrap();
        let record = source.pending_account_record(None).unwrap().unwrap();
        let target_path = directory.path().join("target");
        let mut target = LocalStore::open(&target_path).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target, true);
        let seed = target.account_seed().unwrap();
        target.stage_account_record(&record).unwrap().unwrap();
        assert!(target.volumes().unwrap().is_empty());
        drop(target); // Account commit reached disk; profile adoption did not.
        let target = LocalStore::open(&target_path).unwrap();
        assert!(target.reconcile_account_records().unwrap());
        assert_eq!(target.volume(volume.id).unwrap(), volume);
        assert_eq!(target.credentials(volume.id).unwrap(), source.credentials(volume.id).unwrap());
        assert_eq!(target.account_seed().unwrap(), seed);
        assert!(target.pending_operations().unwrap().is_empty());
        assert!(target.pending_account_record(None).unwrap().is_none());
        assert!(!target.adopt_account_record(&record.key).unwrap());
        assert_eq!(target.volumes().unwrap().len(), 1);
    }

    #[test]
    fn deleted_capability_records_do_not_block_unrelated_account_records() {
        // A tombstone can arrive before its older capability. Keep deletion
        // authoritative while allowing a different volume to be discovered.
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        unlock(&source, true);
        let retired = source.create_volume("Retired", true).unwrap();
        let current = source.create_volume("Current", true).unwrap();
        let build = crate::identifier::encode(&[0xab; 32]);
        let session = source
            .create_session(
                &crate::package::AppIdentity {
                    publisher: source.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                retired.id,
            )
            .unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target, true);
        let deletion =
            sealed(&source, Record::VolumeDeleted { volume: retired.id }, Clock::from_u64(102));
        let old = sealed(
            &source,
            Record::Volume {
                volume: retired.clone(),
                credentials: source.credentials(retired.id).unwrap(),
            },
            Clock::from_u64(100),
        );
        let unrelated = sealed(
            &source,
            Record::Volume {
                volume: current.clone(),
                credentials: source.credentials(current.id).unwrap(),
            },
            Clock::from_u64(103),
        );
        let invitation = sealed(
            &source,
            Record::Invitation {
                app_name: Some("Retired app".into()),
                invitation: source.stored_session(&session.id).unwrap().invitation,
            },
            Clock::from_u64(100),
        );
        for record in [&deletion, &old, &invitation, &unrelated] {
            let delta = record_delta(&target, std::slice::from_ref(record));
            target.receive_account_delta(&delta).unwrap();
            assert!(!target.receive_account_delta(&delta).unwrap());
        }
        assert!(target.volume(retired.id).is_err());
        assert!(target.sessions().unwrap().is_empty());
        assert_eq!(target.volume(current.id).unwrap(), current);
        assert!(target.pending_account_record(None).unwrap().is_none());
    }

    #[test]
    fn received_invitation_preserves_adoption_and_rejects_changed_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        unlock(&source, true);
        let root = source.create_volume("Root", true).unwrap();
        let session = source
            .create_session(
                &crate::package::AppIdentity {
                    publisher: source.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: crate::identifier::encode(&[0xab; 32]) },
                &crate::identifier::encode(&[0xab; 32]),
                root.id,
            )
            .unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target, true);
        let invitation = sealed(
            &source,
            Record::Invitation {
                app_name: Some("Notes".into()),
                invitation: source.stored_session(&session.id).unwrap().invitation,
            },
            Clock::from_u64(100),
        );
        target.stage_account_record(&invitation).unwrap().unwrap();
        // The invitation carries both capabilities; arrival need not wait for
        // the two independent volume records.
        assert!(target.adopt_account_record(&invitation.key).unwrap());
        assert_eq!(target.session(&session.id).unwrap(), session);
        assert_eq!(target.session_app_names().unwrap()[&session.id], "Notes");
        let mut adopted = target.stored_session(&session.id).unwrap();
        adopted.session.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
        adopted.session.selector =
            BuildSelector::Exact { build_id: crate::identifier::encode(&[0xcd; 32]) };
        put(&target.metadata, format!("session/{}", session.id).as_bytes(), &adopted).unwrap();
        assert!(!target.record_session_denial(&session, "old binding denied").unwrap());
        assert!(target.record_session_denial(&adopted.session, "locally denied").unwrap());
        let later = sealed(
            &source,
            Record::Invitation {
                app_name: None,
                invitation: source.stored_session(&session.id).unwrap().invitation,
            },
            Clock::from_u64(101),
        );
        target.stage_account_record(&later).unwrap().unwrap();
        assert!(target.adopt_account_record(&later.key).unwrap());
        assert_eq!(target.session(&session.id).unwrap(), adopted.session);
        assert_eq!(target.stored_session(&session.id).unwrap().invitation, adopted.invitation);
        assert_eq!(target.session_denial(&session.id).unwrap().as_deref(), Some("locally denied"));
        assert!(target.pending_operations().unwrap().is_empty());
        let bad = sealed(
            &source,
            Record::Volume {
                volume: root.clone(),
                credentials: Credentials { database_key: [8; 32], blob_key: [9; 32] },
            },
            Clock::from_u64(102),
        );
        target.stage_account_record(&bad).unwrap().unwrap();
        assert!(target.adopt_account_record(&bad.key).is_err());
        assert_eq!(target.credentials(root.id).unwrap(), source.credentials(root.id).unwrap());
        assert!(
            target.metadata.begin_read().unwrap().get(&key(RECORD, &bad.key)).unwrap().is_none()
        );
    }

    #[test]
    fn account_consent_arrival_order_and_newer_local_intent_preserve_authority() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        unlock(&source, true);
        let root = source.create_volume("Root", true).unwrap();
        let personal = source.create_volume("Personal", true).unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = source
            .create_session(
                &crate::package::AppIdentity {
                    publisher: source.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target, true);
        let grant = PersonalVolumeSelection {
            volume: personal.id,
            write: false,
            incarnation: Id::generate().unwrap(),
        };
        let consent = sealed(
            &source,
            Record::PersonalVolumes { session: session.id.clone(), grants: vec![grant] },
            Clock::from_u64(100),
        );
        target.stage_account_record(&consent).unwrap().unwrap();
        assert!(target.adopt_account_record(&consent.key).unwrap());
        let access = Access::session_version(target.account.id, &session.id, &version);
        assert!(
            target.personal_grant(&access, &session, personal.id).is_err(),
            "missing capability cannot be used"
        );
        let capability = sealed(
            &source,
            Record::Volume {
                volume: personal.clone(),
                credentials: source.credentials(personal.id).unwrap(),
            },
            Clock::from_u64(101),
        );
        target.stage_account_record(&capability).unwrap().unwrap();
        target.adopt_account_record(&capability.key).unwrap();
        assert!(!target.personal_grant(&access, &session, personal.id).unwrap().write);
        let mut other_version = session.clone();
        other_version.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
        assert!(target.personal_grant(&access, &other_version, personal.id).is_ok());
        other_version.id = crate::identifier::encode(&[0xde; 32]);
        assert!(target.personal_grant(&access, &other_version, personal.id).is_err());
        // A local withdrawal committed after receipt but before adoption wins.
        let incoming = sealed(
            &source,
            Record::PersonalVolumes {
                session: session.id.clone(),
                grants: vec![PersonalVolumeSelection {
                    volume: personal.id,
                    write: true,
                    incarnation: Id::generate().unwrap(),
                }],
            },
            Clock::from_u64(102),
        );
        target.stage_account_record(&incoming).unwrap().unwrap();
        target.select_personal_volumes(&session.id, vec![]).unwrap();
        let pending = target.pending_account_record(None).unwrap().unwrap();
        assert!(pending.clock > incoming.clock);
        assert!(!target.adopt_account_record(&incoming.key).unwrap());
        assert_eq!(target.personal_volume_grants(&session.id).unwrap(), Some(vec![]));
        assert_eq!(target.pending_account_record(None).unwrap().unwrap(), pending);
        // Staging and locally replaying the withdrawal is not a remote ACK.
        target.stage_account_record(&pending).unwrap().unwrap();
        assert!(!target.adopt_account_record(&pending.key).unwrap());
        assert_eq!(target.pending_account_record(None).unwrap().unwrap(), pending);
    }

    fn record_delta(store: &LocalStore, records: &[PendingAccountRecord]) -> Vec<u8> {
        let database = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &CreateOptions::syncable(crate::store::account::hash_key(store.account.id), 1),
        )
        .unwrap();
        let mut tx = database
            .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
            .unwrap();
        for record in records {
            tx.put_with_clock(&record.key, &record.ciphertext, record.clock).unwrap();
        }
        let kvdb::CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
        delta
    }

    #[test]
    fn concurrent_account_publication_and_receipt_preserve_the_newest_record() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let publication = scope.spawn(|| {
                let mut failure = None;
                for i in 0..256 {
                    let record = sealed(
                        &store,
                        Record::DisplayName { name: "false".into() },
                        Clock::from_u64(i * 2 + 1),
                    );
                    barrier.wait();
                    if let Err(error) = store.stage_account_record(&record) {
                        failure = Some(error);
                    }
                }
                failure
            });
            let receipt = scope.spawn(|| {
                let mut failure = None;
                for i in 0..256 {
                    let record = sealed(
                        &store,
                        Record::DisplayName { name: "true".into() },
                        Clock::from_u64(i * 2 + 2),
                    );
                    let delta = record_delta(&store, &[record]);
                    barrier.wait();
                    if let Err(error) = store.receive_account_delta(&delta) {
                        failure = Some(error);
                    }
                }
                failure
            });
            let publication = publication.join().unwrap();
            let receipt = receipt.join().unwrap();
            assert!(publication.is_none(), "publication failed: {publication:?}");
            assert!(receipt.is_none(), "receipt failed: {receipt:?}");
        });
        let newest =
            sealed(&store, Record::DisplayName { name: "true".into() }, Clock::from_u64(512));
        let db = store.account_replica().unwrap();
        let snapshot = db.begin_read().unwrap();
        let (clock, value) = snapshot.get_with_clock(&newest.key).unwrap().unwrap();
        assert_eq!(clock, newest.clock);
        assert!(matches!(
            Record::open(&store.unlocked_account_cipher().unwrap(), &newest.key, clock, &value)
                .unwrap(),
            Some(Record::DisplayName { name }) if name == "true"
        ));
    }

    fn account_snapshot(
        target: &LocalStore,
        records: &[PendingAccountRecord],
    ) -> kvdb::sync::PreparedChanges<kvdb::redb::FileStaging> {
        let source = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &CreateOptions::syncable(crate::store::account::hash_key(target.account.id), 1),
        )
        .unwrap();
        let mut tx = source.begin_write(0).unwrap();
        for record in records {
            tx.put_with_clock(&record.key, &record.ciphertext, record.clock).unwrap();
        }
        tx.commit().unwrap();
        let database = target.account_replica().unwrap();
        let mut request = Vec::new();
        let mut receiver = kvdb::sync::PreparedSync::new(
            &database,
            crate::store::sync_staging(&std::env::temp_dir()).unwrap(),
            crate::store::sync_journal(&std::env::temp_dir(), 1024 * 1024).unwrap(),
            1024 * 1024,
            &mut request,
        )
        .unwrap();
        let mut sender = kvdb::SyncSender::new(&source).unwrap();
        let mut chunk = [0; 137];
        loop {
            assert!(!request.is_empty());
            sender.process(&request).unwrap();
            request.clear();
            let mut done = false;
            loop {
                let count = sender.write_output(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                done = receiver.process(&chunk[..count], &mut request).unwrap()
                    == kvdb::sync::SyncStatus::Done;
            }
            if done {
                sender.process(&request).unwrap();
                break;
            }
        }
        receiver.finish().unwrap()
    }

    fn invalid_display_record() -> Vec<u8> {
        let mut plain = Record::DisplayName { name: "false".into() }.encode().unwrap();
        let offset = plain.windows(6).rposition(|bytes| bytes == b"\x05false").unwrap();
        plain[offset] = crate::store::catalog::record::BOOLEAN;
        plain
    }

    fn optional_record(store: &LocalStore, authority: bool) -> PendingAccountRecord {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/account-plaintext-v1.json"))
                .unwrap();
        let mut plain: Vec<u8> = fixture["optional_hex"]
            .as_str()
            .unwrap()
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        if authority {
            use crate::store::catalog::record;
            let value = record::View::record(&plain)
                .unwrap()
                .fields(record::Mode::Write, &[1, 2, 3, 4, 5, 6])
                .unwrap()
                .get(3)
                .unwrap();
            let offset = value.0.as_ptr() as usize - plain.as_ptr() as usize;
            plain[offset + 1] = 1;
        }
        let cipher = store.unlocked_account_cipher().unwrap();
        let key = cipher.record_key(b"optional/future.presentation/one").unwrap();
        let clock = Clock::from_u64(100);
        PendingAccountRecord { key, clock, ciphertext: cipher.seal(&key, clock, &plain).unwrap() }
    }

    fn unknown_selector_record(store: &LocalStore, session: &Session) -> PendingAccountRecord {
        use crate::store::catalog::record;
        let invitation = Record::Invitation {
            invitation: store.stored_session(&session.id).unwrap().invitation,
            app_name: None,
        };
        let mut plain = invitation.encode().unwrap();
        // Replace the selector with future unit variant 4, keeping every other
        // byte and updating only its four enclosing field lengths.
        let (offset, length, lengths) = {
            let payload = record::View::record(&plain)
                .unwrap()
                .fields(record::Mode::Write, &[1, 2, 3, 4, 5, 6])
                .unwrap()
                .get(6)
                .unwrap();
            let (_, body) = record::View::record(payload.body(record::BYTES).unwrap())
                .unwrap()
                .variant()
                .unwrap();
            let invitation = body.fields(record::Mode::Write, &[1]).unwrap().get(1).unwrap();
            let session =
                invitation.fields(record::Mode::Write, &[1, 2, 3, 4]).unwrap().get(2).unwrap();
            let selector = session
                .fields(record::Mode::Write, &[1, 2, 3, 4, 5, 6, 7])
                .unwrap()
                .get(3)
                .unwrap();
            let offset = selector.0.as_ptr() as usize - plain.as_ptr() as usize;
            let lengths = [payload, invitation, session, selector]
                .map(|value| value.0.as_ptr() as usize - plain.as_ptr() as usize - 4);
            (offset, selector.0.len(), lengths)
        };
        for position in lengths {
            let previous = u32::from_le_bytes(plain[position..position + 4].try_into().unwrap());
            plain[position..position + 4]
                .copy_from_slice(&(previous - (length as u32 - 4)).to_le_bytes());
        }
        plain.splice(offset..offset + length, [record::VARIANT, 4, 0, record::STRUCT]);
        let browser: serde_json::Value =
            serde_json::from_slice(&Record::browser_plaintext(&plain).unwrap()).unwrap();
        assert_eq!(browser["unsupported"], true);
        assert!(browser["record"].is_null());
        let cipher = store.unlocked_account_cipher().unwrap();
        let key = cipher.record_key(invitation.logical_key().unwrap().as_bytes()).unwrap();
        let clock = Clock::from_u64(100);
        PendingAccountRecord { key, clock, ciphertext: cipher.seal(&key, clock, &plain).unwrap() }
    }

    #[test]
    fn optional_unknown_account_records_survive_receipt_restart_and_known_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let unknown = optional_record(&store, false);
        let mut known =
            sealed(&store, Record::DisplayName { name: "true".into() }, Clock::from_u64(101));
        let cipher = store.unlocked_account_cipher().unwrap();
        let mut plain = cipher.open(&known.key, known.clock, &known.ciphertext).unwrap();
        let extension = [99, 0, 3, 0, 0, 0, 255, 7, 9];
        plain.extend_from_slice(&extension);
        known.ciphertext = cipher.seal(&known.key, known.clock, &plain).unwrap();
        let delta = record_delta(&store, &[unknown.clone(), known]);
        store.receive_account_delta(&delta).unwrap();
        assert_eq!(store.display_name().unwrap().as_deref(), Some("true"));
        store.set_display_name("false").unwrap();
        let edited = store.pending_account_record(None).unwrap().unwrap();
        let edited = cipher.open(&edited.key, edited.clock, &edited.ciphertext).unwrap();
        assert!(edited.ends_with(&extension));
        let volume = store.create_volume("Familiar volume", true).unwrap();
        drop(store);
        let reopened = LocalStore::open(directory.path()).unwrap();
        reopened.reconcile_account_records().unwrap();
        assert_eq!(reopened.display_name().unwrap().as_deref(), Some("false"));
        assert_eq!(reopened.volume(volume.id).unwrap().name, "Familiar volume");
        let tree = reopened.account_replica().unwrap();
        let snapshot = tree.begin_read().unwrap();
        let (clock, bytes) = snapshot.get_with_clock(&unknown.key).unwrap().unwrap();
        assert_eq!((clock, bytes.as_ref()), (unknown.clock, unknown.ciphertext.as_slice()));
    }

    #[test]
    fn unsupported_account_authority_retains_grants_and_blocks_use_across_restart() {
        // Authenticate future kinds and nested selectors through both receipt
        // paths; reopening must retain evidence and keep saved grants unusable.
        for (snapshot, unknown_selector) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let directory = tempfile::tempdir().unwrap();
            let store = LocalStore::open(directory.path()).unwrap();
            unlock(&store, true);
            let root = store.create_volume("Root", true).unwrap();
            let personal = store.create_volume("Personal", true).unwrap();
            let version = crate::identifier::encode(&[0xab; 32]);
            let session = store
                .create_session(
                    &crate::package::AppIdentity {
                        publisher: store.account.id,
                        app_id: "test".into(),
                    },
                    BuildSelector::Exact { build_id: version.clone() },
                    &version,
                    root.id,
                )
                .unwrap();
            store
                .select_personal_volumes(
                    &session.id,
                    vec![PersonalVolumeGrant { volume: personal.id, write: true }],
                )
                .unwrap();
            let access = Access::session_version(store.account.id, &session.id, &version);
            assert!(store.personal_grant(&access, &session, personal.id).unwrap().write);
            let pending = store.pending_account_record(None).unwrap();
            let unknown = if unknown_selector {
                unknown_selector_record(&store, &session)
            } else {
                optional_record(&store, true)
            };
            let tree = store.account_replica().unwrap();
            let before = (tree.clock(), tree.begin_read().unwrap().root_hash().unwrap());
            let known =
                sealed(&store, Record::DisplayName { name: "prefix".into() }, Clock::from_u64(101));
            let delta = record_delta(&store, &[known.clone(), unknown.clone()]);
            let error = if snapshot {
                store
                    .merge_account_snapshot(account_snapshot(&store, &[known, unknown.clone()]))
                    .unwrap_err()
            } else {
                store.receive_account_delta(&delta).unwrap_err()
            };
            assert!(crate::store::account::is_unsupported(&error));
            assert_eq!((tree.clock(), tree.begin_read().unwrap().root_hash().unwrap()), before);
            assert!(store.display_name().unwrap().is_none());
            assert!(store.personal_grant(&access, &session, personal.id).is_err());
            assert!(store.session_seed(&session.id).is_err());
            assert!(store.select_personal_volumes(&session.id, vec![]).is_err());
            assert_eq!(store.pending_account_record(None).unwrap(), pending);
            assert!(
                store
                    .metadata
                    .begin_read()
                    .unwrap()
                    .has(format!("credentials/{}", personal.id).as_bytes())
                    .unwrap()
            );
            drop(tree);
            drop(store);
            let reopened = LocalStore::open(directory.path()).unwrap();
            assert!(reopened.personal_grant(&access, &session, personal.id).is_err());
            assert!(reopened.reconcile_account_records().is_err());
            assert!(reopened.select_personal_volumes(&session.id, vec![]).is_err());
            assert_eq!(reopened.pending_account_record(None).unwrap(), pending);
            let retained =
                reopened.metadata.begin_read().unwrap().get(QUARANTINE).unwrap().unwrap().to_vec();
            assert_eq!(&retained[..32], unknown.key);
            assert_eq!(
                u64::from_le_bytes(retained[32..40].try_into().unwrap()),
                unknown.clock.as_u64()
            );
            assert_eq!(&retained[40..], unknown.ciphertext);
        }
    }

    #[test]
    fn account_snapshot_preserves_concurrent_winner_and_rejects_invalid_tail_and_loser() {
        let directory = tempfile::tempdir().unwrap();
        let target = LocalStore::open(directory.path()).unwrap();
        unlock(&target, true);
        let remote =
            sealed(&target, Record::DisplayName { name: "false".into() }, Clock::from_u64(100));
        let prepared = account_snapshot(&target, std::slice::from_ref(&remote));
        // The prepared network snapshot must not retain a writer. A newer
        // local receive commits while the completed journal remains alive.
        let winner =
            sealed(&target, Record::DisplayName { name: "true".into() }, Clock::from_u64(200));
        target
            .receive_account_delta(&record_delta(&target, std::slice::from_ref(&winner)))
            .unwrap();
        target.merge_account_snapshot(prepared).unwrap();
        assert_eq!(target.display_name().unwrap().as_deref(), Some("true"));
        let before = target.account_replica().unwrap().begin_read().unwrap().root_hash().unwrap();
        let cipher = target.unlocked_account_cipher().unwrap();
        let mut loser = remote;
        loser.ciphertext = cipher.seal(&loser.key, loser.clock, &invalid_display_record()).unwrap();
        let error = target
            .merge_account_snapshot(account_snapshot(&target, &[loser]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("incorrect record wire kind"), "{error}");
        assert_eq!(
            target.account_replica().unwrap().begin_read().unwrap().root_hash().unwrap(),
            before
        );
        let valid =
            sealed(&target, Record::DisplayName { name: "false".into() }, Clock::from_u64(300));
        let key = [0xff; 32];
        let tail = PendingAccountRecord {
            key,
            clock: Clock::from_u64(301),
            ciphertext: cipher
                .seal(
                    &key,
                    Clock::from_u64(301),
                    &Record::DisplayName { name: "false".into() }.encode().unwrap(),
                )
                .unwrap(),
        };
        assert!(matches!(
            target.merge_account_snapshot(account_snapshot(&target, &[valid, tail])),
            Err(Error::InvalidInput("account record key does not match its contents"))
        ));
        assert_eq!(
            target.account_replica().unwrap().begin_read().unwrap().root_hash().unwrap(),
            before
        );
        assert_eq!(target.display_name().unwrap().as_deref(), Some("true"));
        assert!(target.pending_account_record(None).unwrap().is_none());
    }

    #[test]
    fn whole_account_frame_rejects_invalid_typed_tail_and_loser_before_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let target = LocalStore::open(directory.path()).unwrap();
        unlock(&target, true);
        let valid =
            sealed(&target, Record::DisplayName { name: "false".into() }, Clock::from_u64(100));
        let cipher = target.unlocked_account_cipher().unwrap();
        let invalid_key = [0xff; 32];
        let bad = PendingAccountRecord {
            key: invalid_key,
            clock: Clock::from_u64(101),
            ciphertext: cipher
                .seal(
                    &invalid_key,
                    Clock::from_u64(101),
                    &Record::DisplayName { name: "true".into() }.encode().unwrap(),
                )
                .unwrap(),
        };
        // A valid authenticated ciphertext with the wrong logical binding is
        // rejected before even creating the account file. Prove that the valid
        // entry precedes the bad one in the actual encoded frame.
        let delta = record_delta(&target, &[valid.clone(), bad]);
        let mut order = Vec::new();
        kvdb::sync::visit_delta(&delta, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
            order.push(key.to_vec())
        })
        .unwrap();
        assert_eq!(order, [valid.key.to_vec(), invalid_key.to_vec()]);
        assert!(target.receive_account_delta(&delta).is_err());
        assert!(!target.path.join(crate::paths::ACCOUNT_DB).exists());
        assert!(target.display_name().unwrap().is_none());
        assert!(
            target
                .receive_account_delta(&record_delta(&target, std::slice::from_ref(&valid)))
                .unwrap()
        );
        assert_eq!(target.display_name().unwrap().as_deref(), Some("false"));
        let before = target.account_replica().unwrap().begin_read().unwrap().root_hash().unwrap();
        let mut malformed_loser = valid.clone();
        malformed_loser.clock = Clock::from_u64(1);
        malformed_loser.ciphertext =
            cipher.seal(&valid.key, malformed_loser.clock, &invalid_display_record()).unwrap();
        assert!(target.receive_account_delta(&record_delta(&target, &[malformed_loser])).is_err());
        assert_eq!(
            target.account_replica().unwrap().begin_read().unwrap().root_hash().unwrap(),
            before
        );
        assert!(!target.receive_account_delta(&record_delta(&target, &[valid])).unwrap());
        assert!(target.pending_account_record(None).unwrap().is_none());
    }

    #[test]
    fn foreign_capability_discovery_waits_for_invitation_in_either_record_order() {
        use crate::store::change_feed::CommittedChangeKind;
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let root = source.create_volume("Shared root", true).unwrap();
        let version = crate::identifier::encode(&[0x11; 32]);
        let session = source
            .create_session(
                &crate::package::AppIdentity {
                    publisher: source.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        for invitation_first in [false, true] {
            let target =
                LocalStore::open(directory.path().join(format!("target-{invitation_first}")))
                    .unwrap();
            unlock(&target, true);
            let mut feed = target.take_change_receiver().unwrap();
            let volume = sealed(
                &target,
                Record::Volume {
                    volume: root.clone(),
                    credentials: source.credentials(root.id).unwrap(),
                },
                Clock::from_u64(100),
            );
            let invitation = sealed(
                &target,
                Record::Invitation {
                    app_name: None,
                    invitation: source.stored_session(&session.id).unwrap().invitation,
                },
                Clock::from_u64(101),
            );
            let records =
                if invitation_first { [&invitation, &volume] } else { [&volume, &invitation] };
            for record in records {
                let delta = record_delta(&target, std::slice::from_ref(record));
                assert!(target.receive_account_delta(&delta).unwrap());
                let batch = feed.drain();
                if record.key == invitation.key {
                    let ids = batch
                        .records
                        .iter()
                        .map(|event| {
                            assert_eq!(event.kind(), CommittedChangeKind::VolumeAvailable);
                            assert!(event.bytes().is_empty());
                            event.volume()
                        })
                        .collect::<BTreeSet<_>>();
                    assert_eq!(ids, [session.root_volume].into_iter().collect());
                } else {
                    assert!(
                        batch.records.is_empty(),
                        "a foreign volume alone cannot authorize initialization"
                    );
                }
                assert!(!target.receive_account_delta(&delta).unwrap());
                assert!(
                    feed.drain().records.is_empty(),
                    "duplicate receipt cannot restart initialization"
                );
            }
            let setting =
                sealed(&target, Record::DisplayName { name: "false".into() }, Clock::from_u64(102));
            target.receive_account_delta(&record_delta(&target, &[setting])).unwrap();
            assert!(
                feed.drain().records.is_empty(),
                "settings are live records, not database freshness notifications"
            );
        }
    }

    #[test]
    fn account_receive_refuses_foreign_personal_capability_without_installing_consent() {
        let directory = tempfile::tempdir().unwrap();
        let target = LocalStore::open(directory.path()).unwrap();
        unlock(&target, true);
        let mut foreign = target.create_volume("Other person's volume", false).unwrap();
        foreign.owner = Id::from_bytes([8; 16]);
        put(&target.metadata, format!("volume/{}", foreign.id).as_bytes(), &foreign).unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let record = sealed(
            &target,
            Record::PersonalVolumes {
                session: version.clone(),
                grants: vec![PersonalVolumeSelection {
                    volume: foreign.id,
                    write: true,
                    incarnation: Id::generate().unwrap(),
                }],
            },
            Clock::from_u64(100),
        );
        assert!(
            target
                .receive_account_delta(&record_delta(&target, std::slice::from_ref(&record)))
                .is_err()
        );
        assert_eq!(target.personal_volume_grants(&version).unwrap(), None);
        assert!(!target.path.join(crate::paths::ACCOUNT_DB).exists());
        assert!(
            target.metadata.begin_read().unwrap().get(&key(RECORD, &record.key)).unwrap().is_none()
        );
    }

    #[test]
    fn account_adoption_uses_ciphertext_ties_and_advances_local_origin_clock() {
        let directory = tempfile::tempdir().unwrap();
        let target = LocalStore::open(directory.path()).unwrap();
        unlock(&target, true);
        let clock = Clock::from_u64((1u64 << 60) + 10);
        let mut records = [
            sealed(&target, Record::DisplayName { name: "false".into() }, clock),
            sealed(&target, Record::DisplayName { name: "true".into() }, clock),
        ];
        records.sort_by(|a, b| a.ciphertext.cmp(&b.ciphertext));
        for record in &records {
            target.stage_account_record(record).unwrap().unwrap();
            assert!(target.adopt_account_record(&record.key).unwrap());
            assert!(target.pending_account_record(None).unwrap().is_none());
        }
        let cipher = target.unlocked_account_cipher().unwrap();
        let Some(Record::DisplayName { name }) =
            Record::open(&cipher, &records[1].key, clock, &records[1].ciphertext).unwrap()
        else {
            panic!()
        };
        assert_eq!(target.display_name().unwrap().as_deref(), Some(name.as_str()));
        assert!(target.stage_account_record(&records[0]).unwrap().is_none());
        assert!(!target.adopt_account_record(&records[0].key).unwrap());
        target.set_display_name(if name == "true" { "false" } else { "true" }).unwrap();
        assert!(target.pending_account_record(None).unwrap().unwrap().clock > clock);
    }

    #[test]
    fn account_replica_shares_cache_without_aliasing_a_volume_and_preserves_missing_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        assert!(store.account_replica().is_err());
        assert!(!directory.path().join(crate::paths::ACCOUNT_DB).exists());
        let volume = store.new_volume(store.account.id, "Distinct source", false, true).unwrap();
        unlock(&store, true);
        let account = store.account_replica().unwrap();
        let volume = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        assert!(!Arc::ptr_eq(&account, &volume));
        assert!(Arc::ptr_eq(&account, &store.account_replica().unwrap()));
        assert_ne!(account.meta().hash_key, volume.meta().hash_key);
        assert!(store.open.lock().unwrap().by_id.contains_key(&DatabaseKey::Account));
        assert!(store.open.lock().unwrap().by_id.len() <= OPEN_DATABASE_LIMIT);
        drop(volume);
        drop(account);
        fs::remove_file(directory.path().join(crate::paths::ACCOUNT_DB)).unwrap();
        assert!(store.account_replica().is_err());
        assert!(!directory.path().join(crate::paths::ACCOUNT_DB).exists());
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert!(store.account_replica().is_err());
        assert!(!directory.path().join(crate::paths::ACCOUNT_DB).exists());
    }

    #[test]
    fn account_staging_replays_origin_and_never_overwrites_a_newer_tree_entry() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        store.set_display_name("false").unwrap();
        let pending = store.pending_account_record(None).unwrap().unwrap();
        let cipher = store.unlocked_account_cipher().unwrap();
        for _ in 0..2 {
            let delta = store.stage_account_record(&pending).unwrap().unwrap();
            let mut count = 0;
            kvdb::sync::visit_delta(&delta, crate::store::MAX_REPLICA_DELTA_BYTES, |key, entry| {
                let kvdb::sync::EntState::Present { clock, value } = entry else { panic!() };
                assert_eq!(key, pending.key);
                assert_eq!(clock, pending.clock);
                assert_eq!(value, pending.ciphertext);
                count += 1;
            })
            .unwrap();
            assert_eq!(count, 1);
        }
        let account = store.account_replica().unwrap();
        std::thread::scope(|scope| {
            let _admission = store.lock_account_writer().unwrap();
            let writer = account.begin_write(now_unix_secs()).unwrap();
            let (sent, received) = std::sync::mpsc::channel();
            let source = &store;
            let record = &pending;
            let worker = scope.spawn(move || {
                sent.send(source.stage_account_record(record)).unwrap();
            });
            let result = received.recv_timeout(std::time::Duration::from_secs(5));
            drop(writer); // Release even on regression, so test cleanup cannot hang.
            worker.join().unwrap();
            assert!(result.expect("replay waited for an unrelated writer").unwrap().is_some());
        });
        let retained = account.begin_read().unwrap();
        let newer = pending.clock.incr(now_unix_secs() + 1).unwrap();
        let ciphertext = cipher
            .seal(
                &pending.key,
                newer,
                &Record::DisplayName { name: "true".into() }.encode().unwrap(),
            )
            .unwrap();
        let mut tx = account.begin_write(now_unix_secs()).unwrap();
        tx.put_with_clock(&pending.key, &ciphertext, newer).unwrap();
        tx.commit().unwrap();
        let before = account.begin_read().unwrap().root_hash().unwrap();
        assert!(store.stage_account_record(&pending).unwrap().is_none());
        assert_eq!(account.begin_read().unwrap().root_hash().unwrap(), before);
        let (clock, value) = retained.get_with_clock(&pending.key).unwrap().unwrap();
        assert_eq!(clock, pending.clock);
        assert_eq!(&*value, pending.ciphertext);
        assert_eq!(store.pending_account_record(None).unwrap(), Some(pending));
    }

    #[test]
    fn staging_rejects_authenticated_wrong_keys_and_resolves_equal_clock_values() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let cipher = store.unlocked_account_cipher().unwrap();
        let clock = Clock::now(now_unix_secs());
        let correct = cipher.record_key(b"settings/display-name").unwrap();
        let wrong = cipher.record_key(b"settings/wrong").unwrap();
        let plain = Record::DisplayName { name: "false".into() }.encode().unwrap();
        let mut record = PendingAccountRecord {
            key: wrong,
            clock,
            ciphertext: cipher.seal(&wrong, clock, &plain).unwrap(),
        };
        assert!(store.stage_account_record(&record).is_err());
        assert!(!directory.path().join(crate::paths::ACCOUNT_DB).exists());
        record.key = correct;
        record.ciphertext = cipher.seal(&correct, clock, &plain).unwrap();
        let mut other = record.clone();
        other.ciphertext = cipher
            .seal(&correct, clock, &Record::DisplayName { name: "true".into() }.encode().unwrap())
            .unwrap();
        if other.ciphertext < record.ciphertext {
            std::mem::swap(&mut record, &mut other);
        }
        store.stage_account_record(&record).unwrap().unwrap();
        store.stage_account_record(&other).unwrap().unwrap();
        let database = store.account_replica().unwrap();
        let committed_clock = database.clock();
        let root = database.begin_read().unwrap().root_hash().unwrap();
        assert!(store.stage_account_record(&record).unwrap().is_none());
        assert!(store.stage_account_record(&other).unwrap().is_some());
        assert_eq!(database.clock(), committed_clock, "replay must not commit a new clock");
        assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), root);
    }

    #[test]
    fn account_cache_admission_obeys_existing_handles_and_concurrent_first_open() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let mut held = Vec::new();
        for index in 0..OPEN_DATABASE_LIMIT / 2 {
            let volume = store.create_volume(&format!("Held {index}"), false).unwrap();
            held.push(store.db(volume.id, crate::store::DatabaseId::Index).unwrap());
            held.push(store.db(volume.id, crate::store::DatabaseId::Revocations).unwrap());
        }
        unlock(&store, true);
        assert!(matches!(store.account_replica(), Err(Error::LocalContention(_))));
        assert!(!directory.path().join(crate::paths::ACCOUNT_DB).exists());
        drop(held.pop());
        store.account_replica().unwrap();
        assert_eq!(store.open.lock().unwrap().by_id.len(), OPEN_DATABASE_LIMIT);
        // Same-file first-open waiting is tested with available capacity. At
        // the limit, a concurrently retiring backend legitimately keeps its
        // slot charged and another unreserved opener can report contention.
        let fresh = tempfile::tempdir().unwrap();
        let store = LocalStore::open(fresh.path()).unwrap();
        unlock(&store, true);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let open = || {
                barrier.wait();
                store.account_replica().unwrap()
            };
            let other = scope.spawn(open);
            let first = open();
            assert!(Arc::ptr_eq(&first, &other.join().unwrap()));
        });
        assert_eq!(store.open.lock().unwrap().by_id.len(), 1);
    }

    #[test]
    #[ignore = "reference account staging and replay timing; run with --release"]
    fn benchmark_account_record_staging() {
        use std::time::Instant;
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let cipher = store.unlocked_account_cipher().unwrap();
        let mut body = Record::Volume {
            volume: Volume {
                id: Id::from_bytes([3; 16]),
                name: String::new(),
                owner: store.account.id,
                personal: true,
                share_key_with_service: false,
            },
            credentials: Credentials { database_key: [4; 32], blob_key: [5; 32] },
        };
        let padding = 512 - encode(&body).unwrap().len();
        if let Record::Volume { volume, .. } = &mut body {
            volume.name = "x".repeat(padding);
        }
        let plain = encode(&body).unwrap();
        assert_eq!(plain.len(), 512);
        let key = cipher.record_key(body.logical_key().unwrap().as_bytes()).unwrap();
        let start = Instant::now();
        let database = store.account_replica().unwrap();
        let first_open_ns = start.elapsed().as_nanos();
        let mut clock = Clock::now(now_unix_secs());
        let (mut stage, mut replay, mut read) = (Vec::new(), Vec::new(), Vec::new());
        for index in 0..1100 {
            clock = clock.incr(now_unix_secs()).unwrap();
            let record = PendingAccountRecord {
                key,
                clock,
                ciphertext: cipher.seal(&key, clock, &plain).unwrap(),
            };
            let start = Instant::now();
            let first = store.stage_account_record(&record).unwrap().unwrap();
            let stage_ns = start.elapsed().as_nanos();
            let committed_clock = database.clock();
            let start = Instant::now();
            let repeated = store.stage_account_record(&record).unwrap().unwrap();
            let replay_ns = start.elapsed().as_nanos();
            assert_eq!(database.clock(), committed_clock);
            assert_eq!(first, repeated);
            let start = Instant::now();
            let snapshot = database.begin_read().unwrap();
            let (observed, value) = snapshot.get_with_clock(&key).unwrap().unwrap();
            assert_eq!(observed, clock);
            assert_eq!(value.as_ref(), record.ciphertext);
            let read_ns = start.elapsed().as_nanos();
            if index >= 100 {
                stage.push(stage_ns);
                replay.push(replay_ns);
                read.push(read_ns);
            }
        }
        crate::diagnostic!(
            "account-staging {}",
            serde_json::json!({"samples":1000,
            "plaintext_bytes":512,"envelope_bytes":541,"first_open_ns":first_open_ns,
            "stage_ns":stage,"replay_ns":replay,"read_clock_value_ns":read})
        );
    }

    #[test]
    #[ignore = "reference durable account receive/adoption timing; run with --release"]
    fn benchmark_account_record_receive() {
        use std::time::Instant;
        for retained in [0, 128] {
            let directory = tempfile::tempdir().unwrap();
            let store = LocalStore::open(directory.path()).unwrap();
            unlock(&store, true);
            for index in 0..retained {
                store.create_volume(&format!("Retained {index}"), false).unwrap();
            }
            let mut body = Record::Volume {
                volume: Volume {
                    id: Id::from_bytes([3; 16]),
                    name: String::new(),
                    owner: store.account.id,
                    personal: true,
                    share_key_with_service: false,
                },
                credentials: Credentials { database_key: [4; 32], blob_key: [5; 32] },
            };
            let padding = 512 - encode(&body).unwrap().len();
            if let Record::Volume { volume, .. } = &mut body {
                volume.name = "x".repeat(padding);
            }
            let plain = encode(&body).unwrap();
            assert_eq!(plain.len(), 512);
            let cipher = store.unlocked_account_cipher().unwrap();
            let key = cipher.record_key(body.logical_key().unwrap().as_bytes()).unwrap();
            let mut clock = Clock::now(now_unix_secs());
            let (mut receive, mut replay) = (Vec::new(), Vec::new());
            let mut first_ns = 0;
            for index in 0..1100 {
                clock = clock.incr(now_unix_secs()).unwrap();
                let record = PendingAccountRecord {
                    key,
                    clock,
                    ciphertext: cipher.seal(&key, clock, &plain).unwrap(),
                };
                let delta = record_delta(&store, &[record]);
                let start = Instant::now();
                assert!(store.receive_account_delta(&delta).unwrap());
                let receive_ns = start.elapsed().as_nanos();
                if index == 0 {
                    first_ns = receive_ns;
                }
                let start = Instant::now();
                assert!(!store.receive_account_delta(&delta).unwrap());
                let replay_ns = start.elapsed().as_nanos();
                if index >= 100 {
                    receive.push(receive_ns);
                    replay.push(replay_ns);
                }
            }
            assert_eq!(store.volume(Id::from_bytes([3; 16])).unwrap().name.len(), padding);
            assert!(store.pending_account_record(None).unwrap().is_none() == (retained == 0));
            crate::diagnostic!(
                "account-receive {}",
                serde_json::json!({"samples":1000,
                "retained_volumes":retained,"plaintext_bytes":512,"envelope_bytes":541,
                "first_receive_including_files_ns":first_ns,"receive_ns":receive,"replay_ns":replay})
            );
        }
        crate::trace::mark("account.receive.benchmark");
    }

    fn drain(store: &LocalStore) -> Vec<PendingAccountRecord> {
        let mut records = Vec::new();
        while let Some(record) = store.pending_account_record(None).unwrap() {
            assert!(store.acknowledge_account_record(&record).unwrap());
            records.push(record);
        }
        records
    }

    fn plaintext(store: &LocalStore, record: &PendingAccountRecord) -> Record {
        let bytes = store
            .cipher_for(&[7; 16])
            .unwrap()
            .open(&record.key, record.clock, &record.ciphertext)
            .unwrap();
        Record::decode(&bytes).unwrap()
    }

    #[test]
    fn outbox_commits_with_local_state_and_keeps_exact_bytes_across_restart_and_old_ack() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let mut feed = store.take_change_receiver().unwrap();
        let before = feed.drain().account_generation;
        store.set_display_name("false").unwrap();
        let first = store.pending_account_record(None).unwrap().unwrap();
        assert_eq!(feed.drain().account_generation, before + 1);
        assert!(matches!(
            plaintext(&store, &first),
            Record::DisplayName { name } if name == "false"
        ));
        store.set_display_name("true").unwrap();
        let second = store.pending_account_record(None).unwrap().unwrap();
        assert_eq!(first.key, second.key);
        assert!(second.clock > first.clock);
        assert_ne!(first.ciphertext, second.ciphertext);
        assert!(!store.acknowledge_account_record(&first).unwrap());
        assert_eq!(store.pending_account_record(None).unwrap(), Some(second.clone()));
        assert_eq!(feed.drain().account_generation, before + 2);
        drop(feed);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.display_name().unwrap().as_deref(), Some("true"));
        assert_eq!(store.pending_account_record(None).unwrap(), Some(second.clone()));
        assert!(store.acknowledge_account_record(&second).unwrap());
        assert!(store.pending_account_record(None).unwrap().is_none());
        store.set_display_name("true").unwrap();
        assert!(store.pending_account_record(None).unwrap().is_none());
        let retained = store
            .metadata
            .begin_read()
            .unwrap()
            .get(&key(RECORD, &second.key))
            .unwrap()
            .unwrap()
            .to_vec();
        assert_eq!(retained, second.encode());
    }

    #[test]
    fn shared_credentials_only_publish_in_feature_gated_invitations() {
        for saved_before_creation in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let creator = LocalStore::open(directory.path().join("creator")).unwrap();
            if saved_before_creation {
                unlock(&creator, true);
            }
            let app = crate::package::AppIdentity {
                publisher: creator.account.id,
                app_id: "shared-test".into(),
            };
            let root =
                app.volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None).unwrap();
            let build = crate::identifier::encode(&[0x35; 32]);
            let session = creator
                .create_session_binding(SessionCreation {
                    app: app.clone(),
                    selector: BuildSelector::Exact { build_id: build.clone() },
                    resolved_build_id: build.clone(),
                    root_volume: root,
                    sync_shared: true,
                    user_volume: None,
                })
                .unwrap();
            let shared = session.shared_volume.unwrap();
            let volume = creator
                .initialize_automatic(
                    shared,
                    &automatic::AutomaticBinding {
                        app,
                        role: crate::package::AutomaticVolumeRole::Session,
                        session: Some(session.id.clone()),
                        build,
                    },
                    &RootInitialization {
                        permissions: Acl::default(),
                        databases: Vec::new(),
                        blobs: Vec::new(),
                    },
                )
                .unwrap();
            if !saved_before_creation {
                unlock(&creator, true);
                creator.seed_account_records().unwrap();
            }
            let records = drain(&creator);
            assert_eq!(records.len(), 1);
            assert!(matches!(plaintext(&creator, &records[0]),
                Record::HostedInvitation { invitation, .. }
                    if invitation.session == session && invitation.shared.is_some()));
            let mut recipient = LocalStore::open(directory.path().join("recipient")).unwrap();
            recipient.bind_authenticated_account(creator.account.id).unwrap();
            unlock(&recipient, true);
            recipient.stage_account_record(&records[0]).unwrap().unwrap();
            recipient.reconcile_account_records().unwrap();
            assert_eq!(recipient.session(&session.id).unwrap(), session);
            assert_eq!(
                recipient.session_scopes(&session.id).unwrap(),
                creator.session_scopes(&session.id).unwrap()
            );
            assert_eq!(
                recipient.credentials(shared).unwrap(),
                creator.credentials(shared).unwrap()
            );
            assert!(!recipient.is_automatic_volume(shared).unwrap());
            // Imported Shared has no initializer binding; its retained invitation
            // must still prevent a standalone, unfeatured Volume publication.
            let record =
                Record::Volume { volume, credentials: recipient.credentials(shared).unwrap() };
            let _profile = recipient.lock_profile().unwrap();
            let mut tx = recipient.metadata.begin_write(now_unix_secs()).unwrap();
            assert!(!recipient.queue_account_record(&mut tx, &record).unwrap());
            tx.commit().unwrap();
            drop(_profile);
            assert!(recipient.validate_account_projection(Some(record)).is_err());
            assert!(recipient.pending_account_record(None).unwrap().is_none());
            let original_catalog = recipient.recovery_catalog().unwrap();
            if saved_before_creation {
                recipient.seed_account_records().unwrap();
                // Unchanged invitation seeding keeps its received ciphertext
                // and creates neither an echo nor a standalone Volume record.
                assert!(drain(&recipient).is_empty());
            } else {
                unlock(&recipient, false);
            }
            recipient.delete_volume(shared).unwrap();
            if !saved_before_creation {
                assert!(recipient.pending_account_record(None).unwrap().is_none());
                unlock(&recipient, true);
                recipient.seed_account_records().unwrap();
            }
            let deleted = drain(&recipient);
            assert_eq!(deleted.len(), 1);
            assert!(matches!(plaintext(&recipient, &deleted[0]),
                Record::VolumeDeleted { volume } if volume == shared));
            assert_eq!(recipient.account_record_dependencies(&deleted[0]).unwrap(), [None, None]);
            let deletion = recipient.stage_account_record(&deleted[0]).unwrap().unwrap();
            creator.receive_account_delta(&deletion).unwrap();
            assert!(creator.volume(shared).is_err());
            assert!(creator.session(&session.id).is_err());
            // Fresh recovery retains deletion whichever record arrives first;
            // neither the older account invitation nor its catalog can revive it.
            let invitation = record_delta(&recipient, &records);
            for deletion_first in [false, true] {
                let path = directory.path().join(format!("recovered-{deletion_first}"));
                let mut recovered = LocalStore::open(&path).unwrap();
                recovered.bind_authenticated_account(creator.account.id).unwrap();
                unlock(&recovered, true);
                for delta in
                    if deletion_first { [&deletion, &invitation] } else { [&invitation, &deletion] }
                {
                    recovered.receive_account_delta(delta).unwrap();
                }
                recovered.import_recovery_catalog(&original_catalog, || true).unwrap().unwrap();
                recovered.seed_account_records().unwrap();
                assert!(recovered.pending_account_record(None).unwrap().is_none());
                drop(recovered);
                let recovered = LocalStore::open(&path).unwrap();
                assert!(recovered.volume_deleted(shared).unwrap());
                assert!(recovered.volume(shared).is_err());
                assert!(recovered.session(&session.id).is_err());
                assert!(recovered.sessions().unwrap().is_empty());
                let catalog: serde_json::Value =
                    serde_json::from_slice(&recovered.recovery_catalog().unwrap()).unwrap();
                assert!(catalog["volumes"].as_array().unwrap().is_empty());
                assert!(catalog["invitations"].as_array().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn unsaved_and_locked_mutations_seed_once_with_only_explicitly_shared_records() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let root = store.create_volume("Root", true).unwrap();
        unlock(&store, false);
        let personal = store.create_volume("Personal", false).unwrap();
        let version = crate::identifier::encode(&[0x11; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        store
            .select_personal_volumes(
                &session.id,
                vec![PersonalVolumeGrant { volume: personal.id, write: false }],
            )
            .unwrap();
        store.record_session_denial(&session, "Retain local denial").unwrap();
        assert!(store.pending_account_record(None).unwrap().is_none());
        assert!(store.seed_account_records().is_err());
        unlock(&store, true);
        store.seed_account_records().unwrap();
        let records = drain(&store);
        assert_eq!(records.len(), 4); // Root, personal volume, invitation and consent.
        let mut kinds = [0; 3];
        for record in &records {
            let plain = plaintext(&store, record);
            assert_eq!(
                record.key,
                store
                    .cipher_for(&[7; 16])
                    .unwrap()
                    .record_key(plain.logical_key().unwrap().as_bytes())
                    .unwrap()
            );
            kinds[match plain {
                Record::Volume { volume, .. } => {
                    assert!(store.volume(volume.id).is_ok());
                    0
                }
                Record::Invitation { invitation, .. } => {
                    assert_eq!(
                        invitation.link(crate::release_channel::ReleaseChannel::Public),
                        store
                            .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
                            .unwrap()
                    );
                    1
                }
                Record::PersonalVolumes { session: v, grants } => {
                    assert_eq!(v, session.id);
                    assert_eq!(
                        grants.iter().map(PersonalVolumeSelection::grant).collect::<Vec<_>>(),
                        vec![PersonalVolumeGrant { volume: personal.id, write: false }]
                    );
                    2
                }
                Record::DisplayName { .. } => panic!("this fixture has no display name"),
                Record::VolumeDeleted { .. } => panic!("this fixture has no deleted volume"),
                Record::HostedVolume { .. } | Record::HostedInvitation { .. } => {
                    panic!("this fixture has no automatic scope")
                }
            }] += 1;
        }
        assert_eq!(kinds, [2, 1, 1]);
        store.seed_account_records().unwrap();
        assert!(store.pending_account_record(None).unwrap().is_none());
        assert_eq!(
            store.session_denial(&session.id).unwrap().as_deref(),
            Some("Retain local denial")
        );
    }

    #[test]
    fn aborted_profile_transaction_leaves_neither_local_mutation_nor_publication() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let mut feed = store.take_change_receiver().unwrap();
        let before = feed.drain().account_generation;
        {
            let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
            tx.put(b"settings/display-name", &encode(&"false").unwrap()).unwrap();
            assert!(
                store
                    .queue_account_record(&mut tx, &Record::DisplayName { name: "false".into() })
                    .unwrap()
            );
            // Simulate validation failure/cancellation before the one commit.
        }
        assert!(store.display_name().unwrap().is_none());
        assert!(store.pending_account_record(None).unwrap().is_none());
        assert!(store.metadata.begin_read().unwrap().get(CLOCK).unwrap().is_none());
        assert_eq!(feed.drain().account_generation, before);
    }

    #[test]
    fn publication_cursor_makes_progress_past_a_continuously_dirty_record() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        store.create_volume("One", true).unwrap();
        store.create_volume("Two", true).unwrap();
        store.set_display_name("false").unwrap();
        let first = store.pending_account_record(None).unwrap().unwrap();
        let second = store.pending_account_record(Some(first.key)).unwrap().unwrap();
        let third = store.pending_account_record(Some(second.key)).unwrap().unwrap();
        assert!(first.key < second.key && second.key < third.key);
        assert!(store.pending_account_record(Some(third.key)).unwrap().is_none());
        // The first remains pending; a failed/stale acknowledgment does not
        // force publication to revisit it before every other retained key.
        assert_eq!(store.pending_account_record(None).unwrap(), Some(first.clone()));
        assert!(store.acknowledge_account_record(&second).unwrap());
        assert_eq!(store.pending_account_record(Some(first.key)).unwrap(), Some(third));
    }

    #[test]
    fn concurrent_acknowledgment_never_erases_the_new_local_intent() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        for _ in 0..100 {
            store.set_display_name("false").unwrap();
            let previous = store.pending_account_record(None).unwrap().unwrap();
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|threads| {
                threads.spawn(|| {
                    start.wait();
                    store.set_display_name("true").unwrap();
                });
                start.wait();
                store.acknowledge_account_record(&previous).unwrap();
            });
            assert_eq!(store.display_name().unwrap().as_deref(), Some("true"));
            let pending = store.pending_account_record(None).unwrap().unwrap();
            assert!(pending.clock > previous.clock);
            assert!(matches!(
                plaintext(&store, &pending),
                Record::DisplayName { name } if name == "true"
            ));
            assert!(store.acknowledge_account_record(&pending).unwrap());
        }
    }

    #[test]
    fn first_secret_save_cannot_publish_before_its_vault_acknowledgment() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let mut state = store.recovery_state().unwrap().unwrap();
        state.revision = 0;
        store.save_recovery_state(&state).unwrap();
        store.set_display_name("false").unwrap();
        assert!(store.pending_account_record(None).unwrap().is_none());
        assert!(store.seed_account_records().is_err());
        state.revision = 1;
        store.save_recovery_state(&state).unwrap();
        store.seed_account_records().unwrap();
        assert_eq!(drain(&store).len(), 1);
    }

    #[test]
    fn repeated_mutation_coalesces_and_bootstrap_cannot_overwrite_newer_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        unlock(&store, true);
        let mut feed = store.take_change_receiver().unwrap();
        let before = feed.drain().account_generation;
        for index in 0..1000 {
            store.set_display_name(&(index % 2 == 0).to_string()).unwrap();
        }
        store.seed_account_record(&Record::DisplayName { name: "true".into() }).unwrap();
        let records = drain(&store);
        assert_eq!(records.len(), 1);
        assert!(matches!(
            plaintext(&store, &records[0]),
            Record::DisplayName { name } if name == "false"
        ));
        let batch = feed.drain();
        assert_eq!(batch.account_generation, before + 1000);
        assert!(batch.records.is_empty());
        assert_eq!(batch.status.gap_generation, 0);
        assert!(batch.status.closed.is_none());
    }

    #[test]
    #[ignore = "reference account mutation timing; run with --release"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "Keep executable checks of the compile-time capacity contract in this regression test."
    )]
    fn benchmark_account_record_intent() {
        assert!(!cfg!(debug_assertions));
        for (retained, enabled_outbox) in [(0, false), (0, true), (128, false), (128, true)] {
            let directory = tempfile::tempdir().unwrap();
            let store = LocalStore::open(directory.path()).unwrap();
            for index in 0..retained {
                store.create_volume(&format!("Retained {index}"), true).unwrap();
            }
            unlock(&store, true);
            store.seed_account_records().unwrap();
            drain(&store);
            let mut software = Vec::with_capacity(1000);
            let mut total = Vec::with_capacity(1000);
            for index in 0..1100 {
                let start = std::time::Instant::now();
                let _profile = store.lock_profile().unwrap();
                let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
                let name = (index % 2 == 0).to_string();
                tx.put(b"settings/display-name", &encode(&name).unwrap()).unwrap();
                let seal_start = std::time::Instant::now();
                let changed = enabled_outbox
                    && store.queue_account_record(&mut tx, &Record::DisplayName { name }).unwrap();
                let seal_ns = seal_start.elapsed().as_nanos();
                tx.commit().unwrap();
                store.committed_account_record(changed);
                let total_ns = start.elapsed().as_nanos();
                if index >= 100 {
                    software.push(seal_ns);
                    total.push(total_ns);
                }
            }
            assert_eq!(drain(&store).len(), usize::from(enabled_outbox));
            assert_eq!(store.display_name().unwrap().as_deref(), Some("false"));
            crate::diagnostic!(
                "account-intent {}",
                serde_json::json!({
                    "retained_records": retained, "outbox": enabled_outbox, "samples":1000,
                    "record_software_ns":software, "profile_commit_and_notify_ns":total,
                })
            );
        }
    }
}
