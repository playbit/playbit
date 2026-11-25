#![allow(clippy::unwrap_used, reason = "Tests assert durable filesystem outcomes.")]
use super::*;

#[test]
fn isolated_profiles_cannot_adopt_system_file_provider_domains() {
    // Bundle-wide OS domains never acquire the authority of a custom profile,
    // whether it is Public or Testing. The local provider server is separate.
    let root = tempfile::tempdir().unwrap();
    assert!(require_system_profile(root.path()).is_err());
    std::fs::write(root.path().join("channel"), b"testing").unwrap();
    assert!(require_system_profile(root.path()).is_err());
}
use crate::store::volume_index::EntryKind;

fn enroll_socket_profile(profile: &Path, store: &LocalStore, account: Id) {
    let mut identity = crate::network::ap::ClientIdentity::from_seed(
        "provider@example.invalid",
        "Provider socket fixture",
        store.account_seed().unwrap(),
    )
    .unwrap();
    identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
    identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.save(&profile.join("instance.json")).unwrap();
}

fn request(store: &Arc<LocalStore>, value: Value) -> Value {
    execute(store, &BlobTransport::local(), &serde_json::to_vec(&value).unwrap()).unwrap()
}
fn metadata(store: &Arc<LocalStore>, volume: Id, id: EntryId) -> Value {
    request(store, json!({"op":"item","volume":volume,"entry":id}))["item"].clone()
}

#[test]
fn account_domain_root_volume_creation_and_composite_entry_ids_round_trip() {
    // Exercise the actual JSON boundary from an empty account through volume
    // creation, rename replay, independent entries and permanent failures.
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let root = request(&store, json!({"op":"root"}));
    assert_eq!(root["item"]["role"], "root");
    assert_eq!(root["item"]["filename"], "Playbit");
    let empty = request(&store, json!({"op":"volumes"}));
    assert_eq!(empty["volumes"], json!([]));
    assert_eq!(request(&store, json!({"op":"catalog"}))["items"], json!([]));
    assert_eq!(empty["provider_ready"], false);
    let first = Id::generate().unwrap();
    let second = Id::generate().unwrap();
    let create = json!({"op":"create_volume", "id":first, "name":"Documents"});
    let created = request(&store, create.clone())["item"].clone();
    assert_eq!(created["id"], json!(first));
    assert_eq!(created["role"], "volume");
    assert_eq!(created["uploaded"], false);
    assert!(store.provider_pending_publications().unwrap().is_empty());
    request(
        &store,
        json!({"op":"rename_volume", "volume":first, "name":"Work", "metadata":created["metadata_version"]}),
    );
    assert_eq!(request(&store, create)["item"]["filename"], "Work");
    request(&store, json!({"op":"create_volume", "id":second, "name":"Personal"}));
    let id = EntryId::generate().unwrap();
    for volume in [first, second] {
        request(
            &store,
            json!({"op":"create", "volume":volume, "id":id, "name":"Folder", "directory":true}),
        );
        let item = metadata(&store, volume, id);
        assert_eq!(item["parent"], json!(volume));
        assert_eq!(
            item["id"],
            json!(crate::store::file_provider::provider_entry_identifier(volume, id))
        );
    }
    assert_ne!(metadata(&store, first, id)["id"], metadata(&store, second, id)["id"]);
    let invalid = execute(&store, &BlobTransport::local(), &serde_json::to_vec(&json!({
        "op":"rename_volume", "volume":first, "name":"Stale", "metadata":created["metadata_version"]
    })).unwrap()).unwrap_err();
    assert_eq!(error_response(invalid.as_ref())["code"], "version_changed");
    let stale = execute(
        &store,
        &BlobTransport::local(),
        &serde_json::to_vec(&json!({
            "op":"root", "account": Id::generate().unwrap()
        }))
        .unwrap(),
    )
    .unwrap_err();
    assert_eq!(error_response(stale.as_ref())["code"], "permission_denied");
}

#[test]
fn retained_store_owner_excludes_rebinding_and_provider_recovers_after_rejection() {
    // A retained host owner blocks rebinding. Real workers follow the bound
    // identity, while keys, invitations and pending writes survive failed retries.
    let directory = tempfile::tempdir().unwrap();
    let mut services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let original = services.store.account().id;
    let seed = services.store.account_seed().unwrap();
    let invitation = services
        .store
        .preview_invitation(
            &crate::store::session_link(&[9; 16], crate::release_channel::ReleaseChannel::Public),
            crate::release_channel::ReleaseChannel::Public,
        )
        .unwrap();
    enroll_socket_profile(directory.path(), &services.store, services.store.account().id);
    let server = Server::start(directory.path(), &services, BlobTransport::local(), || {}).unwrap();
    let active = services.store.clone();
    let authenticated = Id::generate().unwrap();
    services.store.protect_account_key(authenticated, &[7; 16]).unwrap();
    assert!(services.bind_authenticated_account(authenticated).is_err());
    assert_eq!(services.store.prepared_account_key(authenticated).unwrap(), Some([7; 16]));
    assert_eq!(active.account().id, original);
    let volumes = || -> Value {
        serde_json::from_slice(&call(directory.path(), br#"{"op":"volumes"}"#).unwrap()).unwrap()
    };
    assert_eq!(volumes()["account"], json!(original));
    drop(active);
    let mut pending = UnixStream::connect(directory.path().join(SOCKET)).unwrap();
    pending.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let stale = serde_json::to_vec(&json!({"op":"volumes", "account":original})).unwrap();
    pending.write_all(&(stale.len() as u32).to_le_bytes()).unwrap();
    services.bind_authenticated_account(authenticated).unwrap();
    enroll_socket_profile(directory.path(), &services.store, authenticated);
    pending.write_all(&stale).unwrap();
    let mut size = [0; 4];
    pending.read_exact(&mut size).unwrap();
    let mut reply = vec![0; u32::from_le_bytes(size) as usize];
    pending.read_exact(&mut reply).unwrap();
    let reply: Value = serde_json::from_slice(&reply).unwrap();
    assert!(reply["error"].as_str().unwrap().contains("no longer signed in"));
    assert_eq!(volumes()["account"], json!(authenticated));
    let volume = services.store.create_volume("Retained files", true).unwrap();
    let pending = services.store.pending_operations().unwrap();
    assert!(!pending.is_empty());
    assert!(services.bind_authenticated_account(original).is_err());
    assert_eq!(volumes()["account"], json!(authenticated));
    assert_eq!(volumes()["volumes"][0]["id"], json!(volume.id));
    drop(server);
    drop(services);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert_eq!(reopened.account().id, authenticated);
    assert_eq!(reopened.account_seed().unwrap(), seed);
    assert_eq!(reopened.prepared_account_key(authenticated).unwrap(), Some([7; 16]));
    assert_eq!(reopened.pending_operations().unwrap(), pending);
    assert_eq!(reopened.pending_invitations().unwrap(), [invitation]);
}

#[test]
fn catalog_and_persistent_anchors_track_namespace_without_backing_edits() {
    // Backing edits and append receipts advance the shared observation frontier
    // without returning a fresh Finder catalog or changing its persistent anchor.
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Documents", true).unwrap().id;
    let database = store.create_database(&access, volume, "Settings", EntryKind::KvDb {}).unwrap();
    let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
    let first = request(&store, json!({"op":"volumes","volume":volume}));
    let anchor = request(&store, json!({"op":"anchor","volume":volume}));
    assert_eq!(first["volumes"][0]["id"], json!(volume));
    assert_eq!(first["anchor"], anchor["anchor"]);
    assert!(store.provider_cached_anchor(Id::generate().unwrap()).unwrap().is_none());
    store.kv_put(&access, EntryAddress { volume, entry: database.id }, b"key", b"value").unwrap();
    let backing = request(&store, json!({"op":"volumes","after":first["revision"]}));
    assert!(backing["volumes"].is_null());
    assert_eq!(backing["changed"], json!([]));
    assert_ne!(backing["revision"], first["revision"]);
    assert_eq!(request(&store, json!({"op":"anchor","volume":volume})), anchor);
    store.bloblog_put(&access, volume, log, &[b"retained".to_vec()]).unwrap();
    let page = store.bloblog_get(&access, volume, log, None).unwrap();
    let appended = request(&store, json!({"op":"volumes","after":backing["revision"]}));
    assert!(appended["volumes"].is_null());
    store.bloblog_set_checkpoint(&access, volume, log, &page.cursor).unwrap();
    let checkpoint = request(&store, json!({"op":"volumes","after":appended["revision"]}));
    assert!(checkpoint["volumes"].is_null());
    assert_eq!(checkpoint["changed"], json!([]));
    assert_ne!(checkpoint["revision"], appended["revision"]);
    assert_eq!(request(&store, json!({"op":"anchor","volume":volume})), anchor);
    store.rename_entry(&access, volume, database.id, "Renamed").unwrap();
    let renamed = request(&store, json!({"op":"volumes","after":backing["revision"]}));
    assert!(renamed["volumes"].is_array());
    assert_eq!(renamed["changed"], json!([volume]));
    let renamed_anchor = request(&store, json!({"op":"anchor","volume":volume}));
    assert_ne!(renamed_anchor, anchor);
    let added = store.create_volume("New drive", false).unwrap().id;
    let membership = request(&store, json!({"op":"volumes","after":renamed["revision"]}));
    assert_eq!(membership["volumes"].as_array().unwrap().len(), 2);
    assert!(membership["volumes"].as_array().unwrap().iter().any(|v| v["id"] == json!(added)));
    store.delete_volume(added).unwrap();
    let removed = request(&store, json!({"op":"volumes","after":membership["revision"]}));
    assert_eq!(removed["volumes"].as_array().unwrap().len(), 1);
    assert_eq!(removed["volumes"][0]["id"], json!(volume));
    let known =
        request(&store, json!({"op":"volumes","after":removed["revision"],"known":[added]}));
    assert!(known["volumes"].is_null());
    assert_eq!(known["retired"], json!([added]));
    drop(store);
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    assert_eq!(request(&store, json!({"op":"anchor","volume":volume})), renamed_anchor);
    let baseline = store.delivery_provider_revision(None, None).0.unwrap();
    let idle =
        request(&store, json!({"op":"volumes","after":baseline,"known":[added],"volume":volume}));
    assert!(idle["volumes"].is_null());
    assert_eq!(idle["anchor"], renamed_anchor["anchor"]);
    assert_eq!(idle["changed"], json!([]));
    assert_eq!(idle["retired"], json!([added]));
}

#[test]
fn filesystem_roundtrip_moves_uploads_and_database_copies_are_durable_offline() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let store = Arc::new(LocalStore::open(&profile).unwrap());
    let volume = store.create_volume("Documents", false).unwrap().id;
    let access = Access::owner(store.account().id);
    let folder = EntryId::generate().unwrap();
    request(
        &store,
        json!({"op":"create","volume":volume,"id":folder,"name":"Folder","directory":true}),
    );
    let path = directory.path().join("input");
    fs::write(&path, b"first").unwrap();
    let file = EntryId::generate().unwrap();
    let created = request(&store, json!({"op":"create","volume":volume,"id":file,"name":"hello.txt","directory":false,"path":path}))["item"].clone();
    fs::write(&path, b"edited in Finder").unwrap();
    request(
        &store,
        json!({"op":"modify","volume":volume,"entry":file,"parent":folder,"name":"moved.txt",
        "version":created["content_version"],"metadata":created["metadata_version"],"path":path}),
    );
    let current = metadata(&store, volume, file);
    assert_eq!(
        current["parent"],
        json!(crate::store::file_provider::provider_entry_identifier(volume, folder))
    );
    assert_eq!(current["filename"], "moved.txt");
    let destination = directory.path().join("materialized");
    request(
        &store,
        json!({"op":"fetch","volume":volume,"entry":file,"version":current["content_version"],"path":destination}),
    );
    assert_eq!(fs::read(destination).unwrap(), b"edited in Finder");
    let database = store.create_database(&access, volume, "Settings", EntryKind::KvDb {}).unwrap();
    let source = EntryAddress { volume, entry: database.id };
    store.kv_put(&access, source, b"key", b"retained value").unwrap();
    let pointer = directory.path().join("Settings.pbdb");
    let db_item = metadata(&store, volume, database.id);
    request(
        &store,
        json!({"op":"fetch","volume":volume,"entry":database.id,"version":db_item["content_version"],"path":pointer}),
    );
    assert_eq!(
        DatabaseReference::parse(&fs::read(&pointer).unwrap()).unwrap(),
        DatabaseReference { volume, entry: database.id }
    );
    let copy = EntryId::generate().unwrap();
    let copy_request = serde_json::to_vec(&json!({"op":"create","volume":volume,"id":copy,
        "parent":folder,"name":"Copy.pbdb","directory":false,"path":pointer}))
    .unwrap();
    assert_eq!(
        super::database_copy_source(&copy_request).unwrap(),
        Some(DatabaseReference { volume, entry: database.id })
    );
    request(
        &store,
        json!({"op":"create","volume":volume,"id":copy,"parent":folder,"name":"Copy.pbdb","directory":false,"path":pointer}),
    );
    assert_eq!(
        store
            .kv_read(&access, EntryAddress { volume, entry: copy }, b"key", 0, 128, None)
            .unwrap()
            .bytes,
        b"retained value"
    );
    let copied = metadata(&store, volume, copy);
    request(
        &store,
        json!({"op":"modify","volume":volume,"entry":copy,"name":"Renamed.pbdb","version":copied["content_version"],"metadata":copied["metadata_version"]}),
    );
    assert_eq!(store.resolve_entry(&access, volume, "Renamed").unwrap().id, copy);
    let other = store.create_volume("Another drive", false).unwrap().id;
    let across = EntryId::generate().unwrap();
    request(
        &store,
        json!({"op":"create","volume":other,"id":across,"name":"Across.pbdb",
        "directory":false,"path":pointer}),
    );
    assert_eq!(
        store
            .kv_read(&access, EntryAddress { volume: other, entry: across }, b"key", 0, 128, None)
            .unwrap()
            .bytes,
        b"retained value"
    );
    drop(store);
    let store = Arc::new(LocalStore::open(&profile).unwrap());
    assert_eq!(store.provider_item(volume, file).unwrap().parent, Some(folder));
    let copied = metadata(&store, volume, copy);
    request(
        &store,
        json!({"op":"delete","volume":volume,"entry":copy,"version":copied["content_version"],"metadata":copied["metadata_version"],"recursive":false}),
    );
    assert!(store.entry(&access, volume, copy).is_err());
    let page = request(&store, json!({"op":"scan","volume":volume}));
    assert!(page["items"].as_array().unwrap().iter().any(|item| item["id"]
        == json!(crate::store::file_provider::provider_entry_identifier(volume, file))));
    // Directory deletion must honor the OS's non-recursive contract and every
    // descendant's current authority under the same namespace mutation gate.
    let original = metadata(&store, volume, folder);
    let deletion = |recursive| {
        json!({"op":"delete","volume":volume,"entry":folder,
        "version":original["content_version"],"metadata":original["metadata_version"],"recursive":recursive})
    };
    assert!(
        execute(&store, &BlobTransport::local(), &serde_json::to_vec(&deletion(false)).unwrap())
            .unwrap_err()
            .to_string()
            .contains("directory not empty")
    );
    store
        .set_entry_permissions(
            &access,
            volume,
            file,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    assert!(
        execute(&store, &BlobTransport::local(), &serde_json::to_vec(&deletion(true)).unwrap())
            .unwrap_err()
            .to_string()
            .contains("protected child")
    );
    store
        .set_entry_permissions(
            &access,
            volume,
            file,
            crate::store::object_contract::EntryAcl::Inherit,
        )
        .unwrap();
    request(&store, deletion(true));
    assert!(store.provider_item(volume, folder).is_err());
    assert!(store.provider_item(volume, file).is_err());
}

#[test]
fn staged_upload_rejects_concurrent_edits_and_keeps_destination_directory_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Files", false).unwrap().id;
    let access = Access::owner(store.account().id);
    let file = store.create_blob(&access, volume, "file.txt", b"old").unwrap().id;
    let folder = EntryId::generate().unwrap();
    store.provider_create_directory(volume, folder, None, "folder").unwrap();
    let before = store.provider_item(volume, file).unwrap();
    let target = store
        .provider_destination(
            volume,
            file,
            Some(folder),
            "new.txt",
            &before.content_version,
            &before.metadata_version,
        )
        .unwrap();
    store.write_blob(&access, volume, file, b"concurrent app edit").unwrap();
    assert!(store.publish_file(&access, &target, &mut b"stale upload".as_slice(), true).is_err());
    assert_eq!(store.provider_item(volume, file).unwrap().parent, None);
    let before = store.provider_item(volume, file).unwrap();
    let target = store
        .provider_destination(
            volume,
            file,
            Some(folder),
            "new.txt",
            &before.content_version,
            &before.metadata_version,
        )
        .unwrap();
    store.rename_entry(&access, volume, folder, "renamed folder").unwrap();
    store.publish_file(&access, &target, &mut b"new content".as_slice(), true).unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "renamed folder/new.txt").unwrap().id, file);
    assert!(store.resolve_entry(&access, volume, "folder").is_err());
}

#[test]
fn same_user_socket_checks_bounds_and_app_roots_are_never_drives() {
    let directory = tempfile::tempdir().unwrap();
    let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let store = &services.store;
    let personal = store.create_volume("Personal", false).unwrap();
    let root = store
        .initialize_root(
            Id::generate().unwrap(),
            "App",
            false,
            &crate::store::RootInitialization {
                permissions: Default::default(),
                databases: vec![],
                blobs: vec![],
            },
        )
        .unwrap();
    assert!(!root.personal);
    enroll_socket_profile(directory.path(), &services.store, services.store.account().id);
    let server = Server::start(directory.path(), &services, BlobTransport::local(), || {}).unwrap();
    let reply: Value =
        serde_json::from_slice(&call(directory.path(), br#"{"op":"volumes"}"#).unwrap()).unwrap();
    assert_eq!(reply["volumes"].as_array().unwrap().len(), 1);
    assert_eq!(reply["volumes"][0]["id"], json!(personal.id));
    assert!(call(directory.path(), &[b'x'; 65537]).is_err());
    let reply: Value =
        serde_json::from_slice(&call(directory.path(), br#"{"op":"unknown"}"#).unwrap()).unwrap();
    assert!(reply["error"].is_string());
    drop(server);
}

#[test]
fn catalog_and_watch_replies_fit_bounds_for_maximum_escaped_labels() {
    // These are legal 256-byte labels whose control characters expand sixfold
    // in JSON. Send both real socket reply shapes at the 256-volume limit.
    let directory = tempfile::tempdir().unwrap();
    let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let label = format!("A{}", "\u{1}".repeat(255));
    let mut expected = std::collections::BTreeSet::new();
    for _ in 0..256 {
        let volume = services.store.create_volume(&label, false).unwrap();
        expected.insert(volume.id.to_string());
    }
    enroll_socket_profile(directory.path(), &services.store, services.store.account().id);
    let server = Server::start(directory.path(), &services, BlobTransport::local(), || {}).unwrap();
    for (operation, field) in [("volumes", "volumes"), ("catalog", "items")] {
        let reply =
            call(directory.path(), &serde_json::to_vec(&json!({"op":operation})).unwrap()).unwrap();
        assert!(reply.len() <= REPLY_BYTES, "{operation} reply exceeds the provider budget");
        let value: Value = serde_json::from_slice(&reply).unwrap();
        let rows = value[field].as_array().unwrap();
        assert_eq!(rows.len(), 256);
        let actual = rows
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual, expected, "{operation} must preserve all volume identities");
        if operation == "volumes" {
            assert!(rows.iter().all(|row| row["name"] == label));
        } else {
            assert!(rows.iter().all(|row| row["filename"].as_str().unwrap().len() <= 255));
        }
    }
    drop(server);
}

#[test]
fn signed_out_retained_profile_denies_system_domains_despite_acknowledged_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let account = services.store.account().id;
    let access = Access::owner(account);
    let volume = services.store.create_volume("Retained documents", false).unwrap().id;
    let entry =
        services.store.create_blob(&access, volume, "kept.txt", b"retained edits").unwrap().id;
    let item = services.store.provider_item(volume, entry).unwrap();
    let folder = services.store.provider_volume_item(volume).unwrap();
    services
        .store
        .save_recovery_state(&crate::store::RecoveryState {
            secret: [7; 16],
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .unwrap();
    let pending = services.store.pending_operations().unwrap();
    enroll_socket_profile(directory.path(), &services.store, account);
    let server = Server::start(directory.path(), &services, BlobTransport::local(), || {}).unwrap();
    let before: Value =
        serde_json::from_slice(&call(directory.path(), br#"{"op":"volumes"}"#).unwrap()).unwrap();
    assert_eq!(before["provider_ready"], true);
    assert_eq!(before["volumes"][0]["id"], json!(volume));
    drop(server);
    let seed = services.store.account_seed().unwrap();
    drop(services);
    // This is the identity shape written by keep-profile sign-out after the
    // engine drains. Recovery, the original profile and its outbox stay intact.
    crate::network::ap::ClientIdentity::from_seed("", "", seed)
        .unwrap()
        .save(&directory.path().join("instance.json"))
        .unwrap();
    let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let server = Server::start(directory.path(), &services, BlobTransport::local(), || {}).unwrap();
    let watch =
        json!({"op":"volumes", "after":before["revision"], "known":[volume], "volume":volume});
    let watch: Value = serde_json::from_slice(
        &call(directory.path(), &serde_json::to_vec(&watch).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(watch["provider_ready"], false);
    assert_eq!(watch["connected"], false);
    assert_eq!(watch["volumes"], json!([]));
    assert_eq!(watch["retired"], json!([]));
    assert!(watch["anchor"].is_null());
    let fetched = directory.path().join("denied-fetch");
    for operation in [
        json!({"op":"root"}),
        json!({"op":"catalog", "known":[volume]}),
        json!({"op":"volume_item", "volume":volume}),
        json!({"op":"item", "volume":volume, "entry":entry}),
        json!({"op":"list", "volume":volume}),
        json!({"op":"scan", "volume":volume}),
        json!({"op":"anchor", "volume":volume}),
        json!({"op":"fetch", "volume":volume, "entry":entry, "version":item.content_version, "path":fetched}),
        json!({"op":"create_volume", "id":Id::generate().unwrap(), "name":"Denied"}),
        json!({"op":"rename_volume", "volume":volume, "name":"Denied", "metadata":folder.metadata_version}),
        json!({"op":"create", "volume":volume, "id":EntryId::generate().unwrap(), "name":"Denied", "directory":true}),
        json!({"op":"modify", "volume":volume, "entry":entry, "name":"Denied", "version":item.content_version, "metadata":item.metadata_version}),
        json!({"op":"delete", "volume":volume, "entry":entry, "version":item.content_version, "metadata":item.metadata_version, "recursive":false}),
        json!({"op":"push_token", "token":"abcd", "invalidated":false, "bundle":"playbit.engine", "sandbox":false}),
    ] {
        let reply: Value = serde_json::from_slice(
            &call(directory.path(), &serde_json::to_vec(&operation).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(reply["code"], "permission_denied", "{}", operation["op"]);
    }
    assert!(!fetched.exists());
    assert_eq!(services.store.read_blob_entry(&access, volume, entry).unwrap(), b"retained edits");
    assert_eq!(services.store.provider_item(volume, entry).unwrap().filename, "kept.txt");
    assert_eq!(services.store.provider_volume_item(volume).unwrap().filename, "Retained documents");
    assert_eq!(services.store.pending_operations().unwrap(), pending);
    assert!(services.store.recovery_state().unwrap().unwrap().acknowledged);
    assert_eq!(services.store.account_seed().unwrap(), seed);
    // Enrollment for a different account cannot grant the retained domain
    // access while the profile is awaiting its normal account-binding checks.
    enroll_socket_profile(directory.path(), &services.store, Id::generate().unwrap());
    let denied: Value =
        serde_json::from_slice(&call(directory.path(), br#"{"op":"root"}"#).unwrap()).unwrap();
    assert_eq!(denied["code"], "permission_denied");
    enroll_socket_profile(directory.path(), &services.store, account);
    let admitted: Value =
        serde_json::from_slice(&call(directory.path(), br#"{"op":"root"}"#).unwrap()).unwrap();
    assert_eq!(admitted["item"]["role"], "root");
    drop(server);
}

#[test]
fn enrollment_does_not_wait_for_provider_push_registration() {
    // A real provider worker waits on a held QUIC reply. Bind before releasing
    // that reply; the protocol deadline is a liveness guard, not timing evidence.
    use crate::network::ap::{ApConnection, ClientIdentity, testing};
    let directory = tempfile::tempdir().unwrap();
    let mut services = super::super::Services::open(directory.path(), || {}).unwrap();
    assert!(matches!(
        push_request(&services.store, "abcd".into(), false, "playbit.engine".into(), false)
            .unwrap(),
        crate::network::ap::operation::ServiceRequest::RegisterFileProviderAccountPush { .. }
    ));
    assert!(
        push_request(&services.store, String::new(), false, "playbit.engine".into(), false)
            .is_err()
    );
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let ap = testing::silent_authenticating_server();
    let account = Id::from_bytes([42; 16]);
    let mut identity = ClientIdentity::from_seed(
        "push@provider.invalid",
        "Test",
        services.store.account_seed().unwrap(),
    )
    .unwrap();
    identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
    identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![1];
    let connection = Arc::new(
        runtime.block_on(ApConnection::connect_authenticated(&ap.config, &mut identity)).unwrap(),
    );
    let blobs = BlobTransport::new(
        runtime.handle().clone(),
        Arc::new(std::sync::Mutex::new(Some(connection.clone()))),
        Arc::new(super::super::blob::BlobTransfers::default()),
        #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
        None,
    );
    enroll_socket_profile(directory.path(), &services.store, services.store.account().id);
    let server = Server::start(directory.path(), &services, blobs, || {}).unwrap();
    let profile = directory.path().to_path_buf();
    let request = std::thread::spawn(move || {
        call(
            &profile,
            b"{\"op\":\"push_token\",\"token\":\"abcd\",\"invalidated\":true,\
              \"bundle\":\"playbit.engine\",\"sandbox\":false}",
        )
    });
    ap.request_held.recv_timeout(Duration::from_secs(5)).unwrap();
    services.bind_authenticated_account(account).unwrap();
    assert!(ap.closed.try_recv().is_err(), "binding must finish while AP is still held");
    assert_eq!(services.store.account().id, account);
    connection.shutdown();
    let reply: Value = serde_json::from_slice(&request.join().unwrap().unwrap()).unwrap();
    assert!(reply["error"].is_string());
    drop(server);
}

#[test]
fn provider_teardown_survives_deleted_profile_and_preserves_replacement() {
    // Drop must wake the original listener without connecting through a pathname
    // that may no longer exist or may now name another engine's endpoint.
    for replace in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let services = crate::engine::Services::open(&profile, || {}).unwrap();
        let (accepted, accepting) = std::sync::mpsc::channel();
        let server = Server::start(&profile, &services, BlobTransport::local(), move || {
            let _ = accepted.send(());
        })
        .unwrap();
        let path = profile.join(SOCKET);
        // A connected client with an incomplete request cannot keep shutdown
        // waiting for the ordinary five-second request deadline.
        let mut partial = UnixStream::connect(&path).unwrap();
        partial.write_all(&[1]).unwrap();
        accepting.recv_timeout(Duration::from_secs(2)).unwrap();
        let replacement = if replace {
            fs::rename(&path, profile.join("retired.sock")).unwrap();
            Some(UnixListener::bind(&path).unwrap())
        } else {
            fs::remove_dir_all(&profile).unwrap();
            None
        };
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            drop(server);
            send.send(()).unwrap();
        });
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        owner.join().unwrap();
        assert_eq!(path.exists(), replacement.is_some());
    }
}
