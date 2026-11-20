#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::clock::Clock;
use crate::encoding::encode_ent_val;
use alloc::vec::Vec;

const KEY: [u8; 32] = [0x55; 32];

fn ent(buf: &mut Vec<u8>, key: &[u8], flags: u8, userval: &[u8], clock: Clock) -> [u8; 32] {
    let h = entry_hash(&KEY, key, flags, userval);
    encode_ent_val(buf, &h, flags, clock, userval);
    h
}

#[test]
fn entry_hash_distinguishes_key_value_split_and_tombstones() {
    assert_ne!(entry_hash(&KEY, b"ab", 0, b"c"), entry_hash(&KEY, b"a", 0, b"bc"));
    assert_ne!(entry_hash(&KEY, b"a", 0, b""), entry_hash(&KEY, b"a", ENT_FLAG_TOMBSTONE, b""));
    assert_ne!(entry_hash(&KEY, b"a", 0, b"x"), entry_hash(&[1u8; 32], b"a", 0, b"x"));
}

#[test]
fn branch_hash_covers_compact_keys_and_clocks() {
    // Same uservals under different keys must produce different branch
    // hashes even for compact entries (a weakness of the C design).
    let mut a = Vec::new();
    let mut b = Vec::new();
    ent(&mut a, b"k1", 0, b"v", Clock { time: 1, count: 0 });
    ent(&mut b, b"k2", 0, b"v", Clock { time: 9, count: 9 });

    let mut h1 = BranchHasher::new(&KEY);
    h1.add_leaf(b"k1", &EntVal::decode(&a).unwrap());
    let mut h2 = BranchHasher::new(&KEY);
    h2.add_leaf(b"k2", &EntVal::decode(&b).unwrap());
    assert_ne!(h1.finish(), h2.finish());

    // Same key+value at different clocks: different branch hash.
    let mut c = Vec::new();
    ent(&mut c, b"k1", 0, b"v", Clock { time: 77, count: 3 });
    let mut h3 = BranchHasher::new(&KEY);
    h3.add_leaf(b"k1", &EntVal::decode(&c).unwrap());
    let mut h4 = BranchHasher::new(&KEY);
    h4.add_leaf(b"k1", &EntVal::decode(&a).unwrap());
    assert_ne!(h3.finish(), h4.finish());
}

#[test]
fn full_leaf_clock_participates_alongside_stored_hash() {
    let mut a = Vec::new();
    let mut b = Vec::new();
    let big: Vec<u8> = (0..100).collect();
    ent(&mut a, b"k", 0, &big, Clock { time: 1, count: 0 });
    ent(&mut b, b"k", 0, &big, Clock { time: 2, count: 0 });
    let mut h1 = BranchHasher::new(&KEY);
    h1.add_leaf(b"k", &EntVal::decode(&a).unwrap());
    let mut h2 = BranchHasher::new(&KEY);
    h2.add_leaf(b"k", &EntVal::decode(&b).unwrap());
    assert_ne!(h1.finish(), h2.finish());
}
