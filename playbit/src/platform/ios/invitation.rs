//! Invitation UI results, projected to the fields UIKit uses and borrowed until free.
use super::IosReply;
use crate::engine::reply::{InvitationState, InvitationStatus};
pub(super) enum Payload {
    Preview { session: String, paused: bool, retained: bool, root_name: Option<String> },
    Status { paused: Option<bool>, status: Option<InvitationStatus> },
    Link { invitation: String, availability: String },
}
impl Payload {
    pub(super) fn storage(&self) -> usize {
        let strings = match self {
            Self::Preview { session, root_name, .. } => {
                session.capacity().saturating_add(root_name.as_ref().map_or(0, String::capacity))
            }
            Self::Status { status, .. } => status.as_ref().map_or(0, |status| {
                status.session.capacity().saturating_add(match &status.state {
                    InvitationState::Ready { build_id } => build_id.capacity(),
                    InvitationState::Denied { message } | InvitationState::Failed { message } => {
                        message.capacity()
                    }
                    _ => 0,
                })
            }),
            Self::Link { invitation, availability } => {
                invitation.capacity().saturating_add(availability.capacity())
            }
        };
        std::mem::size_of::<Self>().saturating_add(strings)
    }
}
#[repr(C)]
#[derive(Default)]
pub struct InvitationFields {
    category: u32,
    paused: u8,
    retained: u8,
    state: u32,
    session: *const u8,
    session_len: usize,
    root_name: *const u8,
    root_name_len: usize,
    build: *const u8,
    build_len: usize,
    message: *const u8,
    message_len: usize,
    invitation: *const u8,
    invitation_len: usize,
    availability: *const u8,
    availability_len: usize,
}
/// # Safety
/// All strings borrow the reply and must be copied before it is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_invitation(reply: *const IosReply) -> InvitationFields {
    let Some(value) = (unsafe { reply.as_ref() }).and_then(|reply| reply.invitation.as_ref())
    else {
        return InvitationFields::default();
    };
    fields(value)
}
pub(super) fn fields(value: &Payload) -> InvitationFields {
    let mut fields = InvitationFields::default();
    match value {
        Payload::Preview { session, paused, retained, root_name } => {
            fields.category = 1;
            fields.paused = u8::from(*paused);
            fields.retained = u8::from(*retained);
            fields.session = session.as_ptr();
            fields.session_len = session.len();
            if let Some(text) = root_name {
                fields.root_name = text.as_ptr();
                fields.root_name_len = text.len();
            }
        }
        Payload::Status { paused, status } => {
            fields.category = if paused.is_some() { 3 } else { 2 };
            fields.paused = u8::from(paused.unwrap_or(false));
            if let Some(status) = status {
                fields.session = status.session.as_ptr();
                fields.session_len = status.session.len();
                fields.state = match &status.state {
                    InvitationState::Paused => 1,
                    InvitationState::Checking => 2,
                    InvitationState::SignIn => 3,
                    InvitationState::WaitingRegistration => 4,
                    InvitationState::WaitingApp => 5,
                    InvitationState::WaitingDatabase => 6,
                    InvitationState::WaitingPeer => 7,
                    InvitationState::Ready { build_id } => {
                        fields.build = build_id.as_ptr();
                        fields.build_len = build_id.len();
                        8
                    }
                    InvitationState::Denied { message } => {
                        fields.message = message.as_ptr();
                        fields.message_len = message.len();
                        9
                    }
                    InvitationState::Failed { message } => {
                        fields.message = message.as_ptr();
                        fields.message_len = message.len();
                        10
                    }
                };
            }
        }
        Payload::Link { invitation, availability } => {
            fields.category = 4;
            fields.invitation = invitation.as_ptr();
            fields.invitation_len = invitation.len();
            fields.availability = availability.as_ptr();
            fields.availability_len = availability.len();
        }
    }
    fields
}
#[cfg(test)]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    #[test]
    fn invitation_states_and_optional_pause_are_preserved() {
        // All state discriminators and optional result shapes survive the native projection.
        for paused in [None, Some(false), Some(true)] {
            for (index, state) in [
                InvitationState::Paused,
                InvitationState::Checking,
                InvitationState::SignIn,
                InvitationState::WaitingRegistration,
                InvitationState::WaitingApp,
                InvitationState::WaitingDatabase,
                InvitationState::WaitingPeer,
                InvitationState::Ready { build_id: "build".into() },
                InvitationState::Denied { message: "é\0🔑".into() },
                InvitationState::Failed { message: "é\0🔑".into() },
            ]
            .into_iter()
            .enumerate()
            {
                let reply = owner_response(|| {
                    Ok(ReplyData {
                        request_id: Some(4),
                        invitation: Some(Payload::Status {
                            paused,
                            status: Some(InvitationStatus { session: "session".into(), state }),
                        }),
                        ..ReplyData::default()
                    })
                });
                unsafe {
                    let fields = pb_ios_reply_invitation(reply);
                    assert_eq!(fields.category, if paused.is_some() { 3 } else { 2 });
                    assert_eq!(fields.paused, u8::from(paused.unwrap_or(false)));
                    assert_eq!(fields.state, index as u32 + 1);
                    assert_eq!(
                        std::slice::from_raw_parts(fields.session, fields.session_len),
                        b"session"
                    );
                    if fields.state == 8 {
                        assert_eq!(
                            std::slice::from_raw_parts(fields.build, fields.build_len),
                            b"build"
                        );
                    }
                    if fields.state >= 9 {
                        assert_eq!(
                            std::slice::from_raw_parts(fields.message, fields.message_len),
                            "é\0🔑".as_bytes()
                        );
                    }
                    pb_ios_reply_free(reply);
                }
            }
        }
    }
    #[test]
    fn preview_link_absence_and_oversize_are_bounded() {
        // No received session is retained merely to display its root name.
        for root_name in [None, Some(String::new()), Some("é\0🔑".into())] {
            let expected = root_name.clone();
            let reply = owner_response(|| {
                Ok(ReplyData {
                    invitation: Some(Payload::Preview {
                        session: "session".into(),
                        paused: true,
                        retained: false,
                        root_name,
                    }),
                    ..ReplyData::default()
                })
            });
            unsafe {
                let fields = pb_ios_reply_invitation(reply);
                assert_eq!(fields.category, 1);
                assert_eq!(fields.paused, 1);
                assert_eq!(fields.retained, 0);
                assert_eq!(fields.root_name.is_null(), expected.is_none());
                if let Some(text) = expected {
                    assert_eq!(
                        std::slice::from_raw_parts(fields.root_name, fields.root_name_len),
                        text.as_bytes()
                    );
                }
                pb_ios_reply_free(reply);
            }
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                invitation: Some(Payload::Link {
                    invitation: "link".into(),
                    availability: "offline".into(),
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            let fields = pb_ios_reply_invitation(reply);
            assert_eq!(fields.category, 4);
            assert_eq!(
                std::slice::from_raw_parts(fields.invitation, fields.invitation_len),
                b"link"
            );
            assert_eq!(
                std::slice::from_raw_parts(fields.availability, fields.availability_len),
                b"offline"
            );
            pb_ios_reply_free(reply);
            assert_eq!(pb_ios_reply_invitation(std::ptr::null()).category, 0);
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                invitation: Some(Payload::Status { paused: None, status: None }),
                ..ReplyData::default()
            })
        });
        unsafe {
            let fields = pb_ios_reply_invitation(reply);
            assert_eq!(fields.category, 2);
            assert_eq!(fields.state, 0);
            pb_ios_reply_free(reply);
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(5),
                invitation: Some(Payload::Link {
                    invitation: "x".repeat(MAX_RESPONSE_BYTES),
                    availability: String::new(),
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_invitation(reply).category, 0);
            let completed = pb_ios_reply_completion(reply);
            assert_eq!(completed.request_id, 5);
            assert!(!completed.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
