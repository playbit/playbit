#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::storage::Storage;
use crate::storage::mem::MemStorage;
use alloc::collections::BTreeMap;

const HASH_KEY: [u8; 32] = [0x42; 32];

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Dump every stored entry (storage key, value), sorted.
fn dump<T: StorageTxn>(txn: &T) -> Vec<Node> {
    let mut out = Vec::new();
    let mut from: Vec<u8> = Vec::new();
    let mut excl = false;
    while let Some((k, v)) = txn.next_from(&from, excl).unwrap() {
        out.push((k.to_vec(), v.to_vec()));
        from = k.to_vec();
        excl = true;
    }
    out
}

/// Assert leaves match the model and branch levels equal a from-scratch
/// rebuild of the same leaves (the canonical-structure invariant).
fn verify<T: StorageTxn>(txn: &mut T, model: &BTreeMap<Vec<u8>, Option<Vec<u8>>>, bf: u8) {
    Tree { txn, hash_key: HASH_KEY, branch_factor: bf }.verify().unwrap();
    let all = dump(txn);
    // 1. leaves == model (tombstones are Some entries with None value)
    let leaves: Vec<&Node> = all.iter().filter(|(k, _)| node_at_level(k, 0)).collect();
    assert_eq!(leaves.len(), model.len(), "leaf count");
    for ((k, v), (mk, mv)) in leaves.iter().zip(model.iter()) {
        assert_eq!(&k[1..], &mk[..], "leaf key");
        let ent = EntVal::decode(v).unwrap();
        match mv {
            None => assert!(ent.is_tombstone(), "expected tombstone at {mk:?}"),
            Some(want) => {
                assert!(!ent.is_tombstone());
                assert_eq!(ent.userval, &want[..], "leaf value at {mk:?}");
            }
        }
    }
    assert_eq!(
        all,
        canonical(leaves.iter().map(|node| (*node).clone()).collect(), bf),
        "tree differs from independent whole-level construction"
    );
    // 2. branch levels equal an independent rebuild from the same leaves
    let s2 = MemStorage::new();
    let mut t2 = s2.begin(true).unwrap();
    for (k, v) in &all {
        if node_at_level(k, 0) {
            t2.put(k, v).unwrap();
        }
    }
    Tree { txn: &mut t2, hash_key: HASH_KEY, branch_factor: bf }.build_all().unwrap();
    let branches = |d: &[Node]| -> Vec<Node> {
        d.iter()
            .filter(|(k, _)| !k.is_empty() && (1..META_KEY_BYTE).contains(&k[0]))
            .cloned()
            .collect()
    };
    let got = branches(&all);
    let want = branches(&dump(&t2));
    assert_eq!(got, want, "branch structure differs from canonical rebuild");
}

fn rand_key(rng: &mut u64) -> Vec<u8> {
    let len = 1 + (xorshift(rng) % 6) as usize;
    (0..len).map(|_| b'a' + (xorshift(rng) % 6) as u8).collect()
}

#[test]
fn randomized_matches_model_and_rebuild() {
    for &bf in &[2u8, 4, 32] {
        for seed in 1..=3u64 {
            let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let storage = MemStorage::new();
            let mut txn = storage.begin(true).unwrap();
            let mut model: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
            for i in 0..250u32 {
                let key = rand_key(&mut rng);
                let clock = Clock { time: i + 1, count: 0 };
                let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: bf };
                match xorshift(&mut rng) % 5 {
                    0..=2 => {
                        // put (values straddle the compact/full threshold)
                        let vlen = (xorshift(&mut rng) % 64) as usize;
                        let val: Vec<u8> = (0..vlen).map(|_| xorshift(&mut rng) as u8).collect();
                        tree.put_leaf(&key, &val, 0, clock).unwrap();
                        model.insert(key, Some(val));
                    }
                    3 => {
                        tree.put_leaf(&key, &[], ENT_FLAG_TOMBSTONE, clock).unwrap();
                        model.insert(key, None);
                    }
                    _ => {
                        tree.remove_leaf(&key).unwrap();
                        model.remove(&key);
                    }
                }
                if i % 25 == 0 {
                    verify(&mut txn, &model, bf);
                }
            }
            verify(&mut txn, &model, bf);
            // drain everything via remove and check the tree vanishes
            let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
            for key in keys {
                let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: bf };
                assert!(tree.remove_leaf(&key).unwrap());
                model.remove(&key);
            }
            verify(&mut txn, &model, bf);
            assert_eq!(root(&txn).unwrap(), None);
            assert!(dump(&txn).is_empty());
        }
    }
}

/// Enumerate every single-key transition from a finite set of complete tree
/// states. The starting and expected trees come from the independent whole-level
/// constructor, so an incremental bug cannot be hidden by replaying its history.
#[test]
fn exhaustive_small_tree_transitions_preserve_canonical_structure() {
    let keys: [&[u8]; 3] = [b"a", b"a\0", b"b"];
    // Missing, tombstone, empty, the compact/full boundary, and a newer version
    // of identical full-layout content. Fixed versions include true no-op writes.
    let states = [
        None,
        Some((vec![], ENT_FLAG_TOMBSTONE, Clock { time: 1, count: 0 })),
        Some((vec![], 0, Clock { time: 1, count: 0 })),
        Some((vec![3; 30], 0, Clock { time: 1, count: 0 })),
        Some((vec![3; 31], 0, Clock { time: 1, count: 0 })),
        Some((vec![3; 31], 0, Clock { time: 2, count: 0 })),
    ];
    let leaves = |state: [usize; 3]| -> Vec<Node> {
        keys.iter()
            .zip(state)
            .filter_map(|(key, state)| {
                let (value, flags, clock) = states[state].as_ref()?;
                let hash = crate::hash::entry_hash(&HASH_KEY, key, *flags, value);
                let mut stored = Vec::new();
                encoding::encode_ent_val(&mut stored, &hash, *flags, *clock, value);
                Some((leaf_skey(key), stored))
            })
            .collect()
    };
    for bf in [2, 3, 32, 128] {
        for initial in 0..states.len().pow(3) {
            let before = [initial % 6, initial / 6 % 6, initial / 36];
            let canonical_before = canonical(leaves(before), bf);
            for key in 0..keys.len() {
                for (next, next_state) in states.iter().enumerate() {
                    let storage = MemStorage::new();
                    let mut txn = storage.begin(true).unwrap();
                    for (key, value) in &canonical_before {
                        txn.put(key, value).unwrap();
                    }
                    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: bf };
                    let changed = match next_state {
                        None => tree.remove_leaf(keys[key]).unwrap(),
                        Some((value, flags, clock)) => {
                            tree.put_leaf(keys[key], value, *flags, *clock).unwrap()
                        }
                    };
                    assert_eq!(
                        changed,
                        before[key] != next,
                        "bf={bf} before={before:?} key={key} next={next}"
                    );
                    tree.verify().unwrap();
                    let mut after = before;
                    after[key] = next;
                    assert_eq!(
                        dump(&txn),
                        canonical(leaves(after), bf),
                        "bf={bf} before={before:?} key={key} next={next}"
                    );
                }
            }
        }
    }
}

#[test]
fn tree_operations_match_regression_cases() {
    fn number<'a>(words: &mut impl Iterator<Item = &'a str>) -> usize {
        words.next().unwrap().parse().unwrap()
    }
    fn parent_key(rank: usize) -> Vec<u8> {
        match rank {
            0 => vec![1],
            7 => vec![2], // Exclusive end of the entire parent level.
            _ => vec![1, rank as u8],
        }
    }
    fn parents<'a>(words: &mut impl Iterator<Item = &'a str>) -> Vec<Node> {
        (0..number(words))
            .map(|_| (parent_key(number(words)), vec![number(words) as u8; 32]))
            .collect()
    }
    let mut counts = [0; 3];
    for line in include_str!("../../tests/fixtures/kvdb-tree.txt").lines() {
        let mut words = line.split_whitespace();
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        match words.next().unwrap() {
            "chunks" => {
                counts[0] += 1;
                let n = number(&mut words);
                let mask = number(&mut words);
                let sizes: Vec<_> = (0..number(&mut words)).map(|_| number(&mut words)).collect();
                assert_eq!(sizes.iter().sum::<usize>(), n);
                // Boundary bytes abstract BLAKE3 output. The real branch hasher
                // still checks exactly which child keys, values and clocks are used.
                let leaves: Vec<Node> = (0..n)
                    .map(|i| {
                        let hash = [if mask & (1 << i) != 0 { 0 } else { 255 }; 32];
                        let mut stored = Vec::new();
                        encoding::encode_ent_val(
                            &mut stored,
                            &hash,
                            0,
                            Clock { time: 1, count: 0 },
                            &[i as u8],
                        );
                        (vec![0, i as u8 + 1], stored)
                    })
                    .collect();
                for (key, value) in &leaves {
                    txn.put(key, value).unwrap();
                }
                let mut expected = Vec::new();
                let mut start = 0;
                let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 2 };
                for size in sizes {
                    assert!(size > 0);
                    let mut hash = BranchHasher::new(&HASH_KEY);
                    for (key, stored) in &leaves[start..start + size] {
                        hash.add_leaf(&key[1..], &EntVal::decode(stored).unwrap());
                    }
                    let hash = hash.finish();
                    let cursor = if start == 0 { vec![0] } else { leaves[start].0.clone() };
                    let next = leaves.get(start + size).map(|(key, _)| key.clone());
                    assert_eq!(tree.verify_chunk(0, &cursor).unwrap(), (hash, next), "{line}");
                    let parent = if start == 0 { vec![1] } else { vec![1, start as u8 + 1] };
                    expected.push((parent, hash.to_vec()));
                    start += size;
                }
                tree.build_all().unwrap();
                let actual: Vec<_> =
                    dump(&txn).into_iter().filter(|(key, _)| node_at_level(key, 1)).collect();
                assert_eq!(actual, expected, "{line}");
            }
            "repairs" => {
                let steps = number(&mut words);
                counts[if steps == 1 { 1 } else { 2 }] += 1;
                let mut cursor = parent_key(number(&mut words));
                let mut exclusive = words.next().unwrap().parse::<bool>().unwrap();
                let mut before = parents(&mut words);
                for (key, value) in &before {
                    txn.put(key, value).unwrap();
                }
                txn.put(&[0, 7], &[7; 10]).unwrap();
                txn.put(&[2], &[9; 32]).unwrap();
                for _ in 0..steps {
                    let endpoint = parent_key(number(&mut words));
                    let hash = words.next().unwrap();
                    let hash =
                        if hash == "-" { None } else { Some([hash.parse::<u8>().unwrap(); 32]) };
                    let expected_cursor = parent_key(number(&mut words));
                    let after = parents(&mut words);
                    let mut changed = None;
                    Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
                        .merge_parent(
                            1,
                            &mut cursor,
                            exclusive,
                            &endpoint,
                            hash.as_ref(),
                            &mut changed,
                        )
                        .unwrap();
                    let actual: Vec<_> =
                        dump(&txn).into_iter().filter(|(key, _)| node_at_level(key, 1)).collect();
                    assert_eq!(actual, after, "{line}");
                    assert_eq!(cursor, expected_cursor, "cursor: {line}");
                    assert_eq!(txn.get(&[0, 7]).unwrap().unwrap(), &[7; 10]);
                    assert_eq!(txn.get(&[2]).unwrap().unwrap(), &[9; 32]);
                    let before_map: BTreeMap<_, _> = before.into_iter().collect();
                    let after_map: BTreeMap<_, _> = after.iter().cloned().collect();
                    let changed_keys: alloc::collections::BTreeSet<_> = before_map
                        .keys()
                        .chain(after_map.keys())
                        .filter(|key| before_map.get(*key) != after_map.get(*key))
                        .map(|key| key[1..].to_vec())
                        .collect();
                    let expected_bounds = changed_keys
                        .first()
                        .zip(changed_keys.last())
                        .map(|(lo, hi)| (lo.clone(), hi.clone()));
                    assert_eq!(changed, expected_bounds, "{line}");
                    before = after;
                    exclusive = true;
                }
            }
            other => panic!("unknown regression case {other}"),
        }
        assert!(words.next().is_none(), "unconsumed vector fields: {line}");
    }
    assert_eq!(counts, [511, 5152, 1760]);
}

#[test]
fn structure_is_insertion_order_independent() {
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..120u32)
        .map(|i| {
            let mut rng = (i as u64 + 7).wrapping_mul(0x2545_f491_4f6c_dd1d) | 1;
            let k = rand_key(&mut rng);
            let vlen = (xorshift(&mut rng) % 50) as usize;
            (k, (0..vlen).map(|_| xorshift(&mut rng) as u8).collect())
        })
        .collect::<BTreeMap<_, _>>() // dedup keys so every order ends in the same state
        .into_iter()
        .collect();

    let build = |order: &[usize]| -> (Vec<Node>, Option<(u8, [u8; 32])>) {
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        for &i in order {
            let (k, v) = &entries[i];
            let clock = Clock { time: 1000 + i as u32, count: 0 }; // same entry versions in either order
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
                .put_leaf(k, v, 0, clock)
                .unwrap();
        }
        let branches = dump(&txn)
            .into_iter()
            .filter(|(k, _)| !k.is_empty() && (1..META_KEY_BYTE).contains(&k[0]))
            .collect();
        (branches, root(&txn).unwrap())
    };

    let fwd: Vec<usize> = (0..entries.len()).collect();
    let rev: Vec<usize> = (0..entries.len()).rev().collect();
    let (b1, r1) = build(&fwd);
    let (b2, r2) = build(&rev);
    assert_eq!(r1, r2, "root hash must not depend on insertion order");
    assert_eq!(b1, b2, "branch structure must not depend on insertion order");
    assert!(r1.is_some());
}

#[test]
fn incremental_matches_rebuild_after_every_insert() {
    // Verifies the canonical-structure invariant after each single
    // insertion, in both ascending and descending key order. Catches
    // window bugs (e.g. the old-first-node headship change) that
    // coarser checks miss; prints a shrunk repro on failure.
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..120u32)
        .map(|i| {
            let mut rng = (i as u64 + 7).wrapping_mul(0x2545_f491_4f6c_dd1d) | 1;
            let k = rand_key(&mut rng);
            let vlen = (xorshift(&mut rng) % 50) as usize;
            (k, (0..vlen).map(|_| xorshift(&mut rng) as u8).collect())
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    let branches = |txn: &crate::storage::mem::MemTxn<'_>| -> Vec<Node> {
        dump(txn)
            .into_iter()
            .filter(|(k, _)| !k.is_empty() && (1..META_KEY_BYTE).contains(&k[0]))
            .collect()
    };
    for order in [false, true] {
        let idx: Vec<usize> =
            if order { (0..entries.len()).rev().collect() } else { (0..entries.len()).collect() };
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        for (n, &i) in idx.iter().enumerate() {
            let (k, v) = &entries[i];
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
                .put_leaf(k, v, 0, Clock { time: 1000 + n as u32, count: 0 })
                .unwrap();
            // rebuild copy
            let s2 = MemStorage::new();
            let mut t2 = s2.begin(true).unwrap();
            for (kk, vv) in dump(&txn) {
                if node_at_level(&kk, 0) {
                    t2.put(&kk, &vv).unwrap();
                }
            }
            Tree { txn: &mut t2, hash_key: HASH_KEY, branch_factor: 4 }.build_all().unwrap();
            let got = branches(&txn);
            let want = branches(&t2);
            assert!(
                got == want,
                "DIVERGED order={order} after n={n} key={:?} got={got:02x?} want={want:02x?}",
                std::string::String::from_utf8_lossy(k)
            );
        }
    }
}

#[test]
fn single_leaf_and_empty_edges() {
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 32 };
    assert_eq!(root(tree.txn).unwrap(), None);
    tree.put_leaf(b"k", b"v", 0, Clock { time: 1, count: 0 }).unwrap();
    let r = root(&txn).unwrap().expect("root after first leaf");
    assert_eq!(r.0, 1, "single-leaf root is the level-1 anchor");
    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 32 };
    assert!(tree.remove_leaf(b"k").unwrap());
    assert_eq!(root(&txn).unwrap(), None);
    assert!(dump(&txn).is_empty());
}

#[test]
fn root_on_raw_storage_edges() {
    // Empty storage: no root.
    let storage = MemStorage::new();
    let txn = storage.begin(false).unwrap();
    assert_eq!(root(&txn).unwrap(), None);
    drop(txn);
    // A leaf with no branch levels above it (mid-write state or
    // corruption) reads as "no root" rather than a bogus root.
    let mut txn = storage.begin(true).unwrap();
    txn.put(b"\x00k", b"0123456789").unwrap();
    assert_eq!(root(&txn).unwrap(), None);
    // A branch whose value is not a 32-byte hash is corruption.
    txn.put(&[1u8], b"short").unwrap();
    assert_eq!(root(&txn), Err(Error::Corrupt));
}

#[test]
fn put_identical_version_is_noop() {
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 32 };
    assert!(tree.put_leaf(b"a", b"1", 0, Clock { time: 1, count: 0 }).unwrap());
    // Same version is a no-op, but a later clock must be retained.
    assert!(!tree.put_leaf(b"a", b"1", 0, Clock { time: 1, count: 0 }).unwrap());
    assert!(tree.put_leaf(b"a", b"1", 0, Clock { time: 9, count: 0 }).unwrap());
    let v = tree.get_leaf_raw(b"a").unwrap().unwrap();
    assert_eq!(EntVal::decode(&v).unwrap().clock, Clock { time: 9, count: 0 });
    // different value: change
    assert!(tree.put_leaf(b"a", b"2", 0, Clock { time: 9, count: 0 }).unwrap());
}
/// Independent whole-level canonical construction, without Tree::rechunk.
fn canonical(mut nodes: Vec<Node>, bf: u8) -> Vec<Node> {
    let mut all = nodes.clone();
    let mut level = 0u8;
    while !nodes.is_empty() && (level == 0 || nodes.len() > 1) {
        let mut parents = Vec::new();
        let mut start = 0;
        while start < nodes.len() {
            let mut end = start + 1;
            while end < nodes.len() && !encoding::is_boundary(nodes[end].1[0], bf) {
                end += 1;
            }
            let mut hash = BranchHasher::new(&HASH_KEY);
            for (key, value) in &nodes[start..end] {
                if level == 0 {
                    hash.add_leaf(&key[1..], &EntVal::decode(value).unwrap());
                } else {
                    hash.add_branch(value.as_slice().try_into().unwrap());
                }
            }
            let mut key = vec![level + 1];
            if start != 0 {
                key.extend_from_slice(&nodes[start].0[1..]);
            }
            parents.push((key, hash.finish().to_vec()));
            start = end;
        }
        all.extend(parents.iter().cloned());
        nodes = parents;
        level += 1;
    }
    all.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    all
}

struct Counted<T> {
    inner: T,
    visits: core::cell::Cell<usize>,
    visited: core::cell::Cell<usize>,
    point_reads: core::cell::Cell<usize>,
    max_read: core::cell::Cell<usize>,
    fail_after: Option<usize>,
}
impl<T> Counted<T> {
    fn before_mutation(&mut self) -> Result<()> {
        if let Some(left) = &mut self.fail_after {
            if *left == 0 {
                return Err(Error::Storage(0xfeed));
            }
            *left -= 1;
        }
        Ok(())
    }
}
impl<T: StorageTxn> StorageTxn for Counted<T> {
    type ValueReader = T::ValueReader;
    type Guard<'g>
        = T::Guard<'g>
    where
        Self: 'g;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>> {
        self.inner.snapshot_value(key)
    }
    fn get<'g>(&'g self, key: &[u8]) -> Result<Option<Self::Guard<'g>>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.get(key)
    }
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.value_len(key)
    }
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.read_value_at(key, offset, out)
    }
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.next_key(key, exclusive)
    }
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.prev_key(key, exclusive)
    }
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        struct CountRead<'a>(&'a dyn ValueRead, &'a core::cell::Cell<usize>);
        impl ValueRead for CountRead<'_> {
            fn len(&self) -> usize {
                self.0.len()
            }
            fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
                self.1.set(self.1.get().max(out.len()));
                self.0.read_at(offset, out)
            }
        }
        self.visits.set(self.visits.get() + 1);
        self.inner.visit(key, exclusive, reverse, &mut |k, v| {
            self.visited.set(self.visited.get() + 1);
            visitor(k, &CountRead(v, &self.max_read))
        })
    }
    fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> Result<()> {
        self.before_mutation()?;
        self.inner.put_from(key, value)
    }
    fn next_from<'g>(
        &'g self,
        key: &[u8],
        exclusive: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.next_from(key, exclusive)
    }
    fn prev_from<'g>(
        &'g self,
        key: &[u8],
        exclusive: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
        self.point_reads.set(self.point_reads.get() + 1);
        self.inner.prev_from(key, exclusive)
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.before_mutation()?;
        self.inner.put(key, value)
    }
    fn del(&mut self, key: &[u8]) -> Result<bool> {
        self.before_mutation()?;
        self.inner.del(key)
    }
    fn commit(self) -> Result<()> {
        self.inner.commit()
    }
}

// Both fixtures are valid keyed trees. Deliberately choose leaf hashes
// so the expected fanout cannot masquerade as a worst-case chunk bound.
fn boundary_fixture(count: u32, bf: u8, boundary: bool) -> Vec<Node> {
    (0..count)
        .map(|n| {
            let mut key = vec![0x55; crate::KEY_SIZE_MAX];
            key[..4].copy_from_slice(&n.to_be_bytes());
            let mut value = [0; 32];
            let hash = (0u32..)
                .find_map(|nonce| {
                    value[..4].copy_from_slice(&nonce.to_le_bytes());
                    let hash = crate::hash::entry_hash(&HASH_KEY, &key, 0, &value);
                    (encoding::is_boundary(hash[0], bf) == boundary).then_some(hash)
                })
                .unwrap();
            let mut encoded = Vec::new();
            encoding::encode_ent_val(
                &mut encoded,
                &hash,
                0,
                Clock { time: n + 1, count: 0 },
                &value,
            );
            (leaf_skey(&key), encoded)
        })
        .collect()
}

#[test]
fn one_reverse_visit_matches_independent_chunk_head_selection() {
    for bf in [2, 128] {
        let boundaries = boundary_fixture(64, bf, true);
        let non_boundaries = boundary_fixture(64, bf, false);
        let leaves = boundaries
            .into_iter()
            .zip(non_boundaries)
            .enumerate()
            .map(|(n, (yes, no))| if n % 3 == 1 { yes } else { no })
            .collect();
        let all = canonical(leaves, bf);
        let storage = MemStorage::new();
        let mut txn = Counted {
            inner: storage.begin(true).unwrap(),
            visits: 0.into(),
            visited: 0.into(),
            point_reads: 0.into(),
            max_read: 0.into(),
            fail_after: None,
        };
        for (key, value) in &all {
            txn.inner.put(key, value).unwrap();
        }
        for level in 0..=all.last().unwrap().0[0] {
            let nodes: Vec<_> = all.iter().filter(|(key, _)| key[0] == level).collect();
            let heads: Vec<_> = nodes
                .iter()
                .enumerate()
                .filter(|(n, (_, value))| *n == 0 || encoding::is_boundary(value[0], bf))
                .map(|(_, (key, _))| key.as_slice())
                .collect();
            let mut queries = vec![vec![], vec![0xff; crate::KEY_SIZE_MAX]];
            for (key, _) in &nodes {
                queries.push(key[1..].to_vec());
                queries.push(key[1..key.len().min(5)].to_vec());
            }
            for query in queries {
                let covering = heads.iter().rposition(|head| head[1..] <= query[..]).unwrap_or(0);
                for covering_only in [false, true] {
                    txn.visits.set(0);
                    let got = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: bf }
                        .window_head(level, &query, &nodes[0].0, covering_only)
                        .unwrap();
                    let expected =
                        if covering_only { covering } else { covering.saturating_sub(1) };
                    assert_eq!(got, heads[expected]);
                    assert_eq!(txn.visits.get(), 1, "both heads must share one retained range");
                }
            }
        }
        assert!(txn.max_read.get() <= encoding::ENT_HEADER_FULL);
        assert_eq!(dump(&txn.inner), all);
    }
}

#[test]
fn combined_reverse_visit_reports_corruption_between_covering_and_previous_heads() {
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    txn.put(&[1], &[0; 32]).unwrap();
    txn.put(&[1, b'b'], &[0; encoding::ENT_HEADER_FULL + 1]).unwrap();
    txn.put(&[1, b'c'], &[0; 32]).unwrap();
    let before = dump(&txn);
    let tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 };
    assert_eq!(tree.window_head(1, b"c", &[1], true).unwrap(), [1, b'c']);
    assert_eq!(tree.window_head(1, b"c", &[1], false), Err(Error::Corrupt));
    assert_eq!(dump(&txn), before);
}

#[test]
fn exhausted_level_skips_only_proven_empty_prune_seeks() {
    for has_root in [false, true] {
        for metadata in [false, true] {
            let storage = MemStorage::new();
            let mut txn = Counted {
                inner: storage.begin(true).unwrap(),
                visits: 0.into(),
                visited: 0.into(),
                point_reads: 0.into(),
                max_read: 0.into(),
                fail_after: Some(0),
            };
            if has_root {
                for (key, value) in canonical(boundary_fixture(1, 4, true), 4) {
                    txn.inner.put(&key, &value).unwrap();
                }
            }
            if metadata {
                txn.inner.put(&[META_KEY_BYTE], b"metadata").unwrap();
            }
            let before = dump(&txn.inner);
            assert_eq!(
                Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
                    .rechunk(u8::from(has_root), &[], &[], false)
                    .unwrap(),
                None
            );
            assert_eq!(
                txn.visits.get(),
                1,
                "EOF/metadata already proves there are no higher branches"
            );
            assert_eq!(dump(&txn.inner), before);
        }
    }
    // A real higher branch must still be pruned, including propagation of
    // backend errors and rollback of the containing transaction.
    for fail in [false, true] {
        let storage = MemStorage::new();
        let baseline = canonical(boundary_fixture(1, 4, true), 4);
        let mut initial = storage.begin(true).unwrap();
        for (key, value) in &baseline {
            initial.put(key, value).unwrap();
        }
        initial.commit().unwrap();
        let mut txn = Counted {
            inner: storage.begin(true).unwrap(),
            visits: 0.into(),
            visited: 0.into(),
            point_reads: 0.into(),
            max_read: 0.into(),
            fail_after: fail.then_some(0),
        };
        txn.inner.put(&[2], &[0xaa; 32]).unwrap();
        let result = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }.rechunk(
            1,
            &[],
            &[],
            false,
        );
        if fail {
            assert_eq!(result, Err(Error::Storage(0xfeed)));
            assert!(txn.inner.get(&[2]).unwrap().is_some());
        } else {
            assert_eq!(result.unwrap(), None);
            assert_eq!(dump(&txn.inner), baseline);
        }
        assert!(txn.visits.get() >= 2);
        drop(txn);
        assert_eq!(dump(&storage.begin(false).unwrap()), baseline);
    }
}

#[test]
fn streamed_parent_diff_handles_long_chunks_many_parents_and_maximum_keys() {
    for bf in [2, 128] {
        for boundary in [false, true] {
            let storage = MemStorage::new();
            let leaves = boundary_fixture(512, bf, boundary);
            let mut txn = Counted {
                inner: storage.begin(true).unwrap(),
                visits: 0.into(),
                visited: 0.into(),
                point_reads: 0.into(),
                max_read: 0.into(),
                fail_after: None,
            };
            for (key, value) in &leaves {
                txn.inner.put(key, value).unwrap();
            }
            // Old parents with keys inside, between and beyond new heads
            // must be deleted by the same ordered merge, not a rebuild.
            for n in 0..514u32 {
                let mut key = vec![1];
                key.extend_from_slice(&n.to_be_bytes());
                txn.inner.put(&key, &[0xa5; 32]).unwrap();
            }
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: bf }
                .propagate(&[], &[0xff; crate::KEY_SIZE_MAX + 1], false)
                .unwrap();
            assert_eq!(dump(&txn.inner), canonical(leaves, bf));
            assert!(txn.max_read.get() <= encoding::ENT_HEADER_FULL);
            assert_eq!(txn.point_reads.get(), 0, "repair must use bounded header visits");
            let parents = dump(&txn.inner).iter().filter(|(k, _)| k[0] == 1).count();
            assert_eq!(parents, if boundary { 512 } else { 1 });
        }
    }
}

#[test]
fn streamed_parent_diff_failure_discards_partial_parent_writes_and_deletions() {
    let storage = MemStorage::new();
    let leaves = boundary_fixture(48, 2, true);
    let baseline = canonical(leaves, 2);
    let mut initial = storage.begin(true).unwrap();
    for (key, value) in &baseline {
        initial.put(key, value).unwrap();
    }
    initial.commit().unwrap();
    for stale_parents in [false, true] {
        let mut txn = Counted {
            inner: storage.begin(true).unwrap(),
            visits: 0.into(),
            visited: 0.into(),
            point_reads: 0.into(),
            max_read: 0.into(),
            fail_after: Some(3),
        };
        if stale_parents {
            // Extra parents force deletion after the first anchor repair.
            for n in 0..48u32 {
                let mut key = vec![1];
                key.extend_from_slice(&n.to_be_bytes());
                txn.inner.put(&key, &[0xaa; 32]).unwrap();
            }
        } else {
            // Wrong parent hashes force writes while retaining all leaves.
            for (key, _) in baseline.iter().filter(|(k, _)| k[0] == 1) {
                txn.inner.put(key, &[0xaa; 32]).unwrap();
            }
        }
        let result = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 2 }.propagate(
            &[],
            &[0xff; crate::KEY_SIZE_MAX + 1],
            false,
        );
        assert_eq!(result, Err(Error::Storage(0xfeed)));
        assert_eq!(txn.fail_after, Some(0));
        assert_ne!(dump(&txn.inner), baseline);
        drop(txn);
        assert_eq!(dump(&storage.begin(false).unwrap()), baseline);
        storage.begin(true).unwrap().commit().unwrap();
    }
}

#[test]
fn parent_interval_rejects_oversized_stored_bounds_in_both_directions() {
    for backward in [false, true] {
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        txn.put(&[1], &[0; 32]).unwrap();
        let mut invalid = vec![1];
        invalid.extend_from_slice(&[if backward { 0 } else { 0xff }; crate::KEY_SIZE_MAX + 1]);
        txn.put(&invalid, &[0; 32]).unwrap();
        let before = dump(&txn);
        let result = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
            .leaf_interval(&[0x80; crate::KEY_SIZE_MAX]);
        assert_eq!(result, Err(Error::Corrupt));
        assert_eq!(dump(&txn), before, "invalid bounds must not mutate the transaction");
    }
}

#[test]
fn tail_deletion_errors_at_a_boundary_and_level_end_roll_back_the_transaction() {
    for bounded_end in [false, true] {
        let storage = MemStorage::new();
        let leaves = boundary_fixture(if bounded_end { 3 } else { 1 }, 4, true);
        let baseline = canonical(leaves.clone(), 4);
        let mut initial = storage.begin(true).unwrap();
        for (key, value) in &baseline {
            initial.put(key, value).unwrap();
        }
        initial.commit().unwrap();
        let mut txn = Counted {
            inner: storage.begin(true).unwrap(),
            visits: 0.into(),
            visited: 0.into(),
            point_reads: 0.into(),
            max_read: 0.into(),
            fail_after: None,
        };
        let key = &leaves[0].0[1..];
        let entry = EntVal::decode(&leaves[0].1).unwrap();
        assert_eq!(
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }
                .write_leaf_from(
                    key,
                    &SliceValue(entry.userval),
                    0,
                    Clock { time: 10_000, count: 0 }
                )
                .unwrap(),
            Some(true)
        );
        // First chunk maps to the empty anchor, so this key is an obsolete
        // parent after the computed anchor but before the next chunk head.
        let mut stale = leaves[0].0.clone();
        stale[0] = 1;
        txn.inner.put(&stale, &[0xaa; 32]).unwrap();
        // The anchor's clock-dependent hash changes successfully; fail on
        // the next mutation, the tail deletion through each endpoint arm.
        txn.fail_after = Some(1);
        let result =
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }.propagate(key, key, true);
        assert_eq!(result, Err(Error::Storage(0xfeed)));
        assert_eq!(txn.fail_after, Some(0));
        assert!(txn.inner.get(&stale).unwrap().is_some());
        assert_ne!(
            txn.inner.get(&[1]).unwrap().unwrap(),
            baseline.iter().find(|(k, _)| k == &[1]).unwrap().1.as_slice()
        );
        drop(txn);
        assert_eq!(dump(&storage.begin(false).unwrap()), baseline);
        storage.begin(true).unwrap().commit().unwrap();
    }
}

#[test]
fn streamed_parent_repair_has_fixed_scratch_and_rejects_oversized_keys() {
    // Conservative simultaneous allocation certificate, not expected
    // fanout: two propagation bounds; first/cursor/next/parent keys;
    // parent cursor and two changed bounds; one stored parent; temporary
    // replacements; caller leaf key; head-search/cleanup temporaries.
    // Sixteen key allocations cover these mutually exclusive phases.
    // Include their Vec headers, one hasher, four entry headers, three
    // hashes and 4 KiB scalar/closure/stack allowance. There is no
    // recursion, collection of parents, or allocation per tree level.
    let bytes = 16 * (crate::KEY_SIZE_MAX + 1 + core::mem::size_of::<Vec<u8>>())
        + core::mem::size_of::<BranchHasher>()
        + 4 * encoding::ENT_HEADER_FULL
        + 3 * 32
        + 4096;
    std::println!(
        "repair scratch certificate: {bytes} bytes; BranchHasher: {} bytes",
        core::mem::size_of::<BranchHasher>()
    );
    assert!(bytes <= 16 * 1024, "repair scratch certificate: {bytes}");
    let mut header = [0; encoding::ENT_HEADER_FULL];
    assert_eq!(
        read_node_header(&[1; crate::KEY_SIZE_MAX + 2], &SliceValue(&[0; 32]), &mut header),
        Err(Error::Corrupt),
    );
}

#[test]
fn retained_ranges_scale_with_tree_levels_and_match_independent_canonical_tree() {
    for size in [32, 10_000] {
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        for n in 0..size as u32 {
            let mut key = [0; 16];
            key[..4].copy_from_slice(&n.to_be_bytes());
            let value = [(n % 251) as u8; 32];
            let flags = if n % 17 == 0 { ENT_FLAG_TOMBSTONE } else { 0 };
            let value = if flags == 0 { &value[..] } else { &[] };
            let hash =
                crate::hash::entry_hash_from(&HASH_KEY, &key, flags, &SliceValue(value)).unwrap();
            let mut encoded = Vec::new();
            encoding::encode_ent_val(
                &mut encoded,
                &hash,
                flags,
                Clock { time: n + 1, count: 0 },
                value,
            );
            txn.put(&leaf_skey(&key), &encoded).unwrap();
        }
        Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 32 }.build_all().unwrap();
        let before = canonical(
            dump(&txn).into_iter().filter(|(key, _)| node_at_level(key, 0)).collect(),
            32,
        );
        assert_eq!(dump(&txn), before);
        txn.commit().unwrap();
        let snapshot = storage.begin(false).unwrap();
        let before_root = root(&snapshot).unwrap();
        let mut counted = Counted {
            inner: storage.begin(true).unwrap(),
            visits: 0.into(),
            visited: 0.into(),
            point_reads: 0.into(),
            max_read: 0.into(),
            fail_after: None,
        };
        let mut key = [0; 16];
        key[..4].copy_from_slice(&(size as u32 / 2).to_be_bytes());
        Tree { txn: &mut counted, hash_key: HASH_KEY, branch_factor: 32 }
            .put_leaf(&key, &[0x7f; 32], 0, Clock { time: 50_000, count: 3 })
            .unwrap();
        let visits = counted.visits.get();
        let entries = counted.visited.get();
        let level = before_root.unwrap().0 as usize;
        assert!(visits <= 5 * (level + 2) + 1, "{visits} ranges at depth{level}");
        assert!(counted.point_reads.get() <= level + 1, "per-node point lookup returned");
        assert!(counted.max_read.get() <= encoding::ENT_HEADER_FULL);
        if size == 10_000 {
            assert!(
                entries > visits * 2,
                "fixture must traverse multiple nodes per retained range"
            );
        }
        let expected = canonical(
            dump(&counted.inner).into_iter().filter(|(key, _)| node_at_level(key, 0)).collect(),
            32,
        );
        assert_eq!(dump(&counted.inner), expected);
        counted.commit().unwrap();
        assert_eq!(
            root(&snapshot).unwrap(),
            before_root,
            "a retained snapshot must not observe the writer"
        );
        assert_eq!(dump(&snapshot), before);
        assert_eq!(dump(&storage.begin(false).unwrap()), expected);
    }
}

#[test]
fn streamed_maximum_leaf_reads_only_header_and_preserves_truncation_errors() {
    struct Large {
        header: [u8; encoding::ENT_HEADER_FULL],
        length: usize,
        maximum: core::cell::Cell<usize>,
        short: bool,
    }
    impl ValueRead for Large {
        fn len(&self) -> usize {
            self.length
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
            self.maximum.set(self.maximum.get().max(out.len()));
            assert!(
                offset + out.len() <= encoding::ENT_HEADER_FULL,
                "tree attempted to materialize a large value"
            );
            if self.short {
                return Ok(0);
            }
            out.copy_from_slice(&self.header[offset..offset + out.len()]);
            Ok(out.len())
        }
    }
    let mut value = Large {
        header: [0x42; encoding::ENT_HEADER_FULL],
        length: crate::VALUE_SIZE_MAX + encoding::ENT_HEADER_FULL,
        maximum: 0.into(),
        short: false,
    };
    let mut scratch = [0; encoding::ENT_HEADER_FULL];
    assert_eq!(read_node_header(b"\0key", &value, &mut scratch).unwrap(), &value.header);
    assert_eq!(value.maximum.get(), encoding::ENT_HEADER_FULL);
    assert_eq!(read_node_header(b"\x01key", &value, &mut scratch), Err(Error::Corrupt));
    value.short = true;
    assert_eq!(read_node_header(b"\0key", &value, &mut scratch), Err(Error::Corrupt));
}

#[test]
fn stable_first_leaf_overwrite_skips_unaffected_chunks() {
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    for n in 0..256u32 {
        Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 32 }
            .put_leaf(&n.to_be_bytes(), &[(n % 251) as u8; 32], 0, Clock { time: n + 1, count: 0 })
            .unwrap();
    }
    let baseline = dump(&txn);
    txn.commit().unwrap();
    let other = MemStorage::new();
    let mut general = other.begin(true).unwrap();
    for (key, value) in &baseline {
        general.put(key, value).unwrap();
    }
    let counted = |inner| Counted {
        inner,
        visits: 0.into(),
        visited: 0.into(),
        point_reads: 0.into(),
        max_read: 0.into(),
        fail_after: None,
    };
    let mut fast = counted(storage.begin(true).unwrap());
    let mut general = counted(general);
    let key = 0u32.to_be_bytes();
    let clock = Clock { time: 10_000, count: 2 };
    Tree { txn: &mut fast, hash_key: HASH_KEY, branch_factor: 32 }
        .put_leaf(&key, &[0; 32], 0, clock)
        .unwrap();
    // Apply exactly the same clock-only overwrite, but explicitly retain
    // the general level-0 window as a structural and work-count reference.
    let hash = crate::hash::entry_hash_from(&HASH_KEY, &key, 0, &SliceValue(&[0; 32])).unwrap();
    let mut encoded = Vec::new();
    encoding::encode_ent_val(&mut encoded, &hash, 0, clock, &[0; 32]);
    general.put(&leaf_skey(&key), &encoded).unwrap();
    Tree { txn: &mut general, hash_key: HASH_KEY, branch_factor: 32 }
        .leaf_changed(&key, false)
        .unwrap();
    assert!(
        fast.visited.get() < general.visited.get(),
        "stable first leaf must not scan the next unchanged chunk"
    );
    let fast_tree = dump(&fast.inner);
    assert_eq!(fast_tree, dump(&general.inner));
    assert_eq!(
        fast_tree,
        canonical(fast_tree.iter().filter(|(key, _)| node_at_level(key, 0)).cloned().collect(), 32)
    );
    assert_eq!(EntVal::decode(&encoded).unwrap().clock, clock);
}

#[test]
fn overwrite_hint_preserves_all_boundary_and_encoding_transitions() {
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let mut model = BTreeMap::new();
    let mut rng = 0x1735_912d_873c_916fu64;
    let mut boundaries = [[0usize; 2]; 2];
    let mut encodings = [[0usize; 2]; 2];
    let mut first_stable = 0;
    let mut tombstone_transitions = 0;
    let mut inserted = 0;
    let mut removed = 0;
    for n in 0..1200u32 {
        let key = vec![if n % 8 == 0 { 0 } else { (xorshift(&mut rng) % 32) as u8 }];
        let old = txn.get(&leaf_skey(&key)).unwrap().map(|bytes| bytes.to_vec());
        if n % 13 == 0 {
            if (Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 })
                .remove_leaf(&key)
                .unwrap()
            {
                removed += 1;
                model.remove(&key);
            }
        } else {
            let length = [0, 1, 30, 31, 64][(xorshift(&mut rng) % 5) as usize];
            let tombstone = n % 7 == 0;
            let value: Vec<_> = if tombstone {
                vec![]
            } else {
                (0..length).map(|_| xorshift(&mut rng) as u8).collect()
            };
            let clock = Clock { time: n + 1, count: n % 3 };
            (Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 })
                .put_leaf(&key, &value, if tombstone { ENT_FLAG_TOMBSTONE } else { 0 }, clock)
                .unwrap();
            let current = txn.get(&leaf_skey(&key)).unwrap().unwrap().to_vec();
            let entry = EntVal::decode(&current).unwrap();
            assert_eq!(entry.clock, clock);
            assert_eq!(entry.is_tombstone(), tombstone);
            if let Some(old) = old {
                let was = encoding::is_boundary(old[0], 4);
                let now = encoding::is_boundary(current[0], 4);
                boundaries[usize::from(was)][usize::from(now)] += 1;
                encodings[usize::from(old.len() >= encoding::ENT_HEADER_FULL)]
                    [usize::from(current.len() >= encoding::ENT_HEADER_FULL)] += 1;
                if key == [0] && was == now {
                    first_stable += 1;
                }
                if EntVal::decode(&old).unwrap().is_tombstone() != tombstone {
                    tombstone_transitions += 1;
                }
            } else {
                inserted += 1;
            }
            model.insert(key, if tombstone { None } else { Some(value) });
        }
        // The independent oracle is checked after every mutation, including
        // first-key deletion/reinsertion; no bulk rebuild can mask leakage.
        verify(&mut txn, &model, 4);
    }
    assert!(
        boundaries.iter().flatten().all(|count| *count > 0),
        "must exercise stable and changing boundary heads"
    );
    assert!(
        encodings.iter().flatten().all(|count| *count > 0),
        "must exercise compact/full transitions"
    );
    assert!(first_stable > 0 && tombstone_transitions > 0 && inserted > 0 && removed > 0);
}

#[test]
fn streamed_leaf_writes_check_the_value_bound_and_surface_backend_failures() {
    let storage = MemStorage::new();
    let mut txn = Counted {
        inner: storage.begin(true).unwrap(),
        visits: 0.into(),
        visited: 0.into(),
        point_reads: 0.into(),
        max_read: 0.into(),
        fail_after: Some(0),
    };
    let mut tree = Tree { txn: &mut txn, hash_key: [3; 32], branch_factor: 4 };
    let clock = Clock { time: 9, count: 0 };
    let oversized = vec![0u8; crate::VALUE_SIZE_MAX + 1];
    assert_eq!(tree.put_leaf_from(b"k", &SliceValue(&oversized), 0, clock), Err(Error::ValueSize));
    // The backend refuses the first mutation: the failure reaches the caller
    // before any branch work.
    assert_eq!(
        tree.write_leaf_from(b"k", &SliceValue(b"value"), 0, clock),
        Err(Error::Storage(0xfeed))
    );
    assert!(dump(&txn.inner).is_empty());
}

#[test]
fn verification_rejects_corrupt_structure_without_repairing_it() {
    // Damage a canonical tree in independent ways; verification must report
    // corruption without replacing the evidence or allocating a second tree.
    let storage = MemStorage::new();
    let mut original = storage.begin(true).unwrap();
    for i in 0..64u8 {
        Tree { txn: &mut original, hash_key: HASH_KEY, branch_factor: 2 }
            .put_leaf(&[i + 1], &[i; 64], 0, Clock { time: 1, count: 0 })
            .unwrap();
    }
    let canonical = dump(&original);
    for damage in 0..13 {
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        for (key, value) in &canonical {
            txn.put(key, value).unwrap();
        }
        let leaf = canonical[0].0.clone();
        let mut value = canonical[0].1.clone();
        match damage {
            0 => {
                txn.del(&[1]).unwrap();
            }
            1 => {
                txn.put(&[1], &[0; 32]).unwrap();
            }
            2 => {
                txn.put(&[1, 255], &[0; 32]).unwrap();
            }
            3 => {
                txn.put(&[encoding::LEVEL_MAX], &[0; 32]).unwrap();
            }
            4 => {
                txn.put(&[], b"invalid key").unwrap();
            }
            5 => {
                txn.put(&[0], &value).unwrap();
            }
            6 => {
                value[0] ^= 1;
                txn.put(&leaf, &value).unwrap();
            }
            7 => {
                value[33..41].fill(0);
                txn.put(&leaf, &value).unwrap();
            }
            8 => {
                value[32] = ENT_FLAG_TOMBSTONE;
                txn.put(&leaf, &value).unwrap();
            }
            9 => {
                txn.put(&[1], &[0; 4096]).unwrap();
            }
            10 => {
                txn.put(&[META_KEY_BYTE + 1], b"invalid level").unwrap();
            }
            11 => {
                txn.put(&encoding::META_KEY, b"metadata").unwrap();
                txn.put(&[META_KEY_BYTE, 1], b"invalid metadata key").unwrap();
            }
            12 => {
                value[encoding::ENT_HEADER_FULL] ^= 1;
                txn.put(&leaf, &value).unwrap();
            }
            _ => unreachable!(),
        }
        let damaged = dump(&txn);
        assert_eq!(
            Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 2 }.verify(),
            Err(Error::Corrupt),
            "damage {damage}"
        );
        assert_eq!(dump(&txn), damaged, "verification must never repair corruption");
    }
}

#[test]
fn unchanged_rechunk_has_no_changed_parents() {
    // Recomputing an unchanged window must not report a parent mutation,
    // including anchors, boundary changes from preceding inserts and full keys.
    for branch_factor in [2, 32, 128] {
        let storage = MemStorage::new();
        let mut txn = storage.begin(true).unwrap();
        let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor };
        for key in 1..=64u8 {
            tree.put_leaf(&[key; crate::KEY_SIZE_MAX], &[key; 32], 0, Clock { time: 1, count: 0 })
                .unwrap();
        }
        for key in 1..=64u8 {
            assert_eq!(
                tree.rechunk(0, &[key; crate::KEY_SIZE_MAX], &[key; crate::KEY_SIZE_MAX], true)
                    .unwrap(),
                None
            );
        }
        tree.verify().unwrap();
    }
}

#[test]
fn verifier_rejects_invalid_parameters_and_chunk_boundaries() {
    // Exercise verifier failure boundaries directly: invalid metadata, empty
    // requested chunks and non-anchor first branches must not hash as a tree.
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 1 };
    assert_eq!(tree.verify(), Err(Error::Corrupt));
    tree.branch_factor = 4;
    assert_eq!(tree.verify_chunk(1, &[1]), Err(Error::Corrupt));
    tree.txn.put(&[1, b'a'], &[0; 32]).unwrap();
    assert_eq!(tree.verify_chunk(1, &[1]), Err(Error::Corrupt));
    // A borrowed payload cannot read beyond its declared end into another field.
    let raw = crate::storage::SliceValue(b"headerpayload");
    let value = LeafValue { raw: &raw, start: 6 };
    let mut out = [0; 8];
    assert_eq!(value.read_at(8, &mut out), Err(Error::InvalidArgument));
    assert_eq!(value.read_at(7, &mut out).unwrap(), 0);
}

#[test]
fn verification_rejects_noncanonical_full_header_for_short_value() {
    // A full header for a compact-sized payload can carry a correct hash but
    // is not a storage layout this replica writes; header-only wire reads are
    // permitted by EntVal::decode, not by whole-tree verification.
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let hash = crate::hash::entry_hash(&HASH_KEY, b"key", 0, b"");
    let mut stored = hash.to_vec();
    stored.push(0);
    stored.extend_from_slice(&Clock { time: 1, count: 0 }.encode());
    txn.put(b"\0key", &stored).unwrap();
    assert_eq!(
        Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }.verify(),
        Err(Error::Corrupt)
    );
}

#[test]
fn ripple_returns_capacity_when_storage_exhausts_the_level_budget() {
    // Synthetic damaged storage keeps many boundary heads at every representable
    // level. The ripple must terminate at the format cap, even without the
    // probabilistic shrinkage a healthy tree normally exhibits. Drop all writes.
    let storage = MemStorage::new();
    let mut txn = storage.begin(true).unwrap();
    let mut leaf = vec![0; encoding::ENT_HEADER_COMPACT];
    leaf[2..10].copy_from_slice(&Clock { time: 1, count: 0 }.encode());
    for level in 0..=encoding::LEVEL_MAX {
        if level > 0 {
            txn.put(&[level], &[0; 32]).unwrap();
        }
        for key in 1..=512u16 {
            let mut stored_key = vec![level];
            stored_key.extend_from_slice(&key.to_be_bytes());
            txn.put(&stored_key, if level == 0 { &leaf } else { &[0; 32] }).unwrap();
        }
    }
    let mut tree = Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 2 };
    assert_eq!(
        tree.propagate(&1u16.to_be_bytes(), &1u16.to_be_bytes(), true),
        Err(Error::Capacity)
    );
}

#[test]
fn verifier_rejects_oversized_tail_keys_without_copying_them() {
    let storage = MemStorage::new();
    let mut txn = Counted {
        inner: storage.begin(true).unwrap(),
        visits: 0.into(),
        visited: 0.into(),
        point_reads: 0.into(),
        max_read: 0.into(),
        fail_after: None,
    };
    txn.inner.put(&encoding::META_KEY, b"metadata").unwrap();
    txn.inner.put(&vec![META_KEY_BYTE + 1; 1024 * 1024], b"invalid tail").unwrap();
    assert_eq!(
        Tree { txn: &mut txn, hash_key: HASH_KEY, branch_factor: 4 }.verify(),
        Err(Error::Corrupt)
    );
    // No leaves: the audit must reject the raw tail through borrowed traversal,
    // without a point lookup/seek that could materialize this unbounded key.
    assert_eq!(txn.point_reads.get(), 0);
    assert_eq!(txn.max_read.get(), 0);
}
