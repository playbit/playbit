//! Log text stays owned by the native reply until Swift has copied its snapshot.
use super::IosReply;
use crate::engine::LogBatch;

pub(super) fn storage(batch: &LogBatch) -> usize {
    batch.lines.iter().fold(
        std::mem::size_of::<LogBatch>()
            .saturating_add(batch.lines.capacity().saturating_mul(std::mem::size_of::<String>())),
        |bytes, line| bytes.saturating_add(line.capacity()),
    )
}
#[repr(C)]
#[derive(Default)]
pub struct LogFields {
    engine: *const u8,
    instance: u64,
    next: u64,
    dropped: u64,
    count: usize,
    reset: u8,
}
#[repr(C)]
#[derive(Default)]
pub struct LogLine {
    bytes: *const u8,
    len: usize,
}
/// # Safety
/// Reply must remain live until the 16-byte engine ID and line spans are copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_logs(reply: *const IosReply) -> LogFields {
    let Some(batch) = (unsafe { reply.as_ref() }).and_then(|reply| reply.logs.as_ref()) else {
        return LogFields::default();
    };
    LogFields {
        engine: batch.cursor.engine.as_bytes().as_ptr(),
        instance: batch.cursor.instance,
        next: batch.cursor.next,
        dropped: batch.dropped,
        count: batch.lines.len(),
        reset: u8::from(batch.reset),
    }
}
/// # Safety
/// Returned bytes borrow the reply; copy before freeing it. A missing line is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_log_line(reply: *const IosReply, index: usize) -> LogLine {
    (unsafe { reply.as_ref() })
        .and_then(|reply| reply.logs.as_ref())
        .and_then(|batch| batch.lines.get(index))
        .map_or_else(LogLine::default, |line| LogLine { bytes: line.as_ptr(), len: line.len() })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert native reply fields and ownership.")]
mod tests {
    use super::super::{ReplyData, owner_response, pb_ios_reply_completion, pb_ios_reply_free};
    use super::*;
    use crate::{engine::LogCursor, store::Id};
    #[test]
    fn native_logs_preserve_cursor_and_text_and_drop_oversized_payloads() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                logs: Some(LogBatch {
                    cursor: LogCursor {
                        engine: Id::from_bytes([7; 16]),
                        instance: u64::MAX,
                        next: u64::MAX,
                    },
                    lines: vec!["same".into(), "same".into(), "é\0🔑".into()],
                    dropped: u64::MAX - 3,
                    reset: false,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            let fields = pb_ios_reply_logs(reply);
            assert_eq!(std::slice::from_raw_parts(fields.engine, 16), &[7; 16]);
            assert_eq!(
                (fields.instance, fields.next, fields.dropped, fields.count, fields.reset),
                (u64::MAX, u64::MAX, u64::MAX - 3, 3, 0)
            );
            for (index, text) in ["same", "same", "é\0🔑"].into_iter().enumerate() {
                let line = pb_ios_reply_log_line(reply, index);
                assert_eq!(std::slice::from_raw_parts(line.bytes, line.len), text.as_bytes());
            }
            assert!(pb_ios_reply_log_line(reply, 3).bytes.is_null());
            pb_ios_reply_free(reply);
            assert!(pb_ios_reply_logs(std::ptr::null()).engine.is_null());
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(14),
                logs: Some(LogBatch {
                    cursor: LogCursor { engine: Id::from_bytes([7; 16]), instance: 1, next: 1 },
                    lines: vec!["x".repeat(super::super::MAX_RESPONSE_BYTES)],
                    dropped: 0,
                    reset: false,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert!(pb_ios_reply_logs(reply).engine.is_null());
            let completed = pb_ios_reply_completion(reply);
            assert_eq!(completed.request_id, 14);
            assert!(!completed.error.is_null());
            pb_ios_reply_free(reply);
        }
    }
}
