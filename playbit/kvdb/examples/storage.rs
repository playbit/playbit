//! Basic operation: a plain (non-syncable) kvdb database on the redb
//! backend. No CRDT machinery, no clocks, no tree — just the ordered
//! transactional store.
//!
//! Run with: `cargo run --example storage -- <dbpath> <command> ...`

#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use kvdb::redb::RedbStorage;
use kvdb::{CreateOptions, Db};
use std::io::Write;
use std::process::exit;

const USAGE: &str = "\
Usage: examples/storage <dbpath> ls [<prefix>]
           List contents of database with optional prefix
       examples/storage <dbpath> set <key> <value>
           Add or replace an entry
       examples/storage <dbpath> get <key>
           Print value for key, or exit 1 if <key> does not exist
       examples/storage <dbpath> rm <key>
           Remove an entry. Exits 1 if <key> does not exist";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }
    if args.len() < 2 {
        eprintln!("{USAGE}");
        exit(2);
    }
    match run(&args[0], args[1].as_str(), &args[2..]) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("examples/storage: error: {e}");
            exit(1);
        }
    }
}

fn run(dbpath: &str, cmd: &str, args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let db = Db::open_or_create(
        RedbStorage::open(dbpath)?,
        &CreateOptions::plain(),
        &Default::default(),
    )?;
    match (cmd, args) {
        ("ls", args) if args.len() <= 1 => {
            let prefix = args.first().map(|s| s.as_bytes()).unwrap_or(b"");
            let txn = db.begin_read()?;
            for item in txn.iter_from(prefix) {
                let (key, value) = item?;
                if !key.starts_with(prefix) {
                    break;
                }
                println!("{}\t{}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&value));
            }
            Ok(0)
        }
        ("set", [key, value]) => {
            let mut txn = db.begin_write(wall_secs())?;
            txn.put(key.as_bytes(), value.as_bytes())?;
            txn.commit()?;
            Ok(0)
        }
        ("get", [key]) => {
            let txn = db.begin_read()?;
            match txn.get(key.as_bytes())? {
                Some(value) => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(&value)?;
                    stdout.write_all(b"\n")?;
                    Ok(0)
                }
                None => Ok(1),
            }
        }
        ("rm", [key]) => {
            let mut txn = db.begin_write(wall_secs())?;
            let existed = txn.del(key.as_bytes())?;
            txn.commit()?;
            Ok(if existed { 0 } else { 1 })
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

/// Current wall-clock time. kvdb is sans-io, so the caller supplies it;
/// a plain database only forwards it (no clocks are stored).
fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
