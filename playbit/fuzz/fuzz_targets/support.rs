//! Shared helpers for the fuzz targets.
#![allow(dead_code)]

use kvdb::storage::mem::MemStorage;
use kvdb::sync::{SyncReceiver, SyncSender, SyncStatus};
use kvdb::{CreateOptions, Db, Lww, SyncResolver};

pub const HASH_KEY: [u8; 32] = [0x5a; 32];
pub const WALL: u64 = 1_800_000_000; // seconds, past the kvdb epoch

pub fn mk_db(branch_factor: u8) -> Db<MemStorage> {
    let mut opts = CreateOptions::syncable(HASH_KEY, 0);
    opts.branch_factor = branch_factor;
    Db::create(MemStorage::new(), &opts).unwrap()
}

/// A small database with seed-derived contents (entries and tombstones).
pub fn small_db(seed: u64) -> Db<MemStorage> {
    let db = mk_db(4);
    let mut w = db.begin_write(WALL).unwrap();
    for i in 0..(seed % 24) {
        let key = format!("k{:02}", (i * seed) % 40);
        if i % 5 == 4 {
            let _ = w.del(key.as_bytes()).unwrap();
        } else {
            let vlen = ((i * 7 + seed) % 60) as usize;
            w.put(key.as_bytes(), &vec![i as u8; vlen]).unwrap();
        }
    }
    w.verify().unwrap();
    w.commit().unwrap();
    db
}

/// Drive one complete pull session in memory. Panics on stall or error —
/// both are findings when the inputs are well-formed databases.
pub fn pump<R: SyncResolver>(dst: &Db<MemStorage>, src: &Db<MemStorage>, resolver: R) {
    let mut r_out = Vec::new();
    let mut rx = SyncReceiver::new(dst, resolver, Staging::default(), WALL, &mut r_out).unwrap();
    let mut tx = SyncSender::new(src).unwrap();
    let mut s_out = Vec::new();
    let (mut r_done, mut s_done) = (false, false);
    let mut rounds = 0;
    while !r_done || !s_done {
        rounds += 1;
        assert!(rounds < 1_000_000, "sync made no progress");
        if !r_out.is_empty() {
            let data = std::mem::take(&mut r_out);
            s_done = drain(&mut tx, &data, &mut s_out).unwrap() == SyncStatus::Done;
        } else if !s_out.is_empty() {
            let data = std::mem::take(&mut s_out);
            r_done = rx.process(&data, &mut r_out).unwrap() == SyncStatus::Done;
        } else {
            panic!("sync stalled (r_done={r_done} s_done={s_done})");
        }
    }
    rx.finish().unwrap();
    dst.verify().unwrap();
    src.verify().unwrap();
}

pub fn contents(db: &Db<MemStorage>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let r = db.begin_read().unwrap();
    r.iter().map(|kv| kv.map(|(k, v)| (k.to_vec(), v.to_vec()))).collect::<Result<_, _>>().unwrap()
}

pub fn root(db: &Db<MemStorage>) -> Option<(u8, [u8; 32])> {
    db.begin_read().unwrap().root_hash().unwrap()
}

/// Bidirectional convergence check for two replicas sharing a hash key.
pub fn assert_convergence(a: &Db<MemStorage>, b: &Db<MemStorage>) {
    pump(a, b, Lww);
    pump(b, a, Lww);
    pump(a, b, Lww);
    assert_eq!(root(a), root(b), "replicas failed to converge");
    assert_eq!(contents(a), contents(b));
}

#[derive(Default)]
pub struct Staging(Vec<u8>);
impl kvdb::ValueRead for Staging {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> kvdb::error::Result<usize> {
        kvdb::storage::SliceValue(&self.0).read_at(offset, out)
    }
}
impl kvdb::SyncStaging for Staging {
    fn clear(&mut self) -> kvdb::error::Result<()> {
        self.0.clear();
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> kvdb::error::Result<()> {
        if self.0.len().saturating_add(bytes.len()) > kvdb::sync::MAX_FRAME {
            return Err(kvdb::Error::Capacity);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}
pub fn drain<S: kvdb::Storage>(
    tx: &mut SyncSender<'_, S>,
    input: &[u8],
    out: &mut Vec<u8>,
) -> kvdb::error::Result<SyncStatus> {
    let status = tx.process(input);
    let mut buf = [0; 4096];
    loop {
        let n = tx.write_output(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    status
}
