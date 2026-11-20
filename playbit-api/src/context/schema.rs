//! Borrowed schema declarations and checked views over caller-owned results.
use super::{Context, Error, PhantomData, ReadError, buffer, range, status};
use crate::abi::{data, schema as ffi, syscall::Id};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    Text,
    Boolean,
    Integer,
    Float,
    Bytes,
    Reference(Id),
    File,
}
fn kind(value: ValueType) -> (u32, Id) {
    (
        match value {
            ValueType::Text => data::TEXT,
            ValueType::Boolean => data::BOOLEAN,
            ValueType::Integer => data::INTEGER,
            ValueType::Float => data::FLOAT,
            ValueType::Bytes => data::BYTES,
            ValueType::File => data::FILE,
            ValueType::Reference(id) => return (data::REFERENCE, id),
        },
        [0; 16],
    )
}
fn value_type(kind: u32, id: Id) -> Result<ValueType, Error> {
    if kind != data::REFERENCE && id != [0; 16] {
        return Err(Error::Internal);
    }
    Ok(match kind {
        data::TEXT => ValueType::Text,
        data::BOOLEAN => ValueType::Boolean,
        data::INTEGER => ValueType::Integer,
        data::FLOAT => ValueType::Float,
        data::BYTES => ValueType::Bytes,
        data::FILE => ValueType::File,
        data::REFERENCE => ValueType::Reference(id),
        _ => return Err(Error::Internal),
    })
}
fn text(value: &str) -> Result<ffi::Text, Error> {
    Ok(ffi::Text {
        data: value.as_ptr(),
        length: u32::try_from(value.len()).map_err(|_| Error::LimitExceeded)?,
        reserved: 0,
    })
}
#[repr(transparent)]
pub struct SchemaField<'a> {
    raw: ffi::FieldInput,
    _borrow: PhantomData<&'a str>,
}
impl<'a> SchemaField<'a> {
    pub fn new(
        id: Id,
        name: &'a str,
        value_type: ValueType,
        repeated: bool,
        deprecated: bool,
    ) -> Result<Self, Error> {
        let (kind, reference_type) = kind(value_type);
        Ok(Self {
            raw: ffi::FieldInput {
                size: core::mem::size_of::<ffi::FieldInput>() as u32,
                written: 0,
                id,
                name_data: text(name)?.data,
                name_length: text(name)?.length,
                kind,
                reference_type,
                flags: u32::from(deprecated) | (u32::from(repeated) << 1),
            },
            _borrow: PhantomData,
        })
    }
}
#[repr(transparent)]
pub struct SchemaType<'a> {
    raw: ffi::TypeInput,
    _borrow: PhantomData<(&'a str, &'a [SchemaField<'a>])>,
}
impl<'a> SchemaType<'a> {
    pub fn new(
        id: Id,
        name: &'a str,
        fields: &'a [SchemaField<'a>],
        deprecated: bool,
    ) -> Result<Self, Error> {
        Ok(Self {
            raw: ffi::TypeInput {
                size: core::mem::size_of::<ffi::TypeInput>() as u32,
                written: 0,
                id,
                name_data: text(name)?.data,
                name_length: text(name)?.length,
                fields: fields.as_ptr().cast(),
                count: u32::try_from(fields.len()).map_err(|_| Error::LimitExceeded)?,
                stride: core::mem::size_of::<ffi::FieldInput>() as u32,
                flags: u32::from(deprecated),
            },
            _borrow: PhantomData,
        })
    }
}
pub struct Schema<'a> {
    types: &'a [ffi::Type],
    fields: &'a [ffi::Field],
    bytes: &'a [u8],
}
pub struct TypeDefinition<'a> {
    pub id: Id,
    pub name: &'a str,
    pub deprecated: bool,
    fields: &'a [ffi::Field],
    bytes: &'a [u8],
}
pub struct FieldDefinition<'a> {
    pub id: Id,
    pub name: &'a str,
    pub value_type: ValueType,
    pub repeated: bool,
    pub deprecated: bool,
}
fn name(data: &[u8], r: data::Range) -> Result<&str, Error> {
    core::str::from_utf8(range(data, r)?).map_err(|_| Error::Internal)
}
impl<'a> Schema<'a> {
    pub fn types(&self) -> impl ExactSizeIterator<Item = TypeDefinition<'a>> + '_ {
        self.types.iter().map(|ty| TypeDefinition {
            id: ty.id,
            name: name(self.bytes, ty.name()).expect("validated type name"),
            deprecated: ty.flags == 1,
            fields: range(self.fields, ty.fields()).expect("validated fields"),
            bytes: self.bytes,
        })
    }
}
impl<'a> TypeDefinition<'a> {
    pub fn fields(&self) -> impl ExactSizeIterator<Item = FieldDefinition<'a>> + '_ {
        self.fields.iter().map(|field| FieldDefinition {
            id: field.id,
            name: name(self.bytes, field.name()).expect("validated field name"),
            value_type: value_type(field.kind, field.reference_type).expect("validated value type"),
            repeated: field.flags & 2 != 0,
            deprecated: field.flags & 1 != 0,
        })
    }
}
impl Context<'_> {
    pub fn begin_read_schema(
        &mut self,
        volume: super::EntryHandle,
    ) -> Result<super::OperationHandle, Error> {
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe { ffi::pb_schema_read_begin(self.raw.as_ptr(), &volume) };
        super::pending::admitted(result)
    }
    pub fn begin_establish_schema(
        &mut self,
        volume: super::EntryHandle,
        types: &[SchemaType<'_>],
    ) -> Result<super::OperationHandle, Error> {
        let count = u32::try_from(types.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_schema_establish_begin(
                self.raw.as_ptr(),
                &volume,
                types.as_ptr().cast(),
                count,
                core::mem::size_of::<ffi::TypeInput>() as u32,
            )
        };
        super::pending::admitted(result)
    }
    pub fn take_establish_schema(
        &mut self,
        pending: super::OperationHandle,
    ) -> Result<super::Completion<()>, Error> {
        // SAFETY: the valid borrowed context is retained for this bounded take.
        let result = unsafe { ffi::pb_schema_establish_take(self.raw.as_ptr(), pending) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        Ok(super::Completion::Ready(()))
    }
    pub fn take_read_schema<'a>(
        &mut self,
        pending: super::OperationHandle,
        types: &'a mut [ffi::Type],
        fields: &'a mut [ffi::Field],
        bytes: &'a mut [u8],
    ) -> Result<super::Completion<Schema<'a>>, ReadError> {
        let mut output = ffi::Schema::new(buffer(types)?, buffer(fields)?, buffer(bytes)?);
        // SAFETY: initialized outputs have mutually exclusive mutable borrows.
        let result = unsafe { ffi::pb_schema_read_take(self.raw.as_ptr(), pending, &mut output) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        decode_result(result, output, types, fields, bytes).map(super::Completion::Ready)
    }
}

fn decode_result<'a>(
    result: i64,
    output: ffi::Schema,
    types: &'a mut [ffi::Type],
    fields: &'a mut [ffi::Field],
    bytes: &'a mut [u8],
) -> Result<Schema<'a>, ReadError> {
    if result == Error::BufferTooSmall as i64 {
        return Err(ReadError::BufferTooSmall {
            records: output.types_count as usize,
            values: output.fields_count as usize,
            bytes: output.bytes_count as usize,
        });
    }
    status(result)?;
    let types = types.get(..output.types_count as usize).ok_or(Error::Internal)?;
    let fields = fields.get(..output.fields_count as usize).ok_or(Error::Internal)?;
    let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
    for ty in types {
        if ty.flags > 1 || ty.reserved != 0 {
            return Err(Error::Internal.into());
        }
        name(bytes, ty.name())?;
        range(fields, ty.fields())?;
    }
    for field in fields {
        if field.flags & !3 != 0 {
            return Err(Error::Internal.into());
        }
        name(bytes, field.name())?;
        value_type(field.kind, field.reference_type)?;
    }
    Ok(Schema { types, fields, bytes })
}
