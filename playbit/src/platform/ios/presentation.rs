//! UIKit presentation metadata, retained without unused session/log payloads.
use super::IosReply;
use crate::engine::{UpdateKind, UpdateStatus};

pub(super) struct Metadata {
    pub instance: crate::store::Id,
    pub generation: u64,
    pub build: String,
    pub invitation: Option<String>,
    pub availability: String,
    pub update: Option<UpdateStatus>,
}
impl Metadata {
    pub(super) fn storage(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.build.capacity())
            .saturating_add(self.invitation.as_ref().map_or(0, String::capacity))
            .saturating_add(self.availability.capacity())
            .saturating_add(self.update.as_ref().map_or(0, |update| {
                update
                    .message
                    .capacity()
                    .saturating_add(update.build_id.as_ref().map_or(0, String::capacity))
            }))
    }
}
#[repr(C)]
#[derive(Default)]
pub struct PresentationFields {
    present: u8,
    instance: *const u8,
    generation: u64,
    build: *const u8,
    build_len: usize,
    invitation: *const u8,
    invitation_len: usize,
    availability: *const u8,
    availability_len: usize,
    update_kind: u32,
    update_build: *const u8,
    update_build_len: usize,
    update_message: *const u8,
    update_message_len: usize,
}
/// # Safety
/// All text borrows the reply. Copy it and the scene before freeing the reply.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_presentation(reply: *const IosReply) -> PresentationFields {
    let Some(value) = (unsafe { reply.as_ref() }).and_then(|reply| reply.presentation.as_ref())
    else {
        return PresentationFields::default();
    };
    let mut fields = PresentationFields {
        present: 1,
        instance: value.instance.as_bytes().as_ptr(),
        generation: value.generation,
        build: value.build.as_ptr(),
        build_len: value.build.len(),
        invitation: value.invitation.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
        invitation_len: value.invitation.as_ref().map_or(0, String::len),
        availability: value.availability.as_ptr(),
        availability_len: value.availability.len(),
        ..PresentationFields::default()
    };
    if let Some(update) = &value.update {
        fields.update_kind = match update.kind {
            UpdateKind::Failed => 1,
            UpdateKind::Paused => 2,
        };
        fields.update_message = update.message.as_ptr();
        fields.update_message_len = update.message.len();
        if let Some(build) = &update.build_id {
            fields.update_build = build.as_ptr();
            fields.update_build_len = build.len();
        }
    }
    fields
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert native borrowed fields and reply ownership.")]
mod tests {
    use super::super::{ReplyData, owner_response, pb_ios_reply_free};
    use super::*;
    #[test]
    fn oversized_metadata_drops_native_payload_and_preserves_correlation() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(43),
                presentation: Some(Metadata {
                    instance: crate::store::Id::from_bytes([7; 16]),
                    generation: 3,
                    build: "build".into(),
                    invitation: None,
                    availability: "x".repeat(super::super::MAX_RESPONSE_BYTES),
                    update: None,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_presentation(reply).present, 0);
            let completion = super::super::pb_ios_reply_completion(reply);
            assert_eq!(completion.request_id, 43);
            assert!(!completion.error.is_null());
            pb_ios_reply_free(reply);
        }
    }

    #[test]
    fn presentation_metadata_preserves_text_and_each_update_kind() {
        for (index, kind) in [UpdateKind::Failed, UpdateKind::Paused].into_iter().enumerate() {
            let reply = owner_response(|| {
                Ok(ReplyData {
                    presentation: Some(Metadata {
                        instance: crate::store::Id::from_bytes([7; 16]),
                        generation: 3,
                        build: "build".into(),
                        invitation: (index != 0).then(|| "invitation".into()),
                        availability: "offline".into(),
                        update: Some(UpdateStatus {
                            kind,
                            build_id: Some("next".into()),
                            message: "é\0🔑".into(),
                        }),
                    }),
                    ..ReplyData::default()
                })
            });
            unsafe {
                let fields = pb_ios_reply_presentation(reply);
                assert_eq!(fields.present, 1);
                assert_eq!(std::slice::from_raw_parts(fields.instance, 16), &[7; 16]);
                assert_eq!(fields.generation, 3);
                assert_eq!(fields.update_kind, index as u32 + 1);
                if index == 0 {
                    assert!(fields.invitation.is_null());
                    assert_eq!(fields.invitation_len, 0);
                } else {
                    assert_eq!(
                        std::slice::from_raw_parts(fields.invitation, fields.invitation_len),
                        b"invitation"
                    );
                }
                for (bytes, len, expected) in [
                    (fields.build, fields.build_len, "build"),
                    (fields.availability, fields.availability_len, "offline"),
                    (fields.update_build, fields.update_build_len, "next"),
                    (fields.update_message, fields.update_message_len, "é\0🔑"),
                ] {
                    assert_eq!(std::slice::from_raw_parts(bytes, len), expected.as_bytes());
                }
                pb_ios_reply_free(reply);
            }
        }
        unsafe {
            assert_eq!(pb_ios_reply_presentation(std::ptr::null()).present, 0);
        }
    }
}
