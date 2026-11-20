//! Replacement decisions preserve account isolation through cancellation and restart.
use super::*;
use crate::{
    network::ap::ClientIdentity,
    store::{Id, LocalStore},
};

fn fixture() -> (tempfile::TempDir, Id, Id, [u8; 32]) {
    let directory = tempfile::tempdir().expect("profile");
    let store = LocalStore::open(directory.path()).expect("store");
    let original = store.account().id;
    store.create_volume("Unsynced local data", true).expect("retained volume");
    fs::create_dir(directory.path().join("file-io")).expect("private File directory");
    fs::write(directory.path().join("file-io/manifest"), b"{}").expect("private File manifest");
    fs::create_dir_all(directory.path().join("development-projects/account/project"))
        .expect("development binding directory");
    fs::write(
        directory.path().join("development-projects/account/project/root.json"),
        b"private pending initializer objects and blob bytes",
    )
    .expect("pending development initialization");
    let seed = store.account_seed().expect("device key");
    let incoming = Id::from_bytes([42; 16]);
    let mut identity = ClientIdentity::from_seed("b@example.test", "Test", seed).expect("identity");
    identity.auth_token = vec![1; 72];
    identity.auth_token[..16].copy_from_slice(incoming.as_bytes());
    identity.instance_id = Id::from_bytes([2; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![2];
    identity.save(&directory.path().join("instance.json")).expect("authenticated attempt");
    (directory, original, incoming, seed)
}

#[test]
fn mismatched_authentication_reopens_without_binding_or_network_access() {
    // This also represents installations stuck by a prior sign-in attempt.
    let (directory, original, _, _) = fixture();
    for _ in 0..2 {
        let mut engine =
            crate::engine::Engine::open(directory.path()).expect("open pending decision");
        assert!(!engine.enrolled());
        assert!(engine.remote.as_ref().expect("remote").replacement_required());
        assert!(engine.status_snapshot().expect("status").account_profile.replacement_required);
        assert_eq!(engine.services.store.account().id, original);
        assert_eq!(engine.services.store.volumes().expect("volumes").len(), 1);
        assert!(
            engine
                .remote
                .as_mut()
                .expect("remote")
                .initialize_account(&engine.services.store)
                .is_err()
        );
        // Preflight cannot hide the pending decision while Remote belongs to its worker.
        let ticket = engine
            .begin_network_request(crate::engine::Request::PrepareSignIn)
            .expect("admit preflight");
        assert!(engine.remote.is_none(), "network worker owns Remote");
        assert!(
            engine
                .status_snapshot()
                .expect("in-flight status")
                .account_profile
                .replacement_required
        );
        assert_eq!(
            engine.shutdown(std::time::Duration::from_secs(1)).expect("collect preflight"),
            crate::engine::Shutdown::Done
        );
        assert!(
            engine
                .take_network_request(ticket)
                .expect_err("preflight needs consent")
                .to_string()
                .contains("Confirm replacing existing data")
        );
        assert!(
            engine
                .status_snapshot()
                .expect("completed status")
                .account_profile
                .replacement_required
        );
        assert!(!engine.enrolled());
    }
}

#[test]
fn sign_out_preserves_all_data_and_suppresses_automatic_sign_in() {
    // Compare durable records and device seed; opening redb may change file bookkeeping.
    let (directory, original, _, seed) = fixture();
    let before = {
        let store = LocalStore::open(directory.path()).expect("store");
        serde_json::to_value(store.volumes().expect("volumes")).expect("records")
    };
    fs::write(directory.path().join("retained-blob"), b"unsynced bytes").expect("blob");
    resolve_replacement(directory.path(), false).expect("sign out");
    assert_eq!(
        fs::read(directory.path().join("file-io/manifest")).expect("retained File manifest"),
        b"{}"
    );
    assert_eq!(
        fs::read(directory.path().join("retained-blob")).expect("retained blob"),
        b"unsynced bytes"
    );
    for _ in 0..2 {
        let engine = crate::engine::Engine::open(directory.path()).expect("reopen signed out");
        assert!(!engine.enrolled());
        assert!(!engine.remote.as_ref().expect("remote").replacement_required());
        assert_eq!(engine.services.store.account().id, original);
        assert_eq!(engine.services.store.account_seed().expect("seed"), seed);
        assert_eq!(
            serde_json::to_value(engine.services.store.volumes().expect("volumes"))
                .expect("records"),
            before
        );
    }
}

#[test]
fn replace_keeps_only_the_authenticated_continuation() {
    // Confirm once, then reopen twice; A's volumes and pending operations are gone.
    let (directory, _, incoming, seed) = fixture();
    fs::create_dir_all(directory.path().join("builds")).expect("builds");
    fs::write(directory.path().join("builds/private-old-file"), b"old data")
        .expect("old local file");
    resolve_replacement(directory.path(), true).expect("replace");
    assert!(!directory.path().join("builds/private-old-file").exists());
    assert!(!directory.path().join("development-projects").exists());
    for _ in 0..2 {
        let engine = crate::engine::Engine::open(directory.path()).expect("reopen B");
        assert!(engine.enrolled());
        assert!(!engine.remote.as_ref().expect("remote").replacement_required());
        assert_eq!(engine.services.store.account().id, incoming);
        assert_eq!(engine.services.store.account_seed().expect("device seed"), seed);
        assert!(engine.services.store.volumes().expect("volumes").is_empty());
        assert!(engine.services.store.sessions().expect("sessions").is_empty());
        assert!(engine.services.store.pending_operations().expect("pending").is_empty());
    }
    assert!(
        resolve_replacement(directory.path(), true).is_err(),
        "repeated consent cannot clear B"
    );
}

#[test]
fn interrupted_confirmed_replacement_resumes_before_profile_open() {
    // Resume before deletion, with stale File state after deletion, and after restoring B.
    for phase in 0..3 {
        let (directory, _, incoming, seed) = fixture();
        let profile = directory.path().canonicalize().expect("path");
        let identity = ClientIdentity::load(&profile.join("instance.json")).expect("identity");
        let intent =
            Intent::Replace { seed, identity: Box::new(identity), account_key: Some([43; 16]) };
        let pending = intent_path(&profile, ".sign-out-pending");
        intent.save(&pending).expect("durable consent");
        if phase >= 1 {
            clear_profile(&profile).expect("remove A");
        }
        if phase == 1 {
            // Data deletion is incomplete, so its format barrier has not yet
            // been retired. A supported pending reset may finish under locks.
            fs::write(
                profile.join("control/profile-format.json"),
                crate::store::profile::format::BASELINE,
            )
            .expect("retained format barrier");
            fs::create_dir(profile.join("file-io")).expect("stale private File directory");
            fs::write(profile.join("file-io/manifest"), b"{}")
                .expect("stale private File manifest");
            fs::create_dir(profile.join("development-projects"))
                .expect("stale development bindings");
            fs::write(profile.join("development-projects/root.json"), b"private initializer")
                .expect("stale pending initializer");
        }
        if phase == 2 {
            intent
                .restore(
                    &profile,
                    &lock_launch(&profile.join(crate::paths::LOCAL_LOCK)).expect("lock"),
                )
                .expect("restore B");
        }
        resume(&profile).expect("finish confirmed replacement");
        assert!(!profile.join("development-projects").exists());
        assert!(!pending.exists());
        let store = LocalStore::open(&profile).expect("reopen");
        assert_eq!(store.account().id, incoming);
        assert_eq!(
            store.prepared_account_key(incoming).expect("retained account key"),
            Some([43; 16])
        );
        assert!(store.volumes().expect("volumes").is_empty());
        assert_eq!(
            ClientIdentity::load(&profile.join("instance.json"))
                .expect("identity")
                .account_id()
                .expect("account"),
            *incoming.as_bytes()
        );
    }
}

#[test]
fn invalid_consent_or_open_store_cannot_delete_data() {
    // A corrupt intent and another live storage owner both fail before removal.
    let (directory, original, _, seed) = fixture();
    let store = LocalStore::open(directory.path()).expect("live store");
    assert!(resolve_replacement(directory.path(), true).is_err());
    drop(store);
    let profile = directory.path().canonicalize().expect("path");
    let identity = ClientIdentity::load(&profile.join("instance.json")).expect("identity");
    let mut bad_seed = seed;
    bad_seed[0] ^= 1;
    Intent::Replace { seed: bad_seed, identity: Box::new(identity), account_key: None }
        .save(&intent_path(&profile, ".sign-out-pending"))
        .expect("damaged intent");
    assert!(resume(&profile).is_err());
    assert!(LocalStore::open(&profile).is_err());
    fs::remove_file(intent_path(&profile, ".sign-out-pending"))
        .expect("repair invalid test intent");
    let store = LocalStore::open(&profile).expect("retained store");
    assert_eq!(store.account().id, original);
    assert_eq!(store.volumes().expect("volumes").len(), 1);
}

#[test]
fn email_code_authentication_requires_consent_before_local_binding() {
    // A real loopback registration exchange must quarantine B without reading A's vault.
    let directory = tempfile::tempdir().expect("profile");
    let store = LocalStore::open(directory.path()).expect("store");
    let original = store.account().id;
    store.create_volume("A's unsynced data", true).expect("volume");
    let incoming = Id::from_bytes([42; 16]);
    let (config, _, gate) = crate::network::ap::testing::held_registration_server(
        crate::network::ap::testing::IssuedCredentials {
            auth_token: incoming.as_bytes().iter().copied().chain([0; 56]).collect(),
            instance_id: Id::from_bytes([3; 16]).to_string().into_bytes(),
            ca_certificate: vec![1],
            instance_certificate: vec![2],
        },
    );
    gate.open();
    let completion = crate::engine::completion::Completion::host(|| {});
    let mut remote = crate::engine::remote::Remote::open(directory.path(), completion.wake.clone())
        .expect("remote");
    remote.set_test_transport(config);
    remote.request_code(&store, "b@example.test").expect("request code");
    let outcome = crate::engine::enrollment::Action::SignIn { code: "123456".into() }
        .execute(&mut remote, &store)
        .expect("sign-in decision");
    assert!(
        matches!(outcome, crate::engine::enrollment::Outcome::SignedIn { account, agreement: None, vault: None } if account == incoming)
    );
    assert!(remote.replacement_required());
    assert!(!remote.enrolled());
    assert_eq!(store.account().id, original);
    assert_eq!(store.volumes().expect("volumes").len(), 1);
    assert!(
        remote.request_code(&store, "c@example.test").is_err(),
        "retry cannot replace the unresolved candidate"
    );
    assert!(remote.finish_code(&store, "123456").is_err());
}

#[test]
#[ignore = "exports an isolated profile for packaged UI checks; set PLAYBIT_REPLACEMENT_FIXTURE"]
fn export_replacement_fixture() {
    // Export only synthetic credentials and unsynced data into a new private directory.
    let destination = std::path::PathBuf::from(
        std::env::var_os("PLAYBIT_REPLACEMENT_FIXTURE").expect("fixture destination"),
    );
    assert!(!destination.exists(), "fixture destination must be new");
    let (directory, _, _, _) = fixture();
    fs::rename(directory.keep(), destination).expect("export isolated profile");
}

#[test]
fn replacement_supports_embedded_profiles_longer_than_unix_socket_paths() {
    for replace in [false, true] {
        let (directory, original, incoming, _) = fixture();
        let root = tempfile::tempdir().expect("long path root");
        let parent = root.path().join("sandbox-container".repeat(8));
        fs::create_dir(&parent).expect("parent");
        let profile = parent.join("profile");
        fs::rename(directory.keep(), &profile).expect("move closed fixture");
        resolve_replacement(&profile, replace).expect("embedded replacement decision");
        let store = LocalStore::open(&profile).expect("reopen");
        assert_eq!(store.account().id, if replace { incoming } else { original });
        assert_eq!(store.volumes().expect("volumes").len(), usize::from(!replace));
    }
}

#[cfg(target_vendor = "apple")]
#[test]
fn confirmed_replacement_resumes_only_the_incoming_saved_key() {
    let (directory, original, incoming, _) = fixture();
    crate::credentials::remember_account(directory.path(), original, "a@example.test", &[7; 16])
        .expect("A's saved key");
    crate::credentials::remember_account(directory.path(), incoming, "b@example.test", &[8; 16])
        .expect("B's saved key");
    let engine = crate::engine::Engine::open(directory.path()).expect("pending decision");
    drop(engine);
    resolve_replacement(directory.path(), true).expect("replace");
    let engine = crate::engine::Engine::open(directory.path()).expect("continue B");
    let credential = crate::credentials::find_account_for(
        directory.path(),
        engine.services.store.account().id,
        "b@example.test",
    )
    .expect("lookup")
    .expect("key");
    assert_eq!(credential.account, incoming);
    assert_eq!(credential.secret, [8; 16]);
}

#[test]
fn ordinary_sign_out_keeps_account_volumes_blobs_and_device_seed() {
    // Sign out an enrolled identity twice, then reopen without automatic enrollment.
    let (directory, original, _, seed) = fixture();
    let mut identity =
        ClientIdentity::load(&directory.path().join("instance.json")).expect("identity");
    identity.auth_token[..16].copy_from_slice(original.as_bytes());
    identity.save(&directory.path().join("instance.json")).expect("enrolled identity");
    fs::create_dir(directory.path().join("blobs")).expect("blob directory");
    fs::write(directory.path().join("blobs/unsynced"), b"local-only data").expect("blob");
    let pending = {
        let store = LocalStore::open(directory.path()).expect("store");
        let pending = store.pending_operations().expect("pending operations");
        assert!(!pending.is_empty(), "fixture has unsynchronized volume registration");
        pending
    };
    for _ in 0..2 {
        keep_profile(directory.path()).expect("sign out without erasing");
        let engine = crate::engine::Engine::open(directory.path()).expect("reopen");
        assert!(!engine.enrolled());
        assert_eq!(engine.services.store.account().id, original);
        assert_eq!(engine.services.store.account_seed().expect("seed"), seed);
        assert_eq!(engine.services.store.volumes().expect("volumes").len(), 1);
        assert_eq!(engine.services.store.pending_operations().expect("pending"), pending);
        assert_eq!(
            fs::read(directory.path().join("development-projects/account/project/root.json"))
                .expect("retained development binding"),
            b"private pending initializer objects and blob bytes"
        );
        assert_eq!(
            fs::read(directory.path().join("blobs/unsynced")).expect("retained blob"),
            b"local-only data"
        );
    }
}

#[test]
fn ordinary_sign_out_refuses_an_owned_profile_without_changing_credentials() {
    // A live store can still own writes. Sign-out cannot race it or claim success.
    let (directory, _, _, _) = fixture();
    let before = fs::read(directory.path().join("instance.json")).expect("identity");
    let store = LocalStore::open(directory.path()).expect("live store");
    assert!(keep_profile(directory.path()).is_err());
    assert_eq!(fs::read(directory.path().join("instance.json")).expect("identity"), before);
    assert_eq!(store.volumes().expect("retained volumes").len(), 1);
}
