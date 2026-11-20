//! An independently owned provider replica. App and extension never open the
//! same profile. Local calls borrow a gated weak store; enrollment drains those
//! leases before binding, and network waits never retain an enrollment lease.
use super::{BlobTransport, Response, execute_request};
use crate::engine::{Result, connected::Connected};
use crate::store::{Id, LocalStore};
use parking_lot::RwLock;
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Weak},
};

pub struct Replica {
    pub connected: Connected,
    local: Local,
}

#[derive(Clone)]
pub struct Local {
    store: Arc<RwLock<Weak<LocalStore>>>,
    blobs: BlobTransport,
    mutations: Arc<crate::engine::connected::ProviderMutations>,
}

impl Replica {
    /// No network I/O. A second process opening this profile fails its existing
    /// exclusive profile lock; a killed owner releases that lock in the kernel.
    pub fn open(path: &Path, wake: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let mut connected = Connected::open(path, wake)?;
        connected.provider_only();
        let references = path.join("provider-references");
        match std::fs::remove_dir_all(&references) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::create_dir(&references)?;
        let store = connected.services.provider_store.clone();
        *store.write() = Arc::downgrade(&connected.services.store);
        let local = Local {
            store,
            blobs: connected.blob_transport(),
            mutations: connected.provider_mutations.clone(),
        };
        Ok(Self { connected, local })
    }

    pub fn local(&self) -> Local {
        self.local.clone()
    }

    /// Initialization/reconnect work belongs to the network lane. Cached local
    /// calls continue independently. This is never called to refresh each read.
    pub fn connect(&mut self, account: Id, email: &str, secret: &[u8; 16]) -> Result<()> {
        self.connected.enroll_provider(account, email, secret)?;
        self.connected.synchronize()?;
        // Publication intent is durable. Failed background delivery never
        // invalidates successful account discovery or admits a full client.
        if let Err(error) = self.connected.flush_provider() {
            crate::diagnostic!("Provider publication remains pending: {error}");
        }
        Ok(())
    }
}

impl Local {
    pub fn execute(&self, bytes: &[u8]) -> Result<Value> {
        let lease = self.store.read();
        let store = lease.upgrade().ok_or("File Provider store is closed")?;
        if bytes.len() > 65_536 {
            return Err("provider request exceeds limit".into());
        }
        let request: Value = serde_json::from_slice(bytes)?;
        let mutation = matches!(
            request["op"].as_str(),
            Some("create" | "modify" | "delete" | "rename_volume" | "create_volume")
        );
        let _mutation = mutation.then(|| self.mutations.begin());
        if mutation
            && request.get("account").is_none_or(|account| {
                account.as_str() == Some(store.account().id.to_string().as_str())
            })
            && let Some(volume) = request.get("volume")
        {
            store.provider_enqueue_publication(serde_json::from_value(volume.clone())?)?;
        }
        let response = execute_request(&store, &self.blobs, bytes, true);
        drop(store);
        drop(lease);
        // AP registration can wait on the transport. Binding must not wait for it.
        response.and_then(|response: Response| response.finish(&self.blobs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_provider_resources_allow_enrollment_and_retire_with_owner() {
        // Exercise the independent host composition with permanent local handles,
        // then reopen its profile to verify durable binding and lock release.
        let directory = tempfile::tempdir().expect("profile");
        let mut replica = Replica::open(directory.path(), || {}).expect("replica");
        let local = replica.local();
        let before = replica.connected.services.store.account().id;
        let after = Id::generate().expect("account");
        replica.connected.services.bind_authenticated_account(after).expect("bind");
        let request =
            serde_json::to_vec(&json!({"op":"volumes", "account":after})).expect("request");
        assert_eq!(local.execute(&request).expect("catalog")["account"], json!(after));
        let stale = serde_json::to_vec(&json!({"op":"volumes", "account":before})).expect("stale");
        assert!(
            local.execute(&stale).expect_err("stale identity").to_string().contains("signed in")
        );
        assert!(Replica::open(directory.path(), || {}).is_err());
        drop(replica);
        assert!(local.execute(&request).is_err());
        let reopened = Replica::open(directory.path(), || {}).expect("reopened");
        assert_eq!(reopened.connected.services.store.account().id, after);
    }
    #[test]
    fn provider_admission_is_bounded_and_durable_local_intent_survives_selection_changes() {
        // Cached writes do not wait for network admission. Switching the two
        // admitted network targets must retain every local publication source.
        let directory = tempfile::tempdir().expect("profile");
        let mut replica = Replica::open(directory.path(), || {}).expect("replica");
        let store = &replica.connected.services.store;
        let first = store.create_volume("First", true).expect("first");
        let second = store.create_volume("Second", true).expect("second");
        let third = store.create_volume("Third", true).expect("third");
        let account = store.account().id;
        replica
            .connected
            .select_provider_volumes(&[first.id, second.id])
            .expect("two admitted targets");
        assert!(
            replica.connected.select_provider_volumes(&[first.id, second.id, third.id]).is_err()
        );
        assert!(replica.connected.select_provider_volumes(&[first.id, first.id]).is_err());
        let request = serde_json::to_vec(&json!({"op":"create", "account":account,
            "volume":third.id,"id":crate::store::EntryId::generate().expect("entry"),
            "parent":null,"name":"Offline","directory":true}))
        .expect("request");
        replica.local().execute(&request).expect("cached operation independent of admission");
        replica.connected.select_provider_volumes(&[second.id]).expect("switch target");
        let pending =
            replica.connected.services.store.provider_pending_publications().expect("intents");
        assert!(pending.iter().any(|(volume, _)| *volume == third.id));
        assert!(replica.local.mutations.idle_epoch().is_some());
        drop(replica);
        let reopened = Replica::open(directory.path(), || {}).expect("reopened");
        let recovered = reopened
            .connected
            .services
            .store
            .provider_pending_publications()
            .expect("recovered intents");
        assert_eq!(recovered, pending);
    }

    #[test]
    fn a_commit_finishing_after_the_drain_capture_keeps_its_publication_intent() {
        // Pause the real provider file write after its marker and before index
        // publication. A drain's old capture cannot acknowledge the later commit,
        // including a mutation which starts and finishes during that drain.
        let directory = tempfile::tempdir().expect("profile");
        let replica = Replica::open(directory.path(), || {}).expect("replica");
        let store = replica.connected.services.store.clone();
        let volume = store.create_volume("Files", true).expect("volume");
        let source = directory.path().join("source");
        std::fs::write(&source, b"committed after capture").expect("content");
        let entry = crate::store::EntryId::generate().expect("entry");
        let local = replica.local();
        let before_admission = local.mutations.idle_epoch();
        let (ready, observed) = std::sync::mpsc::sync_channel(1);
        let (release, commit) = std::sync::mpsc::sync_channel(1);
        let commit = parking_lot::Mutex::new(commit);
        store
            .set_blob_publication_hook(Some(Arc::new(move || {
                ready.send(()).expect("drain observes prepared write");
                commit
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("release commit");
                Ok(())
            })))
            .expect("hook");
        let request = serde_json::to_vec(&json!({"op":"create", "account":store.account().id,
            "volume":volume.id,"id":entry,"parent":null,"name":"late.bin",
            "directory":false,"path":source}))
        .expect("request");
        let writer = local.clone();
        let worker = std::thread::spawn(move || writer.execute(&request));
        observed.recv_timeout(std::time::Duration::from_secs(5)).expect("write prepared");
        let pending = store.provider_pending_publications().expect("precommit intent");
        let during_admission = local.mutations.idle_epoch();
        let captured = serde_json::to_value(store.provider_anchor(volume.id).expect("old source"))
            .expect("anchor");
        release.send(()).expect("finish local commit");
        worker.join().expect("writer").expect("committed");
        store.set_blob_publication_hook(None).expect("clear hook");
        assert!(local.mutations.idle_epoch().is_some());
        assert!(!local.mutations.unchanged(before_admission));
        assert!(!local.mutations.unchanged(during_admission));
        assert_ne!(
            captured,
            serde_json::to_value(store.provider_anchor(volume.id).expect("new source"))
                .expect("anchor")
        );
        assert_eq!(store.provider_pending_publications().expect("retained intent"), pending);
        let stable = local.mutations.idle_epoch();
        assert!(local.mutations.unchanged(stable));
        store.provider_ack_publication(volume.id, pending[0].1).expect("later stable drain");
        assert!(store.provider_pending_publications().expect("acknowledged").is_empty());
    }

    #[test]
    fn stop_interrupts_key_authentication_before_the_ap_replies() {
        // Activity expiry must release profile locks without waiting for the
        // authentication timeout of an unpublished, half-open AP connection.
        let directory = tempfile::tempdir().expect("profile");
        let mut replica = Replica::open(directory.path(), || {}).expect("replica");
        let (config, observed, gate) = crate::network::ap::testing::held_authentication_server();
        replica.connected.set_test_transport(config).expect("private test transport");
        let stop = replica.connected.stop_handle();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = replica.connect(
                Id::generate().expect("account"),
                "provider@example.invalid",
                &[7; 16],
            );
            send.send(result.map_err(|error| error.to_string())).expect("result");
            drop(replica);
        });
        observed.recv_timeout(std::time::Duration::from_secs(5)).expect("AP holds authentication");
        stop.request();
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("stop returns before AP reply")
                .is_err()
        );
        worker.join().expect("owner closed");
        let _reopened = Replica::open(directory.path(), || {}).expect("profile lock released");
        gate.open();
    }

    #[test]
    fn process_enrollment_and_committed_write_survive_owner_termination() {
        // Run a real provider owner beside a separate app profile. Enroll with
        // permanent local handles, commit bytes, kill the owner, and recover.
        use std::{
            io::{BufRead, Read, Write},
            process::{Command, Stdio},
        };
        const CHILD: &str = "PLAYBIT_PROVIDER_LIFECYCLE_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::path::PathBuf::from(path);
            let mut replica = Replica::open(&path, || {}).expect("child replica");
            let local = replica.local();
            let account = Id::generate().expect("authenticated account");
            replica
                .connected
                .services
                .bind_authenticated_account(account)
                .expect("bind with provider alive");
            let volume =
                replica.connected.services.store.create_volume("Files", true).expect("volume");
            let entry = crate::store::EntryId::generate().expect("entry");
            let source = path.join("source");
            std::fs::write(&source, b"durable provider bytes\0\xff").expect("content");
            let request = serde_json::to_vec(&json!({"op":"create", "account":account,
                "volume":volume.id, "id":entry, "parent":null, "name":"exact.bin", "directory":false, "path":source})).expect("create request");
            local.execute(&request).expect("durable create");
            println!(
                "PROVIDER_READY {}",
                json!({"account":account,"volume":volume.id,"entry":entry})
            );
            std::io::stdout().flush().expect("ready");
            let _ = std::io::stdin().read(&mut [0]);
            return;
        }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().expect("profiles");
        let app = Connected::open(directory.path().join("app"), || {}).expect("app owner");
        let profile = directory.path().join("provider");
        let mut child = Child(Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "engine::file_provider::replica::tests::process_enrollment_and_committed_write_survive_owner_termination", "--nocapture"])
            .env(CHILD, &profile).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("provider process"));
        let mut output = std::io::BufReader::new(child.0.stdout.take().expect("output"));
        let mut identity = None;
        for _ in 0..16 {
            let mut line = String::new();
            if output.read_line(&mut line).expect("child output") == 0 {
                break;
            }
            if let Some(value) = line.trim().strip_prefix("PROVIDER_READY ") {
                identity = Some(serde_json::from_str::<Value>(value).expect("identity"));
                break;
            }
        }
        let identity = identity.expect("child committed before readiness");
        assert!(Replica::open(&profile, || {}).is_err(), "another process cannot own this store");
        assert_ne!(json!(app.services.store.account().id), identity["account"]);
        child.0.kill().expect("terminate provider");
        child.0.wait().expect("wait for kernel lock release");
        let replica = Replica::open(&profile, || {}).expect("recover provider");
        assert_eq!(json!(replica.connected.services.store.account().id), identity["account"]);
        let item = replica
            .local()
            .execute(
                &serde_json::to_vec(&json!({"op":"item", "account":identity["account"],
            "volume":identity["volume"], "entry":identity["entry"]}))
                .expect("item request"),
            )
            .expect("recovered item");
        let destination = profile.join("hydrated");
        replica.local().execute(&serde_json::to_vec(&json!({"op":"fetch", "account":identity["account"],
            "volume":identity["volume"], "entry":identity["entry"], "version":item["item"]["content_version"], "path":destination})).expect("fetch request")).expect("cached fetch offline");
        assert_eq!(std::fs::read(destination).expect("hydration"), b"durable provider bytes\0\xff");
        assert!(!replica.connected.services.store.pending_operations().expect("outbox").is_empty());
    }
}
