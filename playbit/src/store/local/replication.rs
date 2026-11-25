//! Ordinary replication addresses reserved controls or one stable named backing.
use super::*;
use crate::store::{DatabaseId, object_database, volume_index};
use std::cell::RefCell;

pub struct ReplicationDatabasePage {
    pub databases: Vec<DatabaseId>,
    pub next: Option<crate::store::EntryId>,
}

#[cfg(feature = "connected-engine")]
struct CapturedControls<'local, 'remote> {
    local_defaults: Acl,
    remote_defaults: Acl,
    local_index: volume_index::IndexSnapshot<'local, RedbStorage>,
    remote_index: volume_index::IndexSnapshot<'remote, RedbStorage>,
}

fn observe_source(
    policy: &DatabasePolicy,
    delta: &kvdb::sync::Delta<'_>,
    entries: &RefCell<BTreeSet<crate::store::EntryId>>,
    objects: &RefCell<BTreeSet<Id>>,
    writer: Option<(Id, &volume_index::IndexSnapshot<'_, RedbStorage>)>,
) -> Result<()> {
    if policy.database == DatabaseId::Index && volume_index::is_index_key(delta.key) {
        if let Some((writer, before)) = writer {
            before.authorize_source(
                policy.volume.volume(),
                policy.volume.default_permissions(),
                writer,
                delta,
            )?;
        }
        let id = volume_index::validate_source(delta)?;
        let mut touched = entries.borrow_mut();
        if touched.len() == volume_index::MAX_REPAIR_IDS && !touched.contains(&id) {
            return Err(Error::LimitExceeded);
        }
        touched.insert(id);
    } else if let Some(object) = policy.validate_source(delta, writer.map(|(id, _)| id))? {
        let mut touched = objects.borrow_mut();
        if touched.len() == 64 && !touched.contains(&object) {
            return Err(Error::LimitExceeded);
        }
        touched.insert(object);
    }
    Ok(())
}

#[derive(Default)]
struct ReceivedSources {
    entries: RefCell<BTreeSet<crate::store::EntryId>>,
    objects: RefCell<BTreeSet<Id>>,
    rejected: RefCell<Option<Error>>,
    changes_policy: std::cell::Cell<bool>,
    acl_tie: std::cell::Cell<bool>,
}

impl ReceivedSources {
    fn resolve(
        &self,
        policy: &DatabasePolicy,
        owner_policy: bool,
        writer: Option<(Id, &volume_index::IndexSnapshot<'_, RedbStorage>)>,
        delta: &kvdb::sync::Delta<'_>,
    ) -> kvdb::sync::Resolution {
        use kvdb::{Resolver, sync::EntState};
        if let Err(error) = observe_source(policy, delta, &self.entries, &self.objects, writer) {
            *self.rejected.borrow_mut() = Some(error);
            return kvdb::sync::Resolution::KeepLocal;
        }
        if owner_policy
            && policy.database == DatabaseId::Index
            && delta.key == DEFAULT_PERMISSIONS_KEY
        {
            self.changes_policy.set(true);
            if let (
                EntState::Present { clock: a, value: left },
                EntState::Present { clock: b, value: right },
            ) = (delta.local, delta.remote)
            {
                self.acl_tie.set(a == b && left != right);
            }
        }
        kvdb::Lww.resolve(delta)
    }
}

fn repair_delta_indexes<'db, R: kvdb::Resolver>(
    receiver: kvdb::DeltaReceiver<'db, RedbStorage, R>,
    database: DatabaseId,
    namespace: Option<(crate::store::EntryId, &[u8])>,
    bytes: &[u8],
    ids: &[crate::store::EntryId],
) -> Result<kvdb::DeltaReceiver<'db, RedbStorage, R>> {
    if let Some((entry, evidence)) = namespace {
        receiver.qualify_local_indexes(|local| -> Result<bool> {
            let changed = volume_index::namespace::live::apply(local, entry, bytes, evidence)?;
            volume_index::repair(local, ids)?;
            volume_index::namespace::source::cut::project(local)?.write_current(local)?;
            Ok(changed)
        })
    } else {
        receiver.repair_local_indexes(|local| -> Result<()> {
            if database == DatabaseId::Index {
                volume_index::repair(local, ids)?;
                volume_index::namespace::source::cut::project(local)?.write_current(local)?;
            }
            Ok(())
        })
    }
}

fn validate_pending(
    tx: &kvdb::WriteTxn<'_, RedbStorage>,
    database: DatabaseId,
    objects: &BTreeSet<Id>,
) -> Result<()> {
    match database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_) => object_database::validate_pending(tx, objects),
        DatabaseId::Revocations => Ok(()),
    }
}

fn append_relayer(policy: &DatabasePolicy, principal: DataActor) -> Result<bool> {
    match policy.authorize(principal, Permission::Write) {
        Ok(()) => Ok(false),
        Err(Error::AccessDenied(_)) => {
            policy.authorize(principal, Permission::Append)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn validate_replica(tx: &kvdb::WriteTxn<'_, RedbStorage>, policy: &DatabasePolicy) -> Result<()> {
    match policy.database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_)
            if matches!(
                policy.entry.as_ref().map(|entry| entry.kind),
                Some(crate::store::EntryType::ObjectDb)
            ) =>
        {
            object_database::validate_all(tx)
        }
        _ => Ok(()),
    }
}

fn check_entry_delivery(
    access: &Access,
    entry: crate::store::EntryId,
    database: DatabaseId,
    readable: bool,
    bytes: &[u8],
) -> Result<()> {
    if matches!(database, DatabaseId::Entry(id) if id != entry) {
        return Err(Error::AccessDenied("backing is outside selected entry"));
    }
    let policy_keys = crate::store::entry_policy_keys(access.build_id.as_deref())?;
    let account = access.account.to_string();
    let mut rejected = false;
    kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
        let admitted = match database {
            DatabaseId::Entry(id) => readable && id == entry,
            DatabaseId::Revocations => key == account.as_bytes(),
            DatabaseId::Index => {
                (volume_index::source_entry_id(key).is_ok_and(|id| id == entry)
                    && (readable || !volume_index::is_entry_history(key)))
                    || policy_keys.iter().any(|allowed| key == allowed)
            }
        };
        rejected |= !admitted;
    })?;
    if rejected {
        return Err(Error::AccessDenied("delivery is outside selected entry"));
    }
    Ok(())
}

impl LocalStore {
    /// Compare pinned local and authenticated AP controls under one admitted
    /// binding. Release local gates before projection or network publication.
    #[cfg(feature = "connected-engine")]
    fn with_captured_controls<T>(
        &self,
        session: Option<&Session>,
        volume: Id,
        received: &Database,
        use_controls: impl FnOnce(&Volume, DataActor, CapturedControls<'_, '_>) -> Result<T>,
    ) -> Result<Option<T>> {
        let identity = self.volume(volume)?;
        if !identity.share_key_with_service
            || self.durable_revision(volume, DatabaseId::Index)?.is_none()
        {
            return Ok(None);
        }
        let access = match session {
            Some(session) if session.replica_volume() == volume => {
                Access::session_version(self.account.id, &session.id, &session.resolved_build_id)
            }
            None if identity.owner == self.account.id => Access::owner(self.account.id),
            _ => return Ok(None),
        };
        let operations = self.access_operations(&access, volume)?;
        let guards = operations.lock()?;
        if let Some(session) = session
            && self.session(&session.id)? != *session
        {
            // A candidate build may pull data before adoption, but it cannot
            // publish edits under the retained session's admission.
            return Ok(None);
        }
        let index = self.db(volume, DatabaseId::Index)?;
        let local_tx = index.begin_read()?;
        if local_tx.root_hash()?.is_none() {
            return Ok(None);
        }
        let remote_tx = received.begin_read()?;
        // Decode one bounded policy at a time and retain only its defaults.
        let defaults = |tx: &kvdb::ReadTxn<'_, RedbStorage>, local| -> Result<Acl> {
            let policy = MetadataPolicy::read_snapshot(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
            if !identity.same_identity(policy.volume()) {
                return Err(Error::AccessDenied("recovery volume identity mismatch"));
            }
            if local {
                self.admit_access(&access, &policy)?;
            }
            if let Some(session) = session {
                policy.admit(self.account.id, session, volume)?;
            }
            Ok(policy.permissions)
        };
        let controls = CapturedControls {
            local_defaults: defaults(&local_tx, true)?,
            remote_defaults: defaults(&remote_tx, false)?,
            local_index: volume_index::IndexSnapshot::new(local_tx),
            remote_index: volume_index::IndexSnapshot::new(remote_tx),
        };
        let actor = self.data_actor(&access, &identity)?;
        drop(guards);
        drop(operations);
        use_controls(&identity, actor, controls).map(Some)
    }

    /// Identify the exact qualified deletion for this blocked backing while
    /// the local and hosted controls are pinned.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn retirement_for_capture(
        &self,
        session: Option<&Session>,
        volume: Id,
        received: &Database,
        entry: crate::store::EntryId,
    ) -> Result<Option<volume_index::Retirement>> {
        self.with_captured_controls(session, volume, received, |volume, actor, controls| {
            controls.local_index.retirement_for(
                entry,
                volume,
                actor,
                &controls.local_defaults,
                &controls.remote_index,
                &controls.remote_defaults,
            )
        })
        .map(Option::flatten)
    }

    /// Project both records into a scoped sync only when the recovery is ready
    /// to publish. A large causal context can exceed a live publication frame.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn project_retirement(
        &self,
        received: &Database,
        retirement: &volume_index::Retirement,
    ) -> Result<OutboundReplica> {
        let replica = empty_detached_replica(&self.path.join(crate::paths::STAGING), received)?;
        let mut tx = replica.database().begin_write(now_unix_secs())?;
        retirement.copy_into(&mut tx)?;
        tx.commit()?;
        self.require_account_authority()?;
        Ok(replica)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn restore_owner_service_read(
        &self,
        volume: Id,
        received: &Database,
        mut publish: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<bool> {
        use crate::store::catalog::PLAYBIT_REPLICA_PRINCIPAL;
        self.with_captured_controls(None, volume, received, |identity, _, controls| {
            let mut after = None;
            let mut scanned = 0;
            let mut published = false;
            loop {
                let page = controls.remote_index.source_headers(after, 64)?;
                scanned += page.entries.len();
                if scanned > volume_index::namespace::MAX_ENTRIES {
                    return Err(Error::LimitExceeded);
                }
                for entry in page
                    .entries
                    .into_iter()
                    .filter(|entry| !entry.deleted && entry.protocol.is_some())
                {
                    match controls.remote_index.authorize_staged_entry(
                        identity,
                        &controls.remote_defaults,
                        PLAYBIT_REPLICA_PRINCIPAL,
                        entry.id,
                        Permission::Read,
                    ) {
                        Ok(()) => continue,
                        Err(Error::AccessDenied(_)) => {}
                        Err(error) => return Err(error),
                    }
                    match controls.local_index.authorize(
                        identity,
                        &controls.local_defaults,
                        PLAYBIT_REPLICA_PRINCIPAL,
                        entry.id,
                        Permission::Read,
                    ) {
                        Ok(()) => {}
                        Err(Error::AccessDenied(_) | Error::NotFound(_)) => continue,
                        Err(error) => return Err(error),
                    }
                    if let Some(bytes) = controls.local_index.permission_restoration_delta(
                        identity.owner,
                        entry.id,
                        &controls.remote_index,
                    )? {
                        self.require_account_authority()?;
                        publish(&bytes)?;
                        published = true;
                    }
                }
                let Some(next) = page.next else { return Ok(published) };
                after = Some(next);
            }
        })
        .map(|published| published.unwrap_or(false))
    }

    /// A fresh selected AP cut may prove the deletion through an authority
    /// fence. This admission neither clears the fence nor exposes retained data.
    #[cfg(feature = "connected-engine")]
    pub(super) fn admit_retirement_writer(
        &self,
        access: &Access,
        entry: crate::store::EntryId,
        policy: &MetadataPolicy,
        index: &volume_index::IndexSnapshot<'_, RedbStorage>,
    ) -> Result<DataActor> {
        if let Some(id) = access.session_id()
            && access.build_id() != Some(self.session(id)?.resolved_build_id.as_str())
        {
            return Err(Error::AccessDenied("entry retirement build changed"));
        }
        let actor = self.data_actor(access, policy.volume())?;
        if self.admit_access(access, policy)?.is_some_and(|grant| !grant.write)
            || !index.source_granted(
                policy.volume(),
                policy.default_permissions(),
                actor,
                entry,
                Permission::Write,
            )?
        {
            return Err(Error::AccessDenied("entry retirement requires current Write"));
        }
        Ok(actor)
    }

    /// Check the qualified local source before opening a live selected scope.
    /// A terminal receipt still needs the current binding and the writer's grant.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn entry_retirement_events(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        session: Option<&Session>,
    ) -> Result<Option<[crate::store::EntryId; 2]>> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        let Some((events, _)) = snapshot.selected_retirement(address.entry)? else {
            return Ok(None);
        };
        let policy = MetadataPolicy::read(&index)?;
        if let Some(session) = session
            && self.session(&session.id)? != *session
        {
            return Err(Error::AccessDenied("entry retirement session binding changed"));
        }
        self.admit_retirement_writer(access, address.entry, &policy, &snapshot)?;
        Ok(Some(events))
    }

    /// Both policies must admit the same birth before replaying its original
    /// deletion and the selected entry's immutable predecessor revisions.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn entry_retirement_snapshot(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        received: &Database,
    ) -> Result<OutboundReplica> {
        let operations = self.access_operations(access, address.volume)?;
        let guards = operations.lock()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let local = MetadataPolicy::read(&index)?;
        let remote = MetadataPolicy::read(received)?;
        if !local.volume().same_identity(remote.volume()) {
            return Err(Error::AccessDenied("entry retirement binding changed"));
        }
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        let actor = self.admit_retirement_writer(access, address.entry, &local, &snapshot)?;
        let captured = volume_index::IndexSnapshot::new(received.begin_read()?);
        let (events, retirement) = snapshot
            .selected_retirement(address.entry)?
            .ok_or(Error::NotFound("local entry retirement"))?;
        if captured.source_birth_event(address.entry)? != events[0]
            || !captured.source_granted(
                remote.volume(),
                remote.default_permissions(),
                actor,
                address.entry,
                Permission::Write,
            )?
        {
            return Err(Error::AccessDenied("entry retirement birth or authority changed"));
        }
        let history =
            super::scoped::ScopedSnapshot::for_publication(&index, &local, address.entry, actor)?
                .only_revision_history();
        drop(guards);
        let source = history.project(&self.path.join(crate::paths::STAGING))?;
        let mut tx = source.database().begin_write(now_unix_secs())?;
        retirement.copy_into(&mut tx)?;
        tx.commit()?;
        self.require_account_authority()?;
        Ok(source)
    }

    /// Pin only this entry's publishable source facts. No catalog Read is
    /// required or granted; the AP independently admits every supplied fact.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn entry_publication_snapshot(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
    ) -> Result<Option<OutboundReplica>> {
        let operations = self.access_operations(access, address.volume)?;
        let guards = operations.lock()?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let policy = MetadataPolicy::read(&index)?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write) {
            return Ok(None);
        }
        let snapshot = match super::scoped::ScopedSnapshot::for_publication(
            &index,
            &policy,
            address.entry,
            self.data_actor(access, policy.volume())?,
        ) {
            Ok(snapshot) => snapshot,
            Err(Error::AccessDenied(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        drop(guards);
        snapshot.project(&self.path.join(crate::paths::STAGING)).map(Some)
    }

    /// Advisory outbox selection. A remote receiver still admits every source
    /// under its current policy; denied local appends remain pending.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn replica_upload_allowed(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
    ) -> Result<bool> {
        let operations = self.access_operations(access, volume)?;
        let guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        // A catalog reader may have entry Write without ever receiving its
        // backing. Only locally ready data can participate in an upload.
        if let DatabaseId::Entry(entry) = database
            && !volume_index::IndexSnapshot::new(index.begin_read()?).backing_ready(entry)?
        {
            return Ok(false);
        }
        let policy = DatabasePolicy::read(&index, database)?;
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        if self.admit_access(access, &policy.volume)?.is_some_and(|grant| !grant.write) {
            return Ok(false);
        }
        if database == DatabaseId::Revocations {
            return Ok(policy.volume.volume().owner == access.account);
        }
        let principal = self.data_actor(access, policy.volume.volume())?;
        for permission in [Permission::Write, Permission::Append] {
            match policy.authorize(principal, permission) {
                Ok(()) => return Ok(true),
                Err(Error::AccessDenied(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if database != DatabaseId::Index {
            return Ok(false);
        }
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        drop(guards);
        let mut after = None;
        loop {
            let page = snapshot.headers(after, 64)?;
            for entry in page.entries {
                // Qualified retirements still need to reach AP after the last
                // live writable entry is gone. Append only applies to live entries.
                let granted = |permission| {
                    snapshot.source_granted(
                        policy.volume.volume(),
                        policy.volume.default_permissions(),
                        principal,
                        entry.id,
                        permission,
                    )
                };
                if granted(Permission::Write)? || (!entry.deleted && granted(Permission::Append)?) {
                    return Ok(true);
                }
            }
            let Some(next) = page.next else { return Ok(false) };
            after = Some(next);
        }
    }

    pub fn with_replica<T>(
        &self,
        volume: Id,
        database: DatabaseId,
        f: impl FnOnce(&Database) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        if matches!(database, DatabaseId::Entry(_)) {
            DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?
                .ordinary_protocol()?;
        }
        let key = self.local_database_key(volume)?;
        let db = self.replica_db(volume, database, Some(key))?;
        f(&db)
    }

    pub fn merge_received_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        self.receive_delta(access, volume, database, None, bytes, None)
    }

    /// A trusted selected-entry stream cannot publish catalog or unrelated facts.
    pub fn merge_received_entry_delta(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        self.receive_delta(access, address.volume, database, Some(address.entry), bytes, None)
    }

    /// Authenticated AP acceptance accompanies one exact selected index delta.
    /// Source, retained payload, observation and current control commit together.
    pub fn merge_received_namespace_delta(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        bytes: &[u8],
        evidence: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        volume_index::namespace::live::validate_evidence(evidence)?;
        self.receive_delta(
            access,
            address.volume,
            DatabaseId::Index,
            Some(address.entry),
            bytes,
            Some(evidence),
        )
    }

    fn receive_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        entry: Option<crate::store::EntryId>,
        bytes: &[u8],
        evidence: Option<&[u8]>,
    ) -> Result<kvdb::SyncOutcome> {
        self.require_replication_supported(volume)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        // An ACL update can follow the account's offer on a separate stream.
        // Admit the authenticated Index feed while ordinary reads/edits wait
        // for its current policy. A directory withdrawal still fences it.
        let identity = policy.volume.volume();
        let grant = if database == DatabaseId::Index
            && access.session_id().is_none()
            && access.account == self.account.id
            && identity.personal
            && identity.owner != self.account.id
            && self.has_direct_volume_grant(volume)?
        {
            self.require_account_authority()?;
            self.check_participant(volume, access.account)?;
            None
        } else {
            self.admit_access(access, &policy.volume)?
        };
        let actor = self.data_actor(access, policy.volume.volume())?;
        policy.ordinary_protocol()?;
        if let Some(entry) = entry {
            let readable = DatabasePolicy::read_source(&index, DatabaseId::Entry(entry))?
                .metadata_readable(actor)?;
            if !readable && grant.is_some_and(|grant| !grant.write) {
                return Err(Error::AccessDenied("personal volume is read-only"));
            }
            check_entry_delivery(access, entry, database, readable, bytes)?;
        } else {
            policy.authorize_reader(ReplicaReader::SessionParticipant(actor))?;
        }
        if let DatabaseId::Entry(id) = database
            && !volume_index::IndexSnapshot::new(index.begin_read()?).backing_ready(id)?
        {
            // A delta can arrive after the bounded initial snapshot retries end.
            // It cannot initialize an absent backing or merge into a rejected one.
            return Err(Error::NotFound(Error::PENDING_BACKING));
        }
        self.apply_received_delta(volume, database, &policy, bytes, entry.zip(evidence), None)
    }

    /// A private stream joins the owner and one specifically named human.
    /// Both endpoints recheck direct membership beside the actual database policy.
    pub(super) fn admit_private_invitation_peer(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        peer: Id,
    ) -> Result<()> {
        use crate::store::object_contract::{Principal, Subject};
        let volume = policy.volume();
        let local = self.account.id;
        if peer == local
            || (access.account != local && access.account != peer)
            || !volume.personal
            || volume.share_key_with_service
        {
            return Err(Error::AccessDenied("private invitation peer identity"));
        }
        self.require_account_authority()?;
        let grantee = if local == volume.owner {
            peer
        } else if peer == volume.owner && self.has_direct_volume_grant(volume.id)? {
            local
        } else {
            return Err(Error::AccessDenied("private invitation peer binding"));
        };
        if !policy
            .default_permissions()
            .grants
            .iter()
            .any(|grant| grant.subject == Subject::Principal(Principal::Human(grantee)))
        {
            return Err(Error::AccessDenied("private volume direct grant required"));
        }
        self.check_participant(volume.id, local)?;
        self.check_participant(volume.id, peer)
    }

    pub fn private_invitation_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        recipient: Id,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        self.admit_private_invitation_peer(&access, &policy.volume, recipient)?;
        self.require_current_direct_acl(&access, &policy.volume)?;
        let entries =
            crate::store::authorize_publication_delta(&index, &policy, access.account, bytes)?;
        if entries.is_empty() {
            self.check_authority(&access, volume, None)?;
        }
        for entry in entries {
            self.check_authority(&access, volume, Some(entry))?;
        }
        crate::store::project_replica_delta(&policy, ReplicaReader::Participant(recipient), bytes)
    }

    pub fn apply_private_invitation_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        writer: Id,
        bytes: &[u8],
    ) -> Result<bool> {
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        self.admit_private_invitation_peer(&access, &policy.volume, writer)?;
        if database != DatabaseId::Index {
            self.require_current_direct_acl(&access, &policy.volume)?;
        }
        self.check_authority(
            &access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        policy.authorize_reader(ReplicaReader::Participant(access.account))?;
        if let DatabaseId::Entry(entry) = database
            && !volume_index::IndexSnapshot::new(index.begin_read()?).backing_ready(entry)?
        {
            return Err(Error::NotFound(Error::PENDING_BACKING));
        }
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        // The owner forwards accepted records with their original object editor
        // and clocks. The grantee may originate only its own admitted writes.
        let writer = (writer != policy.volume.volume().owner).then_some((writer, &snapshot));
        Ok(self.apply_received_delta(volume, database, &policy, bytes, None, writer)?.applied > 0)
    }

    fn apply_received_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        policy: &DatabasePolicy,
        bytes: &[u8],
        namespace: Option<(crate::store::EntryId, &[u8])>,
        private_writer: Option<(Id, &volume_index::IndexSnapshot<'_, RedbStorage>)>,
    ) -> Result<kvdb::SyncOutcome> {
        let sources = ReceivedSources::default();
        let owner_policy =
            self.account.id == policy.volume.volume().owner && policy.volume.volume().personal;
        let resolver = |delta: &kvdb::sync::Delta<'_>| {
            sources.resolve(policy, owner_policy, private_writer, delta)
        };
        let name_changed = self.prepare_received_volume_name(volume, database, bytes)?;
        let backing = self.db(volume, database)?;
        let receiver = if private_writer.is_some() {
            kvdb::DeltaReceiver::new_with_delta
        } else {
            kvdb::DeltaReceiver::new
        };
        let mut receiver =
            receiver(&backing, resolver, now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        receiver.process(bytes)?;
        if let Some(error) = sources.rejected.borrow_mut().take() {
            return Err(error);
        }
        if database == DatabaseId::Index
            || matches!(
                policy.entry.as_ref().map(|entry| entry.kind),
                Some(crate::store::EntryType::ObjectDb)
            )
        {
            validate_pending(receiver.pending_transaction()?, database, &sources.objects.borrow())?;
        }
        let ids: Vec<_> = sources.entries.borrow().iter().copied().collect();
        if let Some((writer, _)) = private_writer {
            for entry in &ids {
                self.check_authority(&Access::owner(writer), volume, Some(*entry))?;
                self.check_authority(&Access::owner(self.account.id), volume, Some(*entry))?;
            }
        }
        let receiver = repair_delta_indexes(receiver, database, namespace, bytes, &ids)?;
        let outcome =
            self.finish_received_delta(volume, database, receiver, private_writer.is_some())?;
        if name_changed {
            self.project_volume_name(volume)?;
        }
        if sources.acl_tie.get() {
            self.restate_volume_acl(volume, self.index(volume)?.default_permissions())?;
        }
        if owner_policy
            && (sources.acl_tie.get()
                || outcome.committed
                    && (sources.changes_policy.get() || database == DatabaseId::Revocations))
        {
            self.notify_volume_invitation_policy(policy.volume.volume());
        }
        Ok(outcome)
    }

    fn finish_received_delta<R: kvdb::Resolver>(
        &self,
        volume: Id,
        database: DatabaseId,
        receiver: kvdb::DeltaReceiver<'_, RedbStorage, R>,
        relay: bool,
    ) -> Result<kvdb::SyncOutcome> {
        if relay {
            let (outcome, delta) = receiver.finish_with_delta()?;
            if let Some(delta) = delta {
                self.changes.publish(volume, database, delta);
            }
            return Ok(outcome);
        }
        let outcome = receiver.finish()?;
        if outcome.committed {
            self.changes.observed_database_changed(volume, database);
        }
        Ok(outcome)
    }

    fn prepare_received_volume_name(
        &self,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<bool> {
        let mut name_changed = false;
        if database == DatabaseId::Index {
            kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
                name_changed |= key == VOLUME_KEY
            })?;
            if name_changed {
                self.prepare_volume_name(volume)?;
            }
        }
        Ok(name_changed)
    }

    pub fn outbound_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        crate::store::project_replica_delta(&policy, reader, bytes)
    }

    /// Local writer publication, as opposed to a trusted replica projection.
    /// The session and every current entry fence are checked under the same cut.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn publication_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        if let DatabaseId::Entry(entry) = database
            && volume_index::IndexSnapshot::new(index.begin_read()?).backing_quarantined(entry)?
        {
            return Err(Error::NotFound(Error::PENDING_BACKING));
        }
        let policy = DatabasePolicy::read(&index, database)?;
        if self.admit_access(access, &policy.volume)?.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("read-only personal selection"));
        }
        let actor = self.data_actor(access, policy.volume.volume())?;
        let entries = crate::store::authorize_publication_delta(&index, &policy, actor, bytes)?;
        if entries.is_empty() {
            self.check_authority(access, volume, None)?;
        }
        for entry in entries {
            self.check_authority(access, volume, Some(entry))?;
        }
        match crate::store::project_replica_delta(&policy, ReplicaReader::Service, bytes) {
            Ok(projected) => Ok(Some(projected)),
            // The writer is authorized above. Keep their service-excluded change
            // local while allowing other entries to continue publishing.
            Err(Error::AccessDenied("entry permission")) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Scan at most 64 source entry IDs. Reserved control databases occur only
    /// on the first page. Sparse pages retain a cursor even without live entries.
    pub fn replication_database_page(
        &self,
        volume: Id,
        after: Option<crate::store::EntryId>,
    ) -> Result<ReplicationDatabasePage> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let mut databases = if after.is_none() {
            vec![DatabaseId::Index, DatabaseId::Revocations]
        } else {
            Vec::new()
        };
        if index.begin_read()?.root_hash()?.is_none() {
            return Ok(ReplicationDatabasePage { databases, next: None });
        }
        MetadataPolicy::read(&index)?;
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        let page = snapshot.headers(after, 64)?;
        for entry in page.entries {
            if !entry.deleted && entry.protocol == Some(volume_index::DatabaseProtocol::Kvdb) {
                databases.push(DatabaseId::Entry(entry.id));
            }
        }
        Ok(ReplicationDatabasePage { databases, next: page.next })
    }

    pub fn with_authorized_replica<T>(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        permission: Option<Permission>,
        f: impl FnOnce(&Database, &DatabasePolicy) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        let grant = self.admit_access(access, &policy.volume)?;
        if permission.is_some_and(|p| p != Permission::Read) && grant.is_some_and(|g| !g.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        if let Some(permission) = permission {
            policy.authorize(self.data_actor(access, policy.volume.volume())?, permission)?;
        } else if access.session_id.is_none()
            && access.invitation_peer.is_none()
            && access.account != policy.volume.volume.owner
        {
            return Err(Error::AccessDenied("replica owner or session admission required"));
        }
        policy.ordinary_protocol()?;
        f(&*self.db(volume, database)?, &policy)
    }

    pub fn with_outbound_replica<T>(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
        f: impl FnOnce(&Database) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        let backing = self.db(volume, database)?;
        let snapshot = OutboundSnapshot::new(&backing)?;
        drop(guards);
        f(snapshot.project(&self.path.join(crate::paths::STAGING), &policy, reader)?.database())
    }

    pub fn projection_root(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
    ) -> Result<ReplicaRoot> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        projection_root(&*self.db(volume, database)?, &policy, reader)
    }
}

impl LocalStore {
    /// The trusted transport derives a scoped key without creating a backing.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn replica_hash_key(&self, volume: Id, database: DatabaseId) -> Result<[u8; 32]> {
        Ok(database.hash_key(&self.credentials(volume)?.database_key, volume))
    }

    /// A peer is an authenticated relayer. Current Write permits forwarding
    /// accepted data; an Append-only relayer may introduce only its own births.
    /// Editor stamps in forwarded records are not authenticated authorship.
    fn authorize_replica_merge(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        policy: &DatabasePolicy,
    ) -> Result<()> {
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        let grant = self.admit_access(access, &policy.volume)?;
        if grant.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        policy.ordinary_protocol()?;
        Ok(())
    }

    pub fn merge_authorized_replica(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        source: &Database,
    ) -> Result<()> {
        use kvdb::sync::{Lww, Resolution, SyncResolver};
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        self.authorize_replica_merge(access, volume, database, &policy)?;
        let actor = self.data_actor(access, policy.volume.volume())?;
        let append = append_relayer(&policy, actor)?;
        let before = volume_index::IndexSnapshot::new(index.begin_read()?);
        // Validate every policy write and every authenticated grantee's own records.
        let grantee =
            access.invitation_peer.is_some() && access.account != policy.volume.volume.owner;
        let writer =
            (grantee || append || !matches!(database, DatabaseId::Entry(_))).then_some(actor);
        let rejected = RefCell::new(None);
        let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
            let admitted = if database == DatabaseId::Index && volume_index::is_index_key(delta.key)
            {
                volume_index::with_sync_delta(delta, |source| {
                    before.authorize_source(
                        policy.volume.volume(),
                        policy.volume.default_permissions(),
                        actor,
                        source,
                    )
                })
            } else {
                policy.validate_sync_source_as(delta, writer).map(|_| ())
            };
            if let Err(error) = admitted {
                *rejected.borrow_mut() = Some(error);
                return Ok(Resolution::KeepLocal);
            }
            Lww.resolve(delta)
        };
        let target = self.db(volume, database)?;
        let validate = |tx: &kvdb::WriteTxn<'_, RedbStorage>| {
            rejected.borrow_mut().take().map_or(Ok(()), Err)?;
            validate_replica(tx, &policy)
        };
        let scratch = self.path.join(crate::paths::STAGING);
        let relay_private =
            access.invitation_peer.is_some() && self.account.id == policy.volume.volume.owner;
        let delta = merge_sender_changes(
            &scratch,
            kvdb::sync::SyncSender::new(source)?,
            &target,
            resolver,
            validate,
            |local| {
                if database == DatabaseId::Index {
                    volume_index::repair_all(local)?;
                    volume_index::namespace::source::cut::project(local)?.write_current(local)?;
                }
                Ok(())
            },
            relay_private,
        )?;
        if let Some(delta) = delta {
            self.changes.publish(volume, database, delta);
        }
        Ok(())
    }
}
