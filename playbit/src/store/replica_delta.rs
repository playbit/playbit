//! Recipient admission for bounded, already committed database changes.
use crate::store::object_contract::{Permission, Principal};
use crate::store::volume_index::{self, IndexSnapshot};
use crate::store::{DatabaseId, DatabasePolicy, EntryId, Error, Id, ReplicaReader, Result};
use std::collections::BTreeSet;
pub const MAX_REPLICA_DELTA_BYTES: usize = 64 * 1024;

/// Current originating-writer admission, independent of catalog/content Read.
/// The caller rechecks session/consent and the returned entry authority fences.
/// Service projection and AP's independent pre-merge validation still follow.
pub fn authorize_publication_delta<S: kvdb::Storage>(
    index: &kvdb::Db<S>,
    policy: &DatabasePolicy,
    account: Id,
    bytes: &[u8],
) -> Result<Vec<EntryId>> {
    let snapshot = IndexSnapshot::new(index.begin_read()?);
    let mut entries = BTreeSet::new();
    let mut admission = Ok(());
    kvdb::sync::visit_delta(bytes, MAX_REPLICA_DELTA_BYTES, |key, remote| {
        if admission.is_err() {
            return;
        }
        admission = (|| {
            if policy.database == DatabaseId::Index && volume_index::is_index_key(key) {
                let entry = volume_index::source_entry_id(key)?;
                if entries.len() == volume_index::MAX_REPAIR_IDS && !entries.contains(&entry) {
                    return Err(Error::LimitExceeded);
                }
                snapshot.authorize_committed_source(
                    policy.volume.volume(),
                    policy.volume.default_permissions(),
                    account,
                    key,
                    remote,
                )?;
                entries.insert(entry);
            } else {
                match policy.authorize(Principal::Human(account), Permission::Write) {
                    Err(Error::AccessDenied(_)) => {
                        policy.authorize(Principal::Human(account), Permission::Append)?;
                    }
                    result => result?,
                }
                if matches!(policy.database, DatabaseId::Index | DatabaseId::Revocations)
                    && policy.volume.volume().owner != account
                {
                    return Err(Error::AccessDenied("reserved publication owner"));
                }
            }
            Ok(())
        })();
    })?;
    admission?;
    if let DatabaseId::Entry(entry) = policy.database {
        entries.insert(entry);
    }
    Ok(entries.into_iter().collect())
}

/// Entry permission applies to the whole named backing. Index metadata remains
/// visible to admitted volume readers, including restricted-entry discovery.
// fuzz: store_contract
pub fn project_replica_delta(
    policy: &DatabasePolicy,
    reader: ReplicaReader,
    bytes: &[u8],
) -> Result<Vec<u8>> {
    policy.ordinary_protocol()?;
    policy.authorize_reader(reader)?;
    if bytes.len() > MAX_REPLICA_DELTA_BYTES {
        return Err(Error::LimitExceeded);
    }
    // Validate framing without copying a second value or inventing new clocks.
    kvdb::sync::visit_delta(bytes, MAX_REPLICA_DELTA_BYTES, |_, _| {})?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful bounded storage fixtures.")]
mod tests {
    use super::*;
    use crate::store::object_contract::{Acl, EntryAcl};
    use crate::store::{MetadataPolicy, Volume};
    use volume_index::{DatabaseProtocol, EntryInfo, EntryKind};

    #[test]
    fn publication_admission_bounds_distinct_entries_and_checks_current_writer() {
        for count in [64u8, 65] {
            let index = kvdb::Db::create(
                kvdb::storage::mem::MemStorage::new(),
                &kvdb::CreateOptions::syncable([7; 32], 0),
            )
            .unwrap();
            let volume = Volume {
                id: Id::from_bytes([1; 16]),
                owner: Id::from_bytes([2; 16]),
                name: "Publication".into(),
                personal: true,
                share_key_with_service: true,
            };
            MetadataPolicy::new(volume.clone(), Acl::default())
                .unwrap()
                .write(&index, 1000)
                .unwrap();
            let mut tx = index.begin_write_with_delta(1001, MAX_REPLICA_DELTA_BYTES).unwrap();
            for number in 1..=count {
                // Independent concurrent births isolate the distinct-entry cap
                // from the separate 64-KiB delta and causal-history byte caps.
                let info = EntryInfo {
                    namespace: Default::default(),
                    origin: crate::store::volume_index::namespace::Origin {
                        installation: crate::store::Id::from_bytes([42; 16]),
                        label: None,
                    },
                    name: format!("entry-{number}"),
                    created_revision: EntryId::from_bytes([100; 15]).unwrap(),
                    kind: EntryKind::KvDb {},
                    protocol: Some(DatabaseProtocol::Kvdb),
                    initial_permissions: EntryAcl::Inherit,
                };
                tx.put(
                    &volume_index::entry_key(EntryId::from_bytes([number; 15]).unwrap()),
                    &crate::store::catalog::record::encode(&info).unwrap(),
                )
                .unwrap();
            }
            let kvdb::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
                panic!()
            };
            let policy = DatabasePolicy::read(&index, DatabaseId::Index).unwrap();
            let admitted = authorize_publication_delta(&index, &policy, volume.owner, &bytes);
            if count == 64 {
                assert_eq!(admitted.unwrap().len(), 64);
            } else {
                assert!(matches!(admitted, Err(Error::LimitExceeded)));
            }
            assert!(matches!(
                authorize_publication_delta(&index, &policy, Id::from_bytes([3; 16]), &bytes),
                Err(Error::AccessDenied(_))
            ));
            let clock = index.clock();
            assert!(
                authorize_publication_delta(
                    &index,
                    &policy,
                    volume.owner,
                    &bytes[..bytes.len() - 1]
                )
                .is_err()
            );
            assert_eq!(index.clock(), clock, "Admission never changes durable source state");
        }
    }
}
