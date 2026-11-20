//! OPFS results cross the host boundary: validate lengths before trusting them.
//! Value copies and sync staging are bounded data-plane operations.
use kvdb::{
    Error, Storage, StorageTxn, ValueRead,
    storage::{Retention, StorageProps},
};

#[link(wasm_import_module = "opfs")]
unsafe extern "C" {
    fn close_database(database: u32);
    fn begin(database: u32, write: u32) -> i32;
    fn get(tx: u32, key: *const u8, key_len: usize, out: *mut u8, capacity: usize) -> i32;
    fn next(
        tx: u32,
        key: *const u8,
        key_len: usize,
        exclusive: u32,
        reverse: u32,
        out: *mut u8,
        capacity: usize,
    ) -> i32;
    fn value_length(tx: u32, key: *const u8, key_len: usize) -> i32;
    fn read_value(
        tx: u32,
        key: *const u8,
        key_len: usize,
        offset: usize,
        out: *mut u8,
        capacity: usize,
    ) -> i32;
    fn next_key(
        tx: u32,
        key: *const u8,
        key_len: usize,
        exclusive: u32,
        reverse: u32,
        out: *mut u8,
        capacity: usize,
    ) -> i32;
    fn put(tx: u32, key: *const u8, key_len: usize, value: *const u8, value_len: usize) -> i32;
    fn put_start(tx: u32, key: *const u8, key_len: usize, value_len: usize) -> i32;
    fn put_part(tx: u32, value: *const u8, value_len: usize) -> i32;
    fn put_finish(tx: u32) -> i32;
    fn delete(tx: u32, key: *const u8, key_len: usize) -> i32;
    fn end(tx: u32, commit: u32) -> i32;
    fn pin(tx: u32, key: *const u8, key_len: usize) -> i32;
    fn reader_length(reader: u32) -> i32;
    fn reader_read(reader: u32, offset: usize, out: *mut u8, capacity: usize) -> i32;
    fn reader_end(reader: u32) -> i32;
    fn stage_begin(slot: u32) -> i32;
    fn stage_clear(slot: u32) -> i32;
    fn stage_append(slot: u32, input: *const u8, length: usize) -> i32;
    fn stage_read(slot: u32, offset: usize, out: *mut u8, capacity: usize) -> i32;
    fn stage_end(slot: u32) -> i32;
}
const MAX_READ_BYTES: usize = 2 * 1024 * 1024;

thread_local! {
    static REVISION: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}
/// Delivery fences inspect memory only. Every attempted durable write and
/// replica pointer replacement retires earlier permission qualifications.
pub(super) fn revision() -> u64 {
    REVISION.with(std::cell::Cell::get)
}
pub(super) fn invalidate() {
    REVISION.with(|revision| {
        revision.set(
            revision
                .get()
                .checked_add(1)
                .expect("browser storage revision cannot wrap during a worker lifetime"),
        )
    });
}

fn checked(result: i32) -> Result<usize, Error> {
    if result < 0 { Err(Error::Storage(result)) } else { Ok(result as usize) }
}
fn optional(result: i32) -> Result<Option<usize>, Error> {
    if result == -1 { Ok(None) } else { checked(result).map(Some) }
}

pub struct OpfsStorage(pub u32);
impl Drop for OpfsStorage {
    fn drop(&mut self) {
        unsafe {
            close_database(self.0);
        }
    }
}
pub struct Transaction {
    id: u32,
    write: bool,
    finished: bool,
}
pub struct SnapshotValue {
    id: u32,
    len: usize,
}
impl ValueRead for SnapshotValue {
    fn len(&self) -> usize {
        self.len
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize, Error> {
        if offset > self.len {
            return Err(Error::InvalidArgument);
        }
        let count = checked(unsafe { reader_read(self.id, offset, out.as_mut_ptr(), out.len()) })?;
        if count > out.len() || count > self.len - offset {
            return Err(Error::Corrupt);
        }
        Ok(count)
    }
}
impl Drop for SnapshotValue {
    fn drop(&mut self) {
        unsafe {
            reader_end(self.id);
        }
    }
}

pub struct OpfsStaging {
    slot: u32,
    length: usize,
}
impl OpfsStaging {
    pub fn new(slot: u32) -> Result<Self, Error> {
        checked(unsafe { stage_begin(slot) })?;
        Ok(Self { slot, length: 0 })
    }
}
impl ValueRead for OpfsStaging {
    fn len(&self) -> usize {
        self.length
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize, Error> {
        if offset > self.length {
            return Err(Error::InvalidArgument);
        }
        let count = checked(unsafe { stage_read(self.slot, offset, out.as_mut_ptr(), out.len()) })?;
        if count > out.len() || count > self.length - offset {
            return Err(Error::Corrupt);
        }
        Ok(count)
    }
}
impl kvdb::SyncStaging for OpfsStaging {
    fn clear(&mut self) -> Result<(), Error> {
        checked(unsafe { stage_clear(self.slot) })?;
        self.length = 0;
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let length = self.length.checked_add(bytes.len()).ok_or(Error::Capacity)?;
        if length > kvdb::sync::MAX_FRAME {
            return Err(Error::Capacity);
        }
        checked(unsafe { stage_append(self.slot, bytes.as_ptr(), bytes.len()) })?;
        self.length = length;
        Ok(())
    }
}
impl Drop for OpfsStaging {
    fn drop(&mut self) {
        unsafe {
            stage_end(self.slot);
        }
    }
}

impl Storage for OpfsStorage {
    type Txn<'db> = Transaction;
    fn begin(&self, write: bool) -> Result<Transaction, Error> {
        let id = checked(unsafe { begin(self.0, u32::from(write)) })? as u32;
        Ok(Transaction { id, write, finished: false })
    }
    fn props(&self) -> StorageProps {
        StorageProps {
            retention: Retention::BestEffort,
            durable_program_crash: true,
            durable_power_loss: false,
        }
    }
}

// Ordinary borrowed-guard APIs materialize their explicitly requested result.
// Full sync uses bounded ranges and pinned readers.
fn read(mut call: impl FnMut(*mut u8, usize) -> i32) -> Result<Option<Vec<u8>>, Error> {
    let Some(length) = optional(call(std::ptr::null_mut(), 0))? else {
        return Ok(None);
    };
    if length > MAX_READ_BYTES {
        return Err(Error::Capacity);
    }
    let mut bytes = vec![0; length];
    if checked(call(bytes.as_mut_ptr(), bytes.len()))? != length {
        return Err(Error::Corrupt);
    }
    Ok(Some(bytes))
}
type KeyValue = (Vec<u8>, Vec<u8>);
impl Transaction {
    fn adjacent(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
    ) -> Result<Option<KeyValue>, Error> {
        let Some(bytes) = read(|out, len| unsafe {
            next(
                self.id,
                key.as_ptr(),
                key.len(),
                u32::from(exclusive),
                u32::from(reverse),
                out,
                len,
            )
        })?
        else {
            return Ok(None);
        };
        let count = bytes.get(..4).ok_or(Error::Corrupt)?;
        let length = u32::from_le_bytes(
            count.try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        let end = length.checked_add(4).ok_or(Error::Corrupt)?;
        if end > bytes.len() {
            return Err(Error::Corrupt);
        }
        Ok(Some((bytes[4..end].to_vec(), bytes[end..].to_vec())))
    }
    fn adjacent_key(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
    ) -> Result<Option<Vec<u8>>, Error> {
        let mut bytes = [0; kvdb::KEY_SIZE_MAX + 1];
        let length = optional(unsafe {
            next_key(
                self.id,
                key.as_ptr(),
                key.len(),
                u32::from(exclusive),
                u32::from(reverse),
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        })?;
        length
            .map(|length| bytes.get(..length).map(<[u8]>::to_vec).ok_or(Error::Corrupt))
            .transpose()
    }
}
impl StorageTxn for Transaction {
    type Guard<'g> = Vec<u8>;
    type ValueReader = SnapshotValue;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<SnapshotValue>, Error> {
        if self.write {
            return Err(Error::Mode);
        }
        let Some(id) = optional(unsafe { pin(self.id, key.as_ptr(), key.len()) })? else {
            return Ok(None);
        };
        let mut value = SnapshotValue { id: id as u32, len: 0 };
        value.len = checked(unsafe { reader_length(value.id) })?;
        Ok(Some(value))
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        read(|out, len| unsafe { get(self.id, key.as_ptr(), key.len(), out, len) })
    }
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>, Error> {
        optional(unsafe { value_length(self.id, key.as_ptr(), key.len()) })
    }
    fn read_value_at(
        &self,
        key: &[u8],
        offset: usize,
        out: &mut [u8],
    ) -> Result<Option<usize>, Error> {
        let count = optional(unsafe {
            read_value(self.id, key.as_ptr(), key.len(), offset, out.as_mut_ptr(), out.len())
        })?;
        if count.is_some_and(|count| count > out.len()) {
            return Err(Error::Corrupt);
        }
        Ok(count)
    }
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>, Error> {
        self.adjacent_key(key, exclusive, false)
    }
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>, Error> {
        self.adjacent_key(key, exclusive, true)
    }
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut kvdb::storage::ValueVisitor<'_>,
    ) -> Result<(), Error> {
        struct BorrowedValue<'a> {
            transaction: &'a Transaction,
            key: &'a [u8],
            len: usize,
        }
        impl ValueRead for BorrowedValue<'_> {
            fn len(&self) -> usize {
                self.len
            }
            fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize, Error> {
                if offset > self.len {
                    return Err(Error::InvalidArgument);
                }
                self.transaction.read_value_at(self.key, offset, out)?.ok_or(Error::Corrupt)
            }
        }
        // OPFS owns the ordered index across the worker boundary. Retain only
        // one key here and stream the requested value range; no whole-value copy.
        let mut current = self.adjacent_key(key, exclusive, reverse)?;
        while let Some(key) = current {
            let value = BorrowedValue {
                transaction: self,
                key: &key,
                len: self.value_len(&key)?.ok_or(Error::Corrupt)?,
            };
            if !visitor(&key, &value)? {
                return Ok(());
            }
            current = self.adjacent_key(&key, true, reverse)?;
        }
        Ok(())
    }
    fn next_from(&self, key: &[u8], exclusive: bool) -> Result<Option<KeyValue>, Error> {
        self.adjacent(key, exclusive, false)
    }
    fn prev_from(&self, key: &[u8], exclusive: bool) -> Result<Option<KeyValue>, Error> {
        self.adjacent(key, exclusive, true)
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        checked(unsafe { put(self.id, key.as_ptr(), key.len(), value.as_ptr(), value.len()) })?;
        Ok(())
    }
    fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> Result<(), Error> {
        checked(unsafe { put_start(self.id, key.as_ptr(), key.len(), value.len()) })?;
        let mut bytes = [0; 4096];
        let mut offset = 0;
        while offset < value.len() {
            let n = bytes.len().min(value.len() - offset);
            value.read_exact_at(offset, &mut bytes[..n])?;
            checked(unsafe { put_part(self.id, bytes.as_ptr(), n) })?;
            offset += n;
        }
        checked(unsafe { put_finish(self.id) })?;
        Ok(())
    }
    fn del(&mut self, key: &[u8]) -> Result<bool, Error> {
        Ok(checked(unsafe { delete(self.id, key.as_ptr(), key.len()) })? == 1)
    }
    fn commit(mut self) -> Result<(), Error> {
        invalidate();
        self.finished = true;
        checked(unsafe { end(self.id, 1) })?;
        Ok(())
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.finished {
            unsafe {
                end(self.id, 0);
            }
        }
    }
}
