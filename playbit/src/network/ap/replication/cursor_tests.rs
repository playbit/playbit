//! Cursor decisions, host durability boundaries and fixed regression cases.
#![allow(clippy::unwrap_used, reason = "Tests assert bounded protocol fixtures.")]
use super::*;
fn ack() -> SubscriptionAck {
    SubscriptionAck { epoch: [1; 16], sequence: 0, policy_revision: 5 }
}
fn event(sequence: u64, revision: u64) -> ReplicaEvent<'static> {
    ReplicaEvent {
        epoch: [1; 16],
        sequence,
        policy_revision: revision,
        kind: ReplicaEventKind::Changes {
            database: crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([7; 15]).unwrap(),
            ),
            delta: &[1],
        },
    }
}
fn epoch(word: &str) -> [u8; 16] {
    [word.parse().unwrap(); 16]
}
#[test]
fn apply_cursor_rejects_a_skipped_durable_boundary() {
    // Catch a host bug that bypasses external event admission before publishing progress.
    let mut cursor = Cursor::new(&ack()).unwrap();
    let original = cursor.sequence;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cursor.record_applied(original + 2, cursor.policy_revision);
        }))
        .is_err()
    );
    assert_eq!(cursor.sequence, original);
}

#[test]
fn continuity_advances_only_after_success_and_allows_old_duplicates() {
    let mut cursor = Cursor::new(&ack()).unwrap();
    let first = event(1, 6);
    assert_eq!(cursor.accept(&first), Ok(true));
    assert_eq!(cursor.sequence, 0); // Receiving is not successful apply.
    assert_eq!(cursor.accept(&event(2, 6)), Err(CursorError::Gap));
    cursor.applied(&first);
    assert_eq!(cursor.accept(&first), Ok(false));
    let second = event(2, 8);
    assert_eq!(cursor.accept(&second), Ok(true));
    cursor.applied(&second);
    assert_eq!(cursor.accept(&first), Ok(false)); // Its older revision is valid.
    assert_eq!(
        cursor.accept(&event(3, 7)),
        Err(CursorError::Invalid("replica policy revision regressed"))
    );
    let mut changed_epoch = event(2, 8);
    changed_epoch.epoch = [2; 16];
    assert_eq!(
        cursor.accept(&changed_epoch),
        Err(CursorError::Invalid("replica stream epoch changed"))
    );
    cursor.sequence = u64::MAX;
    assert_eq!(cursor.accept(&event(u64::MAX, 8)), Ok(false));
    assert!(cursor.accept(&event(0, 8)).is_err());
}

#[test]
fn caught_up_and_terminal_events_obey_stream_order() {
    let mut cursor = Cursor::new(&ack()).unwrap();
    let mut checkpoint = event(1, 6);
    checkpoint.kind = ReplicaEventKind::CaughtUp;
    cursor.applied(&checkpoint);
    let terminal = ReplicaEvent {
        sequence: 2,
        policy_revision: 0,
        kind: ReplicaEventKind::Terminal {
            volume: [7; 16],
            cause: crate::network::ap::replication::TerminalCause::AuthorityChanged,
        },
        ..checkpoint
    };
    assert_eq!(cursor.accept(&terminal), Ok(true));
    assert!(
        cursor
            .accept(&ReplicaEvent { sequence: 3, kind: ReplicaEventKind::Gap, ..terminal })
            .unwrap()
    );
    assert!(
        cursor
            .accept(&ReplicaEvent { sequence: 3, kind: ReplicaEventKind::CaughtUp, ..checkpoint })
            .is_err()
    );
    assert_eq!(cursor.accept(&ReplicaEvent { sequence: 1, ..checkpoint }), Ok(false));
}

#[test]
fn cursor_matches_regression_cases() {
    static PAYLOAD: [u8; 65537] = [0; 65537];
    for line in include_str!("../../../../tests/fixtures/cursor.txt").lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let number = |i: usize| w[i].parse::<u64>().unwrap();
        let size = |i: usize| usize::try_from(number(i)).unwrap();
        let cursor = Cursor { epoch: epoch(w[0]), sequence: number(1), policy_revision: number(2) };
        let kind = match w[6] {
            "changes" => ReplicaEventKind::Changes {
                database: match number(7) {
                    0 => crate::store::DatabaseId::Index,
                    1 => crate::store::DatabaseId::Revocations,
                    2 => crate::store::DatabaseId::Entry(
                        crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                    ),
                    other => panic!("unknown database class {other}"),
                },
                delta: &PAYLOAD[..size(8)],
            },
            "bloblog" => ReplicaEventKind::Bloblog {
                log: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                data: &PAYLOAD[..size(7)],
            },
            "caughtup" => ReplicaEventKind::CaughtUp,
            "gap" => ReplicaEventKind::Gap,
            "terminal" => ReplicaEventKind::Terminal {
                volume: if number(7) == 0 { [0; 16] } else { [7; 16] },
                cause: crate::network::ap::replication::TerminalCause::AuthorityChanged,
            },
            other => panic!("unknown event kind {other}"),
        };
        let event = ReplicaEvent {
            epoch: epoch(w[3]),
            sequence: number(4),
            policy_revision: number(5),
            kind,
        };
        let actual = match cursor.accept(&event) {
            Ok(true) => "fresh",
            Ok(false) => "duplicate",
            Err(CursorError::Gap) => "gap",
            Err(CursorError::Invalid("replica stream epoch changed")) => "epoch",
            Err(CursorError::Invalid("replica policy revision regressed")) => "policy",
            Err(CursorError::Invalid(_)) => "invalid",
        };
        assert_eq!(actual, w[9], "{line}");
    }
}
#[test]
fn initialization_rejects_invalid_acknowledgments() {
    // Untrusted subscription replies cannot create a cursor at a false offset.
    for ack in
        [SubscriptionAck { epoch: [0; 16], ..ack() }, SubscriptionAck { sequence: 1, ..ack() }]
    {
        assert!(matches!(Cursor::new(&ack), Err(CursorError::Invalid(_))));
    }
    assert!(Cursor::new(&ack()).is_ok());
}
