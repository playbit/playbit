//! Owned asynchronous KV calls; take borrows caller storage, never host memory.
use super::{Context, Error, buffer, status};
use crate::abi::{data::EntryHandle, kv as ffi, pending::OperationHandle};

fn key(bytes: &[u8]) -> Result<u32, Error> {
    if bytes.is_empty() || bytes.len() > ffi::MAX_KEY_BYTES {
        return Err(Error::InvalidArgument);
    }
    Ok(bytes.len() as u32)
}

impl Context<'_> {
    pub fn begin_kv_keys(
        &mut self,
        entry: EntryHandle,
        after: Option<&[u8]>,
        root: Option<ffi::Root>,
        limit: u32,
    ) -> Result<OperationHandle, Error> {
        if !entry.valid()
            || limit == 0
            || limit as usize > ffi::MAX_KEYS
            || (after.is_some() && root.is_none())
        {
            return Err(Error::InvalidArgument);
        }
        let length = after.map(key).transpose()?.unwrap_or(0);
        let query = ffi::Query {
            size: core::mem::size_of::<ffi::Query>() as u32,
            written: 0,
            root_level: root.unwrap_or_default().level,
            root_digest: root.unwrap_or_default().digest,
            has_root: u32::from(root.is_some()),
            limit,
            reserved: 0,
        };
        ffi::root(
            query.has_root,
            ffi::Root { level: query.root_level, digest: query.root_digest },
        )?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_kv_keys_begin(
                self.raw.as_ptr(),
                &entry,
                &query,
                after.unwrap_or_default().as_ptr(),
                length,
            )
        };
        super::pending::admitted(result)
    }

    pub fn begin_kv_read(
        &mut self,
        entry: EntryHandle,
        bytes: &[u8],
        offset: u64,
        limit: u32,
        root: Option<ffi::Root>,
    ) -> Result<OperationHandle, Error> {
        let length = key(bytes)?;
        if !entry.valid()
            || limit == 0
            || limit as usize > ffi::MAX_VALUE_BYTES
            || (offset != 0 && root.is_none())
        {
            return Err(Error::InvalidArgument);
        }
        let query = ffi::Read {
            size: core::mem::size_of::<ffi::Read>() as u32,
            written: 0,
            offset,
            limit,
            has_root: u32::from(root.is_some()),
            root_level: root.unwrap_or_default().level,
            root_digest: root.unwrap_or_default().digest,
            reserved: 0,
        };
        ffi::root(
            query.has_root,
            ffi::Root { level: query.root_level, digest: query.root_digest },
        )?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_kv_read_begin(self.raw.as_ptr(), &entry, bytes.as_ptr(), length, &query)
        };
        super::pending::admitted(result)
    }

    pub fn begin_kv_put(
        &mut self,
        entry: EntryHandle,
        bytes: &[u8],
        value: &[u8],
    ) -> Result<OperationHandle, Error> {
        let length = key(bytes)?;
        if !entry.valid() {
            return Err(Error::InvalidArgument);
        }
        if value.len() > ffi::MAX_VALUE_BYTES {
            return Err(Error::LimitExceeded);
        }
        // SAFETY: begin copies bounded supplied key/value bytes before return.
        let result = unsafe {
            ffi::pb_kv_put_begin(
                self.raw.as_ptr(),
                &entry,
                bytes.as_ptr(),
                length,
                value.as_ptr(),
                value.len() as u32,
            )
        };
        super::pending::admitted(result)
    }

    pub fn begin_kv_delete(
        &mut self,
        entry: EntryHandle,
        bytes: &[u8],
    ) -> Result<OperationHandle, Error> {
        let length = key(bytes)?;
        if !entry.valid() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: the key is borrowed only until its owned operation is admitted.
        let result =
            unsafe { ffi::pb_kv_delete_begin(self.raw.as_ptr(), &entry, bytes.as_ptr(), length) };
        super::pending::admitted(result)
    }

    pub fn take_kv_keys<'a>(
        &mut self,
        handle: OperationHandle,
        bytes: &'a mut [u8],
    ) -> Result<super::Completion<ffi::Page<'a>>, Error> {
        let mut output = buffer(bytes)?;
        // SAFETY: descriptor and caller-owned byte storage are disjoint/live.
        let result = unsafe {
            ffi::pb_kv_keys_take(
                self.raw.as_ptr(),
                handle,
                output.data,
                output.capacity,
                &mut output.count,
            )
        };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        let bytes = bytes.get(..output.count as usize).ok_or(Error::Internal)?;
        Ok(super::Completion::Ready(ffi::decode_page(bytes)?))
    }

    pub fn take_kv_read<'a>(
        &mut self,
        handle: OperationHandle,
        bytes: &'a mut [u8],
    ) -> Result<super::Completion<ffi::Chunk<'a>>, Error> {
        let mut output = buffer(bytes)?;
        // SAFETY: the host writes only into these independently borrowed bytes.
        let result = unsafe {
            ffi::pb_kv_read_take(
                self.raw.as_ptr(),
                handle,
                output.data,
                output.capacity,
                &mut output.count,
            )
        };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        let bytes = bytes.get(..output.count as usize).ok_or(Error::Internal)?;
        Ok(super::Completion::Ready(ffi::decode_chunk(bytes)?))
    }

    pub fn take_kv_put(&mut self, handle: OperationHandle) -> Result<super::Completion<()>, Error> {
        // SAFETY: the handle belongs to this borrowed active context.
        let result = unsafe { ffi::pb_kv_put_take(self.raw.as_ptr(), handle) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        Ok(super::Completion::Ready(()))
    }

    pub fn take_kv_delete(
        &mut self,
        handle: OperationHandle,
    ) -> Result<super::Completion<()>, Error> {
        // SAFETY: only the matching retained acknowledgment can be consumed.
        let result = unsafe { ffi::pb_kv_delete_take(self.raw.as_ptr(), handle) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        Ok(super::Completion::Ready(()))
    }
}
