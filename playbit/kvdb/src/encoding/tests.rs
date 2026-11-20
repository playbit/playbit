#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use alloc::vec::Vec;

#[test]
fn metadata_version_is_independent_of_database_kind() {
    for syncable in [false, true] {
        let meta = Metadata {
            syncable,
            branch_factor: 4,
            replica_id: 1,
            clock: Clock::ZERO,
            hash_key: [7; 32],
        };
        let mut encoded = meta.encode();
        assert_eq!(encoded[4], 1);
        assert_eq!(encoded[5], u8::from(syncable));
        assert_eq!(Metadata::decode(&encoded).unwrap(), meta);
        for version in [0, 2, u8::MAX] {
            encoded[4] = version;
            assert!(matches!(Metadata::decode(&encoded), Err(Error::Version)));
        }
        for (offset, value) in [(5, 2), (5, 3), (5, u8::MAX), (7, 1)] {
            let mut encoded = meta.encode();
            encoded[offset] = value;
            assert!(matches!(Metadata::decode(&encoded), Err(Error::Version)));
        }
    }
}

#[test]
fn ent_val_roundtrip_compact_and_full() {
    let hash = [0xabu8; 32];
    let clock = Clock { time: 7, count: 3 };
    for userval_len in [0usize, 1, 29, 30, 31, 32, 100] {
        let userval: Vec<u8> = (0..userval_len as u8).collect();
        let mut buf = Vec::new();
        encode_ent_val(&mut buf, &hash, 0, clock, &userval);
        let compact = userval_len <= ENT_USERVAL_COMPACT_MAX;
        assert_eq!(buf.len() < ENT_HEADER_FULL, compact, "len {}", buf.len());
        let ent = EntVal::decode(&buf).unwrap();
        assert_eq!(ent.hash_byte, hash[0]);
        assert_eq!(ent.hash.is_some(), !compact);
        assert_eq!(ent.clock, clock);
        assert_eq!(ent.userval, &userval[..]);
        assert!(!ent.is_tombstone());
    }
}

#[test]
fn tombstone_is_len_10() {
    let hash = [3u8; 32];
    let mut buf = Vec::new();
    encode_ent_val(&mut buf, &hash, ENT_FLAG_TOMBSTONE, Clock { time: 1, count: 0 }, &[]);
    assert_eq!(buf.len(), ENT_HEADER_COMPACT);
    assert!(EntVal::decode(&buf).unwrap().is_tombstone());
}

#[test]
fn content_eq_ignores_clock() {
    let hash = [9u8; 32];
    let (mut a, mut b) = (Vec::new(), Vec::new());
    encode_ent_val(&mut a, &hash, 0, Clock { time: 1, count: 0 }, b"hello");
    encode_ent_val(&mut b, &hash, 0, Clock { time: 2, count: 5 }, b"hello");
    assert!(EntVal::decode(&a).unwrap().content_eq(&EntVal::decode(&b).unwrap()));
    encode_ent_val(&mut b, &hash, 0, Clock { time: 1, count: 0 }, b"world");
    assert!(!EntVal::decode(&a).unwrap().content_eq(&EntVal::decode(&b).unwrap()));
}

#[test]
fn decode_rejects_malformed_values() {
    // shorter than the compact header
    assert_eq!(EntVal::decode(&[0u8; 9]), Err(Error::Corrupt));
    assert_eq!(EntVal::decode(&[]), Err(Error::Corrupt));
    // unknown flag bits in a compact header
    let mut bad = [0u8; ENT_HEADER_COMPACT];
    bad[1] = 0x80;
    assert_eq!(EntVal::decode(&bad), Err(Error::Corrupt));
    // Full headers must reject the same unknown bits as compact headers.
    let mut bad = [0u8; ENT_HEADER_FULL + 31];
    bad[32] = 0x80;
    assert_eq!(EntVal::decode(&bad), Err(Error::Corrupt));
}

#[test]
fn content_eq_full_layout_and_mixed_layouts() {
    let big_a: Vec<u8> = (0..100u8).collect();
    let big_b: Vec<u8> = (100..200u8).collect();
    let clock = Clock { time: 1, count: 0 };
    let enc = |hash: &[u8; 32], userval: &[u8]| {
        let mut buf = Vec::new();
        encode_ent_val(&mut buf, hash, 0, clock, userval);
        buf
    };
    // full vs full: hashes decide
    let fa = enc(&[1; 32], &big_a);
    let fa2 = enc(&[1; 32], &big_a);
    let fb = enc(&[2; 32], &big_b);
    assert!(EntVal::decode(&fa).unwrap().content_eq(&EntVal::decode(&fa2).unwrap()));
    assert!(!EntVal::decode(&fa).unwrap().content_eq(&EntVal::decode(&fb).unwrap()));
    // full vs compact: never equal (layout is a function of length)
    let small = enc(&[1; 32], b"tiny");
    assert!(!EntVal::decode(&fa).unwrap().content_eq(&EntVal::decode(&small).unwrap()));
    assert!(!EntVal::decode(&small).unwrap().content_eq(&EntVal::decode(&fa).unwrap()));
}

#[test]
fn metadata_decode_rejects_corruption() {
    let good = Metadata {
        syncable: true,
        branch_factor: 32,
        replica_id: 7,
        clock: Clock { time: 1, count: 0 },
        hash_key: [0; 32],
    }
    .encode();
    // wrong length
    assert_eq!(Metadata::decode(&good[..META_SIZE - 1]), Err(Error::Corrupt));
    // wrong magic
    let mut bad = good;
    bad[0] = b'x';
    assert_eq!(Metadata::decode(&bad), Err(Error::Corrupt));
    // syncable with out-of-range branch factor
    let mut bad = good;
    bad[6] = 1;
    assert_eq!(Metadata::decode(&bad), Err(Error::Corrupt));
}

#[test]
fn metadata_roundtrip() {
    let m = Metadata {
        syncable: true,
        branch_factor: 32,
        replica_id: 0x1122_3344_5566_7788,
        clock: Clock { time: 123, count: 45 },
        hash_key: [7u8; 32],
    };
    assert_eq!(Metadata::decode(&m.encode()).unwrap(), m);
    let mut bad = m.encode();
    bad[4] = 99; // unknown version
    assert_eq!(Metadata::decode(&bad), Err(Error::Version));
}

#[test]
fn boundary_threshold() {
    assert!(is_boundary(7, 32));
    assert!(!is_boundary(8, 32));
    assert!(is_boundary(63, 4));
    assert!(!is_boundary(64, 4));
    assert!(is_boundary(1, 128) && !is_boundary(2, 128));
}

#[test]
fn only_a_branch_factor_dividing_256_gets_the_fan_out_it_asks_for() {
    // How many of the 256 byte values start a branch. One byte decides,
    // so this count is what the fan-out is actually made of.
    let boundaries =
        |branch_factor: u8| (0..=u8::MAX).filter(|byte| is_boundary(*byte, branch_factor)).count();
    for branch_factor in [2u8, 4, 8, 16, 32, 64, 128] {
        assert_eq!(
            boundaries(branch_factor) * branch_factor as usize,
            256,
            "a divisor of 256 should get the fan-out it asked for"
        );
    }
    // Every other value is coarser than asked, never finer, and the gap
    // widens as the count of boundary values shrinks.
    for branch_factor in 2..=128u8 {
        let count = boundaries(branch_factor);
        assert!(count >= 1);
        assert!(
            count * branch_factor as usize <= 256,
            "fan-out never comes out finer than the branch factor"
        );
    }
    assert_eq!(boundaries(33), 7, "33 averages 256/7, about 36");
    for branch_factor in 86..=128u8 {
        assert_eq!(boundaries(branch_factor), 2, "every value from 86 up averages 128");
    }
}
