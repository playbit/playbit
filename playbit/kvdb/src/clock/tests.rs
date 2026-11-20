#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

const T0: u64 = EPOCH_UNIX_SECS + 1000;

#[test]
fn exhausted_clock_errors_without_overflow() {
    let maximum = Clock { time: u32::MAX, count: u32::MAX };
    assert_eq!(maximum.incr(0), Err(Error::ClockExhausted));
    assert_eq!(Clock::ZERO.recv(maximum, 0), Err(Error::ClockExhausted));
    assert_eq!(maximum.recv(Clock::ZERO, u64::MAX), Err(Error::ClockExhausted));
    let before = Clock { time: u32::MAX, count: u32::MAX - 1 };
    assert_eq!(before.incr(0).unwrap(), maximum);
}

#[test]
fn ordering_matches_encodings() {
    let a = Clock { time: 5, count: 9 };
    let b = Clock { time: 6, count: 0 };
    let c = Clock { time: 6, count: 1 };
    assert!(a < b && b < c);
    assert!(a.as_u64() < b.as_u64() && b.as_u64() < c.as_u64());
    assert!(a.encode() < b.encode() && b.encode() < c.encode());
    assert_eq!(Clock::decode(&c.encode()), Some(c));
    assert_eq!(Clock::from_u64(a.as_u64()), a);
}

#[test]
fn never_zero() {
    assert!(!Clock::now(0).is_zero());
    assert!(!Clock::now(EPOCH_UNIX_SECS).is_zero());
    assert!(!Clock::ZERO.incr(0).unwrap().is_zero());
}

#[test]
fn incr_is_strictly_monotonic() {
    let mut c = Clock::now(T0);
    for wall in [T0, T0 - 500, 0, T0 + 5, T0 + 5] {
        let next = c.incr(wall).unwrap();
        assert!(next > c, "{next:?} <= {c:?}");
        c = next;
    }
}

#[test]
fn incr_count_overflow_advances_time() {
    let c = Clock { time: 100, count: u32::MAX };
    assert_eq!(c.incr(0).unwrap(), Clock { time: 101, count: 0 });
}

#[test]
fn recv_exceeds_both_sides() {
    let cases = [
        (Clock { time: 10, count: 3 }, Clock { time: 10, count: 7 }, 0),
        (Clock { time: 10, count: 3 }, Clock { time: 2, count: 9 }, 0),
        (Clock { time: 2, count: 9 }, Clock { time: 10, count: 3 }, 0),
        (Clock { time: 2, count: 1 }, Clock { time: 3, count: 1 }, T0),
        (Clock { time: 10, count: u32::MAX }, Clock { time: 10, count: 5 }, 0),
    ];
    for (local, remote, wall) in cases {
        let r = local.recv(remote, wall).unwrap();
        assert!(r > local && r > remote, "{r:?} vs {local:?}/{remote:?}");
    }
}
