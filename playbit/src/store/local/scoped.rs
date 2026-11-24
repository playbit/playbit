//! Prefix-bounded selected-entry projection; original source clocks survive.
use super::*;
use crate::store::{DatabaseId, EntryId, entry_policy_keys, volume_index};

const RECORD_BYTES: usize = volume_index::MAX_ENTRY_RECORD_BYTES + kvdb::KEY_SIZE_MAX + 128;
const DISK_BYTES: u64 = 128 * 1024 * 1024;
const PAGE_BYTES: usize = 1024 * 1024;
const PAGE_ROWS: usize = 64;

pub struct ScopedSnapshot<'db> {
    source: &'db Database,
    snapshot: kvdb::ReadTxn<'db, RedbStorage>,
    prefixes: Vec<Vec<u8>>,
    database: DatabaseId,
    entry: EntryId,
    policy: DatabasePolicy,
    controls: bool,
    world_projection: bool,
    current: Option<volume_index::namespace::source::cut::current::Record>,
}
impl<'db> ScopedSnapshot<'db> {
    /// Pin under the caller's volume guard after checking current participants.
    /// Policy admits the selected participant; the transport checks service access.
    pub fn new(
        source: &'db Database,
        database: DatabaseId,
        policy: &DatabasePolicy,
        actor: impl Into<DataActor>,
        version: Option<&str>,
    ) -> Result<Self> {
        Self::capture(source, database, policy, actor, version, true)
    }

    /// A terminal selected Index pull returns only the qualified birth and
    /// deletion to a current reader/writer. The service also needs entry Read.
    pub fn retirement_confirmation(
        source: &'db Database,
        policy: &DatabasePolicy,
        actor: DataActor,
        entry: EntryId,
    ) -> Result<Self> {
        let snapshot = source.begin_read()?;
        let index = volume_index::IndexSnapshot::new(source.begin_read()?);
        let volume = policy.volume.volume();
        let defaults = policy.volume.default_permissions();
        index.authorize_retained_read(volume, defaults, actor, entry)?;
        index.authorize_retained_read(
            volume,
            defaults,
            crate::store::catalog::PLAYBIT_REPLICA_PRINCIPAL,
            entry,
        )?;
        if !index.source_granted(volume, defaults, actor, entry, Permission::Write)? {
            return Err(Error::AccessDenied("entry retirement requires Write"));
        }
        let mut current =
            volume_index::namespace::source::cut::current::Record::read(&snapshot, entry)?
                .filter(|record| record.deletion.is_some())
                .ok_or(Error::NotFound("qualified retirement"))?;
        current.name = None;
        current.parent = None;
        current.content = None;
        Ok(Self {
            source,
            snapshot,
            prefixes: vec![volume_index::birth_source_key(entry).to_vec()],
            database: DatabaseId::Index,
            entry,
            policy: policy.clone(),
            controls: false,
            world_projection: false,
            current: Some(current),
        })
    }

    /// Reconciliation preserves raw source clocks and LWW. Detached received
    /// indexes have no app-qualified local controls until atomic publication.
    pub(super) fn for_reconciliation(
        source: &'db Database,
        database: DatabaseId,
        policy: &DatabasePolicy,
        actor: impl Into<DataActor>,
        version: Option<&str>,
    ) -> Result<Self> {
        Self::capture(source, database, policy, actor, version, false)
    }

    fn capture(
        source: &'db Database,
        database: DatabaseId,
        policy: &DatabasePolicy,
        actor: impl Into<DataActor>,
        version: Option<&str>,
        qualified: bool,
    ) -> Result<Self> {
        let DatabaseId::Entry(entry) = policy.database else {
            return Err(Error::InvalidInput("selected entry policy required"));
        };
        let actor = actor.into();
        let readable = policy.metadata_readable(actor)?;
        let snapshot = source.begin_read()?;
        let current = if qualified && database == DatabaseId::Index {
            Some(
                volume_index::namespace::source::cut::current::Record::read(&snapshot, entry)?
                    .ok_or(Error::NotFound("qualified selected entry"))?,
            )
        } else {
            None
        };
        let prefixes = match database {
            DatabaseId::Index => {
                let mut prefixes = volume_index::entry_source_prefixes(entry, true);
                if !readable {
                    prefixes.retain(|prefix| !volume_index::is_entry_history(prefix));
                }
                // Current controls must come from qualified exact sources. A
                // Pending raw head must not hide an accepted winner during gap
                // recovery, especially when Read does not permit its history.
                if let Some(current) = current {
                    prefixes.retain(|prefix| {
                        !current
                            .fields()
                            .iter()
                            .any(|(tag, _)| prefix.starts_with(&[volume_index::META_BYTE, *tag]))
                    });
                }
                prefixes.extend(entry_policy_keys(version)?);
                prefixes
            }
            DatabaseId::Revocations => vec![actor.human()?.to_string().into_bytes()],
            DatabaseId::Entry(_) => {
                return Err(Error::InvalidInput("entry backings use ordinary snapshots"));
            }
        };
        assert!(prefixes.len() <= 14, "ten entry source prefixes plus four policy keys");
        Ok(Self {
            source,
            snapshot,
            prefixes,
            database,
            entry,
            policy: DatabasePolicy::reserved(policy.volume.clone(), database)?,
            controls: true,
            world_projection: false,
            current,
        })
    }

    /// Current Write admits entry facts; Append admits only its birth. ACLs
    /// require current management or an exact local publication receipt.
    pub fn for_publication(
        source: &'db Database,
        policy: &MetadataPolicy,
        entry: EntryId,
        actor: impl Into<DataActor>,
    ) -> Result<Self> {
        let actor = actor.into();
        let account = actor.human()?;
        let snapshot = source.begin_read()?;
        let current = volume_index::IndexSnapshot::new(source.begin_read()?);
        let receipt = current.permission_publication(account, entry)?;
        if !receipt {
            current.authorize_source_writer(
                policy.volume(),
                policy.default_permissions(),
                actor,
                entry,
            )?;
        }
        let policy = DatabasePolicy::reserved(policy.clone(), DatabaseId::Index)?;
        policy.authorize_reader(ReplicaReader::Service)?;
        let permissions = current.permissions_for_edit(entry)?;
        let effective = permissions.effective(policy.volume.default_permissions());
        let manages = account == policy.volume.volume().owner
            || actor.permits(policy.volume.volume().id, effective, Permission::ManagePermissions);
        let inherited_owner =
            matches!(permissions, EntryAcl::Inherit) && account == policy.volume.volume().owner;
        let mut prefixes = if inherited_owner
            || actor.permits(policy.volume.volume().id, effective, Permission::Write)
        {
            volume_index::entry_source_prefixes(entry, false)
        } else if actor.permits(policy.volume.volume().id, effective, Permission::Append)
            && !current.source_header(entry)?.deleted
        {
            vec![volume_index::birth_source_key(entry).to_vec()]
        } else {
            Vec::new()
        };
        if manages || receipt {
            prefixes.push(volume_index::permission_source_key(entry).to_vec());
        }
        Ok(Self {
            source,
            snapshot,
            prefixes,
            database: DatabaseId::Index,
            entry,
            policy,
            controls: false,
            world_projection: false,
            current: None,
        })
    }

    #[cfg(feature = "connected-engine")]
    pub(super) fn only_revision_history(mut self) -> Self {
        self.prefixes.retain(|key| volume_index::is_entry_history(key));
        self
    }

    /// Projection runs after releasing the source guard, against this pinned cut.
    pub fn project(self, scratch: &Path) -> Result<OutboundReplica> {
        let projected = if self.world_projection {
            let volume = self.policy.volume.volume().id;
            let hash = self.database.hash_key(&DatabaseId::world_projection_key(volume), volume);
            super::empty_replica_with_hash(scratch, hash, self.source.meta().branch_factor)?
        } else {
            empty_detached_replica(scratch, self.source)?
        };
        self.merge_pages(projected.database(), || {
            if fs::metadata(projected._directory.0.join("replica.kvdb")).map_err(io)?.len()
                > DISK_BYTES
            {
                return Err(Error::LimitExceeded);
            }
            Ok(())
        })?;
        Ok(projected)
    }
    pub fn project_world(mut self, scratch: &Path) -> Result<OutboundReplica> {
        self.world_projection = true;
        self.project(scratch)
    }

    /// The caller retains publication ordering and a durable replay intent for
    /// a live destination. No target-only keyspace scan is part of this merge.
    pub(super) fn merge_into(self, target: &Database) -> Result<()> {
        if self.current.is_some() {
            return Err(Error::InvalidInput(
                "qualified recipient projection requires an empty destination",
            ));
        }
        self.merge_pages(target, || Ok(()))
    }

    fn merge_pages(
        self,
        target: &Database,
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut page: Option<Vec<u8>> = None;
        let mut rows = 0;
        let mut charged = 0u64;
        for prefix in &self.prefixes {
            let mut cursor = prefix.clone();
            let mut exclusive = false;
            while let Some(key) = self.snapshot.next_source_key(&cursor, exclusive)? {
                if !key.starts_with(prefix)
                    || (self.database == DatabaseId::Revocations && key != *prefix)
                {
                    break;
                }
                let Some(kvdb::CommittedDelta::Bytes(bytes)) =
                    self.snapshot.delta_for_key(&key, RECORD_BYTES)?
                else {
                    return Err(Error::LimitExceeded);
                };
                charged = charged.checked_add(bytes.len() as u64).ok_or(Error::LimitExceeded)?;
                if charged > DISK_BYTES {
                    return Err(Error::LimitExceeded);
                }
                if page
                    .as_ref()
                    .is_some_and(|page| rows == PAGE_ROWS || page.len() + bytes.len() > PAGE_BYTES)
                {
                    self.replay_page(target, page.take().expect("checked pending page"))?;
                    checkpoint()?;
                    rows = 0;
                }
                page = Some(match page {
                    Some(page) => kvdb::sync::combine_deltas(&page, &bytes, PAGE_BYTES)?,
                    None => bytes,
                });
                rows += 1;
                assert!(
                    rows <= PAGE_ROWS && page.as_ref().is_some_and(|page| page.len() <= PAGE_BYTES)
                );
                cursor = key;
                exclusive = true;
            }
        }
        if let Some(page) = page {
            self.replay_page(target, page)?;
            checkpoint()?;
        }
        self.finish_projection(target, charged)?;
        checkpoint()?;
        Ok(())
    }

    fn finish_projection(&self, target: &Database, mut charged: u64) -> Result<()> {
        if self.database == DatabaseId::Index {
            let mut tx = target.begin_write(now_unix_secs())?;
            if let Some(current) = self.current {
                for (tag, source) in current.fields() {
                    if let Some(source) = source {
                        let (clock, bytes) = source.read(&self.snapshot)?;
                        charged += bytes.len() as u64;
                        if charged > DISK_BYTES {
                            return Err(Error::LimitExceeded);
                        }
                        let key =
                            [vec![volume_index::META_BYTE, tag], self.entry.as_bytes().to_vec()]
                                .concat();
                        tx.put_with_clock(&key, &bytes, clock)?;
                    }
                }
            }
            if self.controls {
                MetadataPolicy::validate_pending(&tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
            }
            volume_index::repair(&mut tx.local_indexes(), &[self.entry])?;
            tx.commit()?;
        }
        Ok(())
    }

    fn replay_page(&self, target: &Database, mut bytes: Vec<u8>) -> Result<()> {
        if self.world_projection {
            kvdb::sync::rekey_delta(
                &mut bytes,
                PAGE_BYTES,
                &self.source.meta().hash_key,
                &target.meta().hash_key,
            )?;
        }
        let rejected = std::cell::RefCell::new(None);
        let resolver = |delta: &kvdb::sync::Delta<'_>| {
            let admission =
                if self.database == DatabaseId::Index && volume_index::is_index_key(delta.key) {
                    volume_index::validate_source(delta).and_then(|entry| {
                        if entry == self.entry {
                            Ok(())
                        } else {
                            Err(Error::AccessDenied("source outside selected entry"))
                        }
                    })
                } else if self.database == DatabaseId::Index {
                    let value = match delta.remote {
                        kvdb::sync::EntState::Present { value, .. } => Some(value),
                        _ => None,
                    };
                    validate_metadata_entry(self.policy.volume.volume(), delta.key, value)
                } else {
                    self.policy.validate_source(delta, None).map(|_| ())
                };
            match admission {
                Ok(()) => kvdb::Resolver::resolve(&mut kvdb::Lww, delta),
                Err(error) => {
                    *rejected.borrow_mut() = Some(error);
                    kvdb::sync::Resolution::KeepLocal
                }
            }
        };
        let mut receiver = kvdb::DeltaReceiver::new(target, resolver, 0, PAGE_BYTES)?;
        receiver.process(&bytes)?;
        if let Some(error) = rejected.borrow_mut().take() {
            return Err(error);
        }
        // Every durable page keeps its raw source indexes consistent, including
        // interrupted projections. The final caller separately qualifies app
        // state; a detached page has no authority to publish current controls.
        if self.database == DatabaseId::Index {
            receiver = receiver
                .repair_local_indexes(|local| volume_index::repair(local, &[self.entry]))?;
        }
        receiver.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
