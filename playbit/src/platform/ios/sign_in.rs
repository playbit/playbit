//! Sign-in completion owns optional recovery state; every field borrows until reply free.
use super::{IosReply, recovery, status::StatusText};
use crate::engine::reply::SignInReply;

pub(super) struct Payload {
    pub signed_in: bool,
    pub replacement_required: bool,
    pub recovery: Option<recovery::Payload>,
    pub recovery_error: Option<String>,
}
impl Payload {
    pub(super) fn new(value: SignInReply) -> Self {
        Self {
            signed_in: value.signed_in,
            replacement_required: value.replacement_required,
            recovery: value.recovery.map(recovery::Payload::Status),
            recovery_error: value.recovery_error,
        }
    }
    pub(super) fn storage(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.recovery.as_ref().map_or(0, recovery::Payload::storage))
            .saturating_add(self.recovery_error.as_ref().map_or(0, String::capacity))
    }
}
#[repr(C)]
#[derive(Default)]
pub struct SignInFields {
    present: u8,
    signed_in: u8,
    replacement_required: u8,
    recovery: recovery::RecoveryFields,
    recovery_error: StatusText,
}
/// # Safety
/// All text borrows the reply and must be copied before it is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_sign_in(reply: *const IosReply) -> SignInFields {
    (unsafe { reply.as_ref() }).and_then(|reply| reply.sign_in.as_ref()).map_or_else(
        SignInFields::default,
        |value| SignInFields {
            present: 1,
            signed_in: u8::from(value.signed_in),
            replacement_required: u8::from(value.replacement_required),
            recovery: value
                .recovery
                .as_ref()
                .map_or_else(recovery::RecoveryFields::default, recovery::fields),
            recovery_error: value
                .recovery_error
                .as_deref()
                .map_or_else(StatusText::default, StatusText::new),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    use crate::engine::reply::RecoveryStatus;
    #[test]
    fn sign_in_owns_optional_recovery_and_error_spans() {
        for signed_in in [false, true] {
            for recovery in [
                None,
                Some(RecoveryStatus::Setup {
                    revision: u64::MAX,
                    pending_save: false,
                    secret: Some("é\0🔑".into()),
                }),
            ] {
                let reply = owner_response(|| {
                    Ok(ReplyData {
                        sign_in: Some(Payload::new(SignInReply {
                            replacement_required: false,
                            signed_in,
                            recovery,
                            recovery_error: Some("é\0🔑".into()),
                        })),
                        ..ReplyData::default()
                    })
                });
                unsafe {
                    let fields = pb_ios_reply_sign_in(reply);
                    assert_eq!(fields.present, 1);
                    assert_eq!(fields.signed_in, u8::from(signed_in));
                    assert_eq!(
                        std::slice::from_raw_parts(
                            fields.recovery_error.bytes,
                            fields.recovery_error.len
                        ),
                        "é\0🔑".as_bytes()
                    );
                    pb_ios_reply_free(reply);
                }
            }
        }
        unsafe {
            assert_eq!(pb_ios_reply_sign_in(std::ptr::null()).present, 0);
        }
    }
    #[test]
    fn sign_in_capacity_overflow_preserves_request_correlation() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(82),
                sign_in: Some(Payload {
                    signed_in: true,
                    replacement_required: false,
                    recovery: None,
                    recovery_error: Some(String::with_capacity(MAX_RESPONSE_BYTES)),
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_sign_in(reply).present, 0);
            assert_eq!(pb_ios_reply_completion(reply).request_id, 82);
            assert!(!pb_ios_reply_completion(reply).error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
