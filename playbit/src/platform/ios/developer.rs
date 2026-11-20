//! Published app pages and display projections retain only fields UIKit consumes.
use super::{IosReply, status::StatusText};
use crate::{engine::reply::AppPage, network::ap::operation::AppState, package::AppIdentity};
pub(super) struct Row {
    pub identity: AppIdentity,
    pub build: String,
    pub state: AppState,
}
pub(super) enum Payload {
    Page { rows: Vec<Row>, next: Option<String> },
    Display { build: String, name: String },
}
impl Payload {
    pub(super) fn page(value: AppPage) -> Self {
        Self::Page {
            rows: value
                .apps
                .into_iter()
                .map(|row| Row { identity: row.identity, build: row.build_id, state: row.state })
                .collect(),
            next: value.next,
        }
    }
    pub(super) fn storage(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(match self {
            Self::Page { rows, next } => rows.iter().fold(
                rows.capacity()
                    .saturating_mul(std::mem::size_of::<Row>())
                    .saturating_add(next.as_ref().map_or(0, String::capacity)),
                |bytes, row| {
                    bytes
                        .saturating_add(row.identity.app_id.capacity())
                        .saturating_add(row.build.capacity())
                },
            ),
            Self::Display { build, name } => build.capacity().saturating_add(name.capacity()),
        })
    }
}
#[repr(C)]
#[derive(Default)]
pub struct DeveloperFields {
    kind: u32,
    count: usize,
    next: StatusText,
    build: StatusText,
    name: StatusText,
}
#[repr(C)]
#[derive(Default)]
pub struct AppFields {
    publisher: *const u8,
    app_id: StatusText,
    build: StatusText,
    state: u32,
}
/// # Safety
/// Returned spans and 16-byte IDs borrow the reply until it is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_developer(reply: *const IosReply) -> DeveloperFields {
    match (unsafe { reply.as_ref() }).and_then(|reply| reply.developer.as_ref()) {
        Some(Payload::Page { rows, next }) => DeveloperFields {
            kind: 1,
            count: rows.len(),
            next: next.as_deref().map_or_else(StatusText::default, StatusText::new),
            ..DeveloperFields::default()
        },
        Some(Payload::Display { build, name }) => DeveloperFields {
            kind: 2,
            build: StatusText::new(build),
            name: StatusText::new(name),
            ..DeveloperFields::default()
        },
        None => DeveloperFields::default(),
    }
}
/// # Safety
/// Fields borrow the reply. Out-of-range indices return empty fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_app(reply: *const IosReply, index: usize) -> AppFields {
    let Some(Payload::Page { rows, .. }) =
        (unsafe { reply.as_ref() }).and_then(|reply| reply.developer.as_ref())
    else {
        return AppFields::default();
    };
    rows.get(index).map_or_else(AppFields::default, |row| AppFields {
        publisher: row.identity.publisher.as_bytes().as_ptr(),
        app_id: StatusText::new(&row.identity.app_id),
        build: StatusText::new(&row.build),
        state: match row.state {
            AppState::Active => 1,
            AppState::Disabled => 2,
            AppState::Deleted => 3,
        },
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
    fn app_fields_preserve_publisher_app_id_build_cursor_and_all_states() {
        for (state, expected) in
            [(AppState::Active, 1), (AppState::Disabled, 2), (AppState::Deleted, 3)]
        {
            let build = crate::identifier::encode(&[0xbb; 32]);
            let reply = owner_response(|| {
                Ok(ReplyData {
                    developer: Some(Payload::Page {
                        rows: vec![Row {
                            identity: AppIdentity {
                                publisher: Id::from_bytes([1; 16]),
                                app_id: "notes.dev".into(),
                            },
                            build: build.clone(),
                            state,
                        }],
                        next: Some("notes.dev".into()),
                    }),
                    ..ReplyData::default()
                })
            });
            unsafe {
                let fields = pb_ios_reply_developer(reply);
                assert_eq!(fields.kind, 1);
                assert_eq!(fields.count, 1);
                assert_eq!(
                    std::slice::from_raw_parts(fields.next.bytes, fields.next.len),
                    b"notes.dev"
                );
                let row = pb_ios_reply_app(reply, 0);
                assert_eq!(row.state, expected);
                assert_eq!(std::slice::from_raw_parts(row.publisher, 16), &[1; 16]);
                assert_eq!(
                    std::slice::from_raw_parts(row.app_id.bytes, row.app_id.len),
                    b"notes.dev"
                );
                assert_eq!(
                    std::slice::from_raw_parts(row.build.bytes, row.build.len),
                    build.as_bytes()
                );
                assert!(pb_ios_reply_app(reply, 1).publisher.is_null());
                pb_ios_reply_free(reply);
            }
        }
    }
    #[test]
    fn display_spans_and_unused_page_capacity_are_accounted() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                developer: Some(Payload::Display {
                    build: crate::identifier::encode(&[0xbb; 32]),
                    name: "é\0🔑".into(),
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            let fields = pb_ios_reply_developer(reply);
            assert_eq!(fields.kind, 2);
            assert_eq!(
                std::slice::from_raw_parts(fields.name.bytes, fields.name.len),
                "é\0🔑".as_bytes()
            );
            pb_ios_reply_free(reply);
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(83),
                developer: Some(Payload::Page {
                    rows: Vec::with_capacity(MAX_RESPONSE_BYTES / std::mem::size_of::<Row>() + 1),
                    next: None,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert_eq!(pb_ios_reply_developer(reply).kind, 0);
            assert_eq!(pb_ios_reply_completion(reply).request_id, 83);
            assert!(!pb_ios_reply_completion(reply).error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
