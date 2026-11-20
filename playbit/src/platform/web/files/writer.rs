//! Private writer state. JavaScript owns OPFS handles; Rust owns admission,
//! failed-writer state, destination validation and publication outcomes.
use super::*;
use super::{destination::Destination, objects::Object};
use crate::runtime::host::files::{Action, Output};
use playbit_api::abi::files::*;

pub(super) struct StageCredit(Rc<Cell<usize>>);
impl StageCredit {
    pub(super) fn reserve(count: &Rc<Cell<usize>>) -> Result<Rc<Self>> {
        if count.get() >= 8 {
            return Err(Error::LimitExceeded);
        }
        count.set(count.get() + 1);
        Ok(Rc::new(Self(count.clone())))
    }
}
impl Drop for StageCredit {
    fn drop(&mut self) {
        assert!(self.0.get() > 0, "last private stage releases its reservation");
        self.0.set(self.0.get() - 1);
    }
}
pub(super) struct Writer {
    pub(super) target: Destination,
    pub(super) size: Cell<u64>,
    pub(super) failed: Cell<bool>,
    pub(super) _credit: Rc<StageCredit>,
}
impl Writer {
    pub(super) fn check(&self, host: &Host) -> Result<()> {
        self.target.staging_access(host)?;
        if self.failed.get() {
            return Err(Error::Closed);
        }
        Ok(())
    }
}
fn source(pending: &pending::Pending) -> Result<&Writer> {
    let Object::Writer(writer) = pending.object.as_ref() else {
        return Err(Error::InvalidArgument);
    };
    Ok(writer)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_writer_ready(token: u64) -> i32 {
    operations::call(|host, _| {
        let mut files = host.files.borrow_mut();
        files.check(token, false)?;
        let pending = files.pending.lookup_mut(&token).expect("checked pending operation");
        if !pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        let (engine, object) = pending.created.as_ref().ok_or(Error::InvalidArgument)?;
        let Object::Writer(writer) = object.as_ref() else {
            return Err(Error::InvalidArgument);
        };
        writer.check(host)?;
        pending.output = Some(Output::File { handle: *engine });
        pending.terminal = true;
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_write_complete(token: u64, status: i32) -> i32 {
    operations::call(|host, _| {
        if status != 0 && (Error::try_from(status).is_err() || matches!(status, -6 | -11)) {
            return Err(Error::InvalidArgument);
        }
        let mut files = host.files.borrow_mut();
        let pending = files.pending.lookup_mut(&token).ok_or(Error::StaleHandle)?;
        if !pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        let Action::Write { offset, bytes } = &pending.action else {
            return Err(Error::InvalidArgument);
        };
        let writer = source(pending)?;
        if status == 0 {
            // The worker reports success only after a full-request OPFS write.
            if !bytes.is_empty() {
                writer.size.set(writer.size.get().max(offset + bytes.len() as u64));
            }
            pending.output = Some(Output::Written);
        } else {
            writer.failed.set(true);
        }
        pending.terminal = true;
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_seal(token: u64, length: u64) -> i32 {
    let mut output_length = 0;
    let status = operations::call(|host, memory| {
        let digest = memory.admit(8, 32, 1, 1, 32)?.read(memory.data, 0);
        let mut files = host.files.borrow_mut();
        files.table.authorize_resource(
            &files.pending.lookup(&token).ok_or(Error::StaleHandle)?.source,
            true,
        )?;
        let pending = files.pending.lookup_mut(&token).ok_or(Error::StaleHandle)?;
        if !pending.started || pending.terminal || pending.sealed.is_some() {
            return Err(Error::StaleHandle);
        }
        if !matches!(pending.action, Action::Close { flags } if flags != FILE_CLOSE_DISCARD) {
            return Err(Error::InvalidArgument);
        }
        let writer = source(pending)?;
        writer.check(host)?;
        if writer.size.get() != length || length > MAX_FILE_EXTENT {
            return Err(Error::InvalidArgument);
        }
        let reference = writer.target.reference(host, length, digest)?;
        let output = serde_json::to_vec(&reference).map_err(map_error)?;
        pending.sealed = Some(reference);
        output_length = output.len() as i32;
        *OUTPUT.lock().expect("mutex is not poisoned") = output;
        Ok(())
    });
    if status == 0 { output_length } else { status }
}
fn failure(
    host: &Host,
    pending: &mut pending::Pending,
    error: Error,
    uncertain: bool,
    objects: &mut HandleTable<Rc<Object>>,
) -> Result<Output> {
    let writer = source(pending)?;
    let recover = !uncertain && writer.check(host).is_ok();
    let recovery = if recover {
        let engine = pending.source.engine.ok_or(Error::Internal)?;
        let slot = pending.created_slot.ok_or(Error::Internal)?;
        let object = Rc::new(Object::Recovery { writer: pending.object.clone(), engine });
        objects.insert(slot, object.clone()).expect("close owns its recovery backend reservation");
        pending.created = Some((slot.raw(), object));
        Some(slot.raw())
    } else {
        None
    };
    Ok(Output::Close { error: error as i32, publication: if uncertain { 2 } else { 0 }, recovery })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_close_failed(token: u64, status: i32) -> i32 {
    operations::call(|host, _| {
        let error = Error::try_from(status).map_err(|_| Error::InvalidArgument)?;
        if matches!(error, Error::WouldBlock | Error::BufferTooSmall) {
            return Err(Error::InvalidArgument);
        }
        let mut files = host.files.borrow_mut();
        let Files { pending: pending_table, objects, .. } = &mut *files;
        let pending = pending_table.lookup_mut(&token).ok_or(Error::StaleHandle)?;
        if !pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        if !matches!(pending.action, Action::Close { flags } if flags != FILE_CLOSE_DISCARD) {
            return Err(Error::InvalidArgument);
        }
        pending.output = Some(failure(host, pending, error, false, objects)?);
        pending.terminal = true;
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_publish(token: u64, length: u32) -> i32 {
    operations::call(|host, memory| {
        let input = memory.admit(8, length, 1, 1, 1024)?;
        let reference: blob::FileReference =
            serde_json::from_slice(input.bytes(memory.data)).map_err(map_error)?;
        let mut files = host.files.borrow_mut();
        let authority = files.table.authorize_resource(
            &files.pending.lookup(&token).ok_or(Error::StaleHandle)?.source,
            true,
        );
        let Files { pending: pending_table, objects, .. } = &mut *files;
        let pending = pending_table.lookup_mut(&token).ok_or(Error::StaleHandle)?;
        if !pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        let sealed = pending.sealed.as_ref().ok_or(Error::InvalidArgument)?;
        let mut expected = sealed.clone();
        expected.encrypted_key = reference.encrypted_key.clone();
        if reference != expected {
            return Err(Error::InvalidArgument);
        }
        let writer = source(pending)?;
        let result = authority
            .and_then(|()| writer.check(host))
            .map_err(|error| (error, false))
            .and_then(|()| writer.target.publish(host, &reference));
        pending.output = Some(match result {
            Ok(()) => Output::Close { error: 0, publication: 1, recovery: None },
            Err((error, uncertain)) => failure(host, pending, error, uncertain, objects)?,
        });
        pending.terminal = true;
        Ok(())
    })
}

/// Enumerate only stages whose File and accepted-operation owners have retired.
/// Cleanup does not release quota until the worker closes and removes the spool.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_cleanup() -> i32 {
    let mut length = 0;
    let status = operations::call(|host, _| {
        let files = host.files.borrow();
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        for (engine, stage) in &files.staged {
            if Rc::strong_count(stage) == 1 {
                output.extend_from_slice(&engine.to_le_bytes());
            }
        }
        length = output.len() as i32;
        Ok(())
    });
    if status == 0 { length } else { status }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_cleaned(engine: u64) -> i32 {
    operations::call(|host, _| {
        let mut files = host.files.borrow_mut();
        let stage = files.staged.get(&engine).ok_or(Error::StaleHandle)?;
        if Rc::strong_count(stage) != 1 {
            return Err(Error::Busy);
        }
        files.staged.remove(&engine);
        Ok(())
    })
}
