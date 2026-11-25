//! Runtime service operations over the account's one AP connection.
//! Each request uses an independent reliable AP stream. Upload bytes immediately
//! follow its request frame; EOF terminates the upload, not the AP connection.
//! Validation checks untrusted identities, paired session/build scope and bounded
//! payloads before service admission. It does not confer authorization. The base58, hex-payload
//! and base64 scans are data-plane loops bounded by their validated lengths.

use crate::store::Id;
use serde::{Deserialize, Serialize};

mod denial;
pub use denial::{MAX_DENIAL_BYTES, ServiceDenial};

pub const SERVICE_DESTINATION: &[u8] = b"service";
pub const BUILD_DISPLAY_LIMIT: usize = 132 * 1024;
/// Decoded bound of a registered invitation envelope; the store seals within it.
pub const INVITATION_ENVELOPE_LIMIT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeInvitationOffer {
    pub share_key_with_service: bool,
    /// Recipient-encrypted descriptor and volume credentials. AP never opens it.
    pub envelope: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeInvitationItem {
    pub volume: Id,
    pub owner: Id,
    pub owner_name: Option<String>,
    pub clock: u64,
    pub share_key_with_service: bool,
    pub envelope: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VolumeInvitationEvent {
    Upsert { item: VolumeInvitationItem },
    Remove { volume: Id, clock: u64 },
    KeyReady,
    PeerAvailable { volume: Id },
    Ready,
    Gap,
}

pub fn volume_invitation_key_message(account: Id, public_key: &[u8; 32]) -> Vec<u8> {
    let mut bytes = b"app.playbit.volume-invitation.key.v1\0".to_vec();
    bytes.extend_from_slice(account.as_bytes());
    bytes.extend_from_slice(public_key);
    bytes
}

/// The account replica orders concurrent writes by clock, then by the original
/// envelope bytes. Use the same order for its service-readable name projection.
pub fn display_name_version(
    name: &str,
    clock: &str,
    ciphertext: &str,
) -> Result<(u64, Vec<u8>), &'static str> {
    use crate::store::account;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    account::validate_display_name(name).map_err(|_| "invalid display name")?;
    if clock.len() > 20 || !clock.as_bytes().first().is_some_and(|b| matches!(b, b'1'..=b'9')) {
        return Err("invalid display name clock");
    }
    let clock: u64 = clock.parse().map_err(|_| "invalid display name clock")?;
    if ciphertext.len() > account::MAX_ENVELOPE_BYTES.div_ceil(3) * 4 {
        return Err("display name envelope too large");
    }
    let bytes = URL_SAFE_NO_PAD.decode(ciphertext).map_err(|_| "invalid display name envelope")?;
    account::validate_envelope_value(kvdb::Clock::from_u64(clock), &bytes)
        .map_err(|_| "invalid display name version")?;
    Ok((clock, bytes))
}

/// Advisory package metadata. Never use this projection to admit executable
/// code, initialize roots, or authorize access to a volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildDisplay {
    pub app_id: String,
    pub build_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub icon_error: Option<String>,
}
impl BuildDisplay {
    pub fn validate(&self, expected_build: &str) -> Result<(), &'static str> {
        if !crate::package::valid_app_id(&self.app_id)
            || self.build_id != expected_build
            || !crate::identifier::valid(&self.build_id, 32)
            || self.name.is_empty()
            || self.name.len() > 256
        {
            return Err("invalid app display identity or name");
        }
        if self.icon.as_ref().is_some_and(|icon| {
            icon.len() > 128 * 1024 || icon.len() % 2 != 0 || !is_hex(icon, icon.len())
        }) || self.icon_error.as_ref().is_some_and(|error| error.is_empty() || error.len() > 256)
            || self.icon.is_some() && self.icon_error.is_some()
        {
            return Err("invalid app display artwork");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuildSelector {
    Exact { build_id: String },
    App { app: crate::package::AppIdentity },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum AppState {
    Active,
    Disabled,
    Deleted,
}

/// Authenticated publication state. Its default World identity is derived,
/// while explicit root bindings remain solely on retained sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct PublishedApp {
    pub identity: crate::package::AppIdentity,
    pub build_id: String,
    pub notes: String,
    pub revision: u64,
    pub state: AppState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct AppBuild {
    pub build_id: String,
    pub notes: String,
    pub published_at: u64,
}

/// Public app-update address. A subscription grants no data or build authority.
pub fn app_update_channel(app: &crate::package::AppIdentity) -> Result<[u8; 32], &'static str> {
    app.update_channel_id()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Describe {
        operation_id: String,
    },
    /// Account-bound signer proof; the authenticated account is the publisher.
    IssuePublisherCertificate {
        operation_id: String,
        signer: [u8; 32],
        proof: Vec<u8>,
    },
    GetPublisherKeyStatus {
        operation_id: String,
        publisher: crate::store::Id,
        signer: [u8; 32],
    },
    /// Revocation is permanent. Rotation requires a newly generated signer.
    RevokePublisherKey {
        operation_id: String,
        signer: [u8; 32],
    },
    RegisterFileProviderAccountPush {
        operation_id: String,
        token: String,
        bundle: String,
        sandbox: bool,
    },
    InvalidateFileProviderAccountPush {
        operation_id: String,
        token: String,
        bundle: String,
        sandbox: bool,
    },
    ResolveVolumeGrantee {
        operation_id: String,
        volume_id: String,
        email: String,
    },
    RegisterVolumeInvitationKey {
        operation_id: String,
        public_key: [u8; 32],
        signature: Vec<u8>,
    },
    GetVolumeInvitationKey {
        operation_id: String,
        volume: Id,
        recipient: Id,
    },
    PutVolumeInvitationPolicy {
        operation_id: String,
        volume: Id,
        clock: u64,
        recipients: Vec<Id>,
    },
    PutVolumeInvitation {
        operation_id: String,
        volume: Id,
        recipient: Id,
        clock: u64,
        invitation: Option<VolumeInvitationOffer>,
    },
    ListVolumeInvitationRecipients {
        operation_id: String,
        volume: Id,
        after: Option<Id>,
    },
    WatchVolumeInvitations {
        operation_id: String,
    },
    VolumePeerListen {
        operation_id: String,
        volume: Id,
    },
    VolumePeerSubscribe {
        operation_id: String,
        volume: Id,
    },
    VolumePeerRequest {
        operation_id: String,
        volume: Id,
        database_id: crate::store::DatabaseId,
        direction: SyncDirection,
    },
    RegisterVolume {
        operation_id: String,
        volume_id: String,
        owner_account_id: String,
    },
    /// `envelope` is the creator's sealed invitation (base64url, no padding);
    /// the service stores it for `ReadInvitation` and cannot open it.
    RegisterSession {
        app: crate::package::AppIdentity,
        operation_id: String,
        session_id: String,

        root_volume_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shared_volume_id: Option<String>,
        selector: BuildSelector,
        resolved_build_id: String,
        creator_account_id: String,
        envelope: String,
    },
    /// Content is the encoded .pbapp; build_id is its BLAKE3 Package::id().
    UploadBuild {
        operation_id: String,
        build_id: String,
        content_length: u64,
    },
    /// Only the volume owner sends database credentials; never blob/session keys.
    ProvisionReplica {
        operation_id: String,
        volume_id: String,
        owner_account_id: String,
        #[serde(default)]
        personal: bool,
        database_key: [u8; 32],
    },
    OpenSession {
        operation_id: String,
        session_id: String,
        build_id: String,
        proof: Vec<u8>,
    },
    /// Stream readiness changes without admission or invitation secrets.
    WatchInvitation {
        operation_id: String,
        session_id: String,
        proof: Vec<u8>,
    },
    GetApp {
        operation_id: String,
        app: crate::package::AppIdentity,
    },
    /// Display only; the expected build prevents adopting a changed publication.
    GetBuildIdentity {
        operation_id: String,
        build_id: String,
    },
    GetAppDisplay {
        operation_id: String,
        app: crate::package::AppIdentity,
        build_id: String,
    },
    ListApps {
        operation_id: String,
        after: Option<String>,
    },
    ListAppBuilds {
        operation_id: String,
        app: crate::package::AppIdentity,
        after: Option<String>,
    },
    SetAppEnabled {
        operation_id: String,
        app: crate::package::AppIdentity,
        enabled: bool,
        expected_revision: u64,
    },
    DeleteApp {
        operation_id: String,
        app: crate::package::AppIdentity,
        expected_revision: u64,
    },
    PublishApp {
        operation_id: String,
        app: crate::package::AppIdentity,
        build_id: String,
        #[serde(default)]
        notes: String,
        expected_revision: u64,
    },
    ResolveSession {
        operation_id: String,
        session_id: String,
        proof: Vec<u8>,
    },
    /// The registered envelope for a link holder: `{"envelope": base64url}`.
    ReadInvitation {
        operation_id: String,
        session_id: String,
        proof: Vec<u8>,
    },
    AdvanceSession {
        operation_id: String,
        session_id: String,
        build_id: String,
        expected_revision: u64,
    },
    GetCredentialVault {
        operation_id: String,
    },
    /// Opaque client-encrypted private keys, scoped to the authenticated account.
    PutCredentialVault {
        public_key: String,
        signature: String,
        operation_id: String,
        expected_revision: u64,
        ciphertext: String,
    },
    /// Opaque encrypted account records. The account always comes from the
    /// authenticated AP principal, never from a supplied volume or account ID.
    SyncAccount {
        operation_id: String,
        direction: SyncDirection,
    },
    SubscribeAccount {
        operation_id: String,
    },
    /// One bounded kvdb delta follows the request; ciphertext values remain
    /// unreadable to the AP. This is not a volume database credential grant.
    PublishAccount {
        operation_id: String,
    },
    /// The authenticated account supplies the readable name and its opaque source key.
    /// AP verifies the version at that key and orders concurrent updates to the same entry.
    PutAccountDisplayName {
        operation_id: String,
        name: String,
        record_key: String,
        clock: String,
        ciphertext: String,
    },
    /// None uses direct account authority; Some requires session admission on
    /// this connection. Catalog maintenance requires the owner. Named backing
    /// reads check their entry permission independently of catalog Read.
    /// Pull pins an authorized source view before its initial success response.
    /// Its final root, revision and policy_revision describe that view even if
    /// writes proceed during transfer. Push acknowledges the durable result.
    SyncDatabase {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        volume_id: String,
        database_id: crate::store::DatabaseId,
        /// A selected entry admits projected controls without catalog access.
        /// Valid only for pulls; named backing identity must match this entry.
        entry_scope: Option<crate::store::EntryId>,
        direction: SyncDirection,
    },
    /// ACL-checked publisher World replication in its public Merkle context.
    WorldReplica {
        operation_id: String,
        session_id: String,
        build_id: String,
        volume_id: String,
        request: WorldReplicaRequest,
    },
    /// A persistent volume subscription; None requests owner maintenance.
    /// These fields describe scope and never grant authority by themselves.
    SubscribeReplica {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        volume_id: String,
        entry_scope: Option<crate::store::EntryId>,
    },
    /// Exactly one bounded committed kvdb delta follows the request frame.
    PublishReplica {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        volume_id: String,
        database_id: crate::store::DatabaseId,
    },
    PublishBloblog {
        append_id: crate::store::Id,
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        volume_id: String,
        log_id: crate::store::EntryId,
    },
    GetBloblog {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        volume_id: String,
        log_id: crate::store::EntryId,
        cursor: Option<String>,
    },
    /// An authenticated, invitation-holding runtime offers private peer replication.
    PeerListen {
        operation_id: String,
        session_id: String,
        build_id: String,
        proof: Vec<u8>,
    },
    PeerRequest {
        operation_id: String,
        session_id: String,
        build_id: String,
        proof: Vec<u8>,
        volume_id: String,
        database_id: crate::store::DatabaseId,
        direction: SyncDirection,
    },
    PeerReply {
        operation_id: String,
        request_id: String,
    },
    UploadBlob {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        version: crate::store::volume_index::BlobVersion,
        content_length: u64,
    },
    DownloadBlob {
        operation_id: String,
        session_id: Option<String>,
        build_id: Option<String>,
        version: crate::store::volume_index::BlobVersion,
    },
    /// Stored logical user bytes attributed to the volume owner.
    VolumeUsage {
        operation_id: String,
        volume_id: String,
    },
    GetBlob {
        operation_id: String,
        hash: String,
    },
}

// fuzz: ap_codec
fn validate_provider_push(token: &str, bundle: &str) -> Result<(), &'static str> {
    if token.is_empty()
        || token.len() > 256
        || !token.len().is_multiple_of(2)
        || !is_hex(token, token.len())
        || !["playbit.engine", "playbit.engine.debug"].contains(&bundle)
    {
        return Err("invalid File Provider push registration");
    }
    Ok(())
}

impl ServiceRequest {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Describe { operation_id }
            | Self::IssuePublisherCertificate { operation_id, .. }
            | Self::GetPublisherKeyStatus { operation_id, .. }
            | Self::RevokePublisherKey { operation_id, .. }
            | Self::RegisterFileProviderAccountPush { operation_id, .. }
            | Self::InvalidateFileProviderAccountPush { operation_id, .. } => operation_id,
            Self::ResolveVolumeGrantee { operation_id, .. }
            | Self::RegisterVolumeInvitationKey { operation_id, .. }
            | Self::GetVolumeInvitationKey { operation_id, .. }
            | Self::PutVolumeInvitationPolicy { operation_id, .. }
            | Self::PutVolumeInvitation { operation_id, .. }
            | Self::ListVolumeInvitationRecipients { operation_id, .. }
            | Self::WatchVolumeInvitations { operation_id }
            | Self::VolumePeerListen { operation_id, .. }
            | Self::VolumePeerSubscribe { operation_id, .. }
            | Self::VolumePeerRequest { operation_id, .. }
            | Self::RegisterVolume { operation_id, .. }
            | Self::RegisterSession { operation_id, .. }
            | Self::UploadBuild { operation_id, .. }
            | Self::ProvisionReplica { operation_id, .. }
            | Self::OpenSession { operation_id, .. }
            | Self::WatchInvitation { operation_id, .. }
            | Self::GetApp { operation_id, .. }
            | Self::GetBuildIdentity { operation_id, .. }
            | Self::GetAppDisplay { operation_id, .. }
            | Self::ListApps { operation_id, .. }
            | Self::ListAppBuilds { operation_id, .. }
            | Self::SetAppEnabled { operation_id, .. }
            | Self::DeleteApp { operation_id, .. }
            | Self::PublishApp { operation_id, .. }
            | Self::ResolveSession { operation_id, .. }
            | Self::ReadInvitation { operation_id, .. }
            | Self::AdvanceSession { operation_id, .. }
            | Self::GetCredentialVault { operation_id }
            | Self::PutCredentialVault { operation_id, .. }
            | Self::SyncAccount { operation_id, .. }
            | Self::SubscribeAccount { operation_id }
            | Self::PublishAccount { operation_id }
            | Self::PutAccountDisplayName { operation_id, .. }
            | Self::SyncDatabase { operation_id, .. }
            | Self::WorldReplica { operation_id, .. }
            | Self::SubscribeReplica { operation_id, .. }
            | Self::PublishReplica { operation_id, .. }
            | Self::PublishBloblog { operation_id, .. }
            | Self::GetBloblog { operation_id, .. }
            | Self::GetBlob { operation_id, .. }
            | Self::PeerListen { operation_id, .. }
            | Self::PeerRequest { operation_id, .. }
            | Self::PeerReply { operation_id, .. }
            | Self::UploadBlob { operation_id, .. }
            | Self::DownloadBlob { operation_id, .. }
            | Self::VolumeUsage { operation_id, .. } => operation_id,
        }
    }

    // fuzz: ap_codec
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_identity()?;
        self.validate_running_scope()?;
        match self {
            Self::RegisterVolumeInvitationKey { .. }
            | Self::PutVolumeInvitation { .. }
            | Self::PutVolumeInvitationPolicy { .. } => self.validate_volume_invitation(),
            Self::GetVolumeInvitationKey { .. }
            | Self::ListVolumeInvitationRecipients { .. }
            | Self::WatchVolumeInvitations { .. }
            | Self::VolumePeerListen { .. }
            | Self::VolumePeerSubscribe { .. }
            | Self::VolumePeerRequest { .. } => Ok(()),
            Self::PutAccountDisplayName { name, record_key, clock, ciphertext, .. } => {
                validate_id32(record_key, "invalid display name record key")?;
                display_name_version(name, clock, ciphertext).map(|_| ())
            }
            Self::IssuePublisherCertificate { .. }
            | Self::GetPublisherKeyStatus { .. }
            | Self::RevokePublisherKey { .. } => self.validate_publisher(),
            Self::RegisterFileProviderAccountPush { token, bundle, .. }
            | Self::InvalidateFileProviderAccountPush { token, bundle, .. } => {
                validate_provider_push(token, bundle)
            }
            Self::GetBuildIdentity { build_id, .. } => {
                validate_id32(build_id, "invalid build identity")
            }
            Self::ListAppBuilds { .. }
            | Self::GetApp { .. }
            | Self::GetAppDisplay { .. }
            | Self::SetAppEnabled { .. }
            | Self::DeleteApp { .. }
            | Self::PublishApp { .. } => self.validate_app(),
            Self::ListApps { .. } => self.validate_list_apps(),
            Self::ResolveSession { .. } | Self::ReadInvitation { .. } => {
                self.validate_resolve_session()
            }
            Self::AdvanceSession { .. } => self.validate_advance_session(),
            Self::Describe { .. }
            | Self::GetCredentialVault { .. }
            | Self::SyncAccount { .. }
            | Self::SubscribeAccount { .. }
            | Self::PublishAccount { .. } => Ok(()),
            Self::PutCredentialVault { .. } => self.validate_put_credential_vault(),
            Self::ResolveVolumeGrantee { .. } => self.validate_resolve_volume_grantee(),
            Self::RegisterVolume { .. } => self.validate_register_volume(),
            Self::RegisterSession { .. } => self.validate_register_session(),
            Self::ProvisionReplica { .. } => self.validate_provision_replica(),
            Self::OpenSession { .. } | Self::WatchInvitation { .. } => self.validate_open_session(),
            Self::PeerListen { .. } => self.validate_peer_listen(),
            Self::PeerRequest { .. } => self.validate_peer_request(),
            Self::PeerReply { .. } => self.validate_peer_reply(),
            Self::VolumeUsage { .. } => self.validate_volume_usage(),
            Self::UploadBlob { .. } | Self::DownloadBlob { .. } => self.validate_upload_blob(),
            Self::GetBlob { hash, .. } => validate_id32(hash, "invalid blob hash"),
            Self::SubscribeReplica { .. }
            | Self::PublishBloblog { .. }
            | Self::GetBloblog { .. } => self.validate_subscribe_replica(),
            Self::SyncDatabase { .. } | Self::PublishReplica { .. } => {
                self.validate_sync_database()
            }
            Self::WorldReplica { session_id, build_id, volume_id, request, .. } => {
                request.validate_scope(session_id, build_id, volume_id)
            }
            Self::UploadBuild { .. } => self.validate_upload_build(),
        }
    }

    // fuzz: ap_codec
    fn validate_volume_invitation(&self) -> Result<(), &'static str> {
        match self {
            Self::RegisterVolumeInvitationKey { public_key, signature, .. } => {
                (*public_key != [0; 32] && signature.len() == 64)
                    .then_some(())
                    .ok_or("invalid volume invitation public key proof")
            }
            Self::PutVolumeInvitation { clock, invitation, .. } => {
                if *clock == 0 {
                    return Err("invalid volume invitation clock");
                }
                if let Some(offer) = invitation {
                    canonical_base64url(&offer.envelope, INVITATION_ENVELOPE_LIMIT)
                        .map_err(|_| "invalid volume invitation envelope")?;
                }
                Ok(())
            }
            Self::PutVolumeInvitationPolicy { clock, recipients, .. } => {
                // Encoded once in an AP record, within the request and 400-KiB
                // central item limits even at maximum canonical ID width.
                if *clock == 0
                    || recipients.len() > 12_000
                    || recipients.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    return Err("invalid volume invitation recipient policy");
                }
                Ok(())
            }
            _ => unreachable!("volume invitation validation has a volume invitation operation"),
        }
    }

    // fuzz: ap_codec
    fn validate_publisher(&self) -> Result<(), &'static str> {
        match self {
            Self::IssuePublisherCertificate { signer, proof, .. } => {
                if *signer == [0; 32] || proof.len() != 64 {
                    return Err("invalid publisher signer or proof");
                }
                Ok(())
            }
            Self::GetPublisherKeyStatus { publisher, signer, .. } => {
                if *publisher == crate::store::Id::from_bytes([0; 16]) || *signer == [0; 32] {
                    return Err("invalid publisher-key binding");
                }
                Ok(())
            }
            Self::RevokePublisherKey { signer, .. } => {
                if *signer == [0; 32] {
                    return Err("invalid publisher signer");
                }
                Ok(())
            }
            _ => unreachable!("publisher validation has a publisher operation"),
        }
    }

    // fuzz: ap_codec
    fn validate_identity(&self) -> Result<(), &'static str> {
        if !crate::identifier::valid(self.operation_id(), 16) {
            return Err("operation ID must be a base58-encoded 16-byte value");
        }
        if let Self::GetBloblog { cursor, .. } = self
            && cursor.as_ref().is_some_and(|c| crate::store::bloblog::Cursor::validate(c).is_err())
        {
            return Err("invalid bloblog scope or cursor");
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_running_scope(&self) -> Result<(), &'static str> {
        match self {
            Self::OpenSession { build_id, .. }
            | Self::PeerListen { build_id, .. }
            | Self::PeerRequest { build_id, .. }
                if !crate::identifier::valid(build_id, 32) =>
            {
                return Err("invalid running app version");
            }
            Self::SyncDatabase { session_id, build_id, .. }
            | Self::SubscribeReplica { session_id, build_id, .. }
            | Self::PublishReplica { session_id, build_id, .. }
            | Self::PublishBloblog { session_id, build_id, .. }
            | Self::GetBloblog { session_id, build_id, .. }
            | Self::UploadBlob { session_id, build_id, .. }
            | Self::DownloadBlob { session_id, build_id, .. }
                if (session_id.is_some() != build_id.is_some()
                    || build_id
                        .as_ref()
                        .is_some_and(|version| !crate::identifier::valid(version, 32))) =>
            {
                return Err("session and running version must be supplied together");
            }
            _ => {}
        }
        Ok(())
    }

    // fuzz: ap_codec

    // fuzz: ap_codec

    // fuzz: ap_codec

    fn validate_app(&self) -> Result<(), &'static str> {
        let app = match self {
            Self::GetApp { app, .. }
            | Self::GetAppDisplay { app, .. }
            | Self::SetAppEnabled { app, .. }
            | Self::DeleteApp { app, .. }
            | Self::PublishApp { app, .. }
            | Self::ListAppBuilds { app, .. } => app,
            _ => unreachable!("validator dispatched only for its operation kind"),
        };
        app.validate()?;
        if let Self::GetAppDisplay { build_id, .. } = self
            && !crate::identifier::valid(build_id, 32)
        {
            return Err("invalid display build ID");
        }
        if matches!(
            self,
            Self::SetAppEnabled { expected_revision: u64::MAX, .. }
                | Self::DeleteApp { expected_revision: u64::MAX, .. }
        ) {
            return Err("invalid app revision");
        }
        if let Self::PublishApp { build_id, notes, expected_revision, .. } = self
            && (!crate::identifier::valid(build_id, 32)
                || !valid_app_notes(notes)
                || *expected_revision == u64::MAX)
        {
            return Err("invalid app publication");
        }
        if let Self::ListAppBuilds { after: Some(after), .. } = self
            && !crate::identifier::valid(after, 32)
        {
            return Err("invalid app history cursor");
        }
        Ok(())
    }

    fn validate_list_apps(&self) -> Result<(), &'static str> {
        if let Self::ListApps { after: Some(after), .. } = self
            && !crate::package::valid_app_id(after)
        {
            return Err("invalid app cursor");
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_resolve_session(&self) -> Result<(), &'static str> {
        match self {
            Self::ResolveSession { session_id, proof, .. }
            | Self::ReadInvitation { session_id, proof, .. } => {
                if !crate::identifier::valid(session_id, 32) || proof.len() != 64 {
                    return Err("invalid session proof");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_advance_session(&self) -> Result<(), &'static str> {
        match self {
            Self::AdvanceSession { session_id, build_id, expected_revision, .. } => {
                if !crate::identifier::valid(session_id, 32)
                    || !crate::identifier::valid(build_id, 32)
                    || *expected_revision == u64::MAX
                {
                    return Err("invalid session version advance");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_put_credential_vault(&self) -> Result<(), &'static str> {
        match self {
            Self::PutCredentialVault {
                expected_revision,
                ciphertext,
                public_key,
                signature,
                ..
            } => {
                if !is_hex(public_key, 64) || !is_hex(signature, 128) {
                    return Err("invalid account key proof");
                }
                if *expected_revision == u64::MAX {
                    return Err("credential vault revision exhausted");
                }
                canonical_base64url(ciphertext, crate::store::recovery::MAX_CIPHERTEXT_BYTES)
                    .map_err(|noncanonical| {
                        if noncanonical {
                            "noncanonical credential vault ciphertext"
                        } else {
                            "invalid credential vault ciphertext"
                        }
                    })?;
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_resolve_volume_grantee(&self) -> Result<(), &'static str> {
        match self {
            Self::ResolveVolumeGrantee { volume_id, email, .. } => {
                if !crate::identifier::valid(volume_id, 16) || !valid_account_email(email) {
                    return Err("invalid volume or recipient email");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_register_volume(&self) -> Result<(), &'static str> {
        match self {
            Self::RegisterVolume { volume_id, owner_account_id, .. } => {
                if !crate::identifier::valid(volume_id, 16)
                    || !crate::identifier::valid(owner_account_id, 16)
                {
                    return Err("invalid volume or owner account ID");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_register_session(&self) -> Result<(), &'static str> {
        match self {
            Self::RegisterSession {
                app,
                session_id,

                root_volume_id,
                shared_volume_id,
                selector,
                resolved_build_id,
                creator_account_id,
                envelope,
                ..
            } => {
                app.validate()?;
                if canonical_base64url(envelope, INVITATION_ENVELOPE_LIMIT).is_err() {
                    return Err("invalid invitation envelope");
                }
                if !crate::identifier::valid(session_id, 32)
                    || !crate::identifier::valid(resolved_build_id, 32)
                    || [root_volume_id, creator_account_id]
                        .iter()
                        .any(|id| !crate::identifier::valid(id, 16))
                {
                    return Err("invalid session registration identity");
                }
                if let Some(shared) = shared_volume_id {
                    use crate::package::AutomaticVolumeRole;
                    let session = crate::identifier::decode(session_id)
                        .map_err(|_| "invalid Shared session identity")?;
                    let expected_shared =
                        app.volume_id(AutomaticVolumeRole::Session, Some(session), None, None)?;
                    let expected_root =
                        app.volume_id(AutomaticVolumeRole::Root, None, None, None)?;
                    if shared != &expected_shared.to_string()
                        || root_volume_id != &expected_root.to_string()
                    {
                        return Err("Shared session binding differs");
                    }
                }
                match selector {
                    BuildSelector::Exact { build_id } if build_id != resolved_build_id => {
                        return Err("exact selector differs from resolved version");
                    }
                    BuildSelector::App { app: selected } if selected != app => {
                        return Err("app selector differs from session identity");
                    }
                    _ => {}
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_provision_replica(&self) -> Result<(), &'static str> {
        match self {
            Self::ProvisionReplica { volume_id, owner_account_id, .. } => {
                if !crate::identifier::valid(volume_id, 16)
                    || !crate::identifier::valid(owner_account_id, 16)
                {
                    return Err("invalid replica provisioning");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_open_session(&self) -> Result<(), &'static str> {
        match self {
            Self::OpenSession { session_id, proof, .. }
            | Self::WatchInvitation { session_id, proof, .. } => {
                if !crate::identifier::valid(session_id, 32) || proof.len() != 64 {
                    return Err("invalid session proof");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_peer_listen(&self) -> Result<(), &'static str> {
        match self {
            Self::PeerListen { session_id, proof, .. } => {
                if !crate::identifier::valid(session_id, 32) || proof.len() != 64 {
                    return Err("invalid peer session proof");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_peer_request(&self) -> Result<(), &'static str> {
        match self {
            Self::PeerRequest { session_id, proof, volume_id, .. } => {
                if !crate::identifier::valid(session_id, 32)
                    || proof.len() != 64
                    || !crate::identifier::valid(volume_id, 16)
                {
                    return Err("invalid peer database request");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_peer_reply(&self) -> Result<(), &'static str> {
        match self {
            Self::PeerReply { request_id, .. } => {
                if !crate::identifier::valid(request_id, 16) {
                    return Err("invalid peer request ID");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_volume_usage(&self) -> Result<(), &'static str> {
        match self {
            Self::VolumeUsage { volume_id, .. } => {
                if !crate::identifier::valid(volume_id, 16) {
                    return Err("invalid volume ID");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_upload_blob(&self) -> Result<(), &'static str> {
        match self {
            Self::UploadBlob { session_id, version, .. }
            | Self::DownloadBlob { session_id, version, .. } => {
                if crate::store::blob_envelope_len(version.len_bytes).is_err()
                    || session_id.as_ref().is_some_and(|id| !crate::identifier::valid(id, 32))
                {
                    return Err("invalid blob reference");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_subscribe_replica(&self) -> Result<(), &'static str> {
        match self {
            Self::SubscribeReplica { session_id, volume_id, .. }
            | Self::PublishBloblog { session_id, volume_id, .. }
            | Self::GetBloblog { session_id, volume_id, .. } => {
                if !crate::identifier::valid(volume_id, 16)
                    || session_id.as_ref().is_some_and(|id| !crate::identifier::valid(id, 32))
                {
                    return Err("invalid replica scope");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_sync_database(&self) -> Result<(), &'static str> {
        match self {
            Self::SyncDatabase { session_id, volume_id, .. }
            | Self::PublishReplica { session_id, volume_id, .. } => {
                if !crate::identifier::valid(volume_id, 16)
                    || session_id.as_ref().is_some_and(|s| !crate::identifier::valid(s, 32))
                {
                    return Err("invalid database synchronization request");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        if let Self::SyncDatabase { entry_scope: Some(entry), database_id, direction, .. } = self
            && (matches!(database_id, crate::store::DatabaseId::Entry(id) if id != entry)
                || (*direction == SyncDirection::Push
                    && *database_id == crate::store::DatabaseId::Revocations))
        {
            return Err("entry scope requires matching entry data or index facts");
        }
        Ok(())
    }

    // fuzz: ap_codec
    fn validate_upload_build(&self) -> Result<(), &'static str> {
        match self {
            Self::UploadBuild { build_id, .. } => {
                if !crate::identifier::valid(build_id, 32) {
                    return Err("invalid app or version ID");
                }
            }
            _ => unreachable!("validator dispatched only for its operation kind"),
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDirection {
    Pull,
    Push,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorldReplicaRequest {
    Open,
    Sync {
        database_id: crate::store::DatabaseId,
        direction: SyncDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_scope: Option<crate::store::EntryId>,
    },
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_scope: Option<crate::store::EntryId>,
    },
    Publish {
        database_id: crate::store::DatabaseId,
    },
}

impl WorldReplicaRequest {
    fn validate_scope(
        &self,
        session_id: &str,
        build_id: &str,
        volume_id: &str,
    ) -> Result<(), &'static str> {
        if !crate::identifier::valid(session_id, 32)
            || !crate::identifier::valid(build_id, 32)
            || !crate::identifier::valid(volume_id, 16)
        {
            return Err("invalid publisher World scope");
        }
        if let WorldReplicaRequest::Sync { database_id, direction, entry_scope: Some(entry) } = self
            && (matches!(database_id,crate::store::DatabaseId::Entry(id) if id != entry)
                || *direction == SyncDirection::Push
                    && *database_id == crate::store::DatabaseId::Revocations)
        {
            return Err("entry scope requires matching entry data or index facts");
        }
        Ok(())
    }
}

/// Resumable invitation progress. Ready is advisory; opening rechecks authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvitationStatus {
    WaitingRegistration,
    WaitingApp,
    WaitingDatabase,
    /// Private data must be obtained from an authorized runtime peer.
    WaitingPeer,
    Ready {
        build_id: String,
    },
    Denied {
        message: String,
    },
}

/// AP-authenticated endpoints and scope of one opaque private-database relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEvent {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
    pub build_id: String,
    pub request_id: String,
    pub requester_account_id: String,
    pub responder_account_id: String,
    pub session_id: String,
    pub volume_id: String,
    pub database_id: crate::store::DatabaseId,
    pub direction: SyncDirection,
}

/// Domain-separated invitation possession proof: sign this prefix plus raw account ID.
pub const SESSION_PROOF_DOMAIN: &[u8] = b"app.playbit.session.join.v1\0";

/// Bound stored/returned JSON, including escapes, not just source string bytes.
pub fn valid_app_notes(notes: &str) -> bool {
    notes.len() <= 32768
        && notes
            .bytes()
            .try_fold(0usize, |size, byte| {
                let encoded = match byte {
                    b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                    0..=31 => 6,
                    _ => 1,
                };
                (size + encoded <= 32768).then_some(size + encoded)
            })
            .is_some()
}

/// Unpadded base64url of at most `max_bytes`; `Err(true)` marks an encoding
/// whose unused trailing bits are not zero.
pub fn canonical_base64url(value: &str, max_bytes: usize) -> Result<(), bool> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    if value.is_empty()
        || value.len() / 4 * 3 + value.len() % 4 * 3 / 4 > max_bytes
        || value.len() % 4 == 1
        || !value.bytes().all(|byte| ALPHABET.contains(&byte))
    {
        return Err(false);
    }
    let last = ALPHABET
        .iter()
        .position(|byte| Some(byte) == value.as_bytes().last())
        .expect("the nonempty value contains only base64url alphabet bytes");
    if (value.len() % 4 == 2 && last & 15 != 0) || (value.len() % 4 == 3 && last & 3 != 0) {
        return Err(true);
    }
    Ok(())
}

fn validate_id32(value: &str, error: &'static str) -> Result<(), &'static str> {
    crate::identifier::valid(value, 32).then_some(()).ok_or(error)
}

pub fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub timing: super::timing::Timings,
    pub ok: bool,
    pub operation_id: String,
    pub error: Option<String>,
    pub code: Option<String>,
    pub denial: Option<ServiceDenial>,
    pub data: Option<serde_json::Value>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn every_operation_kind_validates_identity_and_round_trips() {
        // Exercise every wire operation with valid fields, then corrupt each identity.
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tests/fixtures/ap-requests.json")).unwrap();
        assert_eq!(fixtures.len(), 52);
        for fixture in fixtures {
            let request: ServiceRequest = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(request.validate(), Ok(()), "{}", fixture["op"]);
            if fixture["version"].is_object() {
                for (path, value) in [
                    (
                        "/version/address/entry",
                        serde_json::json!(crate::identifier::encode(&[0x00; 15])),
                    ),
                    ("/version/address/volume", serde_json::json!("invalid")),
                    ("/version/revision", serde_json::json!("invalid")),
                    ("/version/len_bytes", serde_json::json!(u64::MAX)),
                    ("/version/digest", serde_json::json!([1])),
                    ("/version/encrypted_key", serde_json::json!([1])),
                ] {
                    let mut malformed = fixture.clone();
                    *malformed.pointer_mut(path).unwrap() = value;
                    let invalid = serde_json::from_value::<ServiceRequest>(malformed);
                    assert!(invalid.is_err() || invalid.unwrap().validate().is_err(), "{path}");
                }
            }
            let encoded = serde_json::to_vec(&request).unwrap();
            assert_eq!(serde_json::from_slice::<ServiceRequest>(&encoded).unwrap(), request);
            for field in fixture.as_object().unwrap().keys().filter(|name| name.ends_with("_id")) {
                if !fixture[field].is_string() {
                    continue;
                }
                let mut malformed = fixture.clone();
                malformed[field] = serde_json::json!("Z");
                let invalid = serde_json::from_value::<ServiceRequest>(malformed);
                assert!(
                    invalid.is_err() || invalid.unwrap().validate().is_err(),
                    "{} {field}",
                    fixture["op"]
                );
            }
        }
    }

    #[test]
    fn base64_limit_arithmetic_does_not_overflow() {
        // Limits are byte counts, including the largest representable count.
        for limit in [1, 2, usize::MAX / 2, usize::MAX] {
            assert_eq!(canonical_base64url("AA", limit), Ok(()));
        }
        assert_eq!(canonical_base64url("AA", 0), Err(false));
        assert_eq!(canonical_base64url("AAA", 1), Err(false));
        assert_eq!(canonical_base64url("AAA", 2), Ok(()));
        assert_eq!(canonical_base64url("AB", usize::MAX), Err(true));
    }

    #[test]
    fn build_display_rejects_changed_id_and_unbounded_or_ambiguous_artwork() {
        let build = crate::identifier::encode(&[0xab; 32]);
        let valid = BuildDisplay {
            app_id: "test".into(),
            build_id: build.clone(),
            name: "TODO".into(),
            icon: None,
            icon_error: None,
        };
        assert!(valid.validate(&build).is_ok());
        assert!(valid.validate(&crate::identifier::encode(&[0xcd; 32])).is_err());
        for name in [String::new(), "x".repeat(257)] {
            assert!(BuildDisplay { name, ..valid.clone() }.validate(&build).is_err());
        }
        for icon in ["f".into(), "GG".into(), "00".repeat(65537)] {
            assert!(BuildDisplay { icon: Some(icon), ..valid.clone() }.validate(&build).is_err());
        }
        let maximum = BuildDisplay {
            name: "\u{0001}".repeat(256),
            icon: Some("00".repeat(65536)),
            ..valid.clone()
        };
        assert!(maximum.validate(&build).is_ok());
        let mut timing = super::super::timing::Timings::default();
        for stage in super::super::timing::Stage::ALL {
            timing.push(super::super::timing::Timing { stage, duration_ns: u64::MAX }).unwrap();
        }
        let response = ServiceResponse {
            timing,
            operation_id: crate::identifier::encode(&[0x11; 16]),
            ok: true,
            error: None,
            code: None,
            denial: None,
            data: Some(serde_json::to_value(&maximum).unwrap()),
        };
        assert!(serde_json::to_vec(&response).unwrap().len() <= BUILD_DISPLAY_LIMIT);
        assert!(
            BuildDisplay { icon_error: Some("invalid icon".into()), ..maximum }
                .validate(&build)
                .is_err()
        );
        assert!(
            BuildDisplay { icon_error: Some("e".repeat(257)), ..valid }.validate(&build).is_err()
        );
        for id in [build.clone(), "f".repeat(63), "G".repeat(64)] {
            let request = ServiceRequest::GetAppDisplay {
                operation_id: crate::identifier::encode(&[0x11; 16]),
                app: crate::package::AppIdentity {
                    publisher: crate::store::Id::from_bytes([0x22; 16]),
                    app_id: "test".into(),
                },
                build_id: id.clone(),
            };
            assert_eq!(request.validate().is_ok(), id == build);
        }
    }

    #[test]
    fn app_lifecycle_and_list_validate_exact_contracts() {
        assert!(valid_app_notes(&"a".repeat(32768)));
        assert!(valid_app_notes(&"\n".repeat(16384)));
        assert!(!valid_app_notes(&"\n".repeat(16385)));
        assert!(!valid_app_notes(&"\u{0001}".repeat(5462)));
        let operation_id = crate::identifier::encode(&[0x11; 16]);
        for after in [None, Some("test.app".into())] {
            let request = ServiceRequest::ListApps { operation_id: operation_id.clone(), after };
            assert!(request.validate().is_ok());
            assert_eq!(
                serde_json::from_slice::<ServiceRequest>(&serde_json::to_vec(&request).unwrap())
                    .unwrap(),
                request
            );
        }
        for cursor in ["../other".into(), "A".repeat(32), "1".repeat(65)] {
            assert!(
                ServiceRequest::ListApps {
                    operation_id: operation_id.clone(),
                    after: Some(cursor)
                }
                .validate()
                .is_err()
            );
        }
        for revision in [0, 1, u64::MAX] {
            for request in [
                ServiceRequest::SetAppEnabled {
                    operation_id: operation_id.clone(),
                    app: crate::package::AppIdentity {
                        publisher: crate::store::Id::from_bytes([0x22; 16]),
                        app_id: "test".into(),
                    },
                    enabled: false,
                    expected_revision: revision,
                },
                ServiceRequest::DeleteApp {
                    operation_id: operation_id.clone(),
                    app: crate::package::AppIdentity {
                        publisher: crate::store::Id::from_bytes([0x22; 16]),
                        app_id: "test".into(),
                    },
                    expected_revision: revision,
                },
            ] {
                assert_eq!(request.validate().is_ok(), revision != u64::MAX);
                assert_eq!(request.operation_id(), operation_id);
            }
        }
    }

    #[test]
    fn app_operations_cannot_omit_or_mix_running_version_bindings() {
        for session in [None, Some(crate::identifier::encode(&[0x11; 32]))] {
            for version in [None, Some(crate::identifier::encode(&[0x22; 32]))] {
                let request = ServiceRequest::SyncDatabase {
                    entry_scope: None,
                    operation_id: crate::identifier::encode(&[0x33; 16]),
                    session_id: session.clone(),
                    build_id: version.clone(),
                    volume_id: crate::identifier::encode(&[0x44; 16]),
                    database_id: crate::store::DatabaseId::Index,
                    direction: SyncDirection::Pull,
                };
                assert_eq!(request.validate().is_ok(), session.is_some() == version.is_some());
            }
        }
        let missing = serde_json::json!({"op":"open_session","operation_id":crate::identifier::encode(&[0x33; 16]),
            "session_id":crate::identifier::encode(&[0x11; 32]),"proof":vec![0;64]});
        assert!(serde_json::from_value::<ServiceRequest>(missing).is_err());
    }

    #[test]
    fn selected_snapshots_validate_backing_identity_and_reserved_write_scope() {
        use crate::store::{DatabaseId, EntryId};
        let entry = EntryId::from_bytes([7; 15]).unwrap();
        for direction in [SyncDirection::Pull, SyncDirection::Push] {
            for database in [DatabaseId::Index, DatabaseId::Revocations, DatabaseId::Entry(entry)] {
                let mut request = ServiceRequest::SyncDatabase {
                    operation_id: crate::identifier::encode(&[0x33; 16]),
                    entry_scope: Some(entry),
                    session_id: None,
                    build_id: None,
                    volume_id: crate::identifier::encode(&[0x44; 16]),
                    database_id: database,
                    direction,
                };
                assert_eq!(
                    request.validate().is_ok(),
                    direction == SyncDirection::Pull || database != DatabaseId::Revocations
                );
                if let ServiceRequest::SyncDatabase { database_id, .. } = &mut request {
                    *database_id = DatabaseId::Entry(EntryId::from_bytes([8; 15]).unwrap());
                }
                assert!(request.validate().is_err());
            }
        }
    }

    #[test]
    fn live_replica_requests_validate_scope_identity_and_database() {
        for op in ["subscribe_replica", "publish_replica"] {
            for session in [None, Some(crate::identifier::encode(&[0x11; 32]))] {
                for version in [None, Some(crate::identifier::encode(&[0x22; 32]))] {
                    let mut value = serde_json::json!({
                        "op": op, "operation_id": crate::identifier::encode(&[0x33; 16]),
                        "session_id": session, "build_id": version,
                        "volume_id": crate::identifier::encode(&[0x44; 16])
                    });
                    if op == "publish_replica" {
                        value["database_id"] = crate::identifier::encode(&[0x55; 15]).into();
                    } else {
                        value["entry_scope"] = serde_json::Value::Null;
                    }
                    let request: ServiceRequest = serde_json::from_value(value.clone()).unwrap();
                    assert_eq!(request.validate().is_ok(), session.is_some() == version.is_some());
                    assert_eq!(request.operation_id(), crate::identifier::encode(&[0x33; 16]));
                    assert_eq!(serde_json::to_value(&request).unwrap(), value);
                    if session.is_some() != version.is_some() {
                        continue;
                    }
                    for field in ["operation_id", "volume_id", "session_id", "build_id"] {
                        for bad in ["", "AB", "../volume", &"a".repeat(65)] {
                            let mut invalid = value.clone();
                            invalid[field] = bad.into();
                            assert!(
                                serde_json::from_value::<ServiceRequest>(invalid)
                                    .unwrap()
                                    .validate()
                                    .is_err()
                            );
                        }
                    }
                    if op == "publish_replica" {
                        for database in
                            ["index", "revocations", &crate::identifier::encode(&[0x55; 15])]
                        {
                            let mut candidate = value.clone();
                            candidate["database_id"] = database.into();
                            assert!(
                                serde_json::from_value::<ServiceRequest>(candidate)
                                    .unwrap()
                                    .validate()
                                    .is_ok()
                            );
                        }
                        for database in [
                            "",
                            "1",
                            "Index",
                            "../volume",
                            &crate::identifier::encode(&[0x00; 15]),
                            &crate::identifier::encode(&[0x55; 16]),
                        ] {
                            let mut candidate = value.clone();
                            candidate["database_id"] = database.into();
                            assert!(serde_json::from_value::<ServiceRequest>(candidate).is_err());
                        }
                        for database in [0, 1, 2, 3, u32::MAX] {
                            let mut candidate = value.clone();
                            candidate["database_id"] = database.into();
                            assert!(serde_json::from_value::<ServiceRequest>(candidate).is_err());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn registration_retains_locally_allocated_identity() {
        let request = ServiceRequest::RegisterVolume {
            operation_id: crate::identifier::encode(&[0x12; 16]),
            volume_id: crate::identifier::encode(&[0x34; 16]),
            owner_account_id: crate::identifier::encode(&[0x56; 16]),
        };
        request.validate().unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<ServiceRequest>(&encoded).unwrap(), request);
        assert!(encoded.contains("register_volume"));
        assert!(!crate::identifier::valid(&"AB".repeat(16), 16));
    }

    #[test]
    fn provisioning_contains_only_immutable_identity_and_credentials() {
        let request = ServiceRequest::ProvisionReplica {
            operation_id: crate::identifier::encode(&[0x12; 16]),
            volume_id: crate::identifier::encode(&[0x34; 16]),
            owner_account_id: crate::identifier::encode(&[0x56; 16]),
            personal: true,
            database_key: [7; 32],
        };
        request.validate().unwrap();
        let mut encoded = serde_json::to_value(&request).unwrap();
        assert!(encoded.get("name").is_none());
        assert_eq!(serde_json::from_value::<ServiceRequest>(encoded.clone()).unwrap(), request);
        encoded["name"] = "Mutable name belongs in DB1".into();
        assert!(serde_json::from_value::<ServiceRequest>(encoded).is_err());
    }

    #[test]
    fn invitation_status_requires_a_complete_possession_proof() {
        for length in [0, 63, 64, 65] {
            let request = ServiceRequest::WatchInvitation {
                operation_id: crate::identifier::encode(&[0x12; 16]),
                session_id: crate::identifier::encode(&[0x34; 32]),
                proof: vec![0; length],
            };
            assert_eq!(request.validate().is_ok(), length == 64);
        }
        let status = InvitationStatus::WaitingRegistration;
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({"kind":"waiting_registration"})
        );
    }

    #[test]
    fn credential_vault_rejects_invalid_or_oversized_encoding() {
        // Test canonical encodings at the current decoded ciphertext boundary.
        use crate::store::recovery::MAX_CIPHERTEXT_BYTES;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let request = |ciphertext: String, expected_revision| ServiceRequest::PutCredentialVault {
            public_key: "11".repeat(32),
            signature: "22".repeat(64),
            operation_id: crate::identifier::encode(&[0x12; 16]),
            expected_revision,
            ciphertext,
        };
        for ciphertext in ["", "A", "AA=", "AA+", "AA/", "AB", "AAB"] {
            assert!(request(ciphertext.into(), 0).validate().is_err(), "{ciphertext}");
        }
        for ciphertext in ["AA", "AAA", "AAAA", "AA-_", "AA8"] {
            request(ciphertext.into(), 0).validate().unwrap();
        }
        request(URL_SAFE_NO_PAD.encode(vec![0; MAX_CIPHERTEXT_BYTES]), 0).validate().unwrap();
        assert!(
            request(URL_SAFE_NO_PAD.encode(vec![0; MAX_CIPHERTEXT_BYTES + 1]), 0)
                .validate()
                .is_err()
        );
        assert!(request("AA".into(), u64::MAX).validate().is_err());
    }

    #[test]
    fn exact_selection_cannot_claim_a_different_version() {
        let request = ServiceRequest::RegisterSession {
            app: crate::package::AppIdentity {
                publisher: crate::store::Id::from_bytes([1; 16]),
                app_id: "test".into(),
            },
            operation_id: crate::identifier::encode(&[0x12; 16]),
            session_id: crate::identifier::encode(&[0x34; 32]),

            root_volume_id: crate::identifier::encode(&[0x78; 16]),
            shared_volume_id: None,

            selector: BuildSelector::Exact { build_id: crate::identifier::encode(&[0xab; 32]) },
            resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
            creator_account_id: crate::identifier::encode(&[0xef; 16]),
            envelope: "AQID".into(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn session_registration_bounds_its_envelope() {
        let request = |envelope: String| ServiceRequest::RegisterSession {
            app: crate::package::AppIdentity {
                publisher: crate::store::Id::from_bytes([1; 16]),
                app_id: "test".into(),
            },
            operation_id: crate::identifier::encode(&[0x12; 16]),
            session_id: crate::identifier::encode(&[0x34; 32]),

            root_volume_id: crate::identifier::encode(&[0x78; 16]),
            shared_volume_id: None,
            selector: BuildSelector::Exact { build_id: crate::identifier::encode(&[0xab; 32]) },
            resolved_build_id: crate::identifier::encode(&[0xab; 32]),
            creator_account_id: crate::identifier::encode(&[0xef; 16]),
            envelope,
        };
        request("AQID".into()).validate().unwrap();
        request("A".repeat(INVITATION_ENVELOPE_LIMIT / 3 * 4)).validate().unwrap();
        for envelope in
            ["", "A", "AB", "AQ=", "AQI+", &"A".repeat(INVITATION_ENVELOPE_LIMIT / 3 * 4 + 4)]
        {
            assert!(request(envelope.into()).validate().is_err(), "{envelope}");
        }
    }
}

/// Same bounded email shape as AP enrollment; callers normalize ASCII case.
pub fn valid_account_email(email: &str) -> bool {
    !email.is_empty()
        && email.len() <= 254
        && email.contains('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod grantee_tests {
    use super::*;
    #[test]
    fn resolution_rejects_invalid_email_and_volume() {
        let mut request = ServiceRequest::ResolveVolumeGrantee {
            operation_id: crate::identifier::encode(&[0x01; 16]),
            volume_id: crate::identifier::encode(&[0x02; 16]),
            email: "Alice@example.invalid".into(),
        };
        assert!(request.validate().is_ok());
        for email in ["", "no-at", "a@b\n", "a @b", &"a".repeat(255)] {
            if let ServiceRequest::ResolveVolumeGrantee { email: target, .. } = &mut request {
                *target = email.into();
            }
            assert!(request.validate().is_err());
        }
    }
}
