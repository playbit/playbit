//! Authorized append receipt and per-log gap recovery for initialized replicas.
use super::*;
use crate::store::{
    DatabasePolicy, ReplicaReader,
    bloblog::{Log, ReplicaPage},
    volume_index::{DatabaseProtocol, IndexSnapshot},
};
use databases::Address;

enum LogAccess {
    Read,
    Append,
    Replica,
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_bloblog_address(entry: u32) -> i32 {
    let status = super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::BloblogRead)?;
        let mut output = OUTPUT.lock().expect("output lock");
        output.clear();
        output.extend_from_slice(volume.id.as_bytes());
        output.extend_from_slice(entry.as_bytes());
        Ok(())
    });
    if status == 0 { 31 } else { status }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_bloblog_append(
    entry: u32,
    blobs: u32,
    count: u32,
    stride: u32,
    append_id: u32,
) -> i32 {
    let status = super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::BloblogAppend)?;
        let (volume, database, entry) = open(
            host,
            &Address { volume: volume.id, database: DatabaseId::Entry(entry) },
            LogAccess::Append,
        )?;
        let blobs = crate::runtime::linear_memory::input::bloblog_blobs(
            memory, blobs, count, stride, None,
        )?;
        if append_id == 0 {
            return Err(Error::InvalidArgument);
        }
        let append_id = Id::from_bytes(memory.admit(append_id, 16, 1, 1, 16)?.read(memory.data, 0));
        let cursor = Log {
            database: &*database,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: unsafe { now() } as u64,
        }
        .start_cursor();
        let entries = crate::store::bloblog::append_entries(
            host.account,
            volume.id,
            entry,
            append_id,
            &blobs,
        )
        .map_err(store_error)?;
        let batch = crate::store::bloblog::encode(&entries).map_err(store_error)?;
        let mut output = OUTPUT.lock().expect("output lock");
        output.clear();
        output.extend_from_slice(volume.id.as_bytes());
        output.extend_from_slice(entry.as_bytes());
        output.push(cursor.as_str().len() as u8);
        output.extend_from_slice(cursor.as_str().as_bytes());
        output.extend_from_slice(&batch);
        Ok(())
    });
    if status != 0 { status } else { OUTPUT.lock().expect("mutex is not poisoned").len() as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_bloblog_read(entry: u32, cursor: u32, length: u32) -> i32 {
    let status = super::operations::call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::BloblogRead)?;
        let (volume, database, entry) = open(
            host,
            &Address { volume: volume.id, database: DatabaseId::Entry(entry) },
            LogAccess::Read,
        )?;
        let bytes = memory
            .admit(cursor, length, 1, 1, playbit_api::abi::bloblog::MAX_CURSOR_BYTES as u32)?
            .bytes(memory.data);
        let cursor = if bytes.is_empty() {
            None
        } else {
            Some(
                crate::store::bloblog::Cursor::parse(playbit_api::abi::bloblog::cursor(bytes)?)
                    .map_err(store_error)?,
            )
        };
        let page = Log {
            database: &*database,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: unsafe { now() } as u64,
        }
        .get_committed(cursor.as_ref())
        .map_err(store_error)?;
        publish_guest(crate::runtime::host::DataResult::BloblogPage(page))
    });
    if status != 0 { status } else { OUTPUT.lock().expect("mutex is not poisoned").len() as i32 }
}

fn publish_guest(result: crate::runtime::host::DataResult) -> Result<()> {
    let length = result.bloblog_output_bytes()?;
    let mut output = OUTPUT.lock().expect("mutex is not poisoned");
    output.resize(length, 0);
    result.encode_bloblog(&mut output)?;
    Ok(())
}

fn open(
    host: &Host,
    address: &Address,
    access: LogAccess,
) -> Result<(Rc<Volume>, Rc<Database>, EntryId)> {
    let DatabaseId::Entry(entry) = address.database else {
        return Err(Error::InvalidArgument);
    };
    let volume = host.replica_volume(address.volume)?;
    let permission = match access {
        LogAccess::Append => Permission::Write,
        LogAccess::Read | LogAccess::Replica => Permission::Read,
    };
    host.authorize(&volume, Some(entry), permission)?;
    let policy = DatabasePolicy::read(&volume.index, address.database).map_err(store_error)?;
    if !policy.entry.as_ref().is_some_and(|entry| entry.protocol == Some(DatabaseProtocol::BlobLog))
    {
        return Err(Error::InvalidArgument);
    }
    match access {
        LogAccess::Append if !policy.volume.volume().share_key_with_service => {
            return Err(Error::AccessDenied);
        }
        LogAccess::Replica => {
            policy.authorize_reader(ReplicaReader::Service).map_err(store_error)?;
        }
        LogAccess::Read | LogAccess::Append => {}
    }
    if !IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?)
        .backing_ready(entry)
        .map_err(store_error)?
    {
        return Err(Error::WouldBlock);
    }
    let database = host.entry_database(&volume, entry)?;
    Ok((volume, database, entry))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_bloblog_cache_append(length: usize) -> i32 {
    call(length, 32 + 104 + crate::store::bloblog::MAX_BYTES, |host, bytes| {
        if bytes.len() < 36 {
            return Err(Error::InvalidArgument);
        }
        let volume = Id::from_bytes(bytes[..16].try_into().map_err(map_error)?);
        let entry = EntryId::from_bytes(bytes[16..31].try_into().map_err(map_error)?)
            .map_err(store_error)?;
        let offset = 32 + bytes[31] as usize;
        let batch = bytes.get(offset..).ok_or(Error::InvalidArgument)?;
        let entries = crate::store::bloblog::decode(batch).map_err(store_error)?;
        let (volume, database, entry) = open(
            host,
            &Address { volume, database: DatabaseId::Entry(entry) },
            LogAccess::Replica,
        )?;
        Log {
            database: &*database,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: unsafe { now() } as u64,
        }
        .apply(&entries, false)
        .map_err(store_error)?;
        Ok(Vec::new())
    })
}

fn call(length: usize, limit: usize, run: impl FnOnce(&Host, &[u8]) -> Result<Vec<u8>>) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, limit)?;
        let host = HOST.with(|cell| cell.borrow().clone()).ok_or(Error::Unavailable)?;
        run(&host, &input[..length])
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    address: Address,
    receipt: ReplicaPage,
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_bloblog_checkpoint(length: usize) -> i32 {
    call(length, 256, |host, bytes| {
        let address: Address = serde_json::from_slice(bytes).map_err(map_error)?;
        let (volume, database, entry) = open(host, &address, LogAccess::Replica)?;
        let cursor: Option<crate::store::bloblog::Cursor> = database
            .begin_read()
            .map_err(map_error)?
            .get(b"remote-cursor")
            .map_err(map_error)?
            .map(|value| crate::store::catalog::decode(&value))
            .transpose()
            .map_err(store_error)?;
        if let Some(cursor) = &cursor {
            Log {
                database: &*database,
                volume: volume.id,
                entry,
                encryption_key: None,
                now: unsafe { now() } as u64,
            }
            .validate_cursor(cursor)
            .map_err(store_error)?;
        }
        serde_json::to_vec(&cursor).map_err(map_error)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_bloblog_receive(length: usize) -> i32 {
    call(length, 384 * 1024, |host, bytes| {
        let page: Page = serde_json::from_slice(bytes).map_err(map_error)?;
        page.receipt.cut().map_err(store_error)?;
        let (volume, database, entry) = open(host, &page.address, LogAccess::Replica)?;
        Log {
            database: &*database,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: unsafe { now() } as u64,
        }
        .apply_page(&page.receipt.page)
        .map_err(store_error)?;
        Ok(Vec::new())
    })
}
pub(super) fn receive_inline(host: &Host, address: &Address, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > crate::store::bloblog::INLINE_BYTES {
        return Err(Error::InvalidArgument);
    }
    let (volume, database, entry) = open(host, address, LogAccess::Replica)?;
    let entries = crate::store::bloblog::decode(bytes).map_err(store_error)?;
    Log {
        database: &*database,
        volume: volume.id,
        entry,
        encryption_key: None,
        now: unsafe { now() } as u64,
    }
    .apply(&entries, false)
    .map_err(store_error)?;
    Ok(())
}
