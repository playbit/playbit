//! UIKit status projection: owned display data, with bounded borrowed field access.
use super::{IosReply, invitation, personal_volumes::PersonalVolumeRow};
use crate::{
    engine::reply::{AccountProfile, ExampleInfo, Status},
    store::{Id, Volume},
};
pub(super) struct SessionRow {
    pub id: String,
    pub reference: Option<String>,
    pub build: String,
    pub root: Id,
    pub name: String,
    pub build_kind: &'static str,
    pub build_message: Option<String>,
}
pub(super) struct Payload {
    pub account: String,
    pub enrolled: bool,
    pub email: Option<String>,
    pub account_key_backup: crate::store::AccountKeyBackup,
    pub profile: AccountProfile,
    pub sessions: Vec<SessionRow>,
    pub volumes: Vec<Volume>,
    pub examples: Vec<ExampleInfo>,
    pub invitations: Vec<invitation::Payload>,
    pub watch: invitation::Payload,
}
impl Payload {
    pub(super) fn new(value: Status) -> Self {
        Self {
            account: value.account.id.to_string(),
            enrolled: value.enrolled,
            email: value.email,
            account_key_backup: value.account_key_backup,
            profile: value.account_profile,
            sessions: value
                .sessions
                .into_iter()
                .map(|session| SessionRow {
                    name: value
                        .app_names
                        .get(&session.id)
                        .cloned()
                        .unwrap_or_else(|| "App name unknown".into()),
                    build_message: value
                        .builds
                        .get(&session.resolved_build_id)
                        .unwrap_or(&crate::engine::builds::BuildState::Checking)
                        .message(),
                    build_kind: value
                        .builds
                        .get(&session.resolved_build_id)
                        .unwrap_or(&crate::engine::builds::BuildState::Checking)
                        .kind(),
                    reference: value.session_references.get(&session.id).cloned(),
                    id: session.id,
                    build: session.resolved_build_id,
                    root: session.root_volume,
                })
                .collect(),
            volumes: value.volumes,
            examples: value.examples,
            invitations: value
                .invitations
                .into_iter()
                .map(|preview| invitation::Payload::Preview {
                    session: preview.session,
                    paused: preview.paused,
                    retained: preview.retained,
                    root_name: preview.received.map(|received| received.root_name),
                })
                .collect(),
            watch: invitation::Payload::Status { paused: None, status: value.invitation_status },
        }
    }
    pub(super) fn storage(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>()
            .saturating_add(self.account.capacity())
            .saturating_add(self.email.as_ref().map_or(0, String::capacity))
            .saturating_add(self.profile.display_name.as_ref().map_or(0, String::capacity))
            .saturating_add(
                self.sessions.capacity().saturating_mul(std::mem::size_of::<SessionRow>()),
            )
            .saturating_add(self.volumes.capacity().saturating_mul(std::mem::size_of::<Volume>()))
            .saturating_add(
                self.examples.capacity().saturating_mul(std::mem::size_of::<ExampleInfo>()),
            )
            .saturating_add(
                self.invitations
                    .capacity()
                    .saturating_mul(std::mem::size_of::<invitation::Payload>()),
            )
            .saturating_add(self.watch.storage());
        for row in &self.sessions {
            bytes = bytes
                .saturating_add(row.build_message.as_ref().map_or(0, String::capacity))
                .saturating_add(row.reference.as_ref().map_or(0, String::capacity));
            for text in [&row.id, &row.build, &row.name] {
                bytes = bytes.saturating_add(text.capacity());
            }
        }
        for volume in &self.volumes {
            bytes = bytes.saturating_add(volume.name.capacity());
        }
        for example in &self.examples {
            for text in [
                &example.id,
                &example.name,
                &example.description,
                &example.invitation,
                &example.source,
            ] {
                bytes = bytes.saturating_add(text.capacity());
            }
        }
        // Count the payload's fixed size conservatively as well as its strings.
        for preview in &self.invitations {
            bytes = bytes.saturating_add(preview.storage());
        }
        bytes
    }
}
#[repr(C)]
#[derive(Default)]
pub struct StatusText {
    pub(super) bytes: *const u8,
    pub(super) len: usize,
}
impl StatusText {
    pub(super) fn new(text: &str) -> Self {
        Self { bytes: text.as_ptr(), len: text.len() }
    }
}
#[repr(C)]
#[derive(Default)]
pub struct StatusFields {
    account: StatusText,
    enrolled: u8,
    unlocked: u8,
    initialized: u8,
    replacement_required: u8,
    agreement_required: u8,
    display_name: StatusText,
    email: StatusText,
    account_key_backup: StatusText,
    sessions: usize,
    volumes: usize,
    examples: usize,
    invitations: usize,
}
#[repr(C)]
#[derive(Default)]
pub struct SessionFields {
    id: StatusText,
    build: StatusText,
    root: *const u8,
    name: StatusText,
    build_kind: StatusText,
    build_message: StatusText,
    reference: StatusText,
}
#[repr(C)]
#[derive(Default)]
pub struct ExampleFields {
    id: StatusText,
    name: StatusText,
    description: StatusText,
    invitation: StatusText,
    source: StatusText,
}
/// The canonical Markdown remains valid for the lifetime of this image.
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_user_agreement() -> StatusText {
    StatusText::new(crate::network::ap::registration::TERMS_DOCUMENT)
}
/// # Safety
/// All pointers borrow the reply and must be copied before it is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_status(reply: *const IosReply) -> StatusFields {
    let Some(value) = (unsafe { reply.as_ref() }).and_then(|reply| reply.status.as_ref()) else {
        return StatusFields::default();
    };
    StatusFields {
        account: StatusText::new(&value.account),
        enrolled: u8::from(value.enrolled),
        account_key_backup: StatusText::new(value.account_key_backup.as_str()),
        unlocked: u8::from(value.profile.unlocked),
        initialized: u8::from(value.profile.initialized),
        replacement_required: u8::from(value.profile.replacement_required),
        agreement_required: u8::from(value.profile.agreement_required),
        display_name: value
            .profile
            .display_name
            .as_deref()
            .map_or_else(StatusText::default, StatusText::new),
        email: value.email.as_deref().map_or_else(StatusText::default, StatusText::new),
        sessions: value.sessions.len(),
        volumes: value.volumes.len(),
        examples: value.examples.len(),
        invitations: value.invitations.len(),
    }
}
/// # Safety
/// Returned fields borrow the reply. Out-of-range indices return empty fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status_session(
    reply: *const IosReply,
    index: usize,
) -> SessionFields {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.status.as_ref())
        .and_then(|value| value.sessions.get(index))
        .map_or_else(SessionFields::default, |row| SessionFields {
            id: StatusText::new(&row.id),
            reference: row.reference.as_deref().map_or_else(StatusText::default, StatusText::new),
            build: StatusText::new(&row.build),
            root: row.root.as_bytes().as_ptr(),
            name: StatusText::new(&row.name),
            build_kind: StatusText::new(row.build_kind),
            build_message: row
                .build_message
                .as_deref()
                .map_or_else(StatusText::default, StatusText::new),
        })
}
/// # Safety
/// Returned fields borrow the reply. Out-of-range indices return empty fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status_volume(
    reply: *const IosReply,
    index: usize,
) -> PersonalVolumeRow {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.status.as_ref())
        .and_then(|value| value.volumes.get(index))
        .map_or_else(PersonalVolumeRow::default, |row| PersonalVolumeRow {
            id: row.id.as_bytes().as_ptr(),
            name: row.name.as_ptr(),
            name_len: row.name.len(),
        })
}
/// # Safety
/// Returned fields borrow the reply. Out-of-range indices return empty fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status_example(
    reply: *const IosReply,
    index: usize,
) -> ExampleFields {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.status.as_ref())
        .and_then(|value| value.examples.get(index))
        .map_or_else(ExampleFields::default, |row| ExampleFields {
            id: StatusText::new(&row.id),
            name: StatusText::new(&row.name),
            description: StatusText::new(&row.description),
            invitation: StatusText::new(&row.invitation),
            source: StatusText::new(&row.source),
        })
}
/// # Safety
/// Returned fields borrow the reply. Out-of-range indices return empty fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status_invitation(
    reply: *const IosReply,
    index: usize,
) -> invitation::InvitationFields {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.status.as_ref())
        .and_then(|value| value.invitations.get(index))
        .map_or_else(invitation::InvitationFields::default, invitation::fields)
}
/// # Safety
/// Returned fields borrow the reply; copy before freeing it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status_watch(
    reply: *const IosReply,
) -> invitation::InvitationFields {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.status.as_ref())
        .map_or_else(invitation::InvitationFields::default, |value| {
            invitation::fields(&value.watch)
        })
}

#[cfg(test)]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    fn fixture() -> Payload {
        Payload {
            account: Id::from_bytes([255; 16]).to_string(),
            account_key_backup: crate::store::AccountKeyBackup::Saved,
            enrolled: true,
            email: Some("robin@example.test".into()),
            profile: AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("é\0🔑".into()),
                unlocked: true,
                initialized: true,
            },
            sessions: vec![SessionRow {
                id: "session".into(),
                reference: Some(crate::identifier::encode(&[5; 16])),
                build: "build".into(),
                root: Id::from_bytes([7; 16]),
                name: "Notes".into(),
                build_kind: "ready",
                build_message: None,
            }],
            volumes: vec![Volume {
                id: Id::from_bytes([7; 16]),
                owner: Id::from_bytes([255; 16]),
                name: "Root".into(),
                personal: true,
                share_key_with_service: false,
            }],
            examples: vec![ExampleInfo {
                id: "example".into(),
                name: "Example".into(),
                description: "Description".into(),
                invitation: "link".into(),
                source: "source".into(),
            }],
            invitations: vec![invitation::Payload::Preview {
                session: "pending".into(),
                paused: true,
                retained: false,
                root_name: None,
            }],
            watch: invitation::Payload::Status { paused: None, status: None },
        }
    }
    #[test]
    fn status_fields_borrow_all_catalogs_and_reject_missing_indices() {
        // Retained fields preserve identity, order, UTF-8 and embedded NUL without JSON.
        let reply = owner_response(|| {
            Ok(ReplyData { request_id: Some(8), status: Some(fixture()), ..ReplyData::default() })
        });
        unsafe {
            let fields = pb_ios_reply_status(reply);
            assert_eq!(
                std::slice::from_raw_parts(fields.account.bytes, fields.account.len),
                b"YcVfxkQb6JRzqk5kF2tNLv"
            );
            assert_eq!((fields.enrolled, fields.unlocked, fields.initialized), (1, 1, 1));
            assert_eq!(
                std::slice::from_raw_parts(fields.email.bytes, fields.email.len),
                b"robin@example.test"
            );
            assert_eq!(
                std::slice::from_raw_parts(fields.display_name.bytes, fields.display_name.len),
                "é\0🔑".as_bytes()
            );
            assert_eq!(
                (fields.sessions, fields.volumes, fields.examples, fields.invitations),
                (1, 1, 1, 1)
            );
            let session = pb_ios_status_session(reply, 0);
            for (text, expected) in
                [(session.id, "session"), (session.build, "build"), (session.name, "Notes")]
            {
                assert_eq!(std::slice::from_raw_parts(text.bytes, text.len), expected.as_bytes());
            }
            assert_eq!(
                std::slice::from_raw_parts(session.reference.bytes, session.reference.len),
                crate::identifier::encode(&[5; 16]).as_bytes()
            );
            assert_eq!(std::slice::from_raw_parts(session.root, 16), &[7; 16]);
            let volume = pb_ios_status_volume(reply, 0);
            assert_eq!(std::slice::from_raw_parts(volume.name, volume.name_len), b"Root");
            let example = pb_ios_status_example(reply, 0);
            for (text, expected) in [
                (example.id, "example"),
                (example.name, "Example"),
                (example.description, "Description"),
                (example.invitation, "link"),
                (example.source, "source"),
            ] {
                assert_eq!(std::slice::from_raw_parts(text.bytes, text.len), expected.as_bytes());
            }
            assert!(pb_ios_status_session(reply, usize::MAX).id.bytes.is_null());
            assert!(pb_ios_status_volume(reply, usize::MAX).id.is_null());
            assert!(pb_ios_status_example(reply, usize::MAX).id.bytes.is_null());
            pb_ios_reply_free(reply);
            assert!(pb_ios_reply_status(std::ptr::null()).account.bytes.is_null());
        }
    }
    #[test]
    fn status_capacity_obeys_shared_response_limit() {
        // Spare capacity in any catalog must be counted before crossing the FFI boundary.
        let mut value = fixture();
        value.sessions[0].name = String::with_capacity(MAX_RESPONSE_BYTES);
        let reply = owner_response(|| {
            Ok(ReplyData { request_id: Some(9), status: Some(value), ..ReplyData::default() })
        });
        unsafe {
            assert!(pb_ios_reply_status(reply).account.bytes.is_null());
            let completion = pb_ios_reply_completion(reply);
            assert_eq!(completion.request_id, 9);
            assert!(!completion.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
