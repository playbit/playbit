//! Bounded personal-volume consent data, borrowed until the native reply is freed.
use super::IosReply;
use crate::store::{PersonalVolumeGrant, Volume};
pub(super) struct Payload {
    pub session: String,
    pub volumes: Vec<Volume>,
    pub grants: Option<Vec<PersonalVolumeGrant>>,
}
impl Payload {
    pub(super) fn storage(&self) -> usize {
        let bytes = std::mem::size_of::<Self>()
            .saturating_add(self.session.capacity())
            .saturating_add(self.volumes.capacity().saturating_mul(std::mem::size_of::<Volume>()))
            .saturating_add(self.grants.as_ref().map_or(0, |grants| {
                grants.capacity().saturating_mul(std::mem::size_of::<PersonalVolumeGrant>())
            }));
        self.volumes
            .iter()
            .fold(bytes, |bytes, volume| bytes.saturating_add(volume.name.capacity()))
    }
}
#[repr(C)]
#[derive(Default)]
pub struct PersonalVolumeFields {
    session: *const u8,
    session_len: usize,
    volumes: usize,
    grants: usize,
    has_grants: u8,
}
#[repr(C)]
#[derive(Default)]
pub struct PersonalVolumeRow {
    pub(super) id: *const u8,
    pub(super) name: *const u8,
    pub(super) name_len: usize,
}
#[repr(C)]
#[derive(Default)]
pub struct PersonalGrant {
    id: *const u8,
    write: u8,
}
/// # Safety
/// Session text borrows the live reply. Copy it before freeing the reply.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_personal_volumes(
    reply: *const IosReply,
) -> PersonalVolumeFields {
    let Some(value) = (unsafe { reply.as_ref() }).and_then(|reply| reply.personal_volumes.as_ref())
    else {
        return PersonalVolumeFields::default();
    };
    PersonalVolumeFields {
        session: value.session.as_ptr(),
        session_len: value.session.len(),
        volumes: value.volumes.len(),
        grants: value.grants.as_ref().map_or(0, Vec::len),
        has_grants: u8::from(value.grants.is_some()),
    }
}
/// # Safety
/// The 16-byte ID and name borrow the reply. Out-of-range indices return null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_personal_volume(
    reply: *const IosReply,
    index: usize,
) -> PersonalVolumeRow {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.personal_volumes.as_ref())
        .and_then(|value| value.volumes.get(index))
        .map_or_else(PersonalVolumeRow::default, |volume| PersonalVolumeRow {
            id: volume.id.as_bytes().as_ptr(),
            name: volume.name.as_ptr(),
            name_len: volume.name.len(),
        })
}
/// # Safety
/// The 16-byte ID borrows the reply. Missing grants/indices return null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_personal_grant(
    reply: *const IosReply,
    index: usize,
) -> PersonalGrant {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.personal_volumes.as_ref())
        .and_then(|value| value.grants.as_ref())
        .and_then(|grants| grants.get(index))
        .map_or_else(PersonalGrant::default, |grant| PersonalGrant {
            id: grant.volume.as_bytes().as_ptr(),
            write: u8::from(grant.write),
        })
}
#[cfg(test)]
mod tests {
    use super::super::{
        MAX_RESPONSE_BYTES, ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free,
    };
    use super::*;
    use crate::store::Id;
    #[test]
    fn native_consent_preserves_absent_empty_and_saved_grants() {
        // Never-consented and explicitly empty grants remain distinguishable.
        for grants in [
            None,
            Some(vec![]),
            Some(vec![
                PersonalVolumeGrant { volume: Id::from_bytes([255; 16]), write: false },
                PersonalVolumeGrant { volume: Id::from_bytes([7; 16]), write: true },
            ]),
        ] {
            let present = grants.is_some();
            let count = grants.as_ref().map_or(0, Vec::len);
            let reply = owner_response(|| {
                Ok(ReplyData {
                    personal_volumes: Some(Payload {
                        session: "build".into(),
                        volumes: vec![Volume {
                            id: Id::from_bytes([255; 16]),
                            owner: Id::from_bytes([1; 16]),
                            name: "é\0🔑".into(),
                            personal: true,
                            share_key_with_service: false,
                        }],
                        grants,
                    }),
                    ..ReplyData::default()
                })
            });
            unsafe {
                let fields = pb_ios_reply_personal_volumes(reply);
                assert_eq!(fields.has_grants, u8::from(present));
                assert_eq!(fields.grants, count);
                assert_eq!(fields.volumes, 1);
                assert_eq!(
                    std::slice::from_raw_parts(fields.session, fields.session_len),
                    b"build"
                );
                let row = pb_ios_reply_personal_volume(reply, 0);
                assert_eq!(std::slice::from_raw_parts(row.id, 16), &[255; 16]);
                assert_eq!(std::slice::from_raw_parts(row.name, row.name_len), "é\0🔑".as_bytes());
                assert!(pb_ios_reply_personal_volume(reply, usize::MAX).id.is_null());
                assert!(pb_ios_reply_personal_grant(reply, count).id.is_null());
                if count != 0 {
                    for (index, byte) in [255, 7].into_iter().enumerate() {
                        let grant = pb_ios_reply_personal_grant(reply, index);
                        assert_eq!(std::slice::from_raw_parts(grant.id, 16), &[byte; 16]);
                        assert_eq!(grant.write, index as u8);
                    }
                }
                pb_ios_reply_free(reply);
            }
        }
        unsafe {
            assert!(pb_ios_reply_personal_volumes(std::ptr::null()).session.is_null());
        }
    }
    #[test]
    fn consent_storage_counts_unused_capacity_and_preserves_oversize_correlation() {
        // Retained spare capacity must obey the same bound as visible text.
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(9),
                personal_volumes: Some(Payload {
                    session: String::with_capacity(MAX_RESPONSE_BYTES),
                    volumes: vec![],
                    grants: None,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert!(pb_ios_reply_personal_volumes(reply).session.is_null());
            let completed = pb_ios_reply_completion(reply);
            assert_eq!(completed.request_id, 9);
            assert!(!completed.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
