//! Fuzz database operations against a model: arbitrary op sequences must
//! keep the store consistent with a `BTreeMap`, and the resulting tree
//! must be canonical — rebuilding the final state from scratch, preserving
//! each entry's origin clock, must reproduce the identical root hash.
#![no_main]

mod support;

use arbitrary::Arbitrary;
use kvdb::{Clock, clock::EPOCH_UNIX_SECS};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

#[derive(Arbitrary, Debug)]
struct Plan {
    branch_factor: u8,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Put { k: u8, v: Vec<u8> },
    PutBig { k: u8, extra: u16 },
    Del { k: u8 },
    Forget { k: u8 },
    Commit { dt: u8 },
}

fn key(k: u8) -> Vec<u8> {
    // small keyspace with varying lengths so collisions and prefix
    // relationships are common
    let len = 1 + (k >> 4) % 5;
    vec![b'a' + (k % 16); len as usize]
}

fuzz_target!(|plan: Plan| {
    let bf = [2u8, 4, 8, 32][(plan.branch_factor % 4) as usize];
    let db = support::mk_db(bf);
    let mut model: BTreeMap<Vec<u8>, (Clock, Option<Vec<u8>>)> = BTreeMap::new(); // None = tombstone
    let mut wall = support::WALL;

    let mut ops = plan.ops;
    ops.truncate(96);
    let mut txn = db.begin_write(wall).unwrap();
    for op in ops {
        match op {
            Op::Put { k, mut v } => {
                v.truncate(64);
                txn.put(&key(k), &v).unwrap();
                model.insert(key(k), (txn.clock(), Some(v)));
            }
            Op::PutBig { k, extra } => {
                // over the compact threshold: full-layout entries
                let v = vec![k; 31 + (extra % 500) as usize];
                txn.put(&key(k), &v).unwrap();
                model.insert(key(k), (txn.clock(), Some(v)));
            }
            Op::Del { k } => {
                let existed = txn.del(&key(k)).unwrap();
                let model_live = matches!(model.get(&key(k)), Some((_, Some(_))));
                assert_eq!(existed, model_live, "del presence mismatch");
                if existed {
                    model.insert(key(k), (txn.clock(), None));
                }
            }
            Op::Forget { k } => {
                let existed = txn.del_forget(&key(k)).unwrap();
                assert_eq!(existed, model.contains_key(&key(k)), "forget presence mismatch");
                model.remove(&key(k));
            }
            Op::Commit { dt } => {
                txn.verify().unwrap();
                txn.commit().unwrap();
                wall += dt as u64;
                txn = db.begin_write(wall).unwrap();
            }
        }
        txn.verify().unwrap();
    }
    txn.verify().unwrap();
    txn.commit().unwrap();
    db.verify().unwrap();

    // 1. contents match the model (tombstones invisible)
    let live: Vec<(Vec<u8>, Vec<u8>)> =
        model.iter().filter_map(|(k, (_, v))| v.as_ref().map(|v| (k.clone(), v.clone()))).collect();
    assert_eq!(support::contents(&db), live, "contents diverged from model");

    // 2. tombstone visibility matches the model
    let r = db.begin_read().unwrap();
    for (k, (clock, v)) in &model {
        assert_eq!(r.is_del(k).unwrap(), v.is_none().then_some(*clock));
    }
    drop(r);

    // 3. Replay only surviving entries, grouped by their original transaction
    // clock and sorted key. This removes overwritten/forgotten history and
    // changes insertion order without changing the clock-aware logical state.
    let mut entries: Vec<_> = model.iter().collect();
    entries.sort_by_key(|(key, (clock, _))| (*clock, *key));
    let replay = support::mk_db(bf);
    let mut index = 0;
    while index < entries.len() {
        let origin = entries[index].1.0;
        let wall = EPOCH_UNIX_SECS + u64::from(origin.time);
        let mut w = replay.begin_write(wall).unwrap();
        // Omitted transactions can leave gaps in the logical counter. Advance
        // with empty commits; at most the original 96 operations are needed.
        while w.clock() < origin {
            w.verify().unwrap();
            w.commit().unwrap();
            w = replay.begin_write(wall).unwrap();
        }
        assert_eq!(w.clock(), origin, "replay changed an origin clock");
        while index < entries.len() && entries[index].1.0 == origin {
            let (k, (_, value)) = entries[index];
            match value {
                Some(v) => w.put(k, v).unwrap(),
                None => {
                    w.put(k, b"x").unwrap();
                    w.del(k).unwrap();
                }
            }
            index += 1;
        }
        w.verify().unwrap();
        w.commit().unwrap();
    }
    assert_eq!(
        support::root(&db),
        support::root(&replay),
        "root hash is not a pure function of contents and origin clocks"
    );
    // Rebuilding from stored leaves is a separate check of incremental tree
    // maintenance, preserving all leaf clocks regardless of transaction clock.
    let root = support::root(&db);
    let mut w = db.begin_write(wall + 999).unwrap();
    w.rebuild_tree().unwrap();
    w.verify().unwrap();
    w.commit().unwrap();
    db.verify().unwrap();
    assert_eq!(support::root(&db), root, "tree rebuild changed the root");
});
