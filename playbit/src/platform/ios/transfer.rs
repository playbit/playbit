//! Borrowed transfer progress for UIKit; the owned reply bounds its lifetime.
use super::IosReply;
use crate::engine::{
    UpdateKind,
    reply::{TransferKind, TransferProgress, TransferStatus},
};

pub(super) fn storage(value: &TransferStatus) -> usize {
    std::mem::size_of::<TransferStatus>()
        .saturating_add(value.session.as_ref().map_or(0, String::capacity))
        .saturating_add(value.message.as_ref().map_or(0, String::capacity))
        .saturating_add(match &value.progress {
            Some(TransferProgress::Connecting { hostname }) => hostname.capacity(),
            _ => 0,
        })
        .saturating_add(value.update.as_ref().map_or(0, |update| {
            update
                .message
                .capacity()
                .saturating_add(update.build_id.as_ref().map_or(0, String::capacity))
        }))
}
#[repr(C)]
#[derive(Default)]
pub struct TransferFields {
    kind: u32,
    session: *const u8,
    session_len: usize,
    message: *const u8,
    message_len: usize,
    update_kind: u32,
    update_build: *const u8,
    update_build_len: usize,
    update_message: *const u8,
    update_message_len: usize,
}
/// # Safety
/// Text borrows the reply and must be copied before the reply is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_transfer(reply: *const IosReply) -> TransferFields {
    let Some(value) = (unsafe { reply.as_ref() }).and_then(|reply| reply.transfer.as_ref()) else {
        return TransferFields::default();
    };
    let mut fields = TransferFields {
        kind: match value.kind {
            TransferKind::Idle => 1,
            TransferKind::Running => 2,
            TransferKind::Complete => 3,
            TransferKind::Paused => 4,
            TransferKind::Canceling => 5,
            TransferKind::Failed => 6,
        },
        ..TransferFields::default()
    };
    if let Some(text) = &value.session {
        fields.session = text.as_ptr();
        fields.session_len = text.len();
    }
    if let Some(text) = &value.message {
        fields.message = text.as_ptr();
        fields.message_len = text.len();
    }
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
#[allow(clippy::unwrap_used, reason = "Tests assert native field ownership.")]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    use crate::engine::UpdateStatus;
    #[test]
    fn all_transfer_states_preserve_optional_text_and_updates() {
        // Exercise every progress state and update state, including NUL and UTF-8.
        for (index, kind) in [
            TransferKind::Idle,
            TransferKind::Running,
            TransferKind::Complete,
            TransferKind::Paused,
            TransferKind::Canceling,
            TransferKind::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            for (update_index, update_kind) in
                [UpdateKind::Failed, UpdateKind::Paused].into_iter().enumerate()
            {
                let reply = owner_response(|| {
                    Ok(ReplyData {
                        request_id: Some(42),
                        transfer: Some(TransferStatus {
                            progress: None,
                            kind,
                            session: Some("session".into()),
                            message: Some("é\0🔑".into()),
                            update: Some(UpdateStatus {
                                kind: update_kind,
                                build_id: Some("build".into()),
                                message: "update".into(),
                            }),
                        }),
                        ..ReplyData::default()
                    })
                });
                unsafe {
                    let fields = pb_ios_reply_transfer(reply);
                    assert_eq!(fields.kind, index as u32 + 1);
                    assert_eq!(fields.update_kind, update_index as u32 + 1);
                    for (bytes, len, text) in [
                        (fields.session, fields.session_len, "session"),
                        (fields.message, fields.message_len, "é\0🔑"),
                        (fields.update_build, fields.update_build_len, "build"),
                        (fields.update_message, fields.update_message_len, "update"),
                    ] {
                        assert_eq!(std::slice::from_raw_parts(bytes, len), text.as_bytes());
                    }
                    pb_ios_reply_free(reply);
                }
            }
        }
        let reply = owner_response(|| {
            Ok(ReplyData { transfer: Some(TransferStatus::default()), ..ReplyData::default() })
        });
        unsafe {
            let fields = pb_ios_reply_transfer(reply);
            assert_eq!(fields.kind, 1);
            assert!(fields.session.is_null());
            assert!(fields.message.is_null());
            assert_eq!(fields.update_kind, 0);
            pb_ios_reply_free(reply);
            assert_eq!(pb_ios_reply_transfer(std::ptr::null()).kind, 0);
        }
    }
    #[test]
    fn oversized_transfer_drops_payload_but_preserves_request_identity() {
        // Native replies must obey the shared allowance even without JSON serialization.
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(43),
                transfer: Some(TransferStatus {
                    message: Some("x".repeat(MAX_RESPONSE_BYTES)),
                    ..TransferStatus::default()
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_transfer(reply).kind, 0);
            let completion = pb_ios_reply_completion(reply);
            assert_eq!(completion.request_id, 43);
            assert!(!completion.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
