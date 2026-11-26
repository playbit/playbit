//! Bounded first-root creation. Replica files are private until one profile
//! transaction publishes their catalog, credentials, registration and receipt.
//! A single durable intent bounds abandoned work; recovery discards only that
//! unpublished root. Opening a published root only establishes additive schema.
use super::*;
use crate::store::{DatabaseId, EntryId};
const INITIALIZATION_CACHE_BYTES: usize = 512 * 1024;
const INTENT: &[u8] = b"initializing-root";
const APP_ROOT: &[u8] = b"local-app-root";

#[derive(serde::Serialize, serde::Deserialize)]
struct AppRootBinding {
    id: Id,
    created: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootPurpose<'a> {
    App,
    Personal,
    Provider { independent: bool },
    Automatic(&'a automatic::AutomaticBinding),
}

impl LocalStore {
    pub(super) fn initialize_automatic(
        &self,
        id: Id,
        binding: &automatic::AutomaticBinding,
        declaration: &RootInitialization,
    ) -> Result<Volume> {
        let fingerprint = declaration.fingerprint()?;
        let expected =
            self.automatic_volume_id(&binding.app, binding.session.as_deref(), binding.role)?;
        if id != expected
            || (binding.role == crate::package::AutomaticVolumeRole::Root
                && binding.app.publisher != self.account.id)
        {
            return Err(Error::AccessDenied("automatic initialization binding differs"));
        }
        let operations = self.access_operations(&Access::owner(self.account.id), id)?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        self.check_app_data_erasure(id, binding.role)?;
        if binding.role.is_local()
            && self.metadata.begin_read()?.has(app_removal::erased_local_volume(id).as_bytes())?
            && self.app_removed(&binding.app)?
        {
            return Err(Error::AccessDenied("re-add the app before opening its erased local data"));
        }
        let shared = binding.role == crate::package::AutomaticVolumeRole::Session
            && binding
                .session
                .as_ref()
                .map(|id| self.session(id))
                .transpose()?
                .is_some_and(|session| session.shared_volume == Some(id));
        self.create_initial_root(
            id,
            match binding.role {
                crate::package::AutomaticVolumeRole::Root => "World",
                crate::package::AutomaticVolumeRole::Session => "Shared",
                crate::package::AutomaticVolumeRole::User
                | crate::package::AutomaticVolumeRole::UserSession => "User",
                crate::package::AutomaticVolumeRole::Local => "Local",
                crate::package::AutomaticVolumeRole::LocalSession => "Local session",
            },
            shared || binding.role == crate::package::AutomaticVolumeRole::Root,
            declaration,
            RootPurpose::Automatic(binding),
            fingerprint,
        )
    }
    /// Bind a dedicated native application's profile to one generated root ID.
    /// This local association is durable before initialization, so an interrupted
    /// first launch retries the same ID. It is not a shared volume or app identity.
    /// The profile must have no preexisting volumes when first binding it.
    pub fn prepare_app_root(&self, name: &str, declaration: &RootInitialization) -> Result<Volume> {
        if name.trim().is_empty() || name.len() > 256 {
            return Err(Error::InvalidInput("app name must contain 1 to 256 bytes"));
        }
        let fingerprint = declaration.fingerprint()?;
        let id = {
            let _profile = self.lock_profile()?;
            self.require_account_authority()?;
            let snapshot = self.metadata.begin_read()?;
            if let Some(bytes) = snapshot.get(APP_ROOT)? {
                decode::<AppRootBinding>(&bytes)?.id
            } else {
                if let Some(entry) = snapshot.iter_from(b"volume/").next() {
                    let (key, _) = entry?;
                    if key.starts_with(b"volume/") {
                        return Err(Error::InvalidInput("native app requires a dedicated profile"));
                    }
                }
                let id = Id::generate()?;
                let mut tx = self.metadata.begin_write(now_unix_secs())?;
                tx.put(APP_ROOT, &encode(&AppRootBinding { id, created: false })?)?;
                tx.commit()?;
                id
            }
        };
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, id)?;
        let _guards = operations.lock()?;
        let profile = self.lock_profile()?;
        let binding: AppRootBinding = required(&self.metadata, "local-app-root")?;
        if binding.id != id || id == Id::from_bytes([0; 16]) {
            return Err(Error::InvalidInput("app root binding changed"));
        }
        if binding.created {
            drop(profile);
            return self.open_initialized_root_locked(&access, id, declaration);
        }
        if self.metadata.begin_read()?.has(receipt_key(id).as_bytes())? {
            return Err(Error::InvalidInput("unpublished app binding has a root receipt"));
        }
        self.create_initial_root(id, name, true, declaration, RootPurpose::App, fingerprint)
    }

    /// Open a previously created app root and establish additive declarations by
    /// stable entry/type/field IDs. Seeds, names and permissions are never replayed.
    pub fn open_initialized_root(
        &self,
        access: &Access,
        id: Id,
        declaration: &RootInitialization,
    ) -> Result<Volume> {
        declaration.fingerprint()?;
        let operations = self.access_operations(access, id)?;
        let _guards = operations.lock()?;
        self.open_initialized_root_locked(access, id, declaration)
    }

    fn open_initialized_root_locked(
        &self,
        access: &Access,
        id: Id,
        declaration: &RootInitialization,
    ) -> Result<Volume> {
        let volume = self.volume(id)?;
        if volume.owner != self.account.id || volume.personal {
            return Err(Error::AccessDenied("root ownership or purpose differs"));
        }
        // The original receipt is historical evidence, not today's template hash.
        let _: [u8; 32] = required(&self.metadata, receipt_key(id))?;
        self.authorize(access, &self.index(id)?, Permission::Read)?;
        self.establish_root_schemas(access, id, declaration)?;
        Ok(volume)
    }

    fn establish_root_schemas(
        &self,
        access: &Access,
        volume: Id,
        declaration: &RootInitialization,
    ) -> Result<()> {
        use crate::store::{
            DatabaseId, InitialDatabaseKind, MAX_REPLICA_DELTA_BYTES, volume_index,
        };
        // All schema facts belong to the volume's one Index database. Admission
        // and staging complete under the volume lock before a single commit.
        let database = self.db(volume, DatabaseId::Index)?;
        let snapshot = volume_index::IndexSnapshot::new(database.begin_read()?);
        for seed in &declaration.databases {
            let current = snapshot.live_entry(seed.id)?;
            if std::mem::discriminant(&current.info.kind)
                != std::mem::discriminant(&seed.entry_kind())
                || current.info.protocol != Some(seed.protocol())
            {
                return Err(Error::InvalidInput("root database kind or protocol differs"));
            }
            if let InitialDatabaseKind::ObjectDb { .. } = &seed.kind {
                self.object_database_schema(
                    access,
                    volume_index::EntryAddress { volume, entry: seed.id },
                    Permission::Write,
                    false,
                )?;
            }
        }
        for seed in &declaration.blobs {
            if !matches!(
                snapshot.live_entry(seed.id)?.info.kind,
                volume_index::EntryKind::Blob { .. }
            ) {
                return Err(Error::InvalidInput("root blob kind differs"));
            }
        }
        drop(snapshot);
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        let origin =
            volume_index::namespace::Origin { installation: self.installation(), label: None };
        let mut changed = false;
        for seed in &declaration.databases {
            if let InitialDatabaseKind::ObjectDb { schema, .. } = &seed.kind {
                changed |= volume_index::establish_schema(
                    &mut tx,
                    seed.id,
                    schema,
                    EntryId::generate()?,
                    &origin,
                )?;
            }
        }
        if changed {
            let delta = tx.commit_with_delta()?;
            self.changes.publish(volume, DatabaseId::Index, delta);
        }
        Ok(())
    }

    /// Trusted engine management, not a scoped app operation. The caller saves
    /// `id` in its source/profile association before requesting initialization.
    /// Reuse accepts only the original declaration; it preserves subsequent
    /// object edits, deletion, renaming and permission changes. Different seeds
    /// or schema return an explicit error, never a reset or implicit migration.
    pub fn initialize_root(
        &self,
        id: Id,
        name: &str,
        share_key_with_service: bool,
        declaration: &RootInitialization,
    ) -> Result<Volume> {
        self.initialize_volume(id, name, share_key_with_service, declaration, false)
    }

    pub fn initialize_personal_volume(
        &self,
        id: Id,
        name: &str,
        share_key_with_service: bool,
        declaration: &RootInitialization,
    ) -> Result<Volume> {
        self.initialize_volume(id, name, share_key_with_service, declaration, true)
    }

    /// File Provider's stable template identity is a request identity, not an
    /// instruction to reopen any preexisting volume with the same ID. Publish
    /// the original name receipt atomically with its initialized namespace.
    pub fn provider_create_volume(&self, id: Id, name: &str, independent: bool) -> Result<Volume> {
        validate_volume_label(name)?;
        if id == Id::from_bytes([0; 16]) || name.contains('/') || name.contains('\0') {
            return Err(Error::InvalidInput("invalid provider volume creation"));
        }
        let declaration = RootInitialization {
            permissions: Default::default(),
            databases: Vec::new(),
            blobs: Vec::new(),
        };
        let fingerprint = declaration.fingerprint()?;
        let operations = self.access_operations(&Access::owner(self.account.id), id)?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        let snapshot = self.metadata.begin_read()?;
        if let Some(bytes) = snapshot.get(provider_creation_key(id).as_bytes())? {
            if decode::<String>(&bytes)? != name {
                return Err(Error::AlreadyExists("provider volume creation request differs"));
            }
            let volume = self.volume(id)?;
            if !self.personal_volumes()?.iter().any(|item| item.id == id) {
                return Err(Error::AccessDenied("File Provider personal volume"));
            }
            return Ok(volume);
        }
        if snapshot.has(receipt_key(id).as_bytes())? {
            return Err(Error::AlreadyExists("provider volume creation identity"));
        }
        drop(snapshot);
        if self.personal_volumes()?.len() >= crate::store::file_provider::PROVIDER_VOLUME_LIMIT {
            return Err(Error::LimitExceeded);
        }
        self.create_initial_root(
            id,
            name,
            true,
            &declaration,
            RootPurpose::Provider { independent },
            fingerprint,
        )
    }

    fn initialize_volume(
        &self,
        id: Id,
        name: &str,
        share_key_with_service: bool,
        declaration: &RootInitialization,
        personal: bool,
    ) -> Result<Volume> {
        if id == Id::from_bytes([0; 16]) || name.trim().is_empty() || name.len() > 256 {
            return Err(Error::InvalidInput("root requires a nonzero ID and 1 to 256 name bytes"));
        }
        let fingerprint = declaration.fingerprint()?;
        let operations = self.access_operations(&Access::owner(self.account.id), id)?;
        let _guards = operations.lock()?;
        // Only initialization holds this guard across replica construction.
        // It bounds private staging to one root and never waits on network I/O.
        let _profile = self.lock_profile()?;
        self.create_initial_root(
            id,
            name,
            share_key_with_service,
            declaration,
            if personal { RootPurpose::Personal } else { RootPurpose::App },
            fingerprint,
        )
    }

    fn initialized_volume(
        &self,
        id: Id,
        purpose: RootPurpose<'_>,
        fingerprint: [u8; 32],
    ) -> Result<Option<Volume>> {
        let snapshot = self.metadata.begin_read()?;
        let Some(bytes) = snapshot.get(receipt_key(id).as_bytes())? else {
            return Ok(None);
        };
        let retained = self.automatic_binding(id)?;
        match (purpose, retained.as_ref()) {
            (RootPurpose::Automatic(expected), Some(actual)) if expected == actual => {}
            (RootPurpose::Automatic(_), _) | (_, Some(_)) => {
                return Err(Error::AccessDenied("volume initialization binding differs"));
            }
            _ => {}
        }
        if decode::<[u8; 32]>(&bytes)? != fingerprint {
            return Err(Error::InvalidInput(
                "root initialization differs from its original declaration",
            ));
        }
        let volume = self.volume(id)?;
        let personal = matches!(purpose, RootPurpose::Personal | RootPurpose::Provider { .. });
        if volume.personal != personal {
            return Err(Error::InvalidInput("volume initialization purpose differs"));
        }
        Ok(Some(volume))
    }
    fn shared_initialization_session(
        &self,
        volume: Id,
        purpose: RootPurpose<'_>,
    ) -> Result<Option<Session>> {
        let RootPurpose::Automatic(binding) = purpose else {
            return Ok(None);
        };
        if binding.role != crate::package::AutomaticVolumeRole::Session {
            return Ok(None);
        }
        let Some(id) = &binding.session else {
            return Ok(None);
        };
        let session = self.session(id)?;
        Ok((session.shared_volume == Some(volume)).then_some(session))
    }
    // Caller holds the account/volume operation guard and the profile lock.
    fn create_initial_root(
        &self,
        id: Id,
        name: &str,
        share_key_with_service: bool,
        declaration: &RootInitialization,
        purpose: RootPurpose<'_>,
        fingerprint: [u8; 32],
    ) -> Result<Volume> {
        self.recover_root_initialization()?;
        let personal = matches!(purpose, RootPurpose::Personal | RootPurpose::Provider { .. });
        if let Some(volume) = self.initialized_volume(id, purpose, fingerprint)? {
            return Ok(volume);
        }
        if self.metadata.begin_read()?.has(format!("volume/{id}").as_bytes())? {
            return Err(Error::AlreadyExists("root without an initialization receipt"));
        }
        let directory = self.path.join(crate::paths::VOLUMES).join(id.to_string());
        if directory.try_exists().map_err(io)? {
            return Err(Error::AlreadyExists("root directory"));
        }
        self.enable_initial_root_features(id, share_key_with_service, purpose)?;
        let volume = Volume {
            id,
            name: name.into(),
            owner: self.account.id,
            personal,
            share_key_with_service,
        };
        let credentials = Credentials { database_key: random_array()?, blob_key: random_array()? };
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(INTENT, &encode(&id)?)?;
        tx.commit()?;
        #[cfg(test)]
        checkpoint(1)?;
        let _preparation = entry_blobs::Preparation::enter(&self.entry_blobs)?;
        self.prepare_initial_root(&volume, &credentials, declaration, purpose)?;
        sync_directory(&directory)?;
        sync_directory(&self.path.join(crate::paths::VOLUMES))?;
        #[cfg(test)]
        checkpoint(5)?;
        self.publish_initial_root(
            &volume,
            credentials,
            fingerprint,
            match purpose {
                RootPurpose::Provider { independent } => Some((name, independent)),
                _ => None,
            },
            match purpose {
                RootPurpose::Automatic(binding) => Some(binding),
                _ => None,
            },
        )?;
        if matches!(purpose, RootPurpose::Automatic(_)) {
            // Private scopes have no replicated account record to advance readers.
            // Publish initialization before availability wakes local consumers.
            self.changes.observed_changed(Some(volume.id));
        }
        // New subscribers discover a complete root. Existing replication wakes
        // only after publication and uses its ordinary initial snapshot path.
        self.changes.volume_available(volume.id);
        Ok(volume)
    }
    fn enable_initial_root_features(
        &self,
        id: Id,
        share_key_with_service: bool,
        purpose: RootPurpose<'_>,
    ) -> Result<()> {
        if matches!(purpose, RootPurpose::Automatic(_)) {
            profile_format::enable_local_app_scopes(self)?;
            if share_key_with_service {
                profile_format::enable_hosted_app_scopes(self)?;
            }
            if self.shared_initialization_session(id, purpose)?.is_some() {
                profile_format::enable_shared_sessions(self)?;
            }
        }
        Ok(())
    }
    fn prepare_initial_root(
        &self,
        volume: &Volume,
        credentials: &Credentials,
        declaration: &RootInitialization,
        purpose: RootPurpose<'_>,
    ) -> Result<()> {
        let directory = self.path.join(crate::paths::VOLUMES).join(volume.id.to_string());
        fs::DirBuilder::new().mode(0o700).create(&directory).map_err(io)?;
        let create = |id: DatabaseId| {
            create_replica(
                &directory.join(id.file_name()),
                id.hash_key(&credentials.database_key, volume.id),
                INITIALIZATION_CACHE_BYTES,
            )
        };
        drop(create(DatabaseId::Revocations)?);
        #[cfg(test)]
        checkpoint(2)?;
        for seed in &declaration.databases {
            let database = create(DatabaseId::Entry(seed.id))?;
            seed.write_objects(&database, volume.owner, now_unix_secs())?;
        }
        let blobs = self.prepare_initial_blobs(volume.id, credentials, &declaration.blobs)?;
        let shared = self.shared_initialization_session(volume.id, purpose)?;
        let mut index =
            MetadataPolicy::new(volume.clone(), declaration.volume_permissions(shared.is_some()))?;
        if let Some(session) = shared {
            index.provisioned_versions.insert(session.resolved_build_id);
        } else if let RootPurpose::Automatic(binding) = purpose
            && binding.role == crate::package::AutomaticVolumeRole::Root
        {
            index.provisioned_versions.insert(binding.build.clone());
        }
        #[cfg(test)]
        checkpoint(3)?;
        let db = create(DatabaseId::Index)?;
        let mut tx = db.begin_write(now_unix_secs())?;
        index.write_entries(&mut tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
        for seed in &declaration.databases {
            crate::store::volume_index::create_at_path(
                &mut tx,
                seed.id,
                &seed.name,
                &crate::store::volume_index::EntryInfo {
                    namespace: Default::default(),
                    origin: crate::store::volume_index::namespace::Origin {
                        installation: self.installation(),
                        label: None,
                    },
                    initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                    created_revision: crate::store::EntryId::generate()?,
                    name: seed.name.rsplit('/').next().expect("validated seed path").to_owned(),
                    kind: seed.entry_kind(),
                    protocol: Some(seed.protocol()),
                },
            )?;
            crate::store::volume_index::mark_backing_ready(&mut tx, seed.id)?;
        }
        for (id, path, mut info) in blobs {
            super::entry_blobs::stamp_publication(&mut info.kind)?;
            crate::store::volume_index::create_at_path(&mut tx, id, &path, &info)?;
            crate::store::volume_index::mark_backing_ready(&mut tx, id)?;
        }
        tx.commit()?;
        #[cfg(test)]
        checkpoint(4)?;
        Ok(())
    }
    fn prepare_initial_blobs(
        &self,
        volume: Id,
        credentials: &Credentials,
        seeds: &[crate::store::InitialBlob],
    ) -> Result<Vec<(EntryId, String, crate::store::volume_index::EntryInfo)>> {
        let mut entries = Vec::with_capacity(seeds.len());
        for seed in seeds {
            let revision = EntryId::generate()?;
            let kind = self.prepare_entry_blob_with_key(
                crate::store::volume_index::EntryAddress { volume, entry: seed.id },
                revision,
                &mut &seed.bytes[..],
                &credentials.blob_key,
                true,
                || Ok(()),
            )?;
            entries.push((
                seed.id,
                seed.name.clone(),
                crate::store::volume_index::EntryInfo {
                    namespace: Default::default(),
                    origin: crate::store::volume_index::namespace::Origin {
                        installation: self.installation(),
                        label: None,
                    },
                    initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                    created_revision: revision,
                    name: seed.name.rsplit('/').next().expect("validated seed path").to_owned(),
                    kind,
                    protocol: None,
                },
            ));
        }
        Ok(entries)
    }
    fn publish_initial_root(
        &self,
        volume: &Volume,
        credentials: Credentials,
        fingerprint: [u8; 32],
        provider_creation: Option<(&str, bool)>,
        automatic_binding: Option<&automatic::AutomaticBinding>,
    ) -> Result<()> {
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterVolume { volume: volume.clone() },
        };
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("volume/{}", volume.id).as_bytes(), &encode(volume)?)?;
        tx.put(format!("credentials/{}", volume.id).as_bytes(), &encode(&credentials)?)?;
        let shared = automatic_binding.is_some_and(|binding| {
            binding.role == crate::package::AutomaticVolumeRole::Session
                && volume.share_key_with_service
        });
        if let Some(binding) = automatic_binding {
            automatic::write_binding(&mut tx, volume.id, binding)?;
            if binding.role.is_local() {
                tx.del(app_removal::erased_local_volume(volume.id).as_bytes())?;
            }
        }
        if automatic_binding.is_none() || volume.share_key_with_service {
            tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        }
        tx.put(receipt_key(volume.id).as_bytes(), &encode(&fingerprint)?)?;
        self.complete_initial_root_intent(&mut tx, volume.id, provider_creation)?;
        if let Some(binding) = automatic_binding.filter(|binding| {
            binding.role == crate::package::AutomaticVolumeRole::Root
                && volume.share_key_with_service
        }) {
            let scope = AppScope::World {
                app: binding.app.clone(),
                build: binding.build.clone(),
                initialization: fingerprint,
            };
            self.write_app_scope(&mut tx, &scope)?;
        }
        let mut changed =
            if automatic_binding.is_some() && (!volume.share_key_with_service || shared) {
                false
            } else {
                self.queue_account_record(
                    &mut tx,
                    &crate::store::account::Record::Volume {
                        volume: volume.clone(),
                        credentials: credentials.clone(),
                    },
                )?
            };
        if shared {
            changed |= self.publish_shared_session(
                &mut tx,
                automatic_binding.expect("Shared initialization has its automatic binding"),
                volume,
                &credentials,
            )?;
        }
        tx.commit()?;
        self.committed_account_record(changed);
        if volume.share_key_with_service {
            self.changes.account_changed();
        }
        #[cfg(test)]
        checkpoint(6)?;
        Ok(())
    }
    fn complete_initial_root_intent(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        volume: Id,
        provider_creation: Option<(&str, bool)>,
    ) -> Result<()> {
        if let Some((name, independent)) = provider_creation {
            tx.put(provider_creation_key(volume).as_bytes(), &encode(name)?)?;
            if independent {
                self.queue_provider_publication(tx, volume)?;
            }
        }
        let binding =
            tx.get(APP_ROOT)?.map(|bytes| decode::<AppRootBinding>(&bytes)).transpose()?;
        if binding.is_some_and(|binding| binding.id == volume) {
            tx.put(APP_ROOT, &encode(&AppRootBinding { id: volume, created: true })?)?;
        }
        tx.del(INTENT)?;
        Ok(())
    }
    /// Caller owns the profile guard, or is opening the exclusively locked
    /// profile before any concurrent caller can obtain it. A published root and
    /// intent cannot coexist; treat that as corruption without deleting files.
    pub(super) fn recover_root_initialization(&self) -> Result<()> {
        let snapshot = self.metadata.begin_read()?;
        let Some(bytes) = snapshot.get(INTENT)? else { return Ok(()) };
        let id: Id = decode(&bytes)?;
        if id == Id::from_bytes([0; 16])
            || snapshot.has(format!("volume/{id}").as_bytes())?
            || snapshot.has(format!("credentials/{id}").as_bytes())?
            || snapshot.has(receipt_key(id).as_bytes())?
        {
            return Err(Error::corrupt("volume provisioning found conflicting retained metadata"));
        }
        drop(bytes);
        drop(snapshot);
        match fs::remove_dir_all(self.path.join(crate::paths::VOLUMES).join(id.to_string())) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        sync_directory(&self.path.join(crate::paths::VOLUMES))?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(INTENT)?;
        tx.commit()?;
        Ok(())
    }
}
pub(super) fn receipt_key(id: Id) -> String {
    format!("root-initialization/{id}")
}
fn provider_creation_key(id: Id) -> String {
    format!("provider-volume-creation/{id}")
}
pub(super) fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path).map_err(io)?;
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory)).map_err(|e| io(e.into()))
}

#[cfg(test)]
thread_local! { pub(super) static INTERRUPT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
fn checkpoint(step: u8) -> Result<()> {
    if INTERRUPT.get() == step {
        return Err(Error::System("injected initialization interruption".into()));
    }
    Ok(())
}
#[cfg(test)]
mod tests;
