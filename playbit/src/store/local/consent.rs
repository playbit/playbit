use super::*;

impl LocalStore {
    /// Merge only the root metadata returned by an authenticated candidate-version
    /// transfer. Existing local read denial and session revocation still win.
    pub fn merge_candidate_root_metadata(&self, access: &Access, staged: &Database) -> Result<()> {
        let id =
            access.session_id.as_ref().ok_or(Error::InvalidInput("candidate session required"))?;
        let root = self.session(id)?.replica_volume();
        let operations = self.access_operations(access, root)?;
        let _guards = operations.lock()?;
        let session = self.session(id)?;
        let version =
            access.build_id.as_ref().ok_or(Error::InvalidInput("candidate version required"))?;
        if *version == session.resolved_build_id {
            return Err(Error::InvalidInput("candidate must differ from retained version"));
        }
        let current = self.db(session.replica_volume(), crate::store::DatabaseId::Index)?;
        if current.begin_read()?.root_hash()?.is_some() {
            // Do not let a stale incoming ACL undo a local denial that arrived
            // during the transfer. Provisioning alone may be stale locally.
            let policy = MetadataPolicy::read(&current)?;
            policy.authorize(self.data_actor(access, policy.volume())?, Permission::Read)?;
        }
        // Reconcile edits made since receive began into the detached result,
        // then validate the exact policy that will be installed while locked.
        merge_index_replica(
            &self.path.join(crate::paths::STAGING),
            &current,
            staged,
            kvdb::Lww,
            |tx| MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES),
        )?;
        let candidate = MetadataPolicy::read(staged)?;
        if !candidate.volume.same_identity(&self.volume(session.replica_volume())?) {
            return Err(Error::InvalidInput("candidate root binding"));
        }
        if session.shared_volume.is_some() {
            let mut selected = session.clone();
            selected.resolved_build_id = version.clone();
            if matches!(selected.selector, BuildSelector::Exact { .. }) {
                selected.selector = BuildSelector::Exact { build_id: version.clone() };
            }
            self.authorize_shared_candidate_policy(&selected, &candidate)?;
        } else {
            self.authorize(access, &candidate, Permission::Read)?;
        }
        let name_changed = candidate.volume.name != self.volume(session.replica_volume())?.name;
        if name_changed {
            self.prepare_volume_name(session.replica_volume())?;
        }
        merge_index_with_publication(
            &self.path.join(crate::paths::STAGING),
            staged,
            &current,
            kvdb::Lww,
            |tx| MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES),
            |local| {
                crate::store::volume_index::namespace::source::cut::project(local)?
                    .write_current(local)
            },
        )?;
        self.changes.observed_changed(Some(root));
        if name_changed {
            self.project_volume_name(session.replica_volume())?;
        }
        Ok(())
    }

    pub fn personal_volumes(&self) -> Result<Vec<Volume>> {
        let shared: BTreeSet<_> =
            self.sessions()?.into_iter().flat_map(|s| [s.replica_volume()]).collect();
        Ok(self
            .volumes()?
            .into_iter()
            .filter(|v| v.personal && v.owner == self.account.id && !shared.contains(&v.id))
            .collect())
    }

    /// None means the person has not decided; Some([]) explicitly declines access.
    pub fn personal_volume_grants(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<PersonalVolumeGrant>>> {
        Ok(self
            .personal_volume_selections(session_id)?
            .map(|selections| selections.into_iter().map(|selection| selection.grant()).collect()))
    }

    pub fn personal_volume_selections(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<PersonalVolumeSelection>>> {
        self.require_account_authority()?;
        validate_version(session_id)?;
        self.metadata
            .begin_read()?
            .get(format!("session-mounts/{session_id}").as_bytes())?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    pub fn select_personal_volumes(
        &self,
        session_id: &str,
        grants: Vec<PersonalVolumeGrant>,
    ) -> Result<()> {
        validate_version(session_id)?;
        let _consent = self
            .personal_consent
            .write()
            .map_err(|_| Error::Internal("personal consent lock is poisoned"))?;
        let _profile = self.lock_profile()?;
        self.require_account_authority()?;
        self.commit_personal_volume_grants(session_id, grants)
    }

    /// Add only the grants explicitly requested, merging against current consent.
    #[cfg(any(feature = "app-host", test))]
    pub(crate) fn add_personal_volumes(
        &self,
        session_id: &str,
        grants: Vec<PersonalVolumeGrant>,
    ) -> Result<()> {
        validate_version(session_id)?;
        let _consent = self
            .personal_consent
            .write()
            .map_err(|_| Error::Internal("personal consent lock is poisoned"))?;
        let _profile = self.lock_profile()?;
        self.require_account_authority()?;
        if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES - 1 {
            return Err(Error::LimitExceeded);
        }
        if grants.is_empty() {
            return Ok(());
        }
        let mut merged: BTreeMap<_, _> = self
            .personal_volume_grants(session_id)?
            .unwrap_or_default()
            .into_iter()
            .map(|grant| (grant.volume, grant.write))
            .collect();
        for grant in grants {
            *merged.entry(grant.volume).or_insert(false) |= grant.write;
        }
        let grants = merged
            .into_iter()
            .map(|(volume, write)| PersonalVolumeGrant { volume, write })
            .collect();
        self.commit_personal_volume_grants(session_id, grants)
    }

    /// Caller holds the personal-consent write and profile locks through commit.
    fn commit_personal_volume_grants(
        &self,
        session_id: &str,
        grants: Vec<PersonalVolumeGrant>,
    ) -> Result<()> {
        if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES - 1 {
            return Err(Error::LimitExceeded);
        }
        let eligible: BTreeSet<_> = self.personal_volumes()?.into_iter().map(|v| v.id).collect();
        let mut selected = BTreeSet::new();
        for grant in &grants {
            if !eligible.contains(&grant.volume) || !selected.insert(grant.volume) {
                return Err(Error::InvalidInput(
                    "select each eligible personal volume at most once",
                ));
            }
        }
        let previous = self.personal_volume_selections(session_id)?.unwrap_or_default();
        let grants = grants
            .into_iter()
            .map(|grant| {
                let incarnation = match previous.iter().find(|selection| selection.grant() == grant)
                {
                    Some(selection) => selection.incarnation,
                    None => Id::generate()?,
                };
                Ok(PersonalVolumeSelection {
                    volume: grant.volume,
                    write: grant.write,
                    incarnation,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("session-mounts/{session_id}").as_bytes(), &encode(&grants)?)?;
        let account_changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::PersonalVolumes {
                session: session_id.to_owned(),
                grants,
            },
        )?;
        tx.commit()?;
        self.changes.observed_changed(None);
        self.committed_account_record(account_changed);
        Ok(())
    }

    pub(super) fn personal_grant(
        &self,
        access: &Access,
        session: &Session,
        volume: Id,
    ) -> Result<PersonalVolumeGrant> {
        self.require_account_authority()?;
        if access.account != self.account.id || self.volume(volume)?.owner != self.account.id {
            return Err(Error::AccessDenied("personal volume owner"));
        }
        let selection = self
            .personal_volume_selections(&session.id)?
            .unwrap_or_default()
            .into_iter()
            .find(|g| g.volume == volume)
            .ok_or(Error::AccessDenied("personal volume not mounted for this session"))?;
        if access
            .personal_incarnation
            .is_some_and(|expected| expected != (volume, selection.incarnation))
        {
            return Err(Error::AccessDenied("personal volume consent incarnation changed"));
        }
        Ok(selection.grant())
    }

    /// Candidate metadata admission does not grant a running instance access to
    /// another build. The exact retained binding must still be current.
    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn authorize_session_candidate(
        &self,
        previous: &Session,
        candidate: &Session,
    ) -> Result<()> {
        if candidate.shared_volume.is_none() || !previous.same_identity(candidate) {
            return Err(Error::AccessDenied("candidate Shared binding differs"));
        }
        validate_version(&candidate.resolved_build_id)?;
        if let BuildSelector::Exact { build_id } = &candidate.selector
            && build_id != &candidate.resolved_build_id
        {
            return Err(Error::InvalidInput("candidate selector differs from its build"));
        }
        let operations = self.session_dependency_operations(previous)?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        if self.session(&previous.id)? != *previous {
            return Err(Error::AccessDenied("session changed before candidate admission"));
        }
        self.authorize_shared_candidate_policy(candidate, &self.index(candidate.replica_volume())?)
    }

    fn authorize_shared_candidate_policy(
        &self,
        candidate: &Session,
        policy: &MetadataPolicy,
    ) -> Result<()> {
        self.require_account_authority()?;
        if self.session_denial(&candidate.id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        self.check_participant(candidate.replica_volume(), self.account.id)?;
        self.check_dependency_authority(self.account.id, candidate.replica_volume())?;
        policy.admit(self.account.id, candidate, candidate.replica_volume())?;
        let access = Access::session(self.account.id, &candidate.id);
        policy.authorize(self.data_actor(&access, policy.volume())?, Permission::Read)
    }

    /// Called only after authoritative AP resolution and candidate validation.
    /// Commit only if the captured selection is still current under the profile
    /// and volume guards. Invitation and credential bindings remain immutable.
    pub fn adopt_session(&self, previous: &Session, session: &Session) -> Result<()> {
        let retained = self.session(&session.id)?;
        let operations = self.operations(&[retained.replica_volume()])?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        let current = self.session(&session.id)?;
        validate_version(&session.resolved_build_id)?;
        if current != *previous {
            return Err(Error::InvalidInput("session selection changed during preparation"));
        }
        if self.session_denial(&session.id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        session.app.validate().map_err(Error::InvalidInput)?;
        if current.app != session.app
            || current.root_volume != session.root_volume
            || current.shared_volume != session.shared_volume
            || current.creator != session.creator
            || current.update_authority != session.update_authority
        {
            return Err(Error::InvalidInput("session update changes immutable bindings"));
        }
        match (&current.selector, &session.selector) {
            (BuildSelector::Exact { .. }, BuildSelector::Exact { build_id })
                if build_id == &session.resolved_build_id => {}
            (BuildSelector::App { app: old }, BuildSelector::App { app }) if old == app => {}
            _ => return Err(Error::InvalidInput("session update changes selector authority")),
        }
        if session.shared_volume.is_some() && self.shared_session_initialized(&session.id)? {
            self.authorize_shared_candidate_policy(
                session,
                &self.index(session.replica_volume())?,
            )?;
        } else if !self.has_implicit_local_root(session)? {
            self.index(session.replica_volume())?.admit(
                self.account.id,
                session,
                session.replica_volume(),
            )?;
        }
        if let Some(mut local) = self.local_session(&session.id)? {
            local.session = session.clone();
            put(&self.metadata, format!("local-session/{}", session.id).as_bytes(), &local)?;
        } else {
            let mut stored = self.stored_session(&session.id)?;
            stored.session = session.clone();
            put(&self.metadata, format!("session/{}", session.id).as_bytes(), &stored)?;
        }
        self.changes.observed_changed(Some(session.replica_volume()));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn concurrent_additions_keep_each_other_and_do_not_restore_unrelated_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let first = store.create_volume("First", false).unwrap().id;
        let second = store.create_volume("Second", false).unwrap().id;
        let revoked = store.create_volume("Revoked", false).unwrap().id;
        let session = crate::identifier::encode(&[0x12; 32]);
        store
            .select_personal_volumes(
                &session,
                vec![PersonalVolumeGrant { volume: revoked, write: true }],
            )
            .unwrap();
        // Two clients may have observed this old selection before revocation.
        let stale = store.personal_volume_grants(&session).unwrap().unwrap();
        store.select_personal_volumes(&session, vec![]).unwrap();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let add_first = scope.spawn(|| {
                barrier.wait();
                store.add_personal_volumes(
                    &session,
                    vec![PersonalVolumeGrant { volume: first, write: false }],
                )
            });
            let add_second = scope.spawn(|| {
                barrier.wait();
                store.add_personal_volumes(
                    &session,
                    vec![PersonalVolumeGrant { volume: second, write: true }],
                )
            });
            barrier.wait();
            add_first.join().unwrap().unwrap();
            add_second.join().unwrap().unwrap();
        });
        assert_eq!(stale[0].volume, revoked);
        let grants = store.personal_volume_grants(&session).unwrap().unwrap();
        assert_eq!(grants.len(), 2);
        assert!(grants.contains(&PersonalVolumeGrant { volume: first, write: false }));
        assert!(grants.contains(&PersonalVolumeGrant { volume: second, write: true }));
        assert!(!grants.iter().any(|grant| grant.volume == revoked));
        store
            .add_personal_volumes(
                &session,
                vec![PersonalVolumeGrant { volume: first, write: true }],
            )
            .unwrap();
        let upgraded = store.personal_volume_selections(&session).unwrap().unwrap();
        store
            .add_personal_volumes(
                &session,
                vec![PersonalVolumeGrant { volume: first, write: false }],
            )
            .unwrap();
        assert_eq!(store.personal_volume_selections(&session).unwrap().unwrap(), upgraded);
        drop(store);
        let reopened = LocalStore::open(directory.path()).unwrap();
        assert_eq!(reopened.personal_volume_selections(&session).unwrap().unwrap(), upgraded);
    }

    #[test]
    fn personal_mounts_are_session_scoped_durable_and_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let root = store.create_volume("Root", true).unwrap();
        let personal = store.create_volume("Personal", true).unwrap();
        let owner = Access::owner(store.account.id);
        store
            .create_database(
                &owner,
                personal.id,
                "Folder/seed",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let folder = store.resolve_entry(&owner, personal.id, "Folder").unwrap();
        let entry = store
            .create_database(
                &owner,
                personal.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: Schema::default() },
            )
            .unwrap();
        let address =
            crate::store::volume_index::EntryAddress { volume: personal.id, entry: entry.id };
        let v1 = crate::identifier::encode(&[0x11; 32]);
        let v2 = crate::identifier::encode(&[0x22; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: v1.clone() },
                &v1,
                root.id,
            )
            .unwrap();
        let access = Access::session_version(store.account.id, &session.id, &v1);
        assert!(store.schema(&access, address).is_err());
        assert!(store.volume_details(&access, personal.id).is_err());
        assert!(
            store
                .create_database_in(
                    &access,
                    personal.id,
                    folder.id,
                    "Before",
                    crate::store::volume_index::EntryKind::KvDb {}
                )
                .is_err()
        );
        store
            .select_personal_volumes(
                &session.id,
                vec![PersonalVolumeGrant { volume: personal.id, write: false }],
            )
            .unwrap();
        let selection = store.personal_volume_selections(&session.id).unwrap().unwrap();
        assert!(store.schema(&access, address).is_ok());
        assert!(store.volume_details(&access, personal.id).is_ok());
        assert!(store.establish_schema(&access, address, &Schema::default()).is_err());
        assert!(store.create_bloblog_in(&access, personal.id, folder.id, "Read-only").is_err());
        let later = store.create_volume("Later", true).unwrap();
        let later_entry = store
            .create_database(
                &owner,
                later.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: Schema::default() },
            )
            .unwrap();
        let later_address =
            crate::store::volume_index::EntryAddress { volume: later.id, entry: later_entry.id };
        assert!(store.schema(&access, later_address).is_err());
        let other = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: v1.clone() },
                &v1,
                root.id,
            )
            .unwrap();
        assert!(store.schema(&Access::session(store.account.id, &other.id), address).is_err());
        let mut updated = session.clone();
        updated.selector = BuildSelector::Exact { build_id: v2.clone() };
        updated.resolved_build_id = v2.clone();
        store.adopt_session(&session, &updated).unwrap();
        let access = Access::session_version(store.account.id, &session.id, &v2);
        assert!(store.schema(&access, address).is_ok());
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert!(store.schema(&access, address).is_ok());
        assert_eq!(store.personal_volume_selections(&session.id).unwrap().unwrap(), selection);
        store
            .select_personal_volumes(
                &session.id,
                vec![PersonalVolumeGrant { volume: personal.id, write: true }],
            )
            .unwrap();
        assert!(store.establish_schema(&access, address, &Schema::default()).is_ok());
        assert!(
            store
                .create_database_in(
                    &access,
                    personal.id,
                    folder.id,
                    "Writable",
                    crate::store::volume_index::EntryKind::KvDb {}
                )
                .is_ok()
        );
        store.select_personal_volumes(&session.id, vec![]).unwrap();
        assert!(store.volume_details(&access, personal.id).is_err());
        assert!(
            store
                .create_blob_from_reader_in(
                    &access,
                    personal.id,
                    folder.id,
                    "Revoked",
                    &mut std::io::empty()
                )
                .is_err()
        );
        assert!(
            store.schema(&access, address).is_err(),
            "revocation applies to existing host handles"
        );
        assert!(
            store
                .select_personal_volumes(
                    &session.id,
                    vec![PersonalVolumeGrant { volume: root.id, write: true }]
                )
                .is_err()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert durable consent incarnations.")]
mod incarnation_tests {
    use super::*;
    #[test]
    fn consent_incarnations_survive_noops_and_change_after_rights_or_removal() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let first = store.create_volume("First", false).unwrap().id;
        let second = store.create_volume("Second", false).unwrap().id;
        let version = crate::identifier::encode(&[0x12; 32]);
        let grant = PersonalVolumeGrant { volume: first, write: true };
        let incarnation = |store: &LocalStore| {
            store.personal_volume_selections(&version).unwrap().unwrap()[0].incarnation
        };
        store.select_personal_volumes(&version, vec![grant.clone()]).unwrap();
        let original = incarnation(&store);
        store.select_personal_volumes(&version, vec![grant.clone()]).unwrap();
        assert_eq!(incarnation(&store), original);
        store
            .select_personal_volumes(
                &version,
                vec![grant.clone(), PersonalVolumeGrant { volume: second, write: false }],
            )
            .unwrap();
        assert_eq!(incarnation(&store), original);
        store
            .select_personal_volumes(
                &version,
                vec![PersonalVolumeGrant { volume: first, write: false }],
            )
            .unwrap();
        let narrowed = incarnation(&store);
        assert_ne!(narrowed, original);
        store.select_personal_volumes(&version, vec![]).unwrap();
        store.select_personal_volumes(&version, vec![grant]).unwrap();
        let restored = incarnation(&store);
        assert_ne!(restored, original);
        assert_ne!(restored, narrowed);
        drop(store);
        let reopened = LocalStore::open(directory.path()).unwrap();
        assert_eq!(incarnation(&reopened), restored);
    }
}
