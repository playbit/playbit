//! Small typed control results borrowed by Swift until the reply is freed.
use super::IosReply;
pub(super) enum ControlReply {
    AgreementAccepted,
    PersonalVolumesSelected(bool),
    DisplayName(String),
    SignInAvailable(bool),
    SavedAccount(Option<crate::engine::reply::SavedAccount>),
    AccountKeyBackup(crate::store::AccountKeyBackup),
    CodeSent(bool),
    Ordered(bool),
    Forgotten(String),
    Quit { instance: String, drain_failed: bool },
    Session(String),
    Opened { instance: String, generation: u64 },
    Prepared(String),
}
impl ControlReply {
    pub(super) fn storage(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(match self {
            Self::Opened { instance, .. } | Self::Quit { instance, .. } => instance.capacity(),
            Self::Prepared(text) => text.capacity(),
            Self::SavedAccount(Some(value)) => {
                value.email.capacity() + value.account_key.capacity()
            }
            Self::DisplayName(text) | Self::Forgotten(text) | Self::Session(text) => {
                text.capacity()
            }
            _ => 0,
        })
    }
    fn fields(&self) -> ControlFields {
        let (kind, flag, text) = match self {
            Self::AgreementAccepted => (13, true, None),
            Self::AccountKeyBackup(value) => (11, false, Some(value.as_str())),
            Self::PersonalVolumesSelected(value) => (1, *value, None),
            Self::DisplayName(text) => (2, false, Some(text.as_str())),
            Self::SignInAvailable(value) => (3, *value, None),
            Self::SavedAccount(value) => {
                (12, value.is_some(), value.as_ref().map(|v| v.email.as_str()))
            }
            Self::CodeSent(value) => (4, *value, None),
            Self::Ordered(value) => (5, *value, None),
            Self::Forgotten(text) => (6, false, Some(text.as_str())),
            Self::Quit { instance, drain_failed } => (7, *drain_failed, Some(instance.as_str())),
            Self::Session(text) => (8, false, Some(text.as_str())),
            Self::Opened { instance, .. } => (9, false, Some(instance.as_str())),
            Self::Prepared(text) => (10, false, Some(text.as_str())),
        };
        let secret = match self {
            Self::SavedAccount(Some(value)) => Some(value.account_key.as_str()),
            _ => None,
        };
        ControlFields {
            secret: secret.map_or(std::ptr::null(), |text| text.as_ptr()),
            secret_len: secret.map_or(0, |text| text.len()),
            generation: match self {
                Self::Opened { generation, .. } => *generation,
                _ => 0,
            },
            kind,
            flag: u8::from(flag),
            text: text.map_or(std::ptr::null(), |text| text.as_ptr()),
            text_len: text.map_or(0, |text| text.len()),
        }
    }
}
#[repr(C)]
#[derive(Default)]
pub struct ControlFields {
    kind: u32,
    flag: u8,
    text: *const u8,
    text_len: usize,
    generation: u64,
    secret: *const u8,
    secret_len: usize,
}
/// # Safety
/// Text borrows the reply and must be copied before the reply is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_control(reply: *const IosReply) -> ControlFields {
    unsafe { reply.as_ref() }
        .and_then(|reply| reply.control.as_ref())
        .map_or_else(ControlFields::default, ControlReply::fields)
}
#[cfg(test)]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    #[test]
    fn opened_owner_receipt_keeps_its_identity_before_any_scene() {
        let id = crate::store::Id::from_bytes([9; 16]).to_string();
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(42),
                control: Some(ControlReply::Opened { instance: id.clone(), generation: 3 }),
                ..ReplyData::default()
            })
        });
        unsafe {
            let fields = pb_ios_reply_control(reply);
            assert_eq!(fields.kind, 9);
            assert_eq!(fields.generation, 3);
            assert_eq!(std::slice::from_raw_parts(fields.text, fields.text_len), id.as_bytes());
            assert!(super::super::pb_ios_reply_scene(reply).is_null());
            pb_ios_reply_free(reply);
        }
    }
    #[test]
    fn quit_receipt_reports_retirement_separately_from_successful_drain() {
        for drain_failed in [false, true] {
            let reply = ControlReply::Quit { instance: "instance".into(), drain_failed };
            let fields = reply.fields();
            assert_eq!(fields.kind, 7);
            assert_eq!(fields.flag, u8::from(drain_failed));
            assert_eq!(fields.text_len, 8);
        }
    }
    #[test]
    fn control_fields_preserve_boolean_values_and_owned_text() {
        // Each result keeps its discriminator, including false and empty text.
        for flag in [false, true] {
            for (kind, value) in [
                (1, ControlReply::PersonalVolumesSelected(flag)),
                (3, ControlReply::SignInAvailable(flag)),
                (4, ControlReply::CodeSent(flag)),
                (5, ControlReply::Ordered(flag)),
            ] {
                let reply = owner_response(|| {
                    Ok(ReplyData {
                        request_id: Some(7),
                        control: Some(value),
                        ..ReplyData::default()
                    })
                });
                unsafe {
                    let fields = pb_ios_reply_control(reply);
                    assert_eq!(fields.kind, kind);
                    assert_eq!(fields.flag, u8::from(flag));
                    assert!(fields.text.is_null());
                    pb_ios_reply_free(reply);
                }
            }
        }
        for text in ["", "é\0🔑"] {
            for (kind, value) in [
                (2, ControlReply::DisplayName(text.into())),
                (6, ControlReply::Forgotten(text.into())),
                (7, ControlReply::Quit { instance: text.into(), drain_failed: false }),
                (8, ControlReply::Session(text.into())),
            ] {
                let reply = owner_response(|| {
                    Ok(ReplyData { control: Some(value), ..ReplyData::default() })
                });
                unsafe {
                    let fields = pb_ios_reply_control(reply);
                    assert_eq!(fields.kind, kind);
                    assert_eq!(
                        std::slice::from_raw_parts(fields.text, fields.text_len),
                        text.as_bytes()
                    );
                    pb_ios_reply_free(reply);
                }
            }
        }
        unsafe {
            assert_eq!(pb_ios_reply_control(std::ptr::null()).kind, 0);
        }
    }
    #[test]
    fn oversized_control_text_releases_payload_and_retains_correlation() {
        // A native text result must still obey the combined response limit.
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(43),
                control: Some(ControlReply::DisplayName("x".repeat(MAX_RESPONSE_BYTES))),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_control(reply).kind, 0);
            let completion = pb_ios_reply_completion(reply);
            assert_eq!(completion.request_id, 43);
            assert!(!completion.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
