//! Native database calls run on the existing serialized UIKit engine worker.
use super::*;

/// # Safety
/// Handle remains live and bytes lends a readable UTF-8 span for this call.
/// Returned bytes borrow the reply until `pb_ios_reply_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_database(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        if len == 0 || len > crate::engine::database_browser::MAX_REQUEST_BYTES || bytes.is_null() {
            return Err("Invalid database browser request".into());
        }
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        submit_local(handle, id, LocalRequest::Database(bytes.to_vec()))
    })
}

#[repr(C)]
pub struct DatabaseBytes {
    bytes: *const u8,
    len: usize,
}

/// # Safety
/// Returned bytes borrow the reply and must be copied before freeing it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_database(reply: *const IosReply) -> DatabaseBytes {
    match unsafe { reply.as_ref() }.and_then(|reply| reply.database.as_ref()) {
        Some(bytes) => DatabaseBytes { bytes: bytes.as_ptr(), len: bytes.len() },
        None => DatabaseBytes { bytes: std::ptr::null(), len: 0 },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated C owner and delivery operations.")]
mod tests {
    use super::*;

    #[test]
    fn database_replies_obey_owner_delivery_handshake_including_failed_calls() {
        // A browser completion must be acknowledged exactly like other queued owner replies.
        let directory = tempfile::tempdir().unwrap();
        let profile = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut engine = std::ptr::null_mut();
        unsafe {
            let opened = pb_ios_open(profile.as_ptr(), 0, &mut engine);
            assert!(pb_ios_reply_completion(opened).error.is_null());
            pb_ios_reply_free(opened);
            for (id, request) in [
                (1, br#"{"op":"volumes"}"#.as_slice()),
                (2, b"invalid".as_slice()),
                (3, br#"{"op":"volumes"}"#.as_slice()),
            ] {
                let reply = pb_ios_database(engine, id, request.as_ptr(), request.len());
                let fields = pb_ios_reply_completion(reply);
                assert_eq!(fields.has_response, 1);
                assert_eq!(fields.request_id, id);
                assert_eq!(fields.error.is_null(), id != 2);
                if id != 2 {
                    assert!(!pb_ios_reply_database(reply).bytes.is_null());
                }
                pb_ios_reply_free(reply);
                let ready = pb_ios_delivery_ready(engine);
                assert!(pb_ios_reply_completion(ready).error.is_null());
                pb_ios_reply_free(ready);
            }
            pb_ios_close(engine);
        }
    }
}
