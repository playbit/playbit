//! Full-featured operation: a syncable kvdb database on the redb backend,
//! with TCP synchronization between replicas and an interactive mode.
//!
//! Try it in two terminals:
//!
//! ```text
//! term1$ cargo run --example kvdb -- --listen /tmp/a.db
//! replica 79a19690356ca5e0 (tcp:127.0.0.1:51226)
//! Interactive mode (ls, set, get, rm or help)
//! > set greeting hello
//!
//! term2$ cargo run --example kvdb -- /tmp/b.db pull tcp:127.0.0.1:51226
//! term2$ cargo run --example kvdb -- /tmp/b.db get greeting
//! hello
//! ```

#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use kvdb::redb::{FileStaging, RedbStorage};
use kvdb::sync::{SyncReceiver, SyncSender, SyncStatus};
use kvdb::{CreateOptions, Db, Lww};
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::exit;
use std::sync::{Arc, Mutex};

// (no `"\` continuation here: it would strip the first line's indent)
const COMMANDS: &str = "    ls [<prefix>]      List keys, optionally by prefix.
    ls -l [<prefix>]   List keys and values, optionally by prefix.
    set <key> <value>  Add or replace an entry.
    get <key>          Print value for key on stdout. Exits 1 if <key> does not exist.
    rm <key>           Remove an entry. Exits 1 if <key> does not exist.
    pull <replica-url> Pull in changes from replica database at <replica-url>.";

fn usage() -> String {
    format!(
        "\
Usage: examples/kvdb [options] <dbpath> [<command> [<arg> ...]]
If no <command> is given, kvdb runs in interactive mode.
Options:
    --help             Display help on stdout and exit.
    --verbose          Print extra details on stderr (e.g. hashes.)
    --listen [<addr>]  Run TCP server bound to <addr>, for syncing. If no <addr> is given,
                       binds to localhost only and assign any available port.
Commands:
{COMMANDS}
<replica-url>:
    E.g. \"tcp:127.0.0.1:12345\""
    )
}

/// Every database this example creates shares this hash key, so any two of
/// them can sync with each other. A real application must generate a
/// random key when a database (replica set) is created and distribute it
/// to the other replicas along with the data.
const DEMO_HASH_KEY: [u8; 32] = *b"kvdb example shared hash key\0\0\0\0";

struct Opts {
    verbose: bool,
    /// `Some(None)` = listen on localhost with any port;
    /// `Some(Some(addr))` = listen on the given address.
    listen: Option<Option<String>>,
}

type AnyError = Box<dyn std::error::Error>;
type SharedDb = Arc<Mutex<Db<RedbStorage>>>;

fn main() {
    let mut opts = Opts { verbose: false, listen: None };
    let mut positional: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        // Options come before <dbpath>; everything after the first
        // positional belongs to the command (e.g. `ls -l`).
        if !positional.is_empty() {
            positional.push(arg);
            continue;
        }
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return;
            }
            "--verbose" => opts.verbose = true,
            "--listen" => {
                // The optional <addr> is recognized by its ':' (a dbpath
                // is unlikely to contain one).
                let addr = match args.peek() {
                    Some(next) if next.contains(':') => Some(args.next().unwrap()),
                    _ => None,
                };
                opts.listen = Some(addr);
            }
            _ if arg.starts_with("--listen=") => {
                opts.listen = Some(Some(arg["--listen=".len()..].to_string()));
            }
            _ if arg.starts_with('-') => {
                eprintln!("examples/kvdb: unknown option {arg}\n{}", usage());
                exit(2);
            }
            _ => positional.push(arg),
        }
    }

    if positional.is_empty() {
        eprintln!("{}", usage());
        exit(2);
    }

    match run(&opts, &positional[0], &positional[1..]) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("examples/kvdb: error: {e}");
            exit(1);
        }
    }
}

fn run(opts: &Opts, dbpath: &str, command: &[String]) -> Result<i32, AnyError> {
    let create = CreateOptions::syncable(DEMO_HASH_KEY, random_u64());
    let db = Db::open_or_create(RedbStorage::open(dbpath)?, &create, &Default::default())?;
    let replica_id = db.meta().replica_id;

    // Bind before announcing, so the printed line carries the final port.
    let listener = match &opts.listen {
        Some(addr) => Some(TcpListener::bind(addr.as_deref().unwrap_or("127.0.0.1:0"))?),
        None => None,
    };
    match &listener {
        Some(l) => eprintln!("replica {replica_id:016x} (tcp:{})", l.local_addr()?),
        None => eprintln!("replica {replica_id:016x}"),
    }
    if opts.verbose {
        eprintln!("root hash: {}", root_hex(&db)?);
    }

    // The REPL and the sync server run concurrently, so the database
    // handle is shared behind a mutex: the server locks it per session,
    // the REPL per command.
    let db: SharedDb = Arc::new(Mutex::new(db));
    let server = listener.map(|listener| {
        let db = db.clone();
        let verbose = opts.verbose;
        std::thread::spawn(move || serve_loop(listener, db, verbose))
    });

    if let Some((cmd, args)) = command.split_first() {
        let code = {
            let db = db.lock().unwrap();
            run_command(opts, &db, cmd, args)?
        };
        if code == 2 {
            eprintln!("{}", usage());
        }
        if let Some(server) = server {
            let _ = server.join(); // keep serving until interrupted
        }
        Ok(code)
    } else {
        repl(opts, &db)
    }
}

/// Interactive mode: one command per line until EOF (or exit/quit).
fn repl(opts: &Opts, db: &SharedDb) -> Result<i32, AnyError> {
    println!("Interactive mode (ls, set, get, rm or help)");
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            println!(); // EOF: leave the shell on a fresh line
            return Ok(0);
        }
        let tokens: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        let Some((cmd, args)) = tokens.split_first() else {
            continue;
        };
        match cmd.as_str() {
            "help" => println!("Available commands:\n{COMMANDS}"),
            "exit" | "quit" => return Ok(0),
            _ => {
                let result = {
                    let db = db.lock().unwrap();
                    run_command(opts, &db, cmd, args)
                };
                match result {
                    Ok(0) => {}
                    Ok(1) => eprintln!("not found"),
                    Ok(_) => println!("Unknown command \"{cmd}\". Available commands:\n{COMMANDS}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
    }
}

fn run_command(
    opts: &Opts,
    db: &Db<RedbStorage>,
    cmd: &str,
    args: &[String],
) -> Result<i32, AnyError> {
    match (cmd, args) {
        ("ls", args) => {
            let (long, args) = match args.split_first() {
                Some((flag, rest)) if flag == "-l" => (true, rest),
                _ => (false, args),
            };
            if args.len() > 1 {
                return Ok(2);
            }
            let prefix = args.first().map(|s| s.as_bytes()).unwrap_or(b"");
            let txn = db.begin_read()?;
            for item in txn.iter_from(prefix) {
                let (key, value) = item?;
                if !key.starts_with(prefix) {
                    break;
                }
                if long {
                    println!(
                        "{}\t{}",
                        String::from_utf8_lossy(&key),
                        String::from_utf8_lossy(&value)
                    );
                } else {
                    println!("{}", String::from_utf8_lossy(&key));
                }
            }
            Ok(0)
        }
        ("set", [key, value]) => {
            let mut txn = db.begin_write(wall_secs())?;
            txn.put(key.as_bytes(), value.as_bytes())?;
            if opts.verbose {
                eprintln!("clock: {:?}", txn.clock());
            }
            txn.commit()?;
            if opts.verbose {
                eprintln!("root hash: {}", root_hex(db)?);
            }
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
            let existed = txn.del(key.as_bytes())?; // writes a tombstone
            txn.commit()?;
            if opts.verbose {
                eprintln!("root hash: {}", root_hex(db)?);
            }
            Ok(if existed { 0 } else { 1 })
        }
        ("pull", [url]) => pull(opts, db, url),
        _ => Ok(2),
    }
}

/// Pull changes from a remote replica and merge them (last-writer-wins).
/// Note: in a `--listen` session, pulling from this process's own address
/// would deadlock (the server thread waits for the lock the puller holds).
fn pull(opts: &Opts, db: &Db<RedbStorage>, url: &str) -> Result<i32, AnyError> {
    let addr = url
        .strip_prefix("tcp:")
        .ok_or("unsupported <replica-url>; expected \"tcp:<host>:<port>\"")?;
    let mut stream = TcpStream::connect(addr)?;

    let mut out = Vec::new();
    use std::os::unix::fs::OpenOptionsExt;
    let name = format!(
        "kvdb-sync-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    );
    let path = std::env::temp_dir().join(name);
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    std::fs::remove_file(path)?;
    let mut rx = SyncReceiver::new(db, Lww, FileStaging::new(file)?, wall_secs(), &mut out)?;
    let mut buf = [0u8; 4096];
    loop {
        if !out.is_empty() {
            stream.write_all(&out)?;
            out.clear();
        }
        let n = stream.read(&mut buf)?;
        let status = rx.process(&buf[..n], &mut out)?;
        if status == SyncStatus::Done {
            stream.write_all(&out)?; // deliver the final Bye
            break;
        }
        if n == 0 {
            return Err("connection closed mid-sync".into());
        }
    }
    let outcome = rx.finish()?;

    use kvdb::sync::SyncEnd;
    match outcome.end {
        SyncEnd::NoDifference => println!("already in sync"),
        SyncEnd::RemoteEmpty => println!("remote database is empty; nothing to pull"),
        SyncEnd::Merged => {
            println!("pulled: {} of {} differences applied", outcome.applied, outcome.deltas)
        }
    }
    if opts.verbose {
        eprintln!("root hash: {}", root_hex(db)?);
    }
    Ok(0)
}

/// Serve sync sessions over TCP: each connection is answered from a fresh
/// read snapshot, so a `pull` always sees the latest committed state.
fn serve_loop(listener: TcpListener, db: SharedDb, verbose: bool) {
    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                if verbose && let Ok(peer) = stream.peer_addr() {
                    eprintln!("sync request from {peer}");
                }
                let db = db.lock().unwrap();
                if let Err(e) = serve_one(&db, &mut stream) {
                    eprintln!("sync failed: {e}");
                }
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}

fn serve_one(db: &Db<RedbStorage>, stream: &mut TcpStream) -> Result<(), AnyError> {
    let mut tx = SyncSender::new(db)?;
    let mut buf = [0u8; 4096];
    let mut out = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        let status = tx.process(&buf[..n]);
        loop {
            let emitted = tx.write_output(&mut out)?;
            if emitted == 0 {
                break;
            }
            stream.write_all(&out[..emitted])?;
        }
        let status = status?;
        if status == SyncStatus::Done {
            return Ok(());
        }
        if n == 0 {
            return Err("connection closed mid-sync".into());
        }
    }
}

fn root_hex(db: &Db<RedbStorage>) -> Result<String, AnyError> {
    Ok(match db.begin_read()?.root_hash()? {
        Some((level, hash)) => {
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            format!("{hex} (level {level})")
        }
        None => "(empty database)".to_string(),
    })
}

fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Random-enough id for an example program, without extra dependencies:
/// std's hash-map hasher is randomly seeded per process.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}
