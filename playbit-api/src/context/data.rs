//! Borrowed object inputs and caller-owned output storage. Volume roots are owned.
extern crate alloc;
use super::{Context, Error, PhantomData, buffer, range, status};
use crate::abi::{data as ffi, syscall::Id};

pub use ffi::VolumeHandle;
impl Context<'_> {
    /// Request one VolumesChanged event if roots change after this enumeration.
    /// A change already observed by the host is reported without a polling loop.
    pub fn observe_volumes(&mut self, revision: u64) -> Result<(), Error> {
        // SAFETY: Context owns the live host binding; no guest memory is borrowed.
        status(unsafe { ffi::pb_volumes_observe(self.raw.as_ptr(), revision) })
    }

    pub fn begin_resolve_entry(
        &mut self,
        volume: VolumeHandle,
        name: &str,
    ) -> Result<super::OperationHandle, Error> {
        if name.is_empty() || name.len() > crate::abi::files::MAX_FILE_PATH_BYTES {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_entry_resolve_begin(self.raw.as_ptr(), volume, name.as_ptr(), name.len() as u32)
        };
        super::pending::admitted(result)
    }

    pub fn take_resolve_entry(
        &mut self,
        handle: super::OperationHandle,
    ) -> Result<super::Completion<ffi::EntryHandle>, Error> {
        let mut output = ffi::EntryHandle::default();
        // SAFETY: the initialized output is exclusively borrowed for this take.
        let result = unsafe { ffi::pb_entry_resolve_take(self.raw.as_ptr(), handle, &mut output) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        if output.volume_handle().slot() == 0 || !output.valid() {
            return Err(Error::Internal);
        }
        Ok(super::Completion::Ready(output))
    }
}
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Value<'a> {
    raw: ffi::ValueInput,
    _borrow: PhantomData<&'a [u8]>,
}
impl<'a> Value<'a> {
    const fn scalar(kind: u32, scalar: u64) -> Self {
        Self {
            raw: ffi::ValueInput {
                size: core::mem::size_of::<ffi::ValueInput>() as u32,
                written: 0,
                kind,
                scalar,
                length: 0,
                id1: [0; 16],
                id2: [0; 16],
                entry_id: [0; 15],
                reserved: 0,
                data: core::ptr::null(),
            },
            _borrow: PhantomData,
        }
    }
    pub const fn boolean(value: bool) -> Self {
        Self::scalar(ffi::BOOLEAN, value as u64)
    }
    pub const fn integer(value: i64) -> Self {
        Self::scalar(ffi::INTEGER, value as u64)
    }
    pub fn float(value: f64) -> Result<Self, Error> {
        if !value.is_finite() {
            return Err(Error::InvalidArgument);
        }
        Ok(Self::scalar(ffi::FLOAT, value.to_bits()))
    }
    pub fn text(value: &'a str) -> Result<Self, Error> {
        Self::span(ffi::TEXT, value.as_bytes())
    }
    pub fn bytes(value: &'a [u8]) -> Result<Self, Error> {
        Self::span(ffi::BYTES, value)
    }
    fn span(kind: u32, value: &'a [u8]) -> Result<Self, Error> {
        let mut result = Self::scalar(kind, 0);
        result.raw.length = u32::try_from(value.len()).map_err(|_| Error::LimitExceeded)?;
        result.raw.data = value.as_ptr();
        Ok(result)
    }
    pub const fn reference(volume: Id, database: [u8; 15], object: Id) -> Self {
        let mut value = Self::scalar(ffi::REFERENCE, 0);
        value.raw.id1 = volume;
        value.raw.id2 = object;
        value.raw.entry_id = database;
        value
    }
    pub const fn file(volume: Id, entry: [u8; 15]) -> Self {
        let mut value = Self::scalar(ffi::FILE, 0);
        value.raw.id1 = volume;
        value.raw.entry_id = entry;
        value
    }
}
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct FieldChange<'a> {
    raw: ffi::FieldInput,
    _borrow: PhantomData<&'a [Value<'a>]>,
}
impl<'a> FieldChange<'a> {
    pub fn set(id: Id, values: &'a [Value<'a>]) -> Result<Self, Error> {
        Ok(Self {
            raw: ffi::FieldInput {
                size: core::mem::size_of::<ffi::FieldInput>() as u32,
                written: 0,
                id,
                values: values.as_ptr().cast(),
                stride: core::mem::size_of::<ffi::ValueInput>() as u32,
                count: u32::try_from(values.len()).map_err(|_| Error::LimitExceeded)?,
                present: 1,
            },
            _borrow: PhantomData,
        })
    }
    pub const fn delete(id: Id) -> Self {
        Self {
            raw: ffi::FieldInput {
                size: core::mem::size_of::<ffi::FieldInput>() as u32,
                written: 0,
                id,
                values: core::ptr::null(),
                count: 0,
                stride: core::mem::size_of::<ffi::ValueInput>() as u32,
                present: 0,
            },
            _borrow: PhantomData,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    Host(Error),
    BufferTooSmall { records: usize, values: usize, bytes: usize },
}
impl From<Error> for ReadError {
    fn from(value: Error) -> Self {
        Self::Host(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    Text(&'a str),
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Bytes(&'a [u8]),
    Reference { volume: Id, database: [u8; 15], object: Id },
    File { volume: Id, entry: [u8; 15] },
}
pub(super) fn value<'a>(record: &ffi::Value, bytes: &'a [u8]) -> Result<ValueRef<'a>, Error> {
    if record.reserved != 0 || record.entry_reserved != 0 {
        return Err(Error::Internal);
    }
    Ok(match record.kind {
        ffi::TEXT => ValueRef::Text(
            core::str::from_utf8(range(bytes, record.bytes())?).map_err(|_| Error::Internal)?,
        ),
        ffi::BYTES => ValueRef::Bytes(range(bytes, record.bytes())?),
        ffi::BOOLEAN if record.scalar <= 1 => ValueRef::Boolean(record.scalar != 0),
        ffi::INTEGER => ValueRef::Integer(record.scalar as i64),
        ffi::FLOAT if f64::from_bits(record.scalar).is_finite() => {
            ValueRef::Float(f64::from_bits(record.scalar))
        }
        ffi::REFERENCE
            if record.scalar == 0
                && record.id1 != [0; 16]
                && record.id2 != [0; 16]
                && record.entry_id != [0; 15] =>
        {
            ValueRef::Reference {
                volume: record.id1,
                database: record.entry_id,
                object: record.id2,
            }
        }
        ffi::FILE
            if record.scalar == 0
                && record.id1 != [0; 16]
                && record.id2 == [0; 16]
                && record.entry_id != [0; 15] =>
        {
            ValueRef::File { volume: record.id1, entry: record.entry_id }
        }
        _ => return Err(Error::Internal),
    })
}
pub struct Object<'a> {
    pub type_id: Id,
    fields: &'a [ffi::Field],
    values: &'a [ffi::Value],
    bytes: &'a [u8],
}
pub struct Field<'a> {
    pub id: Id,
    pub(super) values: Option<&'a [ffi::Value]>,
    pub(super) bytes: &'a [u8],
}
impl<'a> Field<'a> {
    pub fn values(&self) -> Option<impl ExactSizeIterator<Item = ValueRef<'a>> + '_> {
        self.values.map(|values| {
            values.iter().map(|record| value(record, self.bytes).expect("validated object value"))
        })
    }
}
impl<'a> Object<'a> {
    pub fn fields(&self) -> impl ExactSizeIterator<Item = Field<'a>> + '_ {
        self.fields.iter().map(|field| Field {
            id: field.id,
            values: (field.present == 1)
                .then(|| range(self.values, field.values()).expect("validated field range")),
            bytes: self.bytes,
        })
    }
    pub fn field(&self, id: Id) -> Option<Field<'a>> {
        self.fields().find(|field| field.id == id)
    }
}
pub struct Volumes<'a> {
    pub revision: u64,
    files: alloc::vec::Vec<Option<super::File>>,
    records: &'a [ffi::Volume],
    tags: &'a [ffi::Range],
    bytes: &'a [u8],
}
pub struct Volume<'a> {
    pub grant_id: Id,
    pub backing: u32,
    pub handle: VolumeHandle,
    pub id: Id,
    pub name: &'a str,
    pub rights: u32,
    pub is_root: bool,
    pub role: Option<ffi::VolumeRole>,
    tags: &'a [ffi::Range],
    bytes: &'a [u8],
}
impl<'a> Volume<'a> {
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &'a str> + '_ {
        self.tags.iter().map(|tag| {
            core::str::from_utf8(range(self.bytes, *tag).expect("validated tag range"))
                .expect("validated tag UTF-8")
        })
    }
}
impl<'a> Volumes<'a> {
    pub fn volume(&self, role: ffi::VolumeRole) -> Option<Volume<'a>> {
        self.iter().find(|volume| volume.role == Some(role))
    }
    /// Transfer the selected scope's independently owned File root.
    pub fn take_volume_file(&mut self, role: ffi::VolumeRole) -> Option<super::File> {
        let index = self.records.iter().position(|volume| volume_role(volume) == Some(role))?;
        self.take_file(index)
    }
    /// Transfer one independently owned root out of this enumeration.
    pub fn take_file(&mut self, index: usize) -> Option<super::File> {
        self.files.get_mut(index)?.take()
    }
    pub fn file(&self, index: usize) -> Option<&super::File> {
        self.files.get(index)?.as_ref()
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Volume<'a>> + '_ {
        self.records.iter().map(|volume| Volume {
            grant_id: volume.grant_id,
            backing: volume.flags & ffi::VOLUME_BACKING_MASK,
            handle: volume.handle(),
            id: volume.id,
            name: core::str::from_utf8(
                range(self.bytes, volume.name()).expect("validated volume name"),
            )
            .expect("validated name UTF-8"),
            rights: volume.rights,
            is_root: volume.flags & ffi::VOLUME_ROOT != 0,
            role: volume_role(volume),
            tags: range(self.tags, volume.tags()).expect("validated volume tags"),
            bytes: self.bytes,
        })
    }
}
impl Context<'_> {
    pub fn begin_read_object(
        &mut self,
        volume: super::EntryHandle,
        object: Id,
        fields: &[Id],
    ) -> Result<super::OperationHandle, Error> {
        let count = u32::try_from(fields.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: borrowed object and field inputs remain live and stable for this call.
        let result = unsafe {
            crate::abi::pending::pb_object_read_begin(
                self.raw.as_ptr(),
                &volume,
                &object,
                fields.as_ptr(),
                count,
            )
        };
        super::pending::admitted(result)
    }

    pub fn begin_edit_object(
        &mut self,
        volume: super::EntryHandle,
        object: Id,
        fields: &[FieldChange<'_>],
    ) -> Result<super::OperationHandle, Error> {
        let count = u32::try_from(fields.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: transparent wrappers borrow every nested input through this call.
        let result = unsafe {
            crate::abi::pending::pb_object_edit_begin(
                self.raw.as_ptr(),
                &volume,
                &object,
                fields.as_ptr().cast(),
                count,
                core::mem::size_of::<ffi::FieldInput>() as u32,
            )
        };
        super::pending::admitted(result)
    }

    pub fn take_edit_object(
        &mut self,
        handle: super::OperationHandle,
    ) -> Result<super::Completion<()>, Error> {
        // SAFETY: only the context is borrowed; the handle is a scalar value.
        let result = unsafe { crate::abi::pending::pb_object_edit_take(self.raw.as_ptr(), handle) };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        status(result)?;
        Ok(super::Completion::Ready(()))
    }

    pub fn take_read_object<'a>(
        &mut self,
        handle: super::OperationHandle,
        fields: &'a mut [ffi::Field],
        values: &'a mut [ffi::Value],
        bytes: &'a mut [u8],
    ) -> Result<super::Completion<Object<'a>>, ReadError> {
        let mut output = ffi::Object::new(buffer(fields)?, buffer(values)?, buffer(bytes)?);
        // SAFETY: independent mutable borrows admit initialized result arrays.
        let result = unsafe {
            crate::abi::pending::pb_object_read_take(self.raw.as_ptr(), handle, &mut output)
        };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        object_result(result, output, fields, values, bytes).map(super::Completion::Ready)
    }
    pub fn available_volumes<'a>(
        &mut self,
        records: &'a mut [ffi::Volume],
        tags: &'a mut [ffi::Range],
        bytes: &'a mut [u8],
    ) -> Result<Volumes<'a>, ReadError> {
        // Probe metadata without allocating handles, then reserve ownership for
        // the whole result. Restrict the real call to that count: a concurrent
        // grant-set increase reports BufferTooSmall instead of transferring
        // unreserved roots. Metadata and roots still come from one actual call.
        let mut probe = ffi::Volumes::new(buffer(&mut [])?, buffer(&mut [])?, buffer(&mut [])?);
        // SAFETY: three empty initialized descriptors borrow no payload storage.
        let code = unsafe { ffi::pb_volumes(self.raw.as_ptr(), &mut probe) };
        if code != Error::BufferTooSmall as i64 {
            status(code)?;
        }
        let count = probe.volumes_count as usize;
        if count > records.len()
            || probe.tags_count as usize > tags.len()
            || probe.bytes_count as usize > bytes.len()
        {
            return Err(ReadError::BufferTooSmall {
                records: count,
                values: probe.tags_count as usize,
                bytes: probe.bytes_count as usize,
            });
        }
        let tokens = self.reserve_files(count)?;
        let mut output =
            ffi::Volumes::new(buffer(&mut records[..count])?, buffer(tags)?, buffer(bytes)?);
        // SAFETY: independent mutable borrows establish all output lifetimes and disjointness.
        let result = unsafe { ffi::pb_volumes(self.raw.as_ptr(), &mut output) };
        if result == Error::BufferTooSmall as i64 {
            return Err(ReadError::BufferTooSmall {
                records: output.volumes_count as usize,
                values: output.tags_count as usize,
                bytes: output.bytes_count as usize,
            });
        }
        status(result)?;
        let records = records.get(..output.volumes_count as usize).ok_or(Error::Internal)?;
        let files = tokens
            .into_iter()
            .zip(records)
            .map(|(token, record)| Some(super::File::adopt(token, record.file())))
            .collect();
        let owned = Volumes { revision: output.revision, files, records, tags: &[], bytes: &[] };
        let tags = tags.get(..output.tags_count as usize).ok_or(Error::Internal)?;
        let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
        validate_volumes(output.revision, records, tags, bytes)?;
        Ok(Volumes { tags, bytes, ..owned })
    }
}

fn validate_volumes(
    revision: u64,
    records: &[ffi::Volume],
    tags: &[ffi::Range],
    bytes: &[u8],
) -> Result<(), ReadError> {
    let mut roles = 0u8;
    for volume in records {
        if revision == 0
            || volume.grant_id == [0; 16]
            || volume.flags & !(ffi::VOLUME_ROOT | ffi::VOLUME_BACKING_MASK) != 0
            || !matches!(
                volume.flags & ffi::VOLUME_BACKING_MASK,
                ffi::VOLUME_PLAYBIT | ffi::VOLUME_HOST_FS | ffi::VOLUME_BUNDLE
            )
            || volume.rights & !7 != 0
        {
            return Err(Error::Internal.into());
        }
        if volume.written >= 84 && volume.role != 0 {
            let role = volume_role(volume).ok_or(Error::Internal)?;
            let bit = 1u8 << role as u32;
            if roles & bit != 0
                || volume.flags & ffi::VOLUME_BACKING_MASK != ffi::VOLUME_PLAYBIT
                || (role == ffi::VolumeRole::Root) != (volume.flags & ffi::VOLUME_ROOT != 0)
            {
                return Err(Error::Internal.into());
            }
            roles |= bit;
        }
        core::str::from_utf8(range(bytes, volume.name())?).map_err(|_| Error::Internal)?;
        range(tags, volume.tags())?;
    }
    for tag in tags {
        core::str::from_utf8(range(bytes, *tag)?).map_err(|_| Error::Internal)?;
    }
    Ok(())
}

fn volume_role(volume: &ffi::Volume) -> Option<ffi::VolumeRole> {
    if volume.written >= 84 { ffi::VolumeRole::from_raw(volume.role) } else { None }
}

fn object_result<'a>(
    result: i64,
    output: ffi::Object,
    fields: &'a mut [ffi::Field],
    values: &'a mut [ffi::Value],
    bytes: &'a mut [u8],
) -> Result<Object<'a>, ReadError> {
    if result == Error::BufferTooSmall as i64 {
        return Err(ReadError::BufferTooSmall {
            records: output.fields_count as usize,
            values: output.values_count as usize,
            bytes: output.bytes_count as usize,
        });
    }
    status(result)?;
    let fields = fields.get(..output.fields_count as usize).ok_or(Error::Internal)?;
    let values = values.get(..output.values_count as usize).ok_or(Error::Internal)?;
    let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
    for field in fields {
        if field.reserved != 0
            || field.present > 1
            || (field.present == 0 && field.values_count != 0)
        {
            return Err(Error::Internal.into());
        }
        range(values, field.values())?;
    }
    for record in values {
        value(record, bytes)?;
    }
    Ok(Object { type_id: output.type_id, fields, values, bytes })
}

pub struct Query<'a> {
    pub type_id: Id,
    pub after: Option<Id>,
    pub limit: u32,
    pub fields: Option<&'a [Id]>,
}
pub struct PageStorage<'a> {
    pub objects: &'a mut [ffi::ObjectInfo],
    pub fields: &'a mut [ffi::Field],
    pub values: &'a mut [ffi::Value],
    pub bytes: &'a mut [u8],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryError {
    Host(Error),
    BufferTooSmall { objects: usize, fields: usize, values: usize, bytes: usize },
}
impl From<Error> for QueryError {
    fn from(error: Error) -> Self {
        Self::Host(error)
    }
}
pub struct Page<'a> {
    pub next: Option<Id>,
    objects: &'a [ffi::ObjectInfo],
    fields: &'a [ffi::Field],
    values: &'a [ffi::Value],
    bytes: &'a [u8],
    projected: bool,
}
pub struct PageObject<'a> {
    pub id: Id,
    pub record: Option<Object<'a>>,
}
impl<'a> Page<'a> {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = PageObject<'a>> + '_ {
        self.objects.iter().map(|object| PageObject {
            id: object.id,
            record: self.projected.then(|| Object {
                type_id: object.type_id,
                fields: range(self.fields, object.fields()).expect("validated query range"),
                values: self.values,
                bytes: self.bytes,
            }),
        })
    }
}
impl Context<'_> {
    pub fn begin_query_objects(
        &mut self,
        volume: super::EntryHandle,
        query: Query<'_>,
    ) -> Result<super::OperationHandle, Error> {
        let fields = query.fields.unwrap_or_default();
        let input = ffi::Query {
            size: core::mem::size_of::<ffi::Query>() as u32,
            written: 0,
            type_id: query.type_id,
            after: query.after.unwrap_or([0; 16]),
            flags: u32::from(query.after.is_some()) | (u32::from(query.fields.is_some()) << 1),
            limit: query.limit,
            fields: fields.as_ptr(),
            count: u32::try_from(fields.len()).map_err(|_| Error::LimitExceeded)?,
            reserved: 0,
        };
        // SAFETY: the query and projection remain live and stable through begin.
        let result = unsafe {
            crate::abi::pending::pb_object_query_begin(self.raw.as_ptr(), &volume, &input)
        };
        super::pending::admitted(result)
    }

    pub fn take_query_objects<'a>(
        &mut self,
        handle: super::OperationHandle,
        storage: PageStorage<'a>,
    ) -> Result<super::Completion<Page<'a>>, QueryError> {
        let mut output = ffi::Page::new(
            buffer(storage.objects)?,
            buffer(storage.fields)?,
            buffer(storage.values)?,
            buffer(storage.bytes)?,
        );
        // SAFETY: independent mutable borrows admit every result array.
        let result = unsafe {
            crate::abi::pending::pb_object_query_take(self.raw.as_ptr(), handle, &mut output)
        };
        if result == Error::WouldBlock as i64 {
            return Ok(super::Completion::Pending);
        }
        page_result(result, output, storage).map(super::Completion::Ready)
    }
}

fn page_result<'a>(
    result: i64,
    output: ffi::Page,
    storage: PageStorage<'a>,
) -> Result<Page<'a>, QueryError> {
    if result == Error::BufferTooSmall as i64 {
        return Err(QueryError::BufferTooSmall {
            objects: output.objects_count as usize,
            fields: output.fields_count as usize,
            values: output.values_count as usize,
            bytes: output.bytes_count as usize,
        });
    }
    status(result)?;
    if output.has_next > 1 || output.projected > 1 {
        return Err(Error::Internal.into());
    }
    let objects = storage.objects.get(..output.objects_count as usize).ok_or(Error::Internal)?;
    let fields = storage.fields.get(..output.fields_count as usize).ok_or(Error::Internal)?;
    let values = storage.values.get(..output.values_count as usize).ok_or(Error::Internal)?;
    let bytes = storage.bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
    for object in objects {
        range(fields, object.fields())?;
    }
    for field in fields {
        if field.reserved != 0
            || field.present > 1
            || (field.present == 0 && field.values_count != 0)
        {
            return Err(Error::Internal.into());
        }
        range(values, field.values())?;
    }
    for record in values {
        value(record, bytes)?;
    }
    Ok(Page {
        next: (output.has_next == 1).then_some(output.next),
        objects,
        fields,
        values,
        bytes,
        projected: output.projected == 1,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_keep_complete_database_and_blob_identities() {
        let reference = Value::reference([1; 16], [2; 15], [3; 16]);
        assert_eq!(reference.raw.id1, [1; 16]);
        assert_eq!(reference.raw.entry_id, [2; 15]);
        assert_eq!(reference.raw.id2, [3; 16]);
        let file = Value::file([4; 16], [5; 15]);
        assert_eq!(file.raw.id1, [4; 16]);
        assert_eq!(file.raw.entry_id, [5; 15]);
        assert_eq!(file.raw.id2, [0; 16]);
        assert_eq!(file.raw.scalar, 0);
    }

    #[test]
    fn borrowed_reference_results_reject_missing_identity_and_reserved_bits() {
        let mut record = ffi::Value {
            kind: ffi::REFERENCE,
            id1: [1; 16],
            id2: [3; 16],
            entry_id: [2; 15],
            ..ffi::Value::default()
        };
        assert_eq!(
            value(&record, &[]),
            Ok(ValueRef::Reference { volume: [1; 16], database: [2; 15], object: [3; 16] })
        );
        record.entry_id = [0; 15];
        assert_eq!(value(&record, &[]), Err(Error::Internal));
        record.entry_id = [2; 15];
        record.entry_reserved = 1;
        assert_eq!(value(&record, &[]), Err(Error::Internal));
        record.entry_reserved = 0;
        record.kind = ffi::FILE;
        assert_eq!(value(&record, &[]), Err(Error::Internal));
        record.id2 = [0; 16];
        assert_eq!(value(&record, &[]), Ok(ValueRef::File { volume: [1; 16], entry: [2; 15] }));
    }

    #[test]
    fn fixed_record_sizes_match_native_and_wasm_lowering() {
        assert_eq!(core::mem::size_of::<ffi::Volume>(), 88);
        assert_eq!(core::mem::size_of::<ffi::Value>(), 80);
        assert_eq!(core::mem::size_of::<crate::abi::authenticated::Metadata>(), 168);
        assert_eq!(core::mem::size_of::<crate::abi::files::FileReference>(), 32);
        assert_eq!(core::mem::size_of::<crate::abi::pending::Selection>(), 40);
    }

    #[test]
    fn automatic_roles_require_the_tail_and_reject_ambiguous_or_foreign_scopes() {
        let shared = ffi::Volume {
            written: 84,
            grant_id: [1; 16],
            flags: ffi::VOLUME_PLAYBIT,
            role: ffi::VolumeRole::Session as u32,
            ..ffi::Volume::default()
        };
        assert!(validate_volumes(1, &[shared], &[], &[]).is_ok());
        assert_eq!(volume_role(&shared), Some(ffi::VolumeRole::Session));
        let prefix = ffi::Volume { written: 80, role: u32::MAX, ..shared };
        assert!(validate_volumes(1, &[prefix], &[], &[]).is_ok());
        assert_eq!(volume_role(&prefix), None);
        for invalid in [
            ffi::Volume { role: 7, ..shared },
            ffi::Volume { flags: ffi::VOLUME_HOST_FS, ..shared },
            ffi::Volume { flags: ffi::VOLUME_PLAYBIT | ffi::VOLUME_ROOT, ..shared },
        ] {
            assert!(validate_volumes(1, &[invalid], &[], &[]).is_err());
        }
        assert!(validate_volumes(1, &[shared, shared], &[], &[]).is_err());
    }
}
