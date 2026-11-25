//! Resolve prepared backings under the received policy, retaining originals
//! before any rejected local data can leave the active generation.
use super::*;

pub(super) fn rejected(host: &Host, stage: &Preparation, entry: EntryId) -> Result<bool> {
    if stage.archive.borrow().rejected_backings.contains(&entry) {
        return Ok(true);
    }
    let original = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?);
    if !original.source_backing_ready(entry).map_err(store_error)? {
        return Ok(false);
    }
    let actor = host.actor(&stage.volume);
    let unavailable =
        match DatabasePolicy::read_source(&stage.accepted_index, DatabaseId::Entry(entry)) {
            Ok(policy) => {
                if allowed(policy.authorize(actor, Permission::Write))?
                    || (policy.entry.as_ref().and_then(|entry| entry.protocol)
                        == Some(DatabaseProtocol::BlobLog)
                        && allowed(policy.authorize(actor, Permission::Append))?)
                {
                    return Ok(false);
                }
                if host.is_projected_world(&stage.volume)
                    && policy.entry.as_ref().is_some_and(|entry| {
                        entry.kind == crate::store::EntryType::KvDb
                            && entry.protocol == Some(DatabaseProtocol::Kvdb)
                    })
                    && allowed(policy.authorize(actor, Permission::Append))?
                    && allowed(policy.authorize_reader(ReplicaReader::Service))?
                {
                    let old = DatabasePolicy::read(&stage.original.index, DatabaseId::Entry(entry))
                        .map_err(store_error)?;
                    // Retained Append can replay only authored births. Losing
                    // Write still sends the backing through reconciliation.
                    if allowed(old.authorize(actor, Permission::Append))?
                        && !allowed(old.authorize(actor, Permission::Write))?
                    {
                        return Ok(false);
                    }
                }
                !allowed(policy.authorize_reader(ReplicaReader::SessionParticipant(actor)))?
                    || !allowed(policy.authorize_reader(ReplicaReader::Service))?
            }
            Err(crate::store::Error::NotFound(_)) => {
                let policy = MetadataPolicy::read(&stage.accepted_index).map_err(store_error)?;
                volume_index::rejected_sources(
                    &stage.original.index.begin_read().map_err(map_error)?,
                    &stage.accepted_index,
                    &policy,
                    actor,
                    entry,
                )
                .map_err(store_error)?
            }
            Err(error) => return Err(store_error(error)),
        };
    if unavailable {
        stage.archive.borrow_mut().reject_backing(entry, true)?;
    }
    Ok(unavailable)
}

pub(super) fn allowed(result: crate::store::Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(crate::store::Error::AccessDenied(_) | crate::store::Error::NotFound(_)) => Ok(false),
        Err(error) => Err(store_error(error)),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_log_prepare() -> i32 {
    call(0, |host, _| {
        let stage = prepared()?;
        prepare_log(host, &stage)?;
        serde_json::to_vec(&stage.log_prepared.get()).map_err(map_error)
    })
}

pub(super) fn prepare_log(host: &Host, stage: &Preparation) -> Result<()> {
    if stage.log_prepared.get() {
        return Ok(());
    }
    let named = stage.named.borrow();
    let named = named.as_ref().ok_or(Error::Unavailable)?;
    let DatabaseId::Entry(entry) = named.address.database else {
        return Err(Error::InvalidArgument);
    };
    let policy = stage.policy(host, named.address.database)?;
    if policy.entry.as_ref().and_then(|entry| entry.protocol) != Some(DatabaseProtocol::BlobLog) {
        return Err(Error::InvalidArgument);
    }
    let replace = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?)
        .backing_quarantined(entry)
        .map_err(store_error)?;
    if !replace
        && (allowed(policy.authorize(host.actor(&stage.volume), Permission::Write))?
            || allowed(policy.authorize(host.actor(&stage.volume), Permission::Append))?)
    {
        stage.log_prepared.set(true);
        return Ok(());
    }
    let (count, complete) = crate::store::bloblog::Log {
        database: &*named.db,
        volume: stage.volume.id,
        entry,
        encryption_key: None,
        now: unsafe { now() } as u64,
    }
    .discard_pending_page()
    .map_err(store_error)?;
    if count > 0 && !replace {
        stage.archive.borrow_mut().reject_backing(entry, false)?;
    }
    if replace {
        stage.archive.borrow_mut().restored_quarantine = true;
    }
    stage.log_prepared.set(complete);
    Ok(())
}

pub(super) fn merge(
    host: &Host,
    stage: &Preparation,
    entry: EntryId,
    database: &Database,
    changes: &kvdb::sync::PreparedChanges<OpfsStaging>,
    policy: &DatabasePolicy,
) -> Result<()> {
    use kvdb::sync::{Lww, Resolution, SyncDelta, SyncEntState, SyncResolver};
    let replace = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?)
        .backing_quarantined(entry)
        .map_err(store_error)?;
    let failure = RefCell::new(None);
    let rejected = Cell::new(false);
    let resolver = |delta: &SyncDelta<'_>| {
        let result = (|| -> crate::store::Result<Resolution> {
            if !matches!(delta.remote, SyncEntState::Missing) {
                policy.validate_sync_source(delta, None)?;
            }
            if replace {
                return Ok(Resolution::UseRemote);
            }
            let ranked = Lww.resolve(delta)?;
            if ranked == Resolution::KeepLocal {
                let proposed =
                    SyncDelta { key: delta.key, local: delta.remote, remote: delta.local };
                match policy.validate_sync_source_as(&proposed, Some(host.actor(&stage.volume))) {
                    Ok(_) => {}
                    Err(crate::store::Error::AccessDenied(_)) => {
                        rejected.set(true);
                        return Ok(Resolution::UseRemote);
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(ranked)
        })();
        match result {
            Ok(resolution) => Ok(resolution),
            Err(error) => {
                *failure.borrow_mut() = Some(store_error(error));
                Ok(Resolution::KeepLocal)
            }
        }
    };
    let merged =
        changes.merge(database, resolver, unsafe { now() } as u64, 0).map_err(map_error)?;
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    live::validate_snapshot(merged.pending_transaction().map_err(map_error)?, policy)
        .map_err(store_error)?;
    merged.finish_with_delta().map_err(map_error)?;
    if rejected.get() {
        stage.archive.borrow_mut().reject_backing(entry, false)?;
    }
    if replace {
        stage.archive.borrow_mut().restored_quarantine = true;
    }
    let mut tx = stage.volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    volume_index::mark_source_backing_ready(&mut tx, entry).map_err(store_error)?;
    tx.commit().map_err(map_error)?;
    Ok(())
}
