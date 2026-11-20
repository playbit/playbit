//! Borrowed authenticated fields retain revision and provenance with their values.
use super::{Context, Error, Field, buffer, data::value, status};
use crate::abi::{authenticated as ffi, data, syscall::Id};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Principal {
    Human(Id),
    Service(Id),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provenance {
    Unproven,
    ApplicationEdit {
        principal: Principal,
        device_id: Id,
    },
    /// Identifies the rekey writer, not the original application author.
    Rekey {
        principal: Principal,
        device_id: Id,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticatedReadError {
    Host(Error),
    BufferTooSmall { values: usize, bytes: usize },
}
impl From<Error> for AuthenticatedReadError {
    fn from(error: Error) -> Self {
        Self::Host(error)
    }
}
pub struct AuthenticatedField<'a> {
    pub volume_id: Id,
    pub database_id: [u8; 15],
    pub object_id: Id,
    pub type_id: Id,
    pub revision: [u8; 32],
    pub provenance: Provenance,
    pub field: Field<'a>,
}
fn provenance(m: &ffi::Metadata) -> Result<Provenance, Error> {
    if m.provenance == 0 {
        if m.principal_kind != 0 || m.principal_id != [0; 16] || m.device_id != [0; 16] {
            return Err(Error::Internal);
        }
        return Ok(Provenance::Unproven);
    }
    if m.principal_id == [0; 16] || m.device_id == [0; 16] {
        return Err(Error::Internal);
    }
    let principal = match m.principal_kind {
        1 => Principal::Human(m.principal_id),
        2 => Principal::Service(m.principal_id),
        _ => return Err(Error::Internal),
    };
    match m.provenance {
        1 => Ok(Provenance::ApplicationEdit { principal, device_id: m.device_id }),
        2 => Ok(Provenance::Rekey { principal, device_id: m.device_id }),
        _ => Err(Error::Internal),
    }
}
impl Context<'_> {
    pub fn begin_read_authenticated_field(
        &mut self,
        volume: super::EntryHandle,
        object: Id,
        field: Id,
    ) -> Result<super::OperationHandle, Error> {
        // SAFETY: borrowed ID inputs remain live and stable for this call.
        let result = unsafe {
            ffi::pb_object_field_read_authenticated_begin(
                self.raw.as_ptr(),
                &volume,
                &object,
                &field,
            )
        };
        super::pending::admitted(result)
    }

    /// Short output retains the same atomic snapshot. Ready(None) means no stored
    /// revision; field.values()==None is a stored tombstone.
    pub fn take_authenticated_field<'a>(
        &mut self,
        handle: super::OperationHandle,
        values: &'a mut [data::Value],
        bytes: &'a mut [u8],
    ) -> Result<super::Completion<Option<AuthenticatedField<'a>>>, AuthenticatedReadError> {
        let mut metadata = ffi::Metadata::default();
        let values_buffer = buffer(values)?;
        let bytes_buffer = buffer(bytes)?;
        let mut output = ffi::Field {
            size: core::mem::size_of::<ffi::Field>() as u32,
            written: 0,
            metadata: &mut metadata,
            values_data: values_buffer.data,
            values_capacity: values_buffer.capacity,
            values_count: 0,
            values_stride: values_buffer.stride,
            bytes_data: bytes_buffer.data,
            bytes_capacity: bytes_buffer.capacity,
            bytes_count: 0,
        };
        // SAFETY: the initialized descriptor and output slices are independent borrows.
        let result = unsafe {
            ffi::pb_object_field_read_authenticated_take(self.raw.as_ptr(), handle, &mut output)
        };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        if result == Error::BufferTooSmall as i64 {
            return Err(AuthenticatedReadError::BufferTooSmall {
                values: output.values_count as usize,
                bytes: output.bytes_count as usize,
            });
        }
        status(result)?;
        let values = values.get(..output.values_count as usize).ok_or(Error::Internal)?;
        let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
        let mut m = metadata;
        m.written = 0;
        if m.state == 0 {
            if m != ffi::Metadata::default() || !values.is_empty() || !bytes.is_empty() {
                return Err(Error::Internal.into());
            }
            return Ok(super::Completion::Ready(None));
        }
        if !matches!(m.state, 1 | 2)
            || m.reserved != 0
            || m.object_id == [0; 16]
            || m.field_id == [0; 16]
            || m.type_id == [0; 16]
            || (m.state == 1 && (!values.is_empty() || !bytes.is_empty()))
        {
            return Err(Error::Internal.into());
        }
        let provenance = provenance(&m)?;
        for v in values {
            value(v, bytes)?;
        }
        Ok(super::Completion::Ready(Some(AuthenticatedField {
            volume_id: m.volume_id,
            database_id: m.entry_id,
            object_id: m.object_id,
            type_id: m.type_id,
            revision: m.revision,
            provenance,
            field: Field { id: m.field_id, values: (m.state == 2).then_some(values), bytes },
        })))
    }
}
