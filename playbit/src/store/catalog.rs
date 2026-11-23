//! Volume policy is stored in the volume index. Database content, schemas,
//! object births and permissions belong to independently named entries.
mod bounded;
mod kv;
mod received;
pub mod record;
mod snapshot;
use crate::store::object_contract::{Acl, EntryAcl, Permission, Principal};
use crate::store::{DatabaseId, EntryId, Error, Id, Result};
use kvdb::{Db, Storage};
pub use received::ReceivedMetadata;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use snapshot::{ReplicaReceipt, SnapshotCut, SnapshotToken};
use std::collections::BTreeSet;

pub const VOLUME_KEY: [u8; 2] = [crate::store::volume_index::META_BYTE, 16];
pub const DEFAULT_PERMISSIONS_KEY: [u8; 2] = [crate::store::volume_index::META_BYTE, 17];
const PROVISION_PREFIX: [u8; 2] = [crate::store::volume_index::META_BYTE, 18];
/// Required before reading entry records. Change this when the volume schema changes;
/// unsupported formats are rejected without rewriting their data.
pub const FORMAT_KEY: [u8; 2] = [crate::store::volume_index::META_BYTE, 19];
const VOLUME_FORMAT: &[u8] = b"1";
const POLICY_END: [u8; 2] = [crate::store::volume_index::META_BYTE, 20];

fn validate_format(value: Option<&[u8]>) -> Result<()> {
    if value == Some(VOLUME_FORMAT) { Ok(()) } else { Err(Error::UnsupportedVolumeFormat) }
}

fn validate_format_reader(value: &dyn kvdb::storage::ValueRead) -> Result<()> {
    if value.len() != VOLUME_FORMAT.len() {
        return Err(Error::UnsupportedVolumeFormat);
    }
    let mut bytes = [0; VOLUME_FORMAT.len()];
    value.read_exact_at(0, &mut bytes)?;
    validate_format(Some(&bytes))
}

fn validate_pending_format(tx: &kvdb::WriteTxn<'_, impl Storage>) -> Result<()> {
    let mut found = false;
    tx.visit(&FORMAT_KEY, false, |key, value| -> Result<bool> {
        if key == FORMAT_KEY {
            validate_format_reader(value)?;
            found = true;
        }
        Ok(false)
    })?;
    if found { Ok(()) } else { Err(Error::UnsupportedVolumeFormat) }
}

/// Only the current session's build needs to accompany a selected-entry cut.
pub fn entry_policy_keys(version: Option<&str>) -> Result<Vec<Vec<u8>>> {
    let mut keys = vec![VOLUME_KEY.to_vec(), DEFAULT_PERMISSIONS_KEY.to_vec(), FORMAT_KEY.to_vec()];
    if let Some(version) = version {
        validate_version(version)?;
        let mut key = PROVISION_PREFIX.to_vec();
        key.extend_from_slice(version.as_bytes());
        keys.push(key);
    }
    Ok(keys)
}

pub(crate) fn preflight_index_record(bytes: &[u8]) -> Result<()> {
    bounded::preflight(bytes)
}
pub(crate) fn index_record_charge(bytes: &[u8]) -> Result<usize> {
    bounded::index_record_charge(bytes)
}
pub(crate) fn json_projection_charge(bytes: &[u8]) -> Result<usize> {
    bounded::json_projection_charge(bytes)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Volume {
    pub id: Id,
    pub name: String,
    pub owner: Id,
    /// Allow the owner to give AP the database key for durable service catch-up.
    /// Withholding the key requires an authorized peer; it does not disable replication.
    pub share_key_with_service: bool,
    /// Created for a person, rather than as an app's private root or session.
    pub personal: bool,
}
impl Volume {
    /// Names change through metadata replication; capabilities bind only these
    /// immutable properties. An invitation may retain an earlier name.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.owner == other.owner
            && self.share_key_with_service == other.share_key_with_service
            && self.personal == other.personal
    }
}

/// Current volume inspector metadata from one admitted policy snapshot. The
/// clocks are originating record clocks; no intermediate rename history exists.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct VolumeDetails {
    pub volume: Volume,
    pub permissions: Acl,
    pub created_clock: u64,
    pub name_clock: u64,
}

pub fn validate_volume_label(label: &str) -> Result<()> {
    if label.len() > 256 || label.trim().is_empty() {
        return Err(Error::InvalidInput("volume label must contain 1 to 256 UTF-8 bytes"));
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum BuildSelector {
    Exact { build_id: String },
    App { app: crate::package::AppIdentity },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum UpdateAuthority {
    Creator,
    App,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Session {
    pub app: crate::package::AppIdentity,
    pub id: String,
    pub selector: BuildSelector,
    pub resolved_build_id: String,
    pub root_volume: Id,
    /// A fresh session's synchronized Shared scope. Absence preserves existing
    /// root-backed sessions and device-local automatic data without promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_volume: Option<Id>,

    pub creator: Id,
    pub update_authority: UpdateAuthority,
}
impl Session {
    pub fn replica_volume(&self) -> Id {
        self.shared_volume.unwrap_or(self.root_volume)
    }
    pub fn validate_shared_volume(&self) -> Result<()> {
        let Some(shared) = self.shared_volume else { return Ok(()) };
        use crate::package::AutomaticVolumeRole;
        let session = crate::identifier::decode(&self.id)
            .map_err(|_| Error::InvalidInput("invalid Shared session identity"))?;
        let expected_shared = self
            .app
            .volume_id(AutomaticVolumeRole::Session, Some(session), None, None)
            .map_err(Error::InvalidInput)?;
        let expected_root = self
            .app
            .volume_id(AutomaticVolumeRole::Root, None, None, None)
            .map_err(Error::InvalidInput)?;
        if shared != expected_shared || self.root_volume != expected_root {
            return Err(Error::AccessDenied("Shared session binding differs"));
        }
        Ok(())
    }
    /// Publishing provenance and selector origin cannot change when a session
    /// advances to a new build. Compare these against authenticated AP metadata,
    /// never against another claim supplied by the invitation's key holder.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.app == other.app
            && self.root_volume == other.root_volume
            && self.shared_volume == other.shared_volume
            && self.creator == other.creator
            && self.update_authority == other.update_authority
            && match (&self.selector, &other.selector) {
                (BuildSelector::Exact { .. }, BuildSelector::Exact { .. }) => true,
                (BuildSelector::App { app: a }, BuildSelector::App { app: b }) => a == b,
                _ => false,
            }
    }
}
/// Local import listing. Running an app still authenticates the retained package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct ImportedApp {
    pub app: crate::package::AppIdentity,
    pub build_id: String,
    pub name: String,
}
/// An account's explicit personal-volume mount for one session. Never shared in invitations.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct PersonalVolumeGrant {
    pub volume: Id,
    pub write: bool,
}

/// Persisted consent incarnation. Removing/re-adding or changing rights mints a
/// new identity, even when intermediate account notices are coalesced.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonalVolumeSelection {
    pub volume: Id,
    pub write: bool,
    pub incarnation: Id,
}
impl PersonalVolumeSelection {
    pub fn grant(&self) -> PersonalVolumeGrant {
        PersonalVolumeGrant { volume: self.volume, write: self.write }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataPolicy {
    #[serde(skip)]
    pub(crate) revision: u64,
    pub(crate) volume: Volume,
    pub(crate) permissions: Acl,
    pub(crate) provisioned_versions: BTreeSet<String>,
}
/// Runtime ACL context for an authenticated principal. The shared binding is
/// never decoded from a request or a stored permission grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataActor {
    principal: Principal,
    shared: Option<Id>,
}
impl DataActor {
    /// The caller has verified possession of this session's invitation and
    /// its registered identity. Bind that admission to its actual Shared volume.
    pub fn session_participant(account: Id, session: &Session, volume: &Volume) -> Result<Self> {
        session.validate_shared_volume()?;
        if session.shared_volume != Some(volume.id)
            || session.creator != volume.owner
            || volume.personal
            || !volume.share_key_with_service
        {
            return Err(Error::AccessDenied("session participant Shared volume binding"));
        }
        Ok(Self { principal: Principal::Human(account), shared: Some(volume.id) })
    }
    pub fn for_session(account: Id, session: &Session, volume: &Volume) -> Result<Self> {
        if session.shared_volume == Some(volume.id) {
            Self::session_participant(account, session, volume)
        } else {
            Ok(account.into())
        }
    }
    pub fn principal(self) -> Principal {
        self.principal
    }
    pub(crate) fn human(self) -> Result<Id> {
        match self.principal {
            Principal::Human(account) => Ok(account),
            Principal::Service(_) => Err(Error::AccessDenied("operation requires a human account")),
        }
    }
    pub(crate) fn permits(self, volume: Id, acl: &Acl, permission: Permission) -> bool {
        acl.checked().is_ok_and(|acl| {
            acl.permits_in_session(Some(self.principal), self.shared == Some(volume), permission)
        })
    }
}
impl From<Id> for DataActor {
    fn from(account: Id) -> Self {
        Principal::Human(account).into()
    }
}
impl From<Principal> for DataActor {
    fn from(principal: Principal) -> Self {
        Self { principal, shared: None }
    }
}
/// Reserved runtime principal for the managed Playbit database replica.
pub const PLAYBIT_REPLICA_PRINCIPAL: Principal = Principal::Service(Id([0x50; 16]));
pub type ReplicaRoot = Option<(u8, [u8; 32])>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableRevision {
    pub revision: u64,
    pub policy_revision: u64,
    pub root: ReplicaRoot,
}
#[derive(Clone, Copy, Debug)]
pub enum ReplicaReader {
    Service,
    Participant(Id),
    SessionParticipant(DataActor),
}

impl MetadataPolicy {
    pub const MAX_MATERIALIZED_BYTES: usize = 3 * 1024 * 1024;
    pub fn new(volume: Volume, permissions: Acl) -> Result<Self> {
        validate_volume_label(&volume.name)?;
        permissions.validate()?;
        Ok(Self { revision: 0, volume, permissions, provisioned_versions: BTreeSet::new() })
    }
    pub fn read(database: &Db<impl Storage>) -> Result<Self> {
        Self::read_bounded(database, Self::MAX_MATERIALIZED_BYTES)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn volume(&self) -> &Volume {
        &self.volume
    }
    pub fn default_permissions(&self) -> &Acl {
        &self.permissions
    }
    pub fn admit(&self, account: Id, session: &Session, volume: Id) -> Result<()> {
        session.validate_shared_volume()?;
        if session.replica_volume() != self.volume.id || session.replica_volume() != volume {
            return Err(Error::AccessDenied("session root binding"));
        }
        if session.shared_volume.is_some()
            && (self.volume.owner != session.creator
                || self.volume.personal
                || !self.volume.share_key_with_service)
        {
            return Err(Error::AccessDenied("Shared volume authority differs"));
        }
        if account != self.volume.owner
            && !self.provisioned_versions.contains(&session.resolved_build_id)
        {
            return Err(Error::AccessDenied("root version provisioning"));
        }
        Ok(())
    }
    pub fn authorize(&self, actor: impl Into<DataActor>, permission: Permission) -> Result<()> {
        let actor = actor.into();
        if actor.principal == Principal::Human(self.volume.owner)
            || actor.permits(self.volume.id, &self.permissions, permission)
        {
            Ok(())
        } else {
            Err(Error::AccessDenied("volume permission"))
        }
    }
    pub fn write(&self, database: &Db<impl Storage>, wall_time: u64) -> Result<()> {
        let mut tx = database.begin_write(wall_time)?;
        self.write_entries(&mut tx, Self::MAX_MATERIALIZED_BYTES)?;
        tx.commit()?;
        Ok(())
    }
    pub fn write_with_delta(
        &self,
        database: &Db<impl Storage>,
        wall_time: u64,
        max_bytes: usize,
    ) -> Result<kvdb::CommittedDelta> {
        let mut tx = database.begin_write_with_delta(wall_time, max_bytes)?;
        self.write_entries(&mut tx, Self::MAX_MATERIALIZED_BYTES)?;
        Ok(tx.commit_with_delta()?)
    }
    pub fn write_entries(
        &self,
        tx: &mut kvdb::WriteTxn<'_, impl Storage>,
        materialized_bytes: usize,
    ) -> Result<()> {
        self.permissions.validate()?;
        if tx.has(&VOLUME_KEY)? {
            validate_pending_format(tx)?;
        }
        for (key, value) in [
            (&FORMAT_KEY, VOLUME_FORMAT.to_vec()),
            (&VOLUME_KEY, record::encode(&self.volume)?),
            (&DEFAULT_PERMISSIONS_KEY, record::encode(&self.permissions)?),
        ] {
            let previous = tx.get(key)?;
            let value = match (key, previous.as_deref()) {
                (&VOLUME_KEY, Some(old)) => record::rewrite(old, &self.volume)?,
                (&DEFAULT_PERMISSIONS_KEY, Some(old)) => record::rewrite(old, &self.permissions)?,
                _ => value,
            };
            if previous.is_none_or(|old| old.as_ref() != value) {
                tx.put(key, &value)?;
            }
        }
        for version in &self.provisioned_versions {
            validate_version(version)?;
            let mut key = PROVISION_PREFIX.to_vec();
            key.extend_from_slice(version.as_bytes());
            // Replication distinguishes an ACL-only delta from a new build grant.
            // Retain the source clock on an existing canonical marker.
            let mut marker = [0; 4];
            tx.visit_value(&key, |_, value| -> Result<()> {
                if value.len() == marker.len() {
                    value.read_exact_at(0, &mut marker)?;
                }
                Ok(())
            })?;
            if &marker != b"true" {
                tx.put(&key, b"true")?;
            }
        }
        Self::validate_pending(tx, materialized_bytes)
    }
}

pub(crate) fn is_policy_key(key: &[u8]) -> bool {
    key == VOLUME_KEY
        || key == DEFAULT_PERMISSIONS_KEY
        || key == FORMAT_KEY
        || key.starts_with(&PROVISION_PREFIX)
}
/// Policy records never delete immutable volume identity or positive provisioning.
/// Entry source records have their own provenance, dependency and ACL validation.
pub(crate) fn validate_metadata_entry(
    volume: &Volume,
    key: &[u8],
    value: Option<&[u8]>,
) -> Result<()> {
    if crate::store::volume_index::is_index_key(key) {
        crate::store::volume_index::validate_record(key, value)?;
        return Ok(());
    }
    if key == FORMAT_KEY {
        return validate_format(value);
    }
    let value = value.ok_or(Error::AccessDenied("volume policy deletion"))?;
    if key == VOLUME_KEY {
        preflight_index_record(value)?;
        let incoming: Volume = record::decode(value)?;
        validate_volume_label(&incoming.name)?;
        if !incoming.same_identity(volume) {
            return Err(Error::AccessDenied("replica volume identity or encoding"));
        }
    } else if key == DEFAULT_PERMISSIONS_KEY {
        preflight_index_record(value)?;
        let incoming: Acl = record::decode(value)?;
        incoming.validate()?;
    } else if let Some(version) = key.strip_prefix(&PROVISION_PREFIX) {
        validate_version(
            std::str::from_utf8(version)
                .map_err(|_| Error::corrupt("provision version is not UTF-8"))?,
        )?;
        if value != b"true" {
            return Err(Error::corrupt("provision marker must be true"));
        }
    } else {
        return Err(Error::AccessDenied("unknown volume policy key"));
    }
    Ok(())
}

/// Immutable, bounded admission view derived from one persistent index snapshot.
/// Holding this view is not authority beyond the caller's volume operation gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum EntryType {
    Directory,
    KvDb,
    ObjectDb,
    Blob,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryPolicy {
    pub id: EntryId,
    pub kind: EntryType,
    pub protocol: Option<crate::store::volume_index::DatabaseProtocol>,
}

#[derive(Clone)]
pub struct DatabasePolicy {
    pub volume: MetadataPolicy,
    pub database: DatabaseId,
    pub entry: Option<EntryPolicy>,
    permissions: EntryAcl,
}
impl DatabasePolicy {
    pub fn reserved(volume: MetadataPolicy, database: DatabaseId) -> Result<Self> {
        if matches!(database, DatabaseId::Entry(_)) {
            return Err(Error::InvalidInput("named database requires a persistent entry"));
        }
        Ok(Self { volume, database, entry: None, permissions: EntryAcl::Inherit })
    }
    pub fn read(index: &Db<impl Storage>, database: DatabaseId) -> Result<Self> {
        Self::read_index(index, database, false)
    }
    /// Detached or incoming replication evaluates source identity and current
    /// ACLs before qualification. This policy alone cannot grant app access.
    pub fn read_source(index: &Db<impl Storage>, database: DatabaseId) -> Result<Self> {
        Self::read_index(index, database, true)
    }
    fn read_index(index: &Db<impl Storage>, database: DatabaseId, source: bool) -> Result<Self> {
        let tx = index.begin_read()?;
        let volume = MetadataPolicy::read_snapshot(&tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
        let snapshot = crate::store::volume_index::IndexSnapshot::new(tx);
        let (entry, permissions) = match database {
            DatabaseId::Entry(id) => {
                let entry = if source {
                    snapshot.source_entry_policy(id)?
                } else {
                    snapshot.entry_policy(id)?
                };
                (Some(entry), snapshot.permissions(id)?)
            }
            _ => (None, EntryAcl::Inherit),
        };
        Ok(Self { volume, database, entry, permissions })
    }
    pub fn authorize(&self, actor: impl Into<DataActor>, permission: Permission) -> Result<()> {
        let actor = actor.into();
        let principal = actor.principal;
        if self.entry.is_none() {
            return match principal {
                Principal::Human(_) => self.volume.authorize(actor, permission),
                _ if permission == Permission::Read
                    && self.volume.volume.share_key_with_service =>
                {
                    Ok(())
                }
                _ => Err(Error::AccessDenied("reserved database permission")),
            };
        }
        let inherit = matches!(self.permissions, EntryAcl::Inherit);
        let acl = self.permissions.effective(&self.volume.permissions);
        if (inherit && principal == Principal::Human(self.volume.volume.owner))
            || (inherit
                && principal == PLAYBIT_REPLICA_PRINCIPAL
                && permission == Permission::Read
                && self.volume.volume.share_key_with_service)
            || actor.permits(self.volume.volume.id, acl, permission)
        {
            Ok(())
        } else {
            Err(Error::AccessDenied("entry permission"))
        }
    }
    pub fn authorize_reader(&self, reader: ReplicaReader) -> Result<()> {
        match reader {
            ReplicaReader::SessionParticipant(actor) => {
                actor.human()?;
                self.authorize(actor, Permission::Read)
            }
            ReplicaReader::Participant(account) => {
                self.authorize(Principal::Human(account), Permission::Read)
            }
            ReplicaReader::Service if self.volume.volume.share_key_with_service => {
                self.authorize(PLAYBIT_REPLICA_PRINCIPAL, Permission::Read)
            }
            ReplicaReader::Service => Err(Error::AccessDenied("waiting for an authorized peer")),
        }
    }
    /// Admit known-entry editing metadata. The result grants content/history
    /// only when Read itself succeeds; Write and Append admit controls alone.
    pub fn metadata_readable(&self, actor: impl Into<DataActor>) -> Result<bool> {
        let actor = actor.into();
        if self.entry.is_none() {
            return Err(Error::InvalidInput("entry metadata requires an entry policy"));
        }
        admit_entry_metadata(|permission| self.authorize(actor, permission))
    }
    pub fn ordinary_protocol(&self) -> Result<()> {
        if let Some(entry) = &self.entry
            && entry.protocol != Some(crate::store::volume_index::DatabaseProtocol::Kvdb)
        {
            return Err(Error::InvalidInput("entry does not use ordinary database replication"));
        }
        Ok(())
    }

    /// Source permissions come from the pre-merge index. A trusted recipient
    /// validates provenance without claiming to be the originating writer.
    pub fn validate_source(
        &self,
        delta: &kvdb::sync::Delta<'_>,
        writer: Option<Id>,
    ) -> Result<Option<Id>> {
        self.validate_source_as(delta, writer.map(Into::into))
    }
    pub fn validate_source_as(
        &self,
        delta: &kvdb::sync::Delta<'_>,
        writer: Option<DataActor>,
    ) -> Result<Option<Id>> {
        self.ordinary_protocol()?;
        // Full anti-entropy reports target-only keys with remote Missing.
        // This contains no source assertion; LWW preserves the target fact.
        if matches!(delta.remote, kvdb::sync::EntState::Missing) {
            return Ok(None);
        }

        let append = if let Some(writer) = writer {
            match self.authorize(writer, Permission::Write) {
                Ok(()) => false,
                Err(Error::AccessDenied(_)) => {
                    self.authorize(writer, Permission::Append)?;
                    true
                }
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        match self.database {
            DatabaseId::Index => {
                let value = match delta.remote {
                    kvdb::sync::EntState::Present { value, clock }
                        if clock != kvdb::Clock::ZERO =>
                    {
                        value
                    }
                    _ => return Err(Error::AccessDenied("volume policy origin or deletion")),
                };
                validate_metadata_entry(&self.volume.volume, delta.key, Some(value))?;
                if writer.is_some_and(|actor| {
                    actor.principal != Principal::Human(self.volume.volume.owner)
                }) {
                    return Err(Error::AccessDenied("entry changes need pinned index admission"));
                }
                Ok(None)
            }
            DatabaseId::Revocations => {
                if writer.is_some_and(|actor| {
                    actor.principal != Principal::Human(self.volume.volume.owner)
                }) || append
                {
                    return Err(Error::AccessDenied("revocation store owner"));
                }
                Id::parse(
                    std::str::from_utf8(delta.key)
                        .map_err(|_| Error::corrupt("revocation record key is not UTF-8"))?,
                )?;
                match delta.remote {
                    kvdb::sync::EntState::Present { value, clock } => {
                        if clock == kvdb::Clock::ZERO || value.len() > 4096 {
                            return Err(Error::LimitExceeded);
                        }
                        decode::<String>(value)?;
                    }
                    kvdb::sync::EntState::Deleted { clock } if clock != kvdb::Clock::ZERO => {}
                    _ => return Err(Error::AccessDenied("invalid revocation origin")),
                }
                Ok(None)
            }
            DatabaseId::Entry(_) => {
                self.validate_entry_source(delta, writer.map(DataActor::human).transpose()?, append)
            }
        }
    }

    fn validate_entry_source(
        &self,
        delta: &kvdb::sync::Delta<'_>,
        writer: Option<Id>,
        append: bool,
    ) -> Result<Option<Id>> {
        let entry =
            self.entry.as_ref().ok_or(Error::corrupt("entry database declaration is missing"))?;
        match entry.kind {
            EntryType::ObjectDb => {
                crate::store::object_database::validate_source(delta, writer, append).map(Some)
            }
            EntryType::KvDb => {
                kv::validate_source(delta, append)?;
                Ok(None)
            }
            _ => Err(Error::InvalidInput("entry is not a database")),
        }
    }

    pub fn validate_sync_source(
        &self,
        delta: &kvdb::sync::SyncDelta<'_>,
        writer: Option<Id>,
    ) -> Result<Option<Id>> {
        self.validate_sync_source_as(delta, writer.map(Into::into))
    }
    pub fn validate_sync_source_as(
        &self,
        delta: &kvdb::sync::SyncDelta<'_>,
        writer: Option<DataActor>,
    ) -> Result<Option<Id>> {
        if matches!(delta.remote, kvdb::sync::SyncEntState::Missing) {
            return Ok(None);
        }
        crate::store::volume_index::with_sync_delta_bounded(delta, 4 * 1024 * 1024, |source| {
            self.validate_source_as(source, writer)
        })
    }
}
pub(crate) fn admit_entry_metadata(
    mut authorize: impl FnMut(Permission) -> Result<()>,
) -> Result<bool> {
    for permission in [Permission::Read, Permission::Write, Permission::Append] {
        match authorize(permission) {
            Ok(()) => return Ok(permission == Permission::Read),
            Err(Error::AccessDenied(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::AccessDenied("entry metadata permission"))
}
pub(crate) fn validate_version(version: &str) -> Result<()> {
    if !crate::identifier::valid(version, 32) {
        return Err(Error::InvalidInput(
            "version/session ID must be a base58-encoded 32-byte value",
        ));
    }
    Ok(())
}

pub(crate) fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| Error::Internal("could not encode a catalog record"))
}
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(Error::decode::<T>)
}
