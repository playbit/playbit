//! Check arbitrary storage headers and valid compact/full entry round trips.
#![no_main]

use kvdb::encoding::{self, EntVal, Metadata};
use kvdb::{Clock, hash};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(entry) = EntVal::decode(bytes) {
        assert_eq!(entry.flags & !encoding::ENT_FLAG_TOMBSTONE, 0);
        assert_eq!(entry.userval.len() + encoding::header_size(bytes), bytes.len());
        assert_eq!(entry.hash.is_some(), bytes.len() >= encoding::ENT_HEADER_FULL);
    }
    if let Ok(metadata) = Metadata::decode(bytes) {
        assert_eq!(Metadata::decode(&metadata.encode()).unwrap(), metadata);
        if metadata.syncable {
            assert!((2..=128).contains(&metadata.branch_factor));
        }
    }
    if let Some(clock) = Clock::decode(bytes) {
        assert_eq!(clock.encode().as_slice(), bytes);
    }

    // Bound valid construction independently of libFuzzer's input-size limit.
    let payload = &bytes[..bytes.len().min(4096)];
    let clock = Clock::now(1_800_000_000);
    let hash = hash::entry_hash(&[42; 32], b"key", 0, payload);
    let mut stored = Vec::new();
    encoding::encode_ent_val(&mut stored, &hash, 0, clock, payload);
    let entry = EntVal::decode(&stored).unwrap();
    assert_eq!(entry.userval, payload);
    assert_eq!(entry.clock, clock);
    assert_eq!(entry.hash_byte, hash[0]);
    for factor in [2, 4, 32, 128] {
        assert_eq!(
            encoding::is_boundary(hash[0], factor),
            u16::from(hash[0]) < 256 / u16::from(factor)
        );
    }
});
