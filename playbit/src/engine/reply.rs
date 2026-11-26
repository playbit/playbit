//! Typed local results. Serde formatting belongs at external JSON boundaries.
use crate::store::Id;

#[cfg(feature = "connected-engine")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum ApConnectionState {
    Live,
    Connecting,
    Offline,
    OfflineMode,
}

/// AP transport health and explicit offline intent; no replication/durability claim.
#[cfg(feature = "connected-engine")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct ApConnectionStatus {
    pub state: ApConnectionState,
    pub hostname: String,
}
#[cfg(feature = "connected-engine")]
impl ApConnectionStatus {
    // DNS name including an optional trailing root dot.
    pub(crate) const HOSTNAME_BYTES_MAX: usize = 254;
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferProgress {
    Connecting { hostname: String },
    Account,
    Registrations { current: usize, total: usize },
    Volumes { current: usize, total: usize },
    Sessions { current: usize, total: usize },
    Files { current: usize, total: usize },
    Finishing,
}
impl TransferProgress {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Connecting { hostname } => format!("Connecting to {hostname}"),
            Self::Account => "Syncing account…".into(),
            Self::Registrations { current, total } => {
                format!("Publishing item {current} of {total}…")
            }
            Self::Volumes { current, total } => format!("Syncing volume {current} of {total}…"),
            Self::Sessions { current, total } => format!("Syncing session {current} of {total}…"),
            Self::Files { current, total } => {
                format!("Checking files in volume {current} of {total}…")
            }
            Self::Finishing => "Finishing synchronization…".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryStatus {
    Ready {
        revision: u64,
        pending_save: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        restored_sessions: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        restored_volumes: Option<usize>,
    },
    Setup {
        revision: u64,
        pending_save: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
    Locked {
        revision: u64,
        pending_save: bool,
    },
    Canceled,
}

#[cfg(feature = "app-host")]
impl RecoveryStatus {
    pub(crate) fn pending_save(&self) -> Option<bool> {
        match self {
            Self::Ready { pending_save, .. }
            | Self::Setup { pending_save, .. }
            | Self::Locked { pending_save, .. } => Some(*pending_save),
            Self::Canceled => None,
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct PreparedRecovery {
    pub secret: String,
}

/// Committed session metadata after local admission/cache preparation. No
/// execution owner, scene, draft or callback is created by this management call.
#[cfg(feature = "app-host")]
#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct PreparedSession {
    pub session: crate::store::Session,
    pub invitation: String,
    pub availability: String,
    pub update: Option<super::UpdateStatus>,
}

#[cfg(feature = "app-host")]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    #[default]
    Idle,
    Running,
    Complete,
    Paused,
    Canceling,
    Failed,
}

#[cfg(feature = "app-host")]
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct TransferStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
    pub kind: TransferKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<super::UpdateStatus>,
}

#[cfg(feature = "app-host")]
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvitationState {
    Paused,
    Checking,
    SignIn,
    WaitingRegistration,
    WaitingApp,
    WaitingDatabase,
    WaitingPeer,
    Ready { build_id: String },
    Denied { message: String },
    Failed { message: String },
}

#[cfg(feature = "app-host")]
impl From<crate::network::ap::operation::InvitationStatus> for InvitationState {
    fn from(status: crate::network::ap::operation::InvitationStatus) -> Self {
        use crate::network::ap::operation::InvitationStatus as Remote;
        match status {
            Remote::WaitingRegistration => Self::WaitingRegistration,
            Remote::WaitingApp => Self::WaitingApp,
            Remote::WaitingDatabase => Self::WaitingDatabase,
            Remote::WaitingPeer => Self::WaitingPeer,
            Remote::Ready { build_id } => Self::Ready { build_id },
            Remote::Denied { message } => Self::Denied { message },
        }
    }
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct InvitationStatus {
    pub session: String,
    pub state: InvitationState,
}

#[cfg(feature = "app-host")]
#[derive(
    Debug, serde::Deserialize, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct AppResponse {
    pub revision: u64,
    pub app: Option<crate::network::ap::operation::PublishedApp>,
}

#[cfg(feature = "app-host")]
#[derive(
    Debug, serde::Deserialize, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct AppPage {
    pub apps: Vec<crate::network::ap::operation::PublishedApp>,
    pub next: Option<String>,
}

#[cfg(feature = "app-host")]
#[derive(
    Debug,
    Default,
    serde::Deserialize,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct AppBuildPage {
    pub builds: Vec<crate::network::ap::operation::AppBuild>,
    pub next: Option<String>,
}

#[cfg(feature = "app-host")]
impl AppBuildPage {
    // The AP pages through build IDs, with at most 128 KiB of build records per page.
    const MAX_BUILDS: usize = 32;
    const MAX_BYTES: usize = crate::network::ap::operation::APP_BUILD_PAGE_BYTES;

    pub(super) fn from_ap(value: serde_json::Value, after: Option<&str>) -> super::Result<Self> {
        let rows = value["builds"].as_array().ok_or("invalid app history page")?;
        // AP charges the encoded rows individually, without array brackets or commas.
        if rows.len() > Self::MAX_BUILDS
            || value.get("next").is_none()
            || serde_json::to_vec(rows)?.len() > Self::MAX_BYTES + 2 + rows.len().saturating_sub(1)
        {
            return Err("invalid app history page".into());
        }
        let page: Self = serde_json::from_value(value)?;
        let rows = page.builds.iter().map(|row| (row.build_id.as_str(), row.notes.as_str()));
        if !Self::valid_rows(rows, after, page.next.as_deref()) {
            return Err("invalid app history records or cursor".into());
        }
        Ok(page)
    }

    fn valid_rows<'a>(
        rows: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
        after: Option<&str>,
        next: Option<&str>,
    ) -> bool {
        let count = rows.len();
        if count > Self::MAX_BUILDS {
            return false;
        }
        let mut prior = after;
        let mut bytes = 0;
        for (id, notes) in rows {
            if !crate::identifier::valid(id, 32)
                || prior.is_some_and(|last| id <= last)
                || !crate::network::ap::operation::valid_app_notes(notes)
            {
                return false;
            }
            bytes += id.len() + notes.len();
            prior = Some(id);
        }
        bytes <= Self::MAX_BYTES && (next.is_none() || count > 0 && next == prior)
    }
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum VolumeAccess {
    #[serde(rename = "r")]
    Read,
    #[serde(rename = "rw")]
    ReadWrite,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[serde(untagged)]
pub enum DeveloperReply {
    AppPrepared,
    PackageIdentity {
        app: crate::package::AppIdentity,
        build_id: String,
        issuer: [u8; 32],
        status: Vec<u8>,
    },
    PackageSigned {
        build_id: String,
    },
    VolumeGranted {
        volume: Id,
        account: Id,
        access: VolumeAccess,
    },
    App(AppResponse),
    Apps(AppPage),
    Display {
        build_id: String,
        name: String,
        icon_error: Option<String>,
    },
    Session(crate::store::Session),
    Updated {
        session: String,
        build_id: String,
        update: Option<super::UpdateStatus>,
        resolved_build_id: Option<String>,
    },
    AccountDisplayNames {
        names: Vec<crate::network::ap::operation::AccountDisplayName>,
    },
    AppBuilds(AppBuildPage),
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ExampleInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub invitation: String,
    pub source: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct AccountProfile {
    pub agreement_required: bool,
    pub replacement_required: bool,
    pub display_name: Option<String>,
    pub unlocked: bool,
    pub initialized: bool,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Status {
    pub startup: Vec<super::session_instance::StartupProgress>,
    pub ap_connection: ApConnectionStatus,
    pub ui_zoom: u32,
    pub account_key_backup: crate::store::AccountKeyBackup,
    pub account: crate::store::Account,
    pub volumes: Vec<crate::store::Volume>,
    pub volume_invitations: Vec<Id>,
    pub volume_invitation_owners: std::collections::BTreeMap<Id, String>,
    /// Sorted IDs of materialized app and session scopes for the desktop catalog.
    #[serde(skip)]
    pub(crate) automatic_volume_ids: Vec<Id>,
    pub examples: Vec<ExampleInfo>,
    pub account_profile: AccountProfile,
    pub sessions: Vec<crate::store::Session>,
    #[serde(skip)]
    pub session_references: std::collections::BTreeMap<String, String>,
    #[serde(skip)]
    pub(crate) session_activity: std::collections::BTreeMap<String, crate::store::SessionActivity>,
    pub imported_apps: Vec<crate::store::ImportedApp>,
    pub app_names: std::collections::BTreeMap<String, String>,
    pub builds: std::collections::BTreeMap<String, super::builds::BuildState>,
    pub recent_sessions: Vec<String>,
    pub updates: std::collections::BTreeMap<String, super::UpdateStatus>,
    pub invitations: Vec<crate::store::InvitationPreview>,
    /// An authenticated preview awaiting acknowledgment from the active desktop.
    #[serde(skip)]
    pub(crate) desktop_invitation: Option<crate::store::InvitationPreview>,
    pub enrolled: bool,
    pub email: Option<String>,
    pub invitation_status: Option<InvitationStatus>,
    pub recovery: Option<RecoveryStatus>,
    pub transfer: TransferStatus,
    pub pending: Vec<crate::store::PendingOperation>,
    pub availability: String,
}

/// Optional form fill, never an authentication result. Debug output omits credentials.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct SavedAccount {
    pub email: String,
    pub account_key: String,
}
impl std::fmt::Debug for SavedAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SavedAccount { .. }")
    }
}
impl Drop for SavedAccount {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.account_key.zeroize();
    }
}

/// Local operation results. The transport checks the variant expected by its caller.
#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ObjectResult {
    pub database: crate::store::volume_index::EntryAddress,
    pub object: crate::store::object_contract::ObjectFields,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ObjectPageResult {
    pub database: crate::store::volume_index::EntryAddress,
    pub type_id: Id,
    pub after: Option<Id>,
    pub page: crate::store::ObjectPage,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct KvRenameReceipt {
    pub address: crate::store::volume_index::EntryAddress,
    pub old_key: Vec<u8>,
    pub new_key: Vec<u8>,
    pub root: crate::store::key_value::Root,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[serde(untagged)]
pub enum Reply {
    WorkOffline {
        enabled: bool,
    },
    UiZoom {
        root: Option<Id>,
        scale: u32,
    },
    BrowserDrafts {
        account: Id,
        #[rkyv(with = rkyv::with::AsBox)]
        result: crate::store::browser_drafts::BrowserDraftResult,
    },
    AccountKeyBackup {
        account_key_backup: crate::store::AccountKeyBackup,
    },
    ToolContext {
        #[rkyv(with = rkyv::with::AsBox)]
        context: super::service_call::Context,
    },
    ToolContextClosed {
        #[rkyv(with = rkyv::with::AsBox)]
        context: super::service_call::Context,
    },
    Syscall(#[rkyv(with = rkyv::with::AsBox)] super::service_call::Outcome),
    FileCall(#[rkyv(with = rkyv::with::AsBox)] super::file_service::Outcome),
    InstanceEvents(#[rkyv(with = rkyv::with::AsBox)] super::service_events::Notice),
    InstanceStatus {
        incarnation: Id,
        #[rkyv(with = rkyv::with::AsBox)]
        instance: Option<super::session_instance::SessionInstance>,
    },
    InstanceStopReason {
        #[rkyv(with = rkyv::with::AsBox)]
        context: super::session_instance::Context,
        incarnation: Id,
        reason: super::service_events::StopReason,
    },
    Instance(#[rkyv(with = rkyv::with::AsBox)] super::session_instance::SessionInstance),
    InstanceShowing {
        instance: Id,
        owner_generation: u64,
        presentation: u64,
    },
    CompiledModule(#[rkyv(with = rkyv::with::AsBox)] Option<crate::runtime::compiled::Descriptor>),
    InstanceAdmission(#[rkyv(with = rkyv::with::AsBox)] super::session_instance::Admission),
    InstanceDrainPending {
        context: super::session_instance::Context,
    },
    InstanceOwners(#[rkyv(with = rkyv::with::AsBox)] Vec<super::session_instance::LaunchOwner>),
    InstanceConfigurations(#[rkyv(with = rkyv::with::AsBox)] Vec<crate::store::Id>),
    Error {
        error: String,
    },
    Changes(super::ChangeToken),
    Recovery(#[rkyv(with = rkyv::with::AsBox)] RecoveryStatus),
    PreparedRecovery(PreparedRecovery),
    Developer(#[rkyv(with = rkyv::with::AsBox)] DeveloperReply),
    Diagnostic(#[rkyv(with = rkyv::with::AsBox)] DiagnosticReply),
    Transfer {
        #[rkyv(with = rkyv::with::AsBox)]
        transfer: TransferStatus,
    },
    PersonalVolumes {
        session: String,
        volumes: Vec<crate::store::Volume>,
        grants: Option<Vec<crate::store::PersonalVolumeGrant>>,
    },
    PersonalVolumesSelected {
        personal_volumes_selected: bool,
    },
    CacheBudget {
        retained_bytes: u64,
    },
    DisplayName {
        display_name: String,
    },
    SavedAccount {
        #[rkyv(with = rkyv::with::AsBox)]
        saved_account: Option<SavedAccount>,
    },
    SignInAvailable {
        sign_in_available: bool,
    },
    CodeSent {
        code_sent: bool,
    },
    SignedIn(#[rkyv(with = rkyv::with::AsBox)] SignInReply),
    AgreementAccepted,
    Availability {
        availability: String,
    },
    Disconnected {
        disconnected: bool,
    },
    Session(#[rkyv(with = rkyv::with::AsBox)] crate::store::Session),
    InvitationPreview(#[rkyv(with = rkyv::with::AsBox)] crate::store::InvitationPreview),
    InvitationStatus {
        #[rkyv(with = rkyv::with::AsBox)]
        invitation_status: Option<InvitationStatus>,
    },
    InvitationPaused {
        paused: bool,
        #[rkyv(with = rkyv::with::AsBox)]
        invitation_status: Option<InvitationStatus>,
    },
    Provisioned {
        provisioned: String,
    },
    Permissions {
        permissions_updated: bool,
    },
    Revoked {
        revoked: Id,
    },
    Protocol {
        protocol: u32,
        version: String,
    },
    Status(Box<Status>),
    Volume(#[rkyv(with = rkyv::with::AsBox)] crate::store::Volume),
    VolumeInvitationDecided {
        volume: Id,
        accept: bool,
    },
    VolumeDeleted {
        volume: Id,
    },
    SessionPrepared(#[rkyv(with = rkyv::with::AsBox)] PreparedSession),
    VolumeEntry {
        volume: Id,
        #[rkyv(with = rkyv::with::AsBox)]
        entry: crate::store::volume_index::VolumeEntry,
    },
    Entries {
        volume: Id,
        after: Option<crate::store::EntryId>,
        #[rkyv(with = rkyv::with::AsBox)]
        page: crate::store::volume_index::EntryPage,
    },
    DirectoryEntries {
        volume: Id,
        target: crate::store::volume_index::DirectoryTarget,
        #[rkyv(with = rkyv::with::AsBox)]
        cut_requested: Option<u64>,
        #[rkyv(with = rkyv::with::AsBox)]
        after: Option<crate::store::EntryId>,
        #[rkyv(with = rkyv::with::AsBox)]
        listing: crate::store::volume_index::DirectoryListing,
    },
    VolumeDetails {
        #[rkyv(with = rkyv::with::AsBox)]
        details: crate::store::VolumeDetails,
    },
    EntryDetails {
        volume: Id,
        #[rkyv(with = rkyv::with::AsBox)]
        details: crate::store::volume_index::EntryDetails,
    },
    EntryRevisions {
        #[rkyv(with = rkyv::with::AsBox)]
        address: crate::store::volume_index::EntryAddress,
        after: Option<crate::store::volume_index::HistoryCursor>,
        page: Box<crate::store::volume_index::HistoryPage>,
    },
    EntryChanged {
        volume: Id,
        entry: crate::store::EntryId,
    },
    EntryPermissions {
        volume: Id,
        entry: crate::store::EntryId,
    },
    Schema {
        database: crate::store::volume_index::EntryAddress,
        ok: bool,
    },
    ObjectCreated(#[rkyv(with = rkyv::with::AsBox)] ObjectResult),
    Object(#[rkyv(with = rkyv::with::AsBox)] ObjectResult),
    Objects(#[rkyv(with = rkyv::with::AsBox)] ObjectPageResult),
    ObjectRemoved {
        database: crate::store::volume_index::EntryAddress,
        object: Id,
    },
    OrderSessions {
        ordered: bool,
    },
    ForgetSession {
        forgotten: String,
    },
    SessionReference {
        reference: crate::store::SessionReference,
    },
    AutomaticVolumes {
        slots: Vec<crate::store::AutomaticVolumeSlot>,
    },
    Invitation {
        invitation: String,
        availability: String,
    },
    KvRenamed {
        #[rkyv(with = rkyv::with::AsBox)]
        receipt: KvRenameReceipt,
    },
    AppRemovalImpact {
        impact: crate::store::AppRemovalImpact,
    },
    AppRemoved(crate::store::AppRemovalResult),
    AppBuildsNotice {
        error: Option<String>,
    },
}
// Small controls share this root. Large payloads live out of line through
// rkyv's AsBox adapter, without changing their owned Rust representation.
// New reply variants must preserve the control-message reservation.
#[cfg(feature = "app-host")]
const _: () = assert!(std::mem::size_of::<ArchivedReply>() <= 64);

#[cfg(feature = "app-host")]
impl ArchivedReply {
    /// The connection supplies correlation; a valid archive of the wrong
    /// result type must never be treated as successful completion.
    pub fn matches_request(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        match request {
            Q::WatchAppBuilds { .. } => matches!(self, Self::AppBuildsNotice { error }
                if error.as_ref().is_none_or(|error| error.len() <= 512)),
            Q::AppRemovalImpact { .. } => matches!(self, Self::AppRemovalImpact { .. }),
            Q::RemoveApp { .. } => matches!(self, Self::AppRemoved(_)),
            Q::WorkOffline { enabled } => {
                matches!(self, Self::WorkOffline { enabled: found } if found == enabled)
            }
            Q::UiZoom { root, scale } => {
                matches!(self, Self::UiZoom { root: found, scale: value }
                    if found.as_ref().map(|id| id.0) == root.map(|id| *id.as_bytes())
                    && crate::store::profile::validate_ui_zoom(value.to_native()).is_ok()
                    && scale.is_none_or(|expected| expected == value.to_native()))
            }
            Q::BrowserDrafts { account, operation } => {
                matches!(self, Self::BrowserDrafts { account: found, result }
                    if found.0 == *account.as_bytes() && result.matches_operation(operation))
            }
            Q::DecideVolumeInvitation { volume, accept } => {
                matches!(self, Self::VolumeInvitationDecided { volume: found, accept: decision }
                    if found.0 == *volume.as_bytes() && decision == accept)
            }
            Q::DeleteVolume { volume } => {
                matches!(self, Self::VolumeDeleted { volume: found } if found.0 == *volume.as_bytes())
            }
            Q::DuplicateVolume { volume } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), ArchivedDiagnosticReply::VolumeDuplicated { source, .. } if source.0 == *volume.as_bytes()))
            }
            Q::DuplicateEntry { address } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), ArchivedDiagnosticReply::EntryDuplicated { source, .. } if matches_address(source, address)))
            }
            Q::PayloadSize { volume, entry, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), ArchivedDiagnosticReply::PayloadSize { volume: found, entry: got, .. }
                    if found.0 == *volume.as_bytes()
                    && got.as_ref().map(|id| id.as_bytes()) == entry.as_ref().map(|id| id.as_bytes())))
            }
            #[rustfmt::skip]
            request @ (Q::Syscall { .. } | Q::FileCall { .. } | Q::ToolContextOpen { .. }
            | Q::ToolContextClose { .. } | Q::InstanceInspect { .. } | Q::InstanceStopReason { .. } | Q::InstanceList { .. }
            | Q::InstanceChannelsAck { .. } | Q::InstanceChannelsClose { .. } | Q::InstanceEvents { .. } | Q::InstanceReserve { .. }
            | Q::InstanceConfigurations { .. } | Q::InstanceConfiguration { .. } | Q::InstanceRestore { .. } | Q::InstanceMounts { .. }
            | Q::InstanceLaunchFailed { .. } | Q::InstanceStartup { .. } | Q::InstanceShow { .. } | Q::InstanceCompile { .. } | Q::InstanceCompiled { .. } | Q::InstanceClaim { .. }
            | Q::InstanceReconstruct { .. }) => {
                self.matches_process(request)
            },
            #[rustfmt::skip]
            request @ (Q::LocalProtocol | Q::Status | Q::DesktopLaunch | Q::WaitChanges { .. }
            | Q::OrderSessions { .. } | Q::ForgetSession { .. } | Q::CacheBudget { .. }
            | Q::SetDisplayName { .. } | Q::AcceptAgreement { .. } | Q::PrepareSignIn | Q::SavedAccount | Q::RetryAccountKeyBackup
            | Q::RequestCode { .. } | Q::SignIn { .. } | Q::SignInWithKey { .. } | Q::Sync
            | Q::StartSync | Q::StartJoin { .. } | Q::TransferStatus | Q::CancelTransfer
            | Q::CheckUpdates { .. } | Q::RetrySessionInfo { .. } | Q::PrepareSession { .. } | Q::Disconnect | Q::Join { .. }
            | Q::PreviewInvitation { .. } | Q::PreviewDesktopInvitation { .. } | Q::RetainDesktopInvitation { .. } | Q::WatchInvitation { .. }
            | Q::ResumeInvitation { .. } | Q::PauseInvitation { .. } | Q::Provision { .. }
            | Q::Permissions { .. } | Q::Revoke { .. } | Q::LabelVolume { .. }
            | Q::CreateVolume { .. } | Q::InitializeRoot { .. } | Q::InitializeApp { .. } | Q::AppVolumes { .. } | Q::AutomaticVolumes { .. } | Q::OpenAppVolume { .. } | Q::OpenAutomaticVolume { .. } | Q::SessionReference { .. } | Q::PersonalVolumes { .. }
            | Q::SelectPersonalVolumes { .. } | Q::AddPersonalVolumes { .. } | Q::Invitation { .. }) => {
                self.matches_control(request)
            },
            #[rustfmt::skip]
            request @ (Q::Entries { .. } | Q::DirectoryEntries { .. } | Q::VolumeDetails { .. } | Q::EntryDetails { .. } | Q::EntryRevisions { .. }
            | Q::RecoverEntry { .. } | Q::RejectedEntries { .. } | Q::CreateDatabase { .. } | Q::CreateDatabaseIn { .. } | Q::RenameEntry { .. } | Q::RenameEntryComponent { .. }
            | Q::RemoveEntry { .. } | Q::EntryPermissions { .. } | Q::BloblogCreate { .. } | Q::BloblogCreateIn { .. } | Q::KvRename { .. }) => {
                self.matches_catalog(request)
            },
            #[rustfmt::skip]
            request @ (Q::FileCreate { .. } | Q::FileCreateIn { .. } | Q::FileWrite { .. } | Q::FileRead { .. }
            | Q::Usage { .. }) => {
                self.matches_file(request)
            },
            #[rustfmt::skip]
            request @ (Q::RecoveryStatus | Q::SaveRecovery
            | Q::RecoverCredentials { .. } | Q::PrepareRecovery) => {
                self.matches_recovery(request)
            },
            #[rustfmt::skip]
            request @ (Q::GrantVolume { .. }
            | Q::SetAppEnabled { .. } | Q::SetAppBuildNotes { .. } | Q::DeleteApp { .. }
            | Q::PublishApp { .. } | Q::ListApps { .. } | Q::ListAppBuilds { .. }
            | Q::GetAppDisplay { .. } | Q::GetAccountDisplayNames { .. } | Q::UpdateSession { .. }
            | Q::CreateAppSession { .. } | Q::CreateBuildSession { .. }
            | Q::PrepareApp { .. } | Q::SignPackage { .. } | Q::AuthenticatePackage { .. } | Q::CreateSession { .. }) => {
                self.matches_developer(request)
            },
            #[rustfmt::skip]
            request @ (Q::Schema { .. } | Q::CreateObject { .. } | Q::ReadObject { .. }
            | Q::EditObject { .. } | Q::RemoveObject { .. } | Q::Objects { .. }) => {
                self.matches_objects(request)
            },
        }
    }

    fn matches_process(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        match request {
            Q::FileCall { context, operation, request } => {
                matches!(self, Self::FileCall(reply) if reply.context.instance.0 == *context.instance.as_bytes()
                    && reply.context.handle.0 == *context.handle.as_bytes()
                    && reply.context.incarnation.0 == *context.incarnation.as_bytes()
                    && reply.context.owner_generation.to_native() == context.owner_generation
                    && reply.operation.to_native() == *operation && reply.kind.to_native() == request.kind())
            }
            Q::Syscall { context, operation, mutation, call } => {
                matches!(self, Self::Syscall(reply) if reply.context.matches(*context) && reply.operation.to_native() == *operation
                && reply.volume.0 == call.volume && reply.mutation.to_native() == *mutation && reply.kind.to_native() == call.operation.kind() as u32)
            }
            Q::ToolContextOpen { .. } => {
                matches!(self, Self::ToolContext { context } if matches!(context.as_ref(), super::service_call::ArchivedContext::Tool { .. }))
            }
            Q::ToolContextClose { context } => {
                matches!(self, Self::ToolContextClosed { context: reply } if reply.matches(*context))
            }
            Q::InstanceInspect { .. } => matches!(self, Self::InstanceStatus { .. }),
            Q::InstanceStopReason { context } => {
                matches!(self, Self::InstanceStopReason { context: reply, .. }
                    if reply.instance.0 == *context.instance.as_bytes()
                    && reply.handle.0 == *context.handle.as_bytes()
                    && reply.incarnation.0 == *context.incarnation.as_bytes()
                    && reply.owner_generation.to_native() == context.owner_generation)
            }
            Q::InstanceConfigurations { .. } => {
                matches!(self, Self::InstanceConfigurations(instances) if instances.len() <= 1024)
            }
            Q::InstanceConfiguration { instance } => {
                matches!(self, Self::Instance(reply) if reply.id.0 == *instance.as_bytes() && reply.restore_host_mounts)
            }
            Q::InstanceRestore { request, .. } => {
                matches!(self, Self::Instance(instance) if instance.launch_request.0 == *request.as_bytes())
            }
            Q::InstanceMounts { instance, .. } => {
                matches!(self, Self::Instance(reply) if reply.id.0 == *instance.as_bytes())
            }
            Q::InstanceList { session } => {
                matches!(self, Self::InstanceOwners(owners) if owners.len() <= 32 && owners.iter().all(|owner| owner.instance.session.as_str() == session && owner.process.as_ref().is_none_or(|process| process.pid.to_native() > 1)))
            }
            Q::InstanceChannelsAck { context, .. } => {
                matches!(self,Self::Instance(instance) if instance.id.0==*context.instance.as_bytes())
            }
            Q::InstanceChannelsClose { context } => {
                matches!(self,Self::Instance(instance) if instance.id.0==*context.instance.as_bytes())
            }
            Q::InstanceEvents { context } => {
                matches!(self, Self::InstanceEvents(notice) if notice.context.instance.0 == *context.instance.as_bytes()
                && notice.context.handle.0 == *context.handle.as_bytes() && notice.context.incarnation.0 == *context.incarnation.as_bytes()
                && notice.context.owner_generation.to_native() == context.owner_generation)
            }
            Q::InstanceReserve { request, .. } => {
                matches!(self, Self::Instance(instance) if instance.launch_request.0 == *request.as_bytes())
            }
            Q::InstanceLaunchFailed { instance, .. } => {
                matches!(self, Self::Instance(reply) if reply.id.0 == *instance.as_bytes())
            }
            Q::InstanceCompile { .. } => matches!(self, Self::CompiledModule(_)),
            Q::InstanceStartup { context, .. } => {
                matches!(self, Self::Instance(instance) if instance.id.0 == *context.instance.as_bytes() && instance.owner_generation.to_native() == context.owner_generation)
            }
            Q::InstanceShow { instance, owner_generation, .. } => {
                matches!(self, Self::InstanceShowing { instance: found, owner_generation: generation, .. } if found.0 == *instance.as_bytes() && generation.to_native() == *owner_generation)
            }
            Q::InstanceCompiled { wasm, compatibility, .. } => {
                matches!(self, Self::CompiledModule(value) if match value.as_ref() {
                    rkyv::option::ArchivedOption::None => true,
                    rkyv::option::ArchivedOption::Some(descriptor) => descriptor.wasm == *wasm && descriptor.compatibility == *compatibility
                })
            }
            Q::InstanceClaim { instance } => {
                matches!(self, Self::InstanceAdmission(admission) if admission.instance.id.0 == *instance.as_bytes() && admission.context.instance.0 == *instance.as_bytes())
            }
            Q::InstanceReconstruct { context, target } => {
                matches!(self, Self::InstanceAdmission(reply) if reply.instance.id.0 == *context.instance.as_bytes()
                    && reply.instance.build.as_str() == target.build && reply.context.incarnation.0 == *target.incarnation.as_bytes())
                    || matches!(self, Self::InstanceDrainPending { context: reply } if reply.instance.0 == *context.instance.as_bytes()
                        && reply.handle.0 == *context.handle.as_bytes() && reply.incarnation.0 == *context.incarnation.as_bytes()
                        && reply.owner_generation.to_native() == context.owner_generation)
            }
            _ => false,
        }
    }

    fn matches_control(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        match request {
            Q::LocalProtocol => matches!(self, Self::Protocol { .. }),
            Q::Status | Q::DesktopLaunch => matches!(self, Self::Status(_)),
            Q::WaitChanges { .. } => matches!(self, Self::Changes(_)),
            Q::OrderSessions { .. } => matches!(self, Self::OrderSessions { .. }),
            Q::ForgetSession { .. } => matches!(self, Self::ForgetSession { .. }),
            Q::CacheBudget { .. } => matches!(self, Self::CacheBudget { .. }),
            Q::SetDisplayName { .. } => matches!(self, Self::DisplayName { .. }),
            Q::AcceptAgreement { .. } => matches!(self, Self::AgreementAccepted),
            Q::SavedAccount => matches!(self, Self::SavedAccount { .. }),
            Q::RetryAccountKeyBackup => matches!(self, Self::AccountKeyBackup { .. }),
            Q::PrepareSignIn => matches!(self, Self::SignInAvailable { .. }),
            Q::RequestCode { .. } => matches!(self, Self::CodeSent { .. }),
            Q::SignIn { .. } | Q::SignInWithKey { .. } => matches!(self, Self::SignedIn(_)),
            Q::Sync => matches!(self, Self::Availability { .. }),
            Q::StartSync
            | Q::StartJoin { .. }
            | Q::TransferStatus
            | Q::CancelTransfer
            | Q::CheckUpdates { .. } => matches!(self, Self::Transfer { .. }),
            Q::RetrySessionInfo { .. } => matches!(self, Self::Status(_)),
            Q::PrepareSession { .. } => {
                matches!(self, Self::SessionPrepared(_) | Self::Transfer { .. })
            }
            Q::Disconnect => matches!(self, Self::Disconnected { .. }),
            Q::Join { .. } => matches!(self, Self::Session(_)),
            Q::PreviewInvitation { .. } | Q::PreviewDesktopInvitation { .. } => {
                matches!(self, Self::InvitationPreview(_))
            }
            Q::WatchInvitation { .. }
            | Q::ResumeInvitation { .. }
            | Q::RetainDesktopInvitation { .. } => {
                matches!(self, Self::InvitationStatus { .. })
            }
            Q::PauseInvitation { .. } => matches!(self, Self::InvitationPaused { .. }),
            Q::Provision { .. } => matches!(self, Self::Provisioned { .. }),
            Q::Permissions { .. } => matches!(self, Self::Permissions { .. }),
            Q::Revoke { .. } => matches!(self, Self::Revoked { .. }),
            Q::LabelVolume { .. }
            | Q::CreateVolume { .. }
            | Q::InitializeRoot { .. }
            | Q::InitializeApp { .. }
            | Q::OpenAppVolume { .. }
            | Q::OpenAutomaticVolume { .. } => {
                matches!(self, Self::Volume(_))
            }
            Q::PersonalVolumes { .. } => matches!(self, Self::PersonalVolumes { .. }),
            Q::SelectPersonalVolumes { .. } | Q::AddPersonalVolumes { .. } => {
                matches!(self, Self::PersonalVolumesSelected { .. })
            }
            Q::Invitation { .. } => matches!(self, Self::Invitation { .. }),
            Q::SessionReference { .. } => matches!(self, Self::SessionReference { .. }),
            Q::AppVolumes { .. } | Q::AutomaticVolumes { .. } => {
                matches!(self, Self::AutomaticVolumes { slots } if slots.len() <= 6)
            }
            _ => false,
        }
    }

    fn matches_catalog(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        use ArchivedDiagnosticReply as G;
        match request {
            Q::Entries { volume, after, .. } => {
                matches!(self, Self::Entries { volume: got, after: cursor, .. }
                    if got.0 == *volume.as_bytes() && cursor.as_ref().map(|id| *id.as_bytes()) == after.map(|id| *id.as_bytes()))
            }
            Q::DirectoryEntries { volume, target, cut, after, .. } => {
                matches!(self, Self::DirectoryEntries { volume: got, target: got_target, cut_requested, after: cursor, .. }
                    if got.0 == *volume.as_bytes()
                    && matches_directory_target(got_target, target)
                    && cut_requested.as_ref().as_ref().map(|value| value.to_native()) == *cut
                    && cursor.as_ref().as_ref().map(|id| *id.as_bytes()) == after.map(|id| *id.as_bytes()))
            }
            Q::VolumeDetails { volume, .. } => {
                matches!(self, Self::VolumeDetails { details }
                    if details.volume.id.0 == *volume.as_bytes())
            }
            Q::EntryDetails { volume, entry, .. } => {
                matches!(self, Self::EntryDetails { volume: got, details }
                    if got.0 == *volume.as_bytes() && details.entry.id.as_bytes() == entry.as_bytes())
            }
            Q::KvRename { mutation, .. } => {
                let super::KvRenameMutation { address, old_key, new_key, .. } = mutation.as_ref();
                matches!(self, Self::KvRenamed { receipt }
                    if matches_address(&receipt.address, address)
                    && receipt.old_key.as_slice() == old_key.as_slice()
                    && receipt.new_key.as_slice() == new_key.as_slice()
                    && receipt.root.level <= kvdb::encoding::LEVEL_MAX)
            }
            Q::EntryRevisions { address, after, .. } => {
                matches!(self, Self::EntryRevisions { address: got, after: cursor, .. }
                    if matches_address(got, address) && cursor.as_ref().map(|cursor| (cursor.origin_clock.to_native(), *cursor.revision.as_bytes())) == after.map(|cursor| (cursor.origin_clock, *cursor.revision.as_bytes())))
            }
            Q::RejectedEntries { volume, after, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::RejectedEntries { volume: got, after: cursor, .. }
                    if got.0 == *volume.as_bytes() && cursor.as_ref().map(|cursor| (cursor.cut.0, cursor.offset.to_native())) == after.map(|cursor| (*cursor.cut.as_bytes(), cursor.offset))))
            }
            Q::RecoverEntry { address, name, revision, rejected_cut } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::EntryRecovered { source, name: got_name, revision: got, rejected_cut: cut, entry }
                    if matches_address(source, address) && got.as_ref().map(|id| *id.as_bytes()) == revision.map(|id| *id.as_bytes())
                        && cut.as_ref().map(|id| id.0) == rejected_cut.map(|id| *id.as_bytes())
                        && entry.id.as_bytes() != address.entry.as_bytes() && got_name.as_str() == name))
            }
            Q::CreateDatabase { volume, .. } => {
                matches!(self, Self::VolumeEntry { volume: got, .. } if got.0 == *volume.as_bytes())
            }
            Q::CreateDatabaseIn { volume, parent, name, .. }
            | Q::BloblogCreateIn { volume, parent, name, .. } => {
                matches!(self, Self::VolumeEntry { volume: got, entry }
                    if got.0 == *volume.as_bytes()
                    && entry.info.namespace.parent.as_ref().is_some_and(|got| got.as_bytes() == parent.as_bytes())
                    && entry.info.name.as_str() == name)
            }
            Q::RenameEntry { volume, entry, .. }
            | Q::RenameEntryComponent { volume, entry, .. }
            | Q::RemoveEntry { volume, entry, .. } => {
                matches!(self, Self::EntryChanged { volume: got_volume, entry: got_entry }
                    if got_volume.0 == *volume.as_bytes() && got_entry.as_bytes() == entry.as_bytes())
            }
            Q::EntryPermissions { volume, entry, .. } => {
                matches!(self, Self::EntryPermissions { volume: got_volume, entry: got_entry }
                    if got_volume.0 == *volume.as_bytes() && got_entry.as_bytes() == entry.as_bytes())
            }
            Q::BloblogCreate { volume, .. } => {
                matches!(self, Self::VolumeEntry { volume: got, .. } if got.0 == *volume.as_bytes())
            }
            _ => false,
        }
    }

    fn matches_file(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        use ArchivedDiagnosticReply as G;
        match request {
            Q::FileCreate { volume, name, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::FileCreated { address, name: got_name, entry }
                    if address.volume.0 == *volume.as_bytes() && entry.id.as_bytes() == address.entry.as_bytes() && got_name.as_str() == name))
            }
            Q::FileCreateIn { volume, parent, name, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::FileCreatedIn { parent: got_parent, address, name: got_name, entry }
                    if got_parent.as_bytes() == parent.as_bytes()
                    && address.volume.0 == *volume.as_bytes()
                    && entry.id.as_bytes() == address.entry.as_bytes()
                    && entry.info.namespace.parent.as_ref().is_some_and(|got| got.as_bytes() == parent.as_bytes())
                    && got_name.as_str() == name && entry.info.name.as_str() == name))
            }
            Q::FileWrite { address, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::FileWritten { address: got, entry }
                    if matches_address(got,address) && entry.id.as_bytes() == address.entry.as_bytes()))
            }
            Q::FileRead { address, .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::FileRead { address: got, .. }
                    if matches_address(got,address)))
            }
            Q::Usage { .. } => {
                matches!(self, Self::Diagnostic(reply) if matches!(reply.as_ref(), G::Usage(_)))
            }
            _ => false,
        }
    }

    fn matches_recovery(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        match request {
            Q::RecoveryStatus | Q::SaveRecovery | Q::RecoverCredentials { .. } => {
                matches!(self, Self::Recovery(_))
            }
            Q::PrepareRecovery => matches!(self, Self::PreparedRecovery(_)),
            _ => false,
        }
    }

    fn matches_developer(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        use ArchivedDeveloperReply as D;
        match request {
            Q::GrantVolume { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::VolumeGranted { .. }))
            }
            Q::SetAppBuildNotes { app, build, notes } => {
                let Self::Developer(reply) = self else { return false };
                let D::App(response) = reply.as_ref() else { return false };
                response.app.as_ref().is_some_and(|current| {
                    current.identity.publisher.0 == *app.publisher.as_bytes()
                        && current.identity.app_id.as_str() == app.app_id
                        && current.revision == response.revision
                        && !matches!(
                            current.state,
                            crate::network::ap::operation::ArchivedAppState::Deleted
                        )
                        && crate::identifier::valid(current.build_id.as_str(), 32)
                        && crate::network::ap::operation::valid_app_notes(current.notes.as_str())
                        && (current.build_id.as_str() != build || current.notes.as_str() == notes)
                })
            }
            Q::SetAppEnabled { .. } | Q::DeleteApp { .. } | Q::PublishApp { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::App(_)))
            }
            Q::ListApps { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::Apps(_)))
            }
            Q::ListAppBuilds { after, .. } => {
                let Self::Developer(reply) = self else { return false };
                let D::AppBuilds(page) = reply.as_ref() else { return false };
                AppBuildPage::valid_rows(
                    page.builds.iter().map(|row| (row.build_id.as_str(), row.notes.as_str())),
                    after.as_deref(),
                    page.next.as_ref().map(|next| next.as_str()),
                )
            }
            Q::GetAppDisplay { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::Display { .. }))
            }
            Q::GetAccountDisplayNames { account_ids, .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(),
                D::AccountDisplayNames { names }
                if names.len() <= account_ids.len() && names.iter().all(|name|
                    account_ids.iter().any(|id| id.as_bytes() == &name.account_id.0)
                    && crate::store::account::validate_display_name(name.display_name.as_str()).is_ok()
                )))
            }
            Q::UpdateSession { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::Updated { .. }))
            }
            Q::SignPackage { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::PackageSigned { .. }))
            }
            Q::AuthenticatePackage { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::PackageIdentity { .. }))
            }
            Q::PrepareApp { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::AppPrepared))
            }
            Q::CreateAppSession { .. } | Q::CreateBuildSession { .. } | Q::CreateSession { .. } => {
                matches!(self, Self::Developer(reply) if matches!(reply.as_ref(), D::Session(_)))
            }
            _ => false,
        }
    }

    fn matches_objects(&self, request: &super::Request) -> bool {
        use super::Request as Q;
        match request {
            Q::Schema { database, .. } => matches!(self, Self::Schema { database: got, .. }
                if matches_address(got, database)),
            Q::CreateObject { database, type_id, .. } => matches!(self, Self::ObjectCreated(result)
                if matches_address(&result.database, database) && result.object.type_id.0 == *type_id.as_bytes()),
            Q::ReadObject { database, object, .. } | Q::EditObject { database, object, .. } => {
                matches!(self, Self::Object(result) if matches_address(&result.database, database)
                    && result.object.object_id.0 == *object.as_bytes())
            }
            Q::RemoveObject { database, object, .. } => matches!(self,
                Self::ObjectRemoved { database: got, object: id } if matches_address(got, database)
                    && id.0 == *object.as_bytes()),
            Q::Objects { database, type_id, after, .. } => matches!(self, Self::Objects(result)
                if matches_address(&result.database, database) && result.type_id.0 == *type_id.as_bytes()
                    && result.after.as_ref().map(|id| id.0) == after.map(|id| *id.as_bytes())),
            _ => false,
        }
    }
}

#[cfg(feature = "app-host")]
fn matches_directory_target(
    actual: &crate::store::volume_index::ArchivedDirectoryTarget,
    expected: &crate::store::volume_index::DirectoryTarget,
) -> bool {
    use crate::store::volume_index::{ArchivedDirectoryTarget as A, DirectoryTarget as T};
    match (actual, expected) {
        (A::Root, T::Root) => true,
        (A::Directory(actual), T::Directory(expected)) | (A::Entry(actual), T::Entry(expected)) => {
            actual.as_bytes() == expected.as_bytes()
        }
        _ => false,
    }
}

#[cfg(feature = "app-host")]
fn matches_address(
    actual: &crate::store::volume_index::ArchivedEntryAddress,
    expected: &crate::store::volume_index::EntryAddress,
) -> bool {
    actual.volume.0 == *expected.volume.as_bytes()
        && actual.entry.as_bytes() == expected.entry.as_bytes()
}

#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct SignInReply {
    pub replacement_required: bool,
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_error: Option<String>,
}

/// AP usage totals are parsed at the remote boundary before entering local IPC.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct VolumeUsage {
    pub owner_account_id: Id,
    pub volume_id: Id,
    pub database_bytes: u64,
    pub blob_bytes: u64,
    pub blob_count: u64,
}

#[cfg(feature = "app-host")]
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[serde(untagged)]
pub enum DiagnosticReply {
    VolumeDuplicated {
        source: Id,
        volume: crate::store::Volume,
    },
    Usage(VolumeUsage),
    PayloadSize {
        volume: Id,
        entry: Option<crate::store::EntryId>,
        bytes: u64,
    },
    EntryRecovered {
        source: crate::store::volume_index::EntryAddress,
        name: String,
        revision: Option<crate::store::EntryId>,
        rejected_cut: Option<Id>,
        entry: crate::store::volume_index::VolumeEntry,
    },
    RejectedEntries {
        volume: Id,
        after: Option<crate::store::volume_index::RecoveryCursor>,
        page: crate::store::volume_index::RejectedEntryPage,
    },
    FileCreated {
        address: crate::store::volume_index::EntryAddress,
        name: String,
        entry: crate::store::volume_index::VolumeEntry,
    },
    FileCreatedIn {
        parent: crate::store::EntryId,
        address: crate::store::volume_index::EntryAddress,
        name: String,
        entry: crate::store::volume_index::VolumeEntry,
    },
    FileWritten {
        address: crate::store::volume_index::EntryAddress,
        entry: crate::store::volume_index::VolumeEntry,
    },
    FileRead {
        address: crate::store::volume_index::EntryAddress,
        bytes: u64,
    },
    EntryDuplicated {
        source: crate::store::volume_index::EntryAddress,
        entry: crate::store::volume_index::VolumeEntry,
    },
}

#[cfg(all(test, feature = "app-host"))]
#[allow(clippy::unwrap_used, reason = "Tests assert valid fixtures.")]
mod tests {
    use super::*;
    use crate::engine::ipc::codec;
    use serde_json::json;

    #[test]
    fn directory_creation_replies_bind_parent_and_component() {
        use crate::engine::Request;
        use crate::store::{
            Access, LocalStore,
            volume_index::{EntryAddress, EntryKind},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Directory creation", false).unwrap().id;
        store.create_database(&access, volume, "Folder/seed", EntryKind::KvDb {}).unwrap();
        store.create_database(&access, volume, "Other/seed", EntryKind::KvDb {}).unwrap();
        let parent = store.resolve_entry(&access, volume, "Folder").unwrap().id;
        let other = store.resolve_entry(&access, volume, "Other").unwrap().id;
        let database = store
            .create_database_in(&access, volume, parent, "Settings", EntryKind::KvDb {})
            .unwrap();
        let log = store.create_bloblog_in(&access, volume, parent, "Events").unwrap();
        let blob = store
            .create_blob_from_reader_in(&access, volume, parent, "Image", &mut &b"image"[..])
            .unwrap();
        let fixtures = [
            (
                Reply::VolumeEntry { volume, entry: database },
                Request::CreateDatabaseIn {
                    session: None,
                    volume,
                    parent,
                    name: "Settings".into(),
                    kind: Box::new(EntryKind::KvDb {}),
                },
            ),
            (
                Reply::VolumeEntry { volume, entry: log },
                Request::BloblogCreateIn { session: None, volume, parent, name: "Events".into() },
            ),
            (
                Reply::Diagnostic(DiagnosticReply::FileCreatedIn {
                    parent,
                    address: EntryAddress { volume, entry: blob.id },
                    name: "Image".into(),
                    entry: blob,
                }),
                Request::FileCreateIn {
                    session: None,
                    volume,
                    parent,
                    name: "Image".into(),
                    path: "/tmp/image".into(),
                },
            ),
        ];
        for (reply, request) in fixtures {
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            assert!(archived.matches_request(&request));
            for wrong_parent in [false, true] {
                let mut wrong = request.clone();
                let (parent, name) = match &mut wrong {
                    Request::CreateDatabaseIn { parent, name, .. }
                    | Request::BloblogCreateIn { parent, name, .. }
                    | Request::FileCreateIn { parent, name, .. } => (parent, name),
                    _ => panic!("creation fixture"),
                };
                if wrong_parent {
                    *parent = other;
                } else {
                    *name = "Wrong".into();
                }
                assert!(!archived.matches_request(&wrong));
            }
        }
    }

    #[test]
    fn volume_details_reply_binds_the_requested_volume() {
        let volume = Id::from_bytes([1; 16]);
        let reply = Reply::VolumeDetails {
            details: crate::store::VolumeDetails {
                volume: crate::store::Volume {
                    id: volume,
                    name: "Volume".into(),
                    owner: Id::from_bytes([2; 16]),
                    personal: true,
                    share_key_with_service: true,
                },
                permissions: Default::default(),
                created_clock: 5,
                name_clock: 6,
            },
        };
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
        assert!(
            archived
                .matches_request(&crate::engine::Request::VolumeDetails { session: None, volume })
        );
        assert!(!archived.matches_request(&crate::engine::Request::VolumeDetails {
            session: None,
            volume: Id::from_bytes([3; 16]),
        }));
        let first = crate::store::EntryId::from_bytes([4; 15]).unwrap();
        let other = crate::store::EntryId::from_bytes([5; 15]).unwrap();
        for entry in [None, Some(first)] {
            let reply =
                Reply::Diagnostic(DiagnosticReply::PayloadSize { volume, entry, bytes: 42 });
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            assert!(archived.matches_request(&crate::engine::Request::PayloadSize {
                session: None,
                volume,
                entry
            }));
            assert!(!archived.matches_request(&crate::engine::Request::PayloadSize {
                session: None,
                volume,
                entry: Some(other)
            }));
            assert!(!archived.matches_request(&crate::engine::Request::PayloadSize {
                session: None,
                volume: Id::from_bytes([3; 16]),
                entry
            }));
        }
    }

    #[test]
    fn directory_replies_bind_target_cut_and_cursor() {
        use crate::store::volume_index::{DirectoryListing, DirectoryTarget};
        let volume = Id::from_bytes([1; 16]);
        let entry = crate::store::EntryId::from_bytes([2; 15]).unwrap();
        let other = crate::store::EntryId::from_bytes([3; 15]).unwrap();
        for listing in [
            DirectoryListing::Page {
                cut: 5,
                parent: Some(entry),
                ancestors: Vec::new(),
                entries: Vec::new(),
                next: None,
            },
            DirectoryListing::Changed { cut: 6 },
        ] {
            let reply = Reply::DirectoryEntries {
                volume,
                target: DirectoryTarget::Directory(entry),
                cut_requested: Some(5),
                after: Some(other),
                listing,
            };
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            let request = |volume, target, cut, after| crate::engine::Request::DirectoryEntries {
                session: None,
                volume,
                target,
                cut,
                after,
                limit: 32,
            };
            assert!(archived.matches_request(&request(
                volume,
                DirectoryTarget::Directory(entry),
                Some(5),
                Some(other),
            )));
            for (volume, target, cut, after) in [
                (Id::from_bytes([4; 16]), DirectoryTarget::Directory(entry), Some(5), Some(other)),
                (volume, DirectoryTarget::Entry(entry), Some(5), Some(other)),
                (volume, DirectoryTarget::Directory(other), Some(5), Some(other)),
                (volume, DirectoryTarget::Root, Some(5), Some(other)),
                (volume, DirectoryTarget::Directory(entry), None, Some(other)),
                (volume, DirectoryTarget::Directory(entry), Some(6), Some(other)),
                (volume, DirectoryTarget::Directory(entry), Some(5), None),
            ] {
                assert!(!archived.matches_request(&request(volume, target, cut, after)));
            }
        }
    }

    #[test]
    fn component_rename_reply_binds_the_original_volume_and_entry() {
        // An unrelated mutation acknowledgment must not finish a stable-parent rename.
        let volume = Id::from_bytes([1; 16]);
        let entry = crate::store::EntryId::from_bytes([2; 15]).unwrap();
        let reply = Reply::EntryChanged { volume, entry };
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let reply = codec::access::<ArchivedReply>(&bytes).unwrap();
        assert!(reply.matches_request(&crate::engine::Request::RenameEntryComponent {
            session: None,
            volume,
            entry,
            name: "name".into(),
        }));
        for (volume, entry) in [
            (Id::from_bytes([3; 16]), entry),
            (volume, crate::store::EntryId::from_bytes([4; 15]).unwrap()),
        ] {
            assert!(!reply.matches_request(&crate::engine::Request::RenameEntryComponent {
                session: None,
                volume,
                entry,
                name: "name".into(),
            }));
        }
    }

    #[test]
    fn entry_copy_replies_bind_both_source_volume_and_entry() {
        // A valid copy for another source cannot finish this entry's pending operation.
        use crate::store::{
            Access, LocalStore,
            volume_index::{EntryAddress, EntryKind},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Copies", false).unwrap();
        let source =
            store.create_database(&access, volume.id, "Settings", EntryKind::KvDb {}).unwrap();
        let address = EntryAddress { volume: volume.id, entry: source.id };
        let copy = store.duplicate_entry(&access, address).unwrap();
        let reply =
            Reply::Diagnostic(DiagnosticReply::EntryDuplicated { source: address, entry: copy });
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let reply = codec::access::<ArchivedReply>(&bytes).unwrap();
        assert!(reply.matches_request(&crate::engine::Request::DuplicateEntry { address }));
        for wrong in [
            EntryAddress { volume: Id::generate().unwrap(), ..address },
            EntryAddress { entry: crate::store::EntryId::generate().unwrap(), ..address },
        ] {
            assert!(
                !reply.matches_request(&crate::engine::Request::DuplicateEntry { address: wrong })
            );
        }
    }

    #[test]
    fn ui_zoom_replies_bind_the_application_and_committed_percentage() {
        // A well-formed reply for another app/value must not acknowledge this save.
        use crate::engine::{Engine, Request};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = Some(Id::from_bytes([1; 16]));
        for scale in [5_000, 10_000, 12_345, 18_765, 20_000] {
            let request = Request::UiZoom { root, scale: Some(scale) };
            let reply = engine.dispatch_reply(request.clone()).unwrap();
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            assert!(archived.matches_request(&request));
            assert!(!archived.matches_request(&Request::UiZoom { root: None, scale: Some(scale) }));
            assert!(!archived.matches_request(&Request::UiZoom { root, scale: Some(20_001) }));
            assert!(archived.matches_request(&Request::UiZoom { root, scale: None }));
        }
        assert!(engine.dispatch_reply(Request::UiZoom { root, scale: Some(20_001) }).is_err());
    }

    #[test]
    fn browser_draft_receipts_bind_account_operation_identity_and_revision_without_change_notice() {
        use crate::engine::{Engine, Request};
        use crate::store::{browser_drafts::*, object_contract::EntryAcl};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let account = engine.services.store.account().id;
        let draft = BrowserDraft {
            id: Id::from_bytes([2; 16]),
            revision: 1,
            volume: Id::from_bytes([3; 16]),
            entry: None,
            ancestors: vec![],
            body: BrowserDraftBody::Permissions(PermissionDraft {
                base_active: EntryAcl::Replace(Default::default()),
                base_retained: None,
                active: EntryAcl::Replace(Default::default()),
                retained: None,
                adding_grant: false,
                grant_input: String::new(),
                grant_target: None,
                grant_service: false,
            }),
        };
        let before = engine.changes;
        let request = Request::BrowserDrafts {
            account,
            operation: BrowserDraftOperation::Upsert {
                expected_revision: None,
                draft: Box::new(draft.clone()),
            },
        };
        let reply = engine.dispatch_reply(request.clone()).unwrap();
        assert_eq!(engine.changes, before, "draft writes do not publish a change notice");
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
        assert!(archived.matches_request(&request));
        assert!(!archived.matches_request(&Request::BrowserDrafts {
            account: Id::from_bytes([4; 16]),
            operation: BrowserDraftOperation::Upsert {
                expected_revision: None,
                draft: Box::new(draft.clone())
            },
        }));
        assert!(!archived.matches_request(&Request::BrowserDrafts {
            account,
            operation: BrowserDraftOperation::Remove { id: draft.id, expected_revision: 1 },
        }));
        let mut newer = draft.clone();
        newer.revision = 2;
        assert!(!archived.matches_request(&Request::BrowserDrafts {
            account,
            operation: BrowserDraftOperation::Upsert {
                expected_revision: Some(1),
                draft: Box::new(newer)
            },
        }));
        assert!(
            engine
                .dispatch_reply(Request::BrowserDrafts {
                    account: Id::from_bytes([4; 16]),
                    operation: BrowserDraftOperation::List
                })
                .is_err()
        );
        assert_eq!(engine.changes, before);
    }

    #[test]
    fn rejected_discovery_reply_requires_the_volume_and_full_cursor() {
        use crate::engine::Request;
        use crate::store::volume_index::{RecoveryCursor, RejectedEntryPage};
        let volume = Id::from_bytes([1; 16]);
        let cursor = RecoveryCursor { cut: Id::from_bytes([2; 16]), offset: 3 };
        let reply = Reply::Diagnostic(DiagnosticReply::RejectedEntries {
            volume,
            after: Some(cursor),
            page: RejectedEntryPage { entries: vec![], next: None },
        });
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
        for (got_volume, after, matches) in [
            (volume, Some(cursor), true),
            (Id::from_bytes([3; 16]), Some(cursor), false),
            (volume, None, false),
            (volume, Some(RecoveryCursor { offset: 4, ..cursor }), false),
            (volume, Some(RecoveryCursor { cut: Id::from_bytes([4; 16]), ..cursor }), false),
        ] {
            assert_eq!(
                archived.matches_request(&Request::RejectedEntries {
                    volume: got_volume,
                    after,
                    limit: 32,
                }),
                matches
            );
        }
    }

    #[test]
    fn local_dispatch_replies_match_the_request_after_archiving() {
        use crate::engine::{Engine, Request};

        fn check(engine: &mut Engine, request: Request) -> Reply {
            let reply = engine.dispatch_reply(request.clone()).unwrap_or_else(|error| {
                panic!("{request:?} failed instead of exercising a successful reply: {error}")
            });
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            assert!(archived.matches_request(&request), "reply {reply:?} for {request:?}");
            // Every successful result must also reject an unrelated operation.
            let unrelated = if matches!(request, Request::LocalProtocol) {
                Request::Status
            } else {
                Request::LocalProtocol
            };
            assert!(!archived.matches_request(&unrelated));
            reply
        }

        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        check(&mut engine, Request::LocalProtocol);
        check(&mut engine, Request::Status);
        let Reply::Volume(volume) = check(
            &mut engine,
            Request::CreateVolume { share_key_with_service: true, name: "Reply checks".into() },
        ) else {
            panic!("volume")
        };
        let volume = volume.id;
        check(&mut engine, Request::LabelVolume { volume, label: "Renamed".into() });
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        check(
            &mut engine,
            Request::InitializeRoot {
                volume: Id::generate().unwrap(),
                name: "Initialized".into(),
                declaration: Box::new(declaration.clone()),
            },
        );
        let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } =
            &declaration.databases[0].kind
        else {
            panic!("object database");
        };
        let access = crate::store::Access::owner(engine.services.store.account().id);
        let entry = engine
            .services
            .store
            .create_database(
                &access,
                volume,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
            )
            .unwrap();
        let database = crate::store::volume_index::EntryAddress { volume, entry: entry.id };
        check(&mut engine, Request::Schema { session: None, database, schema: schema.clone() });
        check(
            &mut engine,
            Request::CreateObject {
                session: None,
                database,
                type_id: Id::from_bytes([1; 16]),
                fields: std::collections::BTreeMap::new(),
            },
        );
        check(
            &mut engine,
            Request::Objects {
                session: None,
                database,
                type_id: Id::from_bytes([1; 16]),
                after: None,
                limit: 64,
            },
        );
        let Reply::VolumeEntry { .. } = check(
            &mut engine,
            Request::BloblogCreate { session: None, volume, name: "edits".into() },
        ) else {
            panic!("entry");
        };
        check(&mut engine, Request::CacheBudget { bytes: 1024 * 1024 });
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
        engine.services.store.set_display_name("Initial").unwrap();
        check(&mut engine, Request::SetDisplayName { name: "Changed".into() });
        check(&mut engine, Request::TransferStatus);
        check(&mut engine, Request::CancelTransfer);
        check(&mut engine, Request::Disconnect);
        check(&mut engine, Request::Permissions { volume, acl: Default::default() });
        let session =
            engine.create_session(crate::engine::tests::button_package(), volume).unwrap();
        let id = session.id.clone();
        check(
            &mut engine,
            Request::Provision { volume, version: session.resolved_build_id.clone() },
        );
        check(&mut engine, Request::PersonalVolumes { session: session.id.clone() });
        check(
            &mut engine,
            Request::SelectPersonalVolumes { session: session.id, grants: Vec::new() },
        );
        check(&mut engine, Request::AddPersonalVolumes { session: id.clone(), grants: Vec::new() });
        check(&mut engine, Request::RetrySessionInfo { session: id.clone() });
        assert!(
            engine.embedded_sessions.is_empty(),
            "retrying display information never opens an app"
        );
        assert!(engine.session_instances.list(&id).unwrap().is_empty());
        assert!(
            engine.background.is_none(),
            "retrying display information never starts a session transfer"
        );
        check(&mut engine, Request::PrepareSession { session: id.clone() });
        check(&mut engine, Request::OrderSessions { sessions: vec![id.clone()] });
        let Reply::Invitation { invitation, .. } =
            check(&mut engine, Request::Invitation { session: id.clone() })
        else {
            panic!("invitation")
        };
        check(&mut engine, Request::PreviewInvitation { invitation });
        check(&mut engine, Request::WatchInvitation { session: id.clone() });
        check(&mut engine, Request::PauseInvitation { session: id.clone() });
        check(&mut engine, Request::ResumeInvitation { session: id.clone() });
        check(
            &mut engine,
            Request::Revoke {
                volume,
                account: Id::from_bytes([77; 16]),
                reason: "Reply fixture".into(),
            },
        );
        // Real transfer admission returns a Transfer reply before network work
        // completes. Keep this test's attempted transport on an unused loopback port.
        let offline = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut config =
            crate::network::ap::ClientConfig::platform(offline.local_addr().unwrap(), "localhost");
        config.connect_timeout = std::time::Duration::from_millis(50);
        engine.remote.as_mut().unwrap().set_test_transport(config);
        check(&mut engine, Request::CheckUpdates { session: id.clone() });
        check(&mut engine, Request::StartSync);
        check(&mut engine, Request::StartJoin { session: id.clone() });
        check(&mut engine, Request::CancelTransfer);
        check(&mut engine, Request::ForgetSession { session: id });
    }

    #[test]
    fn engine_status_archives_the_catalog_and_pending_operations() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let volume = engine.services.store.create_volume("Archive status", true).unwrap();
        let expected =
            serde_json::to_value(engine.dispatch_reply(crate::engine::Request::Status).unwrap())
                .unwrap();
        let status = engine.status_snapshot().unwrap();
        assert!(status.volumes.iter().any(|entry| entry.id == volume.id));
        assert!(!status.pending.is_empty());
        assert!(!status.examples.is_empty());
        let bytes = codec::encode(&status, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedStatus>(&bytes).unwrap();
        assert!(matches!(archived.transfer.kind, ArchivedTransferKind::Idle));
        let decoded = rkyv::api::low::from_bytes::<Status, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }

    #[test]
    fn app_build_page_validates_paging_before_materialization() {
        let first = crate::identifier::encode(&[0; 32]);
        let second = crate::identifier::encode(&[1; 32]);
        let row = |id: &str, notes: &str| json!({"build_id":id, "published_at":17, "notes":notes});
        let one = row(&first, "First release");
        let two = row(&second, "Added colors");
        let page = AppBuildPage::from_ap(json!({"builds":[one,two], "next":second}), None).unwrap();
        let reply = Reply::Developer(DeveloperReply::AppBuilds(page));
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
        let app = crate::package::AppIdentity {
            publisher: Id::from_bytes([7; 16]),
            app_id: "test".into(),
        };
        let request = |after| crate::engine::Request::ListAppBuilds { app: app.clone(), after };
        assert!(archived.matches_request(&request(None)));
        assert!(!archived.matches_request(&request(Some(first.clone()))));
        assert!(AppBuildPage::from_ap(json!({"builds":[two], "next":null}), Some(&first)).is_ok());
        assert!(AppBuildPage::from_ap(json!({"builds":[], "next":null}), None).is_ok());
        let oversized = (0..5)
            .map(|byte| row(&crate::identifier::encode(&[byte; 32]), &"x".repeat(32768)))
            .collect::<Vec<_>>();
        for invalid in [
            json!({"builds":[one,one], "next":null}),
            json!({"builds":[two,one], "next":null}),
            json!({"builds":[two], "next":first}),
            json!({"builds":[], "next":second}),
            json!({"builds":[row("invalid", "")], "next":null}),
            json!({"builds":[row(&first, &"\n".repeat(16385))], "next":null}),
            json!({"builds":oversized, "next":null}),
        ] {
            assert!(AppBuildPage::from_ap(invalid, None).is_err());
        }
    }

    #[test]
    fn note_edit_reply_matches_the_publisher_and_keeps_the_current_build() {
        let publisher = Id::from_bytes([7; 16]);
        let build = crate::identifier::encode(&[3; 32]);
        let reply = Reply::Developer(DeveloperReply::App(
            serde_json::from_value(json!({
                "revision":7, "app":{"identity":{"publisher":publisher,"app_id":"sketch"},
                "build_id":build,"notes":"Current release","revision":7,"state":"disabled"}
            }))
            .unwrap(),
        ));
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
        let edit = |id: Id, app_id: &str, build: String, notes: &str| {
            crate::engine::Request::SetAppBuildNotes {
                app: crate::package::AppIdentity { publisher: id, app_id: app_id.to_owned() },
                build,
                notes: notes.to_owned(),
            }
        };
        let old = crate::identifier::encode(&[2; 32]);
        assert!(archived.matches_request(&edit(
            publisher,
            "sketch",
            old.clone(),
            "Earlier release"
        )));
        assert!(archived.matches_request(&edit(
            publisher,
            "sketch",
            build.clone(),
            "Current release"
        )));
        assert!(!archived.matches_request(&edit(publisher, "sketch", build, "Different text")));
        assert!(!archived.matches_request(&edit(publisher, "other", old.clone(), "")));
        assert!(!archived.matches_request(&edit(Id::from_bytes([9; 16]), "sketch", old, "")));
    }

    #[test]
    fn developer_archives_preserve_app_publication_and_external_reply_shapes() {
        let id = Id::from_bytes([9; 16]);
        let full = json!({"identity":{"publisher":id,"app_id":"app"},
            "build_id":crate::identifier::encode(&[0xab;32]),
            "notes":"Published release notes","revision":1,"state":"disabled"});
        let app = json!({"revision":1,"app":full});
        let page = json!({"apps":[full],"next":"next-app"});
        let missing = json!({"revision":0,"app":null});
        let cases = [
            (DeveloperReply::App(serde_json::from_value(app.clone()).unwrap()), app),
            (DeveloperReply::App(serde_json::from_value(missing.clone()).unwrap()), missing),
            (DeveloperReply::Apps(serde_json::from_value(page.clone()).unwrap()), page),
            (
                DeveloperReply::VolumeGranted {
                    volume: id,
                    account: id,
                    access: VolumeAccess::Read,
                },
                json!({"volume":id,"account":id,"access":"r"}),
            ),
            (
                DeveloperReply::VolumeGranted {
                    volume: id,
                    account: id,
                    access: VolumeAccess::ReadWrite,
                },
                json!({"volume":id,"account":id,"access":"rw"}),
            ),
            (
                DeveloperReply::Display {
                    build_id: crate::identifier::encode(&[0xab; 32]),
                    name: "App".into(),
                    icon_error: None,
                },
                json!({"build_id":crate::identifier::encode(&[0xab; 32]),"name":"App","icon_error":null}),
            ),
            (
                DeveloperReply::Updated {
                    session: crate::identifier::encode(&[0xcd; 32]),
                    build_id: crate::identifier::encode(&[0xab; 32]),
                    update: None,
                    resolved_build_id: Some(crate::identifier::encode(&[0xef; 32])),
                },
                json!({"session":crate::identifier::encode(&[0xcd; 32]),"build_id":crate::identifier::encode(&[0xab; 32]),"update":null,"resolved_build_id":crate::identifier::encode(&[0xef; 32])}),
            ),
        ];
        for (reply, expected) in cases {
            let bytes = codec::encode(&reply, 4096).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<DeveloperReply, rkyv::rancor::Error>(&bytes).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
        let mut invalid = full;
        invalid["state"] = json!("unknown");
        assert!(
            serde_json::from_value::<crate::network::ap::operation::PublishedApp>(invalid).is_err()
        );
    }

    #[test]
    fn invitation_archives_preserve_local_and_remote_states() {
        use crate::network::ap::operation::InvitationStatus as Remote;
        let states = [
            (InvitationState::Paused, "paused"),
            (InvitationState::Checking, "checking"),
            (InvitationState::SignIn, "sign_in"),
            (Remote::WaitingRegistration.into(), "waiting_registration"),
            (Remote::WaitingApp.into(), "waiting_app"),
            (Remote::WaitingDatabase.into(), "waiting_database"),
            (Remote::WaitingPeer.into(), "waiting_peer"),
            (Remote::Ready { build_id: crate::identifier::encode(&[0xab; 32]) }.into(), "ready"),
            (Remote::Denied { message: "Access revoked".into() }.into(), "denied"),
            (InvitationState::Failed { message: "Disconnected".into() }, "failed"),
        ];
        for (state, kind) in states {
            let status =
                InvitationStatus { session: crate::identifier::encode(&[0xcd; 32]), state };
            let bytes = codec::encode(&status, 4096).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<InvitationStatus, rkyv::rancor::Error>(&bytes)
                    .unwrap();
            assert_eq!(decoded.session, status.session);
            assert_eq!(decoded.state, status.state);
            assert_eq!(serde_json::to_value(decoded).unwrap()["state"]["kind"], kind);
        }
    }

    #[test]
    fn transfer_archive_preserves_progress_cancellation_and_update_feedback() {
        use crate::engine::{UpdateKind, UpdateStatus};
        for kind in [
            TransferKind::Idle,
            TransferKind::Running,
            TransferKind::Complete,
            TransferKind::Paused,
            TransferKind::Canceling,
            TransferKind::Failed,
        ] {
            let status = TransferStatus {
                progress: Some(TransferProgress::Volumes { current: 3, total: 4 }),
                kind,
                session: Some(crate::identifier::encode(&[0xab; 32])),
                message: Some("Retained progress".into()),
                update: Some(UpdateStatus {
                    kind: UpdateKind::Failed,
                    build_id: Some(crate::identifier::encode(&[0xcd; 32])),
                    message: "Build unavailable".into(),
                }),
            };
            let bytes = codec::encode(&status, 4096).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<TransferStatus, rkyv::rancor::Error>(&bytes).unwrap();
            assert_eq!(decoded, status);
            let value = serde_json::to_value(decoded).unwrap();
            assert_eq!(value["message"], "Retained progress");
            assert_eq!(value["update"]["kind"], "failed");
        }
        assert_eq!(
            serde_json::to_value(TransferStatus::default()).unwrap(),
            json!({"kind":"idle"})
        );
    }

    #[test]
    fn transfer_progress_round_trips_each_typed_stage() {
        // The local IPC preserves stage identity and the configured AP hostname.
        for progress in [
            TransferProgress::Connecting { hostname: "ap.example.test".into() },
            TransferProgress::Account,
            TransferProgress::Registrations { current: 1, total: 2 },
            TransferProgress::Volumes { current: 3, total: 4 },
            TransferProgress::Sessions { current: 2, total: 3 },
            TransferProgress::Files { current: 1, total: 4 },
            TransferProgress::Finishing,
        ] {
            let transfer = TransferStatus {
                kind: TransferKind::Running,
                message: Some(progress.message()),
                progress: Some(progress),
                ..Default::default()
            };
            let bytes = codec::encode(&transfer, 1024).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<TransferStatus, rkyv::rancor::Error>(&bytes).unwrap();
            assert_eq!(decoded, transfer);
        }
    }

    #[test]
    fn recovery_archives_preserve_status_and_secret_boundaries() {
        let cases = [
            (
                RecoveryStatus::Ready {
                    revision: 3,
                    pending_save: true,
                    restored_sessions: None,
                    restored_volumes: None,
                },
                json!({"kind":"ready","revision":3,"pending_save":true}),
            ),
            (
                RecoveryStatus::Ready {
                    revision: 4,
                    pending_save: false,
                    restored_sessions: Some(0),
                    restored_volumes: Some(2),
                },
                json!({"kind":"ready","revision":4,"pending_save":false,
                    "restored_sessions":0,"restored_volumes":2}),
            ),
            (
                RecoveryStatus::Setup { revision: 0, pending_save: false, secret: None },
                json!({"kind":"setup","revision":0,"pending_save":false}),
            ),
            (
                RecoveryStatus::Setup {
                    revision: 0,
                    pending_save: true,
                    secret: Some("key".into()),
                },
                json!({"kind":"setup","revision":0,"pending_save":true,"secret":"key"}),
            ),
            (
                RecoveryStatus::Locked { revision: 5, pending_save: false },
                json!({"kind":"locked","revision":5,"pending_save":false}),
            ),
            (RecoveryStatus::Canceled, json!({"kind":"canceled"})),
        ];
        for (status, expected) in cases {
            let bytes = codec::encode(&status, codec::MAX_ARCHIVE).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<RecoveryStatus, rkyv::rancor::Error>(&bytes).unwrap();
            assert_eq!(decoded, status);
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
        let prepared = PreparedRecovery { secret: "key".into() };
        let bytes = codec::encode(&prepared, codec::MAX_ARCHIVE).unwrap();
        let decoded =
            rkyv::api::low::from_bytes::<PreparedRecovery, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), json!({"secret":"key"}));
    }

    #[test]
    fn diagnostic_archives_preserve_payloads_and_external_json() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Files", false).unwrap();
        let entry = store
            .create_blob(
                &crate::store::Access::owner(store.account().id),
                volume.id,
                "file",
                b"archive",
            )
            .unwrap();

        let address =
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };

        let id = Id::from_bytes([7; 16]);
        let cases = [
            (
                DiagnosticReply::Usage(VolumeUsage {
                    owner_account_id: id,
                    volume_id: id,
                    database_bytes: u64::MAX,
                    blob_bytes: 128,
                    blob_count: 1,
                }),
                json!({"owner_account_id":id,"volume_id":id,
                "database_bytes":u64::MAX,"blob_bytes":128,"blob_count":1}),
            ),
            (
                DiagnosticReply::FileCreated {
                    address,
                    name: "folder/file".into(),
                    entry: entry.clone(),
                },
                json!({"address":address,"name":"folder/file","entry":entry}),
            ),
            (
                DiagnosticReply::FileWritten { address, entry: entry.clone() },
                json!({"address":address,"entry":entry}),
            ),
            (
                DiagnosticReply::FileRead { address, bytes: 128 },
                json!({"address":address,"bytes":128}),
            ),
        ];
        for (reply, expected) in cases {
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            codec::access::<ArchivedDiagnosticReply>(&bytes).unwrap();
            let decoded =
                rkyv::api::low::from_bytes::<DiagnosticReply, rkyv::rancor::Error>(&bytes).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
    }

    #[test]
    fn usage_boundary_requires_valid_ids_and_unsigned_totals() {
        let id = Id::from_bytes([7; 16]);
        let value = json!({"owner_account_id":id,"volume_id":id,
            "database_bytes":42,"blob_bytes":128,"blob_count":1});
        assert!(serde_json::from_value::<VolumeUsage>(value.clone()).is_ok());
        for field in ["owner_account_id", "volume_id", "database_bytes", "blob_bytes", "blob_count"]
        {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<VolumeUsage>(missing).is_err());
        }
        for (field, invalid) in [("volume_id", json!("invalid")), ("blob_bytes", json!(-1))] {
            let mut malformed = value.clone();
            malformed[field] = invalid;
            assert!(serde_json::from_value::<VolumeUsage>(malformed).is_err());
        }
    }
}
