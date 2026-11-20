#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

#[test]
fn snapshot_isolation_and_atomic_commit() {
    let s = MemStorage::new();
    let mut w = s.begin(true).unwrap();
    w.put(b"a", b"1").unwrap();
    w.put(b"b", b"2").unwrap();
    let r = s.begin(false).unwrap();
    assert_eq!(r.get(b"a").unwrap(), None); // uncommitted invisible
    assert_eq!(w.get(b"a").unwrap(), Some(&b"1"[..])); // read-your-writes
    w.commit().unwrap();
    assert_eq!(r.get(b"a").unwrap(), None); // snapshot stays put
    let r2 = s.begin(false).unwrap();
    assert_eq!(r2.get(b"a").unwrap(), Some(&b"1"[..]));
    assert_eq!(r2.get(b"b").unwrap(), Some(&b"2"[..]));
}

#[test]
fn single_writer_and_abort() {
    let s = MemStorage::new();
    let mut w = s.begin(true).unwrap();
    w.put(b"x", b"1").unwrap();
    assert!(matches!(s.begin(true), Err(Error::Busy)));
    drop(w); // abort
    let w2 = s.begin(true).unwrap(); // writer slot released
    drop(w2);
    assert_eq!(s.begin(false).unwrap().get(b"x").unwrap(), None);
}

#[test]
fn ordered_seeks() {
    let s = MemStorage::new();
    let mut w = s.begin(true).unwrap();
    for k in [&b"a"[..], b"c", b"e"] {
        w.put(k, b"v").unwrap();
    }
    w.commit().unwrap();
    let r = s.begin(false).unwrap();
    let k = |o: Option<(&[u8], &[u8])>| o.map(|(k, _)| k.to_vec());
    assert_eq!(k(r.next_from(b"b", false).unwrap()), Some(b"c".to_vec()));
    assert_eq!(k(r.next_from(b"c", false).unwrap()), Some(b"c".to_vec()));
    assert_eq!(k(r.next_from(b"c", true).unwrap()), Some(b"e".to_vec()));
    assert_eq!(k(r.next_from(b"f", false).unwrap()), None);
    assert_eq!(k(r.prev_from(b"b", false).unwrap()), Some(b"a".to_vec()));
    assert_eq!(k(r.prev_from(b"a", true).unwrap()), None);
    assert_eq!(k(r.prev_from(b"z", false).unwrap()), Some(b"e".to_vec()));
}

#[test]
fn reported_props() {
    let p = MemStorage::new().props();
    assert_eq!(p.retention, Retention::Ephemeral);
    assert!(!p.durable_program_crash && !p.durable_power_loss);
}

#[test]
fn readonly_txn_rejects_writes() {
    let s = MemStorage::new();
    let mut r = s.begin(false).unwrap();
    assert!(matches!(r.put(b"a", b"1"), Err(Error::ReadOnly)));
    assert!(matches!(r.del(b"a"), Err(Error::ReadOnly)));
}

#[test]
fn key_seeks_run_in_both_directions_without_copying_values() {
    let s = MemStorage::new();
    let mut w = s.begin(true).unwrap();
    for k in [b"a", b"c", b"e"] {
        w.put(k, b"v").unwrap();
    }
    w.commit().unwrap();
    let r = s.begin(false).unwrap();
    assert_eq!(r.next_key(b"b", false).unwrap().as_deref(), Some(&b"c"[..]));
    assert_eq!(r.next_key(b"c", true).unwrap().as_deref(), Some(&b"e"[..]));
    assert_eq!(r.next_key(b"e", true).unwrap(), None);
    assert_eq!(r.prev_key(b"d", false).unwrap().as_deref(), Some(&b"c"[..]));
    assert_eq!(r.prev_key(b"c", false).unwrap().as_deref(), Some(&b"c"[..]));
    assert_eq!(r.prev_key(b"c", true).unwrap().as_deref(), Some(&b"a"[..]));
    assert_eq!(r.prev_key(b"a", true).unwrap(), None);
    assert_eq!(r.prev_key(b"", false).unwrap(), None);
}

#[test]
fn snapshot_value_pins_only_read_transactions() {
    let s = MemStorage::new();
    let mut w = s.begin(true).unwrap();
    w.put(b"a", b"1").unwrap();
    assert!(matches!(w.snapshot_value(b"a"), Err(Error::Mode)));
    w.commit().unwrap();
    let r = s.begin(false).unwrap();
    assert_eq!(r.snapshot_value(b"a").unwrap().map(|v| v.len()), Some(1));
    assert!(r.snapshot_value(b"b").unwrap().is_none());
}

#[test]
fn exact_reads_reject_ranges_past_the_end() {
    let value = SliceValue(b"abcdef");
    let mut out = [0; 4];
    value.read_exact_at(2, &mut out).unwrap();
    assert_eq!(&out, b"cdef");
    assert_eq!(value.read_exact_at(3, &mut out), Err(Error::Corrupt));
    assert_eq!(value.read_exact_at(usize::MAX, &mut out), Err(Error::Corrupt));
    assert_eq!(value.read_exact_at(6, &mut []), Ok(()));
}
