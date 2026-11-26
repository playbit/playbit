//! Persistent volume indexes, named databases, and independent retained blobs.
//! Field edits, cache hits and blob sealing are data-plane paths. Cache admission
//! owns each slot through opening and retirement; crypto and backend I/O stay
//! outside the cache lock. Full kvdb verification is an explicit diagnostic.
mod app_removal;
pub use app_removal::{AppRemovalImpact, AppRemovalResult};
mod authority;
mod bloblog;
mod browser_drafts;
mod onboarding;
#[cfg(test)]
mod projection_tests;
mod received;
mod replication;
mod scoped;
pub use received::{ReceiptPage, ReceivedVolume, ReceivedVolumeOutcome};
pub use replication::ReplicationDatabasePage;
pub use scoped::ScopedSnapshot;
mod account_replica;
mod entry_recovery;
mod recovery_catalog;
pub use account_replica::PendingAccountRecord;
mod blob_files;
mod consent;
mod entries;
mod entry_blobs;
mod file_namespace;
mod file_provider;
mod file_provider_publication;
mod private_file_hints;
mod provider_uploads;
#[cfg(feature = "connected-engine")]
pub(crate) use blob_files::blob_transfer_file;
pub use blob_files::{BLOB_DESCRIPTOR_LIMIT, BlobReadLease, VerifiedBlob};
pub use entry_blobs::{BlobUploadPage, MAX_ENTRY_BLOB_BYTES};
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
pub(crate) use file_namespace::FileStagingAccess;
pub use file_namespace::{FileDestination, FilePublicationFailure};
mod automatic;
mod initialization;
mod publisher;
pub use automatic::{AutomaticVolumeSlot, AutomaticVolumeState, SessionCreation};
mod key_value;
mod objects;
mod operation_gate;
mod payload_size;
mod permission_preferences;
mod profile_format;
mod session_list;
#[cfg(all(feature = "desktop", any(test, feature = "desktop-qualification")))]
pub(crate) use session_list::SeenParticipant;
#[cfg(feature = "app-host")]
pub(crate) use session_list::SessionActivity;
mod snapshot;
mod ui_zoom;
mod volume_actions;
mod volume_invitations;
pub use volume_invitations::{VolumeInvitationPolicy, VolumeInvitationPublication, VolumeListing};
mod work_offline;
mod world;
use crate::store::AppScope;
use crate::store::account::Credentials;
use crate::store::catalog::*;
use crate::store::invitation::{Invitation, SEED_BYTES, Seed, SessionKeys};
use crate::store::object_contract::{Acl, EntryAcl, ObjectFields, Permission, Schema, Value};
use crate::store::{
    Account, Error, Id, ObjectPage, Result, RootInitialization, now_unix_secs, profile,
    random_array,
};
use kvdb::redb::RedbStorage;
use kvdb::{CreateOptions, Db};
pub use recovery_catalog::{AccountKeyBackup, RecoveryState, RecoveryUpload};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use snapshot::VolumeSnapshot;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, RwLock},
};
#[cfg(feature = "connected-engine")]
pub(crate) use world::WorldReplicaRoute;

pub type Database = Db<RedbStorage>;
/// A local resource guard, not a promised supported collection/database capacity.
pub const DEFAULT_BLOB_CACHE_BUDGET: u64 = 256 * 1024 * 1024;
const VOLUME_NAME_PREFIX: &[u8] = b"volume-name-dirty/";
fn volume_name_marker(volume: Id) -> [u8; VOLUME_NAME_PREFIX.len() + 16] {
    let mut key = [0; VOLUME_NAME_PREFIX.len() + 16];
    key[..VOLUME_NAME_PREFIX.len()].copy_from_slice(VOLUME_NAME_PREFIX);
    key[VOLUME_NAME_PREFIX.len()..].copy_from_slice(volume.as_bytes());
    key
}

fn open_profile_path(
    path: &Path,
    held: Option<ProfileLock>,
    admit: impl FnOnce(&Path, bool) -> Result<()>,
) -> Result<(PathBuf, bool, ProfileLock)> {
    let path = path.to_path_buf();
    // Refuse retained incompatible stores before creating controls, changing
    // permissions, opening a writable database or deleting staged bytes.
    LocalStore::check_profile_format(&path)?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&path).map_err(io)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    let path = path.canonicalize().map_err(io)?;
    crate::paths::private_directory(&path, "control").map_err(io)?;
    let reset = held.is_some();
    let lock = match held {
        Some(lock) => lock,
        None => ProfileLock::open(&path.join(crate::paths::LOCAL_LOCK))?,
    };
    if !reset && path.join("control/sign-out-pending").try_exists().map_err(io)? {
        return Err(Error::InvalidInput("profile reset must finish before storage can open"));
    }
    let fresh = profile_is_fresh(&path)?;
    profile_format::admit(&path, fresh)?;
    admit(&path, fresh)?;
    for name in ["logs", "volumes", "blobs", "builds", "received", "staging", "cache", "apps"] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::InvalidInput(
                    "profile data directory is not an ordinary directory",
                ));
            }
            Ok(_) => {
                crate::paths::private_directory(&path, name).map_err(io)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io(error)),
        }
    }
    Ok((path, fresh, lock))
}

fn clear_profile_staging(path: &Path) -> Result<()> {
    // Only after both outer and inner format admission: unsupported metadata
    // cannot authorize deleting retained staging, even with a familiar sidecar.
    match fs::remove_dir_all(path.join(crate::paths::STAGING)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    Ok(())
}

fn open_profile_metadata(
    path: &Path,
    fresh: bool,
    recovery: &RecoveryPersistence,
    generate: impl FnOnce() -> Result<(Account, [u8; 32])>,
) -> Result<(Database, Account)> {
    let generate = || {
        let (account, seed) = generate()?;
        Ok((account, seed, Id::generate()?))
    };
    let metadata_path = path.join(crate::paths::PROFILE_DB);
    if fresh {
        // Publish a fully committed identity in one atomic rename. A crash
        // before publication resumes the same private initialization file.
        let pending = path.join("control/profile-init.kvdb");
        let metadata = create_db(&pending, None, PROFILE_DATABASE_CACHE_BYTES)?;
        let account =
            profile::open(&metadata, true, now_unix_secs(), generate).map_err(|error| {
                error.local_context(format!("initialize profile identity in {}", pending.display()))
            })?;
        if matches!(recovery, RecoveryPersistence::Ephemeral(_)) {
            put(&metadata, b"ephemeral-recovery", &true)?;
        }
        drop(metadata);
        fs::rename(&pending, &metadata_path).map_err(io)?;
        initialization::sync_directory(path)?;
        initialization::sync_directory(&path.join("control"))?;
        Ok((open_replica(&metadata_path, PROFILE_DATABASE_CACHE_BYTES)?, account))
    } else {
        let metadata = open_replica(&metadata_path, PROFILE_DATABASE_CACHE_BYTES)?;
        let account =
            profile::open(&metadata, false, now_unix_secs(), generate).map_err(|error| {
                error.local_context(format!(
                    "read profile identity from {}",
                    metadata_path.display()
                ))
            })?;
        Ok((metadata, account))
    }
}

fn profile_is_fresh(path: &Path) -> Result<bool> {
    // Unrelated files do not make a profile. Check only our owned paths, without
    // scanning the directory, and never orphan retained data with a new identity.
    let fresh = match fs::symlink_metadata(path.join(crate::paths::PROFILE_DB)) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(io(error)),
    };
    if fresh {
        for name in [
            "account.kvdb",
            "instance.json",
            "session-instances",
            "session-instances.pending",
            "file-io",
            "native-session.json",
            "volumes",
            "blobs",
            "builds",
            "received",
            "staging",
            "cache",
            "apps",
        ] {
            match fs::symlink_metadata(path.join(name)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(io(error)),
                Ok(_) => {
                    return Err(Error::InvalidInput(
                        "profile metadata is missing; select a new empty profile or restore this profile",
                    ));
                }
            }
        }
    }
    Ok(fresh)
}

fn clear_blob_staging(blob_cache_path: &Path) -> Result<()> {
    let blob_staging = blob_cache_path
        .parent()
        .ok_or(Error::corrupt("blob cache path has no parent"))?
        .join("blob-staging");
    match fs::remove_dir_all(blob_staging) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceIdentity {
    pub id: String,
    pub name: String,
    pub owner: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Access {
    account: Id,
    session_id: Option<String>,
    build_id: Option<String>,
    /// Retained File authority is tied to the exact consent incarnation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    personal_incarnation: Option<(Id, Id)>,
    /// Authenticated transport endpoint for one direct private volume exchange.
    #[serde(skip)]
    invitation_peer: Option<Id>,
}
impl Access {
    pub fn owner(account: Id) -> Self {
        Self {
            account,
            session_id: None,
            build_id: None,
            personal_incarnation: None,
            invitation_peer: None,
        }
    }
    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn private_volume_peer(account: Id, peer: Id) -> Self {
        Self { invitation_peer: Some(peer), ..Self::owner(account) }
    }
    pub fn session(account: Id, session_id: impl Into<String>) -> Self {
        Self {
            account,
            session_id: Some(session_id.into()),
            build_id: None,
            personal_incarnation: None,
            invitation_peer: None,
        }
    }
    pub fn session_version(
        account: Id,
        session_id: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            account,
            session_id: Some(session_id.into()),
            build_id: Some(version.into()),
            personal_incarnation: None,
            invitation_peer: None,
        }
    }
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
    pub(crate) fn with_personal_incarnation(mut self, volume: Id, incarnation: Id) -> Self {
        self.personal_incarnation = Some((volume, incarnation));
        self
    }
    pub fn build_id(&self) -> Option<&str> {
        self.build_id.as_deref()
    }
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum PendingKind {
    RegisterVolume {
        volume: Volume,
    },
    RegisterSession {
        session: Session,
        /// Sealed once at creation; retries must resend identical bytes.
        envelope: Vec<u8>,
    },
    UploadBuild {
        build_id: String,
        #[cfg_attr(feature = "app-host", rkyv(with = crate::engine::ipc::codec::UnixPath))]
        path: PathBuf,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct PendingOperation {
    pub id: Id,
    pub operation: PendingKind,
}
/// `session` adopts authorized updates; `invitation` keeps the sealed original.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSession {
    app_name: Option<String>,
    session: Session,
    invitation: Invitation,
}
/// Safe to show before enrollment. Previewing does not attach any volume or
/// run code. Details arrive with the envelope, after the creator registered;
/// a retained session supplies them at once.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct InvitationPreview {
    pub session: String,
    pub paused: bool,
    pub retained: bool,
    pub received: Option<ReceivedInvitation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct ReceivedInvitation {
    pub session: Session,
    pub root_name: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct PendingInvitation {
    seed: Seed,
    paused: bool,
    invitation: Option<Invitation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_removal: Option<u64>,
    // Older hosts stored all seeds. New callers that need an unresolved
    // capability across launches opt in explicitly (including the iOS host).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    retain_unloaded: bool,
}
struct DesktopInvitation {
    session: String,
    pending: Box<PendingInvitation>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum InvitationRetention {
    Durable,
    Desktop,
    #[cfg(all(unix, feature = "connected-engine"))]
    ProfileReset,
}
/// Credentials authorized for a live service replica. Blob and session secrets
/// are deliberately absent from this serialization boundary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceProvisioning {
    pub volume: Volume,
    pub database_key: [u8; 32],
}

/// Initialize an empty disk-backed replica with an independent local replica ID.
/// Existing files are opened with a key check and never reset.
/// The caller's page-cache allocation applies to creation and every reopen.
pub fn create_replica(
    path: &Path,
    database_key: [u8; 32],
    page_cache_bytes: usize,
) -> Result<Database> {
    create_db(path, Some(database_key), page_cache_bytes)
}
pub fn replica_root(database: &Database) -> Result<ReplicaRoot> {
    Ok(database.begin_read()?.root_hash()?)
}
/// An ephemeral ordinary kvdb projection. It is never a published snapshot or a
/// second persistence format; the original source remains authoritative.
pub struct OutboundReplica {
    database: Database,
    _directory: ProjectionDirectory,
}
impl OutboundReplica {
    pub fn database(&self) -> &Database {
        &self.database
    }
    pub fn root(&self) -> Result<ReplicaRoot> {
        replica_root(&self.database)
    }
}
struct ProjectionDirectory(PathBuf);
impl Drop for ProjectionDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A private receive staging database; no live store lock survives its creation.
/// Replaying this database later preserves concurrent local clocks and tombstones.
pub fn detached_replica(scratch: &Path, source: &Database) -> Result<OutboundReplica> {
    let staged = empty_detached_replica(scratch, source)?;
    merge_replica(scratch, source, staged.database(), kvdb::sync::Lww, |_| Ok(()))?;
    Ok(staged)
}
/// Empty discardable protocol target with the source's exact tree identity.
/// Selected Push negotiates against this instead of disclosing existing records.
pub fn empty_detached_replica(scratch: &Path, source: &Database) -> Result<OutboundReplica> {
    empty_replica_with_hash(scratch, source.meta().hash_key, source.meta().branch_factor)
}
/// Blind selected-World writes negotiate against an empty public tree without
/// reading other records in the publisher's canonical backing.
pub fn empty_world_replica(
    scratch: &Path,
    source: &Database,
    volume: Id,
    database: crate::store::DatabaseId,
) -> Result<OutboundReplica> {
    let hash = database.hash_key(&crate::store::DatabaseId::world_projection_key(volume), volume);
    empty_replica_with_hash(scratch, hash, source.meta().branch_factor)
}

fn empty_replica_with_hash(
    scratch: &Path,
    hash_key: [u8; 32],
    branch_factor: u8,
) -> Result<OutboundReplica> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(scratch).map_err(io)?;
    let directory = ProjectionDirectory(scratch.join(format!("replica-{}", Id::generate()?)));
    fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
    let path = directory.0.join("replica.kvdb");
    let redb = crate::store::file_backend::create(&path, 512 * 1024)
        .map_err(|error| Error::System(error.to_string()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(io)?;
    let mut options = CreateOptions::syncable(hash_key, u64::from_le_bytes(random_array()?));
    options.branch_factor = branch_factor;
    let database = Db::create(RedbStorage::from_database(redb), &options)?;
    Ok(OutboundReplica { database, _directory: directory })
}

/// Private, unlinked scratch for one streamed sync entry. Drop closes and
/// discards it; it never becomes a recoverable replica or visible catalog.
pub fn sync_staging(scratch: &Path) -> Result<kvdb::redb::FileStaging> {
    sync_journal(scratch, kvdb::sync::MAX_FRAME)
}
/// Bounded discardable receive journal. The caller owns its finite byte budget;
/// no live database or durable receipt is replaced by this temporary file.
pub fn sync_journal(scratch: &Path, max_bytes: usize) -> Result<kvdb::redb::FileStaging> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(scratch).map_err(io)?;
    let path = scratch.join(format!("sync-{}", Id::generate()?));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    fs::remove_file(&path).map_err(io)?;
    Ok(kvdb::redb::FileStaging::with_capacity(file, max_bytes)?)
}
/// Merge in one transaction, validating before committing any remote entries.
/// Callers supplying a live destination must hold its operation lock.
pub fn merge_replica<R: kvdb::sync::SyncResolver>(
    scratch: &Path,
    source: &Database,
    target: &Database,
    resolver: R,
    validate: impl FnOnce(&kvdb::WriteTxn<'_, RedbStorage>) -> Result<()>,
) -> Result<()> {
    merge_sender(scratch, kvdb::sync::SyncSender::new(source)?, target, resolver, validate, |_| {
        Ok(())
    })
}

pub fn merge_index_replica<R: kvdb::sync::SyncResolver>(
    scratch: &Path,
    source: &Database,
    target: &Database,
    resolver: R,
    validate: impl FnOnce(&kvdb::WriteTxn<'_, RedbStorage>) -> Result<()>,
) -> Result<()> {
    merge_index_with_publication(scratch, source, target, resolver, validate, |_| Ok(()))
}

// Detached merges do not qualify sources. Authorized live callers supply the
// final local publication step so source validation and controls commit together.
fn merge_index_with_publication<R: kvdb::sync::SyncResolver>(
    scratch: &Path,
    source: &Database,
    target: &Database,
    resolver: R,
    validate: impl FnOnce(&kvdb::WriteTxn<'_, RedbStorage>) -> Result<()>,
    publish: impl FnOnce(&mut kvdb::LocalIndexes<'_, '_, RedbStorage>) -> Result<()>,
) -> Result<()> {
    let rejected = std::cell::RefCell::new(None);
    let mut resolver = resolver;
    let checked = |delta: &kvdb::sync::SyncDelta<'_>| {
        if crate::store::volume_index::is_index_key(delta.key)
            && let Err(error) = crate::store::volume_index::validate_sync_source(delta)
        {
            *rejected.borrow_mut() = Some(error);
            return Ok(kvdb::sync::Resolution::KeepLocal);
        }
        resolver.resolve(delta)
    };
    merge_sender(
        scratch,
        kvdb::sync::SyncSender::new(source)?,
        target,
        checked,
        |pending| {
            rejected.borrow_mut().take().map_or(Ok(()), Err)?;
            validate(pending)
        },
        |local| {
            crate::store::volume_index::repair_all(local)?;
            publish(local)
        },
    )
}

fn merge_sender<R: kvdb::sync::SyncResolver>(
    scratch: &Path,
    sender: kvdb::sync::SyncSender<'_, RedbStorage>,
    target: &Database,
    resolver: R,
    validate: impl FnOnce(&kvdb::WriteTxn<'_, RedbStorage>) -> Result<()>,
    repair: impl FnOnce(&mut kvdb::LocalIndexes<'_, '_, RedbStorage>) -> Result<()>,
) -> Result<()> {
    merge_sender_changes(scratch, sender, target, resolver, validate, repair, false)?;
    Ok(())
}

/// A receiving owner can forward only the entries actually admitted and
/// committed. An oversized result uses the ordinary feed's pull-required gap.
fn merge_sender_changes<R: kvdb::sync::SyncResolver>(
    scratch: &Path,
    mut sender: kvdb::sync::SyncSender<'_, RedbStorage>,
    target: &Database,
    resolver: R,
    validate: impl FnOnce(&kvdb::WriteTxn<'_, RedbStorage>) -> Result<()>,
    repair: impl FnOnce(&mut kvdb::LocalIndexes<'_, '_, RedbStorage>) -> Result<()>,
    capture: bool,
) -> Result<Option<kvdb::CommittedDelta>> {
    let _timing = crate::trace::Span::new("replica.copy");
    use kvdb::sync::{SyncReceiver, SyncStatus};
    let staging = sync_staging(scratch)?;
    let mut request = Vec::with_capacity(256 * 1024);
    let mut receiver = if capture {
        SyncReceiver::new_with_delta(
            target,
            resolver,
            staging,
            now_unix_secs(),
            crate::store::MAX_REPLICA_DELTA_BYTES,
            &mut request,
        )?
    } else {
        SyncReceiver::new(target, resolver, staging, now_unix_secs(), &mut request)?
    };
    let mut fragment = vec![0; 256 * 1024];
    loop {
        sender.process(&request)?;
        request.clear();
        let mut completed = false;
        loop {
            let count = sender.write_output(&mut fragment)?;
            if count == 0 {
                break;
            }
            completed |= receiver.process(&fragment[..count], &mut request)? == SyncStatus::Done;
        }
        if completed {
            sender.process(&request)?;
            validate(receiver.pending_transaction()?)?;
            let receiver = receiver.repair_local_indexes(repair)?;
            if capture {
                return Ok(receiver.finish_with_delta()?.1);
            }
            receiver.finish()?;
            return Ok(None);
        }
        if request.is_empty() {
            return Err(Error::Internal("local replica synchronization made no progress"));
        }
    }
}
/// Construct the destination's readable view using the same kvdb sync protocol.
/// Filtering takes place before any source bytes reach the outbound transport.
/// Original entry clocks and tombstones survive projection unchanged.
pub fn outbound_replica(
    scratch: &Path,
    source: &Database,
    policy: &DatabasePolicy,
    reader: ReplicaReader,
) -> Result<OutboundReplica> {
    OutboundSnapshot::new(source)?.project(scratch, policy, reader)
}
/// Pin the source while the matching index policy is held by the volume gate.
pub struct OutboundSnapshot<'db> {
    source: &'db Database,
    sender: kvdb::sync::SyncSender<'db, RedbStorage>,
}
impl<'db> OutboundSnapshot<'db> {
    pub fn new(source: &'db Database) -> Result<Self> {
        Ok(Self { source, sender: kvdb::sync::SyncSender::new(source)? })
    }
    /// Trusted local fallback only. The caller checked matching immutable
    /// births, readiness and both read policies while pinning this snapshot.
    #[cfg(feature = "connected-engine")]
    pub(super) fn copy_log_into(self, scratch: &Path, target: &Database) -> Result<()> {
        merge_sender(scratch, self.sender, target, kvdb::Lww, |_| Ok(()), |_| Ok(()))
    }
    pub fn project(
        self,
        scratch: &Path,
        policy: &DatabasePolicy,
        reader: ReplicaReader,
    ) -> Result<OutboundReplica> {
        policy.ordinary_protocol()?;
        policy.authorize_reader(reader)?;
        let projected = empty_detached_replica(scratch, self.source)?;
        merge_sender(
            scratch,
            self.sender,
            projected.database(),
            kvdb::Lww,
            |_| Ok(()),
            |local| {
                if policy.database == crate::store::DatabaseId::Index {
                    crate::store::volume_index::repair_all(local)
                } else {
                    Ok(())
                }
            },
        )?;
        Ok(projected)
    }
}
pub fn with_outbound_replica<T>(
    scratch: &Path,
    source: &Database,
    policy: &DatabasePolicy,
    reader: ReplicaReader,
    f: impl FnOnce(&Database) -> Result<T>,
) -> Result<T> {
    f(outbound_replica(scratch, source, policy, reader)?.database())
}
pub fn projection_root(
    source: &Database,
    policy: &DatabasePolicy,
    reader: ReplicaReader,
) -> Result<ReplicaRoot> {
    policy.ordinary_protocol()?;
    policy.authorize_reader(reader)?;
    if !source.meta().syncable {
        return Err(kvdb::Error::Mode.into());
    }
    Ok(source.begin_read()?.root_hash()?)
}
/// Entry ACL contraction removes the service's entire backing, in bounded key
/// batches. This erasure is local; it never generates user deletion tombstones.
pub fn prune_replica(
    database: &Database,
    policy: &DatabasePolicy,
    reader: ReplicaReader,
) -> Result<usize> {
    match policy.authorize_reader(reader) {
        Ok(()) => return Ok(0),
        Err(Error::AccessDenied(_)) => {}
        Err(error) => return Err(error),
    }
    let mut count = 0usize;
    loop {
        let snapshot = database.begin_read()?;
        let mut keys = Vec::with_capacity(64);
        let mut cursor = Vec::new();
        while keys.len() < 64 {
            let Some(key) = snapshot.next_source_key(&cursor, !cursor.is_empty())? else { break };
            cursor = key.clone();
            keys.push(key);
        }
        drop(snapshot);
        if keys.is_empty() {
            return Ok(count);
        }
        let mut tx = database.begin_write(now_unix_secs())?;
        for key in &keys {
            tx.del_forget(key)?;
        }
        tx.commit()?;
        count = count.checked_add(keys.len()).ok_or(Error::LimitExceeded)?;
    }
}

pub fn participant_revoked(database: &Database, account: Id) -> Result<bool> {
    Ok(database.begin_read()?.has(account.to_string().as_bytes())?)
}

/// Bound the ordinary page-cache allowance to 32 MiB while retaining the authorization,
/// revocation and application databases of four independent running apps.
const PROVIDER_EXTENSION: bool =
    cfg!(all(target_os = "ios", feature = "file-provider", not(feature = "ios")));
// The lightweight iOS extension has an observed 20 MiB process ceiling. Its
// two file jobs and replica lane do not need four apps' resident DB caches.
const OPEN_DATABASE_LIMIT: usize = if PROVIDER_EXTENSION { 8 } else { 64 };
const OPEN_DATABASE_CACHE_BYTES: usize = if PROVIDER_EXTENSION { 64 * 1024 } else { 512 * 1024 };
const PROFILE_DATABASE_CACHE_BYTES: usize =
    if PROVIDER_EXTENSION { 256 * 1024 } else { 2 * 1024 * 1024 };
const _: () = assert!(std::mem::size_of::<crate::trace::Gauge>() <= 128);
const _: () = assert!(
    OPEN_DATABASE_LIMIT * OPEN_DATABASE_CACHE_BYTES
        == if PROVIDER_EXTENSION { 512 * 1024 } else { 32 * 1024 * 1024 }
);

struct OpenDatabase {
    database: Arc<Database>,
    used: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum DatabaseKey {
    Volume(Id, crate::store::DatabaseId),
    Account,
}

#[derive(Default)]
struct OpenDatabases {
    // Sample the existing recency clock, including open publication, not hit latency.
    trace_uses: crate::trace::Gauge,
    by_id: HashMap<DatabaseKey, OpenDatabase>,
    transitioning: BTreeSet<DatabaseKey>,
    uses: u64,
}

/// One cache slot remains charged throughout opening or backend retirement.
/// Drop also wakes same-file waiters after an error or unwinding worker.
struct DatabaseTransition<'a> {
    store: &'a LocalStore,
    id: DatabaseKey,
}
impl Drop for DatabaseTransition<'_> {
    fn drop(&mut self) {
        if let Ok(mut open) = self.store.open.lock() {
            open.transitioning.remove(&self.id);
        }
        self.store.open_ready.notify_all();
    }
}

// Three concrete authorization dependencies: target, session root and revocations.
const MAX_OPERATION_VOLUMES: usize = 3;
const MAX_VOLUME_OPERATIONS: usize = 256;
type VolumeOperation = Arc<operation_gate::OperationGate>;
type VolumeRegistry = [Option<(Id, VolumeOperation)>; MAX_VOLUME_OPERATIONS];

fn preflight_operations(
    registry: &VolumeRegistry,
    ids: &[Option<Id>; MAX_OPERATION_VOLUMES],
) -> Result<()> {
    if registry.iter().flatten().any(|(_, lock)| lock.is_poisoned()) {
        return Err(Error::Internal("volume operation lock is poisoned"));
    }
    let mut previous = None;
    let mut missing = 0;
    for id in ids.iter().flatten() {
        if previous == Some(*id) {
            continue;
        }
        previous = Some(*id);
        if !registry.iter().flatten().any(|(key, _)| key == id) {
            missing += 1;
        }
    }
    let available = registry
        .iter()
        .filter(|entry| match entry {
            None => true,
            Some((id, lock)) => {
                Arc::strong_count(lock) == 1
                    && lock.idle_backend_fence()
                    && !ids.contains(&Some(*id))
            }
        })
        .count();
    if missing > available {
        return Err(Error::LocalContention("volume operation limit"));
    }
    Ok(())
}

struct OperationLocks<'a> {
    // Dropped after callers' borrowed volume guards, preserving consent until commit.
    _consent: Option<std::sync::RwLockReadGuard<'a, ()>>,
    handles: [Option<VolumeOperation>; MAX_OPERATION_VOLUMES],
}
impl OperationLocks<'_> {
    fn lock(&self) -> Result<[Option<operation_gate::OperationGuard<'_>>; MAX_OPERATION_VOLUMES]> {
        let mut guards = std::array::from_fn(|_| None);
        for (slot, handle) in guards.iter_mut().zip(&self.handles) {
            if let Some(handle) = handle {
                *slot = Some(handle.lock()?);
            }
        }
        Ok(guards)
    }

    fn lock_for_publication(
        &self,
        target: &VolumeOperation,
    ) -> Result<[Option<operation_gate::OperationGuard<'_>>; MAX_OPERATION_VOLUMES]> {
        let mut guards = std::array::from_fn(|_| None);
        for (slot, handle) in guards.iter_mut().zip(&self.handles) {
            if let Some(handle) = handle {
                *slot = Some(if Arc::ptr_eq(handle, target) {
                    handle.lock_for_publication()?
                } else {
                    handle.lock()?
                });
            }
        }
        Ok(guards)
    }
}

struct ProfileLock(fs::File, bool);
impl ProfileLock {
    fn open(path: &Path) -> Result<Self> {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(io)?;
        if !lock.metadata().map_err(io)?.is_file() {
            return Err(Error::corrupt("profile lock is not an ordinary file"));
        }
        lock.try_lock()
            .map_err(|_| Error::LocalContention("local store is in use by another process"))?;
        Ok(Self(lock, true))
    }
}
impl Drop for ProfileLock {
    fn drop(&mut self) {
        // A concurrently spawned child can retain this open-file description
        // until exec. Closing our descriptor alone would leave its lock held.
        if self.1
            && let Err(error) = self.0.unlock()
        {
            crate::diagnostic!("Failed to release local store lock: {error}");
        }
    }
}

enum RecoveryPersistence {
    Durable,
    Ephemeral(Mutex<Option<RecoveryState>>),
}

pub struct LocalStore {
    path: PathBuf,
    blob_cache_path: PathBuf,
    metadata: Database,
    #[cfg(feature = "app-host")]
    session_catalog_epoch: Arc<kvdb::redb::WriteEpoch>,
    #[cfg(feature = "app-host")]
    session_visitors: session_list::SessionVisitors,
    account_cipher: Mutex<Option<account_replica::CachedCipher>>,
    recovery_persistence: RecoveryPersistence,
    account: Account,
    installation: Id,
    changes: crate::store::change_feed::CommittedChangeFeed,
    #[cfg(feature = "connected-engine")]
    volume_invitation_policy_events: tokio::sync::broadcast::Sender<Id>,
    #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
    guest_display_name: tokio::sync::watch::Sender<String>,
    volume_names_generation: std::sync::atomic::AtomicU64,
    // Lock order: account authority/consent -> sorted unique volume operations
    // -> short profile mutation -> blob cache -> open cache -> database txn.
    // Build publication has its own lock and takes it before the profile writer.
    // Session bindings are immutable; authorization/version/denial is reread only
    // after acquiring all dependencies. No network work under any guard.
    profile: Mutex<()>,
    // A single desktop preview. Its registered envelope stays here until the
    // live app acknowledges receipt or explicitly joins the session.
    desktop_preview: Mutex<Option<DesktopInvitation>>,
    // Admission for account-tree writers only. Read snapshots and profile
    // mutations remain independent; callers release before network/adoption.
    account_writer: Mutex<()>,
    account_authority_paused: std::sync::atomic::AtomicBool,
    personal_consent: RwLock<()>,
    /// Pairs cache files with the references that pin them: publishing staged
    /// bytes, recording entry-version pins,
    /// and eviction. Publishing and recording share one critical section, so no
    /// reference is ever recorded for bytes an eviction removed meanwhile.
    /// Hashing, sealing, opening and the staged write and fsync stay outside it
    /// so one volume's large file cannot block another volume's blob work.
    blob_cache: Arc<Mutex<blob_files::BlobCache>>,
    /// Serialize build directory publication through its durable upload record.
    build_publication: Mutex<()>,
    /// Cache invalidation only. Active verified reads may outlive file eviction.
    blob_eviction_generation: std::sync::atomic::AtomicU64,
    entry_blobs: entry_blobs::BlobPreparations,
    private_file_hints: Mutex<private_file_hints::FileHints>,
    snapshots: snapshot::SnapshotCaptures,
    received: received::ReceivePreparations,
    volume_operations: Mutex<VolumeRegistry>,
    session_operations: Mutex<BTreeMap<String, Arc<operation_gate::OperationGate>>>,
    file_namespace_cache: crate::store::volume_index::NamespaceCache<RedbStorage>,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "The test hook mirrors this operation directly.")]
    after_edit_read: Mutex<Option<Arc<dyn Fn(Id) + Send + Sync>>>,
    /// Fault injection for the build upload durability ordering: stop after the
    /// retained file is renamed, before the fsync that publishes its entry.
    #[cfg(test)]
    before_build_fsync: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "The test hook mirrors this operation directly.")]
    before_creation_commit:
        Mutex<Option<Arc<dyn Fn(crate::store::DatabaseId) -> Result<()> + Send + Sync>>>,
    /// Opening a database file costs about a millisecond whatever its size,
    /// while reading from one already open costs about a microsecond, and a
    /// single request reads from as many as three of them.
    open: Mutex<OpenDatabases>,
    open_ready: Condvar,
    // Retain ownership until databases and caches have been released.
    _lock: ProfileLock,
}
impl LocalStore {
    /// Read-only admission for startup owners before any persisted intent can
    /// perform cleanup. Normal opening repeats this under the lifetime lock.
    pub(crate) fn check_profile_format(path: &Path) -> Result<()> {
        profile_format::inspect(path, profile_is_fresh(path)?)?;
        Ok(())
    }
    /// A confirmed reset can be interrupted after identity deletion. Its
    /// supported outer barrier remains authoritative until all data is gone.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn check_profile_reset_format(path: &Path) -> Result<()> {
        if path.join(profile_format::FILE).try_exists().map_err(io)? {
            profile_format::inspect(path, false)?;
            Ok(())
        } else {
            Self::check_profile_format(path)
        }
    }
    fn lock_profile(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let _timing = crate::trace::Span::new("storage.profile_lock_wait");
        self.profile.lock().map_err(|_| Error::Internal("profile lock is poisoned"))
    }
    fn operations(&self, volumes: &[Id]) -> Result<OperationLocks<'_>> {
        if volumes.len() > MAX_OPERATION_VOLUMES {
            return Err(Error::LimitExceeded);
        }
        let mut ids = [None; MAX_OPERATION_VOLUMES];
        for (slot, id) in ids.iter_mut().zip(volumes) {
            *slot = Some(*id);
        }
        ids.sort_unstable();
        let mut handles = std::array::from_fn(|_| None);
        let mut registry = match self.volume_operations.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                let _timing = crate::trace::Span::new("storage.volume_registry_wait");
                self.volume_operations
                    .lock()
                    .map_err(|_| Error::Internal("volume registry lock is poisoned"))?
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(Error::Internal("volume registry lock is poisoned"));
            }
        };
        // Retain the working set, including idle initialized locks. Only evict
        // an idle, unrequested slot when capacity is needed. Preflight the whole
        // request before cloning handles or evicting anything.
        preflight_operations(&registry, &ids)?;
        let mut last = None;
        let mut next = 0;
        for id in ids.into_iter().flatten() {
            if last == Some(id) {
                continue;
            }
            last = Some(id);
            let existing =
                registry.iter().flatten().find(|(key, _)| *key == id).map(|(_, lock)| lock.clone());
            let handle = match existing {
                Some(handle) => handle,
                None => {
                    let slot = registry
                        .iter()
                        .position(|entry| entry.is_none())
                        .or_else(|| {
                            registry.iter().position(|entry| {
                                entry.as_ref().is_some_and(|(key, lock)| {
                                    Arc::strong_count(lock) == 1
                                        && lock.idle_backend_fence()
                                        && !ids.contains(&Some(*key))
                                })
                            })
                        })
                        .ok_or(Error::LocalContention("volume operation limit"))?;
                    // Drop the evicted native allocation before creating another.
                    registry[slot] = None;
                    let handle = Arc::new(operation_gate::OperationGate::default());
                    registry[slot] = Some((id, handle.clone()));
                    handle
                }
            };
            handles[next] = Some(handle);
            next += 1;
        }
        Ok(OperationLocks { _consent: None, handles })
    }
    fn access_operations(&self, access: &Access, volume: Id) -> Result<OperationLocks<'_>> {
        // Account quarantine and consent withdrawal both wait for admitted
        // operations. A later dispatch rechecks the persistent authority fence.
        let consent = self.lock_account_authority()?;
        let Some(id) = &access.session_id else {
            let mut locks = self.operations(&[volume])?;
            locks._consent = Some(consent);
            return Ok(locks);
        };
        // Discovery only. The operation's existing authorize call rereads all
        // mutable admission state after these immutable dependency IDs are locked.
        let session = self.session(id)?;
        let mut locks = self.operations(&[volume, session.replica_volume()])?;
        locks.handles[2] = Some(self.session_operation(id)?);
        locks._consent = Some(consent);
        Ok(locks)
    }
    fn session_operation(&self, id: &str) -> Result<VolumeOperation> {
        validate_version(id)?;
        let mut registry = self
            .session_operations
            .lock()
            .map_err(|_| Error::Internal("session gate registry poisoned"))?;
        if let Some(gate) = registry.get(id).cloned() {
            return Ok(gate);
        }
        if registry.len() >= MAX_VOLUME_OPERATIONS {
            registry.retain(|_, gate| Arc::strong_count(gate) > 1 || gate.is_poisoned());
        }
        if registry.len() >= MAX_VOLUME_OPERATIONS {
            return Err(Error::LocalContention("session operation limit"));
        }
        let gate = Arc::new(operation_gate::OperationGate::default());
        registry.insert(id.into(), gate.clone());
        Ok(gate)
    }
    fn session_dependency_operations(&self, session: &Session) -> Result<OperationLocks<'_>> {
        let mut locks = self.operations(&[session.replica_volume()])?;
        locks.handles[1] = Some(self.session_operation(&session.id)?);
        Ok(locks)
    }
    fn volume_operation(&self, volume: Id) -> Result<VolumeOperation> {
        Ok(self.operations(&[volume])?.handles[0]
            .take()
            .expect("a single-volume operation returns its requested handle"))
    }

    /// Called while an authorized source snapshot holds this volume's gate.
    /// The token is temporary consistency evidence, never content authority.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn source_snapshot_token(&self, volume: Id) -> Result<SnapshotToken> {
        self.volume_operation(volume)?.snapshot_token()
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Durable,
            path,
            None,
            |_, _| Ok(()),
            generate_account,
            || crate::store::change_feed::CommittedChangeFeed::new(Id::generate()?).map_err(io),
        )
    }

    /// Open an operator job's retained data profile without retaining its
    /// account recovery key. The host must supply that key again after restart.
    /// The durable mode marker prevents opening this profile as an ordinary
    /// client, and ordinary profiles cannot be converted to this mode.
    pub fn open_with_ephemeral_recovery(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Ephemeral(Mutex::new(None)),
            path,
            None,
            |_, _| Ok(()),
            generate_account,
            || crate::store::change_feed::CommittedChangeFeed::new(Id::generate()?).map_err(io),
        )
    }

    /// Check and bind the engine's profile authority under the same exclusive
    /// lock that protects initialization, before opening identity or app data.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn open_admitted(
        path: &Path,
        admit: impl FnOnce(&Path, bool) -> Result<()>,
    ) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Durable,
            path,
            None,
            admit,
            generate_account,
            || crate::store::change_feed::CommittedChangeFeed::new(Id::generate()?).map_err(io),
        )
    }

    /// Open durable local services with notification delivered to the host's
    /// event loop. No notification sockets or workers are created. The callback
    /// follows [`crate::store::change_feed::CommittedChangeFeed::with_wake`]'s
    /// scheduling and lifetime contract; it must not call back into this store.
    pub fn open_with_wake(
        path: impl AsRef<Path>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Durable,
            path,
            None,
            |_, _| Ok(()),
            generate_account,
            || Ok(crate::store::change_feed::CommittedChangeFeed::with_wake(Id::generate()?, wake)),
        )
    }

    #[cfg(all(unix, feature = "connected-engine"))]
    pub(crate) fn open_replacement(
        path: &Path,
        lock: &fs::File,
        id: Id,
        seed: [u8; 32],
    ) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Durable,
            path,
            Some(ProfileLock(lock.try_clone().map_err(io)?, false)),
            |_, _| Ok(()),
            || {
                let key = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| {
                    Error::Internal("could not construct the replacement account signing key")
                })?;
                Ok((Account { id, public_key: hex(key.public_key().as_ref()) }, seed))
            },
            || crate::store::change_feed::CommittedChangeFeed::new(Id::generate()?).map_err(io),
        )
    }

    #[cfg(all(unix, feature = "connected-engine"))]
    pub(crate) fn open_for_reset(path: &Path, lock: &fs::File) -> Result<Self> {
        Self::open_with_changes(
            RecoveryPersistence::Durable,
            path,
            Some(ProfileLock(lock.try_clone().map_err(io)?, false)),
            |_, _| Ok(()),
            generate_account,
            || crate::store::change_feed::CommittedChangeFeed::new(Id::generate()?).map_err(io),
        )
    }

    fn open_with_changes(
        recovery_persistence: RecoveryPersistence,
        path: impl AsRef<Path>,
        held: Option<ProfileLock>,
        admit: impl FnOnce(&Path, bool) -> Result<()>,
        generate: impl FnOnce() -> Result<(Account, [u8; 32])>,
        changes: impl FnOnce() -> Result<crate::store::change_feed::CommittedChangeFeed>,
    ) -> Result<Self> {
        let _timing = crate::trace::Span::new("storage.open");
        let (path, fresh, lock) = open_profile_path(path.as_ref(), held, admit)?;
        let (metadata, account) =
            open_profile_metadata(&path, fresh, &recovery_persistence, generate)?;
        clear_profile_staging(&path)?;
        #[cfg(feature = "app-host")]
        let session_catalog_epoch = Arc::new(kvdb::redb::WriteEpoch::default());
        #[cfg(feature = "app-host")]
        metadata.bind_write_epoch(session_catalog_epoch.clone())?;
        let blob_cache_path = crate::paths::Paths::for_profile(&path).map_err(io)?.blob_cache;
        clear_blob_staging(&blob_cache_path)?;
        let store = Self {
            path,
            blob_cache_path,
            installation: profile::installation(&metadata)?,
            #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
            guest_display_name: Self::open_guest_display_name(&metadata)?,
            metadata,
            #[cfg(feature = "app-host")]
            session_catalog_epoch,
            #[cfg(feature = "app-host")]
            session_visitors: Default::default(),
            account_cipher: Mutex::new(None),
            recovery_persistence,
            account,
            changes: changes()?,
            #[cfg(feature = "connected-engine")]
            volume_invitation_policy_events: tokio::sync::broadcast::channel(32).0,
            volume_names_generation: std::sync::atomic::AtomicU64::new(0),
            profile: Mutex::new(()),
            desktop_preview: Mutex::new(None),
            account_writer: Mutex::new(()),
            account_authority_paused: std::sync::atomic::AtomicBool::new(false),
            personal_consent: RwLock::new(()),
            blob_cache: Arc::new(Mutex::new(blob_files::BlobCache::default())),
            build_publication: Mutex::new(()),
            blob_eviction_generation: std::sync::atomic::AtomicU64::new(0),
            entry_blobs: Default::default(),
            private_file_hints: Default::default(),
            snapshots: Default::default(),
            received: Default::default(),
            volume_operations: Mutex::new(std::array::from_fn(|_| None)),
            session_operations: Mutex::new(BTreeMap::new()),
            file_namespace_cache: Default::default(),
            #[cfg(test)]
            after_edit_read: Mutex::new(None),
            #[cfg(test)]
            before_build_fsync: Mutex::new(None),
            #[cfg(test)]
            before_creation_commit: Mutex::new(None),
            open: Mutex::new(OpenDatabases {
                trace_uses: crate::trace::Gauge::new("storage.cache_uses", 0),
                ..OpenDatabases::default()
            }),
            open_ready: Condvar::new(),
            _lock: lock,
        };
        Self::finish_open(store)
    }

    #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
    fn open_guest_display_name(metadata: &Database) -> Result<tokio::sync::watch::Sender<String>> {
        let name: Option<String> = get(metadata, b"settings/display-name")?;
        if let Some(name) = &name {
            crate::store::account::validate_display_name(name)?;
        }
        Ok(tokio::sync::watch::Sender::new(name.unwrap_or_default()))
    }

    fn finish_open(store: Self) -> Result<Self> {
        crate::paths::private_directory(&store.path, crate::paths::VOLUMES).map_err(io)?;
        let recovery = store.recovery_state()?;
        store.prepared_account_key(store.account.id)?;
        store.validate_recovery_persistence()?;
        if matches!(store.recovery_persistence, RecoveryPersistence::Durable)
            && recovery.is_none()
            && (get::<bool>(&store.metadata, b"recovery-key-published")?.unwrap_or(false)
                || store.path.join(crate::paths::ACCOUNT_DB).try_exists().map_err(io)?)
        {
            return Err(Error::InvalidInput("local account key is missing from the profile"));
        }
        store.recover_root_initialization()?;
        store.recover_volume_duplication()?;
        store.recover_received_publications()?;
        for volume in store.pending_volume_names()? {
            // A committed erase has already retired its descriptor. Reclaim its
            // outstanding projection marker on the same worker as the backing.
            if !store.app_data_erasure_pending(volume)? {
                store.project_volume_name(volume)?;
            }
        }
        Ok(store)
    }
    /// The engine or embedding host owns the sole committed-change consumer.
    pub fn take_change_receiver(
        &self,
    ) -> Option<crate::store::change_feed::CommittedChangeReceiver> {
        self.changes.take_receiver()
    }
    pub fn change_status(&self) -> crate::store::change_feed::FeedStatus {
        self.changes.status()
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn publication_generation(&self) -> (u64, u64) {
        self.changes.publication_generation()
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn delivery_revision(&self) -> Option<(Id, u64)> {
        self.changes.delivery_revision()
    }
    #[cfg(any(
        test,
        feature = "native-runtime",
        feature = "wasm-runtime",
        all(not(target_os = "ios"), feature = "process")
    ))]
    pub(crate) fn delivery_scope_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: &[Id],
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.changes.delivery_scope_revision(previous, volumes)
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn delivery_snapshot_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: &[Id],
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.changes.delivery_snapshot_revision(previous, volumes)
    }
    #[cfg(any(test, all(feature = "file-provider", feature = "ios")))]
    pub(crate) fn delivery_catalog_revision(
        &self,
        previous: Option<(Id, u64)>,
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.changes.delivery_catalog_revision(previous)
    }
    #[cfg(any(test, feature = "file-provider"))]
    pub(crate) fn delivery_provider_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: Option<&[Id]>,
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.changes.delivery_provider_revision(previous, volumes)
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn has_pending_operations(&self) -> Result<bool> {
        Ok(!self.pending_operations()?.is_empty())
    }
    pub fn pending_change_count(&self) -> usize {
        self.changes.pending_count()
    }
    #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
    pub(crate) fn profile_path(&self) -> &Path {
        &self.path
    }

    pub fn installation(&self) -> Id {
        self.installation
    }

    pub fn account(&self) -> &Account {
        &self.account
    }
    /// Authentication chooses the account; it does not recover private keys.
    /// A fresh device may retain an invitation while its account is being resolved.
    pub(crate) fn validate_account_binding(&self, id: Id) -> Result<()> {
        if self.account_replacement_required(id)? {
            return Err(Error::InvalidInput("Confirm replacing existing data before signing in"));
        }
        Ok(())
    }
    pub(crate) fn account_replacement_required(&self, id: Id) -> Result<bool> {
        if id == self.account.id {
            return Ok(false);
        }
        Ok(!self.volumes()?.is_empty()
            || !self.sessions()?.is_empty()
            || !self.pending_operations()?.is_empty()
            || self.recovery_state()?.is_some())
    }
    pub fn bind_authenticated_account(&mut self, id: Id) -> Result<()> {
        self.validate_account_binding(id)?;
        if id == self.account.id {
            return Ok(());
        }
        let mut account = self.account.clone();
        account.id = id;
        put(&self.metadata, b"account", &account)?;
        self.account = account;
        Ok(())
    }
    pub fn account_seed(&self) -> Result<[u8; 32]> {
        profile::seed(&self.metadata)
    }
    /// Domain-separated ephemeral message key. It never leaves participant runtimes.
    pub fn session_message_key(&self, session_id: &str) -> Result<[u8; 32]> {
        self.require_account_authority()?;
        Ok(blake3::derive_key("app.playbit.session.messages.v1", &self.session_seed(session_id)?))
    }
    pub fn session_proof(&self, session_id: &str, account: Id) -> Result<Vec<u8>> {
        self.require_account_authority()?;
        Ok(SessionKeys::derive(&self.session_seed(session_id)?)?.proof(account))
    }
    /// A fresh envelope for the retained invitation, as any seed holder could seal it.
    pub fn session_envelope(&self, session_id: &str) -> Result<Vec<u8>> {
        self.require_account_authority()?;
        self.stored_session(session_id)?.invitation.seal()
    }
    /// Retain the capability before sign-in, separately from joined sessions.
    /// Only the seed is known until the creator's registration is read; a
    /// retained session supplies its own invitation at once.
    pub fn preview_invitation(
        &self,
        link: &str,
        channel: crate::release_channel::ReleaseChannel,
    ) -> Result<InvitationPreview> {
        self.preview_link(link, channel, InvitationRetention::Durable)
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn preview_desktop_invitation(
        &self,
        link: &str,
        channel: crate::release_channel::ReleaseChannel,
    ) -> Result<InvitationPreview> {
        self.preview_link(link, channel, InvitationRetention::Desktop)
    }
    #[cfg(all(unix, feature = "connected-engine"))]
    pub(crate) fn restore_invitation_seed(
        &self,
        link: &str,
        channel: crate::release_channel::ReleaseChannel,
    ) -> Result<InvitationPreview> {
        self.preview_link(link, channel, InvitationRetention::ProfileReset)
    }
    fn desktop_preview(&self) -> Result<std::sync::MutexGuard<'_, Option<DesktopInvitation>>> {
        self.desktop_preview
            .lock()
            .map_err(|_| Error::Internal("desktop invitation lock is poisoned"))
    }
    fn preview_link(
        &self,
        link: &str,
        channel: crate::release_channel::ReleaseChannel,
        retention: InvitationRetention,
    ) -> Result<InvitationPreview> {
        let seed = crate::store::invitation::parse_session_link(link, channel)?;
        let session_id = SessionKeys::derive(&seed)?.session_id();
        let _guard = self.lock_profile()?;
        let observed_removal =
            if matches!(retention, InvitationRetention::Desktop | InvitationRetention::Durable) {
                Some(get::<u64>(&self.metadata, session_list::REMOVAL_SERIAL)?.unwrap_or(0))
            } else {
                None
            };
        self.record_invitation_preview(session_id, seed, retention, observed_removal)
    }

    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn prepare_retained_invitation(
        &self,
        session_id: &str,
    ) -> Result<InvitationPreview> {
        let _guard = self.lock_profile()?;
        if self.app_removed(&self.session(session_id)?.app)? {
            return Err(Error::AccessDenied("this app was removed; join its invitation again"));
        }
        self.record_invitation_preview(
            session_id.to_owned(),
            self.session_seed(session_id)?,
            InvitationRetention::Durable,
            None,
        )
    }

    // Caller holds the profile lock so removal and explicit presentation are ordered.
    fn record_invitation_preview(
        &self,
        session_id: String,
        seed: Seed,
        retention: InvitationRetention,
        observed_removal: Option<u64>,
    ) -> Result<InvitationPreview> {
        let key = format!("invitation/{session_id}");
        let mut slot = self.desktop_preview()?;
        let staged = slot.as_ref().filter(|current| current.session == session_id);
        let mut pending = match staged {
            Some(current) => current.pending.as_ref().clone(),
            None => get::<PendingInvitation>(&self.metadata, key.as_bytes())?.unwrap_or(
                PendingInvitation {
                    seed,
                    paused: false,
                    invitation: None,
                    retain_unloaded: false,
                    observed_removal: None,
                },
            ),
        };
        let unconfirmed = staged.is_some() || pending.invitation.is_none();
        pending.observed_removal = observed_removal.or(pending.observed_removal);
        pending.paused = false;
        if pending.invitation.is_none()
            && let Some(stored) =
                get::<StoredSession>(&self.metadata, format!("session/{session_id}").as_bytes())?
        {
            pending.invitation = Some(stored.invitation);
        }
        let mut preview = self.invitation_preview(&session_id, &pending)?;
        if retention == InvitationRetention::Desktop
            && unconfirmed
            && !preview.retained
            && !pending.retain_unloaded
        {
            if let Some(local) = self.local_session(&session_id)? {
                preview.retained = true;
                // A fresh link to a local-only session opens it directly. The
                // desktop has no envelope to acknowledge before PrepareSession.
                if self.app_removed(&local.session.app)? {
                    *slot =
                        Some(DesktopInvitation { session: session_id, pending: Box::new(pending) });
                    return Ok(preview);
                }
            } else {
                *slot = Some(DesktopInvitation { session: session_id, pending: Box::new(pending) });
                return Ok(preview);
            }
        } else {
            pending.retain_unloaded = pending.invitation.is_none()
                && (retention == InvitationRetention::Durable || pending.retain_unloaded);
            self.save_pending_invitation(&key, &pending)?;
        }
        if retention == InvitationRetention::Desktop
            || slot.as_ref().is_some_and(|current| current.session == session_id)
        {
            *slot = None;
        }
        Ok(preview)
    }
    fn invitation_preview(
        &self,
        session_id: &str,
        pending: &PendingInvitation,
    ) -> Result<InvitationPreview> {
        Ok(InvitationPreview {
            session: session_id.to_owned(),
            paused: pending.paused,
            retained: get::<StoredSession>(
                &self.metadata,
                format!("session/{session_id}").as_bytes(),
            )?
            .is_some(),
            received: pending
                .invitation
                .as_ref()
                .map(|invitation| -> Result<ReceivedInvitation> {
                    Ok(ReceivedInvitation {
                        session: invitation.session.clone(),
                        root_name: invitation.replica_volume()?.name.clone(),
                    })
                })
                .transpose()?,
        })
    }
    /// The link holder can seal arbitrary claims. Check its immutable identity
    /// against the AP's authenticated registration before retaining the envelope.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn check_invitation_registration(
        &self,
        session_id: &str,
        envelope: &[u8],
        registered: &Session,
    ) -> Result<()> {
        let pending = self.pending_invitation(session_id)?;
        let invitation = Invitation::open(&pending.seed, envelope)?;
        if !invitation.session.same_identity(registered) {
            return Err(Error::AccessDenied("invitation differs from authenticated session"));
        }
        Ok(())
    }
    /// Open the creator's registered envelope with the pending seed. It must
    /// match any invitation already held for the session.
    pub fn receive_invitation(
        &self,
        session_id: &str,
        envelope: &[u8],
    ) -> Result<InvitationPreview> {
        validate_version(session_id)?;
        let _guard = self.lock_profile()?;
        let key = format!("invitation/{session_id}");
        let mut pending = self.pending_invitation(session_id)?;
        let invitation = Invitation::open(&pending.seed, envelope)?;
        if invitation.session.id != session_id {
            return Err(Error::InvalidInput("invitation identity or credential mismatch"));
        }
        if self.pending_join_superseded(&invitation.session.app, Some(&pending))? {
            let mut tx = self.metadata.begin_write(now_unix_secs())?;
            tx.del(key.as_bytes())?;
            tx.commit()?;
            self.desktop_preview()?.take_if(|current| current.session == session_id);
            return Err(Error::AccessDenied("this app was removed; join its invitation again"));
        }
        match &pending.invitation {
            Some(existing) if *existing != invitation => {
                return Err(Error::InvalidInput("conflicting session invitation"));
            }
            Some(_) => {}
            None => {
                pending.invitation = Some(invitation);
                pending.retain_unloaded = false;
                let mut slot = self.desktop_preview()?;
                if let Some(current) = slot.as_mut().filter(|current| current.session == session_id)
                {
                    let preview = self.invitation_preview(session_id, &pending)?;
                    *current.pending = pending;
                    return Ok(preview);
                }
                self.save_pending_invitation(&key, &pending)?;
            }
        }
        self.invitation_preview(session_id, &pending)
    }
    pub fn pending_invitations(&self) -> Result<Vec<InvitationPreview>> {
        scan_with_keys::<PendingInvitation>(&self.metadata, "invitation/")?
            .into_iter()
            .map(|(session_id, pending)| self.invitation_preview(&session_id, &pending))
            .collect()
    }
    fn pending_invitation(&self, session_id: &str) -> Result<PendingInvitation> {
        validate_version(session_id)?;
        if let Some(current) = self.desktop_preview()?.as_ref().filter(|s| s.session == session_id)
        {
            return Ok(current.pending.as_ref().clone());
        }
        required(&self.metadata, format!("invitation/{session_id}"))
    }
    fn save_pending_invitation(&self, key: &str, pending: &PendingInvitation) -> Result<()> {
        if pending
            .invitation
            .as_ref()
            .is_some_and(|invitation| invitation.session.shared_volume.is_some())
        {
            profile_format::enable_shared_sessions(self)?;
        }
        put(&self.metadata, key.as_bytes(), pending)
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn resolved_desktop_invitation(
        &self,
        session: &str,
    ) -> Result<Option<InvitationPreview>> {
        let slot = self.desktop_preview()?;
        let Some(current) = slot.as_ref().filter(|s| s.session == session && !s.pending.paused)
        else {
            return Ok(None);
        };
        let Some(invitation) = &current.pending.invitation else { return Ok(None) };
        if self.pending_join_superseded(&invitation.session.app, Some(&current.pending))? {
            return Ok(None);
        }
        self.invitation_preview(session, &current.pending).map(Some)
    }
    /// Only the exact live preview can be promoted. Old acknowledgments after a
    /// pause, replacement, or completed join are harmless.
    pub(crate) fn retain_desktop_invitation(&self, session: &str) -> Result<()> {
        validate_version(session)?;
        let _profile = self.lock_profile()?;
        let mut slot = self.desktop_preview()?;
        let Some(current) =
            slot.as_mut().filter(|current| current.session == session && !current.pending.paused)
        else {
            return Ok(());
        };
        let invitation = current
            .pending
            .invitation
            .as_ref()
            .ok_or(Error::InvalidInput("invitation details are unavailable"))?;
        if self.pending_join_superseded(&invitation.session.app, Some(&current.pending))? {
            *slot = None;
            return Ok(());
        }
        let key = format!("invitation/{session}");
        if let Err(error) = self.save_pending_invitation(&key, &current.pending) {
            current.pending.paused = true;
            return Err(error);
        }
        *slot = None;
        Ok(())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn transient_desktop_session(&self) -> Result<Option<String>> {
        Ok(self.desktop_preview()?.as_ref().map(|s| s.session.clone()))
    }
    /// The engine can outlive a desktop process. Drop its transient preview
    /// and retire unresolved records from hosts that persisted every preview.
    #[cfg(feature = "app-host")]
    pub(crate) fn desktop_launch_invitations(
        &self,
        in_use: impl Fn(&str) -> bool,
    ) -> Result<Vec<InvitationPreview>> {
        let _profile = self.lock_profile()?;
        self.desktop_preview()?.take();
        let read = self.metadata.begin_read()?;
        let prefix = b"invitation/";
        let mut write = None;
        let mut previews = Vec::new();
        for item in read.iter_from(prefix) {
            let (key, value) = item?;
            let Some(suffix) = key.strip_prefix(prefix.as_slice()) else { break };
            let pending: PendingInvitation = decode(&value)?;
            let id = std::str::from_utf8(suffix)
                .map_err(|_| Error::corrupt("invalid invitation record key"))?;
            validate_version(id)?;
            let preview = self.invitation_preview(id, &pending)?;
            if preview.received.is_some()
                || preview.retained
                || pending.retain_unloaded
                || in_use(id)
                || read.has(format!("local-session/{id}").as_bytes())?
            {
                previews.push(preview);
                continue;
            }
            if write.is_none() {
                write = Some(self.metadata.begin_write(now_unix_secs())?);
            }
            write.as_mut().expect("write transaction was opened").del(&key)?;
        }
        if let Some(write) = write {
            write.commit()?;
        }
        Ok(previews)
    }
    /// Read only the selected capability after the reset owner has stopped the
    /// engine and acquired local.lock. This needs no account/Keychain access.
    #[cfg(all(unix, feature = "connected-engine"))]
    pub(crate) fn invitation_for_reset(path: &Path, session: &str) -> Result<(Seed, bool)> {
        profile_format::inspect(path, false)?;
        let metadata =
            open_replica(&path.join(crate::paths::PROFILE_DB), PROFILE_DATABASE_CACHE_BYTES)?;
        let pending: PendingInvitation =
            get(&metadata, format!("invitation/{session}").as_bytes())?
                .ok_or(Error::NotFound("pending invitation"))?;
        Ok((pending.seed, pending.paused))
    }
    pub fn pending_invitation_preview(&self, session_id: &str) -> Result<InvitationPreview> {
        self.invitation_preview(session_id, &self.pending_invitation(session_id)?)
    }
    pub fn pending_invitation_paused(&self, session_id: &str) -> Result<bool> {
        Ok(self.pending_invitation(session_id)?.paused)
    }
    pub fn pause_invitation(&self, session_id: &str, paused: bool) -> Result<()> {
        validate_version(session_id)?;
        let _guard = self.lock_profile()?;
        if let Some(current) = self.desktop_preview()?.as_mut().filter(|s| s.session == session_id)
        {
            current.pending.paused = paused;
            return Ok(());
        }
        let key = format!("invitation/{session_id}");
        let mut pending: PendingInvitation = required(&self.metadata, &key)?;
        pending.paused = paused;
        put(&self.metadata, key.as_bytes(), &pending)
    }
    pub fn pending_invitation_proof(&self, session_id: &str, account: Id) -> Result<Vec<u8>> {
        let pending = self.pending_invitation(session_id)?;
        Ok(SessionKeys::derive(&pending.seed)?.proof(account))
    }
    pub fn complete_invitation(&self, session_id: &str) -> Result<()> {
        validate_version(session_id)?;
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let key = format!("invitation/{session_id}");
        let pending =
            tx.get(key.as_bytes())?.map(|bytes| decode::<PendingInvitation>(&bytes)).transpose()?;
        tx.del(key.as_bytes())?;
        let changed = match self.session(session_id) {
            Ok(session) if self.pending_join_superseded(&session.app, pending.as_ref())? => {
                tx.commit()?;
                return Ok(());
            }
            Ok(session) if pending.is_some() => {
                self.stage_app_removed(&mut tx, &session.app, false)?
            }
            Ok(_) | Err(Error::NotFound(_)) => false,
            Err(error) => return Err(error),
        };
        tx.del(session_list::hidden(session_id).as_bytes())?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        self.committed_account_record(changed);
        Ok(())
    }
    /// Retain a received invitation before bootstrap, preserving all original
    /// identities. Re-import is idempotent and never queues owner registrations.
    pub fn import_invitation(&self, session_id: &str) -> Result<Session> {
        self.retain_desktop_invitation(session_id)?;
        let pending = self.pending_invitation(session_id)?;
        let invitation = pending.invitation.ok_or(Error::NotFound("registered invitation"))?;
        self.adopt_invitation(&invitation)
    }
    /// Preview, receive and import in one step for a caller that already holds
    /// the envelope, as a joining client does after reading it from the AP.
    pub fn import_invitation_link(
        &self,
        link: &str,
        envelope: &[u8],
        channel: crate::release_channel::ReleaseChannel,
    ) -> Result<Session> {
        let preview = self.preview_invitation(link, channel)?;
        self.receive_invitation(&preview.session, envelope)?;
        self.import_invitation(&preview.session)
    }
    fn prepare_invitation_volumes(&self, invitation: &Invitation) -> Result<()> {
        {
            let (volume, credentials) =
                (invitation.replica_volume()?, invitation.replica_credentials()?);
            if let Some(existing) =
                get::<Volume>(&self.metadata, format!("volume/{}", volume.id).as_bytes())?
                && (!existing.same_identity(volume)
                    || match get::<Credentials>(
                        &self.metadata,
                        format!("credentials/{}", volume.id).as_bytes(),
                    )? {
                        Some(stored) => stored != *credentials,
                        None => !self.uses_world_projection(volume.id)?,
                    })
            {
                return Err(Error::InvalidInput("conflicting volume invitation"));
            }
            let dir = self.path.join(crate::paths::VOLUMES).join(volume.id.to_string());
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(io)?;
            for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations]
            {
                self.replica_db(volume.id, database, Some(credentials.database_key))?;
            }
        }
        Ok(())
    }
    fn adopt_invitation(&self, invitation: &Invitation) -> Result<Session> {
        invitation.verify()?;
        let session = &invitation.session;
        if self
            .local_session(&session.id)?
            .is_some_and(|local| !local.session.same_identity(session))
        {
            return Err(Error::AccessDenied("invitation changes an existing local session"));
        }
        let app_name = None;
        let operations = self.operations(&[session.replica_volume()])?;
        let _guards = operations.lock()?;
        if let Some(existing) =
            get::<StoredSession>(&self.metadata, format!("session/{}", session.id).as_bytes())?
        {
            if existing.invitation != *invitation {
                return Err(Error::InvalidInput("conflicting session invitation"));
            }
            return Ok(existing.session);
        }
        self.prepare_invitation_volumes(invitation)?;
        let _profile = self.lock_profile()?;
        // Recovery may have installed the same immutable invitation while files
        // were prepared; never overwrite a newly adopted retained session.
        if let Some(existing) =
            get::<StoredSession>(&self.metadata, format!("session/{}", session.id).as_bytes())?
        {
            if existing.invitation != *invitation {
                return Err(Error::InvalidInput("conflicting session invitation"));
            }
            return Ok(existing.session);
        }
        if session.shared_volume.is_some() {
            profile_format::enable_shared_sessions(self)?;
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        {
            let (volume, credentials) =
                (invitation.replica_volume()?, invitation.replica_credentials()?);
            let key = format!("volume/{}", volume.id);
            // An older invitation must not replace a name already projected
            // from this volume's authoritative metadata.
            if tx.get(key.as_bytes())?.is_none() {
                tx.put(key.as_bytes(), &encode(volume)?)?;
            }
            tx.put(format!("credentials/{}", volume.id).as_bytes(), &encode(credentials)?)?;
        }
        tx.put(
            format!("session/{}", session.id).as_bytes(),
            &encode(&StoredSession {
                app_name: app_name.clone(),
                session: session.clone(),
                invitation: invitation.clone(),
            })?,
        )?;
        let account_changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::Invitation {
                app_name: app_name.clone(),
                invitation: invitation.clone(),
            },
        )?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        self.committed_account_record(account_changed);
        Ok(session.clone())
    }
    pub fn has_service_excluded_data(&self, volume: Id) -> Result<bool> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let index = self.db(volume, crate::store::DatabaseId::Index)?;
        let policy = MetadataPolicy::read(&index)?;
        if !policy.volume.share_key_with_service {
            return Ok(true);
        }
        let snapshot = crate::store::volume_index::IndexSnapshot::new(index.begin_read()?);
        let mut after = None;
        loop {
            let page = snapshot.headers(after, 64)?;
            for entry in page.entries {
                if !entry.deleted
                    && snapshot
                        .authorize(
                            &policy.volume,
                            &policy.permissions,
                            PLAYBIT_REPLICA_PRINCIPAL,
                            entry.id,
                            Permission::Read,
                        )
                        .is_err()
                {
                    return Ok(true);
                }
            }
            let Some(next) = page.next else {
                return Ok(false);
            };
            after = Some(next);
        }
    }
    pub fn service_provisioning(&self, volume: Id) -> Result<ServiceProvisioning> {
        self.require_replication_supported(volume)?;
        self.require_account_authority()?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let index = self.index(volume)?;
        if index.volume.owner != self.account.id {
            return Err(Error::AccessDenied("volume owner"));
        }
        if !index.volume.share_key_with_service {
            return Err(Error::AccessDenied("volume key is not shared with AP"));
        }
        Ok(ServiceProvisioning {
            volume: index.volume,
            database_key: self.credentials(volume)?.database_key,
        })
    }
    pub fn replica_root(
        &self,
        volume: Id,
        database: crate::store::DatabaseId,
    ) -> Result<ReplicaRoot> {
        self.with_replica(volume, database, replica_root)
    }
    pub fn durable_revision(
        &self,
        volume: Id,
        database: crate::store::DatabaseId,
    ) -> Result<Option<DurableRevision>> {
        get(&self.metadata, format!("durable/{volume}/{database}").as_bytes())
    }
    /// Record the exact acknowledged root, not the possibly newer local root.
    pub fn acknowledge_replica(
        &self,
        volume: Id,
        database: crate::store::DatabaseId,
        ack: &DurableRevision,
    ) -> Result<()> {
        let _guard = self.lock_profile()?;
        self.volume(volume)?;
        if let Some(previous) = self.durable_revision(volume, database)?
            && (ack.revision < previous.revision
                || ack.policy_revision < previous.policy_revision
                || (ack.revision == previous.revision
                    && ack.policy_revision == previous.policy_revision
                    && ack.root != previous.root))
        {
            return Err(Error::InvalidInput("stale or conflicting durable acknowledgment"));
        }
        put(&self.metadata, format!("durable/{volume}/{database}").as_bytes(), ack)?;
        drop(_guard);
        if database == crate::store::DatabaseId::Index {
            self.acknowledge_provider_snapshot(volume, ack.root)?;
        }
        Ok(())
    }
    pub fn authorize_replication(
        &self,
        access: &Access,
        volume: Id,
        permission: Permission,
    ) -> Result<()> {
        self.require_replication_supported(volume)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.index(volume)?;
        self.authorize(access, &index, permission)?;
        Ok(())
    }
    pub fn service_identities(&self) -> Result<Vec<ServiceIdentity>> {
        scan(&self.metadata, "service/")
    }
    pub fn create_service_identity(&self, name: &str) -> Result<ServiceIdentity> {
        let _guard = self.lock_profile()?;
        if name.trim().is_empty() {
            return Err(Error::InvalidInput("service identity name is empty"));
        }
        let secret = random_array::<32>()?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&secret)
            .map_err(|_| Error::Internal("could not construct the service signing key"))?;
        let service = ServiceIdentity {
            id: crate::identifier::encode(pair.public_key().as_ref()),
            name: name.to_owned(),
            owner: self.account.id,
        };
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("service/{}", service.id).as_bytes(), &encode(&service)?)?;
        tx.put(format!("service-secret/{}", service.id).as_bytes(), &secret)?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        Ok(service)
    }
    #[cfg(any(
        feature = "native-runtime",
        all(feature = "wasm-runtime", feature = "connected-engine")
    ))]
    pub(crate) fn guest_display_name(&self) -> tokio::sync::watch::Ref<'_, String> {
        self.guest_display_name.borrow()
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn subscribe_display_name(&self) -> tokio::sync::watch::Receiver<String> {
        self.guest_display_name.subscribe()
    }

    #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
    fn publish_display_name(&self, name: &str) -> bool {
        self.guest_display_name.send_if_modified(|current| {
            if current == name {
                return false;
            }
            name.clone_into(current);
            true
        })
    }

    pub fn display_name(&self) -> Result<Option<String>> {
        get(&self.metadata, b"settings/display-name")
    }
    pub fn set_display_name(&self, name: &str) -> Result<()> {
        let _timing = crate::trace::Span::new("account.name.commit");
        crate::store::account::validate_display_name(name)?;
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let state = self.transactional_recovery(&tx)?;
        if !state.is_some_and(|state| state.acknowledged) {
            return Err(Error::AccessDenied(
                "save or recover the account secret before changing its name",
            ));
        }
        tx.put(b"settings/display-name", &encode(&name)?)?;
        let changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::DisplayName { name: name.to_owned() },
        )?;
        tx.commit()?;
        #[cfg(any(feature = "connected-engine", feature = "native-runtime"))]
        if self.publish_display_name(name) && !changed {
            // A saved recovery key can precede the first account publication.
            self.changes.local_policy_changed();
        }
        self.committed_account_record(changed);
        Ok(())
    }
    pub fn create_volume(&self, name: &str, share_key_with_service: bool) -> Result<Volume> {
        let id = Id::generate()?;
        let operations = self.operations(&[id])?;
        let _guards = operations.lock()?;
        self.new_volume(id, name, share_key_with_service, true)
    }
    fn new_volume(
        &self,
        id: Id,
        name: &str,
        share_key_with_service: bool,
        personal: bool,
    ) -> Result<Volume> {
        validate_volume_label(name)?;
        let volume = Volume {
            id,
            name: name.to_owned(),
            owner: self.account.id,
            personal,
            share_key_with_service,
        };
        let credentials = Credentials { database_key: random_array()?, blob_key: random_array()? };
        let index = MetadataPolicy::new(volume.clone(), Acl::default())?;
        let dir = self.path.join(crate::paths::VOLUMES).join(volume.id.to_string());
        fs::DirBuilder::new().mode(0o700).create(&dir).map_err(io)?;
        let metadata = self.replica_db(
            volume.id,
            crate::store::DatabaseId::Index,
            Some(credentials.database_key),
        )?;
        let delta = index.write_with_delta(
            &metadata,
            now_unix_secs(),
            crate::store::MAX_REPLICA_DELTA_BYTES,
        )?;
        self.replica_db(
            volume.id,
            crate::store::DatabaseId::Revocations,
            Some(credentials.database_key),
        )?;
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterVolume { volume: volume.clone() },
        };
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("volume/{}", volume.id).as_bytes(), &encode(&volume)?)?;
        tx.put(format!("credentials/{}", volume.id).as_bytes(), &encode(&credentials)?)?;
        tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        let account_changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::Volume { volume: volume.clone(), credentials },
        )?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        self.committed_account_record(account_changed);
        self.changes.publish(volume.id, crate::store::DatabaseId::Index, delta);
        self.notify_volume_invitation_policy(&volume);
        Ok(volume)
    }
    pub fn volumes(&self) -> Result<Vec<Volume>> {
        let mut volumes: Vec<Volume> = scan(&self.metadata, "volume/")?;
        let snapshot = self.metadata.begin_read()?;
        let mut retained = Vec::with_capacity(volumes.len());
        for volume in volumes.drain(..) {
            if !snapshot.has(format!("deleted-volume/{}", volume.id).as_bytes())? {
                retained.push(volume);
            }
        }
        Ok(retained)
    }
    pub fn volume(&self, id: Id) -> Result<Volume> {
        self.require_volume_available(id)?;
        required(&self.metadata, format!("volume/{id}"))
    }
    pub fn volume_names_generation(&self) -> u64 {
        self.volume_names_generation.load(std::sync::atomic::Ordering::Acquire)
    }
    fn pending_volume_names(&self) -> Result<Vec<Id>> {
        let mut volumes = Vec::new();
        for entry in self.metadata.begin_read()?.iter() {
            let (key, value) = entry?;
            if let Some(id) = key.strip_prefix(VOLUME_NAME_PREFIX) {
                if volumes.len() == 256 {
                    return Err(Error::LimitExceeded);
                }
                if !value.is_empty() {
                    return Err(Error::corrupt("volume name marker must have an empty value"));
                }
                volumes.push(Id::from_bytes(
                    id.try_into().map_err(|_| {
                        Error::corrupt("volume name marker has an invalid ID length")
                    })?,
                ));
            }
        }
        Ok(volumes)
    }
    /// Call before starting the DB1 write, while holding the volume gate.
    fn prepare_volume_name(&self, volume: Id) -> Result<()> {
        let _profile = self.lock_profile()?;
        let key = volume_name_marker(volume);
        if self.metadata.begin_read()?.has(&key)? {
            return Ok(());
        }
        if self.pending_volume_names()?.len() == 256 {
            return Err(Error::LimitExceeded);
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(&key, &[])?;
        tx.commit()?;
        Ok(())
    }
    /// DB1 is authoritative. The profile row is the existing listing projection;
    /// its durable marker covers crashes before or after the DB1 commit.
    fn project_volume_name(&self, volume: Id) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.project_volume_name");
        let path = self
            .path
            .join(crate::paths::VOLUMES)
            .join(volume.to_string())
            .join(crate::store::DatabaseId::Index.file_name());
        let current = if path.try_exists().map_err(io)? {
            let db = self.replica_db(volume, crate::store::DatabaseId::Index, None)?;
            let tx = db.begin_read()?;
            tx.get(&VOLUME_KEY)?.map(|bytes| record::decode::<Volume>(&bytes)).transpose()?
        } else {
            None
        };
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let key = format!("volume/{volume}");
        let previous: Volume = decode(
            &tx.get(key.as_bytes())?
                .ok_or(Error::corrupt("volume metadata is missing while updating its name"))?,
        )?;
        let changed = current.as_ref().is_some_and(|current| current.name != previous.name);
        if let Some(current) = current {
            if !current.same_identity(&previous) {
                return Err(Error::AccessDenied("replica volume mismatch"));
            }
            if changed {
                tx.put(key.as_bytes(), &encode(&current)?)?;
            }
        }
        tx.del(&volume_name_marker(volume))?;
        tx.commit()?;
        if changed {
            self.volume_names_generation.fetch_add(1, std::sync::atomic::Ordering::Release);
            self.notify_volume_invitation_policy(&previous);
        }
        Ok(())
    }
    pub fn label_volume(&self, access: &Access, volume: Id, label: &str) -> Result<Volume> {
        let _timing = crate::trace::Span::new("storage.label_volume");
        validate_volume_label(label)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let mut current: Volume = {
            let tx = db.begin_read()?;
            record::decode(&tx.get(&VOLUME_KEY)?.ok_or(Error::NotFound("volume policy"))?)?
        };
        if current.owner != access.account || access.session_id.is_some() {
            return Err(Error::AccessDenied("volume owner"));
        }
        if current.name == label {
            if self.metadata.begin_read()?.has(&volume_name_marker(volume))? {
                self.project_volume_name(volume)?;
            }
            return Ok(current);
        }
        self.prepare_volume_name(volume)?;
        current.name = label.into();
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let bytes = record::rewrite(
            &tx.get(&VOLUME_KEY)?.ok_or(Error::NotFound("volume policy"))?,
            &current,
        )?;
        tx.put(&VOLUME_KEY, &bytes)?;
        let delta = {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        self.project_volume_name(volume)?;
        Ok(current)
    }
    pub fn create_session(
        &self,
        app: &crate::package::AppIdentity,
        selector: BuildSelector,
        resolved_build_id: &str,
        root_volume: Id,
    ) -> Result<Session> {
        self.require_replication_supported(root_volume)?;
        let operations = self.operations(&[root_volume])?;
        let _guards = operations.lock()?;
        validate_version(resolved_build_id)?;
        match &selector {
            BuildSelector::Exact { build_id } if build_id != resolved_build_id => {
                return Err(Error::InvalidInput("exact selector differs from resolved version"));
            }
            BuildSelector::App { app: selected } if selected != app => {
                return Err(Error::InvalidInput("selected app differs"));
            }
            _ => {}
        }
        let index = self.index(root_volume)?;
        if index.volume.owner != self.account.id
            && !index.provisioned_versions.contains(resolved_build_id)
        {
            return Err(Error::AccessDenied("root version provisioning"));
        }
        let seed = random_array::<SEED_BYTES>()?;
        let keys = SessionKeys::derive(&seed)?;
        let authority = match selector {
            BuildSelector::Exact { .. } => UpdateAuthority::Creator,
            BuildSelector::App { .. } => UpdateAuthority::App,
        };
        app.validate().map_err(Error::InvalidInput)?;
        if matches!(&selector, BuildSelector::Exact { .. }) && app.publisher != self.account.id {
            return Err(Error::InvalidInput("direct app publisher differs from session creator"));
        }
        let app = app.clone();
        let session = Session {
            id: keys.session_id(),
            selector,
            resolved_build_id: resolved_build_id.to_owned(),
            root_volume,
            shared_volume: None,
            app,

            creator: self.account.id,
            update_authority: authority,
        };
        // Every decryption credential stays in the sealed envelope; the AP
        // stores it but only a link holder can open it.
        let invitation = Invitation {
            seed,
            session: session.clone(),
            root: Some(self.volume(root_volume)?),
            root_credentials: Some(self.credentials(root_volume)?),
            shared: None,
        };
        let envelope = invitation.seal()?;
        let stored = StoredSession {
            app_name: self.cached_app_name(resolved_build_id)?,
            session: session.clone(),
            invitation,
        };
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterSession { session: session.clone(), envelope },
        };
        let _profile = self.lock_profile()?;
        let started = now_unix_secs();
        let mut tx = self.metadata.begin_write(started)?;
        tx.put(format!("session/{}", session.id).as_bytes(), &encode(&stored)?)?;
        tx.put(format!("session-start/{}", session.id).as_bytes(), &started.to_le_bytes())?;
        tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        let account_changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::Invitation {
                app_name: stored.app_name.clone(),
                invitation: stored.invitation.clone(),
            },
        )?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        self.committed_account_record(account_changed);
        Ok(session)
    }
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = Vec::new();
        for stored in scan::<StoredSession>(&self.metadata, "session/")? {
            if self.local_session(&stored.session.id)?.is_none()
                && !self.volume_deleted(stored.session.replica_volume())?
            {
                sessions.push(stored.session);
            }
        }
        for stored in scan::<automatic::LocalSession>(&self.metadata, "local-session/")? {
            if !self.volume_deleted(stored.session.replica_volume())? {
                sessions.push(stored.session);
            }
        }
        Ok(sessions)
    }
    pub(crate) fn catalog_session(&self, id: &str) -> Result<Session> {
        Ok(if let Some(local) = self.local_session(id)? {
            local.session
        } else {
            self.stored_session(id)?.session
        })
    }
    pub fn session(&self, id: &str) -> Result<Session> {
        let session = self.catalog_session(id)?;
        self.require_volume_available(session.replica_volume())?;
        Ok(session)
    }
    fn stored_session(&self, id: &str) -> Result<StoredSession> {
        validate_version(id)?;
        required(&self.metadata, format!("session/{id}"))
    }
    /// The shareable link of a retained session.
    pub fn invitation(
        &self,
        id: &str,
        channel: crate::release_channel::ReleaseChannel,
    ) -> Result<String> {
        self.require_account_authority()?;
        if let Some(local) = self.local_session(id)? {
            if local.session.shared_volume.is_some() {
                self.open_automatic_volume(id, crate::package::AutomaticVolumeRole::Session)?;
                return Ok(self.stored_session(id)?.invitation.link(channel));
            }
            return Err(Error::AccessDenied(
                "sharing new app sessions requires encrypted synchronization, which is not supported yet",
            ));
        }
        Ok(self.stored_session(id)?.invitation.link(channel))
    }
    pub fn session_seed(&self, id: &str) -> Result<Seed> {
        self.require_account_authority()?;
        if let Some(local) = self.local_session(id)? {
            return Ok(local.seed);
        }
        Ok(self.stored_session(id)?.invitation.seed)
    }
    pub fn pending_operations(&self) -> Result<Vec<PendingOperation>> {
        let operations: Vec<PendingOperation> = scan(&self.metadata, "pending/")?;
        let mut pending = Vec::with_capacity(operations.len());
        for operation in operations {
            let deleted = match &operation.operation {
                PendingKind::RegisterVolume { volume } => self.volume_deleted(volume.id)?,
                PendingKind::RegisterSession { session, .. } => {
                    self.volume_deleted(session.replica_volume())?
                }
                PendingKind::UploadBuild { .. } => false,
            };
            if !deleted {
                pending.push(operation);
            }
        }
        Ok(pending)
    }
    /// Remove only the acknowledged operation. Registration does not acknowledge content.
    pub fn acknowledge_pending(&self, operation: &PendingOperation) -> Result<()> {
        let _guard = self.lock_profile()?;
        let key = format!("pending/{}", operation.id);
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let value = tx.get(key.as_bytes())?.ok_or(Error::NotFound("pending operation"))?;
        if decode::<PendingOperation>(&value)? != *operation {
            return Err(Error::InvalidInput("acknowledgment does not match pending operation"));
        }
        drop(value);
        if let PendingKind::UploadBuild { build_id, .. } = &operation.operation {
            tx.put(format!("uploaded-build/{build_id}").as_bytes(), &[])?;
        }
        tx.del(key.as_bytes())?;
        {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit()?;
        }
        if let PendingKind::RegisterVolume { volume } = &operation.operation {
            self.notify_volume_invitation_policy(volume);
        }
        Ok(())
    }
    pub fn queue_build_upload(
        &self,
        build_id: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<PendingOperation>> {
        if self.build_uploaded(build_id)? {
            return Ok(None);
        }
        let path = fs::canonicalize(path).map_err(io)?;
        // Retain the upload bytes inside the profile; source/build cleanup cannot
        // destroy pending work. The engine validates bundle identity before queueing.
        let directory = self.path.join(crate::paths::BUILDS);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory).map_err(io)?;
        let retained = directory.join(build_id);
        // Copying the bundle is the expensive part and takes no lock; the
        // destination is named by the build, so a racing retry copies the same
        // bytes. Publication and the pending record that names it share one
        // critical section, and whoever records fsyncs the directory first, so a
        // durable record never names an entry another writer has renamed into
        // place but not yet made durable.
        let staged =
            if retained.exists() { None } else { Some(self.stage_build(&directory, &path)?) };
        let _publication = self
            .build_publication
            .lock()
            .map_err(|_| Error::Internal("build publication lock is poisoned"))?;
        let staged = match staged {
            Some(temporary) => Some(temporary),
            None if retained.exists() => None,
            // Retained when this call started, gone by the time it held the lock.
            None => Some(self.stage_build(&directory, &path)?),
        };
        if let Some(temporary) = staged {
            fs::rename(&temporary, &retained).map_err(io)?;
            #[cfg(test)]
            {
                let hook = self
                    .before_build_fsync
                    .lock()
                    .map_err(|_| Error::Internal("build publication hook lock is poisoned"))?
                    .clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
        }
        initialization::sync_directory(&directory)?;
        let _profile = self.lock_profile()?;
        if self.build_uploaded(build_id)? {
            return Ok(None);
        }
        for op in self.pending_operations()? {
            if let PendingKind::UploadBuild { build_id: version, .. } = &op.operation
                && version == build_id
            {
                return Ok(Some(op));
            }
        }
        let operation = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::UploadBuild {
                build_id: build_id.to_owned(),
                path: PathBuf::from("builds").join(build_id),
            },
        };
        put(&self.metadata, format!("pending/{}", operation.id).as_bytes(), &operation)?;
        self.changes.account_changed();
        Ok(Some(operation))
    }
    pub(crate) fn build_uploaded(&self, build_id: &str) -> Result<bool> {
        validate_version(build_id)?;
        self.metadata
            .begin_read()?
            .has(format!("uploaded-build/{build_id}").as_bytes())
            .map_err(Into::into)
    }
    /// A verified AP download also proves the immutable build is already stored.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn remember_uploaded_build(&self, build_id: &str) -> Result<()> {
        validate_version(build_id)?;
        let _profile = self.lock_profile()?;
        let key = format!("uploaded-build/{build_id}");
        if !self.metadata.begin_read()?.has(key.as_bytes())? {
            let mut tx = self.metadata.begin_write(now_unix_secs())?;
            tx.put(key.as_bytes(), &[])?;
            tx.commit()?;
        }
        Ok(())
    }
    /// Copy and fsync the upload bytes without holding the build publication lock. The
    /// returned file is published by the rename the lock covers.
    fn stage_build(&self, directory: &Path, source: &Path) -> Result<PathBuf> {
        let bytes = fs::read(source).map_err(io)?;
        let temporary = directory.join(format!("{}.tmp", Id::generate()?));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(io)?;
        std::io::Write::write_all(&mut file, &bytes).map_err(io)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(|e| io(e.into()))?;
        Ok(temporary)
    }
    fn credentials(&self, volume: Id) -> Result<Credentials> {
        required(&self.metadata, format!("credentials/{volume}"))
    }
    /// Only fault-injection tests replace an existing replica file.
    #[cfg(test)]
    fn close_db(&self, volume: Id, database: crate::store::DatabaseId) -> Result<()> {
        self.open
            .lock()
            .map_err(|_| Error::Internal("open database cache lock is poisoned"))?
            .by_id
            .remove(&DatabaseKey::Volume(volume, database));
        Ok(())
    }
    fn db(&self, volume: Id, database: crate::store::DatabaseId) -> Result<Arc<Database>> {
        use crate::store::{DatabaseId, volume_index::IndexSnapshot};
        self.volume(volume)?;
        if let DatabaseId::Entry(entry) = database {
            let index = self.replica_db(volume, DatabaseId::Index, None)?;
            if IndexSnapshot::new(index.begin_read()?).backing_quarantined(entry)? {
                return Err(Error::NotFound(Error::PENDING_BACKING));
            }
        }
        self.replica_db(volume, database, None)
    }
    /// All attached replica opening/creation shares one bounded handle cache.
    /// Creation requires a key already validated by the admitting operation.
    fn replica_db(
        &self,
        volume: Id,
        database: crate::store::DatabaseId,
        create: Option<[u8; 32]>,
    ) -> Result<Arc<Database>> {
        let _timing = crate::trace::Span::detail("storage.cache_lookup");
        self.require_volume_available(volume)?;
        // Keep the registry fence alive across open-cache eviction and binding.
        // Operations already hold their sorted volume guards; this does not lock.
        let operations = self.operations(&[volume])?;
        let operation = operations.handles[0]
            .as_ref()
            .ok_or(Error::Internal("admitted volume operation handle is missing"))?;
        let path = self
            .path
            .join(crate::paths::VOLUMES)
            .join(volume.to_string())
            .join(database.file_name());
        let master = match create {
            Some(master) if !self.uses_world_projection(volume)? => master,
            Some(_) => crate::store::DatabaseId::world_projection_key(volume),
            None => self.local_database_key(volume)?,
        };
        let key = database.hash_key(&master, volume);
        let opened =
            self.cached_db(DatabaseKey::Volume(volume, database), &path, create.map(|_| key))?;
        if !opened.meta().syncable {
            return Err(Error::Store(kvdb::Error::Mode));
        }
        if opened.meta().hash_key != key {
            return Err(Error::AccessDenied("backing database identity mismatch"));
        }
        opened.bind_write_epoch(operation.write_epoch.clone())?;
        Ok(opened)
    }

    fn cached_db(
        &self,
        id: DatabaseKey,
        path: &Path,
        create: Option<[u8; 32]>,
    ) -> Result<Arc<Database>> {
        loop {
            let mut open = match self.open.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    let _timing = crate::trace::Span::new("storage.cache_lock_wait");
                    self.open
                        .lock()
                        .map_err(|_| Error::Internal("open database cache lock is poisoned"))?
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Error::Internal("open database cache lock is poisoned"));
                }
            };
            if open.transitioning.contains(&id) {
                let _timing = crate::trace::Span::new("storage.cache_file_wait");
                while open.transitioning.contains(&id) {
                    open = self
                        .open_ready
                        .wait(open)
                        .map_err(|_| Error::Internal("open database cache lock is poisoned"))?;
                }
                continue;
            }
            open.uses += 1;
            let used = open.uses;
            if crate::trace::is_recording() {
                open.trace_uses.set(used);
            }
            if let Some(entry) = open.by_id.get_mut(&id) {
                entry.used = used;
                let cached = entry.database.clone();
                drop(open);
                // Neither filesystem inspection nor kvdb metadata locking
                // holds the shared cache gate. The Arc pins this exact backend.
                if path.exists() {
                    if create.is_some_and(|key| cached.meta().hash_key != key) {
                        return Err(Error::AccessDenied("database key mismatch"));
                    }
                    return Ok(cached);
                }
                let mut open = self
                    .open
                    .lock()
                    .map_err(|_| Error::Internal("open database cache lock is poisoned"))?;
                let Some(entry) = open.by_id.get(&id) else {
                    continue;
                };
                if !Arc::ptr_eq(&entry.database, &cached) {
                    continue;
                }
                if Arc::strong_count(&cached) != 2 {
                    return Err(Error::LocalContention(
                        "A removed database still has active readers",
                    ));
                }
                let retired = open.by_id.remove(&id).expect("the selected cache entry is present");
                assert!(open.transitioning.insert(id), "cache retirement owns its reservation");
                let reservation = DatabaseTransition { store: self, id };
                drop(open);
                drop(cached);
                {
                    let _timing = crate::trace::Span::new("storage.evict_replica");
                    drop(retired);
                }
                drop(reservation);
                continue;
            }
            let retire = if open.by_id.len() + open.transitioning.len() >= OPEN_DATABASE_LIMIT {
                Some(
                    open.by_id
                        .iter()
                        .filter(|(_, entry)| Arc::strong_count(&entry.database) == 1)
                        .min_by_key(|(_, entry)| entry.used)
                        .map(|(id, _)| *id)
                        .ok_or(Error::LocalContention(
                            "All local database cache slots are in use",
                        ))?,
                )
            } else {
                None
            };
            if let Some(retired_id) = retire {
                let retired = open
                    .by_id
                    .remove(&retired_id)
                    .expect("retirement selected an existing cache entry");
                assert!(open.transitioning.insert(retired_id), "eviction owns its reservation");
                let reservation = DatabaseTransition { store: self, id: retired_id };
                drop(open);
                {
                    let _timing = crate::trace::Span::new("storage.evict_replica");
                    drop(retired);
                }
                drop(reservation);
                continue;
            }
            assert!(open.transitioning.insert(id), "opening owns its reservation");
            assert!(
                open.by_id.len() + open.transitioning.len() <= OPEN_DATABASE_LIMIT,
                "resident and transitioning backends share one fixed cache budget",
            );
            let reservation = DatabaseTransition { store: self, id };
            drop(open);
            let opened = Arc::new(if let Some(key) = create {
                create_replica(path, key, OPEN_DATABASE_CACHE_BYTES)?
            } else {
                open_replica(path, OPEN_DATABASE_CACHE_BYTES)?
            });
            let mut open = self
                .open
                .lock()
                .map_err(|_| Error::Internal("open database cache lock is poisoned"))?;
            open.uses += 1;
            let used = open.uses;
            if crate::trace::is_recording() {
                open.trace_uses.set(used);
            }
            assert!(open.transitioning.remove(&id), "opening retains its reservation");
            let replaced = open.by_id.insert(id, OpenDatabase { database: opened.clone(), used });
            assert!(replaced.is_none(), "one opener publishes each cache entry");
            drop(open);
            drop(reservation);
            return Ok(opened);
        }
    }
    fn index(&self, volume: Id) -> Result<MetadataPolicy> {
        MetadataPolicy::read(&*self.db(volume, crate::store::DatabaseId::Index)?)
    }
    /// Recovery installs empty replica files before their policy arrives. A
    /// nonempty index still needs ordinary format and authorization validation.
    pub(crate) fn has_volume_index(&self, volume: Id) -> Result<bool> {
        Ok(replica_root(&*self.db(volume, crate::store::DatabaseId::Index)?)?.is_some())
    }
    fn save_index(&self, index: &MetadataPolicy) -> Result<()> {
        let delta = index.write_with_delta(
            &*self.db(index.volume.id, crate::store::DatabaseId::Index)?,
            now_unix_secs(),
            crate::store::MAX_REPLICA_DELTA_BYTES,
        )?;
        self.changes.publish(index.volume.id, crate::store::DatabaseId::Index, delta);
        self.notify_volume_invitation_policy(&index.volume);
        Ok(())
    }
    /// Resolve the local account’s retained invitation against the Shared binding.
    pub(crate) fn data_actor(&self, access: &Access, volume: &Volume) -> Result<DataActor> {
        let Some(id) = access.session_id().filter(|_| {
            access.account == self.account.id && !volume.personal && volume.share_key_with_service
        }) else {
            return Ok(access.account.into());
        };
        let session = self.session(id)?;
        DataActor::for_session(access.account, &session, volume)
    }

    /// Return the current personal-volume grant so callers can also enforce its write flag.
    fn admit_access(
        &self,
        access: &Access,
        index: &MetadataPolicy,
    ) -> Result<Option<PersonalVolumeGrant>> {
        if let Some(peer) = access.invitation_peer {
            self.admit_private_invitation_peer(access, index, peer)?;
        }
        self.admit_access_with_file_session(access, index, None)
    }
    /// Only retained File staging/control supplies a captured identity here.
    /// Its original build still selects Root provisioning; current policy and
    /// consent are checked below exactly as for ordinary access.
    fn admit_access_with_file_session(
        &self,
        access: &Access,
        index: &MetadataPolicy,
        retained: Option<&Session>,
    ) -> Result<Option<PersonalVolumeGrant>> {
        self.require_account_authority()?;
        self.check_participant(index.volume.id, access.account)?;
        self.require_current_direct_acl(access, index)?;
        let Some(id) = &access.session_id else {
            return Ok(None);
        };
        if self.session_denial(id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        let mut session = self.session(id)?;
        let local = self.has_automatic_scopes(id)?;
        if let Some(retained) = retained
            && (!local
                || access.account != self.account.id
                || access.build_id.as_deref() != Some(retained.resolved_build_id.as_str())
                || !session.same_identity(retained))
        {
            return Err(Error::AccessDenied("retained File session identity changed"));
        }
        if local
            && access.build_id.as_deref().is_some_and(|build| build != session.resolved_build_id)
            && retained.is_none()
        {
            return Err(Error::AccessDenied("local session build changed"));
        }
        if let Some(version) = &access.build_id {
            session.resolved_build_id = version.clone();
        }
        let world = session.is_distinct_world(index.volume.id);
        if world && index.volume.owner != self.account.id {
            self.check_world_admission(&session)?;
            index.admit_world(access.account, &session)?;
        }
        let grant = if session.replica_volume() != index.volume.id
            && !world
            && !(local
                && self.automatic_volumes(id)?.iter().any(|slot| slot.volume == index.volume.id))
        {
            Some(self.personal_grant(access, &session, index.volume.id)?)
        } else {
            None
        };
        let default_root = local
            && session.root_volume
                == session
                    .app
                    .volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)
                    .map_err(Error::InvalidInput)?;
        if default_root
            && (session.shared_volume.is_none() || !self.shared_session_initialized(id)?)
        {
            return Ok(grant);
        }
        let root = if session.replica_volume() == index.volume.id {
            index.clone()
        } else {
            self.index(session.replica_volume())?
        };
        root.admit(access.account, &session, session.replica_volume())?;
        if session.replica_volume() != index.volume.id {
            self.check_participant(session.replica_volume(), access.account)?;
        }
        Ok(grant)
    }
    fn check_participant(&self, volume: Id, account: Id) -> Result<()> {
        let revocations = self.db(volume, crate::store::DatabaseId::Revocations)?;
        if revocations.begin_read()?.has(account.to_string().as_bytes())? {
            return Err(Error::ParticipantRevoked { volume, account });
        }
        Ok(())
    }
    fn authorize(
        &self,
        access: &Access,
        index: &MetadataPolicy,
        permission: Permission,
    ) -> Result<()> {
        self.authorize_with_file_session(access, index, permission, None)
    }
    fn authorize_with_file_session(
        &self,
        access: &Access,
        index: &MetadataPolicy,
        permission: Permission,
        retained: Option<&Session>,
    ) -> Result<()> {
        if permission != Permission::ManagePermissions {
            self.check_authority(access, index.volume.id, None)?;
        }
        if let Some(grant) = self.admit_access_with_file_session(access, index, retained)?
            && permission != Permission::Read
            && (!grant.write || !matches!(permission, Permission::Write | Permission::Append))
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        index.authorize(self.data_actor(access, index.volume())?, permission)
    }
    /// Replace only this human subject's volume grant, preserving other grants
    /// and object overrides. Authorization and read/modify/write share one gate.
    pub fn grant_volume(
        &self,
        access: &Access,
        volume: Id,
        account: Id,
        write: bool,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.grant_volume");
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let mut index = self.index(volume)?;
        self.authorize(access, &index, Permission::ManagePermissions)?;
        if access.account != index.volume.owner {
            return Err(Error::AccessDenied("volume owner"));
        }
        let subject = crate::store::object_contract::Subject::Principal(
            crate::store::object_contract::Principal::Human(account),
        );
        let permissions =
            if write { vec![Permission::Read, Permission::Write] } else { vec![Permission::Read] };
        if let Some(grant) =
            index.permissions.grants.iter_mut().find(|grant| grant.subject == subject)
        {
            if grant.permissions == permissions {
                return Ok(());
            }
            grant.permissions = permissions;
        } else {
            index
                .permissions
                .grants
                .push(crate::store::object_contract::Grant { subject, permissions });
        }
        self.save_index(&index)
    }
    pub fn set_permissions(&self, access: &Access, volume: Id, acl: Acl) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let mut index = self.index(volume)?;
        self.authorize(access, &index, Permission::ManagePermissions)?;
        if access.account != index.volume.owner {
            return Err(Error::AccessDenied("volume owner"));
        }
        acl.validate()?;
        index.permissions = acl;
        self.save_index(&index)
    }
    pub fn provision_version(&self, access: &Access, volume: Id, version: &str) -> Result<()> {
        validate_version(version)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let mut index = self.index(volume)?;
        self.authorize(access, &index, Permission::ManagePermissions)?;
        if index.volume.owner != access.account {
            return Err(Error::AccessDenied("root owner"));
        }
        if index.provisioned_versions.insert(version.to_owned()) {
            self.save_index(&index)?;
        }
        Ok(())
    }
    pub fn revoke(&self, access: &Access, volume: Id, account: Id, message: &str) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.index(volume)?;
        self.authorize(access, &index, Permission::ManagePermissions)?;
        if access.account != index.volume.owner {
            return Err(Error::AccessDenied("revocation store owner"));
        }
        let db = self.db(volume, crate::store::DatabaseId::Revocations)?;
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        tx.put(account.to_string().as_bytes(), &encode(&message)?)?;
        let delta = {
            let _timing = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Revocations, delta);
        self.notify_volume_invitation_policy(&index.volume);
        if index.volume.personal && index.volume.owner == self.account.id {
            let subject = crate::store::object_contract::Subject::Principal(
                crate::store::object_contract::Principal::Human(account),
            );
            if index.permissions.grants.iter().any(|grant| grant.subject == subject) {
                let mut permissions = index.permissions;
                permissions.grants.retain(|grant| grant.subject != subject);
                self.restate_volume_acl(volume, &permissions)?;
            }
        }
        Ok(())
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn has_pending_blob_uploads(&self) -> Result<bool> {
        self.has_pending_entry_blob_uploads()
    }
    fn blob_path(&self, digest: &[u8; 32], recoverable: bool) -> PathBuf {
        let hash = hex(digest);
        if recoverable {
            self.blob_cache_path.join(&hash[..2]).join(hash)
        } else {
            self.path.join(crate::paths::BLOBS).join(hash)
        }
    }
    /// Evict recoverable plaintext. Pending shared content and unknown files
    /// from interrupted preparation remain durable.
    pub fn evict_blobs(&self, budget: u64) -> Result<u64> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        self.evict_blobs_locked(budget, &mut cache)
    }
    pub fn cache_budget(&self) -> Result<u64> {
        Ok(get(&self.metadata, b"settings/blob-cache-budget")?.unwrap_or(DEFAULT_BLOB_CACHE_BUDGET))
    }
    /// Persist the local cache budget and immediately reclaim eligible bytes.
    /// Pending local references remain pinned even when they exceed the budget.
    pub fn set_cache_budget(&self, bytes: u64) -> Result<u64> {
        {
            let _profile = self.lock_profile()?;
            put(&self.metadata, b"settings/blob-cache-budget", &bytes)?;
        }
        // Eviction holds no profile writer; use a newer budget if one superseded ours.
        self.enforce_cache_budget()
    }
    pub fn enforce_cache_budget(&self) -> Result<u64> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        self.evict_blobs_locked(self.cache_budget()?, &mut cache)
    }
    #[cfg(any(
        feature = "connected-engine",
        feature = "native-runtime",
        feature = "wasm-runtime",
        test
    ))]
    pub(crate) fn blob_eviction_generation(&self) -> u64 {
        self.blob_eviction_generation.load(std::sync::atomic::Ordering::Acquire)
    }
    fn evict_blobs_locked(&self, budget: u64, cache: &mut blob_files::BlobCache) -> Result<u64> {
        // A failed pass must preserve previously deferred pins. Only a complete
        // below-budget result can prove that their cleanup is no longer needed.
        loop {
            let (mut total, candidates) = self.blob_eviction_candidates(cache)?;
            if total <= budget {
                cache.clear_deferred();
                return Ok(total);
            }
            if candidates.is_empty() {
                return Ok(total);
            }
            for (_, path, size) in candidates.into_sorted_vec() {
                if total <= budget {
                    cache.clear_deferred();
                    return Ok(total);
                }
                self.blob_eviction_generation
                    .try_update(
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                        |generation| generation.checked_add(1),
                    )
                    .map_err(|_| Error::LimitExceeded)?;
                #[cfg(any(
                    feature = "connected-engine",
                    feature = "native-runtime",
                    feature = "wasm-runtime"
                ))]
                cache.expire_verified(
                    self.blob_eviction_generation.load(std::sync::atomic::Ordering::Acquire),
                );
                remove_blob_file(&path)?;
                total -= size;
            }
        }
    }

    /// One filesystem scan holds at most 257 directory paths and 64 candidate
    /// files. Pending entry-version counts require one exact lookup per digest.
    /// This background operation holds no volume gate.
    fn blob_eviction_candidates(
        &self,
        cache: &mut blob_files::BlobCache,
    ) -> Result<(u64, BlobEvictionCandidates)> {
        let mut directories = vec![(self.path.join(crate::paths::BLOBS), false)];
        if let Some(buckets) = blob_directory(&self.blob_cache_path)? {
            for bucket in buckets {
                let bucket = bucket.map_err(io)?;
                let name = bucket.file_name();
                let name = name.to_str().ok_or(Error::corrupt("blob bucket name is not UTF-8"))?;
                if !lower_hex_name(name, 2) || !bucket.file_type().map_err(io)?.is_dir() {
                    return Err(Error::corrupt(
                        "blob bucket must be a directory named by two lowercase hexadecimal digits",
                    ));
                }
                if directories.len() == 257 {
                    return Err(Error::corrupt("blob directory contains too many buckets"));
                }
                directories.push((bucket.path(), true));
            }
        }
        let mut candidates = BlobEvictionCandidates::new();
        let mut total = 0u64;
        for (directory, recoverable) in directories {
            let Some(files) = blob_directory(&directory)? else { continue };
            for entry in files {
                let entry = entry.map_err(io)?;
                let name = entry.file_name();
                let name = name.to_str().ok_or(Error::corrupt("blob file name is not UTF-8"))?;
                if !lower_hex_name(name, 64) || !entry.file_type().map_err(io)?.is_file() {
                    return Err(Error::corrupt(
                        "blob must be a file named by 64 lowercase hexadecimal digits",
                    ));
                }
                let metadata = entry.metadata().map_err(io)?;
                let size = metadata.len();
                total = total.checked_add(size).ok_or(Error::LimitExceeded)?;
                let eligible = recoverable || !self.has_entry_blob_pin_text(name)?;
                if !cache.defer_pinned(name, eligible) && eligible {
                    candidates.push((metadata.modified().map_err(io)?, entry.path(), size));
                    if candidates.len() > 64 {
                        candidates.pop();
                    }
                }
            }
        }
        Ok((total, candidates))
    }
}
type BlobEvictionCandidates = std::collections::BinaryHeap<(std::time::SystemTime, PathBuf, u64)>;

fn blob_directory(path: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(error)),
    }
}

fn lower_hex_name(name: &str, length: usize) -> bool {
    name.len() == length && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn remove_blob_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error)),
    }
}

/// Open an existing replica within the caller's explicit page-cache allocation.
pub fn open_replica(path: &Path, page_cache_bytes: usize) -> Result<Database> {
    let _timing = crate::trace::Span::new("storage.open_replica");
    let result = (|| {
        let redb = crate::store::file_backend::open(path, page_cache_bytes)
            .map_err(database_open_error)?;
        Ok(Db::open(
            RedbStorage::from_database(redb),
            &kvdb::OpenOptions { verify_on_open: cfg!(debug_assertions) },
        )?)
    })();
    result.map_err(|error: Error| {
        error.local_context(format!("open existing local database {}", path.display()))
    })
}

fn database_open_error(error: redb::DatabaseError) -> Error {
    match error {
        redb::DatabaseError::Storage(redb::StorageError::Corrupted(reason)) => {
            Error::corrupt(format!("database backend: {reason}"))
        }
        redb::DatabaseError::UpgradeRequired(version) => Error::Store(kvdb::Error::Version)
            .local_context(format!("database backend format version {version}")),
        redb::DatabaseError::Storage(redb::StorageError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Error::corrupt(format!("database file is invalid or truncated: {error}"))
        }
        redb::DatabaseError::Storage(redb::StorageError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Error::LocalContention("Local database synchronization capacity is in use")
        }
        error => Error::System(error.to_string()),
    }
}
/// Used only when creating a volume/profile or materializing its first application data.
/// An interrupted first write can leave an initialized database; reopen it without reset.
fn create_db(path: &Path, key: Option<[u8; 32]>, page_cache_bytes: usize) -> Result<Database> {
    if path.try_exists().map_err(io)? {
        let db = open_replica(path, page_cache_bytes)?;
        if key.is_some_and(|key| db.meta().hash_key != key) {
            return Err(Error::AccessDenied("database key mismatch"));
        }
        return Ok(db);
    }
    // Publish only a fully initialized database. An interrupted first creation
    // must not leave a final filename containing redb without kvdb metadata.
    let directory = path.parent().ok_or(Error::InvalidInput("database has no parent directory"))?;
    let temporary = directory.join(format!(".replica-{}.tmp", Id::generate()?));
    let result = (|| {
        let redb = crate::store::file_backend::create(&temporary, page_cache_bytes)
            .map_err(|e| Error::System(e.to_string()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(io)?;
        let options = match key {
            Some(key) => CreateOptions::syncable(key, u64::from_le_bytes(random_array()?)),
            None => CreateOptions::plain(),
        };
        drop(Db::create(RedbStorage::from_database(redb), &options)?);
        let file = fs::File::open(&temporary).map_err(io)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(|e| io(e.into()))?;
        // A hard link publishes atomically without ever replacing a concurrent
        // creator's final file. Both names refer to the same initialized inode.
        match fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io(error)),
        }
        fs::remove_file(&temporary).map_err(io)?;
        let directory_file = fs::File::open(directory).map_err(io)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory_file))
            .map_err(|e| io(e.into()))?;
        let database = open_replica(path, page_cache_bytes)?;
        if key.is_some_and(|key| database.meta().hash_key != key) {
            return Err(Error::AccessDenied("database key mismatch"));
        }
        Ok(database)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn get<T: DeserializeOwned>(db: &Database, key: &[u8]) -> Result<Option<T>> {
    db.begin_read()?.get(key)?.map(|value| decode(&value)).transpose()
}
fn required<T: DeserializeOwned>(db: &Database, key: impl AsRef<[u8]>) -> Result<T> {
    get(db, key.as_ref())?.ok_or(Error::NotFound("local record"))
}
fn put<T: Serialize + ?Sized>(db: &Database, key: &[u8], value: &T) -> Result<()> {
    let _timing = crate::trace::Span::new("storage.put_metadata");
    let mut tx = db.begin_write(now_unix_secs())?;
    tx.put(key, &encode(value)?)?;
    {
        let _timing = crate::trace::Span::new("storage.commit");
        tx.commit()?;
    }
    Ok(())
}
fn scan<T: DeserializeOwned>(db: &Database, prefix: &str) -> Result<Vec<T>> {
    Ok(scan_with_keys(db, prefix)?.into_iter().map(|(_, value)| value).collect())
}
#[cfg(test)]
fn put_source<T: record::Codec>(db: &Database, key: &[u8], value: &T) -> Result<()> {
    let mut tx = db.begin_write(now_unix_secs())?;
    tx.put(key, &record::encode(value)?)?;
    tx.commit()?;
    Ok(())
}
/// Values with the key text after `prefix`.
fn scan_with_keys<T: DeserializeOwned>(db: &Database, prefix: &str) -> Result<Vec<(String, T)>> {
    let tx = db.begin_read()?;
    let mut result = Vec::new();
    for entry in tx.iter_from(prefix.as_bytes()) {
        let (key, value) = entry?;
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        let suffix = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| Error::corrupt("catalog record key is not UTF-8"))?;
        result.push((suffix.to_owned(), decode(&value)?));
    }
    Ok(result)
}
/// Generate a matching device key pair using the host random source.
fn generate_account() -> Result<(Account, [u8; 32])> {
    let secret = random_array::<32>()?;
    let key = Ed25519KeyPair::from_seed_unchecked(&secret)
        .map_err(|_| Error::Internal("could not construct the account signing key"))?;
    let account = Account {
        id: Id::from_bytes(
            blake3::hash(key.public_key().as_ref()).as_bytes()[..16]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ),
        public_key: hex(key.public_key().as_ref()),
    };
    Ok((account, secret))
}

fn io(error: std::io::Error) -> Error {
    Error::System(error.to_string())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    mod blob_storage;
    mod concurrency;
    mod entry_permissions;
    mod live;
    mod profile_lock;
    pub(super) mod received;
    mod session_list;
    use crate::store::object_contract::{
        FieldCell, FieldDeclaration, Grant, Principal, Subject, TypeDeclaration, ValueType,
    };
    fn encrypted_bytes(
        store: &LocalStore,
        access: &Access,
        version: &crate::store::volume_index::BlobVersion,
    ) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut file = store.encrypt_blob_version(access, version)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        Ok(bytes)
    }
    fn sealed_bytes(
        key: &[u8; 32],
        volume: Id,
        digest: &[u8; 32],
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        crate::store::seal_blob_to(
            key,
            volume,
            digest,
            plaintext.len() as u64,
            &mut &plaintext[..],
            &mut bytes,
        )?;
        Ok(bytes)
    }
    fn pull(source: &Database, target: &Database) {
        merge_replica(&std::env::temp_dir(), source, target, kvdb::sync::Lww, |_| Ok(())).unwrap();
    }
    fn sync_volume(source: &LocalStore, target: &LocalStore, volume: Id) {
        let mut after = None;
        loop {
            let page = source.replication_database_page(volume, after).unwrap();
            for identity in page.databases {
                let key = target.credentials(volume).unwrap().database_key;
                let destination = target.replica_db(volume, identity, Some(key)).unwrap();
                source
                    .with_replica(volume, identity, |database| {
                        if identity == crate::store::DatabaseId::Index {
                            merge_index_replica(
                                &std::env::temp_dir(),
                                database,
                                &destination,
                                kvdb::Lww,
                                |_| Ok(()),
                            )?;
                            // This fixture publishes a trusted complete source snapshot.
                            let mut tx = destination.begin_write(now_unix_secs())?;
                            crate::store::volume_index::namespace::source::cut::project(&tx)?
                                .write_current(&mut tx.local_indexes())?;
                            tx.commit()?;
                        } else {
                            merge_replica(
                                &std::env::temp_dir(),
                                database,
                                &destination,
                                kvdb::Lww,
                                |_| Ok(()),
                            )?;
                        }
                        Ok(())
                    })
                    .unwrap();
                if let crate::store::DatabaseId::Entry(entry) = identity {
                    let index = target.db(volume, crate::store::DatabaseId::Index).unwrap();
                    let mut tx = index.begin_write(now_unix_secs()).unwrap();
                    crate::store::volume_index::mark_backing_ready(&mut tx, entry).unwrap();
                    tx.commit().unwrap();
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
    }
    #[test]
    fn profile_lock_releases_after_close_even_with_a_duplicated_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account().id;
        // A child between fork and exec retains this same open-file description.
        let inherited = store._lock.0.try_clone().unwrap();
        assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
        drop(store);
        let reopened = LocalStore::open(directory.path()).unwrap();
        assert_eq!(reopened.account().id, account);
        drop(inherited);
        assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
        drop(reopened);
        assert_eq!(LocalStore::open(directory.path()).unwrap().account().id, account);
    }
    #[test]
    fn volume_creation_enforces_label_bounds_before_persisting() {
        // Creation shares relabel's UTF-8 byte limit and rejects without writing a volume.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let pending = store.pending_operations().unwrap();
        for name in [String::new(), " \n".to_owned(), "a".repeat(257), "é".repeat(129)] {
            assert!(matches!(store.create_volume(&name, false), Err(Error::InvalidInput(_))));
            assert!(store.volumes().unwrap().is_empty());
            assert_eq!(store.pending_operations().unwrap(), pending);
            let id = Id::generate().unwrap();
            assert!(matches!(
                store.new_volume(id, &name, false, true),
                Err(Error::InvalidInput(_))
            ));
            assert!(!directory.path().join(crate::paths::VOLUMES).join(id.to_string()).exists());
        }
        let names = ["a".repeat(256), "é".repeat(128)];
        let volumes =
            names.iter().map(|name| store.create_volume(name, false).unwrap()).collect::<Vec<_>>();
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        for (volume, name) in volumes.iter().zip(names) {
            assert_eq!(store.volume(volume.id).unwrap().name, name);
        }
    }
    #[test]
    fn account_volume_grants_preserve_other_subjects_and_enforce_read_only() {
        let (_directory, store, root, owner) = setup();
        let bob = Id::generate().unwrap();
        let anne = Id::generate().unwrap();
        store.set_permissions(&owner, root.id, Acl::default()).unwrap();
        store.grant_volume(&owner, root.id, bob, true).unwrap();
        store.grant_volume(&owner, root.id, anne, false).unwrap();
        store.grant_volume(&owner, root.id, bob, false).unwrap();
        store.grant_volume(&owner, root.id, bob, false).unwrap();
        let policy = store.index(root.id).unwrap();
        assert_eq!(policy.permissions.grants.len(), 2);
        for account in [bob, anne] {
            assert!(policy.permissions.permits(Some(Principal::Human(account)), Permission::Read));
            assert!(
                !policy.permissions.permits(Some(Principal::Human(account)), Permission::Write)
            );
        }
        assert!(store.schema(&Access::owner(bob), data(&store, root.id)).is_ok());
        assert!(
            store
                .establish_schema(&Access::owner(bob), data(&store, root.id), &Schema::default())
                .is_err()
        );
        assert!(store.grant_volume(&Access::owner(bob), root.id, anne, true).is_err());
        assert_eq!(store.index(root.id).unwrap().permissions, policy.permissions);
        std::thread::scope(|scope| {
            for account in [bob, anne] {
                let store = &store;
                let owner = &owner;
                scope.spawn(move || store.grant_volume(owner, root.id, account, true).unwrap());
            }
        });
        let policy = store.index(root.id).unwrap();
        for account in [bob, anne] {
            assert!(policy.permissions.permits(Some(Principal::Human(account)), Permission::Write));
        }
    }

    #[test]
    #[ignore = "explicit optimized volume-list diagnostic; includes database opening and cache churn"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "Keep executable checks of the compile-time capacity contract in this regression test."
    )]
    fn benchmark_volume_name_listing() {
        assert!(!cfg!(debug_assertions), "use an optimized release build");
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let samples: usize =
            if std::env::var_os("PLAYBIT_QUALIFY_PERF").is_some() { 1000 } else { 8 };
        let mut created = 0;
        for count in [1, 32, 256] {
            while created < count {
                store.create_volume(&format!("{created:032}"), false).unwrap();
                created += 1;
            }
            for authoritative in [false, true] {
                let mut elapsed = Vec::with_capacity(samples);
                for _ in 0..samples {
                    let start = std::time::Instant::now();
                    let volumes = if authoritative {
                        store.volumes().unwrap()
                    } else {
                        scan::<Volume>(&store.metadata, "volume/").unwrap()
                    };
                    elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
                    assert_eq!(volumes.len(), count);
                }
                let first = elapsed[0];
                elapsed.sort_by(f64::total_cmp);
                let p99 = elapsed[(samples * 99).div_ceil(100) - 1];
                crate::diagnostic!(
                    "volume listing: authoritative={authoritative} volumes={count} n={samples} first={first:.6} p50={:.6} p99={p99:.6} max={:.6} ms",
                    elapsed[(samples * 50).div_ceil(100) - 1],
                    elapsed[samples - 1]
                );
                if samples == 1000 {
                    assert!(p99 < 2.0, "volume listing exceeds2 ms owner-thread allocation");
                }
            }
        }
    }

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "Keep executable checks of the compile-time capacity contract in this regression test."
    )]
    fn volume_label_transaction_workload() {
        let (_directory, store, root, owner) = setup();
        let qualify = std::env::var_os("PLAYBIT_QUALIFY_PERF").is_some();
        if qualify {
            assert!(!cfg!(debug_assertions), "qualification requires an optimized release build");
        }
        let samples: usize = if qualify { 1000 } else { 8 };
        for n in 0..32 {
            store
                .create_object_with_fields(
                    &owner,
                    data(&store, root.id),
                    id(1),
                    &title(&format!("Task {n}")),
                )
                .unwrap();
        }
        for count in [1, 32, 256] {
            store
                .set_permissions(
                    &owner,
                    root.id,
                    Acl {
                        grants: (0..count)
                            .map(|_| Grant {
                                subject: Subject::Principal(Principal::Human(
                                    Id::generate().unwrap(),
                                )),
                                permissions: vec![Permission::Read],
                            })
                            .collect(),
                    },
                )
                .unwrap();
            for bytes in [32, 256] {
                let mut elapsed = Vec::with_capacity(samples);
                for n in 0..samples {
                    let label = format!("{n:08}{}", "é".repeat((bytes - 8) / 2));
                    assert_eq!(label.len(), bytes);
                    let start = qualify.then(std::time::Instant::now);
                    let volume = store.label_volume(&owner, root.id, &label).unwrap();
                    if let Some(start) = start {
                        elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
                    }
                    assert_eq!(volume.name, label);
                }
                if qualify {
                    elapsed.sort_by(f64::total_cmp);
                    let p99 = elapsed[(samples * 99).div_ceil(100) - 1];
                    crate::diagnostic!(
                        "label transaction: grants={count} rows=32 label_bytes={bytes} n={samples} p50={:.6} p99={p99:.6} max={:.6} ms metadata_bytes={}",
                        elapsed[(samples * 50).div_ceil(100) - 1],
                        elapsed[samples - 1],
                        encode(&store.index(root.id).unwrap()).unwrap().len()
                    );
                    assert!(p99 < 2.0, "label transaction exceeds2 ms p99");
                }
            }
        }
    }

    #[test]
    fn volume_grant_transaction_workload() {
        let (_directory, store, root, owner) = setup();
        let qualify = std::env::var_os("PLAYBIT_QUALIFY_PERF").is_some();
        let samples: usize = if qualify { 1000 } else { 8 };
        for count in [1, 32, 256] {
            let accounts: Vec<_> = (0..count).map(|_| Id::generate().unwrap()).collect();
            store
                .set_permissions(
                    &owner,
                    root.id,
                    Acl {
                        grants: accounts
                            .iter()
                            .map(|id| Grant {
                                subject: Subject::Principal(Principal::Human(*id)),
                                permissions: vec![Permission::Read],
                            })
                            .collect(),
                    },
                )
                .unwrap();
            let mut elapsed = Vec::with_capacity(samples);
            for n in 0..samples {
                let start = qualify.then(std::time::Instant::now);
                store.grant_volume(&owner, root.id, accounts[0], n % 2 == 0).unwrap();
                if let Some(start) = start {
                    elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            if qualify {
                elapsed.sort_by(f64::total_cmp);
                let p99 = elapsed[(samples * 99).div_ceil(100) - 1];
                crate::diagnostic!(
                    "grant transaction: grants={count} n={samples} p50={:.6} p99={p99:.6} max={:.6} ms metadata_bytes={}",
                    elapsed[(samples * 50).div_ceil(100) - 1],
                    elapsed[samples - 1],
                    encode(&store.index(root.id).unwrap()).unwrap().len()
                );
                assert!(p99 < 2.0, "grant transaction exceeds2 ms p99");
            }
        }
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn desktop_invitation_becomes_durable_only_after_the_app_receives_its_details() {
        // Compare a process restart, a surviving helper with an unseen envelope,
        // and a live app that receives the registered session.
        let (_alice_dir, alice, root, _) = setup();
        let channel = crate::release_channel::ReleaseChannel::Public;
        let build = crate::identifier::encode(&[0xcd; 32]);
        let session = alice
            .create_session(
                &crate::package::AppIdentity {
                    publisher: alice.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        let link = alice.invitation(&session.id, channel).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bob = LocalStore::open(dir.path()).unwrap();
        let preview = bob.preview_desktop_invitation(&link, channel).unwrap();
        assert!(!preview.retained && preview.received.is_none());
        bob.pause_invitation(&session.id, true).unwrap();
        assert!(bob.pending_invitation_paused(&session.id).unwrap());
        assert!(bob.pending_invitations().unwrap().is_empty());
        drop(bob);
        let bob = LocalStore::open(dir.path()).unwrap();
        assert!(bob.pending_invitation_preview(&session.id).is_err());
        bob.preview_desktop_invitation(&link, channel).unwrap();
        let envelope = alice.session_envelope(&session.id).unwrap();
        bob.check_invitation_registration(&session.id, &envelope, &session).unwrap();
        let received = bob.receive_invitation(&session.id, &envelope).unwrap();
        assert_eq!(received.received.as_ref().unwrap().session, session);
        assert!(!received.retained);
        assert_eq!(bob.resolved_desktop_invitation(&session.id).unwrap().as_ref(), Some(&received));
        assert!(bob.desktop_launch_invitations(|_| false).unwrap().is_empty());
        assert!(bob.pending_invitation_preview(&session.id).is_err());

        bob.preview_desktop_invitation(&link, channel).unwrap();
        bob.receive_invitation(&session.id, &envelope).unwrap();
        bob.pause_invitation(&session.id, true).unwrap();
        bob.retain_desktop_invitation(&session.id).unwrap();
        assert!(bob.resolved_desktop_invitation(&session.id).unwrap().is_none());
        assert!(bob.pending_invitations().unwrap().is_empty());
        assert_eq!(bob.preview_desktop_invitation(&link, channel).unwrap(), received);
        bob.retain_desktop_invitation(&crate::identifier::encode(&[5; 32])).unwrap();
        assert!(bob.pending_invitations().unwrap().is_empty());
        bob.retain_desktop_invitation(&session.id).unwrap();
        bob.retain_desktop_invitation(&session.id).unwrap();
        drop(bob);
        let bob = LocalStore::open(dir.path()).unwrap();
        assert_eq!(bob.desktop_launch_invitations(|_| false).unwrap(), [received]);
        assert!(bob.sessions().unwrap().is_empty());

        // The loaded invitation alone creates a sidebar app. Removal also
        // supersedes a second resolved link awaiting its UI acknowledgment.
        let delayed =
            alice.create_session(&session.app, session.selector.clone(), &build, root.id).unwrap();
        bob.preview_desktop_invitation(&alice.invitation(&delayed.id, channel).unwrap(), channel)
            .unwrap();
        bob.receive_invitation(&delayed.id, &alice.session_envelope(&delayed.id).unwrap()).unwrap();
        assert_eq!(bob.app_removal_impact(&session.app).unwrap(), AppRemovalImpact::default());
        assert_eq!(
            bob.remove_app(&session.app, false, false).unwrap(),
            AppRemovalResult::Removed { erased_volumes: vec![], cleanup_error: None }
        );
        assert!(bob.resolved_desktop_invitation(&delayed.id).unwrap().is_none());
        bob.retain_desktop_invitation(&delayed.id).unwrap();
        assert!(bob.pending_invitations().unwrap().is_empty());
        // A freshly opened link can again be kept or dismissed without
        // restoring the app's previously joined sessions.
        bob.preview_desktop_invitation(&link, channel).unwrap();
        bob.receive_invitation(&session.id, &envelope).unwrap();
        bob.retain_desktop_invitation(&session.id).unwrap();
        assert!(bob.app_removed(&session.app).unwrap());
        assert_eq!(bob.app_removal_impact(&session.app).unwrap(), AppRemovalImpact::default());
        assert_eq!(
            bob.remove_app(&session.app, false, false).unwrap(),
            AppRemovalResult::Removed { erased_volumes: vec![], cleanup_error: None }
        );
        assert!(bob.desktop_launch_invitations(|_| false).unwrap().is_empty());

        // An explicit join also owns its loaded session if the management
        // status acknowledgment has not been delivered yet.
        let other_dir = tempfile::tempdir().unwrap();
        let other = LocalStore::open(other_dir.path()).unwrap();
        other.preview_desktop_invitation(&link, channel).unwrap();
        other.receive_invitation(&session.id, &envelope).unwrap();
        assert_eq!(other.import_invitation(&session.id).unwrap(), session);
        assert!(other.transient_desktop_session().unwrap().is_none());
        assert!(other.pending_invitation_preview(&session.id).unwrap().retained);
    }

    #[test]
    fn invitation_preview_survives_pause_restart_and_uri_handoff_without_attaching_data() {
        let (_alice_dir, alice, root, _) = setup();
        let session = alice
            .create_session(
                &crate::package::AppIdentity {
                    publisher: alice.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: crate::identifier::encode(&[0xcd; 32]) },
                &crate::identifier::encode(&[0xcd; 32]),
                root.id,
            )
            .unwrap();
        let url =
            alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
        let seed = alice.session_seed(&session.id).unwrap();
        assert_eq!(
            url,
            crate::store::session_link(&seed, crate::release_channel::ReleaseChannel::Public)
        );
        assert_eq!(crate::store::session_id(&seed).unwrap(), session.id);
        let uri = crate::store::session_uri(&seed, crate::release_channel::ReleaseChannel::Public);
        let envelope = alice.session_envelope(&session.id).unwrap();
        assert!(envelope.len() < 2048);
        let bob_dir = tempfile::tempdir().unwrap();
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        let account = bob.account().clone();
        // The selected authority is checked before retaining any capability,
        // even when the same seed also names a locally retained session.
        let testing_uri =
            crate::store::session_uri(&seed, crate::release_channel::ReleaseChannel::Testing);
        assert!(
            bob.preview_invitation(&testing_uri, crate::release_channel::ReleaseChannel::Public)
                .is_err()
        );
        assert!(bob.pending_invitations().unwrap().is_empty());
        assert!(
            alice
                .preview_invitation(&testing_uri, crate::release_channel::ReleaseChannel::Public)
                .is_err()
        );
        // Only the seed is known before the creator's registration is read.
        let preview =
            bob.preview_invitation(&uri, crate::release_channel::ReleaseChannel::Public).unwrap();
        assert_eq!(
            preview,
            InvitationPreview {
                session: session.id.clone(),
                paused: false,
                retained: false,
                received: None
            }
        );
        // The creator's own link is answered from the retained session, without a read.
        let own =
            alice.preview_invitation(&uri, crate::release_channel::ReleaseChannel::Public).unwrap();
        assert!(
            own.retained
                && own.received.as_ref().is_some_and(|received| received.session == session),
            "{own:?}"
        );
        assert_eq!(alice.pending_invitations().unwrap(), [own]);
        alice.complete_invitation(&session.id).unwrap();
        assert!(bob.import_invitation(&session.id).is_err());
        assert_eq!(
            bob.pending_invitation_proof(&session.id, account.id).unwrap(),
            alice.session_proof(&session.id, account.id).unwrap()
        );
        assert!(bob.sessions().unwrap().is_empty());
        assert!(bob.volumes().unwrap().is_empty());
        assert!(bob.pending_operations().unwrap().is_empty());
        let mut tampered = envelope.clone();
        tampered[20] ^= 1;
        assert!(bob.receive_invitation(&session.id, &tampered).is_err());
        // The link was opened before an account removal. Learning its app
        // identity later cannot restore it; explicitly opening the link again can.
        let mut tx = bob.metadata.begin_write(now_unix_secs()).unwrap();
        bob.stage_app_removed(&mut tx, &session.app, true).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            bob.receive_invitation(&session.id, &envelope),
            Err(Error::AccessDenied(_))
        ));
        assert!(bob.pending_invitations().unwrap().is_empty());
        bob.preview_invitation(&uri, crate::release_channel::ReleaseChannel::Public).unwrap();
        let received = bob.receive_invitation(&session.id, &envelope).unwrap();
        assert_eq!(
            received.received,
            Some(ReceivedInvitation { session: session.clone(), root_name: root.name.clone() })
        );
        let shown = serde_json::to_string(&received).unwrap();
        assert!(!shown.contains("seed") && !shown.contains("credentials"));
        bob.pause_invitation(&session.id, true).unwrap();
        drop(bob);
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        assert_eq!(bob.account(), &account);
        assert_eq!(
            bob.pending_invitations().unwrap(),
            [InvitationPreview {
                session: session.id.clone(),
                paused: true,
                retained: false,
                received: received.received.clone(),
            }]
        );
        // Previewing again keeps the received envelope and resumes.
        assert!(
            !bob.preview_invitation(&url, crate::release_channel::ReleaseChannel::Public)
                .unwrap()
                .paused
        );
        assert_eq!(bob.pending_invitations().unwrap()[0].received, received.received);
        assert_eq!(bob.import_invitation(&session.id).unwrap(), session);
        assert_eq!(bob.import_invitation(&session.id).unwrap(), session);
        assert!(bob.pending_invitation_preview(&session.id).unwrap().retained);
        assert!(
            bob.preview_invitation(&url, crate::release_channel::ReleaseChannel::Public)
                .unwrap()
                .retained
        );
        assert_eq!(
            bob.pending_invitation_proof(&session.id, account.id).unwrap(),
            bob.session_proof(&session.id, account.id).unwrap()
        );
        bob.complete_invitation(&session.id).unwrap();
        assert!(bob.pending_invitations().unwrap().is_empty());
        assert!(!bob.app_removed(&session.app).unwrap());
        assert_eq!(
            bob.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
            url
        );
        for invalid in [
            url.replacen("playbit.app", "example.org", 1),
            "x".repeat(300),
            format!("session:{}", &url[..10]),
        ] {
            assert!(
                bob.preview_invitation(&invalid, crate::release_channel::ReleaseChannel::Public)
                    .is_err()
            );
        }
    }
    #[test]
    fn authenticated_account_binding_preserves_device_keys_and_refuses_to_reassign_local_data() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(directory.path()).unwrap();
        let device_key = store.account().public_key.clone();
        let seed = store.account_seed().unwrap();
        store.bind_authenticated_account(id(42)).unwrap();
        assert_eq!(store.account().id, id(42));
        assert_eq!(store.account().public_key, device_key);
        assert_eq!(store.account_seed().unwrap(), seed);
        let volume = store.create_volume("Personal", true).unwrap();
        assert!(store.bind_authenticated_account(id(43)).is_err());
        assert_eq!(store.volume(volume.id).unwrap().owner, id(42));
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.account().id, id(42));
        assert_eq!(store.account_seed().unwrap(), seed);
    }
    #[test]
    fn invitation_bootstraps_empty_databases_and_preserves_identity_on_retry_and_restart() {
        let (alice_dir, alice, root, access) = setup();
        let version = crate::identifier::encode(&[0xcd; 32]);
        alice.provision_version(&access, root.id, &version).unwrap();
        let session = alice
            .create_session(
                &crate::package::AppIdentity {
                    publisher: alice.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let invitation =
            alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
        let envelope = alice.session_envelope(&session.id).unwrap();
        let bob_dir = tempfile::tempdir().unwrap();
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        let identity = bob.account().clone();
        let seed = bob.account_seed().unwrap();
        assert_eq!(
            bob.import_invitation_link(
                &invitation,
                &envelope,
                crate::release_channel::ReleaseChannel::Public
            )
            .unwrap(),
            session
        );
        assert_eq!(
            bob.import_invitation_link(
                &invitation,
                &envelope,
                crate::release_channel::ReleaseChannel::Public
            )
            .unwrap(),
            session
        );
        assert!(bob.pending_operations().unwrap().is_empty());
        assert_eq!(
            bob.replication_database_page(root.id, None).unwrap().databases,
            [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations]
        );
        let fragment = bob.stored_session(&session.id).unwrap().invitation;
        assert_eq!(fragment, alice.stored_session(&session.id).unwrap().invitation);
        let public = ServiceProvisioning {
            volume: root.clone(),
            database_key: fragment.replica_credentials().unwrap().database_key,
        };
        assert!(!String::from_utf8(encode(&public).unwrap()).unwrap().contains("blob"));
        // An envelope sealed for another seed never adopts under this link.
        let mut tampered = fragment.clone();
        tampered.seed = [0; SEED_BYTES];
        tampered.session.id = crate::store::session_id(&tampered.seed).unwrap();
        let foreign = tampered.seal().unwrap();
        assert!(
            bob.import_invitation_link(
                &invitation,
                &foreign,
                crate::release_channel::ReleaseChannel::Public
            )
            .is_err()
        );
        assert!(
            bob.preview_invitation(
                &crate::store::session_link(
                    &tampered.seed,
                    crate::release_channel::ReleaseChannel::Public
                ),
                crate::release_channel::ReleaseChannel::Public
            )
            .is_ok()
        );
        assert!(bob.receive_invitation(&tampered.session.id, &envelope).is_err());
        sync_volume(&alice, &bob, root.id);
        sync_volume(&alice, &bob, session.root_volume);
        let proof = bob.session_proof(&session.id, identity.id).unwrap();
        let mut message = b"app.playbit.session.join.v1\0".to_vec();
        message.extend_from_slice(identity.id.as_bytes());
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            crate::identifier::decode::<32>(&session.id).unwrap(),
        )
        .verify(&message, &proof)
        .unwrap();
        drop(alice);
        drop(alice_dir);
        drop(bob);
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        assert_eq!(bob.account(), &identity);
        assert_eq!(bob.account_seed().unwrap(), seed);
        assert_eq!(
            bob.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
            invitation
        );
        assert_eq!(bob.sessions().unwrap(), *std::slice::from_ref(&session));
        assert_eq!(
            bob.replication_database_page(session.root_volume, None).unwrap().databases,
            [
                crate::store::DatabaseId::Index,
                crate::store::DatabaseId::Revocations,
                crate::store::DatabaseId::Entry(data(&bob, session.root_volume).entry)
            ]
        );
        assert!(bob.pending_operations().unwrap().is_empty());
    }
    #[test]
    fn independent_replica_creations_merge_catalog_and_fields_and_ack_tracks_exact_root() {
        let (_alice_dir, alice, root, access) = setup();
        let version = crate::identifier::encode(&[0xef; 32]);
        alice.provision_version(&access, root.id, &version).unwrap();
        alice
            .set_permissions(
                &access,
                root.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                },
            )
            .unwrap();
        let session = alice
            .create_session(
                &crate::package::AppIdentity {
                    publisher: alice.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let bob_dir = tempfile::tempdir().unwrap();
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        bob.import_invitation_link(
            &alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
            &alice.session_envelope(&session.id).unwrap(),
            crate::release_channel::ReleaseChannel::Public,
        )
        .unwrap();
        sync_volume(&alice, &bob, root.id);
        sync_volume(&alice, &bob, session.root_volume);
        let bob_access = Access::session(bob.account.id, &session.id);
        let a = alice
            .create_object_with_fields(
                &access,
                data(&alice, root.id),
                id(1),
                &title("Alice offline"),
            )
            .unwrap();
        let b = bob
            .create_object_with_fields(
                &bob_access,
                data(&bob, root.id),
                id(1),
                &title("Bob offline"),
            )
            .unwrap();
        sync_volume(&alice, &bob, root.id);
        sync_volume(&bob, &alice, root.id);
        assert_eq!(
            alice
                .query_objects(&access, data(&alice, root.id), id(1), None, 10)
                .unwrap()
                .objects
                .len(),
            2
        );
        assert_eq!(alice.read_object(&access, data(&alice, root.id), b.object_id).unwrap(), b);
        assert_eq!(bob.read_object(&bob_access, data(&bob, root.id), a.object_id).unwrap(), a);
        for database in [
            crate::store::DatabaseId::Index,
            crate::store::DatabaseId::Revocations,
            crate::store::DatabaseId::Entry(data(&alice, root.id).entry),
        ] {
            assert_eq!(
                alice.replica_root(root.id, database).unwrap(),
                bob.replica_root(root.id, database).unwrap()
            );
        }
        let ack = DurableRevision {
            revision: 1,
            policy_revision: 1,
            root: alice
                .replica_root(root.id, crate::store::DatabaseId::Entry(data(&alice, root.id).entry))
                .unwrap(),
        };
        alice
            .acknowledge_replica(
                root.id,
                crate::store::DatabaseId::Entry(data(&alice, root.id).entry),
                &ack,
            )
            .unwrap();
        alice
            .edit_fields(&access, data(&alice, root.id), a.object_id, &title("Pending newer edit"))
            .unwrap();
        assert_ne!(
            alice
                .replica_root(root.id, crate::store::DatabaseId::Entry(data(&alice, root.id).entry))
                .unwrap(),
            ack.root
        );
        assert_eq!(
            alice
                .durable_revision(
                    root.id,
                    crate::store::DatabaseId::Entry(data(&alice, root.id).entry)
                )
                .unwrap(),
            Some(ack.clone())
        );
        assert!(
            alice
                .acknowledge_replica(
                    root.id,
                    crate::store::DatabaseId::Entry(data(&alice, root.id).entry),
                    &DurableRevision { revision: 0, policy_revision: 1, root: ack.root }
                )
                .is_err()
        );
    }
    #[test]
    fn service_metadata_policy_rejects_private_restricted_and_nonowner_acl_writes() {
        let (_dir, store, root, access) = setup();
        let private = store.create_volume("Private", false).unwrap();
        assert!(store.service_provisioning(private.id).is_err());
        let entry = data(&store, root.id).entry;
        store
            .set_entry_permissions(&access, root.id, entry, EntryAcl::Replace(Acl::default()))
            .unwrap();
        assert!(store.service_provisioning(root.id).is_ok());
        let index = store.db(root.id, crate::store::DatabaseId::Index).unwrap();
        let policy = DatabasePolicy::read(&index, crate::store::DatabaseId::Entry(entry)).unwrap();
        assert!(policy.authorize_reader(ReplicaReader::Service).is_err());
        assert!(policy.authorize_reader(ReplicaReader::Participant(access.account)).is_err());
        let controls = DatabasePolicy::read(&index, crate::store::DatabaseId::Index).unwrap();
        let raw = record::encode(&Acl::default()).unwrap();
        let delta = kvdb::sync::Delta {
            key: &DEFAULT_PERMISSIONS_KEY,
            local: kvdb::sync::EntState::Missing,
            remote: kvdb::sync::EntState::Present { value: &raw, clock: kvdb::Clock::from_u64(9) },
        };
        assert!(controls.validate_source(&delta, Some(id(9))).is_err());
        assert!(controls.validate_source(&delta, Some(access.account)).is_ok());
    }

    #[test]
    fn blob_acknowledgments_are_per_reference_and_a_volume_seals_one_ciphertext_per_file() {
        let (_dir, store, root, access) = setup();
        let second = store.create_volume("Second", true).unwrap();
        let a = blob(&store, &access, root.id, "a", b"same content");
        let b = blob(&store, &access, root.id, "b", b"same content");
        let c = blob(&store, &access, second.id, "c", b"same content");
        let first = encrypted_bytes(&store, &access, &a).unwrap();
        assert_eq!(first, encrypted_bytes(&store, &access, &a).unwrap());
        assert_eq!(first, encrypted_bytes(&store, &access, &b).unwrap());
        let other = encrypted_bytes(&store, &access, &c).unwrap();
        assert_ne!(first, other);
        assert_eq!(a.encrypted_key, *blake3::hash(&first).as_bytes());
        assert!(store.receive_blob_version(&access, &c, &mut &first[..], true).is_err());
        let mut envelope = first.clone();
        *envelope.last_mut().unwrap() ^= 1;
        assert!(store.receive_blob_version(&access, &a, &mut &envelope[..], true).is_err());
        store.receive_blob_version(&access, &a, &mut &first[..], true).unwrap();
        // One durable envelope covers every independent version naming it.
        assert!(store.blob_version_durable(&access, &b).unwrap());
        assert!(!store.blob_version_durable(&access, &c).unwrap());
        assert_eq!(
            store.pending_entry_blobs(&access, root.id, None).unwrap().versions,
            vec![b.clone()]
        );
        store.acknowledge_blob_version(&access, &b).unwrap();
        assert_eq!(store.evict_blobs(0).unwrap(), 12);
        store.receive_blob_version(&access, &c, &mut &other[..], true).unwrap();
        assert_eq!(store.evict_blobs(0).unwrap(), 0);
        assert!(store.read_blob_version(&Access::owner(id(99)), &a).is_err());
        assert!(
            store.receive_blob_version(&Access::owner(id(99)), &a, &mut &first[..], true).is_err()
        );
    }

    #[test]
    fn lazy_bootstrap_authenticates_blob_before_cache_admission_and_never_infers_ack_from_dedup() {
        let (_a, alice, bob_dir, bob, root, owner, participant) = live::pair();
        let a = blob(&alice, &owner, root.id, "a", b"lazy bytes");
        let b = blob(&alice, &owner, root.id, "b", b"lazy bytes");
        let ciphertext = encrypted_bytes(&alice, &owner, &a).unwrap();
        sync_volume(&alice, &bob, root.id);
        assert!(bob.read_blob_version(&participant, &a).is_err());
        assert!(bob.pending_entry_blobs(&participant, root.id, None).unwrap().versions.is_empty());
        for position in [0, 1, 13, ciphertext.len() - 1] {
            let mut corrupt = ciphertext.clone();
            corrupt[position] ^= 1;
            assert!(bob.receive_blob_version(&participant, &a, &mut &corrupt[..], true).is_err());
            assert!(!bob_dir.path().join(crate::paths::BLOBS).exists());
        }
        assert!(bob.receive_blob_version(&participant, &a, &mut &ciphertext[..20], true).is_err());
        let key = bob.credentials(root.id).unwrap().blob_key;
        for wrong in [b"other byte".as_slice(), b"wrong".as_slice()] {
            let wrong_digest = *blake3::hash(wrong).as_bytes();
            let envelope = sealed_bytes(&key, root.id, &wrong_digest, wrong).unwrap();
            assert!(bob.receive_blob_version(&participant, &a, &mut &envelope[..], true).is_err());
        }
        bob.receive_blob_version(&participant, &a, &mut &ciphertext[..], false).unwrap();
        assert_eq!(bob.read_blob_version(&participant, &b).unwrap(), b"lazy bytes");
        assert!(!bob.blob_version_durable(&participant, &b).unwrap());
        assert_eq!(bob.evict_blobs(0).unwrap(), 10);
        bob.receive_blob_version(&participant, &a, &mut &ciphertext[..], true).unwrap();
        assert!(bob.blob_version_durable(&participant, &b).unwrap());
        assert_eq!(bob.evict_blobs(0).unwrap(), 0);
    }

    #[test]
    fn replica_work_reuses_retained_database_and_preserves_read_snapshot() {
        let (_directory, store, volume, _) = setup();
        let database = store.db(volume.id, crate::store::DatabaseId::Revocations).unwrap();
        let snapshot = database.begin_read().unwrap();
        let account = id(99).to_string();
        assert!(!snapshot.has(account.as_bytes()).unwrap());
        store
            .with_replica(volume.id, crate::store::DatabaseId::Revocations, |replica| {
                put(replica, account.as_bytes(), &true)
            })
            .unwrap();
        assert!(!snapshot.has(account.as_bytes()).unwrap(), "a retained snapshot must not change");
        assert!(database.begin_read().unwrap().has(account.as_bytes()).unwrap());
        assert_eq!(
            store.replica_root(volume.id, crate::store::DatabaseId::Revocations).unwrap(),
            replica_root(&database).unwrap()
        );
    }

    #[test]
    fn concurrent_first_access_uses_one_handle_and_failed_open_releases_its_slot() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Concurrent first access", false).unwrap();
        store.close_db(volume.id, crate::store::DatabaseId::Revocations).unwrap();
        let barrier = std::sync::Barrier::new(8);
        let handles = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        store.db(volume.id, crate::store::DatabaseId::Revocations).unwrap()
                    })
                })
                .collect();
            jobs.into_iter().map(|job| job.join().unwrap()).collect::<Vec<_>>()
        });
        for handle in &handles {
            assert!(Arc::ptr_eq(&handles[0], handle));
        }
        assert!(store.open.lock().unwrap().transitioning.is_empty());
        assert!(
            store
                .db(
                    volume.id,
                    crate::store::DatabaseId::Entry(
                        crate::store::EntryId::from_bytes([99; 15]).unwrap()
                    )
                )
                .is_err()
        );
        assert!(store.open.lock().unwrap().transitioning.is_empty());
        let key = store.credentials(volume.id).unwrap().database_key;
        let created = store
            .replica_db(
                volume.id,
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([99; 15]).unwrap(),
                ),
                Some(key),
            )
            .unwrap();
        assert!(Arc::ptr_eq(
            &created,
            &store
                .db(
                    volume.id,
                    crate::store::DatabaseId::Entry(
                        crate::store::EntryId::from_bytes([99; 15]).unwrap()
                    )
                )
                .unwrap()
        ));
        assert!(matches!(
            store.replica_db(
                volume.id,
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([99; 15]).unwrap()
                ),
                Some([0; 32])
            ),
            Err(Error::AccessDenied("database key mismatch"))
        ));
    }

    #[test]
    fn replica_creation_publishes_complete_file_and_never_resets_existing_key() {
        let directory = tempfile::tempdir().unwrap();
        // The only artifact before atomic publication is a temporary file.
        fs::write(directory.path().join(".replica-interrupted.tmp"), b"incomplete").unwrap();
        let path = directory.path().join("replica.kvdb");
        let replica = create_replica(&path, [7; 32], 2 * 1024 * 1024).unwrap();
        put(&replica, b"retained", &"durable").unwrap();
        let root = replica_root(&replica).unwrap();
        drop(replica);
        assert!(create_replica(&path, [8; 32], 2 * 1024 * 1024).is_err());
        let replica = create_replica(&path, [7; 32], 2 * 1024 * 1024).unwrap();
        assert_eq!(replica_root(&replica).unwrap(), root);
        assert_eq!(required::<String>(&replica, "retained").unwrap(), "durable");
    }
    #[test]
    fn outbound_backings_preserve_clocks_and_gate_whole_entry_content() {
        let (_dir, store, root, access) = setup();
        let participant = id(99);
        store
            .set_permissions(
                &access,
                root.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                },
            )
            .unwrap();
        let private = store
            .create_database(
                &access,
                root.id,
                "private",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let service_only = store
            .create_database(
                &access,
                root.id,
                "service-only",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let owner_grant = Grant {
            subject: Subject::Principal(Principal::Human(access.account)),
            permissions: vec![Permission::Read, Permission::Write],
        };
        store
            .set_entry_permissions(
                &access,
                root.id,
                private.id,
                EntryAcl::Replace(Acl { grants: vec![owner_grant.clone()] }),
            )
            .unwrap();
        store
            .set_entry_permissions(
                &access,
                root.id,
                service_only.id,
                EntryAcl::Replace(Acl {
                    grants: vec![
                        owner_grant,
                        Grant {
                            subject: Subject::Principal(PLAYBIT_REPLICA_PRINCIPAL),
                            permissions: vec![Permission::Read],
                        },
                    ],
                }),
            )
            .unwrap();
        for entry in [private.id, service_only.id] {
            let backing = store.db(root.id, crate::store::DatabaseId::Entry(entry)).unwrap();
            let mut tx = backing.begin_write(now_unix_secs()).unwrap();
            tx.put(b"retained", b"payload").unwrap();
            tx.put(b"deleted", b"before").unwrap();
            tx.commit().unwrap();
            let mut tx = backing.begin_write(now_unix_secs()).unwrap();
            tx.del(b"deleted").unwrap();
            tx.commit().unwrap();
            let root_hash = replica_root(&backing).unwrap();
            let policy = DatabasePolicy::read(
                &*store.db(root.id, crate::store::DatabaseId::Index).unwrap(),
                crate::store::DatabaseId::Entry(entry),
            )
            .unwrap();
            assert_eq!(
                projection_root(&backing, &policy, ReplicaReader::Participant(access.account))
                    .unwrap(),
                root_hash
            );
            for reader in [ReplicaReader::Service, ReplicaReader::Participant(participant)] {
                let allowed = entry == service_only.id && matches!(reader, ReplicaReader::Service);
                let result = outbound_replica(&std::env::temp_dir(), &backing, &policy, reader);
                if allowed {
                    let projected = result.unwrap();
                    assert_eq!(projected.root().unwrap(), root_hash);
                    assert_eq!(
                        projected.database().begin_read().unwrap().is_del(b"deleted").unwrap(),
                        backing.begin_read().unwrap().is_del(b"deleted").unwrap()
                    );
                } else {
                    assert!(matches!(result, Err(Error::AccessDenied(_))));
                }
            }
            assert_eq!(replica_root(&backing).unwrap(), root_hash);
        }
        store
            .with_outbound_replica(
                root.id,
                crate::store::DatabaseId::Index,
                ReplicaReader::Service,
                |db| {
                    let snapshot = crate::store::volume_index::IndexSnapshot::new(db.begin_read()?);
                    assert_eq!(snapshot.page(None, 64)?.entries.len(), 3);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn pruning_erases_disallowed_live_entries_and_tombstones_durably() {
        let (_dir, store, root, access) = setup();
        let kept = store
            .create_object_with_fields(&access, data(&store, root.id), id(1), &title("keep"))
            .unwrap();
        let entry = store
            .create_database(
                &access,
                root.id,
                "private",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let backing = store.db(root.id, crate::store::DatabaseId::Entry(entry.id)).unwrap();
        let mut tx = backing.begin_write(now_unix_secs()).unwrap();
        for key in [b"a", b"b", b"c"] {
            tx.put(key, b"payload").unwrap();
        }
        tx.commit().unwrap();
        let mut tx = backing.begin_write(now_unix_secs()).unwrap();
        tx.del(b"c").unwrap();
        tx.commit().unwrap();
        let service_dir = tempfile::tempdir().unwrap();
        let path = service_dir.path().join("replica.kvdb");
        let service = create_replica(&path, backing.meta().hash_key, 512 * 1024).unwrap();
        pull(&backing, &service);
        store
            .set_entry_permissions(&access, root.id, entry.id, EntryAcl::Replace(Acl::default()))
            .unwrap();
        let policy = DatabasePolicy::read(
            &*store.db(root.id, crate::store::DatabaseId::Index).unwrap(),
            crate::store::DatabaseId::Entry(entry.id),
        )
        .unwrap();
        assert_eq!(prune_replica(&service, &policy, ReplicaReader::Service).unwrap(), 3);
        assert!(service.begin_read().unwrap().root_hash().unwrap().is_none());
        drop(service);
        let service = open_replica(&path, 512 * 1024).unwrap();
        assert!(service.begin_read().unwrap().iter_from(b"").next().is_none());
        assert_eq!(
            store.read_object(&access, data(&store, root.id), kept.object_id).unwrap(),
            kept
        );
        assert!(replica_root(&backing).unwrap().is_some());
    }

    #[test]
    fn schema_changes_keep_existing_field_meanings_and_records() {
        let (_dir, store, root, access) = setup();
        let address = data(&store, root.id);
        let object =
            store.create_object_with_fields(&access, address, id(1), &title("retained")).unwrap();
        let before = store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap();
        let mut invalid = schema();
        invalid.types.get_mut(&id(1)).unwrap().fields.get_mut(&id(2)).unwrap().value_type =
            ValueType::Boolean;
        assert!(store.establish_schema(&access, address, &invalid).is_err());
        assert_eq!(store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap(), before);
        assert_eq!(store.read_object(&access, address, object.object_id).unwrap(), object);
        let mut added = schema();
        added.types.get_mut(&id(1)).unwrap().fields.insert(
            id(5),
            FieldDeclaration {
                name: "other".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            },
        );
        store.establish_schema(&access, address, &added).unwrap();
        assert_eq!(store.schema(&access, address).unwrap(), added);
    }

    #[test]
    fn complete_births_check_current_permissions_and_creator_provenance() {
        use kvdb::sync::{Delta, EntState};
        let (_dir, store, root, access) = setup();
        let address = data(&store, root.id);
        let object =
            store.create_object_with_fields(&access, address, id(1), &title("birth")).unwrap();
        let backing = store.db(root.id, crate::store::DatabaseId::Entry(address.entry)).unwrap();
        let snapshot = backing.begin_read().unwrap();
        let key = crate::store::object_database::creation_key(object.object_id);
        let (clock, value) = snapshot.get_with_clock(key.as_slice()).unwrap().unwrap();
        let delta = Delta {
            key: key.as_slice(),
            local: EntState::Missing,
            remote: EntState::Present { value: &value, clock },
        };
        let policy = DatabasePolicy::read(
            &*store.db(root.id, crate::store::DatabaseId::Index).unwrap(),
            crate::store::DatabaseId::Entry(address.entry),
        )
        .unwrap();
        assert!(policy.validate_source(&delta, Some(id(99))).is_err());
        assert!(policy.validate_source(&delta, Some(access.account)).is_ok());
        let mut malformed = value.to_vec();
        // The first tagged binary field is the fixed-width creator identity.
        malformed[12..28].copy_from_slice(id(99).as_bytes());
        let forged = Delta { remote: EntState::Present { value: &malformed, clock }, ..delta };
        assert!(policy.validate_source(&forged, Some(access.account)).is_err());
    }

    #[test]
    fn append_sync_requires_absent_keys_and_creator_provenance() {
        use kvdb::sync::{Delta, EntState};
        let (_dir, store, root, access) = setup();
        store
            .set_permissions(
                &access,
                root.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Append],
                    }],
                },
            )
            .unwrap();
        let writer = id(99);
        let address = data(&store, root.id);
        let object = store
            .create_object_with_fields(&Access::owner(writer), address, id(1), &title("append"))
            .unwrap();
        let backing = store.db(root.id, crate::store::DatabaseId::Entry(address.entry)).unwrap();
        let snapshot = backing.begin_read().unwrap();
        let key = crate::store::object_database::creation_key(object.object_id);
        let (clock, value) = snapshot.get_with_clock(key.as_slice()).unwrap().unwrap();
        let policy = DatabasePolicy::read(
            &*store.db(root.id, crate::store::DatabaseId::Index).unwrap(),
            crate::store::DatabaseId::Entry(address.entry),
        )
        .unwrap();
        let mut delta = Delta {
            key: key.as_slice(),
            local: EntState::Missing,
            remote: EntState::Present { value: &value, clock },
        };
        assert!(policy.validate_source(&delta, Some(writer)).is_ok());
        delta.local = EntState::Present { value: &value, clock };
        assert!(
            policy.validate_source(&delta, Some(writer)).is_ok(),
            "identical duplicate birth is idempotent"
        );
        delta.local = EntState::Deleted { clock };
        assert!(policy.validate_source(&delta, Some(writer)).is_err());
        delta.local = EntState::Missing;
        delta.remote = EntState::Deleted { clock };
        assert!(policy.validate_source(&delta, Some(writer)).is_err());
        delta.remote = EntState::Present { value: &value, clock };
        assert!(policy.validate_source(&delta, Some(id(98))).is_err());
        let field = crate::store::object_database::field_key(object.object_id, id(2));
        delta.key = field.as_bytes();
        assert!(policy.validate_source(&delta, Some(writer)).is_err());
    }

    #[test]
    fn observed_session_denial_survives_restart_without_discarding_owner_data() {
        let (dir, store, root, access) = setup();
        let version = crate::identifier::encode(&[0xee; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let object = store
            .create_object_with_fields(&access, data(&store, root.id), id(1), &title("retained"))
            .unwrap();
        let root_hash = store
            .replica_root(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
            .unwrap();
        store.record_session_denial(&session, "session participant revoked").unwrap();
        assert!(
            store
                .read_object(
                    &Access::session(access.account, &session.id),
                    data(&store, root.id),
                    object.object_id
                )
                .is_err()
        );
        assert_eq!(
            store.read_object(&access, data(&store, root.id), object.object_id).unwrap(),
            object
        );
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(
            store.session_denial(&session.id).unwrap().as_deref(),
            Some("session participant revoked")
        );
        assert!(
            store
                .read_object(
                    &Access::session(access.account, &session.id),
                    data(&store, root.id),
                    object.object_id
                )
                .is_err()
        );
        assert_eq!(
            store
                .replica_root(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                .unwrap(),
            root_hash
        );
        let generation = store.session_denial_generation(&session).unwrap();
        assert!(store.clear_session_denial(&session, generation).unwrap());
        assert_eq!(
            store
                .read_object(
                    &Access::session(access.account, &session.id),
                    data(&store, root.id),
                    object.object_id
                )
                .unwrap(),
            object
        );
    }
    #[test]
    fn durable_receipts_track_policy_changes_without_payload_clock_changes() {
        let (_dir, store, root, _access) = setup();
        let initial = DurableRevision { revision: 10, policy_revision: 20, root: None };
        store
            .acknowledge_replica(
                root.id,
                crate::store::DatabaseId::Entry(data(&store, root.id).entry),
                &initial,
            )
            .unwrap();
        let projected =
            DurableRevision { revision: 10, policy_revision: 21, root: Some((1, [5; 32])) };
        store
            .acknowledge_replica(
                root.id,
                crate::store::DatabaseId::Entry(data(&store, root.id).entry),
                &projected,
            )
            .unwrap();
        for stale in [
            DurableRevision { revision: 9, policy_revision: 22, root: projected.root },
            DurableRevision { revision: 11, policy_revision: 20, root: projected.root },
            DurableRevision { revision: 10, policy_revision: 21, root: None },
        ] {
            assert!(
                store
                    .acknowledge_replica(
                        root.id,
                        crate::store::DatabaseId::Entry(data(&store, root.id).entry),
                        &stale
                    )
                    .is_err()
            );
        }
        store
            .acknowledge_replica(
                root.id,
                crate::store::DatabaseId::Entry(data(&store, root.id).entry),
                &projected,
            )
            .unwrap();
        assert_eq!(
            store
                .durable_revision(
                    root.id,
                    crate::store::DatabaseId::Entry(data(&store, root.id).entry)
                )
                .unwrap(),
            Some(projected)
        );
        store
            .with_replica(root.id, crate::store::DatabaseId::Index, |db| {
                assert_eq!(MetadataPolicy::read(db)?.revision(), db.clock().as_u64());
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn exhausted_eviction_generation_retains_the_file_before_refusing_eviction() {
        let (_dir, store, root, access) = setup();
        let version = blob(&store, &access, root.id, "retained", b"verified bytes");
        store.acknowledge_blob_version(&access, &version).unwrap();
        store.blob_eviction_generation.store(u64::MAX, std::sync::atomic::Ordering::Release);
        assert!(matches!(store.set_cache_budget(0), Err(Error::LimitExceeded)));
        assert_eq!(store.blob_eviction_generation(), u64::MAX);
        assert_eq!(
            store.read_blob_entry(&access, root.id, version.address.entry).unwrap(),
            b"verified bytes"
        );
    }

    #[test]
    fn cache_budget_persists_without_unpinning_pending_bytes() {
        // Reopen a zero-budget store and retain pending bytes until their acknowledgment.
        let (dir, store, root, access) = setup();
        assert_eq!(store.cache_budget().unwrap(), DEFAULT_BLOB_CACHE_BUDGET);
        let version = blob(&store, &access, root.id, "pending", b"pending bytes");
        assert_eq!(store.set_cache_budget(0).unwrap(), 13);
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(store.cache_budget().unwrap(), 0);
        assert_eq!(store.enforce_cache_budget().unwrap(), 13);
        store.acknowledge_blob_version(&access, &version).unwrap();
        assert_eq!(store.enforce_cache_budget().unwrap(), 0);
        assert!(matches!(store.read_blob_version(&access, &version), Err(Error::NotFound(_))));
    }

    #[test]
    fn cache_budget_obeys_profile_writer_admission() {
        // A failing profile writer poisons admission while a budget change
        // races it. Either scheduling order must preserve the budget and cache.
        let (_dir, store, root, access) = setup();
        let version = blob(&store, &access, root.id, "cached", b"retained cache");
        store.acknowledge_blob_version(&access, &version).unwrap();
        let ready = std::sync::Barrier::new(2);
        let result = std::thread::scope(|scope| {
            let mut updating = None;
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _profile = store.lock_profile().unwrap();
                let _writer = store.metadata.begin_write(now_unix_secs()).unwrap();
                updating = Some(scope.spawn(|| {
                    ready.wait();
                    store.set_cache_budget(0)
                }));
                ready.wait();
                panic!("injected profile writer failure");
            }));
            assert!(failed.is_err(), "the admitted writer failed before commit");
            updating.expect("budget worker started").join().unwrap()
        });
        assert!(matches!(result, Err(Error::Internal("profile lock is poisoned"))));
        assert_eq!(store.cache_budget().unwrap(), DEFAULT_BLOB_CACHE_BUDGET);
        assert!(store.blob_path(&version.digest, true).is_file());
    }

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }
    fn schema() -> Schema {
        Schema {
            types: BTreeMap::from([(
                id(1),
                TypeDeclaration {
                    name: "Todo".into(),
                    deprecated: false,
                    fields: BTreeMap::from([
                        (
                            id(2),
                            FieldDeclaration {
                                name: "title".into(),
                                value_type: ValueType::Text,
                                repeated: false,
                                deprecated: false,
                            },
                        ),
                        (
                            id(3),
                            FieldDeclaration {
                                name: "done".into(),
                                value_type: ValueType::Boolean,
                                repeated: false,
                                deprecated: false,
                            },
                        ),
                        (
                            id(4),
                            FieldDeclaration {
                                name: "attachment".into(),
                                value_type: ValueType::File,
                                repeated: false,
                                deprecated: false,
                            },
                        ),
                    ]),
                },
            )]),
        }
    }
    fn title(text: &str) -> BTreeMap<Id, Option<Vec<Value>>> {
        BTreeMap::from([(id(2), Some(vec![Value::Text(text.into())]))])
    }
    fn blob(
        store: &LocalStore,
        access: &Access,
        volume: Id,
        name: &str,
        bytes: &[u8],
    ) -> crate::store::volume_index::BlobVersion {
        let entry = store.create_blob(access, volume, name, bytes).unwrap();
        store
            .blob_version(
                access,
                crate::store::volume_index::EntryAddress { volume, entry: entry.id },
                None,
            )
            .unwrap()
    }
    fn setup() -> (tempfile::TempDir, LocalStore, Volume, Access) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let access = Access::owner(store.account.id);
        let volume = store.create_volume("Root", true).unwrap();
        store
            .create_database(
                &access,
                volume.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
            )
            .unwrap();
        (dir, store, volume, access)
    }

    fn data(store: &LocalStore, volume: Id) -> crate::store::volume_index::EntryAddress {
        let database = store.db(volume, crate::store::DatabaseId::Index).unwrap();
        let snapshot =
            crate::store::volume_index::IndexSnapshot::new(database.begin_read().unwrap());
        crate::store::volume_index::EntryAddress {
            volume,
            entry: snapshot.lookup("data").unwrap().id,
        }
    }

    #[test]
    fn clearing_large_old_fields_does_not_require_a_read_projection() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let access = Access::owner(store.account.id);
        let volume = store.create_volume("Repeated fields", false).unwrap();
        let mut declaration = schema();
        let fields = &mut declaration.types.get_mut(&id(1)).unwrap().fields;
        for field in [id(2), id(3)] {
            let value = fields.get_mut(&field).unwrap();
            value.value_type = ValueType::Text;
            value.repeated = true;
        }
        let entry = store
            .create_database(
                &access,
                volume.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: declaration },
            )
            .unwrap();
        let address =
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
        let object = store.create_object(&access, address, id(1)).unwrap().object_id;
        for field in [id(2), id(3)] {
            let record =
                store.read_object_fields(&access, address, object, Some(&[field])).unwrap();
            let schema = store.schema(&access, address).unwrap();
            store
                .with_replica(volume.id, crate::store::DatabaseId::Entry(entry.id), |db| {
                    let mut tx = db.begin_write(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    )?;
                    crate::store::object_database::edit(
                        &mut tx,
                        &schema,
                        record,
                        access.account,
                        // Together both fields fill the 4096-value object
                        // budget; setup must not create an unreadable object.
                        &BTreeMap::from([(field, Some(vec![Value::Text(String::new()); 2048]))]),
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .unwrap();
        }
        assert!(store.read_object(&access, address, object).is_ok());
        store
            .edit_fields(&access, address, object, &BTreeMap::from([(id(2), None), (id(3), None)]))
            .unwrap();
        let cleared =
            store.read_object_fields(&access, address, object, Some(&[id(2), id(3)])).unwrap();
        assert!(cleared.fields.values().all(|cell| cell.values.is_none()));
    }

    #[test]
    fn local_creation_capacity_rejects_before_data_commit_and_keeps_existing_edits() {
        let (_dir, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let kept =
            store.create_object_with_fields(&access, address, id(1), &title("kept")).unwrap();
        let database = crate::store::DatabaseId::Entry(address.entry);
        let before = store.replica_root(volume.id, database).unwrap();
        assert!(matches!(
            store.create_object_with_fields(
                &access,
                address,
                id(1),
                &title(&"x".repeat(playbit_api::abi::syscall::MAX_DATA_BYTES + 1))
            ),
            Err(Error::LimitExceeded)
        ));
        assert_eq!(store.replica_root(volume.id, database).unwrap(), before);
        store.edit_fields(&access, address, kept.object_id, &title("still usable")).unwrap();
        assert_eq!(
            store.read_object(&access, address, kept.object_id).unwrap().fields[&id(2)].values,
            title("still usable")[&id(2)]
        );
    }

    #[test]
    fn outbound_backing_snapshot_does_not_lock_writes_during_send() {
        let (_directory, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let first =
            store.create_object_with_fields(&access, address, id(1), &title("before")).unwrap();
        store
            .with_outbound_replica(
                volume.id,
                crate::store::DatabaseId::Entry(address.entry),
                ReplicaReader::Service,
                |database| {
                    let later = store.create_object_with_fields(
                        &access,
                        address,
                        id(1),
                        &title("after"),
                    )?;
                    let snapshot =
                        crate::store::object_database::Snapshot::new(database.begin_read()?);
                    assert_eq!(snapshot.read(first.object_id, None)?, first);
                    assert!(matches!(
                        snapshot.read(later.object_id, None),
                        Err(Error::NotFound(_))
                    ));
                    assert_eq!(store.read_object(&access, address, later.object_id)?, later);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn pinned_projection_does_not_include_a_later_committed_value() {
        let (_directory, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let object =
            store.create_object_with_fields(&access, address, id(1), &title("before")).unwrap();
        let database = crate::store::DatabaseId::Entry(address.entry);
        let policy = DatabasePolicy::read(
            &*store.db(volume.id, crate::store::DatabaseId::Index).unwrap(),
            database,
        )
        .unwrap();
        let source = store.db(volume.id, database).unwrap();
        let snapshot = OutboundSnapshot::new(&source).unwrap();
        store.edit_fields(&access, address, object.object_id, &title("after")).unwrap();
        let projected =
            snapshot.project(&std::env::temp_dir(), &policy, ReplicaReader::Service).unwrap();
        let old = crate::store::object_database::Snapshot::new(
            projected.database().begin_read().unwrap(),
        )
        .read(object.object_id, None)
        .unwrap();
        assert_eq!(old, object);
        assert_ne!(store.read_object(&access, address, object.object_id).unwrap(), old);
    }

    #[test]
    fn candidate_root_refresh_requires_provisioning_and_preserves_local_denials() {
        let (_alice_dir, alice, root, owner) = setup();
        alice
            .set_permissions(
                &owner,
                root.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                },
            )
            .unwrap();
        let old_version = crate::identifier::encode(&[0xab; 32]);
        let next_version = crate::identifier::encode(&[0xcd; 32]);
        // Owner may create an unprovisioned original version. Only the later
        // candidate will be provisioned for the recipient.
        let session = alice
            .create_session(
                &crate::package::AppIdentity {
                    publisher: alice.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: old_version.clone() },
                &old_version,
                root.id,
            )
            .unwrap();
        let invitation =
            alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
        let envelope = alice.session_envelope(&session.id).unwrap();
        for scenario in ["missing", "revoked", "denied", "ready"] {
            let bob_dir = tempfile::tempdir().unwrap();
            let bob = LocalStore::open(bob_dir.path()).unwrap();
            bob.import_invitation_link(
                &invitation,
                &envelope,
                crate::release_channel::ReleaseChannel::Public,
            )
            .unwrap();
            sync_volume(&alice, &bob, root.id);
            sync_volume(&alice, &bob, session.root_volume);
            let candidate = Access::session_version(bob.account().id, &session.id, &next_version);
            assert!(
                bob.schema(&Access::session(bob.account().id, &session.id), data(&bob, root.id))
                    .is_err()
            );
            let staged = alice
                .with_replica(root.id, crate::store::DatabaseId::Index, |db| {
                    detached_replica(&std::env::temp_dir(), db)
                })
                .unwrap();
            let candidate_entry = crate::store::EntryId::generate().unwrap();
            let mut tx = staged.database().begin_write(now_unix_secs()).unwrap();
            crate::store::volume_index::create(
                &mut tx,
                candidate_entry,
                &crate::store::volume_index::EntryInfo {
                    namespace: Default::default(),
                    origin: crate::store::volume_index::namespace::Origin {
                        installation: Id::from_bytes([42; 16]),
                        label: None,
                    },
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: crate::store::EntryId::generate().unwrap(),
                    name: "candidate-entry".into(),
                    kind: crate::store::volume_index::EntryKind::KvDb {},
                    protocol: Some(crate::store::volume_index::DatabaseProtocol::Kvdb),
                },
            )
            .unwrap();
            tx.commit().unwrap();
            if scenario != "missing" {
                let mut key = vec![crate::store::volume_index::META_BYTE, 18];
                key.extend_from_slice(next_version.as_bytes());
                put(staged.database(), &key, &true).unwrap();
            }
            if scenario == "revoked" {
                bob.with_replica(
                    session.root_volume,
                    crate::store::DatabaseId::Revocations,
                    |db| put(db, bob.account().id.to_string().as_bytes(), &true),
                )
                .unwrap();
            } else if scenario == "denied" {
                bob.set_permissions(&owner, root.id, Acl::default()).unwrap();
            }
            let before = bob.replica_root(root.id, crate::store::DatabaseId::Index).unwrap();
            let result = bob.merge_candidate_root_metadata(&candidate, staged.database());
            if scenario == "ready" {
                result.unwrap();
                let current = bob.db(root.id, crate::store::DatabaseId::Index).unwrap();
                let snapshot =
                    crate::store::volume_index::IndexSnapshot::new(current.begin_read().unwrap());
                assert_eq!(snapshot.entry(candidate_entry).unwrap().info.name, "candidate-entry");
                assert!(!snapshot.header(candidate_entry).unwrap().deleted);
                bob.schema(&candidate, data(&bob, root.id)).unwrap();
                assert_eq!(bob.session(&session.id).unwrap().resolved_build_id, old_version);
            } else {
                assert!(result.is_err(), "{scenario}");
                assert_eq!(
                    bob.replica_root(root.id, crate::store::DatabaseId::Index).unwrap(),
                    before,
                    "{scenario}"
                );
            }
        }
    }
    #[test]
    fn detached_receive_preserves_edits_made_during_transfer() {
        let (_dir, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let before =
            store.create_object_with_fields(&access, address, id(1), &title("before")).unwrap();
        let stage = received::receive(&store, &store, volume.id);
        store.edit_fields(&access, address, before.object_id, &title("during transfer")).unwrap();
        let added = store
            .create_object_with_fields(&access, address, id(1), &title("new local object"))
            .unwrap();
        store.adopt_received_volume(&access, &stage).unwrap();
        assert_eq!(
            store.read_object(&access, address, before.object_id).unwrap().fields[&id(2)].values,
            title("during transfer")[&id(2)]
        );
        assert_eq!(store.read_object(&access, address, added.object_id).unwrap(), added);
    }

    #[test]
    fn received_delta_advances_delivery_revision_without_outgoing_echo() {
        let (_dir, store, volume, access) = setup();
        let mut receiver = store.take_change_receiver().unwrap();
        while !receiver.drain().records.is_empty() {}
        let staged = store
            .with_replica(volume.id, crate::store::DatabaseId::Revocations, |db| {
                detached_replica(&std::env::temp_dir(), db)
            })
            .unwrap();
        let mut tx = staged
            .database()
            .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
            .unwrap();
        tx.put(id(55).to_string().as_bytes(), &encode(&"Removed participant").unwrap()).unwrap();
        let kvdb::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
            panic!("bounded replica delta");
        };
        let before = store.delivery_revision().unwrap();
        let outgoing = store.publication_generation();
        assert!(
            store
                .merge_received_delta(
                    &access,
                    volume.id,
                    crate::store::DatabaseId::Revocations,
                    &bytes
                )
                .unwrap()
                .committed
        );
        let after = store.delivery_revision().unwrap();
        assert_eq!(after.0, before.0);
        assert!(after.1 > before.1);
        assert_eq!(store.publication_generation(), outgoing);
        assert!(receiver.drain().records.is_empty());
        assert_eq!(store.pending_change_count(), 0);
    }
    #[test]
    fn detached_receive_rechecks_read_permission_and_rolls_back_entire_merge() {
        let (_dir, store, volume, owner) = setup();
        let address = data(&store, volume.id);
        let reader = Access::owner(id(99));
        store
            .set_permissions(
                &owner,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                },
            )
            .unwrap();
        let object =
            store.create_object_with_fields(&owner, address, id(1), &title("before")).unwrap();
        let stage = received::receive(&store, &store, volume.id);
        let database = crate::store::DatabaseId::Entry(address.entry);
        stage
            .with_database(database, |source| {
                let mut tx = source.begin_write(now_unix_secs())?;
                crate::store::object_database::edit(
                    &mut tx,
                    &schema(),
                    object.clone(),
                    owner.account,
                    &title("remote"),
                )?;
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                address.entry,
                EntryAcl::Replace(Acl::default()),
            )
            .unwrap();
        let before = store.replica_root(volume.id, database).unwrap();
        assert!(
            store
                .with_authorized_replica(
                    &reader,
                    volume.id,
                    database,
                    Some(Permission::Read),
                    |_, _| Ok(())
                )
                .is_err()
        );
        // A volume bootstrap still accepts metadata, but withholds denied backing bytes.
        assert!(!store.adopt_received_volume(&reader, &stage).unwrap().changed());
        assert_eq!(store.replica_root(volume.id, database).unwrap(), before);
        store.set_entry_permissions(&owner, volume.id, address.entry, EntryAcl::Inherit).unwrap();
        assert_eq!(store.read_object(&owner, address, object.object_id).unwrap(), object);
        store.set_permissions(&owner, volume.id, Acl::default()).unwrap();
        assert!(store.adopt_received_volume(&reader, &stage).is_err());
    }

    #[test]
    fn detached_write_rechecks_permissions_at_commit() {
        let (_dir, store, volume, owner) = setup();
        let address = data(&store, volume.id);
        let writer = Access::owner(id(99));
        store
            .set_permissions(
                &owner,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                },
            )
            .unwrap();
        let object =
            store.create_object_with_fields(&owner, address, id(1), &title("before")).unwrap();
        let database = crate::store::DatabaseId::Entry(address.entry);
        let stage = store
            .with_authorized_replica(
                &writer,
                volume.id,
                database,
                Some(Permission::Read),
                |db, _| detached_replica(&std::env::temp_dir(), db),
            )
            .unwrap();
        let mut tx = stage.database().begin_write(now_unix_secs()).unwrap();
        crate::store::object_database::edit(
            &mut tx,
            &schema(),
            object.clone(),
            writer.account,
            &title("remote writer"),
        )
        .unwrap();
        tx.commit().unwrap();
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                address.entry,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                }),
            )
            .unwrap();
        let before = store.replica_root(volume.id, database).unwrap();
        assert!(
            store.merge_authorized_replica(&writer, volume.id, database, stage.database()).is_err()
        );
        assert_eq!(store.replica_root(volume.id, database).unwrap(), before);
        assert_eq!(store.read_object(&owner, address, object.object_id).unwrap(), object);
    }

    #[test]
    fn guests_choose_session_keys_in_one_persistent_root_database() {
        let (dir, store, root, owner) = setup();
        let build = crate::identifier::encode(&[8; 32]);
        let a = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        let b = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        let entry = store
            .create_database(
                &owner,
                root.id,
                "app-state",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let address = crate::store::volume_index::EntryAddress { volume: root.id, entry: entry.id };
        let first = Access::session(owner.account, &a.id);
        let second = Access::session(owner.account, &b.id);
        store.kv_put(&first, address, a.id.as_bytes(), b"first session").unwrap();
        store.kv_put(&second, address, b.id.as_bytes(), b"second session").unwrap();
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        for (access, key, value) in [
            (&first, a.id.as_bytes(), b"first session".as_slice()),
            (&second, b.id.as_bytes(), b"second session".as_slice()),
        ] {
            assert_eq!(store.kv_read(access, address, key, 0, 1024, None).unwrap().bytes, value);
        }
        store.revoke(&owner, root.id, owner.account, "withdraw root").unwrap();
        assert!(store.kv_read(&first, address, a.id.as_bytes(), 0, 1024, None).is_err());
    }

    #[test]
    fn offline_restart_preserves_session_invitation_data_and_pending_ids() {
        let (dir, store, root, access) = setup();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let guest = Access::session(access.account, &session.id);
        assert_eq!(store.entries(&guest, session.root_volume, None, 64).unwrap().entries.len(), 1);
        let entry = store
            .create_database(
                &guest,
                session.root_volume,
                "offline",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
            )
            .unwrap();
        let address = crate::store::volume_index::EntryAddress {
            volume: session.root_volume,
            entry: entry.id,
        };
        let object =
            store.create_object_with_fields(&guest, address, id(1), &title("offline")).unwrap();
        let invitation =
            store.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
        let pending = store.pending_operations().unwrap();
        let account = store.account.clone();
        let service = store.create_service_identity("Score arbiter").unwrap();
        assert_eq!(service.owner, account.id);
        assert!(crate::identifier::valid(&service.id, 32));
        assert_eq!(pending.len(), 2);
        assert_eq!(store.entries(&guest, session.root_volume, None, 64).unwrap().entries.len(), 2);
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(store.account, account);
        assert_eq!(store.service_identities().unwrap(), [service]);
        assert_eq!(store.session(&session.id).unwrap(), session);
        assert_eq!(
            store.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
            invitation
        );
        assert_eq!(store.pending_operations().unwrap(), pending);
        assert_eq!(store.read_object(&guest, address, object.object_id).unwrap(), object);
    }

    #[test]
    fn edits_are_atomic_and_preserve_unknown_field_bytes() {
        let (_dir, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let object =
            store.create_object_with_fields(&access, address, id(1), &title("before")).unwrap();
        store.edit_fields(&access, address, object.object_id, &title("before")).unwrap();
        let database = store.db(volume.id, crate::store::DatabaseId::Entry(address.entry)).unwrap();
        let known = crate::store::object_database::field_key(object.object_id, id(2));
        let unknown = crate::store::object_database::field_key(object.object_id, id(99));
        let raw = database.begin_read().unwrap().get(known.as_bytes()).unwrap().unwrap().to_vec();
        let header =
            record::View::record(&raw).unwrap().fields(record::Mode::Write, &[1, 2]).unwrap();
        let mut cell: FieldCell =
            record::Codec::read(header.get(2).unwrap(), record::Mode::Write).unwrap();
        let mut tx = database.begin_write(now_unix_secs()).unwrap();
        cell.stamp.clock = tx.clock().as_u64();
        let mut encoded = Vec::from(record::MAGIC);
        encoded.push(record::STRUCT);
        record::put_raw(&mut encoded, 1, header.get(1).unwrap().0).unwrap();
        record::put(&mut encoded, 2, &cell).unwrap();
        let raw = encoded;
        tx.put(unknown.as_bytes(), &raw).unwrap();
        tx.commit().unwrap();
        let before = store.read_object(&access, address, object.object_id).unwrap();
        let changes = BTreeMap::from([
            (id(2), Some(vec![Value::Text("partial".into())])),
            (id(3), Some(vec![Value::Text("wrong type".into())])),
        ]);
        assert!(store.edit_fields(&access, address, object.object_id, &changes).is_err());
        assert_eq!(store.read_object(&access, address, object.object_id).unwrap(), before);
        store.edit_fields(&access, address, object.object_id, &title("after")).unwrap();
        assert_eq!(
            &*database.begin_read().unwrap().get(unknown.as_bytes()).unwrap().unwrap(),
            &raw
        );
    }

    #[test]
    fn session_scope_root_provisioning_and_revocations_are_checked_each_time() {
        let (_dir, store, root, access) = setup();
        let object = store
            .create_object_with_fields(&access, data(&store, root.id), id(1), &title("visible"))
            .unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let second = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let guest = Access::session(access.account, &session.id);
        assert!(
            store
                .schema(
                    &guest,
                    crate::store::volume_index::EntryAddress {
                        volume: second.root_volume,
                        entry: data(&store, root.id).entry
                    }
                )
                .is_ok()
        );
        let other = Access::session(id(99), &session.id);
        store
            .set_permissions(
                &access,
                root.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                },
            )
            .unwrap();
        assert!(store.schema(&other, data(&store, root.id)).is_err());
        assert!(matches!(
            store.query_objects(&other, data(&store, root.id), id(1), None, 64),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            store.query_object_fields(&other, data(&store, root.id), id(1), None, 64, &[id(2)]),
            Err(Error::AccessDenied(_))
        ));
        store.provision_version(&access, root.id, &version).unwrap();
        assert!(store.schema(&other, data(&store, root.id)).is_ok());
        assert_eq!(
            store
                .query_object_fields(&other, data(&store, root.id), id(1), None, 64, &[id(2)])
                .unwrap()
                .objects
                .len(),
            1
        );
        assert_eq!(
            store.query_objects(&other, data(&store, root.id), id(1), None, 64).unwrap().objects,
            vec![object]
        );
        assert!(store.establish_schema(&other, data(&store, root.id), &schema()).is_err());
        assert!(store.revoke(&other, session.root_volume, access.account, "no").is_err());
        store.revoke(&access, session.root_volume, id(99), "removed").unwrap();
        assert!(store.schema(&other, data(&store, root.id)).is_err());
        assert!(matches!(
            store.query_objects(&other, data(&store, root.id), id(1), None, 64),
            Err(Error::ParticipantRevoked { volume, account })
                if volume == session.root_volume && account == other.account
        ));
        assert!(matches!(
            store.query_object_fields(&other, data(&store, root.id), id(1), None, 64, &[id(2)]),
            Err(Error::ParticipantRevoked { volume, account })
                if volume == session.root_volume && account == other.account
        ));
    }
    #[test]
    fn query_pagination_rechecks_entry_permissions() {
        let (_dir, store, volume, owner) = setup();
        let address = data(&store, volume.id);
        assert!(store.query_objects(&owner, address, id(1), None, 64).unwrap().objects.is_empty());
        assert!(matches!(
            store.query_object_fields(&owner, address, id(1), None, 64, &[id(2), id(2)]),
            Err(Error::InvalidInput(_))
        ));
        for limit in [0, 65] {
            assert!(matches!(
                store.query_objects(&owner, address, id(1), None, limit),
                Err(Error::LimitExceeded)
            ));
        }
        let mut records = ["a", "b", "c"].map(|text| {
            store.create_object_with_fields(&owner, address, id(1), &title(text)).unwrap()
        });
        records.sort_by_key(|record| record.object_id);
        let first = store.query_objects(&owner, address, id(1), None, 1).unwrap();
        assert_eq!(first.objects, records[..1]);
        assert_eq!(first.next, Some(records[0].object_id));
        let second = store.query_objects(&owner, address, id(1), first.next, 1).unwrap();
        assert_eq!(second.objects, records[1..2]);
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                address.entry,
                EntryAcl::Replace(Acl::default()),
            )
            .unwrap();
        assert!(matches!(
            store.query_objects(&owner, address, id(1), second.next, 1),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            store.query_object_fields(&owner, address, id(1), None, 64, &[id(2)]),
            Err(Error::AccessDenied(_))
        ));
        store.set_entry_permissions(&owner, volume.id, address.entry, EntryAcl::Inherit).unwrap();
        assert_eq!(store.query_objects(&owner, address, id(1), None, 64).unwrap().objects, records);
        assert_eq!(
            store.query_object_fields(&owner, address, id(1), None, 64, &[id(2)]).unwrap().objects,
            records
        );
    }

    #[test]
    fn queries_recheck_personal_volume_consent_for_existing_access() {
        let (_dir, store, root, owner) = setup();
        let personal = store.create_volume("Personal", false).unwrap();
        store
            .create_database(
                &owner,
                personal.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
            )
            .unwrap();
        let object = store
            .create_object_with_fields(&owner, data(&store, personal.id), id(1), &title("personal"))
            .unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let access = Access::session_version(owner.account, &session.id, &version);
        assert!(matches!(
            store.query_objects(&access, data(&store, personal.id), id(1), None, 64),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            store.query_object_fields(
                &access,
                data(&store, personal.id),
                id(1),
                None,
                64,
                &[id(2)]
            ),
            Err(Error::AccessDenied(_))
        ));
        store
            .select_personal_volumes(
                &session.id,
                vec![PersonalVolumeGrant { volume: personal.id, write: false }],
            )
            .unwrap();
        assert_eq!(
            store
                .query_object_fields(&access, data(&store, personal.id), id(1), None, 64, &[id(2)])
                .unwrap()
                .objects
                .len(),
            1
        );
        assert_eq!(
            store
                .query_objects(&access, data(&store, personal.id), id(1), None, 64)
                .unwrap()
                .objects,
            vec![object]
        );
        store.select_personal_volumes(&session.id, vec![]).unwrap();
        assert!(matches!(
            store.query_objects(&access, data(&store, personal.id), id(1), None, 64),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            store.query_object_fields(
                &access,
                data(&store, personal.id),
                id(1),
                None,
                64,
                &[id(2)]
            ),
            Err(Error::AccessDenied(_))
        ));
    }
    #[test]
    fn append_cannot_read_or_replace_and_entry_override_replaces_owner_access() {
        let (_dir, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let other = Access::owner(id(99));
        store
            .set_permissions(
                &access,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Append],
                    }],
                },
            )
            .unwrap();
        let object =
            store.create_object_with_fields(&other, address, id(1), &title("request")).unwrap();
        assert!(store.read_object(&other, address, object.object_id).is_err());
        assert!(store.edit_fields(&other, address, object.object_id, &title("overwrite")).is_err());
        store
            .set_entry_permissions(
                &access,
                volume.id,
                address.entry,
                EntryAcl::Replace(Acl::default()),
            )
            .unwrap();
        assert!(store.read_object(&access, address, object.object_id).is_err());
    }

    #[test]
    fn blobs_deduplicate_but_references_authorize_and_unsynced_bytes_survive_eviction() {
        let (dir, store, first, access) = setup();
        let second = store.create_volume("Second", false).unwrap();
        let a = blob(&store, &access, first.id, "a", b"shared bytes");
        let b = blob(&store, &access, second.id, "b", b"shared bytes");
        assert_eq!(fs::read_dir(dir.path().join(crate::paths::BLOBS)).unwrap().count(), 1);
        assert_ne!(a.encrypted_key, b.encrypted_key);
        assert_ne!(
            store.credentials(first.id).unwrap().database_key,
            store.credentials(first.id).unwrap().blob_key
        );
        assert!(store.read_blob_version(&Access::owner(id(99)), &a).is_err());
        assert_eq!(store.evict_blobs(0).unwrap(), 12);
        assert_eq!(store.read_blob_version(&access, &b).unwrap(), b"shared bytes");
        let object = store.create_object(&access, data(&store, first.id), id(1)).unwrap();
        // A cross-volume or dangling reference is an address, never a content grant.
        let reference = BTreeMap::from([(
            id(4),
            Some(vec![Value::File { volume_id: second.id, entry_id: b.address.entry }]),
        )]);
        store.edit_fields(&access, data(&store, first.id), object.object_id, &reference).unwrap();
        store.remove_object(&access, data(&store, first.id), object.object_id).unwrap();
        assert_eq!(store.read_blob_version(&access, &b).unwrap(), b"shared bytes");
    }

    #[test]
    fn sending_an_outbound_projection_leaves_the_store_open_for_local_work() {
        use std::sync::{Arc, mpsc};
        let (_dir, store, volume, access) = setup();
        let store = Arc::new(store);
        let object = store
            .create_object_with_fields(&access, data(&store, volume.id), id(1), &title("local"))
            .unwrap()
            .object_id;
        let (sender, receiver) = mpsc::channel();
        let observed = store
            .with_outbound_replica(
                volume.id,
                crate::store::DatabaseId::Entry(data(&store, volume.id).entry),
                ReplicaReader::Service,
                |projected| {
                    // Stands in for the network transfer a push performs here.
                    let concurrent = store.clone();
                    let writer = access.clone();
                    let id = volume.id;
                    std::thread::spawn(move || {
                        let edit = title("written during the transfer");
                        let _ = sender.send(concurrent.edit_fields(
                            &writer,
                            data(&concurrent, id),
                            object,
                            &edit,
                        ));
                    });
                    assert!(projected.begin_read()?.root_hash()?.is_some());
                    // Bounded: a still-locked store reports a timeout, never a hang.
                    Ok(receiver.recv_timeout(std::time::Duration::from_secs(5)))
                },
            )
            .unwrap();
        assert!(observed.expect("local writes must not wait for an outbound transfer").is_ok());
    }
    #[test]
    fn replaced_and_rejected_blob_versions_retain_recovery_pins() {
        let (dir, store, volume, access) = setup();
        let first = blob(&store, &access, volume.id, "file", b"first");
        let changed = store.write_blob(&access, volume.id, first.address.entry, b"second").unwrap();
        let second = store.blob_version(&access, first.address, None).unwrap();
        assert_ne!(first.revision, changed.content_revision);
        assert_eq!(store.read_blob_version(&access, &first).unwrap(), b"first");
        assert_eq!(store.read_blob_version(&access, &second).unwrap(), b"second");
        assert_eq!(store.evict_blobs(0).unwrap(), 11);
        let before = store.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
        let database = data(&store, volume.id);
        assert!(store.write_blob(&access, volume.id, database.entry, b"rejected").is_err());
        assert_eq!(store.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path().join(crate::paths::BLOBS)).unwrap().count(), 2);
        // Reject the wrong entry kind before preparing any content.
        assert_eq!(store.read_blob_version(&access, &second).unwrap(), b"second");
    }

    #[test]
    fn cache_files_no_reference_names_are_evicted_before_pinned_pending_bytes() {
        let (dir, store, volume, access) = setup();
        let version = blob(&store, &access, volume.id, "pending", b"pending bytes");
        let blobs = dir.path().join(crate::paths::BLOBS);
        let cache = store.blob_cache_path.join("00");
        fs::create_dir_all(&cache).unwrap();
        // Exceed the candidate page so eviction must finish multiple bounded scans.
        for n in 0..130u64 {
            fs::write(cache.join(format!("{n:064x}")), b"orphan").unwrap();
        }
        assert_eq!(store.set_cache_budget(0).unwrap(), 13);
        assert_eq!(fs::read_dir(&blobs).unwrap().count(), 1);
        assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"pending bytes");
    }

    #[test]
    fn an_interrupted_file_write_keeps_its_pending_upload_across_a_restart() {
        let (dir, store, volume, access) = setup();
        let original = blob(&store, &access, volume.id, "file", b"first");
        *store.entry_blobs.before_publish.lock().unwrap() =
            Some(Arc::new(|| Err(Error::System("injected before index commit".into()))));
        assert!(store.write_blob(&access, volume.id, original.address.entry, b"second").is_err());
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(store.blob_version(&access, original.address, None).unwrap(), original);
        assert_eq!(
            store.pending_entry_blobs(&access, volume.id, None).unwrap().versions,
            vec![original.clone()]
        );
        assert_eq!(store.evict_blobs(0).unwrap(), 11);
        assert_eq!(store.read_blob_version(&access, &original).unwrap(), b"first");
        assert_eq!(
            fs::read(store.blob_path(blake3::hash(b"second").as_bytes(), false)).unwrap(),
            b"second"
        );
    }

    #[test]
    fn pending_upload_retries_keep_identity_and_ack_matches_exact_operation() {
        let (dir, store, _volume, _access) = setup();
        let bundle = dir.path().join("app.bundle");
        fs::write(&bundle, b"wasm").unwrap();
        let build = crate::identifier::encode(&[0xab; 32]);
        let first = store.queue_build_upload(&build, &bundle).unwrap().unwrap();
        let second = store.queue_build_upload(&build, &bundle).unwrap().unwrap();
        assert_eq!(first, second);
        let mut incorrect = first.clone();
        incorrect.operation = PendingKind::UploadBuild {
            build_id: crate::identifier::encode(&[0xab; 32]),
            path: bundle.clone(),
        };
        assert!(store.acknowledge_pending(&incorrect).is_err());
        store.acknowledge_pending(&first).unwrap();
        assert_eq!(store.pending_operations().unwrap().len(), 1);
        assert!(store.queue_build_upload(&build, &bundle).unwrap().is_none());
        drop(store);
        let reopened = LocalStore::open(dir.path()).unwrap();
        assert!(reopened.queue_build_upload(&build, &bundle).unwrap().is_none());
    }
    #[test]
    fn four_independent_sessions_reuse_authorization_and_data_databases() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let owner = Access::owner(store.account.id);
        let version = crate::identifier::encode(&[0xab; 32]);
        let mut fixtures = Vec::new();
        for _ in 0..4 {
            let root = store.create_volume("Independent TODO", false).unwrap();
            store
                .create_database(
                    &owner,
                    root.id,
                    "data",
                    crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
                )
                .unwrap();
            let object = store
                .create_object_with_fields(&owner, data(&store, root.id), id(1), &title("before"))
                .unwrap();
            let session = store
                .create_session(
                    &crate::package::AppIdentity {
                        publisher: store.account().id,
                        app_id: "test".into(),
                    },
                    BuildSelector::Exact { build_id: version.clone() },
                    &version,
                    root.id,
                )
                .unwrap();
            fixtures.push((root.id, object.object_id, session));
        }
        let mut handles = Vec::new();
        for (root, object, session) in &fixtures {
            store
                .read_object(
                    &Access::session(store.account.id, &session.id),
                    data(&store, *root),
                    *object,
                )
                .unwrap();
            for (volume, database) in [
                (*root, crate::store::DatabaseId::Index),
                (*root, crate::store::DatabaseId::Entry(data(&store, *root).entry)),
                (session.root_volume, crate::store::DatabaseId::Revocations),
            ] {
                handles.push((
                    (volume, database),
                    Arc::downgrade(&store.db(volume, database).unwrap()),
                ));
            }
        }
        for _ in 0..3 {
            for (root, object, session) in &fixtures {
                let access = Access::session(store.account.id, &session.id);
                store.edit_fields(&access, data(&store, *root), *object, &title("after")).unwrap();
                assert_eq!(
                    store.read_object(&access, data(&store, *root), *object).unwrap().fields
                        [&id(2)]
                        .values,
                    Some(vec![Value::Text("after".into())])
                );
            }
            for ((volume, database), retained) in &handles {
                let current = store.db(*volume, *database).unwrap();
                assert!(
                    retained.ptr_eq(&Arc::downgrade(&current)),
                    "interactive working sets must not close and reopen between app edits"
                );
            }
        }
    }

    #[test]
    fn active_handles_cannot_exceed_the_cache_bound_and_release_restores_progress() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volumes: Vec<_> = (0..=OPEN_DATABASE_LIMIT / 2)
            .map(|_| store.create_volume("Capacity", false).unwrap())
            .collect();
        let mut held = Vec::new();
        for volume in &volumes[..OPEN_DATABASE_LIMIT / 2] {
            held.push(store.db(volume.id, crate::store::DatabaseId::Index).unwrap());
            held.push(store.db(volume.id, crate::store::DatabaseId::Revocations).unwrap());
        }
        assert!(matches!(
            store.db(volumes[OPEN_DATABASE_LIMIT / 2].id, crate::store::DatabaseId::Index),
            Err(Error::LocalContention(_))
        ));
        assert_eq!(store.open.lock().unwrap().by_id.len(), OPEN_DATABASE_LIMIT);
        held.pop();
        let admitted =
            store.db(volumes[OPEN_DATABASE_LIMIT / 2].id, crate::store::DatabaseId::Index).unwrap();
        assert_eq!(store.open.lock().unwrap().by_id.len(), OPEN_DATABASE_LIMIT);
        assert!(Arc::ptr_eq(
            &admitted,
            &store
                .db(volumes[OPEN_DATABASE_LIMIT / 2].id, crate::store::DatabaseId::Index)
                .unwrap()
        ));
    }

    #[test]
    fn cache_churn_releases_evicted_backend_and_preserves_committed_data() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volumes: Vec<_> = (0..=OPEN_DATABASE_LIMIT / 2)
            .map(|_| store.create_volume("Churn", false).unwrap())
            .collect();
        let replacement_id = volumes[OPEN_DATABASE_LIMIT / 2].id;
        let replacement = store.db(replacement_id, crate::store::DatabaseId::Revocations).unwrap();
        let mut tx = replacement.begin_write(now_unix_secs()).unwrap();
        tx.put(b"churn-probe", b"replacement-preserved").unwrap();
        tx.commit().unwrap();
        drop(replacement);
        let mut held = Vec::new();
        for volume in &volumes[..OPEN_DATABASE_LIMIT / 2] {
            held.push(store.db(volume.id, crate::store::DatabaseId::Index).unwrap());
            held.push(store.db(volume.id, crate::store::DatabaseId::Revocations).unwrap());
        }
        assert!(!store.open.lock().unwrap().by_id.contains_key(&DatabaseKey::Volume(
            replacement_id,
            crate::store::DatabaseId::Revocations
        )));
        let old_id = volumes[0].id;
        let old = held.remove(1); // Precisely this backend becomes eligible for eviction.
        let weak_old = Arc::downgrade(&old);
        let mut tx = old.begin_write(now_unix_secs()).unwrap();
        tx.put(b"churn-probe", b"old-committed").unwrap();
        tx.commit().unwrap();
        drop(old);
        let replacement = store.db(replacement_id, crate::store::DatabaseId::Revocations).unwrap();
        assert!(weak_old.upgrade().is_none(), "old cached backend was actually evicted");
        assert_eq!(
            &*replacement.begin_read().unwrap().get(b"churn-probe").unwrap().unwrap(),
            b"replacement-preserved"
        );
        drop(replacement);
        let old = store.db(old_id, crate::store::DatabaseId::Revocations).unwrap();
        assert_eq!(
            &*old.begin_read().unwrap().get(b"churn-probe").unwrap().unwrap(),
            b"old-committed"
        );
        assert_eq!(store.open.lock().unwrap().by_id.len(), OPEN_DATABASE_LIMIT);
    }

    #[test]
    fn a_database_stays_open_between_reads_but_not_across_a_replacement() {
        let (directory, store, root, access) = setup();
        let object = store
            .create_object_with_fields(&access, data(&store, root.id), id(1), &title("kept"))
            .unwrap();
        store.read_object(&access, data(&store, root.id), object.object_id).unwrap();
        let first = Arc::downgrade(
            &store
                .db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                .unwrap(),
        );
        store.read_object(&access, data(&store, root.id), object.object_id).unwrap();
        assert!(
            first.ptr_eq(&Arc::downgrade(
                &store
                    .db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                    .unwrap()
            )),
            "a later read should use the database that is already open"
        );

        store
            .with_replica(
                root.id,
                crate::store::DatabaseId::Entry(data(&store, root.id).entry),
                |_| Ok(()),
            )
            .unwrap();
        assert!(
            first.ptr_eq(&Arc::downgrade(
                &store
                    .db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                    .unwrap()
            )),
            "replica access does not replace the file or its active handle"
        );
        // Only the fault fixture replaces a file; normal replica writes reuse it.
        store
            .close_db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
            .unwrap();
        let path = directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(root.id.to_string())
            .join(crate::store::DatabaseId::Entry(data(&store, root.id).entry).file_name());
        let replacement = directory.path().join("replacement.kvdb");
        fs::copy(&path, &replacement).unwrap();
        fs::rename(&replacement, &path).unwrap();
        // A Weak retains allocation identity without keeping redb open. A raw
        // pointer could compare equal after the allocator reuses freed memory.
        assert!(first.upgrade().is_none(), "replacement must release the old database");
        let reopened = Arc::downgrade(
            &store
                .db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                .unwrap(),
        );
        assert!(!reopened.ptr_eq(&first), "a database the store replaced should be opened again");
        store.read_object(&access, data(&store, root.id), object.object_id).unwrap();
        assert!(
            reopened.ptr_eq(&Arc::downgrade(
                &store
                    .db(root.id, crate::store::DatabaseId::Entry(data(&store, root.id).entry))
                    .unwrap()
            )),
            "and the replacement is then kept open in turn"
        );
    }
    #[test]
    fn missing_registered_named_backing_is_not_recreated_or_read_as_empty() {
        let (directory, store, volume, access) = setup();
        let address = data(&store, volume.id);
        let object =
            store.create_object_with_fields(&access, address, id(1), &title("retained")).unwrap();
        let identity = crate::store::DatabaseId::Entry(address.entry);
        store.close_db(volume.id, identity).unwrap();
        let path = directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(volume.id.to_string())
            .join(identity.file_name());
        fs::remove_file(&path).unwrap();
        assert!(store.read_object(&access, address, object.object_id).is_err());
        assert!(store.query_objects(&access, address, id(1), None, 10).is_err());
        assert!(
            store.edit_fields(&access, address, object.object_id, &title("overwrite")).is_err()
        );
        assert!(store.create_object(&access, address, id(1)).is_err());
        assert!(!path.exists());
        assert_eq!(store.entry(&access, volume.id, address.entry).unwrap().info.name, "data");
    }

    #[test]
    fn missing_metadata_and_revocations_fail_without_recreation() {
        let (directory, store, root, access) = setup();
        let address = data(&store, root.id);
        store.close_db(root.id, crate::store::DatabaseId::Index).unwrap();
        let metadata = directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(root.id.to_string())
            .join(crate::store::DatabaseId::Index.file_name());
        fs::remove_file(&metadata).unwrap();
        assert!(store.schema(&access, address).is_err());
        assert!(store.establish_schema(&access, address, &schema()).is_err());
        assert!(!metadata.exists());
        let root = store.create_volume("Other root", true).unwrap();
        let entry = store
            .create_database(
                &access,
                root.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
            )
            .unwrap();
        let address = crate::store::volume_index::EntryAddress { volume: root.id, entry: entry.id };
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let guest = Access::session(access.account, &session.id);
        store.close_db(session.root_volume, crate::store::DatabaseId::Revocations).unwrap();
        let revocations = directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(session.root_volume.to_string())
            .join(crate::store::DatabaseId::Revocations.file_name());
        fs::remove_file(&revocations).unwrap();
        assert!(store.schema(&guest, address).is_err());
        assert!(store.revoke(&access, session.root_volume, id(99), "removed").is_err());
        assert!(!revocations.exists());
    }

    #[test]
    fn missing_or_mismatched_object_marker_is_corruption() {
        let (_directory, store, volume, access) = setup();
        let object = store
            .create_object_with_fields(&access, data(&store, volume.id), id(1), &title("retained"))
            .unwrap();
        let marker = crate::store::object_database::creation_key(object.object_id);
        let db = store
            .db(volume.id, crate::store::DatabaseId::Entry(data(&store, volume.id).entry))
            .unwrap();
        let mut tx = db.begin_write(now_unix_secs()).unwrap();
        tx.del(marker.as_slice()).unwrap();
        tx.commit().unwrap();
        drop(db);
        assert!(matches!(
            store.read_object(&access, data(&store, volume.id), object.object_id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            store.edit_fields(
                &access,
                data(&store, volume.id),
                object.object_id,
                &title("overwrite")
            ),
            Err(Error::NotFound(_))
        ));
        let db = store
            .db(volume.id, crate::store::DatabaseId::Entry(data(&store, volume.id).entry))
            .unwrap();
        put_source(&db, marker.as_slice(), &id(99)).unwrap();
        drop(db);
        assert!(matches!(
            store.read_object(&access, data(&store, volume.id), object.object_id),
            Err(Error::Corrupt(_))
        ));
    }
    #[test]
    fn repeated_named_blob_reads_preserve_metadata_and_durable_pins() {
        for durable in [false, true] {
            let (directory, store, root, access) = setup();
            let version = blob(&store, &access, root.id, "file", b"retained bytes");
            if durable {
                let sealed = encrypted_bytes(&store, &access, &version).unwrap();
                store.receive_blob_version(&access, &version, &mut &sealed[..], true).unwrap();
            }
            let pending = store.pending_entry_blobs(&access, root.id, None).unwrap();
            assert!(pending.next.is_none());
            let profile = directory.path().join(crate::paths::PROFILE_DB);
            let index = directory
                .path()
                .join(crate::paths::VOLUMES)
                .join(root.id.to_string())
                .join(crate::store::DatabaseId::Index.file_name());
            let profile_bytes = fs::read(&profile).unwrap();
            let index_bytes = fs::read(&index).unwrap();
            // Reads verify the immutable named version and current ACL without
            // an access-time writer/fsync. Both source files stay byte-identical.
            for _ in 0..4 {
                assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"retained bytes");
                let verified = store.verify_blob_version(&access, &version).unwrap();
                assert_eq!(verified.read_range(0, 64).unwrap(), b"retained bytes");
                assert_eq!(fs::read(&profile).unwrap(), profile_bytes);
                assert_eq!(fs::read(&index).unwrap(), index_bytes);
                let current = store.pending_entry_blobs(&access, root.id, None).unwrap();
                assert_eq!(current.versions, pending.versions);
                assert_eq!(current.next, pending.next);
                assert_eq!(store.blob_version_durable(&access, &version).unwrap(), durable);
            }
        }
    }
    #[test]
    fn a_durable_mark_covers_only_the_envelope_that_was_stored() {
        let (_dir, store, root, access) = setup();
        let version = blob(&store, &access, root.id, "file", b"stored once");
        let sealed = encrypted_bytes(&store, &access, &version).unwrap();
        store.receive_blob_version(&access, &version, &mut &sealed[..], false).unwrap();
        assert!(store.has_pending_blob_uploads().unwrap());
        assert!(!store.blob_version_durable(&access, &version).unwrap());
        let mut wrong = version.clone();
        wrong.encrypted_key = [7; 32];
        assert!(store.acknowledge_blob_version(&access, &wrong).is_err());
        assert!(!store.blob_version_durable(&access, &version).unwrap());
        store.acknowledge_blob_version(&access, &version).unwrap();
        assert!(!store.has_pending_blob_uploads().unwrap());
        assert!(store.pending_entry_blobs(&access, root.id, None).unwrap().versions.is_empty());
        assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"stored once");
    }

    #[test]
    fn blob_sealing_matches_the_shared_web_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/blob-v1.json")).unwrap();
        let bytes = |name: &str| {
            let text = fixture[name].as_str().unwrap();
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
                .collect::<Vec<u8>>()
        };
        let key: [u8; 32] = bytes("key").try_into().unwrap();
        let volume = Id::parse(fixture["volume"].as_str().unwrap()).unwrap();
        let plaintext = bytes("plaintext");
        let digest = *blake3::hash(&plaintext).as_bytes();
        assert_eq!(digest.to_vec(), bytes("digest"));
        assert_eq!(
            crate::store::blob_content_key(&key, volume, &digest).to_vec(),
            bytes("cipher_key")
        );
        assert_eq!(crate::store::blob_block_nonce(0).to_vec(), bytes("nonce"));
        assert_eq!(
            crate::store::blob_block_aad(volume, &digest, plaintext.len() as u64, 0).to_vec(),
            bytes("aad")
        );
        let envelope = sealed_bytes(&key, volume, &digest, &plaintext).unwrap();
        assert_eq!(envelope, bytes("envelope"));
        assert_eq!(sealed_bytes(&key, volume, &digest, &plaintext).unwrap(), envelope);
        assert_eq!(blake3::hash(&envelope).as_bytes().to_vec(), bytes("encrypted"));
    }
    #[test]
    fn removing_cache_and_restarting_preserves_pending_bytes_and_requires_authorized_refetch() {
        let (directory, store, volume, access) = setup();
        let pending = blob(&store, &access, volume.id, "pending", b"offline pending");
        let stored = blob(&store, &access, volume.id, "stored", b"recoverable");
        let envelope = encrypted_bytes(&store, &access, &stored).unwrap();
        store.acknowledge_blob_version(&access, &stored).unwrap();
        fs::create_dir_all(&store.blob_cache_path).unwrap();
        fs::remove_dir_all(&store.blob_cache_path).unwrap();
        let account = store.account.id;
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.account.id, account);
        assert_eq!(store.read_blob_version(&access, &pending).unwrap(), b"offline pending");
        assert!(matches!(store.read_blob_version(&access, &stored), Err(Error::NotFound(_))));
        assert!(
            store
                .receive_blob_version(&Access::owner(id(99)), &stored, &mut &envelope[..], true)
                .is_err()
        );
        store.receive_blob_version(&access, &stored, &mut &envelope[..], true).unwrap();
        assert_eq!(store.read_blob_version(&access, &stored).unwrap(), b"recoverable");
        assert_eq!(store.evict_blobs(0).unwrap(), 15);
    }

    #[test]
    fn acknowledging_one_volume_never_unpins_another_volumes_shared_plaintext() {
        let (directory, store, first, access) = setup();
        let second = store.create_volume("Second", true).unwrap();
        let a = blob(&store, &access, first.id, "a", b"shared");
        let b = blob(&store, &access, second.id, "b", b"shared");
        store.acknowledge_blob_version(&access, &a).unwrap();
        assert!(!store.blob_version_durable(&access, &b).unwrap());
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.evict_blobs(0).unwrap(), 6);
        assert_eq!(
            store.pending_entry_blobs(&access, second.id, None).unwrap().versions,
            vec![b.clone()]
        );
        store.acknowledge_blob_version(&access, &b).unwrap();
        assert_eq!(store.evict_blobs(0).unwrap(), 0);
        assert!(matches!(store.read_blob_version(&access, &b), Err(Error::NotFound(_))));
    }

    #[test]
    fn receipt_committed_before_failed_cache_promotion_survives_restart() {
        let (directory, store, volume, access) = setup();
        let version = blob(&store, &access, volume.id, "stored", b"recoverable");
        fs::create_dir_all(store.blob_cache_path.parent().unwrap()).unwrap();
        fs::write(&store.blob_cache_path, b"interrupted cache directory").unwrap();
        assert!(store.acknowledge_blob_version(&access, &version).is_err());
        assert!(store.blob_version_durable(&access, &version).unwrap());
        assert!(store.pending_entry_blobs(&access, volume.id, None).unwrap().versions.is_empty());
        fs::remove_file(&store.blob_cache_path).unwrap();
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"recoverable");
        assert_eq!(store.evict_blobs(0).unwrap(), 0);
    }

    #[test]
    fn deleting_cache_during_staging_does_not_delete_prepared_bytes() {
        use std::io::Write;
        let (_directory, store, _volume, _access) = setup();
        let bytes = b"prepared";
        let destination = store.blob_path(blake3::hash(bytes).as_bytes(), true);
        let staged =
            blob_files::BlobStage::new(&store.blob_stage_directory(true).unwrap()).unwrap();
        (&staged.file).write_all(bytes).unwrap();
        staged.sync().unwrap();
        fs::create_dir_all(&store.blob_cache_path).unwrap();
        fs::remove_dir_all(&store.blob_cache_path).unwrap();
        assert!(staged.publish(&destination, None, true).unwrap());
        assert_eq!(fs::read(destination).unwrap(), bytes);
    }

    #[test]
    fn bytes_evicted_after_staging_are_written_again_before_the_reference_is_pinned() {
        use std::io::Write;
        let (directory, store, volume, access) = setup();
        let version = blob(&store, &access, volume.id, "file", b"cached");
        let path = store.blob_path(&version.digest, false);
        let staged =
            blob_files::BlobStage::new(&store.blob_stage_directory(false).unwrap()).unwrap();
        (&staged.file).write_all(b"cached").unwrap();
        staged.sync().unwrap();
        let existing = store.inspect_blob(&version, &path).unwrap();
        assert!(matches!(existing, blob_files::ExistingBlob::Verified(_)));
        fs::remove_file(&path).unwrap();
        assert!(existing.publish(&staged, &path, true).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"cached");
        drop(staged);
        assert_eq!(fs::read_dir(directory.path().join("staging/blobs")).unwrap().count(), 0);
    }

    #[test]
    fn corrupted_cached_blob_cannot_make_a_successful_identical_write() {
        let (_directory, store, volume, access) = setup();
        let original = blob(&store, &access, volume.id, "file", b"original");
        let path = store.blob_path(&original.digest, false);
        fs::write(&path, b"damaged!").unwrap();
        assert!(matches!(store.read_blob_version(&access, &original), Err(Error::Corrupt(_))));
        assert!(matches!(
            store.write_blob(&access, volume.id, original.address.entry, b"original"),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(store.blob_version(&access, original.address, None).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), b"damaged!");
    }

    #[test]
    fn profile_initialization_preserves_unrelated_files_and_reopens_the_same_identity() {
        // A shared directory can hold unrelated state before first sign-in.
        let directory = tempfile::tempdir().unwrap();
        let unrelated = directory.path().join("ap/connection.json");
        fs::create_dir(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, b"unrelated credentials").unwrap();
        fs::write(directory.path().join("notes.txt"), b"keep me").unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account().id;
        assert!(store.volumes().unwrap().is_empty());
        drop(store);
        let reopened = LocalStore::open(directory.path()).unwrap();
        assert_eq!(reopened.account().id, account);
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated credentials");
        assert_eq!(fs::read(directory.path().join("notes.txt")).unwrap(), b"keep me");
    }

    #[test]
    fn missing_profile_metadata_preserves_owned_files_and_dangling_links() {
        // Retained credentials and data paths must fail before a new identity is published.
        for name in ["account.kvdb", "instance.json", "volumes", "staging", "file-io"] {
            for symlink in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let retained = directory.path().join(name);
                if symlink {
                    std::os::unix::fs::symlink("missing", &retained).unwrap();
                } else {
                    fs::write(&retained, b"retained data").unwrap();
                }
                assert!(matches!(LocalStore::open(directory.path()), Err(Error::InvalidInput(_))));
                assert!(!directory.path().join(crate::paths::PROFILE_DB).exists());
                if symlink {
                    assert_eq!(fs::read_link(&retained).unwrap(), Path::new("missing"));
                } else {
                    assert_eq!(fs::read(&retained).unwrap(), b"retained data");
                }
            }
        }
    }

    #[test]
    fn missing_profile_catalog_never_replaces_an_existing_identity() {
        // Losing the catalog must not replace an identity with retained volumes.
        let (directory, store, volume, _access) = setup();
        drop(store);
        let catalog = directory.path().join(crate::paths::PROFILE_DB);
        fs::remove_file(&catalog).unwrap();
        assert!(LocalStore::open(directory.path()).is_err());
        assert!(!catalog.exists());
        assert!(
            directory
                .path()
                .join(crate::paths::VOLUMES)
                .join(volume.id.to_string())
                .join(crate::store::DatabaseId::Index.file_name())
                .exists()
        );
    }
    #[test]
    fn corrupt_database_file_is_reported_as_corrupt_store() {
        // A damaged storage header is an explicit recovery case, not a generic I/O failure.
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join(crate::paths::PROFILE_DB);
        fs::write(&catalog, b"damaged").unwrap();
        match open_replica(&catalog, 2 * 1024 * 1024) {
            Err(error) => {
                assert!(crate::engine::is_corrupt_store_open_error(&error));
                let message = error.to_string();
                assert!(message.contains(&catalog.display().to_string()));
                assert!(
                    message.contains("invalid or truncated")
                        || message.contains("database backend")
                );
            }
            Ok(_) => panic!("damaged database opened"),
        }
        assert_eq!(fs::read(&catalog).unwrap(), b"damaged");
    }
    #[test]
    fn missing_account_record_is_corruption_and_keeps_existing_credentials() {
        let (directory, store, volume, _access) = setup();
        let account = store.account.clone();
        let original_secret =
            store.metadata.begin_read().unwrap().get(b"account-secret").unwrap().unwrap().to_vec();
        let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
        tx.del(b"account").unwrap();
        tx.commit().unwrap();
        drop(store);
        let error =
            LocalStore::open(directory.path()).err().expect("damaged profile fails to open");
        assert!(crate::engine::is_corrupt_store_open_error(&error));
        assert!(error.to_string().contains("account record is missing"));
        assert!(error.to_string().contains("profile.kvdb"));
        let catalog =
            open_replica(&directory.path().join(crate::paths::PROFILE_DB), 2 * 1024 * 1024)
                .unwrap();
        let tx = catalog.begin_read().unwrap();
        assert!(tx.get(b"account").unwrap().is_none());
        assert_eq!(&*tx.get(b"account-secret").unwrap().unwrap(), original_secret);
        drop(tx);
        put(&catalog, b"account", &account).unwrap();
        drop(catalog);
        let restored = LocalStore::open(directory.path()).unwrap();
        assert_eq!(restored.account, account);
        assert_eq!(restored.volume(volume.id).unwrap(), volume);
    }
}
