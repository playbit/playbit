//! Borrowed recovery payloads for UIKit. No serialized intermediate buffer.
use super::IosReply;
use crate::engine::reply::{PreparedRecovery, RecoveryStatus};

pub(super) enum Payload {
    Status(RecoveryStatus),
    Prepared(PreparedRecovery),
}
impl Payload {
    pub(super) fn storage(&self) -> usize {
        let strings = match self {
            Self::Status(RecoveryStatus::Setup { secret, .. }) => {
                secret.as_ref().map_or(0, String::capacity)
            }
            Self::Prepared(value) => value.secret.capacity(),
            Self::Status(_) => 0,
        };
        std::mem::size_of::<Self>().saturating_add(strings)
    }
}
#[repr(C)]
#[derive(Default)]
pub struct RecoveryText {
    bytes: *const u8,
    len: usize,
}
impl RecoveryText {
    fn new(text: Option<&str>) -> Self {
        text.map_or_else(Self::default, |text| Self { bytes: text.as_ptr(), len: text.len() })
    }
}
#[repr(C)]
#[derive(Default)]
pub struct RecoveryNumber {
    value: u64,
    present: u8,
}
impl RecoveryNumber {
    fn new(value: Option<u64>) -> Self {
        Self { value: value.unwrap_or(0), present: u8::from(value.is_some()) }
    }
}
/// category: 0 absent, 1 local recovery, 2 prepared key.
/// Local kind: 1 ready, 2 setup, 3 locked, 4 canceled.
#[repr(C)]
#[derive(Default)]
pub struct RecoveryFields {
    category: u32,
    kind: u32,
    revision: RecoveryNumber,
    pending_save: RecoveryNumber,
    restored_sessions: RecoveryNumber,
    restored_volumes: RecoveryNumber,
    secret: RecoveryText,
}
/// # Safety
/// Reply must remain live until all borrowed text has been copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_recovery(reply: *const IosReply) -> RecoveryFields {
    let Some(payload) = (unsafe { reply.as_ref() }).and_then(|reply| reply.recovery.as_ref())
    else {
        return RecoveryFields::default();
    };
    fields(payload)
}
pub(super) fn fields(payload: &Payload) -> RecoveryFields {
    let mut fields = RecoveryFields::default();
    match payload {
        Payload::Prepared(value) => {
            fields.category = 2;
            fields.secret = RecoveryText::new(Some(&value.secret));
        }
        Payload::Status(value) => {
            fields.category = 1;
            match value {
                RecoveryStatus::Ready {
                    revision,
                    pending_save,
                    restored_sessions,
                    restored_volumes,
                } => {
                    fields.kind = 1;
                    fields.revision = RecoveryNumber::new(Some(*revision));
                    fields.pending_save = RecoveryNumber::new(Some(u64::from(*pending_save)));
                    fields.restored_sessions =
                        RecoveryNumber::new(restored_sessions.map(|n| n as u64));
                    fields.restored_volumes =
                        RecoveryNumber::new(restored_volumes.map(|n| n as u64));
                }
                RecoveryStatus::Setup { revision, pending_save, secret } => {
                    fields.kind = 2;
                    fields.revision = RecoveryNumber::new(Some(*revision));
                    fields.pending_save = RecoveryNumber::new(Some(u64::from(*pending_save)));
                    fields.secret = RecoveryText::new(secret.as_deref());
                }
                RecoveryStatus::Locked { revision, pending_save } => {
                    fields.kind = 3;
                    fields.revision = RecoveryNumber::new(Some(*revision));
                    fields.pending_save = RecoveryNumber::new(Some(u64::from(*pending_save)));
                }
                RecoveryStatus::Canceled => fields.kind = 4,
            }
        }
    }
    fields
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bridge ownership and field preservation.")]
mod tests {
    use super::super::{ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free};
    use super::*;

    #[test]
    fn local_states_preserve_optional_fields_and_owned_key_text() {
        let cases = [
            Payload::Status(RecoveryStatus::Ready {
                revision: u64::MAX,
                pending_save: false,
                restored_sessions: Some(0),
                restored_volumes: Some(7),
            }),
            Payload::Status(RecoveryStatus::Setup {
                revision: 0,
                pending_save: true,
                secret: Some("clé\0🔑".into()),
            }),
            Payload::Status(RecoveryStatus::Locked { revision: 4, pending_save: false }),
            Payload::Status(RecoveryStatus::Canceled),
            Payload::Prepared(PreparedRecovery { secret: "clé\0🔑".into() }),
        ];
        for (index, payload) in cases.into_iter().enumerate() {
            let reply = owner_response(|| {
                Ok(ReplyData { recovery: Some(payload), ..ReplyData::default() })
            });
            unsafe {
                let fields = pb_ios_reply_recovery(reply);
                assert_eq!(fields.category, if index == 4 { 2 } else { 1 });
                assert_eq!(fields.kind, if index == 4 { 0 } else { index as u32 + 1 });
                assert_eq!(fields.revision.present, u8::from(index < 3));
                assert_eq!(fields.pending_save.present, u8::from(index < 3));
                if index == 0 {
                    assert_eq!(fields.revision.value, u64::MAX);
                    assert_eq!(fields.restored_sessions.present, 1);
                    assert_eq!(fields.restored_sessions.value, 0);
                    assert_eq!(fields.restored_volumes.value, 7);
                }
                if index == 1 || index == 4 {
                    assert_eq!(
                        std::slice::from_raw_parts(fields.secret.bytes, fields.secret.len),
                        "clé\0🔑".as_bytes()
                    );
                } else {
                    assert!(fields.secret.bytes.is_null());
                }
                pb_ios_reply_free(reply);
            }
        }
    }

    #[test]
    fn oversized_prepared_key_is_rejected() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(91),
                recovery: Some(Payload::Prepared(PreparedRecovery {
                    secret: "x".repeat(super::super::MAX_RESPONSE_BYTES),
                })),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_recovery(reply).category, 0);
            let completion = pb_ios_reply_completion(reply);
            assert_eq!(completion.request_id, 91);
            assert!(!completion.error.is_null());
            pb_ios_reply_free(reply);
            assert_eq!(pb_ios_reply_recovery(std::ptr::null()).category, 0);
        }
    }
}
