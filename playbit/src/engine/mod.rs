//! Shared local services and explicit session-instance data authority.
#![deny(clippy::unwrap_used)]
#[cfg(feature = "app-host")]
mod session_id {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        id: &Option<Box<[u8; 32]>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        crate::identifier::optional::serialize(&id.as_deref().copied(), serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Box<[u8; 32]>>, D::Error> {
        crate::identifier::optional::deserialize(deserializer).map(|id| id.map(Box::new))
    }
}
#[cfg(feature = "app-host")]
mod gauges;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod idle;
#[cfg(feature = "app-host")]
pub(crate) mod logs;
#[cfg(feature = "app-host")]
pub use logs::{LogBatch, LogCursor};
#[cfg(feature = "app-host")]
pub mod admission;
#[cfg(feature = "app-host")]
pub(crate) mod background;
#[cfg(feature = "connected-engine")]
mod completion;
#[cfg(feature = "app-host")]
use completion::{Completion, Wake};
#[cfg(feature = "app-host")]
pub mod app_display;
#[cfg(any(feature = "connected-engine", feature = "native-runtime", feature = "wasm-runtime"))]
pub mod blob;
#[cfg(feature = "connected-engine")]
mod bloblog;
#[cfg(all(feature = "wasm-runtime", feature = "local-store"))]
pub(crate) mod compiled_cache;
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub mod data_host;
pub mod database_browser;
#[cfg(test)]
mod database_browser_tests;
#[cfg(feature = "app-host")]
pub(crate) mod developer;
#[cfg(feature = "app-host")]
pub(crate) mod diagnostics;
#[cfg(feature = "app-host")]
pub(crate) mod enrollment;
#[cfg(feature = "app-host")]
pub mod failure;
pub mod ffi;
#[cfg(feature = "file-provider")]
pub mod file_provider;
#[cfg(feature = "app-host")]
pub mod file_service;
#[cfg(any(feature = "app-host", feature = "native-runtime", feature = "wasm-runtime"))]
pub mod files;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod guest_host;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod gui_guest;
#[cfg(all(
    feature = "connected-engine",
    any(feature = "native-runtime", feature = "wasm-runtime")
))]
pub mod host;
#[cfg(feature = "app-host")]
pub mod host_mount;
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub mod in_process_host;
#[cfg(all(target_os = "macos", feature = "process"))]
pub mod installation;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod instance_admission;
#[cfg(feature = "app-host")]
mod invitation;
#[cfg(feature = "app-host")]
pub mod ipc;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod launch;
#[cfg(all(target_os = "macos", feature = "process"))]
pub mod launch_agent;
#[cfg(feature = "native-runtime")]
pub mod native;
#[cfg(feature = "connected-engine")]
pub mod peer;
/// The `playbit-engine` process: serve local requests until a stop signal.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod process;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod process_identity;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(crate) mod process_owner;
pub mod profile;
#[cfg(feature = "app-host")]
pub(crate) mod readiness;
#[cfg(feature = "connected-engine")]
mod recovery;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod recovery_endpoint;
#[cfg(feature = "connected-engine")]
pub mod remote;
#[cfg(feature = "connected-engine")]
pub mod reply;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod restart;
#[cfg(feature = "app-host")]
pub mod service_call;
#[cfg(feature = "app-host")]
pub mod service_events;
#[cfg(feature = "app-host")]
pub mod session_instance;
#[cfg(feature = "app-host")]
pub mod session_presentation;
#[cfg(all(unix, feature = "connected-engine"))]
pub(crate) mod sign_out;
#[cfg(feature = "connected-engine")]
mod sync_progress;
#[cfg(feature = "connected-engine")]
pub mod testing_profile;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub mod tool_client;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod tool_context;
#[cfg(all(not(target_os = "ios"), feature = "process"))]
mod tool_file;
#[cfg(feature = "app-host")]
mod updates;
#[cfg(feature = "app-host")]
pub use background::Shutdown;
#[cfg(feature = "connected-engine")]
mod ap_status;
#[cfg(feature = "connected-engine")]
mod build_download;
#[cfg(feature = "app-host")]
pub mod builds;
#[cfg(feature = "connected-engine")]
mod connection_watch;
#[cfg(feature = "connected-engine")]
mod live;
#[cfg(all(test, feature = "app-host"))]
pub(crate) mod local_owner_test_support;
#[cfg(feature = "app-host")]
mod personal;
#[cfg(feature = "connected-engine")]
mod publisher;
#[cfg(feature = "connected-engine")]
mod replication;
#[cfg(feature = "connected-engine")]
pub mod session_channels;
#[cfg(test)]
#[cfg(feature = "app-host")]
mod update_tests;
#[cfg(feature = "app-host")]
use crate::package::Package;
#[cfg(feature = "app-host")]
use crate::runtime::{ExecutionBackend, Runtime, syscall as rt};
#[cfg(feature = "app-host")]
use crate::store::{Access, BuildSelector, Id, LocalStore, Session};
#[cfg(feature = "app-host")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "app-host")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "app-host")]
use std::{collections::BTreeMap, fs, sync::Arc};

#[cfg(feature = "connected-engine")]
mod cancellation;
#[cfg(feature = "connected-engine")]
pub mod connected;

mod services;
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) mod session_binding;
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
mod session_context;
pub use services::Services;
#[cfg(feature = "app-host")]
mod supervisor;
#[cfg(feature = "app-host")]
pub use supervisor::Engine;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const ERROR_CHAIN_LIMIT: usize = 16;

/// Classify storage corruption separately from other profile open failures.
pub fn is_corrupt_store_open_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    for _ in 0..ERROR_CHAIN_LIMIT {
        let Some(error) = current else { break };
        if matches!(
            error.downcast_ref::<crate::store::Error>(),
            Some(crate::store::Error::Corrupt(_))
                | Some(crate::store::Error::Store(kvdb::Error::Corrupt | kvdb::Error::Version))
        ) {
            return true;
        }
        current = error.source();
    }
    false
}
#[cfg(test)]
#[test]
fn corruption_is_distinct_from_other_profile_open_failures() {
    // Store-open failures select the native critical alert without deleting data.
    assert!(is_corrupt_store_open_error(&crate::store::Error::corrupt("invalid fixture")));
    assert!(is_corrupt_store_open_error(&crate::store::Error::Store(kvdb::Error::Corrupt)));
    assert!(is_corrupt_store_open_error(&crate::store::Error::Store(kvdb::Error::Version)));
    assert!(!is_corrupt_store_open_error(&crate::store::Error::Store(kvdb::Error::Busy)));
    assert!(!is_corrupt_store_open_error(&std::io::Error::other("corrupt database")));
}

/// Selected native profile. Process entries initialize channel ownership with
/// `profile::initialize` or `profile::initialize_at` before starting workers.
pub fn profile_path() -> PathBuf {
    profile::current().0
}

#[cfg(feature = "app-host")]
fn share_volume_key_by_default() -> bool {
    true
}

#[cfg(feature = "app-host")]
#[derive(
    Clone, Debug, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct ChannelDelivery {
    pub account: Id,
    pub sender: Id,
    pub message: Id,
}

/// Infrequent inspector mutation; keep its payload out of every request slot.
#[cfg(feature = "app-host")]
#[derive(
    Clone, Debug, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct KvRenameMutation {
    pub address: crate::store::volume_index::EntryAddress,
    pub old_key: Vec<u8>,
    pub new_key: Vec<u8>,
    pub expected: crate::store::key_value::Root,
    pub value: Vec<u8>,
}

#[cfg(feature = "app-host")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Request {
    UiZoom {
        root: Option<Id>,
        scale: Option<u32>,
    },
    BrowserDrafts {
        account: Id,
        #[rkyv(with = rkyv::with::AsBox)]
        operation: crate::store::browser_drafts::BrowserDraftOperation,
    },
    InstanceInspect {
        request: Option<Id>,
    },
    /// Classify an already stopped exact owner without admitting its data.
    InstanceStopReason {
        context: session_instance::Context,
    },
    InstanceList {
        session: String,
    },
    InstanceConfigurations {
        session: String,
    },
    InstanceConfiguration {
        instance: Id,
    },
    InstanceRestore {
        request: Id,
        incarnation: Id,
        instance: Id,
    },
    InstanceMounts {
        instance: Id,
        expected: Box<[crate::engine::host_mount::HostMount]>,
        mounts: Box<[crate::engine::host_mount::HostMount]>,
    },
    InstanceReserve {
        request: Id,
        incarnation: Id,
        session: String,
        extra_volumes: Box<[crate::store::PersonalVolumeGrant]>,
        host_mounts: Box<[crate::engine::host_mount::HostMount]>,
        restore_host_mounts: bool,
    },
    InstanceLaunchFailed {
        instance: Id,
        incarnation: Id,
    },
    InstanceStartup {
        context: session_instance::Context,
        presentation: Option<u64>,
        phase: session_instance::StartupPhase,
    },
    InstanceShow {
        incarnation: Id,
        instance: Id,
        owner_generation: u64,
    },
    InstanceCompile {
        context: session_instance::Context,
        fuel_limit: Option<std::num::NonZeroU64>,
    },
    InstanceCompiled {
        context: Box<session_instance::Context>,
        #[serde(with = "crate::identifier::serde")]
        wasm: [u8; 32],
        #[serde(with = "crate::identifier::serde")]
        compatibility: [u8; 32],
    },
    InstanceClaim {
        instance: Id,
    },
    InstanceReconstruct {
        context: session_instance::Context,
        target: Box<session_instance::ReconstructionTarget>,
    },
    InstanceChannelsAck {
        context: session_instance::Context,
        delivery: Box<ChannelDelivery>,
    },
    InstanceChannelsClose {
        context: session_instance::Context,
    },
    InstanceEvents {
        context: session_instance::Context,
    },
    FileCall {
        context: session_instance::Context,
        operation: u64,
        request: Box<file_service::Request>,
    },
    Syscall {
        context: service_call::Context,
        operation: u64,
        mutation: u64,
        call: Box<crate::runtime::host::DataCall>,
    },
    ToolContextOpen {
        scope: service_call::ToolScope,
    },
    ToolContextClose {
        context: service_call::Context,
    },
    LocalProtocol,
    Status,
    DecideVolumeInvitation {
        volume: Id,
        accept: bool,
    },
    DeleteVolume {
        volume: Id,
    },
    DuplicateVolume {
        volume: Id,
    },
    Entries {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        after: Option<crate::store::EntryId>,
        limit: u32,
    },
    DirectoryEntries {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        target: crate::store::volume_index::DirectoryTarget,
        cut: Option<u64>,
        after: Option<crate::store::EntryId>,
        limit: u32,
    },
    VolumeDetails {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
    },
    EntryDetails {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: crate::store::EntryId,
    },
    EntryRevisions {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        address: crate::store::volume_index::EntryAddress,
        after: Option<crate::store::volume_index::HistoryCursor>,
        limit: u32,
    },
    RecoverEntry {
        address: crate::store::volume_index::EntryAddress,
        name: String,
        revision: Option<crate::store::EntryId>,
        rejected_cut: Option<Id>,
    },
    RejectedEntries {
        volume: Id,
        after: Option<crate::store::volume_index::RecoveryCursor>,
        limit: u32,
    },
    CreateDatabase {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        name: String,
        kind: Box<crate::store::volume_index::EntryKind>,
    },
    CreateDatabaseIn {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        parent: crate::store::EntryId,
        name: String,
        kind: Box<crate::store::volume_index::EntryKind>,
    },
    RenameEntry {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: crate::store::EntryId,
        /// Destination path relative to the volume root, including explicit moves.
        name: String,
    },
    RemoveEntry {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: crate::store::EntryId,
    },
    EntryPermissions {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: crate::store::EntryId,
        permissions: crate::store::object_contract::EntryAcl,
        /// Device-local remembered grants; absence preserves the previous configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained_permissions: Option<crate::store::object_contract::Acl>,
    },
    BloblogCreate {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        name: String,
    },
    BloblogCreateIn {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        parent: crate::store::EntryId,
        name: String,
    },
    OrderSessions {
        sessions: Vec<String>,
    },
    ForgetSession {
        session: String,
    },
    WaitChanges {
        after: Option<ChangeToken>,
    },
    FileCreate {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        name: String,
        #[rkyv(with = ipc::codec::UnixPath)]
        path: PathBuf,
    },
    FileCreateIn {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        parent: crate::store::EntryId,
        name: String,
        #[rkyv(with = ipc::codec::UnixPath)]
        path: PathBuf,
    },
    FileWrite {
        address: crate::store::volume_index::EntryAddress,
        #[rkyv(with = ipc::codec::UnixPath)]
        path: PathBuf,
    },
    FileRead {
        session: Option<String>,
        address: crate::store::volume_index::EntryAddress,
        #[rkyv(with = ipc::codec::UnixPath)]
        path: PathBuf,
    },
    CacheBudget {
        bytes: u64,
    },
    SetDisplayName {
        name: String,
    },
    Usage {
        volume: Id,
    },
    PayloadSize {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: Option<crate::store::EntryId>,
    },
    PrepareSignIn,
    SavedAccount,
    RetryAccountKeyBackup,
    RequestCode {
        email: String,
    },
    SignIn {
        code: String,
    },
    AcceptAgreement {
        account: Id,
        digest: [u8; 32],
    },
    SignInWithKey {
        email: String,
        account_key: String,
    },
    RecoveryStatus,
    PrepareRecovery,
    SaveRecovery,
    RecoverCredentials {
        secret: String,
    },
    Sync,
    StartSync,
    StartJoin {
        session: String,
    },
    PrepareSession {
        session: String,
    },
    RetrySessionInfo {
        session: String,
    },
    TransferStatus,
    CancelTransfer,
    Disconnect,
    WorkOffline {
        enabled: bool,
    },
    Join {
        invitation: String,
    },
    PreviewInvitation {
        invitation: String,
    },
    WatchInvitation {
        session: String,
    },
    ResumeInvitation {
        session: String,
    },
    PauseInvitation {
        session: String,
    },
    Provision {
        volume: Id,
        version: String,
    },
    Permissions {
        volume: Id,
        acl: crate::store::object_contract::Acl,
    },
    Revoke {
        volume: Id,
        account: Id,
        reason: String,
    },
    GrantVolume {
        volume: Id,
        email: String,
        write: bool,
    },
    LabelVolume {
        volume: Id,
        label: String,
    },
    CreateVolume {
        /// JSON callers opt out per volume; omission shares the key, like pb.
        #[serde(default = "share_volume_key_by_default")]
        share_key_with_service: bool,
        name: String,
    },
    InitializeRoot {
        volume: Id,
        name: String,
        // Keep the infrequent initialization payload out of every queue slot.
        declaration: Box<crate::store::RootInitialization>,
    },
    Schema {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        schema: crate::store::object_contract::Schema,
    },
    CreateObject {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        type_id: Id,
        fields: BTreeMap<Id, Option<Vec<crate::store::object_contract::Value>>>,
    },
    ReadObject {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        object: Id,
    },
    EditObject {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        object: Id,
        fields: BTreeMap<Id, Option<Vec<crate::store::object_contract::Value>>>,
    },
    RemoveObject {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        object: Id,
    },
    Objects {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        database: crate::store::volume_index::EntryAddress,
        type_id: Id,
        after: Option<Id>,
        limit: u32,
    },
    ListApps {
        after: Option<String>,
    },
    GetAppDisplay {
        app: crate::package::AppIdentity,
        build: String,
    },
    SetAppEnabled {
        app: crate::package::AppIdentity,
        enabled: bool,
    },
    DeleteApp {
        app: crate::package::AppIdentity,
    },
    PublishApp {
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
        notes: String,
    },
    UpdateSession {
        session: String,
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
    },
    CreateAppSession {
        app: crate::package::AppIdentity,
        root: Option<Id>,
        user: Option<Id>,
    },
    CreateBuildSession {
        build: String,
        root: Option<Id>,
        user: Option<Id>,
    },
    PersonalVolumes {
        session: String,
    },
    SelectPersonalVolumes {
        session: String,
        grants: Vec<crate::store::PersonalVolumeGrant>,
    },
    CheckUpdates {
        session: String,
    },
    PrepareApp {
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
    },
    InitializeApp {
        app_id: String,
        name: String,
        declaration: Box<crate::store::RootInitialization>,
    },
    AppVolumes {
        app: crate::package::AppIdentity,
    },
    AutomaticVolumes {
        session: String,
    },
    OpenAppVolume {
        app: crate::package::AppIdentity,
        role: crate::package::AutomaticVolumeRole,
    },
    OpenAutomaticVolume {
        session: String,
        role: crate::package::AutomaticVolumeRole,
    },
    SessionReference {
        session: String,
    },
    SignPackage {
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
    },
    AuthenticatePackage {
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
    },
    CreateSession {
        #[rkyv(with = ipc::codec::UnixPath)]
        bundle: PathBuf,
        root: Option<Id>,
        user: Option<Id>,
    },
    Invitation {
        session: String,
    },
    DuplicateEntry {
        address: crate::store::volume_index::EntryAddress,
    },
    /// Rename one component under the entry's current effective parent.
    RenameEntryComponent {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        volume: Id,
        entry: crate::store::EntryId,
        name: String,
    },
    /// Inspector mutation conditional on its complete original KV read cut.
    KvRename {
        #[serde(default, with = "session_id", skip_serializing_if = "Option::is_none")]
        session: Option<Box<[u8; 32]>>,
        mutation: Box<KvRenameMutation>,
    },
    /// Add explicit consent without replacing unrelated current grants.
    AddPersonalVolumes {
        session: String,
        grants: Vec<crate::store::PersonalVolumeGrant>,
    },
    /// First management snapshot of a new desktop process.
    DesktopLaunch,
    /// Keep a desktop link in memory until its authenticated envelope arrives.
    PreviewDesktopInvitation {
        invitation: String,
    },
    /// The desktop received this session's verified preview during this launch.
    RetainDesktopInvitation {
        session: String,
    },
    AppRemovalImpact {
        account: Id,
        app: crate::package::AppIdentity,
    },
    RemoveApp {
        account: Id,
        app: crate::package::AppIdentity,
        confirmed: bool,
        delete_local_data: bool,
    },
    GetAccountDisplayNames {
        account: Id,
        account_ids: Vec<Id>,
    },
}
/// A process-generation cursor for observing local state, never an authority.
#[cfg(feature = "app-host")]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct ChangeToken {
    pub generation: Id,
    pub sequence: u64,
}
/// Local update feedback. Remote AP resolution remains a separate protocol.
#[cfg(feature = "app-host")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    Failed,
    Paused,
}

#[cfg(feature = "app-host")]
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct UpdateStatus {
    pub kind: UpdateKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    pub message: String,
}

/// Presentation owner selected at guest admission. A raw scene callback has no AppKit window.
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
#[derive(Clone, Copy, Default)]
pub enum GuestRenderer {
    #[default]
    Embedded,
    #[cfg(target_os = "macos")]
    AppKit,
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
impl GuestRenderer {
    fn features(self) -> &'static [&'static str] {
        use playbit_api::abi::data::{AUTOMATIC_VOLUMES_FEATURE, SESSION_VOLUME_FEATURE};
        use playbit_api::abi::environment::DISPLAY_NAMES_FEATURE;
        #[cfg(target_os = "macos")]
        {
            use playbit_api::ui;
            const MAC: &[&str] = &[
                // Only an AppKit presentation owner enforces native window constraints.
                ui::WINDOW_SIZE_CONSTRAINTS_FEATURE,
                ui::CANVAS_CLEAR_FEATURE,
                ui::CANVAS_PIXELS_FEATURE,
                playbit_api::abi::input::FEATURE,
                ui::CANVAS_VIEWPORT_FEATURE,
                ui::CANVAS_WINDOW_FEATURE,
                ui::WINDOW_STYLE_FEATURE,
                AUTOMATIC_VOLUMES_FEATURE,
                SESSION_VOLUME_FEATURE,
                DISPLAY_NAMES_FEATURE,
            ];
            match self {
                Self::Embedded => &MAC[1..],
                Self::AppKit => MAC,
            }
        }
        #[cfg(target_os = "ios")]
        {
            use playbit_api::ui;
            &[
                ui::CANVAS_CLEAR_FEATURE,
                ui::CANVAS_PIXELS_FEATURE,
                playbit_api::abi::input::FEATURE,
                AUTOMATIC_VOLUMES_FEATURE,
                SESSION_VOLUME_FEATURE,
                DISPLAY_NAMES_FEATURE,
            ]
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            &[AUTOMATIC_VOLUMES_FEATURE, SESSION_VOLUME_FEATURE, DISPLAY_NAMES_FEATURE]
        }
    }
}

#[cfg(feature = "app-host")]
pub(crate) fn input_event(kind: u32, target: u32, data: Vec<u8>) -> Result<rt::Event> {
    let kind = match kind {
        2 => rt::EventKind::Click,
        3 => rt::EventKind::Submit,
        13 => rt::EventKind::Frame,
        14 => rt::EventKind::TextChanged,
        15 => rt::EventKind::SurfaceChanged,
        22 => rt::EventKind::WindowPointer,
        23 => rt::EventKind::WindowKeyboard,
        24 => rt::EventKind::WindowScroll,
        25 => rt::EventKind::WindowGesture,
        26 => rt::EventKind::WindowGamepad,
        _ => return Err("unsupported input event".into()),
    };
    Ok(rt::Event { object_id: 0, kind, target, data })
}

#[cfg(all(test, feature = "app-host"))]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
pub(crate) mod tests {
    use super::local_owner_test_support::LocalApps;
    use super::*;

    #[test]
    fn entry_permission_json_keeps_absence_and_saves_dormant_grants() {
        // Existing JSON callers omit the new field; the same writable operation saves explicit configuration.
        use crate::store::object_contract::{Acl, EntryAcl, Grant, Permission, Principal, Subject};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let store = &engine.services.store;
        let volume = store.create_volume("Permissions", false).unwrap().id;
        let access = Access::owner(store.account().id);
        let entry = store
            .create_database(
                &access,
                volume,
                "Settings",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap()
            .id;
        let configured = Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(store.account().id)),
                permissions: vec![Permission::Read],
            }],
        };
        let json = serde_json::json!({ "op": "entry-permissions", "volume": volume, "entry": entry, "permissions": EntryAcl::Inherit });
        let request: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), json);
        engine.dispatch_reply(request).unwrap();
        let mut json = json;
        json["retained_permissions"] = serde_json::to_value(&configured).unwrap();
        engine.dispatch_reply(serde_json::from_value(json).unwrap()).unwrap();
        let details = engine.services.store.entry_details(&access, volume, entry).unwrap();
        assert_eq!(details.permissions, EntryAcl::Inherit);
        assert_eq!(details.retained_permissions, Some(configured));
        let mut legacy = serde_json::to_value(details).unwrap();
        legacy.as_object_mut().unwrap().remove("retained_permissions");
        legacy.as_object_mut().unwrap().remove("created_clock");
        let decoded: crate::store::volume_index::EntryDetails =
            serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.retained_permissions, None);
        assert_eq!(decoded.created_clock, 0);
    }

    #[test]
    fn browser_snapshot_refreshes_do_not_create_change_notifications() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let store = &engine.services.store;
        let volume = store.create_volume("Browser", false).unwrap().id;
        let access = Access::owner(store.account().id);
        let entry = store
            .create_database(
                &access,
                volume,
                "notes",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let token = engine.change_token();
        for _ in 0..3 {
            engine.dispatch_reply(Request::VolumeDetails { session: None, volume }).unwrap();
            engine
                .dispatch_reply(Request::Entries { session: None, volume, after: None, limit: 64 })
                .unwrap();
            engine
                .dispatch_reply(Request::DirectoryEntries {
                    session: None,
                    volume,
                    target: crate::store::volume_index::DirectoryTarget::Root,
                    cut: None,
                    after: None,
                    limit: 32,
                })
                .unwrap();
            assert!(
                engine
                    .dispatch_reply(Request::DirectoryEntries {
                        session: None,
                        volume,
                        target: crate::store::volume_index::DirectoryTarget::Root,
                        cut: None,
                        after: Some(entry.id),
                        limit: 32,
                    })
                    .is_err()
            );
            engine
                .dispatch_reply(Request::EntryDetails { session: None, volume, entry: entry.id })
                .unwrap();
            assert!(
                engine
                    .dispatch_reply(Request::EntryDetails {
                        session: None,
                        volume,
                        entry: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                    })
                    .is_err()
            );
            assert_eq!(engine.change_token(), token);
        }
        engine
            .dispatch_reply(Request::RenameEntry {
                session: None,
                volume,
                entry: entry.id,
                name: "work".into(),
            })
            .unwrap();
        assert_ne!(engine.change_token(), token);
    }

    #[test]
    fn startup_status_reports_local_recovery_during_background_sync() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        assert!(serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()["recovery"].is_null());
        engine
            .services
            .store
            .save_recovery_state(&crate::store::RecoveryState {
                secret: [7; 16],
                acknowledged: true,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();
        // The network worker owns Remote; local status must remain available.
        engine.remote = None;
        engine.transfer_result = crate::engine::reply::TransferStatus {
            kind: crate::engine::reply::TransferKind::Running,
            ..Default::default()
        };
        let status = serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap();
        assert_eq!(
            status["recovery"],
            serde_json::json!({"kind":"ready","revision":1,"pending_save":false})
        );
        assert_eq!(status["transfer"]["kind"], "running");
        assert!(!status["recovery"].as_object().unwrap().contains_key("secret"));
    }

    #[test]
    fn leaving_retains_metadata_and_does_not_change_independent_local_owners() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Local list", false).unwrap();
        let a = engine.create_session(button_package(), root.id).unwrap();
        let b = engine.create_session(button_package(), root.id).unwrap();
        let invitation = engine
            .services
            .store
            .invitation(&a.id, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        let mut owners = LocalApps::default();
        let ia = owners.open(&mut engine, &a.id).unwrap();
        let ib = owners.open(&mut engine, &b.id).unwrap();
        owners.settle();
        engine
            .dispatch_reply(Request::OrderSessions { sessions: vec![b.id.clone(), a.id.clone()] })
            .unwrap();
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()["sessions"]
                [0]["id"],
            b.id
        );
        engine.dispatch_reply(Request::ForgetSession { session: a.id.clone() }).unwrap();
        assert!(owners.apps.generation(ia).is_ok());
        owners.input(ib, input_event(2, 1, vec![]).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()["sessions"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            engine
                .services
                .store
                .invitation(&a.id, crate::release_channel::ReleaseChannel::Public)
                .unwrap(),
            invitation
        );
        assert_eq!(engine.services.store.session(&a.id).unwrap(), a);
        assert!(matches!(
            engine.dispatch_reply(Request::PrepareSession { session: a.id.clone() }).unwrap(),
            reply::Reply::SessionPrepared(_)
        ));
        owners.quit(&mut engine, ia).unwrap();
        owners.quit(&mut engine, ib).unwrap();
        assert_eq!(engine.services.store.retained_sessions().unwrap(), [b, a]);
    }

    #[test]
    fn joining_a_known_invitation_restores_hidden_sessions_offline_and_respects_denials() {
        // Reopen real ordinary/local bindings offline without initializing scopes or joining AP.
        use crate::{package::AutomaticVolumeRole, store::AutomaticVolumeState};

        for case in [
            "ordinary",
            "local lazy",
            "local materialized",
            "local override",
            "shared lazy",
            "shared World",
            "shared initialized",
            "shared override",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = matches!(case, "ordinary" | "local override" | "shared override")
                .then(|| engine.services.store.create_volume("Local join", false).unwrap());
            let session = if case == "ordinary" {
                engine.create_session(button_package(), root.as_ref().unwrap().id).unwrap()
            } else {
                let package = button_package();
                let package = if case.starts_with("shared") {
                    Package::create(
                        package.app_id().into(),
                        package.name().into(),
                        package.wasm().unwrap(),
                        BTreeMap::new(),
                        &crate::package::VolumeInitializers::single(
                            AutomaticVolumeRole::Session,
                            package.initializer(AutomaticVolumeRole::Session).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap()
                } else {
                    package
                };
                let package =
                    crate::package::signature::testing::signed(&engine.services.store, package)
                        .unwrap();
                let bundle = directory.path().join("local.pbapp");
                fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
                developer::create_session(
                    &engine.services.store,
                    &engine.runtime,
                    directory.path(),
                    &bundle,
                    root.as_ref().map(|root| root.id),
                    None,
                )
                .unwrap()
            };
            if matches!(case, "local materialized" | "shared World") {
                let volume = engine
                    .services
                    .store
                    .open_automatic_volume(&session.id, AutomaticVolumeRole::Root)
                    .unwrap();
                assert_eq!(volume.id, session.root_volume);
            }
            if case == "shared initialized" {
                let shared = engine
                    .services
                    .store
                    .open_automatic_volume(&session.id, AutomaticVolumeRole::Session)
                    .unwrap();
                assert_eq!(Some(shared.id), session.shared_volume);
            }
            assert_eq!(
                session.shared_volume.is_some(),
                matches!(case, "shared lazy" | "shared World" | "shared initialized")
            );
            let scopes = engine.services.store.automatic_volumes(&session.id).unwrap();
            let root = scopes.iter().find(|scope| scope.role == AutomaticVolumeRole::Root).unwrap();
            assert_eq!(root.volume, session.root_volume);
            assert_eq!(
                root.state,
                if matches!(case, "local lazy" | "shared lazy" | "shared initialized") {
                    AutomaticVolumeState::Lazy
                } else {
                    AutomaticVolumeState::Ready
                }
            );
            let invitation = engine
                .services
                .store
                .session_reference(&session.id)
                .unwrap()
                .link(engine.channel());
            engine.remote = None;

            for hidden in [false, true] {
                if hidden {
                    engine
                        .dispatch_reply(Request::ForgetSession { session: session.id.clone() })
                        .unwrap();
                    assert!(engine.services.store.retained_sessions().unwrap().is_empty());
                }
                let reply = engine
                    .dispatch_reply(Request::Join { invitation: invitation.clone() })
                    .unwrap_or_else(|error| panic!("{case}: {error}"));
                assert!(matches!(reply, reply::Reply::Session(joined) if joined == session));
                assert_eq!(
                    engine.services.store.retained_sessions().unwrap(),
                    std::slice::from_ref(&session)
                );
                let reply::Reply::SessionPrepared(prepared) =
                    engine.prepare_session(&session.id).unwrap()
                else {
                    panic!("retained session prepares locally: {case}");
                };
                assert_eq!(prepared.session, session);
                assert_eq!(engine.services.store.automatic_volumes(&session.id).unwrap(), scopes);
                assert!(engine.services.store.pending_invitations().unwrap().is_empty());
            }

            engine
                .services
                .store
                .record_session_denial(&session, "session participant revoked")
                .unwrap();
            let error = engine.dispatch_reply(Request::Join { invitation }).unwrap_err();
            assert_eq!(error.to_string(), "session participant revoked");
            assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
        }
    }

    #[test]
    fn display_name_waits_for_initialization_but_known_names_can_change_offline() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        engine
            .services
            .store
            .save_recovery_state(&crate::store::RecoveryState {
                secret: [7; 16],
                acknowledged: true,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();
        assert!(
            engine.dispatch_reply(Request::SetDisplayName { name: "New name".into() }).is_err()
        );
        assert_eq!(engine.services.store.display_name().unwrap(), None);
        assert!(engine.services.store.pending_account_record(None).unwrap().is_none());
        // Models an already recovered profile; first-time UI creation must
        // instead wait for the actual account bootstrap boundary above.
        engine.services.store.set_display_name("Recovered name").unwrap();
        engine.remote = None;
        assert_eq!(
            serde_json::to_value(
                engine
                    .dispatch_reply(Request::SetDisplayName { name: "Offline edit".into() })
                    .unwrap()
            )
            .unwrap(),
            serde_json::json!({"display_name":"Offline edit"})
        );
        let status = serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap();
        assert_eq!(status["account_profile"]["display_name"], "Offline edit");
        assert_eq!(status["account_profile"]["initialized"], false);
        assert!(engine.dispatch_reply(Request::SetDisplayName { name: "".into() }).is_err());
        assert_eq!(engine.services.store.display_name().unwrap().as_deref(), Some("Offline edit"));
    }

    #[test]
    fn input_mapping_keeps_text_changes_distinct_from_submission() {
        let event = input_event(14, 7, "New draft".as_bytes().to_vec()).unwrap();
        assert_eq!(event.kind, rt::EventKind::TextChanged);
        assert_eq!(event.target, 7);
        assert_eq!(event.data, b"New draft");
        assert!(input_event(4, 7, vec![]).is_err());
    }

    #[test]
    fn availability_does_not_wait_for_volume_storage() {
        use std::{sync::mpsc, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let volume = engine.services.store.create_volume("Root", true).unwrap();
        let store = engine.services.store.clone();
        let (held, holding) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            store
                .with_replica(volume.id, crate::store::DatabaseId::Index, |_| {
                    held.send(()).unwrap();
                    // Failure deadline releases the fixture even if status regresses
                    // to acquiring the storage lock. This is not a latency benchmark.
                    assert!(released.recv_timeout(Duration::from_secs(2)).is_ok());
                    Ok(())
                })
                .unwrap();
        });
        holding.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(engine.availability().unwrap(), "Offline; local data is retained");
        release.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn default_profile_isolated_from_unrelated_playbit_data() {
        let directory = tempfile::tempdir().unwrap();
        let application_data = directory.path().join("Application Support");
        let unrelated = application_data.join("Playbit");
        fs::create_dir_all(unrelated.join("EFI")).unwrap();
        fs::create_dir_all(unrelated.join("toolchain")).unwrap();
        let existing = [
            ("disk1.qcow2", b"unrelated virtual machine".as_slice()),
            ("EFI/boot", b"unrelated firmware".as_slice()),
            ("toolchain/compiler", b"unrelated toolchain".as_slice()),
            ("server", b"unrelated server".as_slice()),
        ];
        for (path, bytes) in existing {
            fs::write(unrelated.join(path), bytes).unwrap();
        }

        let selection = profile::Selection::open(&application_data, None, None).unwrap();
        let profile = selection.path.clone();
        assert_eq!(profile, application_data.canonicalize().unwrap().join(".playbit"));
        let engine = Engine::open(&profile).unwrap();
        let account = engine.services.store.account().id;
        assert!(engine.services.store.volumes().unwrap().is_empty());
        assert!(profile.join(crate::paths::PROFILE_DB).exists());
        drop(engine);
        assert_eq!(Engine::open(&profile).unwrap().services.store.account().id, account);

        for (path, bytes) in existing {
            assert_eq!(fs::read(unrelated.join(path)).unwrap(), bytes);
        }
        assert_eq!(fs::read_dir(&unrelated).unwrap().count(), 4);
        assert!(!unrelated.join(crate::paths::PROFILE_DB).exists());
    }

    fn package() -> Package {
        Package::new(
            "test".into(),
            "Test".into(),
            wat::parse_str("(module (memory (export \"memory\") 1) (func (export \"pb_main\")))")
                .unwrap(),
            BTreeMap::new(),
        )
        .unwrap()
    }
    pub(crate) fn object_volume(
        store: &crate::store::LocalStore,
        name: &str,
        service: bool,
    ) -> crate::store::Volume {
        let declaration = crate::store::RootInitialization {
            permissions: Default::default(),
            databases: vec![crate::store::InitialDatabase {
                id: crate::store::EntryId::from_bytes([5; 15]).unwrap(),
                name: "data".into(),
                kind: crate::store::InitialDatabaseKind::ObjectDb {
                    schema: Default::default(),
                    objects: vec![],
                },
            }],
            blobs: vec![],
        };
        store.initialize_root(Id::generate().unwrap(), name, service, &declaration).unwrap()
    }
    pub(crate) fn object_address(
        volume: crate::store::Id,
    ) -> crate::store::volume_index::EntryAddress {
        crate::store::volume_index::EntryAddress {
            volume,
            entry: crate::store::EntryId::from_bytes([5; 15]).unwrap(),
        }
    }
    /// An app that draws one button and nothing else.
    pub(crate) fn button_package() -> Package {
        counted_button_package(true)
    }
    pub(crate) fn counted_button_package(drain_events: bool) -> Package {
        let view = {
            let mut view_state = playbit_api::context::ContextState::default();
            let mut view = view_state.view();
            view.column_start();
            view.button(1, "Press", playbit_api::ui::ButtonStyle::Default, false);
            view.column_end();
            view.finish()
        };
        let bytes: String = view.iter().map(|byte| format!("\\{byte:02x}")).collect();
        let wat = format!(
            r#"(module (import "playbit" "pb_syscall" (func $syscall (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
             (func $log (param i32 i32) (result i32)
               (i32.wrap_i64 (call $syscall (i32.const 2) (i64.extend_i32_u (local.get 0))
                 (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
             (func $view (param i32 i32) (result i32)
               (i32.wrap_i64 (call $syscall (i32.const 3) (i64.extend_i32_u (local.get 0))
                 (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
             (func $event (param i32 i32 i32) (result i32)
               (i32.store (local.get 0) (i32.const 24))
               (i32.store offset=4 (local.get 0) (i32.const 0))
               (i32.wrap_i64 (call $syscall (i32.const 4) (i64.extend_i32_u (local.get 0))
                 (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
             (memory (export "memory") 1) (data (i32.const 1024) "{bytes}")
             (data (i32.const 8) "turn")
             (func (export "pb_main") (local $status i32)
               (drop (call $log (i32.const 8) (i32.const 4)))
               (loop $events
                 (local.set $status (call $event (i32.const 8192) (i32.const 8224) (i32.const 1024)))
                 (if (i32.eq (local.get $status) (i32.const -6)) (then return))
                 (drop (call $view (i32.const 1024) (i32.const {len})))
                 {next})))"#,
            len = view.len(),
            next = if drain_events { "(br $events)" } else { "return" },
        );
        Package::new("test".into(), "Button".into(), wat::parse_str(&wat).unwrap(), BTreeMap::new())
            .unwrap()
    }

    #[test]
    fn retained_session_reopens_and_copy_uses_prepared_memory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile");
        let mut engine = Engine::open(&path).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session = engine.create_session(button_package(), root.id).unwrap();
        let mut owners = LocalApps::default();
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        owners.quit(&mut engine, instance).unwrap();
        let invitation = engine.invitation(&session.id).unwrap();
        drop(engine);
        let mut engine = Engine::open(&path).unwrap();
        let mut owners = LocalApps::default();
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        owners.quit(&mut engine, instance).unwrap();
        assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
        // Copy must work even when its former path can no longer be looked up.
        fs::rename(&path, directory.path().join("renamed")).unwrap();
        assert_eq!(engine.invitation(&session.id).unwrap(), invitation);
    }
    #[test]
    fn corrupt_cached_app_cannot_run_with_retained_identity() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session = engine.create_session(package(), root.id).unwrap();
        fs::write(
            directory.path().join(crate::paths::BUILDS).join(&session.resolved_build_id),
            b"corrupt",
        )
        .unwrap();
        assert!(LocalApps::default().open(&mut engine, &session.id).is_err());
        assert!(engine.services.store.session(&session.id).is_ok());
    }
}

#[cfg(all(test, feature = "app-host"))]
mod completion_tests;
