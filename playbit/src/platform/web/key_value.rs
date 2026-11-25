//! Named KV operations execute on the existing serial host publication lane.
use super::*;
use crate::{
    runtime::{
        host::{DataResult, kv_stored_root},
        linear_memory::{Memory, layout},
    },
    store::{
        key_value,
        volume_index::{DatabaseProtocol, IndexSnapshot},
    },
};
use playbit_api::abi::kv as abi;

impl Host {
    pub(super) fn kv_backing(
        &self,
        volume: &Volume,
        entry: EntryId,
        permission: Permission,
    ) -> Result<Rc<Database>> {
        self.authorize(volume, Some(entry), permission)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let policy = snapshot.entry_policy(entry).map_err(store_error)?;
        if policy.kind != crate::store::EntryType::KvDb
            || policy.protocol != Some(DatabaseProtocol::Kvdb)
        {
            return Err(Error::InvalidArgument);
        }
        if !snapshot.backing_ready(entry).map_err(map_error)? {
            if self.is_projected_world(volume) && permission != Permission::Read {
                // Mutation rights alone cannot initialize a remote backing.
                // A later qualified Read capture can make it locally ready.
                match self.authorize(volume, Some(entry), Permission::Read) {
                    Err(Error::AccessDenied) => {
                        return Err(if snapshot.backing_quarantined(entry).map_err(store_error)? {
                            Error::Unavailable
                        } else {
                            Error::NotFound
                        });
                    }
                    other => {
                        other?;
                    }
                }
            }
            self.need_entry(volume, entry)?;
            return Err(Error::WouldBlock);
        }
        self.entry_database(volume, entry)
    }
}

fn publish(result: DataResult) -> Result<()> {
    let length = result.kv_output_bytes()?;
    let mut output = OUTPUT.lock().expect("mutex is not poisoned");
    output.resize(length, 0);
    result.encode_kv(&mut output)?;
    Ok(())
}
fn query_root(
    memory: &Memory<'_>,
    record: crate::runtime::linear_memory::Region,
    offset: usize,
    present: usize,
) -> Result<Option<key_value::Root>> {
    kv_stored_root(
        record.u32(memory.data, present),
        abi::Root {
            level: record.u32(memory.data, offset),
            digest: record.read(memory.data, offset + 4),
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_kv_keys(entry: u32, query: u32, after: u32, length: u32) -> i32 {
    let status = super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::KvKeys)?;
        let query = memory.record(query, layout::kv::query::LAYOUT, true, None)?;
        if query.u32(memory.data, 52) != 0 {
            return Err(Error::InvalidArgument);
        }
        let root = query_root(memory, query, 8, 44)?;
        let after = memory.admit(after, length, 1, 1, abi::MAX_KEY_BYTES as u32)?;
        let database = host.kv_backing(&volume, entry, Permission::Read)?;
        let page = key_value::page(
            &database.begin_read().map_err(map_error)?,
            (length != 0).then(|| after.bytes(memory.data)),
            root,
            query.u32(memory.data, 48) as usize,
        )
        .map_err(store_error)?;
        publish(DataResult::KvKeys(page))
    });
    if status != 0 { status } else { OUTPUT.lock().expect("mutex is not poisoned").len() as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_kv_read(entry: u32, key: u32, length: u32, query: u32) -> i32 {
    let status = super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::KvRead)?;
        let query = memory.record(query, layout::kv::read::LAYOUT, true, None)?;
        if query.u32(memory.data, 60) != 0 {
            return Err(Error::InvalidArgument);
        }
        let root = query_root(memory, query, 24, 20)?;
        let key = memory.admit(key, length, 1, 1, abi::MAX_KEY_BYTES as u32)?;
        let database = host.kv_backing(&volume, entry, Permission::Read)?;
        let chunk = key_value::read(
            &database.begin_read().map_err(map_error)?,
            key.bytes(memory.data),
            query.u64(memory.data, 8),
            query.u32(memory.data, 16),
            root,
        )
        .map_err(store_error)?;
        publish(DataResult::KvRead(chunk))
    });
    if status != 0 { status } else { OUTPUT.lock().expect("mutex is not poisoned").len() as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_kv_put(
    entry: u32,
    key: u32,
    length: u32,
    value: u32,
    value_length: u32,
) -> i32 {
    super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::KvPut)?;
        let key = memory.admit(key, length, 1, 1, abi::MAX_KEY_BYTES as u32)?;
        let value = memory.admit(value, value_length, 1, 1, abi::MAX_VALUE_BYTES as u32)?;
        let key = key.bytes(memory.data);
        key_value::validate_key(key).map_err(store_error)?;
        let value = value.bytes(memory.data);
        let (database, append) = match host.kv_backing(&volume, entry, Permission::Write) {
            Ok(database) => (database, false),
            Err(Error::AccessDenied) => {
                (host.kv_backing(&volume, entry, Permission::Append)?, true)
            }
            Err(error) => return Err(error),
        };
        {
            let tx = database.begin_read().map_err(map_error)?;
            if let Some(reader) = tx.value_reader(key).map_err(map_error)? {
                if key_value::identical(&reader, value).map_err(store_error)? {
                    return Ok(());
                }
                if append {
                    return Err(Error::AccessDenied);
                }
            } else if append && tx.is_del(key).map_err(map_error)?.is_some() {
                return Err(Error::AccessDenied);
            }
        }
        let mut tx = database
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        tx.put(key, value).map_err(map_error)?;
        if append && host.is_projected_world(&volume) {
            let marker = key_value::kv_append_key(tx.clock());
            tx.local_indexes()
                .put(&marker, &key_value::kv_append_record(key, value))
                .map_err(map_error)?;
        }
        let delta = tx.commit_with_delta().map_err(map_error)?;
        host.committed_change(volume.id, DatabaseId::Entry(entry), delta);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_kv_delete(entry: u32, key: u32, length: u32) -> i32 {
    super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::KvDelete)?;
        let key = memory.admit(key, length, 1, 1, abi::MAX_KEY_BYTES as u32)?.bytes(memory.data);
        key_value::validate_key(key).map_err(store_error)?;
        let database = host.kv_backing(&volume, entry, Permission::Write)?;
        if database.begin_read().map_err(map_error)?.value_len(key).map_err(map_error)?.is_none() {
            return Ok(());
        }
        let mut tx = database
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        tx.del(key).map_err(map_error)?;
        let delta = tx.commit_with_delta().map_err(map_error)?;
        host.committed_change(volume.id, DatabaseId::Entry(entry), delta);
        Ok(())
    })
}
