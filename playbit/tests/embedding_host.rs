//! A small trusted embedding UI, also compiled independently by
//! platform/native/build-engine-host.sh to exclude test feature unification.
use playbit::{
    engine::Services,
    store::{
        Access, Id, Result,
        change_feed::CommittedChangeReceiver,
        object_contract::{FieldDeclaration, Schema, TypeDeclaration, Value, ValueType},
        volume_index::{EntryAddress, EntryKind},
    },
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const TYPE: Id = Id::from_bytes([1; 16]);
const DONE: Id = Id::from_bytes([2; 16]);
const VOLUME_NAME: &str = "Native embedding fixture";

pub(crate) struct Host {
    services: Services,
    changes: CommittedChangeReceiver,
    pending: Arc<AtomicBool>,
    access: Access,
    address: EntryAddress,
    object: Id,
}

impl Host {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_with_wake(path, || {})
    }

    pub(crate) fn open_with_wake(
        path: &Path,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let pending = Arc::new(AtomicBool::new(false));
        let wake = pending.clone();
        // This fixture has only synchronous owner-thread writers. The flag
        // coalesces readiness; an optional UI wake schedules a later turn.
        let services = Services::open(path, move || {
            wake.store(true, Ordering::Release);
            notify();
        })?;
        let changes =
            services.store.take_change_receiver().expect("new services have one consumer");
        assert!(changes.readiness().is_none(), "embedding must not allocate a wake socket");
        let access = Access::owner(services.store.account().id);
        let (address, object) = fixture(&services, &access)?;
        Ok(Self { services, changes, pending, access, address, object })
    }

    pub(crate) fn value(&self) -> Result<bool> {
        let object = self.services.store.read_object(&self.access, self.address, self.object)?;
        match object.fields.get(&DONE).and_then(|cell| cell.values.as_deref()) {
            Some([Value::Boolean(done)]) => Ok(*done),
            _ => Err(playbit::store::Error::corrupt("unexpected embedding host result")),
        }
    }

    pub(crate) fn toggle(&self) -> Result<()> {
        let changes = BTreeMap::from([(DONE, Some(vec![Value::Boolean(!self.value()?)]))]);
        self.services
            .store
            .edit_fields(&self.access, self.address, self.object, &changes)
            .map(|_| ())
    }

    pub(crate) fn advance(&mut self) -> Result<usize> {
        // Clear before draining: the feed rearms if this turn leaves records.
        if !self.pending.swap(false, Ordering::AcqRel) {
            return Ok(0);
        }
        let batch = self.changes.drain();
        if batch.status.gap_generation != 0 || batch.status.closed.is_some() {
            return Err(playbit::store::Error::System(format!("change feed: {:?}", batch.status)));
        }
        Ok(batch.records.len())
    }
}

fn fixture(services: &Services, access: &Access) -> Result<(EntryAddress, Id)> {
    let store = &services.store;
    let volumes = store.volumes()?;
    let volume = match volumes.as_slice() {
        [] => store.create_volume(VOLUME_NAME, false)?.id,
        [volume] if volume.name == VOLUME_NAME && !volume.share_key_with_service => volume.id,
        _ => return Err(playbit::store::Error::InvalidInput("use a dedicated fixture profile")),
    };
    let wanted = Schema {
        types: BTreeMap::from([(
            TYPE,
            TypeDeclaration {
                name: "Item".into(),
                deprecated: false,
                fields: BTreeMap::from([(
                    DONE,
                    FieldDeclaration {
                        name: "done".into(),
                        value_type: ValueType::Boolean,
                        repeated: false,
                        deprecated: false,
                    },
                )]),
            },
        )]),
    };
    let entry = match store.resolve_entry(access, volume, "data") {
        Ok(entry) => entry,
        Err(playbit::store::Error::NotFound(_)) => store.create_database(
            access,
            volume,
            "data",
            EntryKind::ObjectDb { schema: wanted.clone() },
        )?,
        Err(error) => return Err(error),
    };
    let address = EntryAddress { volume, entry: entry.id };
    if store.schema(access, address)? != wanted {
        return Err(playbit::store::Error::InvalidInput("fixture schema differs"));
    }
    let objects = store.query_objects(access, address, TYPE, None, 2)?;
    let object = match objects.objects.as_slice() {
        [] => {
            store
                .create_object_with_fields(
                    access,
                    address,
                    TYPE,
                    &BTreeMap::from([(DONE, Some(vec![Value::Boolean(false)]))]),
                )?
                .object_id
        }
        [record] => record.object_id,
        _ => return Err(playbit::store::Error::InvalidInput("fixture needs exactly one item")),
    };
    Ok((address, object))
}

fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("usage: engine-host PROFILE; commands: t, q")?;
    if args.next().is_some() {
        return Err("expected one profile directory".into());
    }
    let start = std::time::Instant::now();
    let mut host = Host::open(Path::new(&path))?;
    println!(
        "Local engine host: pid={} open_us={} done={}",
        std::process::id(),
        start.elapsed().as_micros(),
        host.value()?
    );
    println!("t + Return: toggle; q + Return: quit. Edits commit before display.");
    let mut input = std::io::stdin().lock();
    let mut byte = [0];
    loop {
        let delivered = host.advance()?;
        if delivered > 0 {
            println!("committed_changes={delivered} done={}", host.value()?);
        }
        if host.pending.load(Ordering::Acquire) {
            continue; // Another bounded, already-ready turn; never freshness polling.
        }
        std::io::stdout().flush()?;
        if input.read(&mut byte)? == 0 {
            break;
        }
        match byte[0] {
            b't' => host.toggle()?,
            b'q' => break,
            b'\n' | b'\r' => {}
            _ => eprintln!("Use t to toggle or q to quit."),
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("engine-host: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use playbit::store::change_feed::FeedClosure;

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("playbit-embedding-{}", Id::generate().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn supplied_ui_wake_rearms_after_each_bounded_drain() {
        // Exercise the optional GUI wake with the actual coalesced store feed.
        let directory = Directory::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let mut host = Host::open_with_wake(&directory.0, move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert!(calls.load(Ordering::Relaxed) > 0);
        while host.pending.load(Ordering::Acquire) {
            host.advance().unwrap();
        }
        for _ in 0..2 {
            calls.store(0, Ordering::Relaxed);
            host.toggle().unwrap();
            assert!(calls.load(Ordering::Relaxed) > 0);
            assert_eq!(host.advance().unwrap(), 1);
            assert!(!host.pending.load(Ordering::Acquire));
        }
    }

    #[test]
    fn independent_hosts_commit_notify_reopen_and_retire_pending_work() {
        // Use only the public embedding API. Retain notifications past source
        // close, then reopen the same profile with a distinct feed epoch.
        let first = Directory::new();
        let second = Directory::new();
        let mut a = Host::open(&first.0).unwrap();
        let mut b = Host::open(&second.0).unwrap();
        assert_ne!(a.services.store.account().id, b.services.store.account().id);
        assert_eq!(a.services.profile_path(), first.0);
        assert!(a.advance().unwrap() > 0);
        assert!(b.advance().unwrap() > 0);
        assert!(!a.value().unwrap());
        assert!(!b.value().unwrap());
        a.toggle().unwrap();
        assert!(a.pending.load(Ordering::Acquire));
        assert!(!b.pending.load(Ordering::Acquire));
        assert_eq!(a.advance().unwrap(), 1);
        assert!(a.value().unwrap());
        assert!(!b.value().unwrap());
        let Host { services, mut changes, pending, .. } = a;
        let epoch = services.store.change_status().epoch;
        drop(services);
        assert!(pending.load(Ordering::Acquire));
        assert_eq!(changes.drain().status.closed, Some(FeedClosure::SourceDropped));
        let reopened = Host::open(&first.0).unwrap();
        assert!(reopened.value().unwrap());
        assert_ne!(reopened.services.store.change_status().epoch, epoch);
        assert!(!first.0.join("control/engine.sock").exists());
        assert!(!second.0.join("control/engine.sock").exists());
        // A fresh committed change remains queued when this host is closed.
        b.toggle().unwrap();
        assert!(b.pending.load(Ordering::Acquire));
        drop(b);
        assert!(Host::open(&second.0).unwrap().value().unwrap());
    }
}
