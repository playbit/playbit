//! Typed host service contract and app context, independent of VM memory.
//!
//! Hosts bind a verified principal and recheck current store authority for
//! every operation. Runtime capability handles add an instance-scoped ceiling;
//! app-supplied IDs never establish authority. All work here is bounded local
//! work; long operations use generation-scoped completion notifications.

use std::collections::{BTreeMap, BTreeSet};

use playbit_api::abi::syscall::{Error as HostError, Id, Opcode};
pub type HostResult<T> = Result<T, HostError>;

mod generation;
mod limits;
pub use limits::QueryLimits;
mod data;
pub mod files;
#[cfg(any(target_arch = "wasm32", feature = "wasm-runtime", feature = "native-runtime"))]
pub(crate) use data::kv_stored_root;
#[cfg(feature = "app-host")]
pub use data::{ArchivedDataCall, ArchivedDataOperation};
pub use data::{DataCall, DataKind, DataOperation, DataResult};
/// Retained owned request/result payload per guest; transport credit is separate.
pub const MAX_RETAINED_DATA_BYTES: usize = 16 * 1024 * 1024;

/// Complete database identity in the volume addressing context. No authority is
/// carried by this value; every host operation checks the current entry ACL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntryAddress {
    pub volume_id: Id,
    pub entry_id: [u8; 15],
}

pub use crate::capability::Rights as VolumeRights;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    feature = "app-host",
    derive(serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct AvailableVolume {
    /// One validated backing kind, using the public Volume flag encoding.
    pub backing: u32,
    /// Persisted consent incarnation used by trusted root admission. Zero is
    /// reserved for session-bound or embedding-only metadata.
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub source_epoch: [u8; 16],
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub id: Id,
    pub name: String,
    pub tags: Vec<String>,
    pub rights: VolumeRights,
    pub is_root: bool,
    /// Public ABI VolumeRole tag, or zero for an explicit/embedding mount.
    pub role: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub account_id: Id,
    pub is_service: bool,
    pub device_id: Id,
    pub instance_id: Id,
    pub session_id: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Participant {
    pub account_id: Id,
    pub device_id: Id,
    pub instance_id: Id,
    pub build_id: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct GuestContext {
    /// Private display label supplied with this app's admitted package.
    pub app_name: String,
    /// Exact admitted package identity; unavailable for bare embedding.
    pub app_build: Option<[u8; 32]>,
    pub identity: Identity,
    pub volumes: Vec<AvailableVolume>,
    pub startup_arguments: BTreeMap<String, String>,
    pub trace_catalog: Option<std::sync::Arc<[u8]>>,
    pub required_capabilities: BTreeSet<String>,
    pub participants: Vec<Participant>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
pub struct Schema {
    pub types: Vec<TypeDeclaration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
pub struct TypeDeclaration {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub id: Id,
    pub name: String,
    pub deprecated: bool,
    pub fields: Vec<FieldDeclaration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
pub struct FieldDeclaration {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub id: Id,
    pub name: String,
    pub value_type: ValueType,
    pub repeated: bool,
    pub deprecated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub enum ValueType {
    Text,
    Boolean,
    Integer,
    Float,
    Bytes,
    Reference(#[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] Id),
    File,
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
pub enum Value {
    Text(String),
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Bytes(Vec<u8>),
    Reference {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        volume_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        database_id: [u8; 15],
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        object_id: Id,
    },
    /// Stable reference to an independent blob; conveys no read authority.
    File {
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        volume_id: Id,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        entry_id: [u8; 15],
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
pub struct FieldChange {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub field_id: Id,
    pub values: Option<Vec<Value>>,
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
pub struct ObjectRecord {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub id: Id,
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub type_id: Id,
    pub fields: Vec<FieldChange>,
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
pub struct ObjectPage {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::list"))]
    pub objects: Vec<Id>,
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::optional"))]
    pub next: Option<Id>,
    pub records: Option<Vec<ObjectRecord>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
pub struct FileChunk {
    pub total_bytes: u64,
    pub bytes: Vec<u8>,
}

/// Scoped source and existing blob destination. Mount handles are resolved by the
/// runtime; the host must recheck current entry authority before copying.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileCopyRequest {
    pub source: EntryAddress,
    pub destination: EntryAddress,
}

/// An explicitly selected blob. Selection confers no mount or Read grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileSelection {
    pub address: EntryAddress,
}

/// Local bounded operations only: do not wait for network I/O in a syscall.
/// Return WouldBlock/Unavailable and arrange a generation-scoped event instead.
/// The advertised set must contain only host operations actually implemented.
///
/// A host belongs to its execution adapter's owning thread. Native apps may
/// directly own thread-affine platform resources; adapters that move hosts must
/// impose Send at that transfer boundary instead of on every implementation.
pub trait Host {
    /// Shared frontend/helper quota, established before admitting backend roots.
    /// File-enabled hosts retain and return the same capacity for their lifetime.
    fn file_capacity(&self) -> std::sync::Arc<files::capacity::Capacity> {
        std::sync::Arc::new(files::capacity::Capacity::default())
    }

    /// Return the complete already-admitted root set (at most MAX_VOLUMES).
    /// Unchanged roots retain their engine/grant IDs; replacing rights requires
    /// fresh IDs and a File credit reserved before backend creation. New handles transfer to the runtime even if batch validation
    /// fails, and are discarded there. Existing handles remain runtime-owned.
    /// This callback only moves bounded memory; filesystem and namespace
    /// admission happen outside guest turns.
    fn file_roots(&mut self, _volumes: &[AvailableVolume]) -> HostResult<Vec<files::Root>> {
        Ok(Vec::new())
    }
    /// Allocate before any guest execution. A remote adapter obtains this from
    /// the parent, so a new child process cannot reuse an old generation.
    fn allocate_generation(&mut self) -> HostResult<u32> {
        generation::allocate()
    }

    fn supported_operations(&self) -> &[Opcode];
    /// Presentation features are admitted independently of syscall identities.
    fn supported_features(&self) -> &[&str] {
        &[]
    }
    /// Only a local presentation owner may enable sound output.
    fn supports_audio(&self) -> bool {
        false
    }
    /// Existing owner readiness wake, used after audio device initialization.
    /// No guest code runs on the device worker.
    fn audio_wake(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> {
        None
    }
    fn retire_channels(&mut self, _instance: Id) {}
    fn channel_consumed(
        &mut self,
        _recipient: Id,
        _account: Id,
        _sender: Id,
        _message: Id,
    ) -> HostResult<()> {
        Ok(())
    }

    /// Own a bounded request and enqueue it without doing I/O on this thread.
    /// Tokens never repeat within the generation. Notify data readiness after
    /// storing a terminal result; ordinary exchange closure does not retire
    /// this logical operation. Admitted mutations survive guest disposal.
    fn begin_data(&mut self, _generation: u64, _call: std::sync::Arc<DataCall>) -> HostResult<u64> {
        Err(HostError::Unsupported)
    }

    /// Inspect retained memory only. Recheck the locally applied policy epoch
    /// before delivery. WouldBlock retains the operation. Do not retry I/O here,
    /// including after a lost reply with an uncertain mutation outcome.
    fn peek_data(&mut self, _token: u64) -> HostResult<std::sync::Arc<DataResult>> {
        Err(HostError::Unsupported)
    }

    /// Release local request/result credit. This does not cancel engine-owned
    /// work or perform I/O. Hosts implementing begin_data own this lifecycle.
    fn release_data(&mut self, _token: u64) {}

    /// File admission and delivery touch retained memory only. The host orders
    /// submissions on each File and retains accepted mutations after disposal.
    fn begin_file(
        &mut self,
        _generation: u64,
        _call: std::sync::Arc<files::Call>,
        _retention: files::capacity::Retention,
    ) -> HostResult<u64> {
        Err(HostError::Unsupported)
    }
    fn peek_file(&mut self, _token: u64) -> HostResult<std::sync::Arc<files::Output>> {
        Err(HostError::Unsupported)
    }
    /// `claimed` transfers a returned File/List/recovery capability. Otherwise
    /// dispose it, including results that arrive after this call.
    fn release_file(&mut self, _token: u64, _claimed: bool) {}
    /// Enqueue descriptor cleanup without waiting. The runtime retires its local
    /// capability first. Accepted normal closes are never replaced by discard.
    fn discard_file(
        &mut self,
        _file: u64,
        _list: bool,
        _retention: files::capacity::Retention,
    ) -> HostResult<()> {
        Err(HostError::Unsupported)
    }

    /// Captured and validated once, before the app runs. A host with smaller
    /// storage/workspace budgets must report those here and bound allocations
    /// before producing query results; runtime output checks are a second guard.
    fn query_limits(&self) -> QueryLimits {
        QueryLimits::default()
    }

    /// Maximum encoded scene bytes, captured before app callbacks. The runtime
    /// rejects larger submissions before decoding or retaining the scene.
    /// Renderer feature, node and text admission remain separate constraints.
    fn view_bytes_limit(&self) -> u32 {
        playbit_api::abi::syscall::MAX_CALL_BYTES as u32
    }

    fn bloblog_append(
        &mut self,
        _entry: EntryAddress,
        _append_id: Id,
        _blobs: &[Vec<u8>],
    ) -> HostResult<crate::store::bloblog::Cursor> {
        Err(HostError::Unsupported)
    }
    fn bloblog_read(
        &mut self,
        _entry: EntryAddress,
        _cursor: Option<&crate::store::bloblog::Cursor>,
    ) -> HostResult<crate::store::bloblog::Page> {
        Err(HostError::Unsupported)
    }
    fn kv_keys(
        &mut self,
        _entry: EntryAddress,
        _after: Option<&[u8]>,
        _root: Option<crate::store::key_value::Root>,
        _limit: u32,
    ) -> HostResult<crate::store::key_value::Page> {
        Err(HostError::Unsupported)
    }
    fn kv_read(
        &mut self,
        _entry: EntryAddress,
        _key: &[u8],
        _offset: u64,
        _limit: u32,
        _root: Option<crate::store::key_value::Root>,
    ) -> HostResult<crate::store::key_value::Chunk> {
        Err(HostError::Unsupported)
    }
    fn kv_put(&mut self, _entry: EntryAddress, _key: &[u8], _value: &[u8]) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn kv_delete(&mut self, _entry: EntryAddress, _key: &[u8]) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn resolve_entry(&mut self, _volume: Id, _name: &str) -> HostResult<[u8; 15]> {
        Err(HostError::Unsupported)
    }
    fn schema(&mut self, _volume: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
        Err(HostError::Unsupported)
    }
    fn establish_schema(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _schema: &Schema,
    ) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn create_object(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _type_id: Id,
        _changes: &[FieldChange],
    ) -> HostResult<Id> {
        Err(HostError::Unsupported)
    }
    /// Enqueue a bounded copy worker; never wait for network or file transfer
    /// here. Tokens are unique within this operation kind and guest generation,
    /// including completed work. A destination object must already exist.
    fn begin_file_copy(&mut self, _generation: u64, _request: FileCopyRequest) -> HostResult<u64> {
        Err(HostError::Unsupported)
    }
    /// Inspect a local result and recheck delivery-time authorization. Success
    /// returns the original destination entry ID; WouldBlock retains the work.
    fn poll_file_copy(&mut self, _token: u64) -> HostResult<[u8; 15]> {
        Err(HostError::Unsupported)
    }
    /// Enqueue native selection without blocking or opening native UI here.
    /// Tokens must be unique within this kind and generation, including finished
    /// work. The native host obtains explicit user selection or cancellation.
    fn begin_file_selection(&mut self, _generation: u64) -> HostResult<u64> {
        Err(HostError::Unsupported)
    }
    /// Inspect a local result. None means user cancellation. Before returning a
    /// file, recheck its current entry Read permission.
    /// Selection must not implicitly add a guest mount or broaden access.
    fn poll_file_selection(&mut self, _token: u64) -> HostResult<Option<FileSelection>> {
        Err(HostError::Unsupported)
    }
    fn read_object(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _object_id: Id,
        _fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        Err(HostError::Unsupported)
    }
    /// Read one field's current values, revision and verified provenance from
    /// one atomic snapshot. None means no stored field; a present record with
    /// values None is a tombstone. Recheck current object Read and enrollment on
    /// every call, including output-size probes. Never infer an author from
    /// guest values, the current reader, or a key-rotation writer.
    fn read_authenticated_field(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _object_id: Id,
        _field_id: Id,
    ) -> HostResult<Option<AuthenticatedFieldRead>> {
        Err(HostError::Unsupported)
    }
    fn edit_fields(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _object_id: Id,
        _changes: &[FieldChange],
    ) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn query_objects(
        &mut self,
        _volume: crate::runtime::host::EntryAddress,
        _type_id: Id,
        _after: Option<Id>,
        _limit: u32,
        _projection: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        Err(HostError::Unsupported)
    }
    fn file_metadata(&mut self, _address: EntryAddress) -> HostResult<(u64, [u8; 32])> {
        Err(HostError::Unsupported)
    }
    fn read_file(
        &mut self,
        _address: EntryAddress,
        _offset: u64,
        _limit: u32,
    ) -> HostResult<FileChunk> {
        Err(HostError::Unsupported)
    }
    fn write_file(&mut self, _address: EntryAddress, _bytes: &[u8]) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
}

/// An account kind established by the verified host, never a guest assertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub enum Principal {
    Human(#[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] Id),
    Service(#[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] Id),
}

/// Opaque identity of this exact authenticated field revision. It is not a
/// timestamp or a sort key and changes when the encrypted revision changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub struct FieldRevision(
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))] pub [u8; 32],
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub enum FieldProvenance {
    /// The independently verified signer of this application field edit.
    ApplicationEdit {
        principal: Principal,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        device_id: Id,
    },
    /// The independently verified rekey writer. This identity makes no claim
    /// about who originally submitted the preserved field value.
    Rekey {
        principal: Principal,
        #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
        device_id: Id,
    },
    /// No application author identity is proven for this field revision.
    Unproven,
}

/// One atomic, currently authorized field read. An absent value is a stored
/// tombstone; a field with no stored revision is the Host method's outer None.
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
pub struct AuthenticatedFieldRead {
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub volume_id: Id,
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub entry_id: [u8; 15],
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub object_id: Id,
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub type_id: Id,
    #[cfg_attr(feature = "app-host", serde(with = "crate::identifier::serde"))]
    pub field_id: Id,
    pub values: Option<Vec<Value>>,
    pub revision: FieldRevision,
    pub provenance: FieldProvenance,
}
