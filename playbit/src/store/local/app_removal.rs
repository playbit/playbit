//! App list membership travels through the account replica. A local erasure
//! intent records only the initiating installation's proven private bindings.
use super::*;
use crate::package::{AppIdentity, AutomaticVolumeRole as Role};

const ERASE: &str = "erase-app-local/";
// Bound only the private IDs retained for one removal transaction. Unrelated
// catalog rows and hidden sessions are streamed from a single read snapshot.
const MAX_LOCAL_TARGETS: usize = 4096;

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct AppRemovalImpact {
    pub user_volumes: bool,
    pub local_data: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppRemovalResult {
    Confirmation(AppRemovalImpact),
    Removed { erased_volumes: Vec<Id>, cleanup_error: Option<String> },
}

pub(super) fn erased_local_volume(id: Id) -> String {
    format!("erased-app-local/{id}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemovalInventory {
    Membership,
    Impact,
    Erasure,
}

struct Inventory {
    removed: bool,
    listed: bool,
    impact: AppRemovalImpact,
    local: BTreeSet<Id>,
}

impl LocalStore {
    /// Check the target set before asking any running app to quit. Commit
    /// repeats the inventory under the profile writer after they have stopped.
    #[cfg(feature = "app-host")]
    pub(crate) fn preflight_app_erasure(&self, app: &AppIdentity) -> Result<bool> {
        let _consent = self.lock_account_authority()?;
        let inventory = self.app_removal_inventory(app, RemovalInventory::Erasure)?;
        if !inventory.listed && !inventory.removed {
            return Err(Error::NotFound("app"));
        }
        Ok(inventory.listed)
    }

    pub fn app_removal_impact(&self, app: &AppIdentity) -> Result<AppRemovalImpact> {
        let _consent = self.lock_account_authority()?;
        let inventory = self.app_removal_inventory(app, RemovalInventory::Impact)?;
        if !inventory.listed {
            return Err(Error::NotFound("app"));
        }
        Ok(inventory.impact)
    }

    // The menu reads one snapshot without taking the profile writer. Commit
    // repeats the same discovery under that writer. Leave hides a session
    // without releasing its volumes; unopened slots own no data.
    fn app_removal_inventory(
        &self,
        app: &AppIdentity,
        purpose: RemovalInventory,
    ) -> Result<Inventory> {
        app.validate().map_err(Error::InvalidInput)?;
        let snapshot = self.metadata.begin_read()?;
        let removed = snapshot.has(self.removed_app_key(app).as_bytes())?;
        let mut inventory = Inventory {
            removed,
            listed: !removed
                && snapshot
                    .has(format!("imported-app/{}/{}", app.publisher, app.app_id).as_bytes())?,
            impact: AppRemovalImpact::default(),
            local: BTreeSet::new(),
        };
        if removed {
            // An invitation can list an app again before the person joins.
            inventory.listed = self.has_pending_app_invitation(&snapshot, app)?;
            if !inventory.listed {
                return Ok(inventory);
            }
        }
        if purpose != RemovalInventory::Membership {
            self.collect_app_bindings(app, &snapshot, purpose, &mut inventory)?;
        } else {
            self.inspect_app_sessions(&snapshot, app, purpose, &mut inventory)?;
        }
        if !inventory.listed {
            inventory.listed = self.has_pending_app_invitation(&snapshot, app)?;
        }
        Ok(inventory)
    }

    fn inspect_app_sessions(
        &self,
        snapshot: &kvdb::ReadTxn<'_, RedbStorage>,
        app: &AppIdentity,
        purpose: RemovalInventory,
        inventory: &mut Inventory,
    ) -> Result<()> {
        if purpose == RemovalInventory::Membership && (inventory.listed || inventory.removed) {
            return Ok(());
        }
        for prefix in [b"session/".as_slice(), b"local-session/".as_slice()] {
            for row in snapshot.iter_from(prefix) {
                let (key, bytes) = row?;
                if !key.starts_with(prefix) {
                    break;
                }
                let (session, user) = if prefix == b"session/" {
                    (decode::<StoredSession>(&bytes)?.session, None)
                } else {
                    let local: automatic::LocalSession = decode(&bytes)?;
                    (local.session, local.user_volume)
                };
                if session.app == *app
                    && !inventory.listed
                    && !inventory.removed
                    && !snapshot
                        .has(format!("deleted-volume/{}", session.replica_volume()).as_bytes())?
                    && !snapshot.has(session_list::hidden(&session.id).as_bytes())?
                {
                    inventory.listed = true;
                    if purpose == RemovalInventory::Membership {
                        return Ok(());
                    }
                }
                if purpose == RemovalInventory::Membership {
                    continue;
                }
                self.protect_session_bindings(
                    snapshot,
                    app,
                    &session,
                    user,
                    prefix == b"session/",
                    inventory,
                )?;
            }
        }
        inventory.impact.local_data |= !inventory.local.is_empty();
        Ok(())
    }

    fn collect_app_bindings(
        &self,
        app: &AppIdentity,
        snapshot: &kvdb::ReadTxn<'_, RedbStorage>,
        purpose: RemovalInventory,
        inventory: &mut Inventory,
    ) -> Result<()> {
        let mut targets = BTreeSet::new();
        for row in snapshot.iter_from(b"automatic-volume/") {
            let (key, value) = row?;
            let Some(suffix) = key.strip_prefix(b"automatic-volume/") else { break };
            let binding: automatic::AutomaticBinding = decode(&value)?;
            if binding.app != *app
                || !matches!(
                    binding.role,
                    Role::Local | Role::LocalSession | Role::User | Role::UserSession
                )
            {
                continue;
            }
            let id = self.automatic_volume_id(app, binding.session.as_deref(), binding.role)?;
            if suffix != id.to_string().as_bytes()
                || binding.role.is_session() != binding.session.is_some()
            {
                return Err(Error::corrupt("private automatic volume identity differs"));
            }
            if snapshot.has(format!("deleted-volume/{id}").as_bytes())?
                || snapshot.has(format!("{ERASE}{id}").as_bytes())?
            {
                continue;
            }
            let record = snapshot
                .get(format!("volume/{id}").as_bytes())?
                .ok_or(Error::corrupt("private automatic volume has no descriptor"))?;
            let volume: Volume = decode(&record)?;
            if volume.id != id
                || volume.owner != self.account.id
                || volume.personal
                || volume.share_key_with_service
            {
                return Err(Error::corrupt("private automatic volume ownership differs"));
            }
            if binding.role.is_local() {
                if inventory.local.len() == MAX_LOCAL_TARGETS {
                    self.finish_binding_batch(snapshot, app, purpose, inventory, &mut targets)?;
                }
                inventory.local.insert(id);
            } else {
                inventory.impact.user_volumes = true;
            }
        }
        self.finish_binding_batch(snapshot, app, purpose, inventory, &mut targets)?;
        inventory.local = targets;
        Ok(())
    }

    fn finish_binding_batch(
        &self,
        snapshot: &kvdb::ReadTxn<'_, RedbStorage>,
        app: &AppIdentity,
        purpose: RemovalInventory,
        inventory: &mut Inventory,
        targets: &mut BTreeSet<Id>,
    ) -> Result<()> {
        self.inspect_app_sessions(snapshot, app, purpose, inventory)?;
        if purpose == RemovalInventory::Erasure {
            // Only eligible targets count toward the atomic transaction limit.
            if inventory.local.len() > MAX_LOCAL_TARGETS - targets.len() {
                return Err(Error::LimitExceeded);
            }
            targets.append(&mut inventory.local);
        } else {
            inventory.local.clear();
        }
        Ok(())
    }

    fn protect_session_bindings(
        &self,
        snapshot: &kvdb::ReadTxn<'_, RedbStorage>,
        app: &AppIdentity,
        session: &Session,
        mut user: Option<Id>,
        shared: bool,
        inventory: &mut Inventory,
    ) -> Result<()> {
        let deleted = |id| snapshot.has(format!("deleted-volume/{id}").as_bytes());
        let is_target = session.app == *app;
        // Existing explicit roots and User overrides can name a Local
        // volume. Preserve those references, even for other apps.
        inventory.impact.user_volumes |= inventory.local.remove(&session.root_volume);
        if inventory.local.is_empty() && (!is_target || inventory.impact.user_volumes) {
            return Ok(());
        }
        if is_target
            && !inventory.impact.user_volumes
            && session.root_volume != self.automatic_volume_id(app, None, Role::Root)?
            && !deleted(session.root_volume)?
            && let Some(record) =
                snapshot.get(format!("volume/{}", session.root_volume).as_bytes())?
        {
            let root: Volume = decode(&record)?;
            inventory.impact.user_volumes = root.personal && root.owner == self.account.id;
        }
        if shared
            && let Some(record) =
                snapshot.get(format!("automatic-session/{}", session.id).as_bytes())?
        {
            let scope: AppScope = decode(&record)?;
            match scope {
                AppScope::Session { id, user_volume, .. } if id == session.id => user = user_volume,
                _ => {
                    return Err(Error::corrupt("automatic session scope identity differs"));
                }
            }
        }
        let grants: Option<Vec<PersonalVolumeSelection>> = snapshot
            .get(format!("session-mounts/{}", session.id).as_bytes())?
            .map(|bytes| decode(&bytes))
            .transpose()?;
        for bound in user.into_iter().chain(grants.into_iter().flatten().map(|grant| grant.volume))
        {
            let protected = inventory.local.remove(&bound);
            inventory.impact.user_volumes |=
                protected || is_target && !inventory.impact.user_volumes && !deleted(bound)?;
        }
        Ok(())
    }

    fn has_pending_app_invitation(
        &self,
        snapshot: &kvdb::ReadTxn<'_, RedbStorage>,
        app: &AppIdentity,
    ) -> Result<bool> {
        // A loaded invitation is grouped under its app before the person joins.
        for row in snapshot.iter_from(b"invitation/") {
            let (key, value) = row?;
            let Some(id) = key.strip_prefix(b"invitation/") else { break };
            let pending: PendingInvitation = decode(&value)?;
            if let Some(invitation) = pending.invitation
                && &invitation.session.app == app
                && invitation.session.id.as_bytes() == id
                && !snapshot.has(format!("session/{}", invitation.session.id).as_bytes())?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A confirmation reply leaves list membership and local data intact.
    pub fn remove_app(
        &self,
        app: &AppIdentity,
        confirmed: bool,
        delete_local_data: bool,
    ) -> Result<AppRemovalResult> {
        app.validate().map_err(Error::InvalidInput)?;
        if delete_local_data && !confirmed {
            return Err(Error::InvalidInput("deleting local app data requires confirmation"));
        }
        let consent = self.lock_account_authority()?;
        let profile = self.lock_profile()?;
        let purpose = if delete_local_data {
            RemovalInventory::Erasure
        } else if confirmed {
            RemovalInventory::Membership
        } else {
            RemovalInventory::Impact
        };
        let inventory = self.app_removal_inventory(app, purpose)?;
        if !inventory.listed && inventory.removed {
            drop(profile);
            drop(consent);
            if delete_local_data {
                self.finish_app_data_removal(app)?;
            }
            return Ok(AppRemovalResult::Removed {
                erased_volumes: Vec::new(),
                cleanup_error: None,
            });
        }
        if !inventory.listed {
            return Err(Error::NotFound("app"));
        }
        if !confirmed && (inventory.impact.user_volumes || inventory.impact.local_data) {
            return Ok(AppRemovalResult::Confirmation(inventory.impact));
        }
        let erasing = delete_local_data && !inventory.local.is_empty();
        if erasing {
            profile_format::enable_app_local_erasure(self)?;
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if erasing {
            self.stage_app_erasure(&mut tx, &inventory.local)?;
        }
        let changed = self.stage_app_removed(&mut tx, app, true)?;
        tx.commit()?;
        drop(profile);
        drop(consent);
        self.committed_account_record(changed);
        self.changes.observed_changed(None);
        let outcome = if erasing {
            let cleanup_error = self.finish_app_data_removal(app).err().map(|error|
                format!("App removed. Local cleanup will resume when Playbit next opens this profile: {error}"));
            AppRemovalResult::Removed {
                erased_volumes: inventory.local.into_iter().collect(),
                cleanup_error,
            }
        } else {
            AppRemovalResult::Removed { erased_volumes: Vec::new(), cleanup_error: None }
        };
        Ok(outcome)
    }

    fn stage_app_erasure(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        volumes: &BTreeSet<Id>,
    ) -> Result<()> {
        self.remove_erased_volume_drafts(tx, volumes)?;
        for id in volumes {
            let binding = self.automatic_binding(*id)?.ok_or(Error::NotFound("local app data"))?;
            tx.put(format!("{ERASE}{id}").as_bytes(), &encode(&binding)?)?;
            tx.put(erased_local_volume(*id).as_bytes(), &[])?;
            // The original binding and initialization receipt fence re-creation
            // until cleanup completes. Removing credentials denies fresh opens.
            tx.del(format!("volume/{id}").as_bytes())?;
            tx.del(format!("credentials/{id}").as_bytes())?;
        }
        Ok(())
    }

    pub(crate) fn has_pending_app_data_removal(&self) -> Result<bool> {
        let read = self.metadata.begin_read()?;
        let first = read.iter_from(ERASE.as_bytes()).next().transpose()?;
        Ok(first.is_some_and(|(key, _)| key.starts_with(ERASE.as_bytes())))
    }

    pub(super) fn app_data_erasure_pending(&self, volume: Id) -> Result<bool> {
        Ok(self.metadata.begin_read()?.has(format!("{ERASE}{volume}").as_bytes())?)
    }

    pub(super) fn check_app_data_erasure(&self, volume: Id, role: Role) -> Result<()> {
        if role.is_local() && self.app_data_erasure_pending(volume)? {
            return Err(Error::LocalContention("local app data removal is pending"));
        }
        Ok(())
    }

    pub(crate) fn finish_app_data_removal(&self, app: &AppIdentity) -> Result<()> {
        self.drain_app_data_removals(Some(app), || false)
    }

    pub(crate) fn resume_app_data_removals(&self, stopping: impl Fn() -> bool) -> Result<()> {
        self.drain_app_data_removals(None, stopping)
    }

    // Keep one intent and one cursor in memory. Release each snapshot before
    // reclaiming storage so a long erase does not pin unrelated metadata pages.
    fn drain_app_data_removals(
        &self,
        app: Option<&AppIdentity>,
        stopping: impl Fn() -> bool,
    ) -> Result<()> {
        let mut cursor = ERASE.as_bytes().to_vec();
        let mut first_error = None;
        while !stopping() {
            let bytes = {
                let read = self.metadata.begin_read()?;
                let Some((key, bytes)) = read.iter_from(&cursor).next().transpose()? else { break };
                if !key.starts_with(ERASE.as_bytes()) {
                    break;
                }
                cursor = key.to_vec();
                bytes.to_vec()
            };
            if let Err(error) = self.finish_app_data_target(&cursor, &bytes, app) {
                if app.is_some() {
                    first_error.get_or_insert(error);
                } else {
                    crate::platform::failure::log(
                        &self.path,
                        "resume local app data removal",
                        &error.to_string(),
                    );
                }
            }
            // Success deletes the row. Failure leaves it for the next open or
            // explicit retry without starving the remaining volumes in this pass.
            cursor.push(0);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish_app_data_target(
        &self,
        key: &[u8],
        bytes: &[u8],
        app: Option<&AppIdentity>,
    ) -> Result<()> {
        let binding: automatic::AutomaticBinding = decode(bytes)?;
        if app.is_some_and(|target| target != &binding.app) {
            return Ok(());
        }
        let id =
            self.automatic_volume_id(&binding.app, binding.session.as_deref(), binding.role)?;
        if !binding.role.is_local()
            || binding.role.is_session() != binding.session.is_some()
            || key != format!("{ERASE}{id}").as_bytes()
        {
            return Err(Error::corrupt("app cleanup identity differs"));
        }
        let gate = self.volume_operation(id)?;
        let _guard = if app.is_some() {
            gate.lock()?
        } else {
            gate.try_lock().map_err(|error| match error {
                operation_gate::TryLockError::WouldBlock => {
                    Error::LocalContention("local app data is still in use")
                }
                operation_gate::TryLockError::Poisoned => {
                    Error::Internal("volume operation lock is poisoned")
                }
            })?
        };
        let profile = self.lock_profile()?;
        let read = self.metadata.begin_read()?;
        let Some(current) = read.get(key)? else { return Ok(()) };
        if decode::<automatic::AutomaticBinding>(&current)? != binding
            || !read.has(erased_local_volume(id).as_bytes())?
            || self.automatic_binding(id)?.as_ref() != Some(&binding)
            || !read.has(initialization::receipt_key(id).as_bytes())?
            || read.has(format!("volume/{id}").as_bytes())?
            || read.has(format!("credentials/{id}").as_bytes())?
        {
            return Err(Error::corrupt("app cleanup conflicts with retained volume"));
        }
        drop(current);
        drop(read);
        drop(profile);
        // This volume gate and the committed marker reserve the target.
        // Other apps can mutate their catalogs while its files are erased.
        self.retire_app_volume_backings(id)?;
        self.remove_local_blob_pins(id)?;
        self.remove_local_volume_metadata(id)?;
        let parent = self.path.join(crate::paths::VOLUMES);
        match fs::remove_dir_all(parent.join(id.to_string())) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(io(e)),
        }
        initialization::sync_directory(&parent)?;
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(format!("automatic-volume/{id}").as_bytes())?;
        tx.del(initialization::receipt_key(id).as_bytes())?;
        tx.del(&volume_name_marker(id))?;
        tx.del(key)?;
        tx.commit()?;
        self.changes.observed_changed(Some(id));
        Ok(())
    }

    /// Caller holds the volume gate and has reclaimed every content pin.
    /// A receipt must survive as long as any acknowledged version might still
    /// need discovery. Entry preferences would otherwise reopen on fresh seeds.
    fn remove_local_volume_metadata(&self, volume: Id) -> Result<()> {
        for prefix in [
            format!("entry-durable/{volume}/"),
            format!("settings/entry-permissions/{}/{volume}/", self.account.id),
        ] {
            loop {
                let _profile = self.lock_profile()?;
                let mut tx = self.metadata.begin_write(now_unix_secs())?;
                let mut keys = Vec::new();
                for row in tx.iter_from(prefix.as_bytes()).take(64) {
                    let (key, _) = row?;
                    if !key.starts_with(prefix.as_bytes()) {
                        break;
                    }
                    keys.push(key.to_vec());
                }
                if keys.is_empty() {
                    break;
                }
                for key in keys {
                    tx.del(&key)?;
                }
                tx.commit()?;
            }
        }
        Ok(())
    }

    fn retire_app_volume_backings(&self, volume: Id) -> Result<()> {
        let mut cache = self
            .open
            .lock()
            .map_err(|_| Error::Internal("open database cache lock is poisoned"))?;
        let target = |key: &DatabaseKey| matches!(key, DatabaseKey::Volume(id, _) if *id == volume);
        if cache.transitioning.iter().any(target)
            || cache
                .by_id
                .iter()
                .any(|(key, entry)| target(key) && Arc::strong_count(&entry.database) != 1)
        {
            return Err(Error::LocalContention("local app data is still in use"));
        }
        let retired: Vec<_> = cache.by_id.extract_if(|key, _| target(key)).collect();
        drop(cache);
        drop(retired);
        Ok(())
    }
}
