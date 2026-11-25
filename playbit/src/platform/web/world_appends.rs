//! Re-submit only a locally authored KV birth while a World grants Append.
//! A full backing may also contain values written by other players.
use super::*;
use crate::store::key_value::{KV_APPEND, kv_append_key, kv_append_parts};
use crate::store::{DatabasePolicy, ReplicaReader};
use kvdb::Clock;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    volume: Id,
    database: EntryId,
    after: Option<[u8; 8]>,
    through: Option<[u8; 8]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledged {
    volume: Id,
    database: EntryId,
    clock: [u8; 8],
}
struct Offered {
    volume: Id,
    entry: EntryId,
    clock: Clock,
    source: std::rc::Weak<Database>,
}
thread_local! { static OFFERED: RefCell<Option<Offered>> = const { RefCell::new(None) }; }

fn backing(host: &Host, volume: Id, entry: EntryId) -> Result<Rc<Database>> {
    let volume = host.replica_volume(volume)?;
    if !host.is_projected_world(&volume) {
        return Err(Error::AccessDenied);
    }
    let db = match host.kv_backing(&volume, entry, Permission::Append) {
        Err(Error::AccessDenied) => host.kv_backing(&volume, entry, Permission::Write)?,
        other => other?,
    };
    let policy =
        DatabasePolicy::read(&volume.index, DatabaseId::Entry(entry)).map_err(store_error)?;
    policy.authorize_reader(ReplicaReader::Service).map_err(store_error)?;
    Ok(db)
}

fn next(host: &Host, input: &[u8]) -> Result<Vec<u8>> {
    let cursor: Cursor = serde_json::from_slice(input).map_err(map_error)?;
    if cursor.after.is_some() != cursor.through.is_some() {
        return Err(Error::InvalidArgument);
    }
    let source = backing(host, cursor.volume, cursor.database)?;
    let snapshot = source.begin_read().map_err(map_error)?;
    let latest = snapshot.clock().map_err(map_error)?;
    let through = cursor.through.map_or(latest, |bytes| Clock::from_u64(u64::from_be_bytes(bytes)));
    if through > latest || cursor.after.is_some_and(|after| after > through.encode()) {
        return Err(Error::Conflict);
    }
    let start = match cursor.after {
        None => KV_APPEND.to_vec(),
        Some(clock) => [KV_APPEND, &clock, &[0]].concat(),
    };
    let Some(row) = snapshot.local_iter_from(&start).map_err(map_error)?.next() else {
        return Ok(Vec::new());
    };
    let (marker, record) = row.map_err(map_error)?;
    let Some(bytes) = marker.strip_prefix(KV_APPEND) else {
        return Ok(Vec::new());
    };
    let clock = Clock::decode(bytes).ok_or(Error::InvalidArgument)?;
    if clock > through {
        return Ok(Vec::new());
    }
    let (digest, key) = kv_append_parts(&record).map_err(store_error)?;
    let mut output = [through.encode(), clock.encode()].concat();
    if let Some(value) = snapshot.value_reader(key).map_err(map_error)?
        && value.clock() == Some(clock)
        && kvdb::hash::entry_hash_from(&[0; 32], key, 0, &value).map_err(map_error)? == digest
    {
        let Some(kvdb::CommittedDelta::Bytes(delta)) = snapshot
            .delta_for_key(key, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?
        else {
            return Err(Error::LimitExceeded);
        };
        output.extend_from_slice(&delta);
    }
    OFFERED.with(|offered| {
        *offered.borrow_mut() = Some(Offered {
            volume: cursor.volume,
            entry: cursor.database,
            clock,
            source: Rc::downgrade(&source),
        })
    });
    Ok(output)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_world_append_next(length: usize) -> i32 {
    super::automatic::with_host(length, |host, input| {
        OFFERED.with(|offered| offered.borrow_mut().take());
        next(host, input)
    })
}
/// A durable live receipt covers the exact submitted delta, even if another
/// edit or a qualified capture changed the current backing while AP replied.
#[unsafe(no_mangle)]
pub extern "C" fn pb_world_append_published(slot: usize, length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let bytes = admit_input(length, crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let host = HOST.with(|cell| cell.borrow().clone()).ok_or(Error::Unavailable)?;
        let address = host.database_address(slot)?;
        let volume = host.replica_volume(address.volume)?;
        if !host.is_projected_world(&volume) || !matches!(address.database, DatabaseId::Entry(_)) {
            return Err(Error::InvalidArgument);
        }
        let db = host.database(slot)?;
        crate::store::key_value::acknowledge_appends(&db, &bytes[..length], unsafe { now() }
            as u64)
        .map_err(store_error)?;
        Ok(())
    })();
    result.map_or_else(|error| error as i32, |()| 0)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_world_append_ack(length: usize) -> i32 {
    super::automatic::with_host(length, |host, input| {
        let ack: Acknowledged = serde_json::from_slice(input).map_err(map_error)?;
        let current = backing(host, ack.volume, ack.database)?;
        let offered = OFFERED.with(|offered| offered.borrow_mut().take()).ok_or(Error::Conflict)?;
        if (ack.volume, ack.database, ack.clock)
            != (offered.volume, offered.entry, offered.clock.encode())
            || !std::rc::Weak::ptr_eq(&Rc::downgrade(&current), &offered.source)
        {
            return Err(Error::Conflict);
        }
        let mut tx = current.begin_write(unsafe { now() } as u64).map_err(map_error)?;
        tx.local_indexes().del(&kv_append_key(offered.clock)).map_err(map_error)?;
        tx.commit().map_err(map_error)?;
        Ok(Vec::new())
    })
}
