//! Typed worker replies and bounded decoding of engine responses.
use super::*;

const VOLUME_NAME_BYTES_MAX: usize = 256;
const PERMISSION_KINDS_MAX: usize = 4;
const VOLUME_ACL_BYTES_MAX: usize = crate::store::catalog::record::MAX_ITEMS
    * (std::mem::size_of::<crate::store::object_contract::Grant>()
        + PERMISSION_KINDS_MAX * std::mem::size_of::<crate::store::object_contract::Permission>()
        + 128);
const _: () = assert!(VOLUME_ACL_BYTES_MAX <= crate::store::MetadataPolicy::MAX_MATERIALIZED_BYTES);

// An explicitly opened session must remain available for consent and dismissal
// even if the ordinary list is full. Keep catalog order when it fits.
fn selected_invitation_index<T>(
    rows: &[T],
    selected: Option<&InvitationStatus>,
    session: impl Fn(&T) -> &str,
) -> usize {
    if rows.len() > application_browser::MAX_SESSIONS
        && let Some(selected) = selected
        && let Some(index) = rows[application_browser::MAX_SESSIONS..]
            .iter()
            .position(|row| session(row) == selected.session)
    {
        return application_browser::MAX_SESSIONS + index;
    }
    0
}

/// A session summary used by the sidebar and recent-app view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionRow {
    pub(super) reference: String,
    pub(super) id: String,
    pub(super) resolved_build_id: String,
    pub(super) app_name: Option<String>,
    pub(super) build_state: crate::engine::builds::BuildState,
}

pub(super) enum AccountResponse {
    SavedAccount(Option<crate::engine::reply::SavedAccount>),
    Recovery(RecoveryStatus),
    Prepared(String),
    SignedIn(SignInReply),
    CodeSent(bool),
    DisplayName(String),
    AgreementAccepted,
}
/// One completed response can wait for the UI. Blocking backpressure is confined
/// to the I/O worker; the UI executor is awakened by channel readiness.
pub(super) struct WorkerResponse {
    pub(super) sign_in: Option<crate::ui::desktop::sign_in::flow::Ticket>,
    pub(super) account_identity: Option<(String, String)>,
    // Management admission stays charged through UI delivery.
    pub(super) credit: Option<worker_queue::Credit>,
    pub(super) result: std::result::Result<(), String>,
    pub(super) prepared: Option<crate::engine::reply::PreparedSession>,
    pub(super) launch_failed: bool,
    pub(super) startup: Option<Vec<crate::engine::session_instance::StartupProgress>>,
    pub(super) launched: Option<guest_app::LaunchReceipt>,
    pub(super) personal_snapshot: Option<personal::Snapshot>,
    pub(super) personal_selected: bool,
    pub(super) transfer_status: Option<TransferStatus>,
    pub(super) status_recovery: Option<RecoveryStatus>,
    pub(super) account_response: Option<AccountResponse>,
    pub(super) invitation_preview: Option<crate::store::InvitationPreview>,
    pub(super) desktop_invitation: Option<String>,
    pub(super) saved_invitations: Option<Vec<crate::store::InvitationPreview>>,
    pub(super) status_volumes: Option<Vec<crate::store::Volume>>,
    pub(super) status_scoped_volumes: Vec<crate::store::Volume>,
    pub(super) volume_invitations: Vec<crate::store::Id>,
    pub(super) volume_invitation_owners: BTreeMap<crate::store::Id, String>,
    pub(super) volume_decided: Option<(crate::store::Id, bool)>,
    pub(super) volume_created: Option<crate::store::Volume>,
    pub(super) app_removal_impact: Option<crate::store::AppRemovalImpact>,
    pub(super) erased_app_volumes: Vec<crate::store::Id>,
    pub(super) volume_label: Option<crate::store::Id>,
    pub(super) browser_reply: Option<Reply>,
    pub(super) data_reply: Option<worker_data::DataReply>,
    pub(super) browser_failure: Option<browser::Failure>,
    pub(super) status_account: Option<crate::store::Account>,
    pub(super) status_account_profile: Option<AccountProfile>,
    pub(super) account_key_backup: Option<crate::store::AccountKeyBackup>,
    pub(super) session_rows: Option<Vec<SessionRow>>,
    pub(super) session_activity: Option<BTreeMap<String, crate::store::SessionActivity>>,
    pub(super) application_catalog: Option<Vec<crate::store::Session>>,
    pub(super) imported_apps: Option<Vec<crate::store::ImportedApp>>,
    pub(super) session_created: Option<(gpui::EntityId, SessionRow)>,
    pub(super) recent_sessions: Option<Vec<String>>,
    pub(super) status_enrolled: Option<bool>,
    pub(super) status_ui_zoom: Option<u32>,
    pub(super) status_ap_connection: Option<crate::engine::reply::ApConnectionStatus>,
    pub(super) invitation_status: Option<Option<InvitationStatus>>,
    pub(super) session: Option<String>,
    pub(super) engine_lost: bool,
    // Account work retains the existing admission gate's connection lifetime.
    pub(super) recovery_epoch: Option<u64>,
    pub(super) recovery_started: Option<std::time::Instant>,
    pub(super) engine_recovered: bool,
    pub(super) critical: bool,
    pub(super) origin: Option<crate::ui::desktop::alert::Origin>,
    pub(super) recovery_action: Option<worker_recovery::Action>,
    pub(super) personal_session: Option<String>,
}

impl WorkerResponse {
    pub(super) fn from_archive(result: std::result::Result<ipc::Response, String>) -> Self {
        use crate::engine::reply::ArchivedReply as R;
        let result = result.and_then(|archive| {
            match archive.archived() {
                R::VolumeDeleted { .. } => Ok(Self::new(Ok(()))),
                R::Volume(volume) if volume.name.len() <= VOLUME_NAME_BYTES_MAX => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::Volume(_) => Err("Volume name exceeds its 256-byte limit".into()),
                R::Diagnostic(reply)
                    if matches!(
                        reply.as_ref(),
                        crate::engine::reply::ArchivedDiagnosticReply::VolumeDuplicated { volume, .. }
                            if volume.name.len() <= VOLUME_NAME_BYTES_MAX
                    ) =>
                {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::Diagnostic(reply)
                    if matches!(
                        reply.as_ref(),
                        crate::engine::reply::ArchivedDiagnosticReply::VolumeDuplicated { .. }
                    ) =>
                {
                    Err("Volume name exceeds its 256-byte limit".into())
                }
                R::Status(status) => Self::from_status_archive(status).map_err(|e| e.to_string()),
                R::SessionPrepared(_)
                | R::AppRemovalImpact { .. }
                | R::AppRemoved(_)
                | R::PersonalVolumes { .. }
                | R::PersonalVolumesSelected { .. }
                | R::Transfer { .. }
                | R::Recovery(_)
                | R::PreparedRecovery(_)
                | R::SignedIn(_)
                | R::CodeSent { .. }
                | R::SavedAccount { .. }
                | R::SignInAvailable { .. }
                | R::AccountKeyBackup { .. }
                | R::DisplayName { .. }
                | R::Permissions { .. }
                | R::AgreementAccepted
                | R::InvitationPreview(_)
                | R::VolumeInvitationDecided { .. }
                | R::InvitationStatus { .. }
                | R::InvitationPaused { .. } => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::Entries { page, .. } if page.entries.len() <= 64 => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::DirectoryEntries { listing, .. } => {
                    use crate::store::volume_index::{ArchivedDirectoryListing as L, namespace};
                    let valid = match listing.as_ref() {
                        L::Changed { .. } => true,
                        L::Page { ancestors, entries, .. } => {
                            ancestors.len() <= namespace::MAX_DEPTH
                                && entries.len() <= crate::store::volume_index::MAX_ENTRY_PAGE
                                && ancestors.iter().all(|entry| entry.name.len() <= namespace::MAX_COMPONENT_BYTES)
                                && entries.iter().all(|entry| entry.name.len() <= namespace::MAX_COMPONENT_BYTES)
                        }
                    };
                    if !valid {
                        return Err("Directory page exceeds its metadata credit".into());
                    }
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::VolumeDetails { details } => {
                    if details.volume.name.len() > VOLUME_NAME_BYTES_MAX
                        || details.permissions.grants.len() > crate::store::catalog::record::MAX_ITEMS
                        || details.permissions.grants.iter().any(|grant| grant.permissions.len() > PERMISSION_KINDS_MAX)
                    {
                        return Err("Volume details exceed their policy metadata credit".into());
                    }
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::EntryDetails { .. }
                | R::EntryChanged { .. }
                | R::EntryPermissions { .. }
                | R::VolumeEntry { .. } => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::EntryRevisions { page, .. } if page.revisions.len() <= 64 => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::EntryRevisions { .. } => {
                    Err("History page exceeds its 64-revision limit".into())
                }
                R::ObjectCreated(_)
                | R::Object(_)
                | R::Objects(_)
                | R::ObjectRemoved { .. }
                | R::Schema { .. } => {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::Diagnostic(reply)
                    if matches!(
                        reply.as_ref(),
                        crate::engine::reply::ArchivedDiagnosticReply::FileCreated { .. }
                            | crate::engine::reply::ArchivedDiagnosticReply::FileCreatedIn { .. }
                            | crate::engine::reply::ArchivedDiagnosticReply::FileWritten { .. }
                            | crate::engine::reply::ArchivedDiagnosticReply::FileRead { .. }
                            | crate::engine::reply::ArchivedDiagnosticReply::EntryDuplicated { .. }
                            | crate::engine::reply::ArchivedDiagnosticReply::PayloadSize { .. }
                    ) =>
                {
                    Ok(Self::from_reply(archive.into_owned().map_err(|e| e.to_string())))
                }
                R::Entries { .. } => Err("Entry page exceeds its 64-entry limit".into()),
                // These replies acknowledge an action. Their operation match
                // was checked by call_archive; none of their data is retained.
                _ => Ok(Self::new(Ok(()))),
            }
        });
        result.unwrap_or_else(|error| Self::new(Err(error)))
    }

    fn set_status_volumes(
        &mut self,
        mut volumes: Vec<crate::store::Volume>,
        automatic_volume_ids: &[crate::store::Id],
    ) {
        self.status_scoped_volumes = volumes
            .extract_if(.., |volume| automatic_volume_ids.binary_search(&volume.id).is_ok())
            .collect();
        // Bounded catalogs are sorted in place by the shell. Sort overflow
        // catalogs on this worker so each page uses the same display order.
        if volumes.len() > 256 {
            volumes.sort_unstable_by(super::sidebar_view::compare_volumes);
        }
        self.status_volumes = Some(volumes);
    }

    fn from_status_archive(
        status: &crate::engine::reply::ArchivedStatus,
    ) -> crate::engine::Result<Self> {
        use rkyv::{api::low::deserialize, rancor::Error};
        // Check before allocating rows or reading app headers. Pending work,
        // examples, update maps and other unused fields stay in the archive.
        if status.sessions.len() > 256
            || status.recent_sessions.len() > 256
            || status.imported_apps.len() > 256
            || status.session_references.len() > 256
        {
            return Err("Session list exceeds its 256-session display limit".into());
        }
        if status.session_activity.len() > 256
            || status.session_activity.iter().any(|(id, row)| {
                !crate::identifier::valid(id.as_str(), 32)
                    || row.participants.len() > 256
                    || row
                        .participants
                        .iter()
                        .any(|p| p.name.len() > crate::session_channel::DISPLAY_NAME_BYTES)
            })
            || status.session_activity.iter().map(|(_, r)| r.participants.len()).sum::<usize>()
                > 256
        {
            return Err("Session activity exceeds its display limits".into());
        }
        if status.session_references.iter().any(|(session, code)| {
            !crate::identifier::valid(session.as_str(), 32)
                || crate::store::parse_session_code(code.as_str()).is_err()
        }) {
            return Err("Invalid session reference in engine status".into());
        }
        if status.recent_sessions.iter().any(|id| !crate::identifier::valid(id.as_str(), 32)) {
            return Err("Invalid recent session identity".into());
        }
        if status.ap_connection.hostname.is_empty()
            || status.ap_connection.hostname.len()
                > crate::engine::reply::ApConnectionStatus::HOSTNAME_BYTES_MAX
            || status.ap_connection.hostname.as_str().contains('\0')
        {
            return Err("Invalid AP hostname in engine status".into());
        }
        if status.volume_invitations.len() > status.volumes.len()
            || status.volume_invitation_owners.len() > status.volumes.len()
            || status
                .volume_invitation_owners
                .iter()
                .any(|(_, name)| name.len() > crate::session_channel::DISPLAY_NAME_BYTES)
        {
            return Err("Invalid volume invitation metadata in engine status.".into());
        }
        let mut response = Self::new(Ok(()));
        if status.startup.len() > crate::engine::session_instance::MAX_STARTUP_PROGRESS {
            return Err("Startup status exceeds its capacity".into());
        }
        response.startup = Some(deserialize::<_, Error>(&status.startup)?);
        response.status_ap_connection = Some(deserialize::<_, Error>(&status.ap_connection)?);
        response.account_identity = Some((
            deserialize::<crate::store::Account, Error>(&status.account)?.id.to_string(),
            deserialize::<Option<String>, Error>(&status.email)?.unwrap_or_default(),
        ));
        response.recent_sessions = Some(deserialize::<_, Error>(&status.recent_sessions)?);
        response.status_enrolled = Some(status.enrolled);
        response.status_ui_zoom = Some(status.ui_zoom.to_native());
        response.status_recovery = deserialize::<_, Error>(&status.recovery)?;
        response.invitation_status = Some(deserialize::<_, Error>(&status.invitation_status)?);
        if let Some(preview) = status.desktop_invitation.as_ref() {
            if !crate::identifier::valid(preview.session.as_str(), 32) {
                return Err("Invalid desktop invitation in engine status".into());
            }
            response.desktop_invitation = Some(preview.session.as_str().to_owned());
        }
        // Read one extra displayable row so the shell can report overflow
        // while still opening the user's apps. Seed-only previews have no row.
        let rows = status.invitations.as_slice();
        let selected = selected_invitation_index(
            rows,
            response.invitation_status.as_ref().and_then(Option::as_ref),
            |row| row.session.as_str(),
        );
        let (before, rest) = rows.split_at(selected);
        response.saved_invitations = Some(
            rest.iter()
                .take(1)
                .chain(before)
                .chain(rest.iter().skip(1))
                .filter(|preview| preview.retained || preview.received.is_some())
                .take(application_browser::MAX_SESSIONS + 1)
                .map(deserialize::<_, Error>)
                .collect::<Result<Vec<_>, _>>()?,
        );
        response.set_status_volumes(
            deserialize::<_, Error>(&status.volumes)?,
            &deserialize::<Vec<crate::store::Id>, Error>(&status.automatic_volume_ids)?,
        );
        response.volume_invitations = deserialize::<_, Error>(&status.volume_invitations)?;
        response.volume_invitation_owners =
            deserialize::<_, Error>(&status.volume_invitation_owners)?;
        response.status_account = Some(deserialize::<_, Error>(&status.account)?);
        response.status_account_profile = Some(deserialize::<_, Error>(&status.account_profile)?);
        response.account_key_backup = Some(deserialize::<_, Error>(&status.account_key_backup)?);
        response.session_activity = Some(deserialize::<_, Error>(&status.session_activity)?);
        response.application_catalog = Some(deserialize::<_, Error>(&status.sessions)?);
        response.imported_apps = Some(deserialize::<_, Error>(&status.imported_apps)?);
        response.session_rows = Some(
            status
                .sessions
                .iter()
                .map(|session| {
                    Ok(SessionRow {
                        reference: status
                            .session_references
                            .get(session.id.as_str())
                            .map(|code| code.as_str().to_owned())
                            .unwrap_or_default(),
                        id: session.id.as_str().to_owned(),
                        resolved_build_id: session.resolved_build_id.as_str().to_owned(),
                        app_name: status
                            .app_names
                            .get(session.id.as_str())
                            .map(|name| name.as_str().to_owned()),
                        build_state: status
                            .builds
                            .get(session.resolved_build_id.as_str())
                            .map(deserialize::<_, Error>)
                            .transpose()?
                            .unwrap_or_default(),
                    })
                })
                .collect::<crate::engine::Result<Vec<_>>>()?,
        );
        Ok(response)
    }

    // Project typed engine replies into the fields consumed by the desktop.
    pub(super) fn from_reply(result: std::result::Result<Reply, String>) -> Self {
        let reply = match result {
            Ok(Reply::Error { error }) | Err(error) => return Self::new(Err(error)),
            Ok(reply) => reply,
        };
        let mut response = Self::new(Ok(()));
        match reply {
            Reply::Volume(volume) => response.volume_created = Some(volume),
            Reply::Diagnostic(crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                volume,
                ..
            }) => response.volume_created = Some(volume),
            reply @ (Reply::Entries { .. }
            | Reply::DirectoryEntries { .. }
            | Reply::VolumeDetails { .. }
            | Reply::Permissions { .. }
            | Reply::EntryDetails { .. }
            | Reply::EntryRevisions { .. }
            | Reply::EntryChanged { .. }
            | Reply::EntryPermissions { .. }
            | Reply::ObjectCreated(_)
            | Reply::Object(_)
            | Reply::Objects(_)
            | Reply::ObjectRemoved { .. }
            | Reply::Schema { .. }
            | Reply::Diagnostic(_)
            | Reply::VolumeEntry { .. }) => response.browser_reply = Some(reply),
            Reply::AppRemovalImpact { impact } => response.app_removal_impact = Some(impact),
            Reply::AppRemoved(crate::store::AppRemovalResult::Confirmation(impact)) => {
                response.app_removal_impact = Some(impact);
            }
            Reply::AppRemoved(crate::store::AppRemovalResult::Removed {
                erased_volumes,
                cleanup_error,
            }) => {
                response.erased_app_volumes = erased_volumes;
                if let Some(error) = cleanup_error {
                    response.result = Err(error);
                }
            }
            Reply::SessionPrepared(prepared) => response.prepared = Some(prepared),
            Reply::PersonalVolumes { session, volumes, grants } => {
                response.personal_snapshot = Some(personal::Snapshot { session, volumes, grants });
            }
            Reply::PersonalVolumesSelected { personal_volumes_selected } => {
                response.personal_selected = personal_volumes_selected;
            }
            Reply::Transfer { transfer } => response.transfer_status = Some(transfer),
            Reply::AccountKeyBackup { account_key_backup } => {
                response.account_key_backup = Some(account_key_backup)
            }
            Reply::Status(status) => response.apply_status(status),
            Reply::Recovery(state) => {
                response.account_response = Some(AccountResponse::Recovery(state));
            }
            Reply::SavedAccount { saved_account } => {
                response.account_response = Some(AccountResponse::SavedAccount(saved_account));
            }
            Reply::PreparedRecovery(state) => {
                response.account_response = Some(AccountResponse::Prepared(state.secret));
            }
            Reply::SignedIn(state) => {
                response.account_response = Some(AccountResponse::SignedIn(state));
            }
            Reply::CodeSent { code_sent } => {
                response.account_response = Some(AccountResponse::CodeSent(code_sent));
            }
            Reply::AgreementAccepted => {
                response.account_response = Some(AccountResponse::AgreementAccepted);
            }
            Reply::DisplayName { display_name } => {
                response.account_response = Some(AccountResponse::DisplayName(display_name));
            }
            Reply::InvitationPreview(preview) => response.invitation_preview = Some(preview),
            Reply::VolumeInvitationDecided { volume, accept } => {
                response.volume_decided = Some((volume, accept));
            }
            Reply::InvitationStatus { invitation_status }
            | Reply::InvitationPaused { invitation_status, .. } => {
                response.invitation_status = Some(invitation_status);
            }
            // Other successful replies acknowledge actions; the socket client
            // has already checked their variant against the submitted request.
            _ => {}
        }
        if response
            .volume_created
            .as_ref()
            .is_some_and(|volume| volume.name.len() > VOLUME_NAME_BYTES_MAX)
        {
            return Self::new(Err("Volume name exceeds its 256-byte limit".into()));
        }
        match response.browser_reply.as_ref() {
            Some(Reply::DirectoryEntries { listing, .. }) => {
                use crate::store::volume_index::{DirectoryListing as L, namespace};
                if let L::Page { ancestors, entries, .. } = listing
                    && (ancestors.len() > namespace::MAX_DEPTH
                        || entries.len() > crate::store::volume_index::MAX_ENTRY_PAGE
                        || ancestors
                            .iter()
                            .any(|entry| entry.name.len() > namespace::MAX_COMPONENT_BYTES)
                        || entries
                            .iter()
                            .any(|entry| entry.name.len() > namespace::MAX_COMPONENT_BYTES))
                {
                    return Self::new(Err("Directory page exceeds its metadata credit".into()));
                }
            }
            Some(Reply::VolumeDetails { details })
                if details.volume.name.len() > VOLUME_NAME_BYTES_MAX
                    || details.permissions.grants.len()
                        > crate::store::catalog::record::MAX_ITEMS
                    || details
                        .permissions
                        .grants
                        .iter()
                        .any(|grant| grant.permissions.len() > PERMISSION_KINDS_MAX)
                    || details.permissions.validate().is_err() =>
            {
                return Self::new(Err("Volume details exceed their policy metadata credit".into()));
            }
            _ => {}
        }
        response
    }

    fn apply_status(&mut self, mut status: Box<crate::engine::reply::Status>) {
        if status.sessions.len() > 256
            || status.imported_apps.len() > 256
            || status.session_references.len() > 256
            || status.session_references.iter().any(|(session, code)| {
                !crate::identifier::valid(session, 32)
                    || crate::store::parse_session_code(code).is_err()
            })
        {
            self.result = Err("Invalid or oversized session catalog in engine status".into());
            return;
        }
        if status.ap_connection.hostname.is_empty()
            || status.ap_connection.hostname.len()
                > crate::engine::reply::ApConnectionStatus::HOSTNAME_BYTES_MAX
            || status.ap_connection.hostname.contains('\0')
        {
            self.result = Err("Invalid AP hostname in engine status".into());
            return;
        }
        if status.volume_invitations.len() > status.volumes.len()
            || status.volume_invitation_owners.len() > status.volumes.len()
            || status
                .volume_invitation_owners
                .values()
                .any(|name| name.len() > crate::session_channel::DISPLAY_NAME_BYTES)
        {
            self.result = Err("Invalid volume invitation metadata in engine status.".into());
            return;
        }
        self.startup = Some(status.startup);
        self.status_ap_connection = Some(status.ap_connection);
        self.account_identity =
            Some((status.account.id.to_string(), status.email.clone().unwrap_or_default()));
        self.recent_sessions = Some(status.recent_sessions);
        self.status_enrolled = Some(status.enrolled);
        self.status_ui_zoom = Some(status.ui_zoom);
        self.status_recovery = status.recovery;
        self.invitation_status = Some(status.invitation_status);
        self.desktop_invitation = status.desktop_invitation.map(|preview| preview.session);
        let selected = selected_invitation_index(
            &status.invitations,
            self.invitation_status.as_ref().and_then(Option::as_ref),
            |row| row.session.as_str(),
        );
        if selected != 0 {
            status.invitations[..=selected].rotate_right(1);
        }
        status.invitations.retain(|preview| preview.retained || preview.received.is_some());
        status.invitations.truncate(application_browser::MAX_SESSIONS + 1);
        self.saved_invitations = Some(status.invitations);
        self.set_status_volumes(status.volumes, &status.automatic_volume_ids);
        self.volume_invitations = status.volume_invitations;
        self.volume_invitation_owners = status.volume_invitation_owners;
        self.session_activity = Some(status.session_activity);
        self.application_catalog = Some(status.sessions.clone());
        self.imported_apps = Some(status.imported_apps);
        self.session_rows = Some(
            status
                .sessions
                .into_iter()
                .map(|session| SessionRow {
                    reference: status
                        .session_references
                        .get(&session.id)
                        .cloned()
                        .unwrap_or_default(),
                    app_name: status.app_names.get(&session.id).cloned(),
                    build_state: status
                        .builds
                        .get(&session.resolved_build_id)
                        .cloned()
                        .unwrap_or_default(),
                    id: session.id,
                    resolved_build_id: session.resolved_build_id,
                })
                .collect(),
        );
        self.status_account = Some(status.account);
        self.status_account_profile = Some(status.account_profile);
        self.account_key_backup = Some(status.account_key_backup);
    }

    pub(super) fn critical(error: crate::ui::desktop::alert::Failure) -> Self {
        let mut response = Self::new(Err(error.details));
        response.origin = Some(error.origin);
        response.critical = true;
        response
    }

    pub(super) fn new(result: std::result::Result<(), String>) -> Self {
        Self {
            origin: None,
            result,
            sign_in: None,
            account_identity: None,
            credit: None,
            prepared: None,
            launch_failed: false,
            startup: None,
            launched: None,
            personal_snapshot: None,
            personal_selected: false,
            transfer_status: None,
            status_recovery: None,
            account_response: None,
            invitation_preview: None,
            desktop_invitation: None,
            saved_invitations: None,
            status_volumes: None,
            status_scoped_volumes: Vec::new(),
            volume_invitations: Vec::new(),
            volume_invitation_owners: BTreeMap::new(),
            volume_decided: None,
            volume_created: None,
            app_removal_impact: None,
            erased_app_volumes: Vec::new(),
            volume_label: None,
            browser_reply: None,
            data_reply: None,
            browser_failure: None,
            status_account: None,
            status_account_profile: None,
            account_key_backup: None,
            session_rows: None,
            session_activity: None,
            application_catalog: None,
            imported_apps: None,
            session_created: None,
            recent_sessions: None,
            status_enrolled: None,
            status_ui_zoom: None,
            status_ap_connection: None,
            invitation_status: None,
            session: None,
            engine_lost: false,
            recovery_epoch: None,
            recovery_started: None,
            engine_recovered: false,
            critical: false,
            recovery_action: None,
            personal_session: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn catalog_replies() -> [Reply; 3] {
        use crate::store::{
            Id,
            volume_index::{DirectoryListing, DirectoryTarget},
        };
        let volume = Id::from_bytes([1; 16]);
        [
            Reply::DirectoryEntries {
                volume,
                target: DirectoryTarget::Root,
                cut_requested: None,
                after: None,
                listing: DirectoryListing::Page {
                    cut: 5,
                    parent: None,
                    ancestors: Vec::new(),
                    entries: Vec::new(),
                    next: None,
                },
            },
            Reply::DirectoryEntries {
                volume,
                target: DirectoryTarget::Root,
                cut_requested: Some(4),
                after: None,
                listing: DirectoryListing::Changed { cut: 5 },
            },
            Reply::VolumeDetails {
                details: crate::store::VolumeDetails {
                    volume: crate::store::Volume {
                        id: volume,
                        name: "Metadata".into(),
                        owner: Id::from_bytes([2; 16]),
                        personal: true,
                        share_key_with_service: true,
                    },
                    permissions: Default::default(),
                    created_clock: 5,
                    name_clock: 6,
                },
            },
        ]
    }

    #[test]
    fn catalog_queries_reach_the_browser_after_archive_ownership_ends() {
        let size = Reply::Diagnostic(crate::engine::reply::DiagnosticReply::PayloadSize {
            volume: crate::store::Id::from_bytes([1; 16]),
            entry: None,
            bytes: 42,
        });
        for reply in catalog_replies().into_iter().chain([size]) {
            let expected = serde_json::to_value(&reply).unwrap();
            let archive =
                ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                    .unwrap();
            let archived = WorkerResponse::from_archive(Ok(archive));
            let owned = WorkerResponse::from_reply(Ok(reply));
            for response in [archived, owned] {
                assert!(response.result.is_ok());
                assert_eq!(
                    serde_json::to_value(response.browser_reply.unwrap()).unwrap(),
                    expected
                );
                assert!(response.volume_created.is_none());
            }
        }
    }

    #[test]
    fn volume_permission_acknowledgment_preserves_its_existing_scalar_shape() {
        for permissions_updated in [false, true] {
            let reply = Reply::Permissions { permissions_updated };
            let archive =
                ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                    .unwrap();
            for response in
                [WorkerResponse::from_archive(Ok(archive)), WorkerResponse::from_reply(Ok(reply))]
            {
                assert!(response.result.is_ok());
                assert!(matches!(response.browser_reply,
                    Some(Reply::Permissions { permissions_updated: actual }) if actual == permissions_updated));
            }
        }
    }

    #[test]
    fn catalog_archive_bounds_are_checked_before_owned_projection() {
        use crate::store::{
            EntryId,
            volume_index::{DirectoryListing, EntrySummary},
        };
        let [mut directory, _, mut volume] = catalog_replies();
        let Reply::DirectoryEntries { listing: DirectoryListing::Page { entries, .. }, .. } =
            &mut directory
        else {
            panic!("page")
        };
        entries.resize(
            65,
            EntrySummary {
                id: EntryId::from_bytes([1; 15]).unwrap(),
                parent: None,
                name: "Entry".into(),
                kind: crate::store::EntryType::KvDb,
                protocol: None,
                deleted: false,
                name_conflict: false,
            },
        );
        let Reply::VolumeDetails { details } = &mut volume else { panic!("volume") };
        details.volume.name = "x".repeat(VOLUME_NAME_BYTES_MAX + 1);
        for reply in [directory, volume] {
            let archive =
                ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                    .unwrap();
            for response in
                [WorkerResponse::from_archive(Ok(archive)), WorkerResponse::from_reply(Ok(reply))]
            {
                assert!(response.result.is_err());
                assert!(response.browser_reply.is_none());
            }
        }
    }

    fn volume_replies(name: String) -> [Reply; 2] {
        let volume = crate::store::Volume {
            id: crate::store::Id::from_bytes([1; 16]),
            name,
            owner: crate::store::Id::from_bytes([2; 16]),
            personal: true,
            share_key_with_service: true,
        };
        [
            Reply::Volume(volume.clone()),
            Reply::Diagnostic(crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                source: crate::store::Id::from_bytes([3; 16]),
                volume,
            }),
        ]
    }

    #[test]
    fn created_volume_projection_preserves_identity_from_archived_and_owned_replies() {
        // Creation and duplication retain the same bounded payload after archive ownership ends.
        for reply in volume_replies("é".repeat(VOLUME_NAME_BYTES_MAX / 2)) {
            let archive =
                ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                    .unwrap();
            let archived = WorkerResponse::from_archive(Ok(archive));
            let owned = WorkerResponse::from_reply(Ok(reply));
            assert!(archived.result.is_ok());
            assert!(owned.result.is_ok());
            assert_eq!(archived.volume_created, owned.volume_created);
            let volume = archived.volume_created.unwrap();
            assert_eq!(volume.id, crate::store::Id::from_bytes([1; 16]));
            assert_eq!(volume.owner, crate::store::Id::from_bytes([2; 16]));
            assert_eq!(volume.name, "é".repeat(VOLUME_NAME_BYTES_MAX / 2));
            assert!(archived.browser_reply.is_none());
            assert!(owned.browser_reply.is_none());
        }
    }

    #[test]
    fn created_volume_projection_rejects_oversized_names() {
        // Invalid created-volume payloads cannot reach the UI through either delivery path.
        for reply in volume_replies("a".repeat(VOLUME_NAME_BYTES_MAX + 1)) {
            let archive =
                ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                    .unwrap();
            let archived = WorkerResponse::from_archive(Ok(archive));
            let owned = WorkerResponse::from_reply(Ok(reply));
            for response in [archived, owned] {
                assert!(response.result.unwrap_err().contains("256-byte"));
                assert!(response.volume_created.is_none());
                assert!(response.browser_reply.is_none());
            }
        }
    }

    fn status_archive(status: Box<crate::engine::reply::Status>) -> ipc::Response {
        ipc::Response::new(
            ipc::codec::encode(&Reply::Status(status), ipc::codec::MAX_ARCHIVE).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn archived_status_retains_consumed_fields_after_the_buffer_is_dropped() {
        use crate::store::{
            BuildSelector, Id, PendingKind, PendingOperation, Session, UpdateAuthority,
        };
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
            panic!("status reply");
        };
        // Fixed-width session codes can contain padding that ordinary ID text
        // does not. They must survive the real engine-status projection.
        let reference = crate::store::SessionReference::new([0; 16]).unwrap();
        let session = Session {
            id: reference.internal_id().into(),
            selector: BuildSelector::Exact { build_id: "unused selector".repeat(1000) },
            resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
            root_volume: Id::from_bytes([1; 16]),
            shared_volume: None,
            app: crate::package::AppIdentity {
                publisher: Id::from_bytes([3; 16]),
                app_id: "test".into(),
            },

            creator: Id::from_bytes([3; 16]),
            update_authority: UpdateAuthority::Creator,
        };
        status.sessions = vec![session.clone()];
        status.session_references.insert(session.id.clone(), reference.code());
        status.app_names.insert(session.id.clone(), "Notes λ".into());
        status.builds.insert(
            session.resolved_build_id.clone(),
            crate::engine::builds::BuildState::DownloadFailed { error: "blob not found λ".into() },
        );
        status.pending = vec![PendingOperation {
            id: Id::from_bytes([4; 16]),
            operation: PendingKind::RegisterSession { session, envelope: vec![7; 256 * 1024] },
        }];
        status.enrolled = true;
        status.ap_connection = crate::engine::reply::ApConnectionStatus {
            state: crate::engine::reply::ApConnectionState::Live,
            hostname: "configured.ap.test".into(),
        };
        status.account_profile.display_name = Some("Name λ\0tail".into());
        status.invitation_status = Some(InvitationStatus {
            session: reference.internal_id().into(),
            state: InvitationState::Denied { message: "Retained λ\0tail".into() },
        });
        let ordinary = engine.services.store.create_volume("Ordinary", false).unwrap();
        let scoped = crate::store::Volume {
            id: Id::from_bytes([5; 16]),
            name: "Shared".into(),
            personal: false,
            ..ordinary.clone()
        };
        status.volumes = vec![ordinary.clone(), scoped.clone()];
        status.automatic_volume_ids = vec![scoped.id];
        status.volume_invitations = vec![ordinary.id];
        status.volume_invitation_owners.insert(ordinary.owner, "Frank".into());
        let response = WorkerResponse::from_archive(Ok(status_archive(status)));
        assert!(response.result.is_ok());
        assert_eq!(response.volume_invitations, [ordinary.id]);
        assert_eq!(response.volume_invitation_owners[&ordinary.owner], "Frank");
        assert_eq!(response.status_volumes.unwrap(), [ordinary]);
        assert_eq!(response.status_scoped_volumes, [scoped]);
        assert_eq!(response.status_enrolled, Some(true));
        assert_eq!(
            response.status_ap_connection.unwrap(),
            crate::engine::reply::ApConnectionStatus {
                state: crate::engine::reply::ApConnectionState::Live,
                hostname: "configured.ap.test".into(),
            }
        );
        assert_eq!(
            response.status_account_profile.unwrap().display_name.as_deref(),
            Some("Name λ\0tail")
        );
        let rows = response.session_rows.unwrap();
        assert_eq!(
            rows,
            vec![SessionRow {
                reference: reference.code(),
                id: reference.internal_id().into(),
                resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
                build_state: crate::engine::builds::BuildState::DownloadFailed {
                    error: "blob not found λ".into()
                },
                app_name: Some("Notes λ".into())
            }]
        );
        assert!(matches!(response.invitation_status, Some(Some(InvitationStatus {
            state: InvitationState::Denied { message }, ..
        })) if message == "Retained λ\0tail"));
    }

    #[test]
    fn a_full_app_list_keeps_the_explicitly_opened_invitation_visible() {
        use crate::store::{BuildSelector, InvitationPreview, ReceivedInvitation};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
            panic!("expected status");
        };
        let limit = application_browser::MAX_SESSIONS;
        let store = &engine.services.store;
        let root = store.create_volume("Shared list", true).unwrap();
        let build = crate::identifier::encode(&[9; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "list".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        status.invitations = (0..limit + 2)
            .map(|index| {
                let mut session = session.clone();
                let mut identity = [0; 32];
                identity[..8].copy_from_slice(&(index as u64).to_be_bytes());
                session.id = crate::identifier::encode(&identity);
                InvitationPreview {
                    session: session.id.clone(),
                    retained: false,
                    paused: false,
                    received: Some(ReceivedInvitation { session, root_name: "Shared list".into() }),
                }
            })
            .collect();
        let selected = status.invitations.last().unwrap().session.clone();
        status.invitation_status = Some(InvitationStatus {
            session: selected.clone(),
            state: InvitationState::Ready { build_id: build },
        });
        let reply = Reply::Status(status);
        let archive =
            ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                .unwrap();
        for response in
            [WorkerResponse::from_archive(Ok(archive)), WorkerResponse::from_reply(Ok(reply))]
        {
            let rows = response.saved_invitations.unwrap();
            assert_eq!(rows.len(), limit + 1);
            assert_eq!(rows[0].session, selected);
        }
    }

    #[test]
    fn archived_status_rejects_excess_sessions_before_projection() {
        use crate::store::{BuildSelector, Id, Session, UpdateAuthority};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
            panic!("status reply");
        };
        status.sessions = vec![
            Session {
                id: crate::identifier::encode(&[0xab; 32]),
                selector: BuildSelector::Exact { build_id: crate::identifier::encode(&[0xcd; 32]) },
                resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
                root_volume: Id::from_bytes([1; 16]),
                shared_volume: None,
                app: crate::package::AppIdentity {
                    publisher: Id::from_bytes([3; 16]),
                    app_id: "test".into()
                },

                creator: Id::from_bytes([3; 16]),
                update_authority: UpdateAuthority::Creator,
            };
            257
        ];
        let response = WorkerResponse::from_archive(Ok(status_archive(status)));
        assert!(response.result.unwrap_err().contains("256-session"));
        assert!(response.session_rows.is_none());
        assert!(response.status_account_profile.is_none());
    }

    #[test]
    fn archived_ap_status_rejects_invalid_tooltip_hostnames() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        for hostname in [String::new(), "a".repeat(255), "ap.test\0extra".into()] {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status reply");
            };
            status.ap_connection.hostname = hostname;
            let response = WorkerResponse::from_archive(Ok(status_archive(status)));
            assert!(response.result.unwrap_err().contains("Invalid AP hostname"));
            assert!(response.status_ap_connection.is_none());
        }
    }

    #[test]
    fn archived_acknowledgments_keep_only_consumed_payloads() {
        // Agreement acceptance must reach the sign-in UI; unrelated data stays in the archive.
        let archive = ipc::Response::new(
            ipc::codec::encode(&Reply::AgreementAccepted, ipc::codec::MAX_ARCHIVE).unwrap(),
        )
        .unwrap();
        let response = WorkerResponse::from_archive(Ok(archive));
        assert!(response.result.is_ok());
        assert!(matches!(response.account_response, Some(AccountResponse::AgreementAccepted)));

        let reply = Reply::Developer(crate::engine::reply::DeveloperReply::Display {
            build_id: crate::identifier::encode(&[0xab; 32]),
            name: "unused".repeat(64 * 1024),
            icon_error: None,
        });
        let archive =
            ipc::Response::new(ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap())
                .unwrap();
        let response = WorkerResponse::from_archive(Ok(archive));
        assert!(response.result.is_ok());
        assert!(response.prepared.is_none());
        assert!(response.account_response.is_none());
    }
}
