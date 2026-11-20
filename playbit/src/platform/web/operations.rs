//! Named web host calls. Only admitted C-layout input records cross from the
//! guest's memory through JavaScript into INPUT; scalar arguments stay scalars.
//! Current grants, schema checks, transactions and changesets stay in Rust.
use super::*;
use crate::runtime::{
    host as runtime,
    linear_memory::{Memory, input, layout},
};

// Maximum field/value arrays, logical payload, IDs and up to seven padding
// bytes before each nested aligned value array.
const STAGING_LIMIT: usize = LIMIT + 256 * (40 + 7) + 4096 * 80 + 128;

pub(super) fn call(operation: impl FnOnce(&Host, &mut Memory<'_>) -> Result<()>) -> i32 {
    crate::platform::failure::guard();
    RESULT.with(|result| *result.borrow_mut() = None);
    let mut input = INPUT.lock().expect("mutex is not poisoned");
    if input.len() > STAGING_LIMIT {
        return Error::LimitExceeded as i32;
    }
    // No guest instruction runs while this synchronous export lends the input.
    let mut memory = Memory { data: &mut input, fuel: None };
    HOST.with(|host| {
        let host = host.borrow();
        operation(host.as_ref().ok_or(Error::Unavailable)?, &mut memory)
    })
    .map_or_else(|error| error as i32, |()| 0)
}

fn changes(
    memory: &mut Memory<'_>,
    pointer: u32,
    count: u32,
    stride: u32,
) -> Result<BTreeMap<Id, Option<Vec<Value>>>> {
    let changes = input::changes(memory, pointer, count, stride, None)?;
    let mut fields = BTreeMap::new();
    for change in changes {
        let values = change
            .values
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| crate::runtime::store_bridge::to_value(&value))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        if fields.insert(Id::from_bytes(change.field_id), values).is_some() {
            return Err(Error::InvalidArgument);
        }
    }
    Ok(fields)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_object_create(
    entry: u32,
    type_id: u32,
    fields: u32,
    count: u32,
    stride: u32,
) -> i32 {
    call(|host, memory| {
        let type_id = Id::from_bytes(memory.admit(type_id, 1, 16, 1, 1)?.read(memory.data, 0));
        let fields = changes(memory, fields, count, stride)?;
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::Create)?;
        let id = host.create(&volume, entry, type_id, fields)?;
        // A fixed sixteen-byte result cannot fail after the durable mutation.
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(id.as_bytes());
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_object_edit(
    entry: u32,
    object: u32,
    fields: u32,
    count: u32,
    stride: u32,
) -> i32 {
    call(|host, memory| {
        let object = Id::from_bytes(memory.admit(object, 1, 16, 1, 1)?.read(memory.data, 0));
        let fields = changes(memory, fields, count, stride)?;
        if fields.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::Edit)?;
        host.edit(&volume, entry, object, fields)
    })
}

use playbit_api::abi::{data as abi, environment};

// Only one synchronous caller uses these results. Pointers borrow the retained
// arrays until the next named host operation; JavaScript copies them immediately.
thread_local! {
    static RESULT: RefCell<Option<Output>> = const { RefCell::new(None) };
}
enum Record {
    Capabilities(environment::Capabilities),
    Volumes(abi::Volumes),
    Object(abi::Object),
    Page(abi::Page),
    Schema(playbit_api::abi::schema::Schema),
}
struct Output {
    record: Record,
    _arrays: Arrays,
}
#[derive(Default)]
struct Arrays {
    volumes: Vec<abi::Volume>,
    ranges: Vec<abi::Range>,
    objects: Vec<abi::ObjectInfo>,
    fields: Vec<abi::Field>,
    values: Vec<abi::Value>,
    bytes: Vec<u8>,
    logical_bytes: usize,
    schema_types: Vec<playbit_api::abi::schema::Type>,
    schema_fields: Vec<playbit_api::abi::schema::Field>,
}
fn extent<T: playbit_api::abi::record::Record>() -> u32 {
    playbit_api::abi::record::extent::<T>(size_of::<T>() as u32)
}
fn publish(record: Record, arrays: Arrays) {
    RESULT.with(|result| *result.borrow_mut() = Some(Output { record, _arrays: arrays }));
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_result() -> *const u8 {
    crate::platform::failure::guard();
    RESULT.with(|result| match result.borrow().as_ref().map(|result| &result.record) {
        Some(Record::Capabilities(record)) => std::ptr::from_ref(record).cast(),
        Some(Record::Volumes(record)) => std::ptr::from_ref(record).cast(),
        Some(Record::Object(record)) => std::ptr::from_ref(record).cast(),
        Some(Record::Schema(record)) => std::ptr::from_ref(record).cast(),
        Some(Record::Page(record)) => std::ptr::from_ref(record).cast(),
        None => std::ptr::null(),
    })
}
impl Arrays {
    fn bytes(&mut self, bytes: &[u8]) -> Result<abi::Range> {
        if bytes.len() > LIMIT - self.bytes.len() {
            return Err(Error::LimitExceeded);
        }
        let range = abi::Range {
            size: size_of::<abi::Range>() as u32,
            written: extent::<abi::Range>(),
            offset: self.bytes.len() as u32,
            count: bytes.len() as u32,
        };
        self.bytes.extend_from_slice(bytes);
        Ok(range)
    }
    fn field(&mut self, id: Id, values: Option<&[Value]>) -> Result<()> {
        let count = values.map_or(0, <[Value]>::len);
        if count > 4096
            || self.values.len() + count > playbit_api::abi::syscall::MAX_PAGE_VALUES as usize
            || self.fields.len() >= 64 * 256
        {
            return Err(Error::LimitExceeded);
        }
        self.values.try_reserve_exact(count).map_err(|_| Error::LimitExceeded)?;
        let range = abi::Range {
            size: size_of::<abi::Range>() as u32,
            written: extent::<abi::Range>(),
            offset: self.values.len() as u32,
            count: count as u32,
        };
        for value in values.into_iter().flatten() {
            self.field_value(value)?;
        }
        self.fields.push(abi::Field {
            size: size_of::<abi::Field>() as u32,
            written: extent::<abi::Field>(),
            id: *id.as_bytes(),
            values_offset: range.offset,
            values_count: range.count,
            present: u32::from(values.is_some()),
            reserved: 0,
        });
        Ok(())
    }
    fn field_value(&mut self, value: &Value) -> Result<()> {
        let size = match value {
            Value::Text(value) => value.len(),
            Value::Bytes(value) => value.len(),
            Value::Boolean(_) => 1,
            Value::Integer(_) | Value::Float(_) => 8,
            Value::Reference { .. } => 47,
            Value::File { .. } => 31,
        };
        if size > LIMIT - self.logical_bytes {
            return Err(Error::LimitExceeded);
        }
        self.logical_bytes += size;
        let mut record = abi::Value::default();
        record.written = extent::<abi::Value>();
        record.kind = match value {
            Value::Text(value) => {
                let bytes = self.bytes(value.as_bytes())?;
                record.bytes_offset = bytes.offset;
                record.bytes_count = bytes.count;
                abi::TEXT
            }
            Value::Bytes(value) => {
                let bytes = self.bytes(value)?;
                record.bytes_offset = bytes.offset;
                record.bytes_count = bytes.count;
                abi::BYTES
            }
            Value::Boolean(value) => {
                record.scalar = u64::from(*value);
                abi::BOOLEAN
            }
            Value::Integer(value) => {
                record.scalar = *value as u64;
                abi::INTEGER
            }
            Value::Float(value) => {
                if !value.is_finite() {
                    return Err(Error::Internal);
                }
                record.scalar = value.to_bits();
                abi::FLOAT
            }
            Value::Reference { volume_id, database_id, object_id } => {
                record.entry_id = *database_id.as_bytes();
                record.id1 = *volume_id.as_bytes();
                record.id2 = *object_id.as_bytes();
                abi::REFERENCE
            }
            Value::File { volume_id, entry_id } => {
                record.id1 = *volume_id.as_bytes();
                record.entry_id = *entry_id.as_bytes();
                abi::FILE
            }
        };
        self.values.push(record);
        Ok(())
    }
}
fn ids(memory: &mut Memory<'_>, pointer: u32, count: u32) -> Result<Vec<Id>> {
    if count == 0 {
        return Err(Error::InvalidArgument);
    }
    let region = memory.admit(pointer, count, 16, 1, 256)?;
    let mut result = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let id = Id::from_bytes(region.read(memory.data, i * 16));
        if result.contains(&id) {
            return Err(Error::InvalidArgument);
        }
        result.push(id);
    }
    Ok(result)
}
fn query_error(error: crate::store::Error) -> Error {
    store_error(error)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_object_read(entry: u32, object: u32, fields: u32, count: u32) -> i32 {
    call(|host, memory| {
        let object = Id::from_bytes(memory.admit(object, 1, 16, 1, 1)?.read(memory.data, 0));
        let fields = ids(memory, fields, count)?;
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::Read)?;
        host.schema(&volume, entry, Permission::Read, true)?;
        let database = host.entry_database(&volume, entry)?;
        let snapshot =
            crate::store::object_database::Snapshot::new(database.begin_read().map_err(map_error)?);
        let object = snapshot.read(object, Some(&fields)).map_err(query_error)?;
        let mut arrays = Arrays::default();
        for field in fields {
            arrays
                .field(field, object.fields.get(&field).and_then(|cell| cell.values.as_deref()))?;
        }
        let record = abi::Object {
            size: size_of::<abi::Object>() as u32,
            written: extent::<abi::Object>(),
            type_id: *object.type_id.as_bytes(),
            fields_data: arrays.fields.as_mut_ptr(),
            fields_capacity: arrays.fields.len() as u32,
            fields_count: arrays.fields.len() as u32,
            fields_stride: size_of::<abi::Field>() as u32,
            values_data: arrays.values.as_mut_ptr(),
            values_capacity: arrays.values.len() as u32,
            values_count: arrays.values.len() as u32,
            values_stride: size_of::<abi::Value>() as u32,
            bytes_data: arrays.bytes.as_mut_ptr(),
            bytes_capacity: arrays.bytes.len() as u32,
            bytes_count: arrays.bytes.len() as u32,
        };
        publish(Record::Object(record), arrays);
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_object_query(entry: u32, query: u32) -> i32 {
    call(|host, memory| {
        let query = memory.record(query, layout::data::query::LAYOUT, true, None)?;
        let type_id = Id::from_bytes(query.read(memory.data, 8));
        let after = query.read(memory.data, 24);
        let flags = query.u32(memory.data, 40);
        let limit = query.u32(memory.data, 44);
        let count = query.u32(memory.data, 52);
        if flags & !3 != 0
            || query.u32(memory.data, 56) != 0
            || (flags & 1 == 0 && after != [0; 16])
            || (flags & 2 == 0 && count != 0)
            || limit == 0
        {
            return Err(Error::InvalidArgument);
        }
        if limit > 64 {
            return Err(Error::LimitExceeded);
        }
        let fields = if flags & 2 != 0 {
            ids(memory, query.u32(memory.data, 48), count)?
        } else {
            Vec::new()
        };
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::Query)?;
        let schema = host.schema(&volume, entry, Permission::Read, true)?;
        if !schema.types.contains_key(&type_id) {
            return Err(Error::NotFound);
        }
        let database = host.entry_database(&volume, entry)?;
        let snapshot =
            crate::store::object_database::Snapshot::new(database.begin_read().map_err(map_error)?);
        let page = snapshot
            .page(
                type_id,
                (flags & 1 != 0).then(|| Id::from_bytes(after)),
                limit as usize,
                Some(fields.as_slice()),
            )
            .map_err(query_error)?;
        object_query_output(&page, flags & 2 != 0)
    })
}
fn object_query_output(page: &crate::store::ObjectPage, projected: bool) -> Result<()> {
    let mut arrays = Arrays::default();
    for object in &page.objects {
        let range = abi::Range {
            size: size_of::<abi::Range>() as u32,
            written: extent::<abi::Range>(),
            offset: arrays.fields.len() as u32,
            count: object.fields.len() as u32,
        };
        for (field, cell) in &object.fields {
            arrays.field(*field, cell.values.as_deref())?;
        }
        arrays.objects.push(abi::ObjectInfo {
            size: size_of::<abi::ObjectInfo>() as u32,
            written: extent::<abi::ObjectInfo>(),
            id: *object.object_id.as_bytes(),
            type_id: *object.type_id.as_bytes(),
            fields_offset: range.offset,
            fields_count: range.count,
        });
    }
    let record = abi::Page {
        size: size_of::<abi::Page>() as u32,
        written: extent::<abi::Page>(),
        next: page.next.map_or([0; 16], |id| *id.as_bytes()),
        has_next: u32::from(page.next.is_some()),
        projected: u32::from(projected),
        objects_data: arrays.objects.as_mut_ptr(),
        objects_capacity: arrays.objects.len() as u32,
        objects_count: arrays.objects.len() as u32,
        objects_stride: size_of::<abi::ObjectInfo>() as u32,
        fields_data: arrays.fields.as_mut_ptr(),
        fields_capacity: arrays.fields.len() as u32,
        fields_count: arrays.fields.len() as u32,
        fields_stride: size_of::<abi::Field>() as u32,
        values_data: arrays.values.as_mut_ptr(),
        values_capacity: arrays.values.len() as u32,
        values_count: arrays.values.len() as u32,
        values_stride: size_of::<abi::Value>() as u32,
        bytes_data: arrays.bytes.as_mut_ptr(),
        bytes_capacity: arrays.bytes.len() as u32,
        bytes_count: arrays.bytes.len() as u32,
    };
    publish(Record::Page(record), arrays);
    Ok(())
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_volumes() -> i32 {
    call(|host, _| {
        let mut arrays = Arrays::default();
        let mut files = host.files.borrow_mut();
        files.refresh(host)?;
        for (handle, volume, grant_id) in files.metadata() {
            let name = arrays.bytes(volume.name.as_bytes())?;
            let tags = abi::Range {
                size: size_of::<abi::Range>() as u32,
                written: extent::<abi::Range>(),
                offset: arrays.ranges.len() as u32,
                count: volume.tags.len() as u32,
            };
            for tag in &volume.tags {
                let tag = arrays.bytes(tag.as_bytes())?;
                arrays.ranges.push(tag);
            }
            arrays.volumes.push(abi::Volume {
                size: size_of::<abi::Volume>() as u32,
                written: extent::<abi::Volume>(),
                handle: handle.raw(),
                id: volume.id,
                name_offset: name.offset,
                name_count: name.count,
                tags_offset: tags.offset,
                tags_count: tags.count,
                rights: volume.rights.0,
                flags: u32::from(volume.is_root) | volume.backing,
                // Internal metadata only. Guest enumeration acquires aliases
                // after validating the complete destination in JavaScript.
                file: Default::default(),
                grant_id,
                role: volume.role,
            });
        }
        let record = abi::Volumes {
            size: size_of::<abi::Volumes>() as u32,
            written: extent::<abi::Volumes>(),
            revision: files.revision,
            volumes_data: arrays.volumes.as_mut_ptr(),
            volumes_capacity: arrays.volumes.len() as u32,
            volumes_count: arrays.volumes.len() as u32,
            volumes_stride: size_of::<abi::Volume>() as u32,
            tags_data: arrays.ranges.as_mut_ptr(),
            tags_capacity: arrays.ranges.len() as u32,
            tags_count: arrays.ranges.len() as u32,
            tags_stride: size_of::<abi::Range>() as u32,
            bytes_data: arrays.bytes.as_mut_ptr(),
            bytes_capacity: arrays.bytes.len() as u32,
            bytes_count: arrays.bytes.len() as u32,
        };
        publish(Record::Volumes(record), arrays);
        Ok(())
    })
}

// Capability ceilings are memory-only. Catalog authorization below is a
// separate operation because a selected entry may remain readable without it.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_volume_check(generation: u64, slot: u32, rights: u32) -> i32 {
    call(|_, _| {
        if rights == 0 || rights & !7 != 0 {
            return Err(Error::InvalidArgument);
        }
        let generation = u32::try_from(generation).map_err(|_| Error::StaleHandle)?;
        work::capabilities(|capabilities| {
            capabilities
                .volume(
                    crate::capability::Handle::new(generation, slot),
                    crate::capability::Rights(rights),
                )
                .map(|_| ())
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_volume_authorize(generation: u64, slot: u32, rights: u32) -> i32 {
    call(|host, _| {
        if rights == 0 || rights & !7 != 0 {
            return Err(Error::InvalidArgument);
        }
        let generation = u32::try_from(generation).map_err(|_| Error::StaleHandle)?;
        work::capabilities(|capabilities| {
            capabilities
                .volume(
                    crate::capability::Handle::new(generation, slot),
                    crate::capability::Rights(rights),
                )
                .map(|_| ())
        })?;
        let volume = host.volume_handle(u64::from(generation), slot)?;
        host.authorize(&volume, None, Permission::Read)?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_entry_authorize(entry: u32) -> i32 {
    call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::Read)?;
        host.authorize_entry_read(&volume, entry)?;
        Ok(())
    })
}
#[link(wasm_import_module = "opfs")]
unsafe extern "C" {
    // The trusted browser dispatcher reports its actual callback map, including
    // TypeScript-owned operations. This is internal host glue, not a guest ABI.
    fn operation_supported(opcode: u32) -> i32;
}

fn supported(opcode: Opcode) -> bool {
    // SAFETY: the synchronous browser import reads only its callback map; no
    // Rust memory or host export is lent to it or re-entered.
    unsafe { operation_supported(opcode as u32) == 1 }
}

pub(super) fn supports(feature: &str) -> bool {
    feature == abi::SESSION_VOLUME_FEATURE
        || playbit_api::abi::syscall::REGISTRY
            .iter()
            .any(|(opcode, capability)| *capability == feature && supported(*opcode))
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_capabilities() -> i32 {
    call(|host, _| {
        let mut arrays = Arrays::default();
        for (opcode, capability) in playbit_api::abi::syscall::REGISTRY {
            if supported(*opcode) {
                let name = arrays.bytes(capability.as_bytes())?;
                arrays.ranges.push(name);
            }
        }
        if host.session.borrow().shared_volume.is_some() {
            let name = arrays.bytes(abi::SESSION_VOLUME_FEATURE.as_bytes())?;
            arrays.ranges.push(name);
        }
        let record = environment::Capabilities {
            size: size_of::<environment::Capabilities>() as u32,
            written: extent::<environment::Capabilities>(),
            names_data: arrays.ranges.as_mut_ptr(),
            names_capacity: arrays.ranges.len() as u32,
            names_count: arrays.ranges.len() as u32,
            names_stride: size_of::<abi::Range>() as u32,
            bytes_data: arrays.bytes.as_mut_ptr(),
            bytes_capacity: arrays.bytes.len() as u32,
            bytes_count: arrays.bytes.len() as u32,
        };
        publish(Record::Capabilities(record), arrays);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_host_limit(kind: u32) -> i32 {
    crate::platform::failure::guard();
    HOST.with(|host| {
        if host.borrow().is_none() {
            return Error::Unavailable as i32;
        }
        if kind == playbit_api::abi::environment::HostLimit::ViewBytes as u32 {
            return LIMIT as i32;
        }
        // The web store and its query adapters enforce the shared ABI ceilings.
        runtime::QueryLimits::default().get(kind).unwrap_or_else(|error| error as i32)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_entry_resolve(generation: u64, slot: u32, name: u32, length: u32) -> i32 {
    call(|host, memory| {
        let bytes = memory.admit(
            name,
            length,
            1,
            1,
            playbit_api::abi::files::MAX_FILE_PATH_BYTES as u32,
        )?;
        let name =
            std::str::from_utf8(bytes.bytes(memory.data)).map_err(|_| Error::InvalidArgument)?;
        let checked_generation = u32::try_from(generation).map_err(|_| Error::StaleHandle)?;
        work::capabilities(|capabilities| {
            capabilities
                .volume(
                    crate::capability::Handle::new(checked_generation, slot),
                    crate::capability::Rights::READ,
                )
                .map(|_| ())
        })?;
        let volume = host.volume_handle(generation, slot)?;
        host.authorize(&volume, None, Permission::Read)?;
        let snapshot = crate::store::volume_index::IndexSnapshot::new(
            volume.index.begin_read().map_err(map_error)?,
        );
        let entry = snapshot.lookup(name).map_err(query_error)?;
        host.authorize(&volume, Some(entry.id), Permission::Read)?;
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(&32u32.to_le_bytes());
        output.extend_from_slice(&32u32.to_le_bytes());
        output.extend_from_slice(
            &playbit_api::abi::dispatch::pack_handle(generation as u32, slot).to_le_bytes(),
        );
        output.extend_from_slice(entry.id.as_bytes());
        output.push(0);
        Ok(())
    })
}

fn schema_kind(value: &crate::store::object_contract::ValueType) -> (u32, [u8; 16]) {
    use crate::store::object_contract::ValueType;
    (
        match value {
            ValueType::Text => abi::TEXT,
            ValueType::Boolean => abi::BOOLEAN,
            ValueType::Integer => abi::INTEGER,
            ValueType::Float => abi::FLOAT,
            ValueType::Bytes => abi::BYTES,
            ValueType::File => abi::FILE,
            ValueType::Reference(id) => return (abi::REFERENCE, *id.as_bytes()),
        },
        [0; 16],
    )
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_schema_read(entry: u32) -> i32 {
    call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::SchemaRead)?;
        let schema = host.schema(&volume, entry, Permission::Read, false)?;
        let mut arrays = Arrays::default();
        for (id, ty) in schema.types {
            let name = arrays.bytes(ty.name.as_bytes())?;
            let fields = abi::Range {
                size: size_of::<abi::Range>() as u32,
                written: extent::<abi::Range>(),
                offset: arrays.schema_fields.len() as u32,
                count: ty.fields.len() as u32,
            };
            arrays.schema_types.push(playbit_api::abi::schema::Type {
                size: size_of::<playbit_api::abi::schema::Type>() as u32,
                written: extent::<playbit_api::abi::schema::Type>(),
                id: *id.as_bytes(),
                name_offset: name.offset,
                name_count: name.count,
                fields_offset: fields.offset,
                fields_count: fields.count,
                flags: u32::from(ty.deprecated),
                reserved: 0,
            });
            for (id, field) in ty.fields {
                let name = arrays.bytes(field.name.as_bytes())?;
                let (kind, reference_type) = schema_kind(&field.value_type);
                arrays.schema_fields.push(playbit_api::abi::schema::Field {
                    size: size_of::<playbit_api::abi::schema::Field>() as u32,
                    written: extent::<playbit_api::abi::schema::Field>(),
                    id: *id.as_bytes(),
                    name_offset: name.offset,
                    name_count: name.count,
                    kind,
                    reference_type,
                    flags: u32::from(field.deprecated) | (u32::from(field.repeated) << 1),
                });
            }
        }
        let record = playbit_api::abi::schema::Schema {
            size: size_of::<playbit_api::abi::schema::Schema>() as u32,
            written: extent::<playbit_api::abi::schema::Schema>(),
            types_data: arrays.schema_types.as_mut_ptr(),
            types_capacity: arrays.schema_types.len() as u32,
            types_count: arrays.schema_types.len() as u32,
            types_stride: size_of::<playbit_api::abi::schema::Type>() as u32,
            fields_data: arrays.schema_fields.as_mut_ptr(),
            fields_capacity: arrays.schema_fields.len() as u32,
            fields_count: arrays.schema_fields.len() as u32,
            fields_stride: size_of::<playbit_api::abi::schema::Field>() as u32,
            bytes_data: arrays.bytes.as_mut_ptr(),
            bytes_capacity: arrays.bytes.len() as u32,
            bytes_count: arrays.bytes.len() as u32,
        };
        publish(Record::Schema(record), arrays);
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_schema_establish(entry: u32, types: u32, count: u32, stride: u32) -> i32 {
    call(|host, memory| {
        let (volume, entry) =
            host.entry_handle(memory, entry, crate::runtime::host::DataKind::SchemaEstablish)?;
        let schema =
            crate::runtime::linear_memory::schema::decode(memory, types, count, stride, None)?;
        let declaration = crate::runtime::store_bridge::to_schema(&schema);
        declaration.validate().map_err(store_error)?;
        host.schema(&volume, entry, Permission::Write, false)?;
        let mut tx = volume
            .index
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        if !crate::store::volume_index::establish_schema(
            &mut tx,
            entry,
            &declaration,
            databases::random_entry()?,
            &crate::store::volume_index::namespace::Origin {
                installation: host.installation,
                label: None,
            },
        )
        .map_err(store_error)?
        {
            return Ok(());
        }
        let delta = tx.commit_with_delta().map_err(map_error)?;
        host.changes.borrow_mut().push(volume.id, DatabaseId::Index, delta);
        Ok(())
    })
}
