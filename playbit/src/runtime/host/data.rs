//! Owned data syscall inputs/results shared by local executors and engine IPC.
//! No guest pointers, storage transactions or process ownership live here.
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
use super::QueryLimits;
use super::{
    AuthenticatedFieldRead, FieldChange, FileChunk, HostError, HostResult, Id, ObjectPage,
    ObjectRecord, Schema, VolumeRights,
};
#[path = "data_bloblog.rs"]
#[cfg(any(target_arch = "wasm32", feature = "wasm-runtime", feature = "native-runtime"))]
mod bloblog;
#[path = "data_kv.rs"]
#[cfg(any(target_arch = "wasm32", feature = "wasm-runtime", feature = "native-runtime"))]
mod kv;
#[path = "data_memory.rs"]
mod memory;
#[cfg(any(target_arch = "wasm32", feature = "wasm-runtime", feature = "native-runtime"))]
pub(crate) use kv::stored_root as kv_stored_root;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "app-host",
    derive(
        serde::Serialize,
        serde::Deserialize,
        rkyv::Archive,
        rkyv::Serialize,
        rkyv::Deserialize
    )
)]
pub struct DataCall {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub volume: Id,
    pub operation: DataOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DataKind {
    SchemaRead = 1,
    SchemaEstablish = 2,
    Create = 3,
    Read = 4,
    Edit = 5,
    Query = 6,
    AuthenticatedField = 7,
    FileRead = 8,
    FileWrite = 9,
    FileTransferStart = 10,
    FileTransferChunk = 11,
    FileTransferCommit = 12,
    BloblogAppend = 13,
    BloblogRead = 14,
    ChannelEvents = 15,
    ChannelCommand = 16,
    ResolveEntry = 17,
    FileMetadata = 18,
    KvKeys = 19,
    KvRead = 20,
    KvPut = 21,
    KvDelete = 22,
    FileOpen = 23,
    FileStatus = 24,
    FileClose = 25,
    FileListOpen = 26,
    FileListNext = 27,
    FileOpenWriter = 28,
}

impl DataKind {
    pub const fn bloblog(self) -> bool {
        matches!(self, Self::BloblogAppend | Self::BloblogRead)
    }

    pub const fn network(self) -> bool {
        matches!(self, Self::ChannelEvents | Self::ChannelCommand)
    }

    pub fn from_raw(value: u32) -> HostResult<Self> {
        match value {
            1 => Ok(Self::SchemaRead),
            2 => Ok(Self::SchemaEstablish),
            3 => Ok(Self::Create),
            4 => Ok(Self::Read),
            5 => Ok(Self::Edit),
            6 => Ok(Self::Query),
            7 => Ok(Self::AuthenticatedField),
            8 => Ok(Self::FileRead),
            9 => Ok(Self::FileWrite),
            10 => Ok(Self::FileTransferStart),
            11 => Ok(Self::FileTransferChunk),
            12 => Ok(Self::FileTransferCommit),
            13 => Ok(Self::BloblogAppend),
            14 => Ok(Self::BloblogRead),
            15 => Ok(Self::ChannelEvents),
            16 => Ok(Self::ChannelCommand),
            17 => Ok(Self::ResolveEntry),
            18 => Ok(Self::FileMetadata),
            19 => Ok(Self::KvKeys),
            20 => Ok(Self::KvRead),
            21 => Ok(Self::KvPut),
            22 => Ok(Self::KvDelete),
            23 => Ok(Self::FileOpen),
            24 => Ok(Self::FileStatus),
            25 => Ok(Self::FileClose),
            26 => Ok(Self::FileListOpen),
            27 => Ok(Self::FileListNext),
            28 => Ok(Self::FileOpenWriter),
            _ => Err(HostError::Unsupported),
        }
    }

    pub const fn rights(self) -> VolumeRights {
        match self {
            Self::SchemaEstablish => VolumeRights::SCHEMA,
            Self::Create
            | Self::Edit
            | Self::FileWrite
            | Self::FileOpenWriter
            | Self::FileClose
            | Self::FileTransferStart
            | Self::FileTransferChunk
            | Self::FileTransferCommit
            | Self::BloblogAppend => VolumeRights::WRITE,
            Self::KvPut | Self::KvDelete => VolumeRights::KV_WRITE,
            _ => VolumeRights::READ,
        }
    }

    pub const fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::SchemaEstablish
                | Self::Create
                | Self::Edit
                | Self::FileWrite
                | Self::FileOpenWriter
                | Self::FileClose
                | Self::FileTransferStart
                | Self::FileTransferChunk
                | Self::FileTransferCommit
                | Self::BloblogAppend
                | Self::KvPut
                | Self::KvDelete
        )
    }
}

/// The typed representation of a service opcode and its owned arguments.
/// UI, logging, event polling, resource snapshots and selection are local.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "app-host",
    derive(
        serde::Serialize,
        serde::Deserialize,
        rkyv::Archive,
        rkyv::Serialize,
        rkyv::Deserialize
    )
)]
pub enum DataOperation {
    KvKeys {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        after: Option<Vec<u8>>,
        root: Option<crate::store::key_value::Root>,
        limit: u32,
    },
    KvRead {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        key: Vec<u8>,
        offset: u64,
        limit: u32,
        root: Option<crate::store::key_value::Root>,
    },
    KvPut {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        key: Vec<u8>,
        value: Vec<u8>,
    },
    KvDelete {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        key: Vec<u8>,
    },
    ResolveEntry {
        name: String,
    },
    SchemaRead {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
    },
    SchemaEstablish {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        schema: Schema,
    },
    Create {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        type_id: Id,
        fields: Vec<FieldChange>,
    },
    Read {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        object_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::list"))]
        fields: Vec<Id>,
    },
    Edit {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        object_id: Id,
        fields: Vec<FieldChange>,
    },
    Query {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        type_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::optional"))]
        after: Option<Id>,
        limit: u32,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::optional_list"))]
        projection: Option<Vec<Id>>,
    },
    AuthenticatedField {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        object_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        field_id: Id,
    },
    FileMetadata {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
    },
    FileRead {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        offset: u64,
        limit: u32,
    },
    FileWrite {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        bytes: Vec<u8>,
    },
    /// Trusted tools only. These have no guest ABI opcode or guest adapter.
    FileTransferStart {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
        total_bytes: u64,
    },
    FileTransferChunk {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        transfer: Id,
        offset: u64,
        bytes: Vec<u8>,
    },
    FileTransferCommit {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        transfer: Id,
    },
    BloblogAppend {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        append_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        log: [u8; 15],
        blobs: Vec<Vec<u8>>,
    },
    BloblogRead {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        log: [u8; 15],
        cursor: Option<crate::store::bloblog::Cursor>,
    },
    ChannelEvents {
        instance: [u8; 16],
    },
    ChannelCommand {
        instance: [u8; 16],
        action: u32,
        name: String,
        data: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "app-host",
    derive(
        serde::Serialize,
        serde::Deserialize,
        rkyv::Archive,
        rkyv::Serialize,
        rkyv::Deserialize
    )
)]
pub enum DataResult {
    KvKeys(crate::store::key_value::Page),
    KvRead(crate::store::key_value::Chunk),
    KvWritten,
    KvDeleted,
    EntryResolved(
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] [u8; 15],
    ),
    Schema(Schema),
    Created(#[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] Id),
    Object(ObjectRecord),
    Edited,
    Page(ObjectPage),
    AuthenticatedField(Option<AuthenticatedFieldRead>),
    File(FileChunk),
    FileMetadata {
        len_bytes: u64,
        digest: [u8; 32],
    },
    Written,
    SchemaEstablished,
    FileTransfer {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        transfer: Id,
    },
    FileTransferred {
        offset: u64,
    },
    BloblogAppended(crate::store::bloblog::Cursor),
    BloblogPage(crate::store::bloblog::Page),
    ChannelEvents(Vec<crate::session_channel::Event>),
    ChannelCompleted,
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
impl DataResult {
    /// A receipt contains only the terminal acknowledgment of accepted work.
    /// It grants no new read access after its original authority is withdrawn.
    pub(crate) fn is_receipt(&self, expected: DataKind) -> bool {
        let returned = match self {
            Self::Created(_) => Some(DataKind::Create),
            Self::Edited => Some(DataKind::Edit),
            Self::KvWritten => Some(DataKind::KvPut),
            Self::KvDeleted => Some(DataKind::KvDelete),
            Self::Written => Some(DataKind::FileWrite),
            Self::SchemaEstablished => Some(DataKind::SchemaEstablish),
            Self::FileTransfer { .. } => Some(DataKind::FileTransferStart),
            Self::FileTransferred { .. } => Some(DataKind::FileTransferChunk),
            Self::BloblogAppended(_) => Some(DataKind::BloblogAppend),
            Self::ChannelCompleted => Some(DataKind::ChannelCommand),
            _ => None,
        };
        receipt_matches(expected, returned)
    }
}
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
fn receipt_matches(expected: DataKind, returned: Option<DataKind>) -> bool {
    expected.is_mutation()
        && (returned == Some(expected)
            || (expected == DataKind::FileTransferCommit && returned == Some(DataKind::FileWrite)))
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
fn file_chunk_fits(total: u64, offset: u64, limit: u32, bytes: usize) -> bool {
    offset <= total && bytes <= limit as usize && bytes as u64 <= total - offset
}

impl DataCall {
    /// Validate a retained completion against the request which admitted it.
    /// This is bounded memory work, never a new storage read or mutation.
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    pub(crate) fn validate_result(
        &self,
        result: &DataResult,
        limits: QueryLimits,
    ) -> HostResult<()> {
        use crate::runtime::syscall::data_validation as validate;
        match (&self.operation, result) {
            (DataOperation::ResolveEntry { .. }, DataResult::EntryResolved(entry_id))
                if *entry_id != [0; 15] =>
            {
                Ok(())
            }
            (DataOperation::SchemaRead { .. }, DataResult::Schema(schema)) => {
                validate::schema(schema).map_err(validate::host_error)
            }
            (DataOperation::Create { .. }, DataResult::Created(id)) if *id != [0; 16] => Ok(()),
            (DataOperation::Read { object_id, fields, .. }, DataResult::Object(object)) => {
                if object.id != *object_id
                    || object.fields.iter().any(|field| !fields.contains(&field.field_id))
                {
                    return Err(HostError::Internal);
                }
                validate::changes(&object.fields).map_err(validate::host_error).map(|_| ())
            }
            (
                DataOperation::Query { type_id, after, limit, projection, .. },
                DataResult::Page(page),
            ) => validate::query_page(
                page,
                *type_id,
                *after,
                *limit,
                projection.as_deref(),
                limits.value_bytes,
            ),
            (
                DataOperation::AuthenticatedField { entry_id, object_id, field_id },
                DataResult::AuthenticatedField(field),
            ) => crate::runtime::syscall::authenticated_field::validate_record(
                super::EntryAddress { volume_id: self.volume, entry_id: *entry_id },
                *object_id,
                *field_id,
                field.as_ref(),
            ),
            (DataOperation::FileRead { offset, limit, .. }, DataResult::File(file)) => {
                if !file_chunk_fits(file.total_bytes, *offset, *limit, file.bytes.len()) {
                    return Err(HostError::Internal);
                }
                Ok(())
            }
            (DataOperation::SchemaEstablish { .. }, DataResult::SchemaEstablished)
            | (DataOperation::Edit { .. }, DataResult::Edited)
            | (DataOperation::FileWrite { .. }, DataResult::Written) => Ok(()),
            (
                DataOperation::KvKeys { .. }
                | DataOperation::KvRead { .. }
                | DataOperation::KvPut { .. }
                | DataOperation::KvDelete { .. },
                _,
            ) => self.validate_kv_result(result),
            (DataOperation::BloblogAppend { .. } | DataOperation::BloblogRead { .. }, _) => {
                self.validate_bloblog_result(result)
            }
            _ => self.validate_file_channel_result(result),
        }
    }

    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    fn validate_file_channel_result(&self, result: &DataResult) -> HostResult<()> {
        match (&self.operation, result) {
            (DataOperation::FileMetadata { .. }, DataResult::FileMetadata { len_bytes, .. })
                if *len_bytes <= crate::store::MAX_BLOB_PLAINTEXT_BYTES =>
            {
                Ok(())
            }
            (DataOperation::FileTransferStart { .. }, DataResult::FileTransfer { transfer })
                if *transfer != [0; 16] =>
            {
                Ok(())
            }
            (
                DataOperation::FileTransferChunk { offset, bytes, .. },
                DataResult::FileTransferred { offset: end },
            ) if offset.checked_add(bytes.len() as u64) == Some(*end) => Ok(()),
            (DataOperation::FileTransferCommit { .. }, DataResult::Written) => Ok(()),
            (DataOperation::ChannelEvents { .. }, DataResult::ChannelEvents(events)) => {
                crate::session_channel::validate_events(events).map_err(|_| HostError::Internal)
            }
            (DataOperation::ChannelCommand { .. }, DataResult::ChannelCompleted) => Ok(()),
            _ => Err(HostError::Internal),
        }
    }

    /// Execute one already context-bound request on the storage/bulk worker.
    /// This function may perform I/O. A guest begin/take must never call it.
    /// A lost reply does not authorize repeating this call.
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    pub fn execute(self, host: &mut dyn super::Host) -> HostResult<DataResult> {
        let limits = host.query_limits();
        self.operation.validate(limits)?;
        let volume = self.volume;
        let result = match &self.operation {
            DataOperation::KvKeys { .. }
            | DataOperation::KvRead { .. }
            | DataOperation::KvPut { .. }
            | DataOperation::KvDelete { .. } => self.execute_kv(host)?,
            DataOperation::ResolveEntry { name } => {
                DataResult::EntryResolved(host.resolve_entry(volume, name)?)
            }
            DataOperation::SchemaRead { .. } | DataOperation::SchemaEstablish { .. } => {
                self.execute_schema(host)?
            }
            DataOperation::Create { entry_id, type_id, fields } => DataResult::Created(
                host.create_object(address(volume, *entry_id), *type_id, fields)?,
            ),
            DataOperation::Read { entry_id, object_id, fields } => DataResult::Object(
                host.read_object(address(volume, *entry_id), *object_id, fields)?,
            ),
            DataOperation::Edit { entry_id, object_id, fields } => {
                host.edit_fields(address(volume, *entry_id), *object_id, fields)?;
                DataResult::Edited
            }
            DataOperation::Query { entry_id, type_id, after, limit, projection } => {
                DataResult::Page(host.query_objects(
                    address(volume, *entry_id),
                    *type_id,
                    *after,
                    *limit,
                    projection.as_deref(),
                )?)
            }
            DataOperation::AuthenticatedField { entry_id, object_id, field_id } => {
                DataResult::AuthenticatedField(host.read_authenticated_field(
                    address(volume, *entry_id),
                    *object_id,
                    *field_id,
                )?)
            }
            DataOperation::FileMetadata { entry_id } => {
                let (len_bytes, digest) = host.file_metadata(address(volume, *entry_id))?;
                DataResult::FileMetadata { len_bytes, digest }
            }
            DataOperation::FileRead { entry_id, offset, limit } => {
                DataResult::File(host.read_file(address(volume, *entry_id), *offset, *limit)?)
            }
            DataOperation::FileWrite { entry_id, bytes } => {
                host.write_file(address(volume, *entry_id), bytes)?;
                DataResult::Written
            }
            DataOperation::FileTransferStart { .. }
            | DataOperation::FileTransferChunk { .. }
            | DataOperation::FileTransferCommit { .. } => return Err(HostError::Unsupported),
            DataOperation::ChannelEvents { .. } | DataOperation::ChannelCommand { .. } => {
                return Err(HostError::Unsupported);
            }
            DataOperation::BloblogAppend { log, append_id, blobs } => DataResult::BloblogAppended(
                host.bloblog_append(address(volume, *log), *append_id, blobs)?,
            ),
            DataOperation::BloblogRead { log, cursor } => {
                DataResult::BloblogPage(host.bloblog_read(address(volume, *log), cursor.as_ref())?)
            }
        };
        self.validate_result(&result, limits)?;
        Ok(result)
    }

    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    fn execute_schema(&self, host: &mut dyn super::Host) -> HostResult<DataResult> {
        match &self.operation {
            DataOperation::SchemaRead { entry_id } => {
                Ok(DataResult::Schema(host.schema(address(self.volume, *entry_id))?))
            }
            DataOperation::SchemaEstablish { entry_id, schema } => {
                host.establish_schema(address(self.volume, *entry_id), schema)?;
                Ok(DataResult::SchemaEstablished)
            }
            _ => Err(HostError::Internal),
        }
    }
}

#[cfg(all(not(target_arch = "wasm32"), any(feature = "wasm-runtime", feature = "native-runtime")))]
fn address(volume_id: Id, entry_id: [u8; 15]) -> super::EntryAddress {
    super::EntryAddress { volume_id, entry_id }
}

impl DataOperation {
    pub fn entry_id(&self) -> Option<[u8; 15]> {
        match self {
            Self::KvKeys { entry_id, .. }
            | Self::KvRead { entry_id, .. }
            | Self::KvPut { entry_id, .. }
            | Self::KvDelete { entry_id, .. }
            | Self::SchemaRead { entry_id }
            | Self::SchemaEstablish { entry_id, .. }
            | Self::Create { entry_id, .. }
            | Self::Read { entry_id, .. }
            | Self::Edit { entry_id, .. }
            | Self::Query { entry_id, .. }
            | Self::AuthenticatedField { entry_id, .. }
            | Self::FileMetadata { entry_id }
            | Self::FileRead { entry_id, .. }
            | Self::FileWrite { entry_id, .. }
            | Self::FileTransferStart { entry_id, .. } => Some(*entry_id),
            Self::BloblogAppend { log, .. } | Self::BloblogRead { log, .. } => Some(*log),
            _ => None,
        }
    }

    pub fn kind(&self) -> DataKind {
        match self {
            Self::KvKeys { .. } => DataKind::KvKeys,
            Self::KvRead { .. } => DataKind::KvRead,
            Self::KvPut { .. } => DataKind::KvPut,
            Self::KvDelete { .. } => DataKind::KvDelete,
            Self::ResolveEntry { .. } => DataKind::ResolveEntry,
            Self::SchemaRead { .. } => DataKind::SchemaRead,
            Self::SchemaEstablish { .. } => DataKind::SchemaEstablish,
            Self::Create { .. } => DataKind::Create,
            Self::Read { .. } => DataKind::Read,
            Self::Edit { .. } => DataKind::Edit,
            Self::Query { .. } => DataKind::Query,
            Self::AuthenticatedField { .. } => DataKind::AuthenticatedField,
            Self::FileMetadata { .. } => DataKind::FileMetadata,
            Self::FileRead { .. } => DataKind::FileRead,
            Self::FileWrite { .. } => DataKind::FileWrite,
            Self::FileTransferStart { .. } => DataKind::FileTransferStart,
            Self::FileTransferChunk { .. } => DataKind::FileTransferChunk,
            Self::FileTransferCommit { .. } => DataKind::FileTransferCommit,
            Self::BloblogAppend { .. } => DataKind::BloblogAppend,
            Self::BloblogRead { .. } => DataKind::BloblogRead,
            Self::ChannelEvents { .. } => DataKind::ChannelEvents,
            Self::ChannelCommand { .. } => DataKind::ChannelCommand,
        }
    }

    pub fn rights(&self) -> VolumeRights {
        self.kind().rights()
    }
    pub fn is_mutation(&self) -> bool {
        self.kind().is_mutation()
    }
    pub const fn tool_only(&self) -> bool {
        match self {
            Self::FileTransferStart { .. }
            | Self::FileTransferChunk { .. }
            | Self::FileTransferCommit { .. } => true,
            Self::ChannelEvents { .. } | Self::ChannelCommand { .. } => false,
            _ => false,
        }
    }

    pub fn network(&self) -> bool {
        self.kind().network()
    }

    /// Semantic input admission shared by the guest adapter and service.
    /// Authorization is separate: the service must resolve the bound context
    /// and recheck current authority before execution and result delivery.
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    pub(crate) fn validate(&self, limits: QueryLimits) -> HostResult<()> {
        use crate::runtime::syscall::data_validation;
        limits.validate()?;
        if self.entry_id() == Some([0; 15]) {
            return Err(HostError::InvalidArgument);
        }
        match self {
            Self::KvKeys { .. }
            | Self::KvRead { .. }
            | Self::KvPut { .. }
            | Self::KvDelete { .. } => self.validate_kv(),
            Self::ResolveEntry { name } => crate::store::volume_index::validate_name(name)
                .map(|_| ())
                .map_err(|_| HostError::InvalidArgument),
            Self::SchemaRead { .. } | Self::FileMetadata { .. } => Ok(()),
            Self::AuthenticatedField { object_id, field_id, .. } => {
                if *object_id == [0; 16] || *field_id == [0; 16] {
                    return Err(HostError::InvalidArgument);
                }
                Ok(())
            }
            Self::ChannelEvents { .. } => Ok(()),
            Self::ChannelCommand { name, data, action, .. } => {
                crate::session_channel::validate_command(name, *action, data.len())
            }
            Self::SchemaEstablish { schema, .. } => data_validation::schema(schema),
            Self::Create { fields, .. } => data_validation::changes(fields).map(|_| ()),
            Self::Read { fields, .. } => data_validation::ids(fields),
            Self::Edit { fields, .. } => {
                data_validation::changes(fields)?;
                if fields.is_empty() {
                    return Err(HostError::InvalidArgument);
                }
                Ok(())
            }
            Self::Query { limit, projection, .. } => {
                if *limit == 0 {
                    return Err(HostError::InvalidArgument);
                }
                if *limit > limits.records {
                    return Err(HostError::LimitExceeded);
                }
                if let Some(fields) = projection {
                    data_validation::ids(fields)?;
                }
                Ok(())
            }
            Self::FileRead { .. }
            | Self::FileWrite { .. }
            | Self::FileTransferStart { .. }
            | Self::FileTransferChunk { .. }
            | Self::FileTransferCommit { .. } => self.validate_file(),
            Self::BloblogAppend { blobs, .. } => {
                playbit_api::abi::bloblog::validate_lengths(blobs.iter().map(Vec::len))
            }
            Self::BloblogRead { cursor, .. } => {
                if let Some(cursor) = cursor {
                    crate::store::bloblog::Cursor::validate(cursor.as_str())
                        .map_err(|_| HostError::InvalidArgument)?;
                }
                Ok(())
            }
        }
    }
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
    fn validate_file(&self) -> HostResult<()> {
        use playbit_api::abi::syscall as abi;
        match self {
            Self::FileRead { offset, limit, .. } => {
                if *limit == 0 {
                    return Err(HostError::InvalidArgument);
                }
                if *limit as usize > abi::MAX_DATA_BYTES {
                    return Err(HostError::LimitExceeded);
                }
                offset.checked_add(u64::from(*limit)).ok_or(HostError::InvalidArgument)?;
                Ok(())
            }
            Self::FileWrite { bytes, .. } => {
                if bytes.len() > abi::MAX_DATA_BYTES {
                    return Err(HostError::LimitExceeded);
                }
                Ok(())
            }
            Self::FileTransferStart { .. } | Self::FileTransferCommit { .. } => Ok(()),
            Self::FileTransferChunk { offset, bytes, .. } => {
                if bytes.is_empty() {
                    return Err(HostError::InvalidArgument);
                }
                if bytes.len() > abi::MAX_DATA_BYTES {
                    return Err(HostError::LimitExceeded);
                }
                offset.checked_add(bytes.len() as u64).ok_or(HostError::InvalidArgument)?;
                Ok(())
            }
            _ => unreachable!("file input validation group"),
        }
    }
}

#[cfg(all(
    test,
    feature = "local-store",
    any(feature = "wasm-runtime", feature = "native-runtime")
))]
#[path = "data_tests.rs"]
mod tests;
