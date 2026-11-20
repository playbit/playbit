//! Owned capacity accounting, independent of codec and transport scratch.
use super::{DataCall, DataOperation, DataResult};
use crate::runtime::host::{FieldChange, ObjectRecord, Schema, Value};
use std::mem::size_of;

fn vector<T>(values: &Vec<T>) -> Option<usize> {
    values.capacity().checked_mul(size_of::<T>())
}

fn values(values: &Vec<Value>) -> Option<usize> {
    values.iter().try_fold(vector(values)?, |bytes, value| {
        bytes.checked_add(match value {
            Value::Text(text) => text.capacity(),
            Value::Bytes(bytes) => bytes.capacity(),
            _ => 0,
        })
    })
}

fn fields(fields: &Vec<FieldChange>) -> Option<usize> {
    fields.iter().try_fold(vector(fields)?, |bytes, field| {
        bytes.checked_add(field.values.as_ref().map_or(Some(0), values)?)
    })
}

fn records(records: &Vec<ObjectRecord>) -> Option<usize> {
    records
        .iter()
        .try_fold(vector(records)?, |bytes, record| bytes.checked_add(fields(&record.fields)?))
}

fn schema(schema: &Schema) -> Option<usize> {
    schema.types.iter().try_fold(vector(&schema.types)?, |bytes, ty| {
        ty.fields.iter().try_fold(
            bytes.checked_add(ty.name.capacity())?.checked_add(vector(&ty.fields)?)?,
            |bytes, field| bytes.checked_add(field.name.capacity()),
        )
    })
}

impl DataCall {
    /// Includes fixed records and allocated capacity, not only used lengths.
    /// Arc control blocks and allocator metadata are executor overhead.
    pub fn retained_bytes(&self) -> Option<usize> {
        size_of::<Self>().checked_add(match &self.operation {
            DataOperation::KvKeys { after, .. } => after.as_ref().map_or(0, Vec::capacity),
            DataOperation::KvRead { key, .. } | DataOperation::KvDelete { key, .. } => {
                key.capacity()
            }
            DataOperation::KvPut { key, value, .. } => {
                key.capacity().checked_add(value.capacity())?
            }
            DataOperation::ResolveEntry { name } => name.capacity(),
            DataOperation::SchemaEstablish { schema: input, .. } => schema(input)?,
            DataOperation::Create { fields: input, .. }
            | DataOperation::Edit { fields: input, .. } => fields(input)?,
            DataOperation::Read { fields, .. } => vector(fields)?,
            DataOperation::Query { projection, .. } => {
                projection.as_ref().map_or(Some(0), vector)?
            }
            DataOperation::FileWrite { bytes, .. }
            | DataOperation::FileTransferChunk { bytes, .. } => vector(bytes)?,
            DataOperation::ChannelCommand { name, data, .. } => {
                name.capacity().checked_add(vector(data)?)?
            }
            DataOperation::BloblogAppend { blobs, .. } => blobs
                .iter()
                .try_fold(vector(blobs)?, |bytes, blob| bytes.checked_add(vector(blob)?))?,
            DataOperation::BloblogRead { cursor, .. } => {
                cursor.as_ref().map_or(0, crate::store::bloblog::Cursor::retained_bytes)
            }
            _ => 0,
        })
    }
}

impl DataResult {
    pub fn retained_bytes(&self) -> Option<usize> {
        size_of::<Self>().checked_add(match self {
            Self::KvKeys(page) => page.keys.iter().try_fold(
                vector(&page.keys)?.checked_add(page.next.as_ref().map_or(0, Vec::capacity))?,
                |bytes, key| {
                    bytes.checked_add(key.bytes.capacity())?.checked_add(key.preview.capacity())
                },
            )?,
            Self::KvRead(chunk) => vector(&chunk.bytes)?,
            Self::Schema(output) => schema(output)?,
            Self::Object(object) => fields(&object.fields)?,
            Self::Page(page) => vector(&page.objects)?
                .checked_add(page.records.as_ref().map_or(Some(0), records)?)?,
            Self::AuthenticatedField(field) => {
                field.as_ref().and_then(|field| field.values.as_ref()).map_or(Some(0), values)?
            }
            Self::File(file) => vector(&file.bytes)?,
            Self::ChannelCompleted => 0,
            Self::ChannelEvents(events) => {
                events.iter().try_fold(vector(events)?, |bytes, event| {
                    use crate::session_channel::Event;
                    bytes.checked_add(match event {
                        Event::Presence { participants } => {
                            participants.iter().try_fold(vector(participants)?, |bytes, p| {
                                bytes.checked_add(
                                    p.account_id.capacity()
                                        + p.device_id.capacity()
                                        + p.instance_id.capacity()
                                        + p.build_id.capacity(),
                                )
                            })?
                        }
                        Event::Message { channel, sender, message_id, data } => channel
                            .capacity()
                            .checked_add(
                                sender.account_id.capacity()
                                    + sender.device_id.capacity()
                                    + sender.instance_id.capacity()
                                    + sender.build_id.capacity(),
                            )?
                            .checked_add(message_id.capacity())?
                            .checked_add(vector(data)?)?,
                    })
                })?
            }
            Self::BloblogAppended(cursor) => cursor.retained_bytes(),
            Self::BloblogPage(page) => page.entries.iter().try_fold(
                vector(&page.entries)?.checked_add(page.cursor.retained_bytes())?,
                |bytes, entry| bytes.checked_add(vector(&entry.data)?),
            )?,
            _ => 0,
        })
    }
}

impl DataCall {
    pub fn completion_bytes(&self) -> usize {
        let call = self;
        use playbit_api::abi::syscall as abi;
        // Strings and byte vectors may retain up to twice their used payload.
        let object = 2 * abi::MAX_DATA_BYTES
            + abi::MAX_VALUES as usize * size_of::<Value>()
            + abi::MAX_FIELDS as usize * size_of::<FieldChange>()
            + size_of::<ObjectRecord>();
        size_of::<DataResult>()
            + match &call.operation {
                DataOperation::KvKeys { limit, .. } => {
                    *limit as usize
                        * (size_of::<crate::store::key_value::Key>()
                            + kvdb::KEY_SIZE_MAX
                            + crate::store::key_value::PREVIEW_BYTES)
                        + kvdb::KEY_SIZE_MAX
                }
                DataOperation::KvRead { limit, .. } => *limit as usize,
                DataOperation::Query { limit, projection: Some(fields), .. } => {
                    2 * abi::MAX_DATA_BYTES
                    + *limit as usize
                        * (size_of::<ObjectRecord>()
                            + size_of::<playbit_api::abi::syscall::Id>()
                            + fields.len() * size_of::<FieldChange>())
                    // The bounded page encoding charges at least five bytes
                    // per value, even an empty string. Reserve for that bound,
                    // rather than 4,096 values for every possible row.
                    + (*limit as usize * abi::MAX_VALUES as usize).min(abi::MAX_DATA_BYTES / 5)
                        * size_of::<Value>()
                }
                DataOperation::Query { limit, projection: None, .. } => {
                    *limit as usize * size_of::<playbit_api::abi::syscall::Id>()
                }
                DataOperation::Read { .. } | DataOperation::AuthenticatedField { .. } => object,
                DataOperation::SchemaRead { .. } => {
                    abi::MAX_DATA_BYTES
                        + 256 * size_of::<crate::runtime::host::TypeDeclaration>()
                        + 4096 * size_of::<crate::runtime::host::FieldDeclaration>()
                }
                DataOperation::FileRead { limit, .. } => *limit as usize,
                DataOperation::ChannelCommand { .. } => 0,
                DataOperation::ChannelEvents { .. } => {
                    use crate::session_channel as channel;
                    channel::SESSION_QUEUE_BYTES + 64 * size_of::<channel::Event>()
                }
                DataOperation::BloblogAppend { .. } => 512,
                DataOperation::BloblogRead { .. } => {
                    2 * crate::store::bloblog::MAX_BYTES
                        + crate::store::bloblog::MAX_BLOBS
                            * size_of::<crate::store::bloblog::Entry>()
                        + 512
                }
                _ => 0,
            }
    }
}
