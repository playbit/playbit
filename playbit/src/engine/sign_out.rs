//! Explicit local sign-out, serialized with launch and resumed after interruption.
//! Synchronized recovery keys stay in Keychain; this device must sign in manually.
mod intent;
use super::Result;
#[cfg(feature = "process")]
use super::{ipc, process_identity::Peer};
use intent::Intent;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
};

// Sign-out decisions belong beside durable profile data, never in its cache.
fn intent_path(profile: &Path, suffix: &str) -> PathBuf {
    profile.join("control").join(suffix.trim_start_matches('.'))
}

pub(crate) fn resume(profile: &Path) -> Result<()> {
    // Removal retains the directory (and therefore symlink aliases). An absent
    // profile has no interrupted reset and must not be created during discovery.
    if !profile.try_exists()? {
        return Ok(());
    }
    if intent_path(&profile.canonicalize()?, ".sign-out-pending").try_exists()? {
        crate::store::LocalStore::check_profile_reset_format(profile)?;
        reset_profile_with_admission(profile, None, true)?;
    }
    Ok(())
}

/// Revoke this device's enrollment without changing its durable account or app data.
#[cfg(any(
    all(feature = "connected-engine", feature = "desktop-ui"),
    feature = "ios",
    all(test, feature = "app-host")
))]
pub(crate) fn keep_profile(profile: &Path) -> Result<()> {
    let profile = profile.canonicalize()?;
    let paths = crate::paths::Paths::for_profile(&profile)?;
    let _launch = lock_launch(&paths.launch_lock)?;
    stop_engine(&profile, false)?;
    if intent_path(&profile, ".sign-out-pending").try_exists()? {
        return Err("A profile replacement is still in progress".into());
    }
    let store = crate::store::LocalStore::open(&profile)?;
    let seed = store.account_seed()?;
    crate::network::ap::ClientIdentity::from_seed("", "", seed)?
        .save(&profile.join("instance.json"))?;
    Ok(())
}

#[cfg(any(test, feature = "desktop-ui", feature = "ios"))]
pub(crate) fn remove_profile(profile: &Path) -> Result<()> {
    reset_profile(profile, None).map(|_| ())
}

#[cfg(any(test, feature = "desktop-ui", feature = "ios"))]
pub(crate) fn reset_profile(
    profile: &Path,
    invitation: Option<&str>,
) -> Result<Option<crate::store::InvitationPreview>> {
    reset_profile_with_admission(profile, invitation, false)
}

fn reset_profile_with_admission(
    profile: &Path,
    invitation: Option<&str>,
    automatic: bool,
) -> Result<Option<crate::store::InvitationPreview>> {
    let profile = profile.canonicalize()?;
    let paths = crate::paths::Paths::for_profile(&profile)?;
    let _launch = lock_launch(&paths.launch_lock)?;
    if automatic && !intent_path(&profile, ".sign-out-pending").try_exists()? {
        return Ok(None);
    }
    reset_locked(&profile, invitation, automatic)
}

// The caller retains the profile's launch lock through shutdown and deletion.
fn reset_locked(
    profile: &Path,
    invitation: Option<&str>,
    automatic: bool,
) -> Result<Option<crate::store::InvitationPreview>> {
    // The retained control directory journals the decision before destructive work.
    let pending = intent_path(profile, ".sign-out-pending");
    let mut intent = Intent::load_or_create(&pending, invitation)?;
    stop_engine(profile, true)?;
    // A missing socket is not proof that storage is unused. Also acquire the
    // store lock, and refuse deletion if any other owner still holds it.
    let local = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join(crate::paths::LOCAL_LOCK))?;
    local.try_lock()?;
    if automatic {
        crate::store::LocalStore::check_profile_reset_format(profile)?;
    }
    // Capture the selected capability before deleting its metadata. Each phase
    // is durable so Quit/crash resumes without losing or replaying a UI action.
    intent.capture(profile, &pending)?;
    let trace = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join("control/trace.lock"))?;
    trace.try_lock().map_err(|_| "Trace recording is still stopping; retry sign-out")?;
    clear_profile(profile)?;
    let preview = intent.restore(profile, &local)?;
    fs::remove_file(&pending)?;
    File::open(pending.parent().expect("marker has a parent"))?.sync_all()?;
    Ok(preview)
}

fn stop_engine(profile: &Path, replace_data: bool) -> Result<()> {
    #[cfg(not(feature = "process"))]
    let _ = replace_data;
    // Embedded hosts have no process socket, and their sandbox paths may exceed
    // sockaddr_un. The store lock below still proves exclusive ownership.
    let socket = profile.join(crate::paths::ENGINE_SOCKET);
    if !socket.try_exists()? {
        return Ok(());
    }
    match UnixStream::connect(socket) {
        Ok(stream) => {
            #[cfg(not(feature = "process"))]
            {
                drop(stream);
                return Err("Profile is owned by another engine".into());
            }
            #[cfg(feature = "process")]
            {
                ipc::verify_peer(&stream)?;
                let peer = Peer::capture(&stream)?;
                peer.require_engine()?;
                drop(stream);
                if replace_data {
                    peer.stop_for_reset()?;
                } else {
                    peer.stop(std::time::Duration::from_secs(3))?;
                }
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn clear_profile(profile: &Path) -> Result<()> {
    // Keep the namespace and held lock intact so aliases stay usable, including
    // if termination interrupts deletion. Diagnostics and control records do
    // not authorize replacing retained storage. Never follow symlinked entries.
    for name in [
        "profile.kvdb",
        "account.kvdb",
        "instance.json",
        "session-instances",
        "session-instances.pending",
        "development-projects",
        "file-io",
        "native-session.json",
        "control/profile-init.kvdb",
        "volumes",
        "blobs",
        "builds",
        "received",
        "staging",
        "cache",
        "apps",
    ] {
        let path = profile.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    File::open(profile)?.sync_all()?;
    File::open(profile.join("control"))?.sync_all()?;
    // Only explicit, completed data deletion can retire its format barrier.
    // Sync data removal first: interruption must never expose markerless old
    // data as a fresh profile. Automatic resume admitted the retained format.
    for name in ["control/profile-format.pending", "control/profile-format.json"] {
        match fs::remove_file(profile.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    File::open(profile.join("control"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_closed_profile_and_requires_manual_sign_in_after_relaunch() {
        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("profile");
        let other = root.path().join("other");
        fs::create_dir_all(profile.join("control")).expect("profile");
        fs::create_dir(&other).expect("other");
        fs::write(profile.join("instance.json"), b"credentials").expect("identity");
        fs::create_dir_all(profile.join("cache/wasm-vm")).expect("compiled cache");
        fs::write(profile.join("cache/wasm-vm/image"), b"private app contents")
            .expect("compiled app");
        fs::write(other.join("data"), b"independent").expect("other data");
        fs::create_dir(profile.join("logs")).expect("logs");
        fs::write(profile.join("logs/engine.log"), b"diagnostics").expect("diagnostics");
        use std::os::unix::fs::MetadataExt;
        let locks = ["launch.lock", "local.lock", "trace.lock"].map(|name| {
            let path = profile.join("control").join(name);
            let file = File::create(&path).expect("lock file");
            (path, file.metadata().expect("metadata").ino())
        });
        for directory in [
            crate::paths::APP_DISPLAYS,
            crate::paths::CORRUPT_BLOBS,
            crate::paths::NATIVE_APPS,
            "file-io",
            "development-projects/account/project",
        ] {
            let path =
                crate::paths::private_directory(&profile, directory).expect("owned directory");
            fs::write(path.join("retained"), b"private data").expect("owned data");
        }
        let root_inode = fs::metadata(&profile).expect("root metadata").ino();
        fs::write(profile.join("control/profile-format.json"), b"unsupported future format")
            .expect("unsupported descriptor");
        remove_profile(&profile).expect("sign out");
        assert_eq!(fs::metadata(&profile).expect("retained root").ino(), root_inode);
        for (path, inode) in locks {
            assert_eq!(fs::metadata(path).expect("retained lock").ino(), inode);
        }
        assert_eq!(
            fs::read(profile.join("logs/engine.log")).expect("retained log"),
            b"diagnostics"
        );
        assert!(profile.is_dir());
        resume(&profile).expect("relaunch");
        assert!(profile.join("control/local.lock").is_file());
        assert_eq!(fs::read(other.join("data")).expect("other data"), b"independent");
        assert!(!profile.join("instance.json").exists());
        assert!(!profile.join("cache/wasm-vm").exists());
        assert!(!profile.join("cache").exists());
        assert!(!profile.join(crate::paths::NATIVE_APPS).exists());
        assert!(!profile.join("received").exists());
        assert!(!profile.join("development-projects").exists());
        drop(
            crate::store::LocalStore::open(&profile).expect("fresh identity after explicit reset"),
        );
    }

    #[test]
    fn absent_profile_resume_has_no_discovery_side_effects() {
        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("absent");
        resume(&profile).expect("nothing to resume");
        assert!(!profile.exists());
    }

    #[test]
    fn automatic_resume_rechecks_format_under_the_local_lock_before_cleanup() {
        let directory = tempfile::tempdir().expect("profile");
        let profile = directory.path();
        let store = crate::store::LocalStore::open(profile).expect("store");
        drop(store);
        let pending = intent_path(profile, ".sign-out-pending");
        Intent::Remove.save(&pending).expect("confirmed intent");
        fs::write(profile.join("control/profile-format.json"), b"future format").expect("format");
        let before = fs::read(profile.join(crate::paths::PROFILE_DB)).expect("retained identity");
        let intent = fs::read(&pending).expect("intent");
        assert!(reset_profile_with_admission(profile, None, true).is_err());
        assert_eq!(
            fs::read(profile.join(crate::paths::PROFILE_DB)).expect("same identity"),
            before
        );
        assert_eq!(fs::read(&pending).expect("same intent"), intent);
        remove_profile(profile).expect("explicit deletion remains authorized");
        drop(crate::store::LocalStore::open(profile).expect("new empty profile"));
    }

    #[test]
    fn symlinked_profile_remains_usable_and_cache_removal_retains_sign_out() {
        // Reset through an alias, discard all cache data, and reopen through
        // either spelling: both must retain the manual sign-in decision.
        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("profile");
        fs::create_dir_all(profile.join("control")).expect("profile");
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&profile, &alias).expect("alias");
        let independent = root.path().join("independent-files");
        fs::create_dir(&independent).expect("independent directory");
        fs::write(independent.join("data"), b"independent File data").expect("independent data");
        for directory in ["file-io", "development-projects"] {
            std::os::unix::fs::symlink(&independent, profile.join(directory))
                .expect("private data alias");
        }
        fs::write(profile.join("account.kvdb"), b"account data").expect("data");
        remove_profile(&alias).expect("sign out through alias");
        assert!(!profile.join("development-projects").exists());
        assert_eq!(
            fs::read(independent.join("data")).expect("retained independent data"),
            b"independent File data"
        );
        fs::create_dir_all(&alias).expect("alias remains usable");
        resume(&alias).expect("resume alias");
        assert!(!alias.join("account.kvdb").exists());
        let paths = crate::paths::Paths::for_profile(&profile).expect("paths");
        fs::create_dir_all(&paths.blob_cache).expect("cache");
        fs::remove_dir_all(paths.blob_cache.parent().expect("cache directory"))
            .expect("discard cache");
        assert!(alias.join("control/local.lock").is_file());
        let store = crate::store::LocalStore::open(&alias).expect("fresh store through alias");
        drop(store);
        remove_profile(&alias).expect("repeat reset");
    }

    #[test]
    fn reset_restores_only_the_selected_invitation_for_the_new_account() {
        // An unjoined invitation survives recovery Back, including its pause
        // choice, while unrelated invitations and the previous account are gone.
        for paused in [false, true] {
            let root = tempfile::tempdir().expect("root");
            let profile = root.path().join("profile");
            let store = crate::store::LocalStore::open(&profile).expect("store");
            let old_account = store.account().id;
            let link = crate::store::session_link(
                &[7; 16],
                crate::release_channel::ReleaseChannel::Public,
            );
            let preview = store
                .preview_invitation(&link, crate::release_channel::ReleaseChannel::Public)
                .expect("selected invitation");
            store.pause_invitation(&preview.session, paused).expect("pause choice");
            store
                .preview_invitation(
                    &crate::store::session_link(
                        &[8; 16],
                        crate::release_channel::ReleaseChannel::Public,
                    ),
                    crate::release_channel::ReleaseChannel::Public,
                )
                .expect("unrelated invitation");
            drop(store);
            let restored = reset_profile(&profile, Some(&preview.session))
                .expect("reset")
                .expect("restored preview");
            assert_eq!(restored.session, preview.session);
            assert_eq!(restored.paused, paused);
            assert!(restored.received.is_none());
            let store = crate::store::LocalStore::open(&profile).expect("replacement store");
            assert_ne!(store.account().id, old_account);
            assert_eq!(store.pending_invitations().expect("pending"), [restored]);
            assert!(store.pending_invitation_proof(&preview.session, store.account().id).is_ok());
        }
    }

    #[test]
    fn interrupted_invitation_reset_resumes_capture_and_restore() {
        // Restart both before capture and after deletion: the durable phase
        // must recover the same capability without the original URL or UI.
        for after_capture in [false, true] {
            let root = tempfile::tempdir().expect("root");
            let profile = root.path().join("profile");
            let store = crate::store::LocalStore::open(&profile).expect("store");
            let link = crate::store::session_link(
                &[9; 16],
                crate::release_channel::ReleaseChannel::Public,
            );
            let preview = store
                .preview_invitation(&link, crate::release_channel::ReleaseChannel::Public)
                .expect("preview");
            drop(store);
            let pending =
                intent_path(&profile.canonicalize().expect("canonical"), ".sign-out-pending");
            let mut intent =
                Intent::load_or_create(&pending, Some(&preview.session)).expect("intent");
            if after_capture {
                intent.capture(&profile, &pending).expect("durable capture");
                clear_profile(&profile).expect("interrupted removal");
                fs::write(
                    profile.join("control/profile-format.json"),
                    crate::store::profile::format::BASELINE,
                )
                .expect("barrier remains during incomplete cleanup");
            }
            // Model an interruption before deletion reaches the compiled cache,
            // both before capture and after other profile entries were removed.
            fs::create_dir_all(profile.join("cache/wasm-vm")).expect("compiled cache");
            fs::write(profile.join("cache/wasm-vm/image"), b"private app contents")
                .expect("compiled app");
            fs::create_dir_all(profile.join("development-projects/account/project"))
                .expect("pending development binding");
            fs::write(
                profile.join("development-projects/account/project/root.json"),
                b"private pending initializer",
            )
            .expect("pending initializer data");
            resume(&profile).expect("resume reset");
            assert!(!profile.join("development-projects").exists());
            assert!(!pending.exists());
            assert!(!profile.join("cache/wasm-vm").exists());
            let store = crate::store::LocalStore::open(&profile).expect("replacement store");
            assert_eq!(
                store.pending_invitations().expect("pending"),
                std::slice::from_ref(&preview)
            );
            assert!(store.pending_invitation_proof(&preview.session, store.account().id).is_ok());
        }
    }

    #[test]
    fn invalid_reset_intent_cannot_delete_profile_data() {
        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("profile");
        fs::create_dir_all(profile.join("control")).expect("profile");
        fs::write(profile.join("account.kvdb"), b"retained").expect("data");
        let pending = intent_path(&profile.canonicalize().expect("canonical"), ".sign-out-pending");
        for bytes in [b"incomplete".to_vec(), vec![b' '; 1025]] {
            fs::write(&pending, bytes).expect("invalid intent");
            assert!(resume(&profile).is_err());
            assert_eq!(fs::read(profile.join("account.kvdb")).expect("retained data"), b"retained");
        }
    }

    #[test]
    fn locked_store_is_retained_and_interrupted_sign_out_resumes() {
        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("profile");
        fs::create_dir_all(profile.join("control")).expect("profile");
        fs::write(
            profile.join("control/profile-format.json"),
            crate::store::profile::format::BASELINE,
        )
        .expect("supported profile format");
        fs::write(profile.join("account.kvdb"), b"retained until stop").expect("data");
        let local = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(profile.join(crate::paths::LOCAL_LOCK))
            .expect("lock");
        local.try_lock().expect("owner");
        assert!(remove_profile(&profile).is_err());
        assert!(profile.join("account.kvdb").exists());
        drop(local);
        resume(&profile).expect("finish interrupted sign-out");
        assert!(profile.is_dir());
        resume(&profile).expect("fresh launch");
        assert!(!profile.join(crate::paths::PROFILE_DB).exists());
    }
}

#[cfg(all(test, target_vendor = "apple"))]
#[test]
fn signed_out_profile_does_not_automatically_use_its_synced_key() {
    let root = tempfile::tempdir().expect("root");
    let profile = root.path().join("profile");
    fs::create_dir(&profile).expect("profile");
    let account = crate::store::Id::from_bytes([42; 16]);
    crate::credentials::remember_account(&profile, account, "test@example.com", &[3; 16])
        .expect("synthetic saved key");
    remove_profile(&profile).expect("sign out");
    resume(&profile).expect("fresh profile");
    assert!(crate::credentials::find_account(&profile).expect("synced key").is_some());
    let engine = super::Engine::open(&profile).expect("new engine");
    assert!(!engine.enrolled());
}

fn lock_launch(path: &Path) -> Result<File> {
    let control = path.parent().expect("launch lock has a parent");
    crate::paths::private_directory(control.parent().expect("control has a profile"), "control")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    lock.try_lock()?;
    Ok(lock)
}

/// The host retires its UI/engine owners first. Only Replace authorizes deletion.
/// Consent and the authenticated continuation survive in the retained control directory.
#[cfg(any(
    feature = "desktop",
    feature = "ios",
    feature = "test-support",
    all(test, feature = "app-host"),
    all(target_os = "macos", feature = "desktop-ui", feature = "native-runtime")
))]
pub(crate) fn resolve_replacement(profile: &Path, replace: bool) -> Result<()> {
    let profile = profile.canonicalize()?;
    let paths = crate::paths::Paths::for_profile(&profile)?;
    let _launch = lock_launch(&paths.launch_lock)?;
    stop_engine(&profile, replace)?;
    let pending = intent_path(&profile, ".sign-out-pending");
    if pending.try_exists()? {
        if !replace {
            return Err("Replacement already started; retry Replace to finish".into());
        }
    } else {
        let store = crate::store::LocalStore::open(&profile)?;
        let identity_path = profile.join("instance.json");
        let identity = crate::network::ap::ClientIdentity::load(&identity_path)?;
        let account = crate::store::Id::from_bytes(identity.account_id()?);
        if !store.account_replacement_required(account)? {
            return Err("No account replacement is waiting for confirmation".into());
        }
        let seed = store.account_seed()?;
        let expected = crate::network::ap::ClientIdentity::from_seed("", "", seed)?;
        if identity.public_key()? != expected.public_key()? {
            return Err("Sign-in credentials belong to another device".into());
        }
        if !replace {
            expected.save(&identity_path)?;
            return Ok(());
        }
        let account_key = store.prepared_account_key(account)?;
        let intent = Intent::Replace { seed, identity: Box::new(identity), account_key };
        intent.validate()?;
        intent.save(&pending)?;
    }
    reset_locked(&profile, None, false)?;
    Ok(())
}

#[cfg(all(test, feature = "app-host"))]
mod replacement_tests;
