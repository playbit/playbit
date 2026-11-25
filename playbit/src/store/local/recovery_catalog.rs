//! Recovery copies contain capabilities, never device authentication or data.
use super::*;

/// Status of an optional recovery copy, independent of local readiness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum AccountKeyBackup {
    #[default]
    Unavailable,
    Pending,
    Saved,
    Failed,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryUpload {
    pub operation_id: Id,
    pub expected_revision: u64,
    pub ciphertext: String,
    pub local_digest: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryState {
    pub secret: [u8; 16],
    pub acknowledged: bool,
    pub revision: u64,
    pub synchronized_digest: Option<String>,
    pub pending: Option<RecoveryUpload>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogVolume {
    volume: Volume,
    credentials: Credentials,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryCatalog {
    account: Id,
    volumes: Vec<CatalogVolume>,
    invitations: Vec<Invitation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<AppScope>,
}

impl RecoveryCatalog {
    pub(super) fn with_scope(mut self, scope: AppScope) -> Self {
        self.scopes.push(scope);
        self
    }

    pub(super) fn volume(account: Id, volume: Volume, credentials: Credentials) -> Self {
        Self {
            account,
            volumes: vec![CatalogVolume { volume, credentials }],
            invitations: Vec::new(),
            scopes: Vec::new(),
        }
    }

    pub(super) fn invitation(account: Id, invitation: Invitation) -> Result<Self> {
        invitation.verify()?;
        Ok(Self {
            account,
            volumes: vec![CatalogVolume {
                volume: invitation.replica_volume()?.clone(),
                credentials: invitation.replica_credentials()?.clone(),
            }],
            invitations: vec![invitation],
            scopes: Vec::new(),
        })
    }

    fn parse(bytes: &[u8], account: Id) -> Result<Self> {
        if bytes.len() > crate::store::recovery::MAX_PLAINTEXT_BYTES {
            return Err(Error::InvalidInput("credential catalog is too large"));
        }
        let catalog: Self = decode(bytes)?;
        if catalog.account != account {
            return Err(Error::AccessDenied("credential catalog belongs to another account"));
        }
        let mut volumes = BTreeMap::new();
        for item in &catalog.volumes {
            if item.volume.name.trim().is_empty()
                || item.credentials.database_key == item.credentials.blob_key
                || volumes.insert(item.volume.id, item).is_some()
            {
                return Err(Error::InvalidInput("invalid or duplicate recovered volume"));
            }
        }
        let mut sessions = BTreeSet::new();
        for invitation in &catalog.invitations {
            invitation.verify()?;
            if !sessions.insert(invitation.session.id.clone()) {
                return Err(Error::InvalidInput("duplicate recovered session"));
            }
            {
                let (volume, credentials) =
                    (invitation.replica_volume()?, invitation.replica_credentials()?);
                let item = volumes
                    .get(&volume.id)
                    .ok_or(Error::InvalidInput("recovered invitation volume is missing"))?;
                if item.volume.owner != volume.owner || item.credentials != *credentials {
                    return Err(Error::InvalidInput("recovered invitation keys conflict"));
                }
            }
        }
        let mut scopes = BTreeSet::new();
        for scope in &catalog.scopes {
            if !scopes.insert(scope.logical_key()?) {
                return Err(Error::InvalidInput("duplicate recovered app scope"));
            }
        }
        Ok(catalog)
    }
}

impl AccountKeyBackup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Pending => "pending",
            Self::Saved => "saved",
            Self::Failed => "failed",
        }
    }
}

impl LocalStore {
    pub fn account_key_backup(&self) -> Result<AccountKeyBackup> {
        #[cfg(all(target_vendor = "apple", any(test, feature = "connected-engine")))]
        if !crate::credentials::available() {
            return Ok(AccountKeyBackup::Unavailable);
        }
        #[cfg(not(all(target_vendor = "apple", any(test, feature = "connected-engine"))))]
        return Ok(AccountKeyBackup::Unavailable);
        #[cfg(all(target_vendor = "apple", any(test, feature = "connected-engine")))]
        {
            let Some(state) = self.recovery_state()?.filter(|s| s.acknowledged && s.revision > 0)
            else {
                return Ok(AccountKeyBackup::Unavailable);
            };
            let saved: Option<(Id, String, AccountKeyBackup)> =
                get(&self.metadata, b"keychain-backup")?;
            Ok(saved
                .filter(|(account, digest, _)| {
                    *account == self.account.id
                        && digest == blake3::hash(&state.secret).to_hex().as_str()
                })
                .map(|(_, _, status)| status)
                .unwrap_or(AccountKeyBackup::Pending))
        }
    }

    #[cfg(all(target_vendor = "apple", any(test, feature = "connected-engine")))]
    pub(crate) fn record_account_key_backup(
        &self,
        account: Id,
        secret: &[u8; 16],
        status: AccountKeyBackup,
    ) -> Result<()> {
        let _guard = self.lock_profile()?;
        if account != self.account.id
            || !self
                .recovery_state()?
                .is_some_and(|s| s.secret == *secret && s.acknowledged && s.revision > 0)
        {
            return Err(Error::InvalidInput("account changed during key backup"));
        }
        put(
            &self.metadata,
            b"keychain-backup",
            &(account, blake3::hash(secret).to_hex().to_string(), status),
        )?;
        self.changes.account_changed();
        Ok(())
    }

    pub fn recovery_state(&self) -> Result<Option<RecoveryState>> {
        if let RecoveryPersistence::Ephemeral(state) = &self.recovery_persistence {
            return state
                .lock()
                .map(|state| state.clone())
                .map_err(|_| Error::Internal("ephemeral recovery lock is poisoned"));
        }
        self.metadata
            .begin_read()?
            .get(b"recovery")?
            .map(|bytes| self.decode_recovery_state(&bytes))
            .transpose()
    }

    pub fn save_recovery_state(&self, state: &RecoveryState) -> Result<()> {
        if let RecoveryPersistence::Ephemeral(saved) = &self.recovery_persistence {
            let _profile = self.lock_profile()?;
            *saved.lock().map_err(|_| Error::Internal("ephemeral recovery lock is poisoned"))? =
                Some(state.clone());
            self.changes.account_changed();
            return Ok(());
        }
        let encoded = encode(state)?;
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let prepared = tx
            .get(b"prepared-account-key")?
            .map(|bytes| decode::<(Id, [u8; 16])>(&bytes))
            .transpose()?;
        if let Some((account, secret)) = prepared
            && account == self.account.id
        {
            if secret != state.secret {
                return Err(Error::InvalidInput(
                    "prepared account key differs from recovery state",
                ));
            }
            tx.del(b"prepared-account-key")?;
        }
        tx.put(b"recovery", &encoded)?;
        tx.put(b"recovery-key-published", &encode(&true)?)?;
        tx.commit()?;
        self.changes.account_changed();
        Ok(())
    }

    /// Retain a prepared enrollment key before publishing its account binding.
    #[cfg(any(test, feature = "connected-engine"))]
    pub(crate) fn protect_account_key(&self, account: Id, secret: &[u8; 16]) -> Result<()> {
        if matches!(self.recovery_persistence, RecoveryPersistence::Ephemeral(_)) {
            return Err(Error::InvalidInput("ephemeral recovery cannot retain an account key"));
        }
        let _guard = self.lock_profile()?;
        put(&self.metadata, b"prepared-account-key", &(account, secret))
    }

    pub(crate) fn prepared_account_key(&self, account: Id) -> Result<Option<[u8; 16]>> {
        let saved: Option<(Id, [u8; 16])> = get(&self.metadata, b"prepared-account-key")?;
        Ok(saved.filter(|(id, _)| *id == account).map(|(_, secret)| secret))
    }

    pub(super) fn transactional_recovery(
        &self,
        tx: &kvdb::WriteTxn<'_, RedbStorage>,
    ) -> Result<Option<RecoveryState>> {
        match &self.recovery_persistence {
            RecoveryPersistence::Durable => {
                tx.get(b"recovery")?.map(|bytes| self.decode_recovery_state(&bytes)).transpose()
            }
            RecoveryPersistence::Ephemeral(_) => self.recovery_state(),
        }
    }

    pub(super) fn validate_recovery_persistence(&self) -> Result<()> {
        let metadata = self.metadata.begin_read()?;
        let ephemeral: Option<bool> =
            metadata.get(b"ephemeral-recovery")?.map(|bytes| decode(&bytes)).transpose()?;
        match &self.recovery_persistence {
            RecoveryPersistence::Durable if ephemeral.is_some() => {
                Err(Error::InvalidInput("ephemeral profile requires host-supplied recovery"))
            }
            RecoveryPersistence::Durable => Ok(()),
            RecoveryPersistence::Ephemeral(_) => {
                if ephemeral.is_none() {
                    return Err(Error::InvalidInput(
                        "ephemeral recovery requires a dedicated job profile",
                    ));
                }
                if metadata.has(b"recovery")?
                    || metadata.has(b"prepared-account-key")?
                    || metadata.has(b"recovery-key-published")?
                {
                    return Err(Error::InvalidInput(
                        "ephemeral recovery requires a dedicated job profile",
                    ));
                }
                if ephemeral == Some(false) {
                    return Err(Error::corrupt("invalid ephemeral recovery marker"));
                }
                Ok(())
            }
        }
    }

    pub(super) fn decode_recovery_state(&self, bytes: &[u8]) -> Result<RecoveryState> {
        decode(bytes)
    }

    /// Stable credential-only catalog. Original invitations carry immutable
    /// bootstrap identities; current version adoption stays with AP authority.
    pub fn recovery_catalog(&self) -> Result<Vec<u8>> {
        let _guard = self.lock_profile()?;
        self.export_recovery_catalog()
    }

    fn export_recovery_catalog(&self) -> Result<Vec<u8>> {
        let mut invitations = Vec::new();
        for stored in scan::<StoredSession>(&self.metadata, "session/")? {
            if !self.volume_deleted(stored.session.replica_volume())? {
                invitations.push(stored.invitation);
            }
        }
        let mut scopes = Vec::new();
        for scope in self.hosted_app_scopes()? {
            let retained = match &scope {
                AppScope::World { app, .. } => !self.volume_deleted(self.automatic_volume_id(
                    app,
                    None,
                    crate::package::AutomaticVolumeRole::Root,
                )?)?,
                AppScope::Session { id, .. } => {
                    invitations.iter().any(|invitation| &invitation.session.id == id)
                }
            };
            if retained {
                scopes.push(scope);
            }
        }
        let session_volumes: BTreeSet<_> =
            invitations.iter().map(|invite| invite.session.replica_volume()).collect();
        let mut volumes = Vec::new();
        for volume in self.volumes()? {
            if !self.replication_supported(volume.id)?
                || volume.owner != self.account.id
                    && !session_volumes.contains(&volume.id)
                    && self.has_volume_inbox_entry(volume.id)?
            {
                continue;
            }
            match self.credentials(volume.id) {
                Ok(credentials) => volumes.push(CatalogVolume { volume, credentials }),
                Err(Error::NotFound(_)) if self.uses_world_projection(volume.id)? => (),
                Err(error) => return Err(error),
            }
        }
        encode(&RecoveryCatalog { account: self.account.id, volumes, invitations, scopes })
    }

    /// Union both devices' capabilities. Existing local session adoption and
    /// volume presentation win; incompatible keys always fail explicitly.
    pub fn merge_recovery_catalog(&self, remote: &[u8]) -> Result<Vec<u8>> {
        let _guard = self.lock_profile()?;
        self.merge_catalog(remote)
    }

    fn merge_catalog(&self, remote: &[u8]) -> Result<Vec<u8>> {
        let remote = RecoveryCatalog::parse(remote, self.account.id)?;
        let local = RecoveryCatalog::parse(&self.export_recovery_catalog()?, self.account.id)?;
        let mut volumes: BTreeMap<_, _> =
            remote.volumes.into_iter().map(|item| (item.volume.id, item)).collect();
        for item in local.volumes {
            if let Some(other) = volumes.get(&item.volume.id)
                && (other.volume.owner != item.volume.owner
                    || other.credentials != item.credentials)
            {
                return Err(Error::InvalidInput("recovered volume keys conflict with this device"));
            }
            volumes.insert(item.volume.id, item);
        }
        let mut invitations: BTreeMap<String, Invitation> = BTreeMap::new();
        let mut retained = BTreeMap::new();
        for (id, item) in volumes {
            if !self.volume_deleted(id)? {
                retained.insert(id, item);
            }
        }
        let volumes = retained;
        let mut scopes = BTreeMap::new();
        for scope in remote.scopes.into_iter().chain(local.scopes) {
            if let AppScope::World { app, .. } = &scope
                && self.volume_deleted(self.automatic_volume_id(
                    app,
                    None,
                    crate::package::AutomaticVolumeRole::Root,
                )?)?
            {
                continue;
            }
            let key = scope.logical_key()?;
            if scopes.get(&key).is_some_and(|other| other != &scope) {
                return Err(Error::InvalidInput("recovered app scope conflicts with this device"));
            }
            scopes.insert(key, scope);
        }
        for invitation in remote.invitations.into_iter().chain(local.invitations) {
            if self.volume_deleted(invitation.session.replica_volume())? {
                scopes.remove(&format!("automatic-session/{}", invitation.session.id));
                continue;
            }
            if let Some(other) = invitations.get(&invitation.session.id) {
                // A selector can change through an authorized session update;
                // the session's keys and data bindings cannot change with it.
                if !same_session_identity(&other.session, &invitation.session)
                    || other.seed != invitation.seed
                    || other.replica_credentials()? != invitation.replica_credentials()?
                {
                    return Err(Error::InvalidInput(
                        "recovered session keys conflict with this device",
                    ));
                }
            }
            invitations.insert(invitation.session.id.clone(), invitation);
        }
        encode(&RecoveryCatalog {
            account: self.account.id,
            volumes: volumes.into_values().collect(),
            invitations: invitations.into_values().collect(),
            scopes: scopes.into_values().collect(),
        })
    }

    /// Validate only identities named by the incoming catalog. Call while the
    /// profile gate is held, before file preparation and again before adoption.
    /// Unrelated local records do not participate in a remote envelope's limit.
    pub(super) fn validate_incoming_catalog(&self, catalog: &RecoveryCatalog) -> Result<()> {
        for scope in &catalog.scopes {
            self.validate_app_scope(scope)?;
            if let AppScope::Session { id, .. } = scope {
                let session = catalog
                    .invitations
                    .iter()
                    .find(|invitation| &invitation.session.id == id)
                    .ok_or(Error::InvalidInput("recovered scope has no session capability"))?;
                if session.session.creator != self.account.id {
                    return Err(Error::AccessDenied(
                        "automatic session belongs to another account",
                    ));
                }
            }
        }
        let local = self.metadata.begin_read()?;
        for item in &catalog.volumes {
            if let Some(bytes) = local.get(format!("volume/{}", item.volume.id).as_bytes())? {
                let volume: Volume = decode(&bytes)?;
                let credentials = local.get(format!("credentials/{}", volume.id).as_bytes())?;
                let keys_match = match credentials {
                    Some(bytes) => {
                        volume.owner == item.volume.owner
                            && decode::<Credentials>(&bytes)? == item.credentials
                    }
                    None => {
                        volume.same_identity(&item.volume)
                            && self.uses_world_projection(volume.id)?
                    }
                };
                if !keys_match {
                    return Err(Error::InvalidInput(
                        "recovered volume keys conflict with this device",
                    ));
                }
            }
        }
        for incoming in &catalog.invitations {
            if self
                .local_session(&incoming.session.id)?
                .is_some_and(|local| !local.session.same_identity(&incoming.session))
            {
                return Err(Error::AccessDenied("recovery changes an existing local session"));
            }
            let Some(bytes) = local.get(format!("session/{}", incoming.session.id).as_bytes())?
            else {
                continue;
            };
            let stored: StoredSession = decode(&bytes)?;
            let original = &stored.invitation;
            if !same_session_identity(&stored.session, &incoming.session)
                || !same_session_identity(&original.session, &incoming.session)
                || original.seed != incoming.seed
                || original.replica_credentials()? != incoming.replica_credentials()?
            {
                return Err(Error::InvalidInput(
                    "recovered session keys conflict with this device",
                ));
            }
        }
        Ok(())
    }

    /// Restore only credentials. Empty replicas pull acknowledged databases
    /// through AP when used. No owner registration or permission mutation occurs.
    /// Returns None when cancellation wins before adoption. The callback runs
    /// once with the local metadata transaction prepared; it must not block.
    pub fn import_recovery_catalog(
        &self,
        bytes: &[u8],
        commit_allowed: impl FnOnce() -> bool,
    ) -> Result<Option<(usize, usize)>> {
        self.import_credentials(
            || RecoveryCatalog::parse(bytes, self.account.id),
            |_| Ok(commit_allowed()),
        )
    }

    /// Shared credential adoption for explicit recovery and authenticated account
    /// records. The final callback participates in the same profile transaction;
    /// false aborts every adoption, including its additional caller-owned state.
    pub(super) fn import_credentials(
        &self,
        prepare_catalog: impl FnOnce() -> Result<RecoveryCatalog>,
        commit_allowed: impl FnOnce(&mut kvdb::WriteTxn<'_, RedbStorage>) -> Result<bool>,
    ) -> Result<Option<(usize, usize)>> {
        self.import_credentials_under_grant(None, prepare_catalog, commit_allowed)
    }

    pub(super) fn import_credentials_under_grant(
        &self,
        direct_volume: Option<Id>,
        prepare_catalog: impl FnOnce() -> Result<RecoveryCatalog>,
        commit_allowed: impl FnOnce(&mut kvdb::WriteTxn<'_, RedbStorage>) -> Result<bool>,
    ) -> Result<Option<(usize, usize)>> {
        let _timing = crate::trace::Interval::new("recovery.import");
        // Validate incoming credentials before preparing files. The
        // profile gate is never held while waiting for a volume or filesystem.
        let catalog = self.prepare_credential_import(prepare_catalog)?;
        // Directory changes participate in the same volume admission gate as
        // ordinary writes and incoming replication. Always acquire it before
        // the profile lock, including when a withdrawal raced file preparation.
        let operations = direct_volume
            .map(|volume| self.access_operations(&Access::owner(self.account.id), volume))
            .transpose()?;
        let _guards = operations.as_ref().map(OperationLocks::lock).transpose()?;
        // Import/adoption may have completed during file preparation. Revalidate
        // again and insert only absent identities: existing version/denial wins.
        let profile_wait = crate::trace::Interval::new("recovery.import_profile_wait");
        let _profile = self.lock_profile()?;
        drop(profile_wait);
        let _adoption = crate::trace::Interval::new("recovery.import_adoption");
        self.validate_incoming_catalog(&catalog)?;
        let sessions = self.recovered_absent_sessions(&catalog)?;
        if !catalog.scopes.is_empty() {
            profile_format::enable_hosted_app_scopes(self)?;
        }
        let previous = self.metadata.begin_read()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let new_volumes = Self::import_volume_credentials(&catalog, &previous, &mut tx)?;
        for session in &sessions {
            tx.put(format!("session/{}", session.session.id).as_bytes(), &encode(session)?)?;
        }
        for scope in &catalog.scopes {
            self.write_app_scope(&mut tx, scope)?;
        }
        // File preparation and validation can be slow. Cancellation must be
        // checked at the final adoption boundary, before any keys are exposed.
        // Once commit begins it finishes normally; never weaken durability or
        // roll back credentials already committed to satisfy cancellation.
        let allowed = {
            let _timing = crate::trace::Span::new("recovery.import_admission");
            commit_allowed(&mut tx)?
        };
        if !allowed {
            return Ok(None);
        }
        let _commit = crate::trace::Interval::new("recovery.import_commit");
        tx.commit()?;
        drop(_commit);
        drop(_profile);
        self.announce_recovered_capabilities(&catalog, &sessions, &previous)?;
        Ok(Some((sessions.len(), new_volumes)))
    }

    fn prepare_credential_import(
        &self,
        prepare_catalog: impl FnOnce() -> Result<RecoveryCatalog>,
    ) -> Result<RecoveryCatalog> {
        let catalog = {
            let _timing = crate::trace::Interval::new("recovery.import_preflight");
            let catalog = prepare_catalog()?;
            let _profile = self.lock_profile()?;
            self.validate_incoming_catalog(&catalog)?;
            if catalog
                .invitations
                .iter()
                .any(|invitation| invitation.session.shared_volume.is_some())
            {
                profile_format::enable_shared_sessions(self)?;
            }
            catalog
        };
        let preparation = crate::trace::Interval::new("recovery.import_files");
        self.prepare_recovered_volume_files(&catalog)?;
        drop(preparation);
        Ok(catalog)
    }

    fn import_volume_credentials(
        catalog: &RecoveryCatalog,
        previous: &kvdb::ReadTxn<'_, RedbStorage>,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
    ) -> Result<usize> {
        let mut new_volumes = 0;
        for item in &catalog.volumes {
            if previous.has(format!("deleted-volume/{}", item.volume.id).as_bytes())? {
                continue;
            }
            let key = format!("volume/{}", item.volume.id);
            if tx.get(key.as_bytes())?.is_some() {
                if !tx.has(format!("credentials/{}", item.volume.id).as_bytes())? {
                    tx.put(
                        format!("credentials/{}", item.volume.id).as_bytes(),
                        &encode(&item.credentials)?,
                    )?;
                    new_volumes += 1;
                }
                continue;
            }
            tx.put(key.as_bytes(), &encode(&item.volume)?)?;
            tx.put(
                format!("credentials/{}", item.volume.id).as_bytes(),
                &encode(&item.credentials)?,
            )?;
            new_volumes += 1;
        }
        Ok(new_volumes)
    }

    fn announce_recovered_capabilities(
        &self,
        catalog: &RecoveryCatalog,
        sessions: &[StoredSession],
        previous: &kvdb::ReadTxn<'_, RedbStorage>,
    ) -> Result<()> {
        // Announce only newly usable capabilities. Settings and duplicate
        // account records do not refresh data. The pinned snapshot identifies
        // inserts without retaining another list alongside the bounded catalog.
        for item in &catalog.volumes {
            if item.volume.owner == self.account.id
                && !previous.has(format!("deleted-volume/{}", item.volume.id).as_bytes())?
                && previous.get(format!("volume/{}", item.volume.id).as_bytes())?.is_none()
            {
                self.changes.volume_available(item.volume.id);
            }
        }
        for session in sessions {
            {
                let volume = session.session.replica_volume();
                // Foreign records alone cannot authorize a data bootstrap;
                // their new invitation supplies the required session binding.
                self.changes.volume_available(volume);
            }
        }
        Ok(())
    }

    fn prepare_recovered_volume_files(&self, catalog: &RecoveryCatalog) -> Result<()> {
        for item in &catalog.volumes {
            let operations = self.operations(&[item.volume.id])?;
            let _guards = operations.lock()?;
            if self.volume_deleted(item.volume.id)? {
                continue;
            }
            if get::<Volume>(&self.metadata, format!("volume/{}", item.volume.id).as_bytes())?
                .is_some()
            {
                continue;
            }
            let directory = self.path.join(crate::paths::VOLUMES).join(item.volume.id.to_string());
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory).map_err(io)?;
            for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations]
            {
                self.replica_db(item.volume.id, database, Some(item.credentials.database_key))?;
            }
        }
        Ok(())
    }

    // Caller retains the profile lock through identity checks and adoption.
    fn recovered_absent_sessions(&self, catalog: &RecoveryCatalog) -> Result<Vec<StoredSession>> {
        let mut sessions = Vec::new();
        for invitation in &catalog.invitations {
            if self.volume_deleted(invitation.session.replica_volume())? {
                continue;
            }
            if let Some(existing) = get::<StoredSession>(
                &self.metadata,
                format!("session/{}", invitation.session.id).as_bytes(),
            )? {
                if !same_session_identity(&existing.session, &invitation.session)
                    || existing.invitation.seed != invitation.seed
                {
                    return Err(Error::InvalidInput(
                        "recovered session identity conflicts with this device",
                    ));
                }
            } else {
                sessions.push(StoredSession {
                    app_name: None,
                    session: invitation.session.clone(),
                    invitation: invitation.clone(),
                });
            }
        }
        Ok(sessions)
    }
}

fn same_session_identity(a: &Session, b: &Session) -> bool {
    a.id == b.id
        && a.root_volume == b.root_volume
        && a.shared_volume == b.shared_volume
        && a.creator == b.creator
        && a.update_authority == b.update_authority
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn full_volume_catalog_round_trips_the_recovery_envelope_and_frame() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let hash = |label: &str| -> [u8; 32] {
            ring::digest::digest(&ring::digest::SHA256, label.as_bytes())
                .as_ref()
                .try_into()
                .unwrap()
        };
        let account = Id::from_bytes([0xab; 16]);
        let mut catalog = RecoveryCatalog {
            account,
            volumes: Vec::new(),
            invitations: Vec::new(),
            scopes: Vec::new(),
        };
        for n in 0..256 {
            let volume = Volume {
                id: Id::from_bytes(hash(&format!("id-{n}"))[..16].try_into().unwrap()),
                owner: account,
                name: (0..6)
                    .map(|block| URL_SAFE_NO_PAD.encode(hash(&format!("name-{n}-{block}"))))
                    .collect::<String>()[..256]
                    .to_string(),
                share_key_with_service: true,
                personal: false,
            };
            let credentials = Credentials {
                database_key: hash(&format!("database-{n}")),
                blob_key: hash(&format!("blob-{n}")),
            };
            let seed = hash(&format!("seed-{n}"))[..16].try_into().unwrap();
            let build_id = crate::identifier::encode(&[0x11; 32]);
            catalog.invitations.push(Invitation {
                seed,
                session: crate::store::Session {
                    id: crate::store::invitation::session_id(&seed).unwrap(),
                    root_volume: volume.id,
                    shared_volume: None,
                    creator: account,
                    app: crate::package::AppIdentity {
                        publisher: account,
                        app_id: format!("app-{n}"),
                    },
                    selector: crate::store::BuildSelector::Exact { build_id: build_id.clone() },
                    resolved_build_id: build_id,
                    update_authority: crate::store::UpdateAuthority::Creator,
                },
                root: Some(volume.clone()),
                root_credentials: Some(credentials.clone()),
                shared: None,
            });
            catalog.volumes.push(CatalogVolume { volume, credentials });
        }
        let bytes = encode(&catalog).unwrap();
        assert!(bytes.len() > 128 * 1024);
        let encrypted = crate::store::recovery::seal(account, 1, &[7; 32], &bytes).unwrap();
        assert!(encrypted.len() > 128 * 1024);
        let encoded = crate::store::recovery::encode_ciphertext(&encrypted).unwrap();
        assert!(encoded.len() + 4096 < crate::network::ap::service::MAX_REQUEST_BYTES);
        let decrypted = crate::store::recovery::open(account, 1, &[7; 32], &encrypted).unwrap();
        assert_eq!(RecoveryCatalog::parse(&decrypted, account).unwrap().invitations.len(), 256);
        assert_eq!(RecoveryCatalog::parse(&decrypted, account).unwrap().volumes.len(), 256);
    }

    // Explicit ignored-benchmark ownership. Run traced benchmarks separately;
    // ordinary parallel store tests never initialize a process trace source.
    fn trace_capture() -> Option<(crate::trace::Recorder, std::path::PathBuf)> {
        use std::os::unix::fs::DirBuilderExt;
        let profile = std::env::var_os("PLAYBIT_PROFILE")?;
        crate::trace::initialize(8, &[]).unwrap();
        if !crate::trace::is_recording() {
            return None;
        }
        let directory = crate::engine::profile::isolated_benchmark_profile(Path::new(&profile))
            .expect("benchmark trace output requires an isolated profile")
            .join(crate::paths::TRACE);
        std::fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let recorder = crate::trace::Recorder::start(&directory, &directory, false).unwrap();
        Some((recorder, directory))
    }

    #[test]
    fn small_import_does_not_serialize_unrelated_local_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let incoming = source.create_volume("Incoming", true).unwrap();
        let catalog = source.recovery_catalog().unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        // The input limit bounds a remote envelope, not the receiver's existing
        // local records. Inject an oversized retained record directly: ordinary
        // volume creation rejects it. This exercises recovery's read boundary
        // without creating hundreds of replica files in the ordinary test suite.
        let mut retained = target.create_volume("Retained", false).unwrap();
        retained.name = "x".repeat(crate::store::recovery::MAX_PLAINTEXT_BYTES);
        put(&target.metadata, format!("volume/{}", retained.id).as_bytes(), &retained).unwrap();
        assert_eq!(target.import_recovery_catalog(&catalog, || true).unwrap(), Some((0, 1)));
        assert_eq!(target.volume(incoming.id).unwrap(), incoming);
        assert_eq!(target.volume(retained.id).unwrap(), retained);
        assert_eq!(target.import_recovery_catalog(&catalog, || true).unwrap(), Some((0, 0)));
    }

    #[test]
    fn concurrent_imports_adopt_each_identity_once() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let root = source.create_volume("Concurrent", true).unwrap();
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
        let catalog = source.recovery_catalog().unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        let seed = target.account_seed().unwrap();
        let barrier = std::sync::Barrier::new(2);
        let totals = std::thread::scope(|scope| {
            let import = || {
                barrier.wait();
                target.import_recovery_catalog(&catalog, || true).unwrap().unwrap()
            };
            let other = scope.spawn(import);
            let first = import();
            let second = other.join().unwrap();
            (first.0 + second.0, first.1 + second.1)
        });
        assert_eq!(totals, (1, 1));
        assert_eq!(target.account_seed().unwrap(), seed);
        assert_eq!(target.session(&session.id).unwrap(), session);
        assert_eq!(
            target.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
            source.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap()
        );
        assert!(target.pending_operations().unwrap().is_empty());
    }

    #[test]
    #[ignore = "reference filesystem timing; run without concurrent benchmarks"]
    fn benchmark_small_credential_import() {
        let _capture = trace_capture();
        for retained in [0, 128] {
            for incoming in [1, 16] {
                let directory = tempfile::tempdir().unwrap();
                let source = LocalStore::open(directory.path().join("source")).unwrap();
                let mut target = LocalStore::open(directory.path().join("target")).unwrap();
                target.bind_authenticated_account(source.account.id).unwrap();
                for index in 0..retained {
                    target.create_volume(&format!("Retained {index}"), false).unwrap();
                }
                for index in 0..incoming {
                    source.create_volume(&format!("Incoming {index}"), true).unwrap();
                }
                let catalog = source.recovery_catalog().unwrap();
                let start = std::time::Instant::now();
                assert_eq!(
                    target.import_recovery_catalog(&catalog, || true).unwrap(),
                    Some((0, incoming))
                );
                let preparation_ns = start.elapsed().as_nanos();
                let mut samples = Vec::with_capacity(1000);
                for _ in 0..1000 {
                    let start = std::time::Instant::now();
                    assert_eq!(
                        target.import_recovery_catalog(&catalog, || true).unwrap(),
                        Some((0, 0))
                    );
                    samples.push(start.elapsed().as_nanos());
                }
                crate::diagnostic!(
                    "credential_import {}",
                    serde_json::json!({
                        "retained_volumes": retained, "incoming_volumes": incoming,
                        "input_bytes": catalog.len(), "new_file_import_ns": preparation_ns,
                        "repeated_import_ns": samples,
                    })
                );
            }
        }
    }

    #[test]
    #[ignore = "reference filesystem timing; run without concurrent benchmarks"]
    fn benchmark_recovery_import_cancellation() {
        let _capture = trace_capture();
        for (volume_count, session_count) in [(1, 0), (16, 4)] {
            let directory = tempfile::tempdir().unwrap();
            let source = LocalStore::open(directory.path().join("source")).unwrap();
            let mut roots = Vec::new();
            for index in 0..volume_count {
                roots.push(source.create_volume(&format!("Recovery {index}"), true).unwrap().id);
            }
            for root in roots.iter().take(session_count) {
                source
                    .create_session(
                        &crate::package::AppIdentity {
                            publisher: source.account().id,
                            app_id: "test".into(),
                        },
                        BuildSelector::Exact { build_id: crate::identifier::encode(&[0xab; 32]) },
                        &crate::identifier::encode(&[0xab; 32]),
                        *root,
                    )
                    .unwrap();
            }
            let catalog = source.recovery_catalog().unwrap();
            let canceled = std::sync::atomic::AtomicBool::new(true);
            let mut preparation = Vec::new();
            let mut admission = Vec::new();
            let mut retry = Vec::new();
            for index in 0..100 {
                let path = directory.path().join(format!("target-{index}"));
                let mut target = LocalStore::open(&path).unwrap();
                target.bind_authenticated_account(source.account().id).unwrap();
                let before = target.recovery_catalog().unwrap();
                let start = std::time::Instant::now();
                assert_eq!(
                    target
                        .import_recovery_catalog(&catalog, || {
                            let start = std::time::Instant::now();
                            let allowed = !canceled.load(std::sync::atomic::Ordering::SeqCst);
                            admission.push(start.elapsed().as_nanos());
                            allowed
                        })
                        .unwrap(),
                    None
                );
                preparation.push(start.elapsed().as_nanos());
                assert_eq!(target.recovery_catalog().unwrap(), before);
                let start = std::time::Instant::now();
                assert_eq!(
                    target.import_recovery_catalog(&catalog, || true).unwrap(),
                    Some((session_count, volume_count + session_count))
                );
                retry.push(start.elapsed().as_nanos());
                drop(target);
                std::fs::remove_dir_all(path).unwrap();
            }
            for (name, mut samples) in [
                ("prepare-and-cancel", preparation),
                ("cancel-guard", admission),
                ("retry-import", retry),
            ] {
                samples.sort_unstable();
                crate::diagnostic!(
                    "recovery import roots={volume_count} sessions={session_count} catalog_bytes={} {name} n=100 p50={} ns p95={} ns p99={} ns max={} ns",
                    catalog.len(),
                    samples[49],
                    samples[94],
                    samples[98],
                    samples[99]
                );
                if name == "cancel-guard" {
                    assert!(samples[98] < 10_000);
                }
            }
        }
    }

    #[test]
    fn canceled_recovery_prepares_files_without_exposing_keys_and_can_retry() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let root = source.create_volume("Canceled import", true).unwrap();
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
        let catalog = source.recovery_catalog().unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account().id).unwrap();
        let before = target.recovery_catalog().unwrap();
        let canceled = AtomicBool::new(false);
        let imported = target
            .import_recovery_catalog(&catalog, || {
                assert!(
                    target
                        .path
                        .join(crate::paths::VOLUMES)
                        .join(root.id.to_string())
                        .join(crate::store::DatabaseId::Index.file_name())
                        .exists(),
                    "the cancellation boundary follows actual replica preparation"
                );
                canceled.store(true, Ordering::SeqCst);
                !canceled.load(Ordering::SeqCst)
            })
            .unwrap();
        assert_eq!(imported, None);
        assert_eq!(target.recovery_catalog().unwrap(), before);
        assert!(target.volumes().unwrap().is_empty());
        assert!(target.session(&session.id).is_err());
        assert_eq!(target.import_recovery_catalog(&catalog, || true).unwrap(), Some((1, 1)));
        assert_eq!(target.session(&session.id).unwrap(), session);
        // Cancellation after the durable transaction cannot remove its data.
        assert_eq!(target.import_recovery_catalog(&catalog, || false).unwrap(), None);
        assert_eq!(target.session(&session.id).unwrap(), session);
        assert_eq!(target.volumes().unwrap().len(), 1);
    }

    #[test]
    fn recovery_preserves_device_auth_pending_invitation_and_current_session_selector() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let root = source.create_volume("Private root", true).unwrap();
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
        let catalog = source.recovery_catalog().unwrap();
        let plaintext: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
        let mut keys =
            plaintext.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, ["account", "invitations", "volumes"]);
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        let seed = target.account_seed().unwrap();
        let public_key = target.account().public_key.clone();
        target
            .preview_invitation(
                &source
                    .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
                    .unwrap(),
                crate::release_channel::ReleaseChannel::Public,
            )
            .unwrap();
        target.bind_authenticated_account(source.account().id).unwrap();
        assert_eq!(target.import_recovery_catalog(&catalog, || true).unwrap(), Some((1, 1)));
        assert_eq!(target.account_seed().unwrap(), seed);
        assert_eq!(target.account().public_key, public_key);
        assert_eq!(target.pending_invitations().unwrap().len(), 1);
        assert!(target.pending_operations().unwrap().is_empty());
        assert!(target.replica_root(root.id, crate::store::DatabaseId::Index).unwrap().is_none());
        let mut current = target.stored_session(&session.id).unwrap();
        current.session.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
        current.session.selector =
            BuildSelector::Exact { build_id: crate::identifier::encode(&[0xcd; 32]) };
        put(&target.metadata, format!("session/{}", session.id).as_bytes(), &current).unwrap();
        assert_eq!(target.import_recovery_catalog(&catalog, || true).unwrap(), Some((0, 0)));
        assert_eq!(
            target.session(&session.id).unwrap().resolved_build_id,
            crate::identifier::encode(&[0xcd; 32])
        );
        assert_eq!(
            target.session_proof(&session.id, target.account().id).unwrap(),
            source.session_proof(&session.id, source.account().id).unwrap()
        );
    }

    #[test]
    fn recovery_merges_device_catalogs_and_rejects_wrong_accounts_and_keys_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let root = source.create_volume("Saved", true).unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        assert!(
            target.import_recovery_catalog(&source.recovery_catalog().unwrap(), || true).is_err()
        );
        assert!(target.volumes().unwrap().is_empty());
        target.bind_authenticated_account(source.account().id).unwrap();
        target.import_recovery_catalog(&source.recovery_catalog().unwrap(), || true).unwrap();
        target.create_volume("This device", false).unwrap();
        source.create_volume("Other device", true).unwrap();
        let merged = target.merge_recovery_catalog(&source.recovery_catalog().unwrap()).unwrap();
        assert_eq!(RecoveryCatalog::parse(&merged, source.account().id).unwrap().volumes.len(), 3);
        let before = target.recovery_catalog().unwrap();
        let mut corrupt =
            RecoveryCatalog::parse(&source.recovery_catalog().unwrap(), source.account().id)
                .unwrap();
        corrupt.volumes.iter_mut().find(|v| v.volume.id == root.id).unwrap().credentials.blob_key =
            [0; 32];
        assert!(target.import_recovery_catalog(&encode(&corrupt).unwrap(), || true).is_err());
        assert_eq!(target.recovery_catalog().unwrap(), before);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn account_key_and_recovery_state_survive_relocation() {
        let root = tempfile::tempdir().expect("profiles");
        let original = root.path().join("original");
        let store = LocalStore::open(&original).expect("new profile");
        let state = RecoveryState {
            secret: [71; 16],
            acknowledged: true,
            revision: 3,
            synchronized_digest: None,
            pending: None,
        };
        let account = store.account.id;
        store.protect_account_key(account, &state.secret).expect("prepared key");
        drop(store);
        let store = LocalStore::open(&original).expect("reopen prepared key");
        assert_eq!(store.prepared_account_key(account).expect("prepared"), Some(state.secret));
        let conflicting = RecoveryState { secret: [72; 16], ..state.clone() };
        assert!(store.save_recovery_state(&conflicting).is_err());
        assert!(store.recovery_state().expect("unpublished state").is_none());
        store.save_recovery_state(&state).expect("atomic recovery publication");
        assert!(store.prepared_account_key(account).expect("retired preparation").is_none());
        drop(store);
        let moved = root.path().join("moved");
        fs::rename(&original, &moved).expect("move stopped profile");
        let reopened = LocalStore::open(moved).expect("self-contained profile");
        let recovered = reopened.recovery_state().expect("read").expect("retained state");
        assert_eq!(recovered.secret, state.secret);
        assert_eq!(recovered.revision, state.revision);
        assert_eq!(reopened.account.id, account);
        #[cfg(target_vendor = "apple")]
        {
            assert!(
                reopened
                    .record_account_key_backup(
                        Id::generate().expect("other account"),
                        &state.secret,
                        AccountKeyBackup::Saved
                    )
                    .is_err()
            );
            assert!(
                reopened
                    .record_account_key_backup(account, &[72; 16], AccountKeyBackup::Saved)
                    .is_err()
            );
            reopened
                .record_account_key_backup(account, &state.secret, AccountKeyBackup::Failed)
                .expect("independent backup failure");
            assert_eq!(
                reopened.account_key_backup().expect("backup state"),
                AccountKeyBackup::Failed
            );
            assert_eq!(
                reopened.recovery_state().expect("local state").expect("ready").secret,
                state.secret
            );
        }
        let path = reopened.path.clone();
        let mut tx = reopened.metadata.begin_write(now_unix_secs()).expect("damage fixture");
        tx.del(b"recovery").expect("remove credential");
        tx.commit().expect("commit damage");
        drop(reopened);
        assert!(LocalStore::open(&path).is_err());
        assert!(path.join(crate::paths::PROFILE_DB).exists());
    }
    #[test]
    fn ephemeral_jobs_retain_encrypted_intents_without_persisting_recovery_keys() {
        // Exercise both transactional recovery readers, retain a real account
        // tree, and reopen without a secret. Only an explicit host key unlocks it.
        let directory = tempfile::tempdir().expect("job profile");
        let store =
            LocalStore::open_with_ephemeral_recovery(directory.path()).expect("ephemeral store");
        let secret = [37; 16];
        let recovery = RecoveryState {
            secret,
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        };
        store.save_recovery_state(&recovery).expect("RAM recovery");
        store.set_display_name("Synthetic account").expect("transactional display name");
        let volume = store.create_volume("Fixture", true).expect("transactional volume record");
        store.seed_account_records().expect("encrypted intents");
        let record = store.pending_account_record(None).expect("outbox").expect("record");
        store.stage_account_record(&record).expect("encrypted account tree");
        {
            let metadata = store.metadata.begin_read().expect("metadata");
            for key in [b"recovery".as_slice(), b"prepared-account-key", b"recovery-key-published"]
            {
                assert!(
                    !metadata.has(key).expect("metadata key"),
                    "ephemeral mode must retain no account key record"
                );
            }
        }
        assert!(store.protect_account_key(store.account().id, &secret).is_err());
        drop(store);
        assert!(
            LocalStore::open(directory.path()).is_err(),
            "ordinary clients must reject a job profile"
        );
        let ordinary = tempfile::tempdir().expect("ordinary profile");
        drop(LocalStore::open(ordinary.path()).expect("ordinary store"));
        assert!(LocalStore::open_with_ephemeral_recovery(ordinary.path()).is_err());
        let store = LocalStore::open_with_ephemeral_recovery(directory.path()).expect("resume job");
        assert!(store.recovery_state().expect("RAM state").is_none());
        assert!(
            store.account_replica().is_err(),
            "restart cannot silently unlock retained encrypted records"
        );
        assert_eq!(store.volumes().expect("retained resources")[0].id, volume.id);
        store.save_recovery_state(&recovery).expect("host supplied the escrowed key");
        store.account_replica().expect("unlock existing encrypted tree");
        drop(store);
        let bytes = fs::read(directory.path().join(crate::paths::PROFILE_DB))
            .expect("retained metadata file");
        let encoded = serde_json::to_vec(&secret).expect("key representation");
        assert!(
            !bytes.windows(encoded.len()).any(|part| part == encoded),
            "recovery plaintext must never enter the backing file"
        );
    }
}
