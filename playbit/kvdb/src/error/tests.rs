#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use alloc::string::ToString;

#[test]
fn display_covers_every_variant() {
    let cases: &[(Error, &str)] = &[
        (Error::NotFound, "not found"),
        (Error::Exists, "database already exists"),
        (Error::InvalidArgument, "invalid argument"),
        (Error::InvalidKey, "invalid key"),
        (Error::KeySize, "key too large"),
        (Error::ValueSize, "value too large"),
        (Error::ReadOnly, "read-only transaction"),
        (Error::Busy, "write transaction already active"),
        (Error::Corrupt, "corrupt database"),
        (Error::Version, "incompatible database version"),
        (Error::Mode, "operation not supported in this database mode"),
        (Error::HashKeyMismatch, "replica hash keys differ"),
        (Error::BranchFactorMismatch, "replica branching factors differ"),
        (Error::Protocol, "malformed sync message"),
        (Error::Remote, "remote peer error"),
        (Error::Capacity, "capacity exceeded"),
        (Error::ClockExhausted, "logical clock exhausted"),
        (Error::Storage(42), "storage backend error 42"),
    ];
    for (err, want) in cases {
        assert_eq!(err.to_string(), *want);
    }
    // usable as a trait object (core::error::Error)
    let dyn_err: &dyn core::error::Error = &Error::NotFound;
    assert_eq!(dyn_err.to_string(), "not found");
}
