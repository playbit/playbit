//! Cursor-based reads of bounded local-owner logs.
use super::Result;
use crate::store::Id;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct LogCursor {
    pub engine: Id,
    pub instance: u64,
    pub next: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[cfg_attr(feature = "app-host", rkyv(bytecheck(verify)))]
pub struct LogBatch {
    pub cursor: LogCursor,
    #[serde(deserialize_with = "bounded_lines")]
    pub lines: Vec<String>,
    pub dropped: u64,
    pub reset: bool,
}

#[cfg(feature = "app-host")]
#[allow(
    unsafe_code,
    reason = "Verify checks log limits after bytecheck validates every vector and string."
)]
// SAFETY: all fields are valid archived values. Check both retained-log limits
// without assuming anything about their lengths or allocating owned strings.
unsafe impl<C> rkyv::bytecheck::Verify<C> for ArchivedLogBatch
where
    C: rkyv::rancor::Fallible + ?Sized,
    C::Error: rkyv::rancor::Source,
{
    fn verify(&self, _: &mut C) -> std::result::Result<(), C::Error> {
        if self.lines.len() > 256
            || self
                .lines
                .iter()
                .try_fold(0usize, |sum, line| sum.checked_add(line.len()))
                .is_none_or(|bytes| bytes > 64 * 1024)
        {
            return Err(rkyv::rancor::Source::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "app log text limit",
            )));
        }
        Ok(())
    }
}

/// The lines after `after` of an instance whose retained `logs` end at
/// `cursor`. Batches must pass `validate_after`; following their cursors
/// reconstructs the log, with dropped lines reported explicitly.
pub(crate) fn batch(
    cursor: LogCursor,
    logs: &[String],
    after: Option<LogCursor>,
) -> Result<Option<LogBatch>> {
    let same =
        after.is_some_and(|old| old.engine == cursor.engine && old.instance == cursor.instance);
    let requested = if same { after.expect("same-generation cursors are present").next } else { 0 };
    if requested > cursor.next {
        return Err("log cursor is ahead of this instance".into());
    }
    if after == Some(cursor) {
        return Ok(None);
    }
    let _timing = crate::trace::Span::new("engine.logs.snapshot");
    let first = cursor.next - logs.len() as u64;
    let start = requested.saturating_sub(first) as usize;
    let result = LogBatch {
        cursor,
        lines: logs[start..].to_vec(),
        dropped: first.saturating_sub(requested),
        reset: after.is_some() && !same,
    };
    result.validate_after(after)?;
    Ok(Some(result))
}

// Bound each decoded line before allocating its String, then bound the aggregate
// before retaining another line. serde_json borrows unescaped input; its escape
// scratch plus the512 KiB wire buffer remain within the1 MiB client allowance.
fn bounded_lines<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<String>, D::Error> {
    struct Lines;
    impl<'de> serde::de::Visitor<'de> for Lines {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded app log lines")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut lines = Vec::new();
            let mut bytes = 0;
            while let Some(line) = seq.next_element::<BoundedLine>()? {
                bytes += line.0.len();
                if lines.len() == 256 || bytes > 64 * 1024 {
                    return Err(serde::de::Error::custom("app log text limit"));
                }
                lines.push(line.0);
            }
            Ok(lines)
        }
    }
    d.deserialize_seq(Lines)
}
pub(crate) struct BoundedLine(pub String);
impl<'de> Deserialize<'de> for BoundedLine {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Line;
        impl serde::de::Visitor<'_> for Line {
            type Value = BoundedLine;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded app log text")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                if value.len() > 64 * 1024 {
                    return Err(E::custom("app log line exceeds64 KiB"));
                }
                Ok(BoundedLine(value.to_owned()))
            }
        }
        d.deserialize_str(Line)
    }
}
impl LogBatch {
    pub(crate) fn validate_after(&self, after: Option<LogCursor>) -> Result<()> {
        let same = after.is_some_and(|old| {
            old.engine == self.cursor.engine && old.instance == self.cursor.instance
        });
        if self.reset != (after.is_some() && !same) {
            return Err("invalid log reset marker".into());
        }
        let first = if same { after.expect("same-generation cursors are present").next } else { 0 };
        let count = self.cursor.next.checked_sub(first).ok_or("log cursor regressed")?;
        if (same && count == 0) || Some(count) != self.dropped.checked_add(self.lines.len() as u64)
        {
            return Err("invalid log cursor or dropped-line count".into());
        }
        Ok(())
    }
}

pub(super) const MAX_RETAINED_LOG_BYTES: usize = 64 * 1024;
/// Keep the newest lines within 256 lines and `MAX_RETAINED_LOG_BYTES`; a
/// single line over the byte limit is dropped too.
pub(crate) fn retain_recent_logs(logs: &mut Vec<String>) {
    let start = retained_start(logs, String::len);
    logs.drain(..start);
}

/// The index of the first line `retain_recent_logs` keeps. Generic over the
/// line so a proof can use byte counts instead of strings.
pub(super) fn retained_start<T>(logs: &[T], bytes_of: fn(&T) -> usize) -> usize {
    let mut bytes: usize = logs.iter().map(bytes_of).sum();
    let mut remove = logs.len().saturating_sub(256);
    bytes -= logs[..remove].iter().map(bytes_of).sum::<usize>();
    while bytes > MAX_RETAINED_LOG_BYTES {
        bytes -= bytes_of(&logs[remove]);
        remove += 1;
    }
    remove
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::engine::{Engine, local_owner_test_support::LocalApps};
    #[test]
    fn archived_log_limits_apply_inside_replies_before_materialization() {
        use crate::engine::ipc::codec;
        for (lines, valid) in [
            (Vec::new(), true),
            (vec!["x".repeat(256); 256], true),
            (vec![String::new(); 257], false),
            (vec!["x".repeat(64 * 1024 + 1)], false),
            (vec!["x".repeat(32769); 2], false),
        ] {
            let reply = LogBatch {
                cursor: LogCursor {
                    engine: Id::from_bytes([1; 16]),
                    instance: 1,
                    next: lines.len() as u64,
                },
                lines,
                dropped: 0,
                reset: false,
            };
            let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
            assert_eq!(codec::access::<ArchivedLogBatch>(&bytes).is_ok(), valid);
            assert_eq!(
                rkyv::api::low::from_bytes::<LogBatch, rkyv::rancor::Error>(&bytes).is_ok(),
                valid
            );
        }
    }
    #[test]
    fn cursors_preserve_identical_lines_report_rotation_and_reset_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Logs", false).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let mut owners = LocalApps::default();
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        let incarnation = engine.change_token().generation;
        let first = owners.apps.log_batch(instance, incarnation, None).unwrap().unwrap();
        assert_eq!(first.lines, ["turn"]);
        first.validate_after(None).unwrap();
        assert!(
            owners.apps.log_batch(instance, incarnation, Some(first.cursor)).unwrap().is_none()
        );
        for _ in 0..300 {
            owners.input(instance, crate::engine::input_event(2, 1, vec![]).unwrap()).unwrap();
        }
        let batch =
            owners.apps.log_batch(instance, incarnation, Some(first.cursor)).unwrap().unwrap();
        assert_eq!(batch.lines, vec!["turn"; 256]);
        assert_eq!(batch.dropped, 44);
        assert!(!batch.reset);
        batch.validate_after(Some(first.cursor)).unwrap();
        assert!(
            owners
                .apps
                .log_batch(instance, incarnation, Some(LogCursor { next: 302, ..batch.cursor }))
                .is_err()
        );
        owners.quit(&mut engine, instance).unwrap();
        assert!(owners.apps.log_batch(instance, incarnation, Some(batch.cursor)).is_err());
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        let restarted =
            owners.apps.log_batch(instance, incarnation, Some(batch.cursor)).unwrap().unwrap();
        assert!(restarted.reset);
        assert_eq!(restarted.lines, ["turn"]);
        restarted.validate_after(Some(batch.cursor)).unwrap();
    }
    #[test]
    fn malformed_log_batches_are_bounded_and_cannot_create_a_busy_loop() {
        let cursor = LogCursor { engine: Id::generate().unwrap(), instance: 1, next: 1 };
        let batch = LogBatch { cursor, lines: vec!["same".into()], dropped: 0, reset: false };
        batch.validate_after(None).unwrap();
        assert!(batch.validate_after(Some(cursor)).is_err());
        let mut value = serde_json::to_value(&batch).unwrap();
        value["lines"] = serde_json::json!(vec![""; 257]);
        assert!(serde_json::from_slice::<LogBatch>(&serde_json::to_vec(&value).unwrap()).is_err());
        value["lines"] = serde_json::json!(["x".repeat(65537)]);
        assert!(serde_json::from_slice::<LogBatch>(&serde_json::to_vec(&value).unwrap()).is_err());
        value["lines"] = serde_json::json!(["x".repeat(32769), "y".repeat(32768)]);
        assert!(serde_json::from_slice::<LogBatch>(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(
            LogBatch { cursor, lines: vec![], dropped: 0, reset: false }
                .validate_after(None)
                .is_err()
        );
    }

    /// Checks log batches and retention against fixed regression cases.
    /// Line `i` of an instance's log is the text of `i`.
    #[test]
    fn log_cursors_and_retention_match_regression_cases() {
        fn cursor(w: &[&str]) -> Option<LogCursor> {
            (w[0] != "-").then(|| LogCursor {
                engine: Id::from_bytes([w[0].parse().unwrap(); 16]),
                instance: w[1].parse().unwrap(),
                next: w[2].parse().unwrap(),
            })
        }
        fn lines(first: &str, count: &str) -> Vec<String> {
            let first: u64 = first.parse().unwrap();
            (first..first + count.parse::<u64>().unwrap()).map(|line| line.to_string()).collect()
        }
        fn batch_words(batch: &LogBatch) -> Vec<String> {
            let first = batch.lines.first().map_or("0", String::as_str);
            let cursor = batch.cursor;
            [
                cursor.engine.0[0].to_string(),
                cursor.instance.to_string(),
                cursor.next.to_string(),
                batch.dropped.to_string(),
                batch.reset.to_string(),
                batch.lines.len().to_string(),
                first.to_string(),
            ]
            .into()
        }
        let (mut batches, mut validations, mut trims) = (0, 0, 0);
        for line in include_str!("../../tests/fixtures/logs.txt").lines() {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[0] {
                "batch" => {
                    let next: u64 = w[1].parse().unwrap();
                    let retained: u64 = w[2].parse().unwrap();
                    let logs = lines(&(next - retained).to_string(), w[2]);
                    let at = LogCursor { engine: Id::from_bytes([1; 16]), instance: 1, next };
                    let actual = match batch(at, &logs, cursor(&w[3..6])) {
                        Err(error) => {
                            assert_eq!(error.to_string(), "log cursor is ahead of this instance");
                            vec!["error".to_string(), "ahead".into()]
                        }
                        Ok(None) => vec!["none".to_string()],
                        Ok(Some(batch)) => {
                            assert_eq!(w[6], "batch", "{line}");
                            assert_eq!(batch.lines, lines(w[13], w[12]), "{line}");
                            batch.validate_after(cursor(&w[3..6])).unwrap();
                            let mut words = vec!["batch".to_string()];
                            words.extend(batch_words(&batch));
                            words
                        }
                    };
                    assert_eq!(actual, w[6..], "{line}");
                    batches += 1;
                }
                "validate" => {
                    let batch = LogBatch {
                        cursor: cursor(&w[4..7]).unwrap(),
                        lines: lines(w[10], w[9]),
                        dropped: w[7].parse().unwrap(),
                        reset: w[8] == "true",
                    };
                    let actual = match batch.validate_after(cursor(&w[1..4])) {
                        Ok(()) => "ok".to_string(),
                        Err(error) => match error.to_string().as_str() {
                            "invalid log reset marker" => "reset".into(),
                            "log cursor regressed" => "regressed".into(),
                            "invalid log cursor or dropped-line count" => "count".into(),
                            other => other.into(),
                        },
                    };
                    assert_eq!(actual, w[11], "{line}");
                    validations += 1;
                }
                "trim" => {
                    let count: usize = w[1].parse().unwrap();
                    let lines: Vec<String> = w[2..2 + count]
                        .iter()
                        .map(|size| "x".repeat(size.parse().unwrap()))
                        .collect();
                    let mut logs = lines.clone();
                    retain_recent_logs(&mut logs);
                    let kept: usize = w[2 + count].parse().unwrap();
                    assert_eq!(logs, lines[count - kept..], "{line}");
                    trims += 1;
                }
                other => panic!("unknown log vector {other}"),
            }
        }
        assert!(batches > 0 && validations > 0 && trims > 0);
    }
}
