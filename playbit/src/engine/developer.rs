//! Bounded developer work on the existing background lane, never the UI owner.
use super::reply::{AppResponse, DeveloperReply, VolumeAccess};
use super::*;

pub(crate) struct Outcome {
    pub value: DeveloperReply,
}

/// Source scaffolding establishes a canonical address, without opening data,
/// minting reader credentials or requiring a saved recovery key.
pub(super) fn source_app_descriptor(
    store: &LocalStore,
    app_id: String,
    name: &str,
    declaration: &crate::store::RootInitialization,
) -> Result<crate::store::Volume> {
    use crate::package::AutomaticVolumeRole;
    let app = crate::package::AppIdentity { publisher: store.account().id, app_id };
    app.validate()?;
    crate::store::validate_volume_label(name)?;
    declaration.fingerprint()?;
    let id = app.volume_id(AutomaticVolumeRole::Root, None, None, None)?;
    match store.volume(id) {
        Ok(volume) => {
            if volume.personal || volume.owner != app.publisher {
                return Err("World descriptor has a different owner or purpose".into());
            }
            return Ok(volume);
        }
        Err(crate::store::Error::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(crate::store::Volume {
        id,
        name: "World".into(),
        owner: app.publisher,
        share_key_with_service: true,
        personal: false,
    })
}

pub(crate) fn is_request(request: &Request) -> bool {
    matches!(
        request,
        Request::GrantVolume { .. }
            | Request::PublishApp { .. }
            | Request::ListApps { .. }
            | Request::SetAppEnabled { .. }
            | Request::DeleteApp { .. }
            | Request::GetAppDisplay { .. }
            | Request::CreateAppSession { .. }
            | Request::PrepareApp { .. }
            | Request::SignPackage { .. }
            | Request::AuthenticatePackage { .. }
            | Request::CreateSession { .. }
            | Request::CreateBuildSession { .. }
            | Request::UpdateSession { .. }
    )
}

pub(super) fn validate(request: &Request) -> Result<()> {
    match request {
        Request::CreateBuildSession { build, .. } | Request::GetAppDisplay { build, .. }
            if !crate::identifier::valid(build, 32) =>
        {
            return Err("invalid build ID".into());
        }
        Request::GrantVolume { email, .. }
            if !crate::network::ap::operation::valid_account_email(email) =>
        {
            return Err("invalid recipient email".into());
        }
        Request::PublishApp { notes, .. }
            if !crate::network::ap::operation::valid_app_notes(notes) =>
        {
            return Err("publication notes exceed32KiB".into());
        }
        Request::UpdateSession { session, .. } if !crate::identifier::valid(session, 32) => {
            return Err("invalid session ID".into());
        }
        Request::GetAppDisplay { app, .. }
        | Request::SetAppEnabled { app, .. }
        | Request::DeleteApp { app }
        | Request::CreateAppSession { app, .. } => app.validate()?,
        _ => {}
    }
    let path = match request {
        Request::PublishApp { bundle, .. }
        | Request::PrepareApp { bundle }
        | Request::SignPackage { bundle }
        | Request::AuthenticatePackage { bundle }
        | Request::CreateSession { bundle, .. }
        | Request::UpdateSession { bundle, .. } => Some(bundle),
        _ => None,
    };
    if path.is_some_and(|path| path.as_os_str().len() > 4096) {
        return Err("bundle path is too long".into());
    }
    Ok(())
}

pub(super) fn prepare_app(store: &LocalStore, runtime: &Runtime, source: &Path) -> Result<Package> {
    let _timing = crate::trace::Span::new("developer.bundle.prepare");
    let package =
        store.authenticate_package(Package::open(source)?, crate::store::now_unix_secs())?;
    runtime.validate_guest(&package.wasm()?)?;
    Ok(package)
}

pub(super) fn cache_package(
    store: &LocalStore,
    runtime: &Runtime,
    profile: &Path,
    source: &Path,
) -> Result<Package> {
    use std::io::Read;
    let mut input = fs::File::open(source)?;
    if !input.metadata()?.is_file()
        || input.metadata()?.len() > crate::package::MAX_BUNDLE_BYTES as u64
    {
        return Err("bundle must be a bounded regular file".into());
    }
    let directory = profile.join(crate::paths::BUILDS);
    fs::create_dir_all(&directory)?;
    let temporary = directory.join(format!("{}.candidate", Id::generate()?));
    let result = (|| -> Result<Package> {
        let mut output = fs::File::create_new(&temporary)?;
        let length = std::io::copy(
            &mut input.by_ref().take(crate::package::MAX_BUNDLE_BYTES as u64 + 1),
            &mut output,
        )?;
        if length > crate::package::MAX_BUNDLE_BYTES as u64 {
            return Err("bundle exceeds size limit".into());
        }
        let package = prepare_app(store, runtime, &temporary)?;
        let build = package.id()?;
        let path = directory.join(&build);
        if path.exists() {
            if Package::open(&path)?.id()? != build {
                return Err("cached build identity mismatch".into());
            }
        } else {
            rustix::io::retry_on_intr(|| rustix::fs::fsync(&output))
                .map_err(std::io::Error::from)?;
            fs::rename(&temporary, &path)?;
            let directory = fs::File::open(&directory)?;
            rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory))
                .map_err(std::io::Error::from)?;
        }
        store.queue_build_upload(&build, &path)?;
        store.remember_imported_app(&package)?;
        Ok(package)
    })();
    if temporary.exists() {
        fs::remove_file(temporary)?;
    }
    result
}

fn provision_owned_root(store: &LocalStore, root: Id, build: &str) -> Result<()> {
    let volume = match store.volume(root) {
        Ok(volume) => volume,
        Err(crate::store::Error::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if volume.owner == store.account().id && store.has_volume_index(root)? {
        store.provision_version(&Access::owner(store.account().id), root, build)?;
    }
    Ok(())
}

pub(super) fn package_identity(store: &LocalStore, package: &Package) -> Result<DeveloperReply> {
    let status = store.package_publisher_status(package, crate::store::now_unix_secs())?;
    Ok(DeveloperReply::PackageIdentity {
        app: package.identity()?.clone(),
        build_id: package.id()?,
        issuer: status.claims.issuer,
        status: status.to_bytes().to_vec(),
    })
}

pub(super) fn sign_package(store: &LocalStore, source: &Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let source = source.canonicalize()?;
    let parent = source.parent().ok_or("package has no parent directory")?;
    let package = Package::open(&source)?;
    let now = crate::store::now_unix_secs();
    let signer = store.publisher_signer(now)?;
    let temporary = parent.join(format!(".signed-{}.pbapp", Id::generate()?));
    let result = (|| -> Result<String> {
        let mut file =
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        package.write_signed(&signer, signer.certificate.claims.issued_at, &mut file)?;
        file.sync_all()?;
        let signed = store.authenticate_package(Package::open(&temporary)?, now)?;
        let build = signed.id()?;
        fs::rename(&temporary, &source)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(build)
    })();
    if temporary.try_exists()? {
        fs::remove_file(&temporary)?;
    }
    result
}

/// Run on the bounded preparation worker. A cold trust lookup is handed to
/// the network queue before any import, session or source-file mutation.
pub(super) fn requires_network_trust(store: &LocalStore, request: &Request) -> Result<bool> {
    if matches!(request, Request::SignPackage { .. }) {
        return Ok(store.publisher_signer(crate::store::now_unix_secs()).is_err());
    }
    let source = match request {
        Request::AuthenticatePackage { bundle }
        | Request::PrepareApp { bundle }
        | Request::CreateSession { bundle, .. } => bundle,
        _ => return Ok(false),
    };
    let package = Package::open(source)?;
    if package.publisher_certificate().is_none() {
        return Err("package has no publisher signature".into());
    }
    Ok(store.authenticate_package(package, crate::store::now_unix_secs()).is_err())
}

// Local bundle import has no Remote dependency and can run while AP is stalled.
pub(super) fn create_session(
    store: &LocalStore,
    runtime: &Runtime,
    profile: &Path,
    bundle: &Path,
    root: Option<Id>,
    user: Option<Id>,
) -> Result<crate::store::Session> {
    let package = cache_package(store, runtime, profile, bundle)?;
    let app = package.identity()?.clone();
    let sync_shared = root.is_none()
        && package.initializers().declares(crate::package::AutomaticVolumeRole::Session);
    let root = select_root(store, &app, &package, root)?;
    let build = package.id()?;
    provision_owned_root(store, root, &build)?;
    store
        .create_session_binding(crate::store::SessionCreation {
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            root_volume: root,
            sync_shared,
            user_volume: user,
        })
        .map_err(Into::into)
}

/// Validate and commit local development code without borrowing AP ownership.
/// Admission's single local-preparation worker bounds this work independently.
pub(super) fn update_session(
    remote: &mut remote::Remote,
    store: &Arc<LocalStore>,
    runtime: &Runtime,
    profile: &Path,
    session: String,
    bundle: &Path,
) -> Result<DeveloperReply> {
    let old = store.session(&session)?;
    if old.creator != store.account().id {
        return Err("only the session creator can update this development session".into());
    }
    if !matches!(old.selector, BuildSelector::Exact { .. }) {
        return Err("publish the app to update app followers".into());
    }
    let package = cache_package(store, runtime, profile, bundle)?;
    if package.identity()? != &old.app {
        return Err("Changing app_id requires a new session".into());
    }
    let build = package.id()?;
    provision_owned_root(store, old.replica_volume(), &build)?;
    if store.is_local_session(&session)?
        && (old.shared_volume.is_none() || !store.shared_session_initialized(&session)?)
    {
        store.select_development_build(&old, &build)?;
    } else {
        // Ordinary sessions keep their authoritative AP selection. The network
        // task prepares and adopts that selection after this CAS succeeds.
        remote.prepare_publication(store, Some(old.replica_volume()))?;
        let current = remote.resolve_session(store, &session)?;
        remote.advance_session(
            store,
            Id::generate()?,
            &session,
            &build,
            current["revision"].as_u64().ok_or("missing session revision")?,
        )?;
    }
    Ok(DeveloperReply::Updated { session, build_id: build, update: None, resolved_build_id: None })
}

fn select_root(
    store: &LocalStore,
    app: &crate::package::AppIdentity,
    _package: &Package,
    root: Option<Id>,
) -> Result<Id> {
    if let Some(root) = root {
        store.volume(root)?;
        return Ok(root);
    }
    let root = app.volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)?;
    Ok(root)
}

fn authenticate_package(
    remote: &mut remote::Remote,
    store: &LocalStore,
    bundle: &Path,
) -> Result<Package> {
    remote.authenticate_public_package(store, Package::open(bundle)?)
}

pub(super) fn execute(
    request: Request,
    remote: &mut remote::Remote,
    store: &Arc<LocalStore>,
    runtime: &Runtime,
    profile: &Path,
) -> Result<Outcome> {
    let _timing = crate::trace::Span::new("developer.execute");
    validate(&request)?;
    let value = match request {
        Request::GrantVolume { volume, email, write } => {
            if store.volume(volume)?.owner != store.account().id {
                return Err("only the volume owner may grant access".into());
            }
            remote.prepare_publication(store, Some(volume))?;
            let account = remote.resolve_volume_grantee(store, volume, &email)?;
            store.grant_volume(&Access::owner(store.account().id), volume, account, write)?;
            DeveloperReply::VolumeGranted {
                volume,
                account,
                access: if write { VolumeAccess::ReadWrite } else { VolumeAccess::Read },
            }
        }
        Request::ListApps { after } => {
            DeveloperReply::Apps(serde_json::from_value(remote.list_apps(store, after)?)?)
        }
        Request::GetAppDisplay { app, build } => {
            let display = remote.get_app_display(store, &app, &build)?;
            app_display::publish(profile, &display)?;
            DeveloperReply::Display {
                build_id: display.build_id,
                name: display.name,
                icon_error: display.icon_error,
            }
        }
        Request::SetAppEnabled { app, enabled } => {
            let current: AppResponse = serde_json::from_value(remote.get_app(store, &app)?)?;
            DeveloperReply::App(serde_json::from_value(remote.set_app_enabled(
                store,
                &app,
                enabled,
                current.revision,
            )?)?)
        }
        Request::DeleteApp { app } => {
            let current: AppResponse = serde_json::from_value(remote.get_app(store, &app)?)?;
            DeveloperReply::App(serde_json::from_value(remote.delete_app(
                store,
                &app,
                current.revision,
            )?)?)
        }
        Request::PublishApp { bundle, notes } => {
            authenticate_package(remote, store, &bundle)?;
            let package = cache_package(store, runtime, profile, &bundle)?;
            let app = package.identity()?;
            if app.publisher != store.account().id {
                return Err("only the authenticated publisher may publish an app".into());
            }
            remote.prepare_publication(store, None)?;
            let current: AppResponse = serde_json::from_value(remote.get_app(store, app)?)?;
            DeveloperReply::App(serde_json::from_value(remote.publish_app(
                store,
                Id::generate()?,
                app,
                &package.id()?,
                &notes,
                current.revision,
            )?)?)
        }
        Request::CreateAppSession { app, root, user } => {
            let current: AppResponse = serde_json::from_value(remote.get_app(store, &app)?)?;
            let selected = current.app.ok_or("app is unavailable for new sessions")?;
            if selected.identity != app
                || selected.state != crate::network::ap::operation::AppState::Active
            {
                return Err("app is unavailable for new sessions".into());
            }
            let cached = profile.join(crate::paths::BUILDS).join(&selected.build_id);
            remote.ensure_exact_build(store, &selected.build_id, &cached)?;
            let package = Package::open(&cached)?;
            let package = store.authenticate_package(package, crate::store::now_unix_secs())?;
            if package.identity()? != &app {
                return Err("published build app identity differs".into());
            }
            store.remember_imported_app(&package)?;
            let sync_shared = root.is_none()
                && package.initializers().declares(crate::package::AutomaticVolumeRole::Session);
            let root = select_root(store, &app, &package, root)?;
            let session = store.create_session_binding(crate::store::SessionCreation {
                selector: BuildSelector::App { app: app.clone() },
                app,
                resolved_build_id: selected.build_id,
                root_volume: root,
                sync_shared,
                user_volume: user,
            })?;
            DeveloperReply::Session(session)
        }
        Request::CreateBuildSession { build, root, user } => {
            let cached = profile.join(crate::paths::BUILDS).join(&build);
            remote.ensure_exact_build(store, &build, &cached)?;
            let package = authenticate_package(remote, store, &cached)?;
            if package.id()? != build {
                return Err("cached build identity differs".into());
            }
            let app = package.identity()?.clone();
            store.remember_imported_app(&package)?;
            let sync_shared = root.is_none()
                && package.initializers().declares(crate::package::AutomaticVolumeRole::Session);
            let root = select_root(store, &app, &package, root)?;
            provision_owned_root(store, root, &build)?;
            let session = store.create_session_binding(crate::store::SessionCreation {
                app,
                selector: BuildSelector::Exact { build_id: build.clone() },
                resolved_build_id: build,
                root_volume: root,
                sync_shared,
                user_volume: user,
            })?;
            DeveloperReply::Session(session)
        }
        Request::PrepareApp { bundle } => {
            authenticate_package(remote, store, &bundle)?;
            cache_package(store, runtime, profile, &bundle)?;
            DeveloperReply::AppPrepared
        }
        Request::AuthenticatePackage { bundle } => {
            let package = authenticate_package(remote, store, &bundle)?;
            package_identity(store, &package)?
        }
        Request::SignPackage { bundle } => {
            if store.publisher_signer(crate::store::now_unix_secs()).is_err() {
                remote.issue_publisher_certificate(store)?;
            }
            DeveloperReply::PackageSigned { build_id: sign_package(store, &bundle)? }
        }
        Request::CreateSession { bundle, root, user } => {
            authenticate_package(remote, store, &bundle)?;
            DeveloperReply::Session(create_session(store, runtime, profile, &bundle, root, user)?)
        }
        Request::UpdateSession { session, bundle } => {
            remote.check_cancellation()?;
            update_session(remote, store, runtime, profile, session, &bundle)?
        }
        _ => return Err("not a developer request".into()),
    };
    Ok(Outcome { value })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn source_initialization_needs_no_recovery_key_and_allocates_no_data_backing() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        assert!(engine.services.store.recovery_state().unwrap().is_none());
        let pending = engine.services.store.pending_operations().unwrap().len();
        let app = crate::package::AppIdentity {
            publisher: engine.services.store.account().id,
            app_id: "scaffold-only".into(),
        };
        let expected =
            app.volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None).unwrap();
        for name in ["Source app", "Renamed source app"] {
            let reply = engine
                .dispatch_reply(Request::InitializeApp {
                    app_id: app.app_id.clone(),
                    name: name.into(),
                    declaration: Box::new(declaration.clone()),
                })
                .unwrap();
            let crate::engine::reply::Reply::Volume(volume) = reply else {
                panic!("World descriptor")
            };
            assert_eq!(volume.id, expected);
            assert_eq!(volume.owner, app.publisher);
            assert!(matches!(
                engine.services.store.volume(expected),
                Err(crate::store::Error::NotFound(_))
            ));
            assert!(
                !directory.path().join(crate::paths::VOLUMES).join(expected.to_string()).exists()
            );
            assert!(engine.services.store.volumes().unwrap().is_empty());
            assert!(engine.services.store.sessions().unwrap().is_empty());
            assert_eq!(engine.services.store.pending_operations().unwrap().len(), pending);
            assert!(engine.services.store.recovery_state().unwrap().is_none());
        }
    }

    #[test]
    fn invalid_local_bundle_or_initializer_creates_no_volumes_or_sessions() {
        // Reject incomplete/corrupt installations before publishing a fresh root.
        let directory = tempfile::tempdir().unwrap();
        let resources = tempfile::tempdir().unwrap();
        let bundle = resources.path().join("todo.pbapp");
        let mut engine = Engine::open(directory.path()).unwrap();
        let mut remote = engine.remote.take().unwrap();
        let package = crate::engine::tests::button_package();
        let mut invalid_seed = package.to_bytes().unwrap();
        let entry = package.index().entry(crate::package::codec::INITIALIZERS).unwrap();
        invalid_seed[entry.offset as usize] = b'x';
        for bytes in [b"broken app".to_vec(), invalid_seed] {
            fs::write(&bundle, bytes).unwrap();
            assert!(
                execute(
                    Request::CreateSession { bundle: bundle.clone(), root: None, user: None },
                    &mut remote,
                    &engine.services.store,
                    &engine.runtime,
                    directory.path()
                )
                .is_err()
            );
            assert!(engine.services.store.volumes().unwrap().is_empty());
            assert!(engine.services.store.sessions().unwrap().is_empty());
        }
    }

    #[test]
    fn recovered_world_defers_session_publication_until_index_arrives() {
        // Recovery installs descriptors before replica contents. Local launch must
        // retain the session without provisioning or replacing the empty index.
        use crate::package::AutomaticVolumeRole as Role;
        use crate::store::DatabaseId;
        use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};
        let source_directory = tempfile::tempdir().unwrap();
        let source = Engine::open(source_directory.path()).unwrap();
        let package = crate::package::signature::testing::signed(
            &source.services.store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        let bundle = source_directory.path().join("app.pbapp");
        fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let original = create_session(
            &source.services.store,
            &source.runtime,
            source_directory.path(),
            &bundle,
            None,
            None,
        )
        .unwrap();
        let world = source.services.store.open_automatic_volume(&original.id, Role::Root).unwrap();
        let recovered_directory = tempfile::tempdir().unwrap();
        let mut recovered = LocalStore::open(recovered_directory.path()).unwrap();
        recovered.bind_authenticated_account(source.services.store.account().id).unwrap();
        recovered
            .import_recovery_catalog(&source.services.store.recovery_catalog().unwrap(), || true)
            .unwrap();
        let claims = &package.publisher_certificate().unwrap().claims;
        let now = crate::store::now_unix_secs();
        let status = crate::network::ap::publisher::KeyStatus::issue(
            crate::network::ap::publisher::KeyStatusClaims {
                issuer: claims.issuer,
                publisher: claims.publisher,
                signer: claims.signer,
                epoch: claims.revocation_epoch,
                checked_at: now,
                revoked: false,
            },
            SigningKey::from_bytes(&[7; 32]).to_pkcs8_der().unwrap().as_bytes(),
        )
        .unwrap();
        recovered.cache_publisher_status(claims.issuer, &status.to_bytes(), now).unwrap();
        drop(recovered);
        let mut recovered = Engine::open(recovered_directory.path()).unwrap();
        let session = create_session(
            &recovered.services.store,
            &recovered.runtime,
            recovered_directory.path(),
            &bundle,
            None,
            None,
        )
        .unwrap();
        let store = &recovered.services.store;
        assert_eq!(session.root_volume, world.id);
        assert!(store.is_local_session(&session.id).unwrap());
        assert!(store.replica_root(world.id, DatabaseId::Index).unwrap().is_none());
        store.publish_automatic_session(&session.id).unwrap();
        assert!(store.is_local_session(&session.id).unwrap());
        recovered.set_work_offline(true).unwrap();
        assert!(
            recovered.prepare_session(&session.id).unwrap_err().to_string().contains("Offline")
        );
        let store = &recovered.services.store;
        let root_slot = || {
            store
                .automatic_volumes(&session.id)
                .unwrap()
                .into_iter()
                .find(|s| s.role == Role::Root)
                .unwrap()
        };
        assert_eq!(root_slot().state, crate::store::AutomaticVolumeState::Unavailable);
        assert!(matches!(
            store.open_automatic_volume(&session.id, Role::Root),
            Err(crate::store::Error::AccessDenied(_))
        ));
        // Deliver the original policy through the real replica merge, then
        // publish the same retained session without reseeding or changing data.
        source
            .services
            .store
            .with_replica(world.id, DatabaseId::Index, |source| {
                store.with_replica(world.id, DatabaseId::Index, |target| {
                    crate::store::merge_index_replica(
                        recovered_directory.path(),
                        source,
                        target,
                        kvdb::Lww,
                        |_| Ok(()),
                    )
                })
            })
            .unwrap();
        store.publish_automatic_session(&session.id).unwrap();
        assert!(!store.is_local_session(&session.id).unwrap());
        assert_eq!(store.session(&session.id).unwrap(), session);
        assert_eq!(root_slot().state, crate::store::AutomaticVolumeState::Ready);
        assert_eq!(store.open_automatic_volume(&session.id, Role::Root).unwrap(), world);
        assert_eq!(
            store.replica_root(world.id, DatabaseId::Index).unwrap(),
            source.services.store.replica_root(world.id, DatabaseId::Index).unwrap()
        );
        assert!(matches!(
            recovered.prepare_session(&session.id).unwrap(),
            super::super::reply::Reply::SessionPrepared(_)
        ));
    }

    #[test]
    fn local_bundle_existing_root_skips_initializer_and_preserves_volume() {
        // An explicit root is retained across opens; packaged seed data is never reapplied.
        let directory = tempfile::tempdir().unwrap();
        let resources = tempfile::tempdir().unwrap();
        let bundle = resources.path().join("app.pbapp");
        let mut engine = Engine::open(directory.path()).unwrap();
        let package = crate::package::signature::testing::signed(
            &engine.services.store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let root = engine.services.store.create_volume("Existing root", false).unwrap();
        for _ in 0..2 {
            let session = create_session(
                &engine.services.store,
                &engine.runtime,
                directory.path(),
                &bundle,
                Some(root.id),
                None,
            )
            .unwrap();
            assert_eq!(session.root_volume, root.id);
            assert_eq!(engine.services.store.volume(root.id).unwrap(), root);
            engine.prepare_session(&session.id).unwrap();
        }
    }

    #[test]
    fn uncached_build_requires_authenticated_app_metadata_before_creating_a_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Existing root", true).unwrap();
        let build = crate::identifier::encode(&[0x31; 32]);
        let before =
            engine.services.store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap();
        let mut remote = engine.remote.take().unwrap();
        assert!(
            execute(
                Request::CreateBuildSession { build, root: Some(root.id), user: None },
                &mut remote,
                &engine.services.store,
                &engine.runtime,
                directory.path()
            )
            .is_err()
        );
        assert!(engine.services.store.sessions().unwrap().is_empty());
        assert_eq!(
            engine.services.store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap(),
            before
        );
    }

    #[test]
    fn cached_exact_session_registers_its_app_for_inspector_scope_opening() {
        // A downloaded/cached signed build need not have been imported locally.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let package = crate::package::signature::testing::signed(
            &engine.services.store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        let app = package.identity().unwrap().clone();
        let builds = directory.path().join(crate::paths::BUILDS);
        fs::create_dir_all(&builds).unwrap();
        fs::write(builds.join(package.id().unwrap()), package.to_bytes().unwrap()).unwrap();
        assert!(engine.services.store.imported_apps().unwrap().is_empty());
        let mut remote = engine.remote.take().unwrap();
        execute(
            Request::CreateBuildSession { build: package.id().unwrap(), root: None, user: None },
            &mut remote,
            &engine.services.store,
            &engine.runtime,
            directory.path(),
        )
        .unwrap();
        assert_eq!(engine.services.store.imported_apps().unwrap()[0].app, app);
        let user = engine
            .services
            .store
            .open_app_volume(&app, crate::package::AutomaticVolumeRole::User)
            .unwrap();
        assert_eq!(user.owner, engine.services.store.account().id);
    }

    #[test]
    fn invalid_exact_build_or_missing_root_creates_no_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Existing root", true).unwrap();
        let mut remote = engine.remote.take().unwrap();
        for build in ["../content".into(), "AB".repeat(32), "a".repeat(63)] {
            assert!(
                execute(
                    Request::CreateBuildSession { build, root: Some(root.id), user: None },
                    &mut remote,
                    &engine.services.store,
                    &engine.runtime,
                    directory.path()
                )
                .is_err()
            );
        }
        assert!(
            execute(
                Request::CreateBuildSession {
                    build: crate::identifier::encode(&[0x31; 32]),
                    root: Some(Id::generate().unwrap()),
                    user: None
                },
                &mut remote,
                &engine.services.store,
                &engine.runtime,
                directory.path()
            )
            .is_err()
        );
        assert!(engine.services.store.sessions().unwrap().is_empty());
        assert_eq!(engine.services.store.volumes().unwrap(), vec![root]);
    }

    #[test]
    fn signed_out_update_validates_candidate_before_changing_selection() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Existing root", true).unwrap();
        let build = crate::identifier::encode(&[0x31; 32]);
        let mut remote = engine.remote.take().unwrap();
        let session = engine
            .services
            .store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: engine.services.store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        let metadata =
            engine.services.store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap();
        // Offline updates still require a real authenticated candidate.
        let error = execute(
            Request::UpdateSession {
                session: session.id.clone(),
                bundle: directory.path().join("absent.pbapp"),
            },
            &mut remote,
            &engine.services.store,
            &engine.runtime,
            directory.path(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("No such file"));
        assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
        assert_eq!(engine.services.store.volume(root.id).unwrap(), root);
        assert_eq!(
            engine.services.store.replica_root(root.id, crate::store::DatabaseId::Index).unwrap(),
            metadata
        );
    }
}
