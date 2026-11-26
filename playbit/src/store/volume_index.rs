//! Persistent volume entries and a local, transactionally derived name index.
//!
//! Callers authorize source changes before invoking the resolver. Creation facts,
//! independent name/content registers,
//! immutable revisions, permission overrides and permanent retirement facts
//! replicate; names, candidates and reverse names do not. Repair cannot mutate
//! the source namespace or confer authority.
use crate::store::catalog::record::{self, decode, encode};
use crate::store::object_contract::{Acl, EntryAcl, Permission, Principal, Schema};
use crate::store::{DataActor, EntryId, Error, Id, Result, Volume};
use kvdb::sync::{Delta, EntState, Resolution};
use kvdb::{LocalIndexes, ReadTxn, Resolver, Storage, WriteTxn};
use serde::{Deserialize, Serialize};
mod current;
#[cfg(feature = "connected-engine")]
pub(crate) use current::Retirement;
mod directory;
mod headers;
mod history;
pub use history::{HistoryCursor, HistoryPage};
pub mod namespace;
mod path_creation;
mod projection;
#[cfg(any(feature = "connected-engine", target_arch = "wasm32"))]
mod publication;
#[cfg(any(feature = "local-store", test))]
pub(crate) use projection::NamespaceCache;
#[cfg(any(feature = "connected-engine", target_arch = "wasm32"))]
pub(crate) use publication::blob_versions as published_blob_versions;
#[cfg(feature = "connected-engine")]
pub(crate) use publication::changes_permissions;
mod reconciliation;
mod recovery;
mod schema;
mod summaries;
use crate::store::MetadataPolicy;
pub use headers::{EntryHeader, HeaderPage};
pub use reconciliation::rejected_sources;
#[cfg(any(target_arch = "wasm32", feature = "local-store"))]
pub(crate) use reconciliation::visit_rejected_sources;
pub use recovery::{RecoveryCursor, RejectedEntry, RejectedEntryPage};

pub const ENTRY_ID_BYTE: u8 = 1;
pub const ENTRY_NAME_BYTE: u8 = 2;
pub const META_BYTE: u8 = 3;
const RETIRED: u8 = 1;
const PERMISSIONS: u8 = 2;
const REVISION: u8 = 3;
const CANDIDATE: u8 = 4;
const REVERSE_NAME: u8 = 5;
const BACKING_READY: u8 = 6;
const QUARANTINED_BACKING: &[u8] = b"unaccepted-backing/";
const PERMISSION_PUBLICATION_PREFIX: &[u8] = b"permission-publication/";
const PERMISSION_PUBLICATION_MAGIC: &[u8; 4] = b"EPP\x01";
const PERMISSION_PUBLICATION_BYTES: usize = 4 + 16 + 8 + 32;
const NAME: u8 = 7;
const CONTENT: u8 = 8;
const PARENT: u8 = 13;
pub const MAX_ENTRY_COMPONENT_BYTES: usize = namespace::MAX_COMPONENT_BYTES;
/// Admission ceiling, not a supported schema capacity promise.
pub const MAX_ENTRY_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_ENTRY_PAGE: usize = 64;
pub const MAX_ENTRY_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_REPAIR_IDS: usize = 64;

pub fn is_index_key(key: &[u8]) -> bool {
    matches!(key.first(), Some(&ENTRY_ID_BYTE) | Some(&ENTRY_NAME_BYTE))
        || (key.first() == Some(&META_BYTE)
            && key.get(1).is_some_and(|byte| (1..=11).contains(byte) || *byte == PARENT))
}

pub fn validate_record(key: &[u8], value: Option<&[u8]>) -> Result<EntryId> {
    let id = source_entry_id(key)?;
    validate_value(key, value.ok_or(Error::AccessDenied("volume entry facts cannot be erased"))?)?;
    Ok(id)
}

pub fn read_sync_value(value: &dyn kvdb::storage::ValueRead) -> Result<Vec<u8>> {
    read_sync_value_bounded(value, MAX_ENTRY_RECORD_BYTES)
}

pub(crate) fn read_sync_value_bounded(
    value: &dyn kvdb::storage::ValueRead,
    maximum: usize,
) -> Result<Vec<u8>> {
    if value.len() > maximum {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = vec![0; value.len()];
    value.read_exact_at(0, &mut bytes)?;
    Ok(bytes)
}

pub fn validate_sync_source(delta: &kvdb::sync::SyncDelta<'_>) -> Result<EntryId> {
    with_sync_delta(delta, validate_source)
}

pub fn with_sync_delta<T>(
    delta: &kvdb::sync::SyncDelta<'_>,
    inspect: impl FnOnce(&Delta<'_>) -> Result<T>,
) -> Result<T> {
    with_sync_delta_bounded(delta, MAX_ENTRY_RECORD_BYTES, inspect)
}

pub(crate) fn with_sync_delta_bounded<T>(
    delta: &kvdb::sync::SyncDelta<'_>,
    maximum: usize,
    inspect: impl FnOnce(&Delta<'_>) -> Result<T>,
) -> Result<T> {
    use kvdb::sync::SyncEntState;
    let local = match delta.local {
        SyncEntState::Present { value, .. } => Some(read_sync_value_bounded(value, maximum)?),
        _ => None,
    };
    let remote = match delta.remote {
        SyncEntState::Present { value, .. } => Some(read_sync_value_bounded(value, maximum)?),
        _ => None,
    };
    fn state<'a>(original: SyncEntState<'_>, bytes: Option<&'a [u8]>) -> EntState<'a> {
        match original {
            SyncEntState::Missing => EntState::Missing,
            SyncEntState::Deleted { clock } => EntState::Deleted { clock },
            SyncEntState::Present { clock, .. } => EntState::Present {
                clock,
                value: bytes.expect("present state has a bounded copied value"),
            },
        }
    }
    inspect(&Delta {
        key: delta.key,
        local: state(delta.local, local.as_deref()),
        remote: state(delta.remote, remote.as_deref()),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum EntryKind {
    Directory {},
    KvDb {},
    ObjectDb { schema: Schema },
    Blob { len_bytes: u64, digest: [u8; 32], encrypted_key: [u8; 32], published_at_micros: i64 },
}

/// The existing opaque append log is a named kv database, with its transport
/// semantics explicit in the authoritative head. Ordinary kvdb sync must never
/// ingest its arrival clocks, pending markers or remote checkpoints.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum DatabaseProtocol {
    Kvdb,
    BlobLog,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntryInfo {
    pub namespace: namespace::Birth,
    pub origin: namespace::Origin,
    pub initial_permissions: EntryAcl,
    pub created_revision: EntryId,
    pub name: String,
    pub kind: EntryKind,
    pub protocol: Option<DatabaseProtocol>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct VolumeEntry {
    pub id: EntryId,
    pub info: EntryInfo,
    pub name_revision: EntryId,
    pub content_revision: EntryId,
    pub deleted: bool,
    pub name_conflict: bool,
}

/// Catalog navigation metadata; full schema, ACL and content require details.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntrySummary {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub name: String,
    pub kind: crate::store::EntryType,
    pub protocol: Option<DatabaseProtocol>,
    pub deleted: bool,
    pub name_conflict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntryPage {
    pub entries: Vec<EntrySummary>,
    pub next: Option<EntryId>,
}

/// A native browser target; Entry resolves its effective parent after a move.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum DirectoryTarget {
    Root,
    Directory(EntryId),
    Entry(EntryId),
}

/// Effective canonical directory metadata from the same cut as its child page.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct DirectoryAncestor {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub name: String,
}

/// A changed cut is a restart, never evidence of an empty directory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "listing", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum DirectoryListing {
    Page {
        cut: u64,
        parent: Option<EntryId>,
        ancestors: Vec<DirectoryAncestor>,
        entries: Vec<EntrySummary>,
        next: Option<EntryId>,
    },
    Changed {
        cut: u64,
    },
}

/// A process-local observation of a readable private file revision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct FileAvailability {
    /// Last notice included in this observation, including unrelated files.
    pub through: u64,
    /// Latest matching file or reconnect notice, or the end of discarded history.
    pub invalidated: u64,
}

/// Known-ID editing metadata requires catalog Read, retained entry Read, or
/// current Write/Append. Content and history retain their separate Read checks;
/// an excluded owner can repair an override without gaining content access.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntryDetails {
    pub entry: VolumeEntry,
    /// Local IPC observation for an open private file. Never persists or conveys authority.
    #[serde(skip)]
    pub file_availability: FileAvailability,
    pub permissions: EntryAcl,
    /// Device-local configured grants, including when volume defaults are active.
    #[serde(default)]
    pub retained_permissions: Option<Acl>,
    /// The immutable birth's originating HLC; zero means unavailable in older replies.
    #[serde(default)]
    pub created_clock: u64,
    pub default_permissions: Acl,
    pub backing_ready: bool,
    pub schema_pending: bool,
    pub readable: bool,
    pub retained_readable: bool,
    pub recoverable: bool,
    pub writable: bool,
    pub appendable: bool,
    pub manage_permissions: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RevisionPage {
    pub revisions: Vec<EntryRevision>,
    pub next: Option<EntryId>,
}

/// A retained operation, rather than a synthetic whole-entry revision. Name and
/// content updates never carry each other's state. The origin clock is bound in
/// the canonical bytes as well as the kvdb envelope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntryRevision {
    // Equal-clock LWW compares the fixed-width revision ID first.
    pub revision: EntryId,
    pub creation: [u8; 32],
    pub origin_clock: u64,
    pub origin: namespace::Origin,
    #[serde(with = "namespace::causal_context")]
    pub observed: Vec<EntryId>,
    pub change: EntryChange,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum EntryChange {
    Created { info: Box<EntryInfo> },
    Name { name: String },
    Move { parent: Option<EntryId>, name: Option<String> },
    Delete {},
    Content { kind: EntryKind },
    Schema { declaration: Schema },
}

/// Cross-volume references carry both identities. Rename never changes either.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EntryAddress {
    pub volume: Id,
    pub entry: EntryId,
}

/// One immutable, index-declared body. A version address confers no authority;
/// every use checks current permissions. Selecting a new view also requires a
/// live entry, while an explicitly captured revision survives retirement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlobVersion {
    pub address: EntryAddress,
    pub revision: EntryId,
    pub len_bytes: u64,
    pub digest: [u8; 32],
    pub encrypted_key: [u8; 32],
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.len() > namespace::MAX_PATH_BYTES || name.split('/').count() > namespace::MAX_DEPTH {
        return Err(Error::LimitExceeded);
    }
    for component in name.split('/') {
        namespace::component(component)?;
    }
    Ok(())
}

impl EntryInfo {
    pub fn validate(&self) -> Result<()> {
        self.origin.validate()?;
        self.namespace.validate(matches!(self.kind, EntryKind::Directory {}))?;
        if self.namespace.creation_context.is_some() && !matches!(self.kind, EntryKind::Blob { .. })
        {
            return Err(Error::InvalidInput("only files retain a writer creation context"));
        }
        namespace::component(&self.name)?;
        if let EntryAcl::Replace(acl) = &self.initial_permissions {
            acl.validate()?;
        }
        match (&self.kind, self.protocol) {
            (EntryKind::KvDb {}, Some(_))
            | (EntryKind::ObjectDb { .. }, Some(DatabaseProtocol::Kvdb))
            | (EntryKind::Blob { .. } | EntryKind::Directory {}, None) => {}
            _ => return Err(Error::InvalidInput("entry kind and database protocol disagree")),
        }
        if let EntryKind::ObjectDb { schema } = &self.kind {
            schema.validate()?;
        }
        Ok(())
    }

    fn canonical(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = encode(self)?;
        if bytes.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
}

fn info(bytes: &[u8]) -> Result<EntryInfo> {
    if bytes.len() > MAX_ENTRY_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    // Charge container growth before serde's tagged-enum allocations, including
    // records which will lose conflict resolution.
    crate::store::catalog::preflight_index_record(bytes)?;
    let record: EntryInfo = decode(bytes)?;
    record.validate()?;
    Ok(record)
}

impl EntryRevision {
    fn canonical(&self) -> Result<Vec<u8>> {
        namespace::validate_context(&self.observed)?;
        self.origin.validate()?;
        if self.origin_clock == 0 {
            return Err(Error::InvalidInput("entry revision requires an origin clock"));
        }
        match &self.change {
            EntryChange::Created { info } => info.validate()?,
            EntryChange::Delete {} => {}
            EntryChange::Name { name } => namespace::component(name)?,
            EntryChange::Move { name, .. } => {
                if let Some(name) = name {
                    namespace::component(name)?;
                }
            }
            EntryChange::Schema { declaration } => declaration.validate()?,
            EntryChange::Content { kind: EntryKind::Blob { .. } } => {}
            EntryChange::Content {
                kind: EntryKind::KvDb {} | EntryKind::ObjectDb { .. } | EntryKind::Directory {},
            } => {
                return Err(Error::InvalidInput(
                    "database content metadata is not a blob register",
                ));
            }
        }
        let bytes = encode(self)?;
        if bytes.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
}

fn revision(bytes: &[u8]) -> Result<EntryRevision> {
    if bytes.len() > MAX_ENTRY_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    crate::store::catalog::preflight_index_record(bytes)?;
    let record: EntryRevision = decode(bytes)?;
    record.canonical()?;
    Ok(record)
}

fn creation_digest(id: EntryId, clock: kvdb::Clock, bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"app.playbit.entry.creation.v1\0");
    hash.update(id.as_bytes());
    hash.update(&clock.as_u64().to_le_bytes());
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

fn validate_dependency(
    declared_kind: std::mem::Discriminant<EntryKind>,
    created_revision: EntryId,
    creation: [u8; 32],
    clock: kvdb::Clock,
    update: &EntryRevision,
) -> Result<()> {
    if update.creation != creation
        || update.origin_clock <= clock.as_u64()
        || update.revision == created_revision
    {
        return Err(Error::AccessDenied("entry update does not follow its creation"));
    }
    if let EntryChange::Content { kind } = &update.change
        && std::mem::discriminant(kind) != declared_kind
    {
        return Err(Error::AccessDenied("entry kind and database protocol are immutable"));
    }
    if matches!(update.change, EntryChange::Schema { .. })
        && declared_kind
            != std::mem::discriminant(&EntryKind::ObjectDb { schema: Schema::default() })
    {
        return Err(Error::AccessDenied("schema history requires an object database"));
    }
    Ok(())
}

type SourceValue<'a> = (kvdb::Clock, &'a [u8]);

/// Bound the combined declaration/register allocation before decoding any of
/// them. Pages also charge these raw values before materializing their rows.
fn source_charge(values: &[Option<SourceValue<'_>>]) -> Result<(usize, usize)> {
    let mut bytes = 0usize;
    let mut charged = 16 * 1024usize;
    for (_, value) in values.iter().flatten() {
        bytes = bytes.checked_add(value.len()).ok_or(Error::LimitExceeded)?;
        charged = charged
            .checked_add(crate::store::catalog::index_record_charge(value)?)
            .ok_or(Error::LimitExceeded)?;
        if charged > crate::store::MetadataPolicy::MAX_MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
    }
    Ok((bytes + 512, charged))
}

fn assemble(
    id: EntryId,
    declaration: SourceValue<'_>,
    name: Option<SourceValue<'_>>,
    content: Option<SourceValue<'_>>,
    parent: Option<SourceValue<'_>>,
) -> Result<(EntryInfo, EntryId, EntryId)> {
    source_charge(&[Some(declaration), name, content, parent])?;
    let (clock, bytes) = declaration;
    let creation = creation_digest(id, clock, bytes);
    let mut record = info(bytes)?;
    let mut name_revision = record.created_revision;
    let mut content_revision = record.created_revision;
    if let Some((origin, bytes)) = name {
        let update = revision(bytes)?;
        validate_dependency(
            std::mem::discriminant(&record.kind),
            record.created_revision,
            creation,
            clock,
            &update,
        )?;
        if update.origin_clock != origin.as_u64() {
            return Err(Error::corrupt("entry name revision clock differs from its storage clock"));
        }
        let name = match update.change {
            EntryChange::Name { name } | EntryChange::Move { name: Some(name), .. } => name,
            _ => return Err(Error::corrupt("entry name revision has the wrong change kind")),
        };
        record.name = name;
        name_revision = update.revision;
    }
    if let Some((origin, bytes)) = content {
        let update = revision(bytes)?;
        validate_dependency(
            std::mem::discriminant(&record.kind),
            record.created_revision,
            creation,
            clock,
            &update,
        )?;
        if update.origin_clock != origin.as_u64() {
            return Err(Error::corrupt(
                "entry content revision clock differs from its storage clock",
            ));
        }
        let EntryChange::Content { kind } = update.change else {
            return Err(Error::corrupt("entry content revision has the wrong change kind"));
        };
        record.kind = kind;
        content_revision = update.revision;
    }
    if name_revision == content_revision && name_revision != record.created_revision {
        return Err(Error::AccessDenied("entry revision ID reused across registers"));
    }
    if let Some(parent_revision) = apply_parent(id, declaration, &mut record, parent)?
        && (parent_revision == content_revision
            || parent_revision == name_revision && parent != name)
    {
        return Err(Error::AccessDenied("entry revision ID reused across registers"));
    }
    Ok((record, name_revision, content_revision))
}

/// Validate retained deletion even when its declaration arrived later. Register
/// repair runs before publication, so no arrival order can bypass this binding.
fn validate_deletion(
    id: EntryId,
    declaration: SourceValue<'_>,
    heads: [Option<SourceValue<'_>>; 3],
    deletion: Option<SourceValue<'_>>,
) -> Result<bool> {
    let Some((clock, bytes)) = deletion else { return Ok(false) };
    source_charge(&[Some(declaration), heads[0], heads[1], heads[2], deletion])?;
    let witness = deletion_witness(id, declaration, (clock, bytes))?;
    for (_, bytes) in heads.into_iter().flatten() {
        if revision(bytes)?.revision == witness.revision {
            return Err(Error::AccessDenied("entry revision ID reused across registers"));
        }
    }
    Ok(true)
}

fn deletion_witness(
    id: EntryId,
    declaration: SourceValue<'_>,
    deletion: SourceValue<'_>,
) -> Result<namespace::observations::Witness> {
    let birth = namespace::source::Declaration::decode(id, declaration.0, declaration.1)?;
    let decoded =
        birth.revision(deletion.0, deletion.1)?.ok_or(Error::corrupt("invalid deletion"))?;
    if !matches!(decoded.fact.change, namespace::Change::Delete { .. }) {
        return Err(Error::corrupt("entry deletion register has the wrong change kind"));
    }
    Ok(decoded.witness)
}

pub fn entry_key(id: EntryId) -> [u8; 16] {
    let mut key = [ENTRY_ID_BYTE; 16];
    key[1..].copy_from_slice(id.as_bytes());
    key
}

/// Prefix seeks for one entry's authoritative facts. No names, candidates,
/// local readiness or another entry's facts belong in a scoped projection.
pub fn entry_source_prefixes(id: EntryId, include_permissions: bool) -> Vec<Vec<u8>> {
    let mut prefixes = vec![birth_source_key(id).to_vec()];
    for kind in [RETIRED, PERMISSIONS, REVISION, NAME, CONTENT, 9, 10, 11, PARENT] {
        if kind == PERMISSIONS && !include_permissions {
            continue;
        }
        prefixes.push(meta_key(kind, id).to_vec());
    }
    assert_eq!(prefixes.len(), 9 + usize::from(include_permissions));
    prefixes
}

pub(crate) fn birth_source_key(id: EntryId) -> [u8; 16] {
    entry_key(id)
}

pub(crate) fn permission_source_key(id: EntryId) -> [u8; 17] {
    meta_key(PERMISSIONS, id)
}

fn match_blob_version(kind: &EntryKind, version: &BlobVersion) -> Result<()> {
    if matches!(kind, EntryKind::Blob { len_bytes, digest, encrypted_key, .. }
        if *len_bytes == version.len_bytes && *digest == version.digest && *encrypted_key == version.encrypted_key)
    {
        Ok(())
    } else {
        Err(Error::AccessDenied("blob version differs from its index declaration"))
    }
}

/// History records are distinct from the birth/current heads needed to edit.
pub fn is_entry_history(key: &[u8]) -> bool {
    key.starts_with(&[META_BYTE, REVISION])
}

fn meta_key(kind: u8, id: EntryId) -> [u8; 17] {
    let mut key = [0; 17];
    key[0] = META_BYTE;
    key[1] = kind;
    key[2..].copy_from_slice(id.as_bytes());
    key
}

fn revision_key(id: EntryId, revision: EntryId) -> [u8; 32] {
    let mut key = [0; 32];
    key[..17].copy_from_slice(&meta_key(REVISION, id));
    key[17..].copy_from_slice(revision.as_bytes());
    key
}

fn name_key(parent: Option<EntryId>, name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + name.len());
    key.push(ENTRY_NAME_BYTE);
    key.extend_from_slice(parent.as_ref().map_or(&[0; 15][..], |id| &id.as_bytes()[..]));
    key.extend_from_slice(name.as_bytes());
    key
}

fn candidate_prefix(parent: Option<EntryId>, name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(18 + name.len() + 15);
    key.extend_from_slice(&[META_BYTE, CANDIDATE]);
    key.extend_from_slice(parent.as_ref().map_or(&[0; 15][..], |id| &id.as_bytes()[..]));
    key.extend_from_slice(name.as_bytes());
    key.push(0);
    key
}

fn candidate_key(parent: Option<EntryId>, name: &str, id: EntryId) -> Vec<u8> {
    let mut key = candidate_prefix(parent, name);
    // Complementing fixed-width IDs puts the largest ID at the first seek,
    // without scanning every entry competing for this name.
    key.extend_from_slice(&candidate_suffix(id));
    key
}

fn candidate_suffix(id: EntryId) -> [u8; 15] {
    let mut suffix = [0; 15];
    for (byte, source) in suffix.iter_mut().zip(id.as_bytes()) {
        *byte = !source;
    }
    suffix
}

fn id_bytes(bytes: &[u8]) -> Result<EntryId> {
    EntryId::from_bytes(
        bytes.try_into().map_err(|_| Error::corrupt("entry ID has an invalid byte length"))?,
    )
}

/// Only source keys are accepted from replication. In particular, a peer
/// cannot install a name mapping even if it can write volume entries.
pub fn source_entry_id(key: &[u8]) -> Result<EntryId> {
    if schema::is_key(key) {
        return schema::key_entry(key);
    }
    if key.len() == 16 && key[0] == ENTRY_ID_BYTE {
        return id_bytes(&key[1..]);
    }
    if (key.len() == 17
        && key[0] == META_BYTE
        && [RETIRED, PERMISSIONS, NAME, CONTENT, PARENT].contains(&key[1]))
        || (key.len() == 32 && key[0] == META_BYTE && key[1] == REVISION)
    {
        return id_bytes(&key[2..17]);
    }
    Err(Error::AccessDenied("unknown volume index source key"))
}

/// Identity of exact namespace bytes acknowledged by the trusted AP transport.
/// This is durability evidence, never canonical acceptance or authorization.
#[cfg(any(feature = "local-store", test))]
pub(crate) fn acknowledged_namespace_event(
    key: &[u8],
    clock: kvdb::Clock,
    bytes: &[u8],
) -> Result<Option<EntryId>> {
    let Ok(entry) = source_entry_id(key) else { return Ok(None) };
    if key == entry_key(entry) {
        return namespace::event_id(
            entry,
            info(bytes)?.created_revision,
            creation_digest(entry, clock, bytes),
        )
        .map(Some);
    }
    if (key.len() == 32 && is_entry_history(key))
        || (key.len() == 17 && [NAME, CONTENT, PARENT, RETIRED].contains(&key[1]))
    {
        let change = revision(bytes)?;
        if matches!(
            change.change,
            EntryChange::Name { .. }
                | EntryChange::Move { .. }
                | EntryChange::Content { .. }
                | EntryChange::Delete { .. }
        ) {
            return namespace::event_id(entry, change.revision, *blake3::hash(bytes).as_bytes())
                .map(Some);
        }
    }
    Ok(None)
}

/// Validate before resolution and publication. The originating writer's
/// permissions are checked separately against the pre-merge policy snapshot.
pub fn validate_source(delta: &Delta<'_>) -> Result<EntryId> {
    validate_source_record(delta).map(|(id, _)| id)
}

// A namespace scan can consume the canonical revision already decoded here.
// All source-key, immutability and clock checks remain shared with replication.
fn validate_source_record(delta: &Delta<'_>) -> Result<(EntryId, Option<EntryRevision>)> {
    let id = source_entry_id(delta.key)?;
    if schema::is_key(delta.key) {
        schema::validate_delta(delta)?;
        return Ok((id, None));
    }
    let EntState::Present { value, clock } = delta.remote else {
        return match delta.remote {
            EntState::Missing => Ok((id, None)),
            _ => Err(Error::AccessDenied("volume entry facts cannot be erased")),
        };
    };
    if clock.is_zero() {
        return Err(Error::AccessDenied("entry source origin clock is zero"));
    }
    let remote = validate_value(delta.key, value)?;
    let immutable = delta.key[0] == ENTRY_ID_BYTE || delta.key[1] == REVISION;
    if immutable
        && let EntState::Present { value: local, clock: local_clock } = delta.local
        && (local != value || local_clock != clock)
    {
        return Err(Error::AccessDenied("entry creation and history are immutable"));
    }
    if let Some(remote) = remote.as_ref() {
        if remote.origin_clock != clock.as_u64() {
            return Err(Error::AccessDenied("entry revision origin clock changed"));
        }
        if let EntState::Present { value: local, .. } = delta.local {
            let local = revision(local)?;
            if local.creation != remote.creation
                || local.revision == remote.revision && &local != remote
            {
                return Err(Error::AccessDenied("entry revision or creation binding changed"));
            }
        }
    }
    Ok((id, remote))
}

fn validate_value(key: &[u8], value: &[u8]) -> Result<Option<EntryRevision>> {
    if schema::is_key(key) {
        return schema::validate_value(key, value).map(|()| None);
    }
    match key[0] {
        ENTRY_ID_BYTE => {
            info(value)?;
        }
        META_BYTE if [NAME, CONTENT, PARENT, RETIRED, REVISION].contains(&key[1]) => {
            let remote = revision(value)?;
            match (key[1], &remote.change) {
                (NAME, EntryChange::Name { .. } | EntryChange::Move { name: Some(_), .. })
                | (PARENT, EntryChange::Move { .. })
                | (RETIRED, EntryChange::Delete {})
                | (CONTENT, EntryChange::Content { .. }) => {}
                (
                    REVISION,
                    EntryChange::Name { .. }
                    | EntryChange::Move { .. }
                    | EntryChange::Delete {}
                    | EntryChange::Content { .. }
                    | EntryChange::Schema { .. },
                ) if remote.revision.as_bytes() == &key[17..] => {}
                _ => return Err(Error::InvalidInput("entry revision purpose disagrees with key")),
            }
            return Ok(Some(remote));
        }
        META_BYTE if key[1] == PERMISSIONS => {
            if value.len() > MAX_ENTRY_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
            crate::store::catalog::preflight_index_record(value)?;
            let acl: EntryAcl = decode(value)?;
            if let EntryAcl::Replace(acl) = &acl {
                acl.validate()?;
            }
        }
        _ => return Err(Error::corrupt("unknown volume index record kind")),
    }
    Ok(None)
}

fn can_append_source(
    creation: bool,
    local_missing: bool,
    entry_missing: bool,
    retired: bool,
    replayed: bool,
) -> bool {
    creation && !retired && ((local_missing && entry_missing) || replayed)
}

fn source_permission(
    manages: bool,
    changes_permissions: bool,
    write: bool,
    append: bool,
    fresh: bool,
) -> bool {
    if changes_permissions { manages } else { write || (append && fresh) }
}

fn initial_permission_admitted(
    owner: bool,
    replacement: bool,
    local: EntState<'_>,
    remote: EntState<'_>,
) -> bool {
    owner || !replacement || local == remote
}

fn permission_publication_key(id: EntryId) -> [u8; PERMISSION_PUBLICATION_PREFIX.len() + 15] {
    let mut key = [0; PERMISSION_PUBLICATION_PREFIX.len() + 15];
    key[..PERMISSION_PUBLICATION_PREFIX.len()].copy_from_slice(PERMISSION_PUBLICATION_PREFIX);
    key[PERMISSION_PUBLICATION_PREFIX.len()..].copy_from_slice(id.as_bytes());
    key
}

fn content_permission(
    inherits: bool,
    owner: bool,
    service: bool,
    service_key_shared: bool,
    read: bool,
    granted: bool,
) -> bool {
    if service && !service_key_shared {
        return false;
    }
    granted || inherits && (owner || service && read)
}

/// Read all source and derived data through this single snapshot.
pub struct IndexSnapshot<'db, S: Storage> {
    tx: ReadTxn<'db, S>,
    namespace: std::sync::OnceLock<std::sync::Arc<namespace::source::cut::Projection>>,
    namespace_cache: Option<projection::CacheSnapshot<'db, S>>,
}

/// One declaration retained only for the duration of a bounded history page.
struct RevisionDeclaration {
    initial: Option<EntryRevision>,
    kind: std::mem::Discriminant<EntryKind>,
    revision: EntryId,
    digest: [u8; 32],
    clock: kvdb::Clock,
    bytes: usize,
    charge: usize,
}

impl RevisionDeclaration {
    fn new(id: EntryId, clock: kvdb::Clock, bytes: &[u8], after: Option<EntryId>) -> Result<Self> {
        let info = info(bytes)?;
        let revision = info.created_revision;
        let kind = std::mem::discriminant(&info.kind);
        let digest = creation_digest(id, clock, bytes);
        let charge = crate::store::catalog::index_record_charge(bytes)?;
        let initial = if after.is_none_or(|after| revision > after) {
            Some(EntryRevision {
                observed: info.namespace.observed.clone(),
                revision,
                creation: digest,
                origin_clock: clock.as_u64(),
                origin: info.origin.clone(),
                change: EntryChange::Created { info: Box::new(info) },
            })
        } else {
            None
        };
        Ok(Self { initial, kind, revision, digest, clock, bytes: bytes.len(), charge })
    }

    fn history(&self, key: &[u8], bytes: &[u8]) -> Result<EntryRevision> {
        let record = revision(bytes)?;
        if record.revision.as_bytes() != &key[17..] || record.revision == self.revision {
            return Err(Error::corrupt(
                "entry history revision does not match its key or duplicates its creation",
            ));
        }
        validate_dependency(self.kind, self.revision, self.digest, self.clock, &record)?;
        Ok(record)
    }
}

impl<'db, S: Storage> IndexSnapshot<'db, S> {
    pub fn new(tx: ReadTxn<'db, S>) -> Self {
        Self { tx, namespace: std::sync::OnceLock::new(), namespace_cache: None }
    }

    /// Resolve current content or a retained version through this same snapshot.
    /// Selecting current content requires a live entry. Explicit retained revisions
    /// remain readable after retirement, subject to current retained Read authority.
    pub fn blob_version(
        &self,
        address: EntryAddress,
        requested: Option<EntryId>,
    ) -> Result<BlobVersion> {
        let (revision, kind) = match requested {
            None => {
                let current = self.live_entry(address.entry)?;
                (current.content_revision, current.info.kind)
            }
            Some(_) => self.retained_blob_kind(address.entry, requested)?,
        };
        let EntryKind::Blob { len_bytes, digest, encrypted_key, .. } = kind else {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        };
        Ok(BlobVersion { address, revision, len_bytes, digest, encrypted_key })
    }

    /// Bind an admitted capture to its original immutable declaration.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn blob_creation(&self, entry: EntryId) -> Result<[u8; 32]> {
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(entry))?.ok_or(Error::NotFound("entry"))?;
        info(&bytes)?;
        Ok(creation_digest(entry, clock, &bytes))
    }

    /// Resolve only exact accepted body evidence, returning its original birth
    /// binding for transfer rechecks. Raw history is never acceptance evidence.
    /// The caller must independently check current Read or upload authority.
    pub fn accepted_blob_creation(&self, version: &BlobVersion) -> Result<[u8; 32]> {
        let entry = version.address.entry;
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(entry))?.ok_or(Error::NotFound("entry"))?;
        let initial = info(&bytes)?;
        if !matches!(initial.kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        }
        let birth = version.revision == initial.created_revision;
        if birth {
            match_blob_version(&initial.kind, version)?;
        }
        drop(initial);
        let declaration = namespace::source::Declaration::decode(entry, clock, &bytes)?;
        if birth {
            if !namespace::payloads::birth(&self.tx, &declaration.witness(), &bytes)? {
                return Err(Error::AccessDenied("blob birth is not accepted"));
            }
        } else {
            let (clock, source) = namespace::payloads::blob(&self.tx, version)?
                .ok_or(Error::AccessDenied("blob version is not accepted"))?;
            declaration.revision(clock, &source)?;
            let EntryChange::Content { kind } = revision(&source)?.change else {
                return Err(Error::corrupt("accepted blob source is not content"));
            };
            match_blob_version(&kind, version)?;
        }
        Ok(creation_digest(entry, clock, &bytes))
    }

    /// Validate the complete captured body identity. Current authority remains
    /// the caller's responsibility; source existence does not qualify a head.
    pub fn validate_blob_version(&self, version: &BlobVersion) -> Result<()> {
        let entry = version.address.entry;
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(entry))?.ok_or(Error::NotFound("entry"))?;
        let initial = info(&bytes)?;
        if !matches!(initial.kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        }
        if version.revision == initial.created_revision {
            return match_blob_version(&initial.kind, version);
        }
        drop(initial);
        let declaration = namespace::source::Declaration::decode(entry, clock, &bytes)?;
        if let Some((clock, bytes)) = namespace::payloads::blob(&self.tx, version)? {
            declaration.revision(clock, &bytes)?;
            let record = revision(&bytes)?;
            return match record.change {
                EntryChange::Content { kind } => match_blob_version(&kind, version),
                _ => Err(Error::corrupt("captured blob source is not content")),
            };
        }
        // Selected Write replicas can receive a current head without history.
        // Check both fixed locations without letting a selector hide an exact
        // source, and without promoting either source to admission evidence.
        let history = revision_key(entry, version.revision);
        let head = meta_key(CONTENT, entry);
        for key in [&history[..], &head[..]] {
            let Some((clock, bytes)) = self.tx.get_with_clock(key)? else { continue };
            declaration.source(key, clock, &bytes)?;
            let record = revision(&bytes)?;
            if record.revision == version.revision
                && let EntryChange::Content { kind } = record.change
                && match_blob_version(&kind, version).is_ok()
            {
                return Ok(());
            }
        }
        Err(Error::AccessDenied("blob version differs from its index declaration"))
    }

    /// Metadata for a retained revision, including recovery and captured views.
    /// The caller must admit current retained Read before preparing these bytes.
    pub fn retained_blob_kind(
        &self,
        entry: EntryId,
        requested: Option<EntryId>,
    ) -> Result<(EntryId, EntryKind)> {
        let (revision, kind) = if let Some(revision) = requested {
            let kind = match self.revision(entry, revision)?.change {
                EntryChange::Created { info } => info.kind,
                EntryChange::Content { kind } => kind,
                _ => return Err(Error::NotFound("blob version")),
            };
            (revision, kind)
        } else {
            let current = self.entry(entry)?;
            (current.content_revision, current.info.kind)
        };
        if !matches!(kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        }
        Ok((revision, kind))
    }

    pub fn live_entry(&self, id: EntryId) -> Result<VolumeEntry> {
        let record = self.entry(id)?;
        if record.deleted {
            return Err(Error::NotFound("retired entry"));
        }
        Ok(record)
    }

    pub fn permissions(&self, id: EntryId) -> Result<EntryAcl> {
        if let Some(bytes) = self.tx.get(&meta_key(PERMISSIONS, id))? {
            return decode(&bytes);
        }
        match self.tx.get(&entry_key(id))? {
            Some(bytes) => Ok(info(&bytes)?.initial_permissions),
            None => Ok(EntryAcl::Inherit),
        }
    }

    /// Permission repair needs a valid birth, not a projected namespace cut.
    pub(crate) fn permissions_for_edit(&self, id: EntryId) -> Result<EntryAcl> {
        info(&self.tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?)?;
        self.permissions(id)
    }

    pub fn created_clock(&self, id: EntryId) -> Result<u64> {
        self.tx
            .get_with_clock(&entry_key(id))?
            .map(|(clock, _)| clock.as_u64())
            .ok_or(Error::NotFound("entry"))
    }

    pub fn schema_pending(&self, id: EntryId) -> Result<bool> {
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        if !matches!(info(&bytes)?.kind, EntryKind::ObjectDb { .. }) {
            return Ok(false);
        }
        Ok(schema::read(&self.tx, id, (clock, &bytes))?.2 != 0)
    }

    pub fn backing_ready(&self, id: EntryId) -> Result<bool> {
        if self.backing_quarantined(id)? {
            return Ok(false);
        }
        self.backing_ready_for(id, &current::body(&self.tx, id)?)
    }

    /// Detached receipt/rejected recovery only; a source marker never qualifies
    /// a birth or grants app access. Publication rechecks the current body.
    pub(crate) fn source_backing_ready(&self, id: EntryId) -> Result<bool> {
        if self.backing_quarantined(id)? {
            return Ok(false);
        }
        self.backing_ready_for(id, &current::source_body(&self.tx, id)?)
    }

    fn backing_ready_for(&self, id: EntryId, kind: &EntryKind) -> Result<bool> {
        if matches!(kind, EntryKind::Directory {}) {
            return Ok(true);
        }
        let expected = backing_identity(kind);
        match self.tx.local_get(&meta_key(BACKING_READY, id))? {
            Some(bytes) if bytes.len() == expected.len() => Ok(bytes.as_ref() == expected),
            Some(_) => Err(Error::corrupt("entry backing identity has an invalid length")),
            None => Ok(false),
        }
    }

    pub(crate) fn backing_quarantined(&self, id: EntryId) -> Result<bool> {
        match self.tx.local_get(&quarantined_backing_key(id))? {
            Some(value) if value.as_ref() == [1] => Ok(true),
            Some(_) => Err(Error::corrupt("entry quarantine marker must be one")),
            None => Ok(false),
        }
    }

    pub fn authorize(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
        permission: Permission,
    ) -> Result<()> {
        let (_, deleted) = current::identity(&self.tx, id)?;
        if deleted {
            return Err(Error::NotFound("retired entry"));
        }
        self.authorize_acl(volume, defaults, principal, id, permission)
    }

    /// Candidate policy evaluation in detached receipt only. The eventual
    /// publication must qualify the birth and direct deletion before app access.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn authorize_staged_entry(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
        permission: Permission,
    ) -> Result<()> {
        let birth = self.tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        info(&birth)?;
        if self.tx.has(&meta_key(RETIRED, id))? {
            return Err(Error::NotFound("retired entry"));
        }
        self.authorize_acl(volume, defaults, principal, id, permission)
    }

    /// Known-ID metadata is available to readers and writers, independently of
    /// catalog admission. The returned bit is the separate content Read grant.
    pub fn metadata_readable(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        id: EntryId,
    ) -> Result<bool> {
        let actor = actor.into();
        super::catalog::admit_entry_metadata(|permission| {
            self.authorize(volume, defaults, actor, id, permission)
        })
    }

    /// Uploading the immutable creation body completes an Append-authorized
    /// creation. Later content versions still require current Write. This never
    /// grants plaintext Read or authority to mutate the entry's source records.
    pub fn authorize_blob_upload(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        id: EntryId,
        revision: EntryId,
    ) -> Result<()> {
        let birth = self.tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        let initial = info(&birth)?;
        if !matches!(initial.kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        }
        let actor = actor.into();
        admit_blob_upload(revision == initial.created_revision, |permission| {
            self.authorize(volume, defaults, actor, id, permission)
        })
    }

    /// Identity and ACL admission never assemble mutable names or schemas.
    /// The immutable birth already fixes database kind and backing protocol.
    pub fn entry_policy(&self, id: EntryId) -> Result<crate::store::EntryPolicy> {
        self.header(id)?.policy()
    }

    /// Candidate source admission only; this does not qualify app access.
    pub(crate) fn source_entry_policy(&self, id: EntryId) -> Result<crate::store::EntryPolicy> {
        self.source_header(id)?.policy()
    }

    /// Explicit retained reads use current ACLs. This cannot admit a write,
    /// upload or resurrection of a retired ID.
    pub fn authorize_retained_read(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
    ) -> Result<()> {
        let birth = self.tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        info(&birth)?;
        self.authorize_acl(volume, defaults, principal, id, Permission::Read)
    }

    /// Revalidate an existing writer's authority over its own private staging.
    /// Namespace deletion permits recovery but cannot grant entry publication.
    pub(crate) fn authorize_staged_write(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
    ) -> Result<()> {
        let birth = self.tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        info(&birth)?;
        self.authorize_acl(volume, defaults, principal, id, Permission::Write)
    }

    fn authorize_acl(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
        permission: Permission,
    ) -> Result<()> {
        let actor = principal.into();
        let override_acl = self.permissions(id)?;
        let acl = match &override_acl {
            EntryAcl::Inherit => defaults,
            EntryAcl::Replace(acl) => acl,
        };
        if content_permission(
            matches!(override_acl, EntryAcl::Inherit),
            actor.principal() == Principal::Human(volume.owner),
            actor.principal() == crate::store::catalog::PLAYBIT_REPLICA_PRINCIPAL,
            volume.share_key_with_service,
            permission == Permission::Read,
            actor.permits(volume.id, acl, permission),
        ) {
            Ok(())
        } else {
            Err(Error::AccessDenied("entry permission"))
        }
    }

    pub(super) fn source_granted(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        id: EntryId,
        permission: Permission,
    ) -> Result<bool> {
        match self.authorize_acl(volume, defaults, actor, id, permission) {
            Ok(()) => Ok(true),
            Err(Error::AccessDenied(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Stream admission only: every supplied fact still needs authorize_source.
    /// Unknown births use current defaults; retirement allows only a writer's
    /// permanent source facts, never backing edits or ordinary content reads.
    pub fn authorize_source_writer(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        id: EntryId,
    ) -> Result<()> {
        let actor = actor.into();
        if actor.human()? == volume.owner
            || self.source_granted(volume, defaults, actor, id, Permission::Write)?
            || self.source_granted(volume, defaults, actor, id, Permission::ManagePermissions)?
            || (!self.tx.has(&meta_key(RETIRED, id))?
                && self.source_granted(volume, defaults, actor, id, Permission::Append)?)
        {
            Ok(())
        } else {
            Err(Error::AccessDenied("entry source write permission"))
        }
    }

    /// Originating writer admission uses this pinned pre-merge policy. Incoming
    /// grants never authorize another source change in their own changeset.
    /// A trusted recipient checks transport/read visibility separately and can
    /// repair already-authorized source data without having write permission.
    pub fn authorize_source(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        delta: &Delta<'_>,
    ) -> Result<()> {
        let id = validate_source(delta)?;
        if matches!(delta.remote, EntState::Missing) {
            return Ok(());
        }
        self.validate_revision_creation(id, delta)?;
        let actor = actor.into();
        let owner = actor.human()? == volume.owner;
        if delta.key[0] == ENTRY_ID_BYTE
            && let EntState::Present { value, .. } = delta.remote
            && !initial_permission_admitted(
                owner,
                matches!(info(value)?.initial_permissions, EntryAcl::Replace(_)),
                delta.local,
                delta.remote,
            )
        {
            return Err(Error::AccessDenied("initial entry permission owner"));
        }
        let changes_permissions = delta.key[0] == META_BYTE && delta.key[1] == PERMISSIONS;
        if changes_permissions {
            if !owner {
                self.permissions_for_edit(id)?;
            }
            if delta.local == delta.remote && matches!(delta.remote, EntState::Present { .. }) {
                return Ok(());
            }
            let manages = owner
                || self.source_granted(
                    volume,
                    defaults,
                    actor,
                    id,
                    Permission::ManagePermissions,
                )?;
            return if source_permission(manages, true, false, false, false) {
                Ok(())
            } else {
                Err(Error::AccessDenied("entry permission management"))
            };
        }
        let write = self.source_granted(volume, defaults, actor, id, Permission::Write)?;
        if source_permission(owner, false, write, false, false) {
            return Ok(());
        }
        // Append admits a fresh or exactly replayed immutable birth. Independent
        // history and registers require Write, even before creation arrives.
        let admitted_birth = can_append_source(
            delta.key[0] == ENTRY_ID_BYTE,
            matches!(delta.local, EntState::Missing),
            self.tx.get(&entry_key(id))?.is_none(),
            self.tx.get(&meta_key(RETIRED, id))?.is_some(),
            delta.local == delta.remote,
        );
        let append = admitted_birth
            && self.source_granted(volume, defaults, actor, id, Permission::Append)?;
        if source_permission(owner, false, write, append, admitted_birth) {
            Ok(())
        } else {
            Err(Error::AccessDenied("entry source write permission"))
        }
    }

    fn validate_revision_creation(&self, id: EntryId, delta: &Delta<'_>) -> Result<()> {
        if delta.key[0] == META_BYTE
            && [NAME, CONTENT, PARENT, RETIRED, REVISION].contains(&delta.key[1])
            && let Some((clock, declaration)) = self.tx.get_with_clock(&entry_key(id))?
            && let EntState::Present { value, .. } = delta.remote
        {
            let initial = info(&declaration)?;
            validate_dependency(
                std::mem::discriminant(&initial.kind),
                initial.created_revision,
                creation_digest(id, clock, &declaration),
                clock,
                &revision(value)?,
            )?;
        }
        Ok(())
    }

    /// Publication rechecks current source permission after the local commit.
    /// Its exact immutable birth is a replay for Append admission. This does
    /// not admit incoming data or change the recipient's pre-merge policy.
    pub fn authorize_committed_source(
        &self,
        volume: &Volume,
        defaults: &Acl,
        actor: impl Into<DataActor>,
        key: &[u8],
        remote: EntState<'_>,
    ) -> Result<()> {
        let actor = actor.into();
        let account = actor.human()?;
        let current = self.tx.get_with_clock(key)?;
        let local = match &current {
            Some((clock, value)) => EntState::Present { clock: *clock, value },
            None => EntState::Missing,
        };
        let delta = Delta { key, local, remote };
        if key.starts_with(&[META_BYTE, PERMISSIONS]) {
            let id = validate_source(&delta)?;
            if account == volume.owner
                || self.source_granted(
                    volume,
                    defaults,
                    actor,
                    id,
                    Permission::ManagePermissions,
                )?
                || (local == remote && self.permission_publication(account, id)?)
            {
                return Ok(());
            }
            return Err(Error::AccessDenied("entry permission publication"));
        }
        self.authorize_source(volume, defaults, actor, &delta)
    }

    /// An exact locally authorized ACL commit may still leave after self-removal.
    /// This receipt never admits a new edit or an incoming source record.
    pub(crate) fn permission_publication(&self, account: Id, id: EntryId) -> Result<bool> {
        let Some(receipt) = self.tx.local_get(&permission_publication_key(id))? else {
            return Ok(false);
        };
        if receipt.len() != PERMISSION_PUBLICATION_BYTES
            || &receipt[..4] != PERMISSION_PUBLICATION_MAGIC
        {
            return Err(Error::corrupt("invalid permission publication receipt"));
        }
        let Some((clock, bytes)) = self.tx.get_with_clock(&permission_source_key(id))? else {
            return Ok(false);
        };
        Ok(receipt[4..20] == account.as_bytes()[..]
            && receipt[20..28] == clock.as_u64().to_le_bytes()
            && receipt[28..] == blake3::hash(&bytes).as_bytes()[..])
    }

    /// Replay only an exact local permission commit newer than the captured AP
    /// register. A reflected receipt, stale grant or other metadata cannot leave.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn permission_restoration_delta(
        &self,
        account: Id,
        id: EntryId,
        remote: &IndexSnapshot<'_, S>,
    ) -> Result<Option<Vec<u8>>> {
        if !self.permission_publication(account, id)? {
            return Ok(None);
        }
        let key = permission_source_key(id);
        let Some((clock, _)) = self.tx.get_with_clock(&key)? else {
            return Ok(None);
        };
        if remote.tx.get_with_clock(&key)?.is_some_and(|(received, _)| received >= clock) {
            return Ok(None);
        }
        match self.tx.delta_for_key(&key, MAX_ENTRY_RECORD_BYTES + 1024)? {
            Some(kvdb::sync::CommittedDelta::Bytes(bytes)) => Ok(Some(bytes)),
            Some(kvdb::sync::CommittedDelta::PullRequired) => Err(Error::LimitExceeded),
            None => Err(Error::corrupt("retained permission commit is missing")),
        }
    }

    pub fn revision(&self, id: EntryId, requested: EntryId) -> Result<EntryRevision> {
        self.read_revision(id, requested, namespace::payloads::revision(&self.tx, id, requested)?)
    }

    fn read_revision(
        &self,
        id: EntryId,
        requested: EntryId,
        retained: Option<(kvdb::Clock, Vec<u8>)>,
    ) -> Result<EntryRevision> {
        let (clock, declaration) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        let initial = info(&declaration)?;
        let creation = creation_digest(id, clock, &declaration);
        if requested == initial.created_revision {
            return Ok(EntryRevision {
                observed: initial.namespace.observed.clone(),
                revision: requested,
                creation,
                origin_clock: clock.as_u64(),
                origin: initial.origin.clone(),
                change: EntryChange::Created { info: Box::new(initial) },
            });
        }
        // Selected receipts need not include history keys. Their exact accepted
        // bytes remain addressable after a subsequent mutable head replaces them.
        let (origin, bytes) = match retained {
            Some(source) => source,
            None => {
                let (clock, bytes) = self
                    .tx
                    .get_with_clock(&revision_key(id, requested))?
                    .ok_or(Error::NotFound("entry revision"))?;
                if bytes.len() > MAX_ENTRY_RECORD_BYTES {
                    return Err(Error::LimitExceeded);
                }
                (clock, bytes.to_vec())
            }
        };
        let record = revision(&bytes)?;
        if record.revision != requested || record.origin_clock != origin.as_u64() {
            return Err(Error::corrupt(
                "entry revision identity or clock differs from its storage key or clock",
            ));
        }
        validate_dependency(
            std::mem::discriminant(&initial.kind),
            initial.created_revision,
            creation,
            clock,
            &record,
        )?;
        Ok(record)
    }

    /// Merge the immutable creation with the ordered history seek. Retain at
    /// most one pending source record, one bounded page and one declaration.
    pub fn revisions(
        &self,
        id: EntryId,
        after: Option<EntryId>,
        limit: usize,
    ) -> Result<RevisionPage> {
        if limit == 0 || limit > MAX_ENTRY_PAGE {
            return Err(Error::LimitExceeded);
        }
        let (clock, declaration) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        let mut declaration = RevisionDeclaration::new(id, clock, &declaration, after)?;
        let prefix = meta_key(REVISION, id);
        let start = after.map(|revision| revision_key(id, revision));
        let mut iter = self.tx.iter_from(start.as_ref().map_or(&prefix[..], |key| key));
        let mut pending_history = iter.next().transpose()?;
        let mut revisions = Vec::with_capacity(limit);
        let mut next = None;
        let mut bytes = 0usize;
        // Once emitted, the declaration moves into the page rather than cloning.
        let mut charged =
            16 * 1024usize + if declaration.initial.is_some() { declaration.charge } else { 0 };
        loop {
            if let Some((key, _)) = &pending_history {
                if !key.starts_with(&prefix) {
                    pending_history = None;
                } else if key.len() != 32 {
                    return Err(Error::corrupt("entry history key must be 32 bytes"));
                } else if Some(id_bytes(&key[17..])?) == after {
                    pending_history = iter.next().transpose()?;
                    continue;
                }
            }
            let history_id =
                pending_history.as_ref().map(|(key, _)| id_bytes(&key[17..])).transpose()?;
            let use_creation = declaration.initial.is_some()
                && history_id.is_none_or(|history| declaration.revision < history);
            if !use_creation && pending_history.is_none() {
                break;
            }
            let (record_bytes, record_charge) = if use_creation {
                (declaration.bytes + 256, 0)
            } else {
                let (_, value) = pending_history.as_ref().expect("history selected");
                (value.len() + 256, crate::store::catalog::index_record_charge(value)?)
            };
            if revisions.len() == limit
                || bytes + record_bytes > MAX_ENTRY_PAGE_BYTES
                || charged + record_charge > crate::store::MetadataPolicy::MAX_MATERIALIZED_BYTES
            {
                if revisions.is_empty() {
                    return Err(Error::LimitExceeded);
                }
                next = revisions.last().map(|record: &EntryRevision| record.revision);
                break;
            }
            bytes += record_bytes;
            charged += record_charge;
            let record = if use_creation {
                declaration.initial.take().expect("creation selected")
            } else {
                let (key, value) = pending_history.take().expect("history selected");
                let record = declaration.history(&key, &value)?;
                pending_history = iter.next().transpose()?;
                record
            };
            revisions.push(record);
        }
        Ok(RevisionPage { revisions, next })
    }
}

/// Caller has already authorized and prepared backing/content. Known local
/// name collisions fail; remote competition goes through source resolution.
pub fn create(tx: &mut WriteTxn<'_, impl Storage>, id: EntryId, entry: &EntryInfo) -> Result<()> {
    let view = namespace::source::cut::project(tx)?.admission.view;
    create_in_namespace(tx, id, entry, view)
}

fn create_in_namespace(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    entry: &EntryInfo,
    view: namespace::View,
) -> Result<()> {
    entry.validate()?;
    let parent = projection::canonical_directory(&view, entry.namespace.parent)?;
    if view
        .entries
        .values()
        .any(|other| other.visible && other.parent == parent && other.name == entry.name)
    {
        return Err(Error::AlreadyExists("entry name"));
    }
    validate_parent(&view, id, entry)?;
    drop(view);
    insert_birth(tx, id, entry)
}

/// Ordinary writers opened against absence keep their fresh identities even if
/// another writer publishes at that name first. EXCLUSIVE callers use create.
pub(crate) fn create_competing_blob(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    entry: &EntryInfo,
) -> Result<()> {
    if !matches!(entry.kind, EntryKind::Blob { .. }) {
        return Err(Error::InvalidInput("competing publication requires a blob"));
    }
    let view = namespace::source::cut::project(tx)?.admission.view;
    validate_parent(&view, id, entry)?;
    drop(view);
    insert_birth(tx, id, entry)
}

fn insert_birth(tx: &mut WriteTxn<'_, impl Storage>, id: EntryId, entry: &EntryInfo) -> Result<()> {
    let mut publication = namespace::observations::Publication::load(&tx.local_indexes())?;
    if let Some(parent) = entry.namespace.parent {
        let witness = {
            let (clock, bytes) = tx
                .get_with_clock(&entry_key(parent))?
                .ok_or(Error::NotFound("parent directory"))?;
            namespace::source::Declaration::decode(parent, clock, &bytes)?.witness()
        };
        publication.observe(witness, &tx.local_indexes())?;
    }
    let mut entry = entry.clone();
    entry.namespace.observed = publication.context();
    if entry.namespace.automatic {
        path_creation::set_incarnation(tx, id, &mut entry, &publication)?;
    }
    let bytes = entry.canonical()?;
    let source = namespace::source::Declaration::decode(id, tx.clock(), &bytes)?.witness();
    let event = source.id()?;
    publication.publish(source, &tx.local_indexes())?;
    if tx.get(&meta_key(RETIRED, id))?.is_some() {
        return Err(Error::AccessDenied("entry ID permanently retired"));
    }
    if tx.get(&entry_key(id))?.is_some() {
        return Err(Error::AlreadyExists("entry creation is immutable"));
    }
    tx.put(&entry_key(id), &bytes)?;
    publication.write(&mut tx.local_indexes())?;
    repair(&mut tx.local_indexes(), &[id])?;
    drop(bytes);
    drop(entry);
    let projected = namespace::source::cut::project(tx)?;
    if projected.admission.events.get(&event) != Some(&namespace::EventState::Accepted) {
        return Err(Error::InvalidInput("new namespace birth is not causally admitted"));
    }
    projected.write_current(&mut tx.local_indexes())
}

/// Create a path relative to the declaration's parent, making missing directory
/// components durable. The leaf declaration contains one component. Abort the transaction on error.
/// Automatic parent identities derive from this fresh leaf birth, so a retry
/// cannot substitute a different directory under the same prepared publication.
pub fn create_at_path(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    path: &str,
    entry: &EntryInfo,
) -> Result<()> {
    validate_name(path)?;
    entry.validate()?;
    if path.rsplit('/').next() != Some(&entry.name) {
        return Err(Error::InvalidInput("path creation requires the leaf component"));
    }
    let mut parent = entry.namespace.parent;
    let mut components = path.split('/').peekable();
    for depth in 0..namespace::MAX_DEPTH {
        let Some(name) = components.next() else { break };
        let view = namespace::source::cut::project(tx)?.admission.view;
        if components.peek().is_none() {
            let mut leaf = entry.clone();
            leaf.namespace.parent = parent;
            return create_in_namespace(tx, id, &leaf, view);
        }
        let canonical = projection::canonical_directory(&view, parent)?;
        if let Some(existing) = view
            .entries
            .values()
            .find(|entry| entry.visible && entry.parent == canonical && entry.name == name)
        {
            if existing.kind != namespace::Kind::Directory {
                return Err(Error::InvalidInput("entry path component is not a directory"));
            }
            parent = Some(existing.id);
            continue;
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"app.playbit.automatic-directory.v1\0");
        hash.update(id.as_bytes());
        hash.update(entry.created_revision.as_bytes());
        hash.update(&(depth as u64).to_le_bytes());
        let digest = hash.finalize();
        let directory = EntryId::from_bytes(digest.as_bytes()[..15].try_into().expect("15 bytes"))?;
        let revision =
            EntryId::from_bytes(digest.as_bytes()[15..30].try_into().expect("15 bytes"))?;
        create_in_namespace(
            tx,
            directory,
            &EntryInfo {
                namespace: namespace::Birth { parent, automatic: true, ..Default::default() },
                origin: entry.origin.clone(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: revision,
                name: name.to_owned(),
                kind: EntryKind::Directory {},
                protocol: None,
            },
            view,
        )?;
        parent = Some(directory);
    }
    Err(Error::LimitExceeded)
}

fn validate_parent(view: &namespace::View, id: EntryId, entry: &EntryInfo) -> Result<()> {
    let mut parent = entry.namespace.parent;
    let mut bytes = entry.name.len();
    for _ in 1..namespace::MAX_DEPTH {
        let Some(current) = parent else { return Ok(()) };
        let record = projection::visible_entry(view, current)?;
        if record.id == id {
            return Err(Error::InvalidInput("invalid parent directory"));
        }
        if record.kind != namespace::Kind::Directory {
            return Err(Error::InvalidInput("parent is not a directory"));
        }
        bytes += record.name.len() + 1;
        if bytes > namespace::MAX_PATH_BYTES {
            return Err(Error::LimitExceeded);
        }
        parent = record.parent;
    }
    if parent.is_some() {
        return Err(Error::LimitExceeded);
    }
    Ok(())
}

/// Rename writes only the name register/history. The independent content
/// register and declaration retain their original bytes and clocks.
/// Abort the transaction on error, including resulting namespace exhaustion.
pub fn rename(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    name: &str,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    namespace::component(name)?;
    let view = namespace::source::cut::project(tx)?.admission.view;
    let entry = projection::visible_entry(&view, id)?;
    if entry.requested_name == name {
        return Ok(());
    }
    validate_namespace_destination(&view, entry.id, entry.parent, name)?;
    drop(view);
    change(tx, id, revision, EntryChange::Name { name: name.to_owned() }, origin)?;
    qualify_namespace_edit(tx, id, revision, false)
}

/// Move updates the parent register and, when requested, the independent name
/// register using the same immutable revision. Descendant sources are untouched.
/// Abort the transaction on error, including resulting namespace exhaustion.
pub fn move_entry(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    parent: Option<EntryId>,
    name: Option<&str>,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    if let Some(name) = name {
        namespace::component(name)?;
    }
    let view = namespace::source::cut::project(tx)?.admission.view;
    let entry = projection::visible_entry(&view, id)?;
    let destination = projection::canonical_directory(&view, parent)?;
    let name = name.filter(|name| *name != entry.requested_name);
    let reparent = entry.parent != destination;
    if !reparent && name.is_none() {
        return Ok(());
    }
    validate_namespace_destination(
        &view,
        entry.id,
        destination,
        name.unwrap_or(&entry.requested_name),
    )?;
    let assignment = if reparent {
        // Canonicalization routes the request but never borrows another
        // identity's ACL or rewrites the caller's observed parent witness.
        EntryChange::Move { parent, name: name.map(str::to_owned) }
    } else {
        EntryChange::Name { name: name.expect("changed name without reparenting").to_owned() }
    };
    drop(view);
    change(tx, id, revision, assignment, origin)?;
    qualify_namespace_edit(tx, id, revision, false)
}

/// Publish an entry-bound root/name assignment without catalog traversal. The
/// caller has admitted exact entry Write, not authority to inspect siblings.
/// Opaque observations stay opaque; the source intent remains provisional.
#[cfg(any(feature = "local-store", test))]
pub(crate) fn assign_entry_root(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    name: &str,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    namespace::component(name)?;
    if tx.get(&meta_key(RETIRED, id))?.is_some() {
        return Err(Error::NotFound("live entry"));
    }
    validate_assignment_observations(tx, id)?;
    // Selected source fields cannot establish effective no-ops across omitted
    // aliases or cycle fallback. Full causal admission normalizes this intent.
    let assignment = EntryChange::Move { parent: None, name: Some(name.to_owned()) };
    change(tx, id, revision, assignment, origin)?;
    qualify_namespace_edit(tx, id, revision, true)
}

#[cfg(any(feature = "local-store", test))]
fn validate_assignment_observations(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
) -> Result<()> {
    let witnesses = {
        let (clock, birth) = tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        let declaration = namespace::source::Declaration::decode(id, clock, &birth)?;
        let mut witnesses = vec![declaration.witness()];
        for key in [meta_key(NAME, id), meta_key(PARENT, id)] {
            if let Some((clock, bytes)) = tx.get_with_clock(&key)? {
                witnesses.push(
                    declaration
                        .revision(clock, &bytes)?
                        .ok_or(Error::corrupt("entry assignment is not a namespace source"))?
                        .witness,
                );
            }
        }
        witnesses
    };
    let publication = namespace::observations::Publication::load(&tx.local_indexes())?;
    for witness in witnesses {
        if publication.witness(witness.id()?, &tx.local_indexes())? != witness {
            return Err(Error::AccessDenied("entry assignment observation changed"));
        }
    }
    Ok(())
}

fn validate_namespace_destination(
    view: &namespace::View,
    id: EntryId,
    parent: Option<EntryId>,
    name: &str,
) -> Result<()> {
    if view.entries.values().any(|other| {
        other.visible && other.id != id && other.parent == parent && other.name == name
    }) {
        return Err(Error::AlreadyExists("entry name"));
    }
    let mut next = parent;
    for _ in 0..namespace::MAX_DEPTH {
        let Some(current) = next else { return Ok(()) };
        if current == id {
            return Err(Error::InvalidInput("invalid parent directory"));
        }
        next = projection::visible_entry(view, current)?.parent;
    }
    Err(Error::LimitExceeded)
}

fn apply_parent(
    id: EntryId,
    birth: SourceValue<'_>,
    entry: &mut EntryInfo,
    parent: Option<SourceValue<'_>>,
) -> Result<Option<EntryId>> {
    let Some((clock, bytes)) = parent else { return Ok(None) };
    let update = revision(bytes)?;
    validate_dependency(
        std::mem::discriminant(&entry.kind),
        entry.created_revision,
        creation_digest(id, birth.0, birth.1),
        birth.0,
        &update,
    )?;
    if update.origin_clock != clock.as_u64() {
        return Err(Error::corrupt("entry parent revision clock differs from storage"));
    }
    let EntryChange::Move { parent, .. } = update.change else {
        return Err(Error::corrupt("entry parent register has the wrong change kind"));
    };
    entry.namespace.parent = parent;
    Ok(Some(update.revision))
}

pub fn update_content(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    kind: EntryKind,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    change(tx, id, revision, EntryChange::Content { kind }, origin)?;
    qualify_namespace_edit(tx, id, revision, true)
}

/// File close succeeds only when the exact new content is visible in the local
/// namespace. Selected known-ID publication may retain a Pending source intent.
pub(crate) fn update_file_content<S: Storage>(
    tx: &mut WriteTxn<'_, S>,
    id: EntryId,
    kind: EntryKind,
    revision: EntryId,
    origin: &namespace::Origin,
    predecessor: Option<projection::CacheSnapshot<'_, S>>,
) -> Result<namespace::source::cut::Projection> {
    // This is the first namespace mutation after predecessor was captured from
    // this writer's committed clock. change adds only this exact content event;
    // its birth observation can become Provisional, never change qualification.
    change(tx, id, revision, EntryChange::Content { kind }, origin)?;
    let prior = predecessor.map(projection::CacheSnapshot::take).transpose()?.flatten();
    let projected = match prior {
        Some(prior) => namespace::source::cut::append_content(tx, prior, id, revision)?,
        None => None,
    };
    finish_namespace_edit(tx, id, revision, false, projected)
}

fn change(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision_id: EntryId,
    change: EntryChange,
    origin: &namespace::Origin,
) -> Result<()> {
    let namespace_edit = matches!(change, EntryChange::Name { .. } | EntryChange::Move { .. });
    if !namespace_edit && tx.get(&meta_key(RETIRED, id))?.is_some() {
        if !matches!(change, EntryChange::Content { .. } | EntryChange::Delete {}) {
            return Err(Error::AccessDenied("entry ID permanently retired"));
        }
        // A received deletion head may still be Pending. A complete visible
        // namespace cut can authorize another deletion or File publication;
        // a selected writer without that evidence cannot bypass retirement.
        projection::visible_entry(&namespace::source::cut::project(tx)?.admission.view, id)?;
    }
    let mut publication = namespace::observations::Publication::load(&tx.local_indexes())?;
    if let EntryChange::Move { parent: Some(parent), .. } = &change {
        let witness = {
            let (clock, bytes) = tx
                .get_with_clock(&entry_key(*parent))?
                .ok_or(Error::NotFound("parent directory"))?;
            namespace::source::Declaration::decode(*parent, clock, &bytes)?.witness()
        };
        publication.observe(witness, &tx.local_indexes())?;
    }
    let (mut event, birth) = change_event(tx, id, revision_id, change, origin)?;
    publication.observe(birth, &tx.local_indexes())?;
    event.observed = publication.context();
    let bytes = event.canonical()?;
    if !matches!(event.change, EntryChange::Schema { .. }) {
        publication.publish(
            namespace::observations::Witness {
                entry: id,
                revision: event.revision,
                source_digest: *blake3::hash(&bytes).as_bytes(),
                clock: event.origin_clock,
                birth: false,
                observed: event.observed.clone().into(),
            },
            &tx.local_indexes(),
        )?;
    }
    if let EntryChange::Move { name: Some(_), .. } = &event.change {
        tx.put(&meta_key(NAME, id), &bytes)?;
    }
    let kind = match event.change {
        EntryChange::Name { .. } => Some(NAME),
        EntryChange::Move { .. } => Some(PARENT),
        EntryChange::Delete {} => Some(RETIRED),
        EntryChange::Content { .. } => Some(CONTENT),
        EntryChange::Schema { .. } => None,
        EntryChange::Created { .. } => unreachable!("change accepts only name or content"),
    };
    if let Some(kind) = kind {
        tx.put(&meta_key(kind, id), &bytes)?;
    }
    tx.put(&revision_key(id, revision_id), &bytes)?;
    publication.write(&mut tx.local_indexes())?;
    repair(&mut tx.local_indexes(), &[id])?;
    Ok(())
}

fn qualify_namespace_edit(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision: EntryId,
    allow_pending: bool,
) -> Result<()> {
    project_namespace_edit(tx, id, revision, allow_pending).map(|_| ())
}

fn project_namespace_edit(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision: EntryId,
    allow_pending: bool,
) -> Result<namespace::source::cut::Projection> {
    finish_namespace_edit(tx, id, revision, allow_pending, None)
}

fn finish_namespace_edit(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision: EntryId,
    allow_pending: bool,
    projected: Option<namespace::source::cut::Projection>,
) -> Result<namespace::source::cut::Projection> {
    let event = {
        let bytes = tx
            .get(&revision_key(id, revision))?
            .ok_or(Error::corrupt("new namespace edit is missing"))?;
        namespace::event_id(id, revision, *blake3::hash(&bytes).as_bytes())?
    };
    // Recompute actual effective names and every descendant path after the
    // edit. A valid local preflight must not commit an unusable resulting cut.
    let projected = match projected {
        Some(projected) => projected,
        None => namespace::source::cut::project(tx)?,
    };
    match projected.admission.events.get(&event) {
        Some(namespace::EventState::Accepted) => {}
        Some(namespace::EventState::Pending) if allow_pending => {}
        _ => return Err(Error::InvalidInput("new namespace edit is not causally admitted")),
    }
    projected.write_current(&mut tx.local_indexes())?;
    Ok(projected)
}

fn change_event(
    tx: &WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision_id: EntryId,
    change: EntryChange,
    origin: &namespace::Origin,
) -> Result<(EntryRevision, namespace::observations::Witness)> {
    let (clock, declaration) =
        tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
    let initial = info(&declaration)?;
    let birth = namespace::source::Declaration::decode(id, clock, &declaration)?.witness();
    if revision_id == initial.created_revision || tx.get(&revision_key(id, revision_id))?.is_some()
    {
        return Err(Error::AlreadyExists("entry revision"));
    }
    for key in [meta_key(NAME, id), meta_key(CONTENT, id), meta_key(PARENT, id)] {
        if let Some(bytes) = tx.get(&key)?
            && revision(&bytes)?.revision == revision_id
        {
            return Err(Error::AlreadyExists("entry revision"));
        }
    }
    let event = EntryRevision {
        observed: Vec::new(),
        revision: revision_id,
        creation: creation_digest(id, clock, &declaration),
        origin_clock: tx.clock().as_u64(),
        origin: origin.clone(),
        change,
    };
    validate_dependency(
        std::mem::discriminant(&initial.kind),
        initial.created_revision,
        event.creation,
        clock,
        &event,
    )?;
    Ok((event, birth))
}

/// Schema identity/deprecation facts and their retained operation history.
/// The caller authorizes against a pinned pre-merge entry policy.
pub fn establish_schema(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    declaration: &Schema,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<bool> {
    if !schema::stage(tx, id, declaration)? {
        return Ok(false);
    }
    change(tx, id, revision, EntryChange::Schema { declaration: declaration.clone() }, origin)?;
    Ok(true)
}

/// Caller verified durable backing publication while holding the volume gate.
/// This fact is local: receiving an entry head never implies receiving its data.
pub fn mark_backing_ready(tx: &mut WriteTxn<'_, impl Storage>, id: EntryId) -> Result<()> {
    let (_, deleted) = current::identity(tx, id)?;
    if deleted {
        return Err(Error::NotFound("retired entry"));
    }
    let kind = current::body(tx, id)?;
    write_backing_ready(tx, id, &kind)
}

/// Publishing a pending or losing revision verifies only its own body. Keep a
/// prior readiness marker intact unless these exact backing bytes are current.
pub(crate) fn mark_published_blob_ready(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    published: &EntryKind,
) -> Result<()> {
    if !matches!(published, EntryKind::Blob { .. }) {
        return Err(Error::InvalidInput("published body must be a blob"));
    }
    let (_, deleted) = current::identity(tx, id)?;
    if deleted {
        return Err(Error::NotFound("retired entry"));
    }
    let current = current::body(tx, id)?;
    if backing_identity(&current) == backing_identity(published) {
        write_backing_ready(tx, id, &current)?;
    }
    Ok(())
}

/// A verified received backing may precede qualification of its source birth.
/// Keep its local identity for staging/recovery; public reads still require
/// qualified controls and current authority. Never use this to publish a File.
pub(crate) fn mark_source_backing_ready(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
) -> Result<()> {
    let kind = current::source_body(tx, id)?;
    write_backing_ready(tx, id, &kind)
}

fn write_backing_ready(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    kind: &EntryKind,
) -> Result<()> {
    tx.local_indexes().put(&meta_key(BACKING_READY, id), &backing_identity(kind))?;
    tx.local_indexes().del(&quarantined_backing_key(id))?;
    Ok(())
}

fn quarantined_backing_key(id: EntryId) -> Vec<u8> {
    [QUARANTINED_BACKING, id.as_bytes()].concat()
}

/// The qualified publication retained these bytes before withdrawing readiness.
pub(crate) fn quarantine_backing(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
    id: EntryId,
) -> Result<()> {
    local.put(&quarantined_backing_key(id), &[1])?;
    local.del(&meta_key(BACKING_READY, id))?;
    Ok(())
}

fn backing_identity(kind: &EntryKind) -> Vec<u8> {
    match kind {
        EntryKind::Blob { len_bytes, digest, encrypted_key, .. } => {
            let mut hash = blake3::Hasher::new();
            hash.update(b"app.playbit.entry.blob.ready.v1\0");
            hash.update(&len_bytes.to_le_bytes());
            hash.update(digest);
            hash.update(encrypted_key);
            let mut ready = Vec::with_capacity(33);
            ready.push(2);
            ready.extend_from_slice(hash.finalize().as_bytes());
            ready
        }
        _ => vec![1],
    }
}

/// Retain a causal deletion in both the current deletion register and immutable
/// history. Directory visibility is derived from its observed namespace scope.
pub fn retire(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    if tx.get(&meta_key(RETIRED, id))?.is_some() {
        return Ok(());
    }
    change(tx, id, revision, EntryChange::Delete {}, origin)?;
    qualify_namespace_edit(tx, id, revision, true)
}

/// Catalog deletion removes the currently admitted incarnation, including a
/// directory restored by concurrent children since an earlier deletion.
#[cfg(any(feature = "local-store", test))]
pub(crate) fn retire_namespace(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    revision: EntryId,
    origin: &namespace::Origin,
) -> Result<()> {
    projection::visible_entry(&namespace::source::cut::project(tx)?.admission.view, id)?;
    change(tx, id, revision, EntryChange::Delete {}, origin)?;
    qualify_namespace_edit(tx, id, revision, false)
}

pub fn set_permissions(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    permissions: &EntryAcl,
) -> Result<()> {
    info(&tx.get(&entry_key(id))?.ok_or(Error::NotFound("entry"))?)?;
    if let EntryAcl::Replace(acl) = permissions {
        acl.validate()?;
    }
    let key = meta_key(PERMISSIONS, id);
    let bytes = match tx.get(&key)? {
        Some(old) => record::rewrite(&old, permissions)?,
        None => encode(permissions)?,
    };
    if bytes.len() > MAX_ENTRY_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    tx.put(&meta_key(PERMISSIONS, id), &bytes)?;
    Ok(())
}

/// The caller authorized this exact ACL edit under the volume operation guard.
/// The fixed latest receipt commits atomically and never enters source exports.
#[cfg(any(feature = "local-store", test))]
pub(crate) fn retain_permission_publication(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    account: Id,
) -> Result<()> {
    let mut receipt = [0; PERMISSION_PUBLICATION_BYTES];
    receipt[..4].copy_from_slice(PERMISSION_PUBLICATION_MAGIC);
    receipt[4..20].copy_from_slice(account.as_bytes());
    let (clock, bytes) = tx
        .get_with_clock(&meta_key(PERMISSIONS, id))?
        .ok_or(Error::Internal("permission publication requires its committed ACL register"))?;
    receipt[20..28].copy_from_slice(&clock.as_u64().to_le_bytes());
    receipt[28..].copy_from_slice(blake3::hash(&bytes).as_bytes());
    drop(bytes);
    tx.local_indexes().put(&permission_publication_key(id), &receipt)?;
    Ok(())
}

/// Source winner IDs must be bounded by the receiver's admission policy. Each
/// ID needs at most two candidate seeks, independent of total volume size.
pub fn repair(local: &mut LocalIndexes<'_, '_, impl Storage>, ids: &[EntryId]) -> Result<()> {
    if ids.len() > MAX_REPAIR_IDS {
        return Err(Error::LimitExceeded);
    }
    for &id in ids {
        validate_register_history(local, id)?;
        let reverse = meta_key(REVERSE_NAME, id);
        let old = local.get(&reverse)?.map(|bytes| bytes.to_vec());
        let old_location = old.as_ref().map(|bytes| decode::<Location>(bytes)).transpose()?;
        if let Some(location) = &old_location {
            namespace::component(&location.name)?;
            local.del(&candidate_key(location.parent, &location.name, id))?;
        }
        let current = {
            let declaration = local.source_get_with_clock(&entry_key(id))?;
            match declaration {
                Some((clock, declaration)) => {
                    let name = local.source_get_with_clock(&meta_key(NAME, id))?;
                    let content = local.source_get_with_clock(&meta_key(CONTENT, id))?;
                    let parent = local.source_get_with_clock(&meta_key(PARENT, id))?;
                    let deletion = local.source_get_with_clock(&meta_key(RETIRED, id))?;
                    let deleted = validate_deletion(
                        id,
                        (clock, &declaration),
                        [
                            name.as_ref().map(|(clock, value)| (*clock, &value[..])),
                            content.as_ref().map(|(clock, value)| (*clock, &value[..])),
                            parent.as_ref().map(|(clock, value)| (*clock, &value[..])),
                        ],
                        deletion.as_ref().map(|(clock, value)| (*clock, &value[..])),
                    )?;
                    let (record, _, _) = assemble(
                        id,
                        (clock, &declaration),
                        name.as_ref().map(|(clock, value)| (*clock, &value[..])),
                        content.as_ref().map(|(clock, value)| (*clock, &value[..])),
                        parent.as_ref().map(|(clock, value)| (*clock, &value[..])),
                    )?;
                    schema::repair(local, id)?;
                    if !deleted { Some(record) } else { None }
                }
                None => None,
            }
        };
        let current =
            current.map(|record| Location { parent: record.namespace.parent, name: record.name });
        if let Some(location) = &current {
            local.put(&candidate_key(location.parent, &location.name, id), &[])?;
            local.put(&reverse, &encode(location)?)?;
        } else {
            local.del(&reverse)?;
        }
        if let Some(location) = &old_location {
            repair_name(local, location)?;
        }
        if let Some(location) = &current
            && current != old_location
        {
            repair_name(local, location)?;
        }
    }
    Ok(())
}

/// A current head and its immutable history are copies of one event. Missing
/// history can arrive later; known different bytes or clocks are contradictions.
fn validate_register_history(
    local: &LocalIndexes<'_, '_, impl Storage>,
    id: EntryId,
) -> Result<()> {
    for kind in [NAME, CONTENT, PARENT, RETIRED] {
        let Some((clock, bytes)) = local.source_get_with_clock(&meta_key(kind, id))? else {
            continue;
        };
        let event = revision(&bytes)?;
        if let Some((history_clock, history)) =
            local.source_get_with_clock(&revision_key(id, event.revision))?
            && (clock != history_clock || bytes.as_ref() != history.as_ref())
        {
            return Err(Error::AccessDenied("entry head differs from immutable history"));
        }
    }
    Ok(())
}

/// Bootstrap/reconnect only. Seek one source head at a time, releasing each
/// iterator before mutation. No source or derived catalog is materialized.
pub fn repair_all(local: &mut LocalIndexes<'_, '_, impl Storage>) -> Result<()> {
    let mut after = None;
    loop {
        let key = {
            let mut iter = local.source_iter_from(
                after.as_ref().map_or(&[ENTRY_ID_BYTE][..], |key: &[u8; 16]| &key[..]),
            );
            let mut next = iter.next().transpose()?;
            if let (Some(after), Some((key, _))) = (after.as_ref(), next.as_ref())
                && &key[..] == after
            {
                next = iter.next().transpose()?;
            }
            match next {
                Some((key, _)) if key.first() == Some(&ENTRY_ID_BYTE) => {
                    Some(entry_key(id_bytes(&key[1..])?))
                }
                _ => None,
            }
        };
        let Some(key) = key else { return Ok(()) };
        repair(local, &[id_bytes(&key[1..])?])?;
        after = Some(key);
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Location {
    parent: Option<EntryId>,
    name: String,
}
record::fields!(Location { 1 => parent, 2 => name });
fn repair_name(local: &mut LocalIndexes<'_, '_, impl Storage>, location: &Location) -> Result<()> {
    let prefix = candidate_prefix(location.parent, &location.name);
    let winner = {
        let mut candidates = local.iter_from(&prefix)?;
        match candidates.next().transpose()? {
            Some((key, value)) if key.starts_with(&prefix) => {
                if key.len() != prefix.len() + 15 || !value.is_empty() {
                    return Err(Error::corrupt(
                        "entry name candidate has an invalid key length or nonempty value",
                    ));
                }
                let mut bytes = [0; 15];
                for (byte, stored) in bytes.iter_mut().zip(&key[prefix.len()..]) {
                    *byte = !stored;
                }
                Some(EntryId::from_bytes(bytes)?)
            }
            _ => None,
        }
    };
    let key = name_key(location.parent, &location.name);
    if let Some(id) = winner {
        local.put(&key, id.as_bytes())?;
    } else {
        local.del(&key)?;
    }
    Ok(())
}

/// Validated heads use kvdb's HLC/revision-first canonical value order. All
/// immutable facts are present-only; separate retirement always dominates a
/// head, regardless of its clock. Admission errors abort the whole receiver.
pub fn resolve(delta: &Delta<'_>) -> Result<Resolution> {
    validate_source(delta)?;
    Ok(kvdb::Lww.resolve(delta))
}

/// Apply one bounded live changeset, then derive its names in the same durable
/// transaction. The caller serializes policy/publication with the volume gate
/// and supplies originating-writer or authenticated-recipient admission.
pub fn apply_delta<S: Storage>(
    database: &kvdb::Db<S>,
    bytes: &[u8],
    wall_time: u64,
    mut authorize: impl FnMut(&IndexSnapshot<'_, S>, &Delta<'_>) -> Result<()>,
) -> Result<(kvdb::sync::SyncOutcome, Option<kvdb::CommittedDelta>)> {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    if bytes.len() > crate::store::MAX_REPLICA_DELTA_BYTES {
        return Err(Error::LimitExceeded);
    }
    let prior = IndexSnapshot::new(database.begin_read()?);
    let changed = RefCell::new(BTreeSet::new());
    let rejected = RefCell::new(None);
    let resolver = |delta: &Delta<'_>| {
        let mut admit = || -> Result<Resolution> {
            let id = validate_source(delta)?;
            authorize(&prior, delta)?;
            let resolution = kvdb::Lww.resolve(delta);
            if resolution == Resolution::UseRemote {
                let mut changed = changed.borrow_mut();
                if changed.len() == MAX_REPAIR_IDS && !changed.contains(&id) {
                    return Err(Error::LimitExceeded);
                }
                changed.insert(id);
            }
            Ok(resolution)
        };
        match admit() {
            Ok(resolution) => resolution,
            Err(error) => {
                if rejected.borrow().is_none() {
                    *rejected.borrow_mut() = Some(error);
                }
                Resolution::KeepLocal
            }
        }
    };
    let mut receiver = kvdb::DeltaReceiver::new_with_delta(
        database,
        resolver,
        wall_time,
        crate::store::MAX_REPLICA_DELTA_BYTES,
    )?;
    receiver.process(bytes)?;
    if let Some(error) = rejected.borrow_mut().take() {
        return Err(error);
    }
    let ids: Vec<_> = changed.borrow().iter().copied().collect();
    Ok(receiver
        .repair_local_indexes(|local| {
            repair(local, &ids)?;
            namespace::source::cut::project(local)?.write_current(local)
        })?
        .finish_with_delta()?)
}

fn admit_blob_upload(
    initial: bool,
    mut authorize: impl FnMut(Permission) -> Result<()>,
) -> Result<()> {
    match authorize(Permission::Write) {
        Err(Error::AccessDenied(_)) if initial => authorize(Permission::Append),
        result => result,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests;
