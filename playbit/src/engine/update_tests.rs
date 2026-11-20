//! Package validation and current-authority metadata adoption.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

fn fixture(count: usize) -> (tempfile::TempDir, Engine, Vec<Session>) {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let sessions = (0..count)
        .map(|_| {
            let root = engine.services.store.create_volume("Instance admission", true).unwrap();
            engine
                .create_session(
                    crate::package::signature::testing::signed(
                        &engine.services.store,
                        crate::engine::tests::button_package(),
                    )
                    .unwrap(),
                    root.id,
                )
                .unwrap()
        })
        .collect();
    (directory, engine, sessions)
}
fn resolved(engine: &Engine, session: &str, package: &Package) -> updates::PreparedUpdate {
    let package =
        crate::package::signature::testing::signed(&engine.services.store, package.clone())
            .unwrap();
    let version = package.id().unwrap();
    fs::write(
        engine.services.path.join(crate::paths::BUILDS).join(&version),
        package.to_bytes().unwrap(),
    )
    .unwrap();
    updates::prepare_candidate(
        &engine.runtime,
        &engine.services.store,
        session,
        &engine.services.path.join(crate::paths::BUILDS),
        serde_json::json!({"resolved_build_id":version,
            "selector":BuildSelector::Exact { build_id: package.id().unwrap() }}),
    )
    .unwrap()
}

fn trapping_package() -> Package {
    Package::new(
        "test".into(),
        "Trapping".into(),
        wat::parse_str(
            r#"(module (memory (export "memory") 1) (func (export "pb_main") unreachable))"#,
        )
        .unwrap(),
        Default::default(),
    )
    .unwrap()
}

#[test]
fn candidate_preparation_and_adoption_never_run_a_guest_initializer() {
    let (_directory, mut engine, sessions) = fixture(1);
    let session = &sessions[0];
    let candidate = resolved(&engine, &session.id, &trapping_package());
    let version = candidate.candidate.resolved_build_id.clone();
    engine.finish_update(&session.id, Ok(candidate));
    assert!(!engine.updates.contains_key(&session.id));
    assert_eq!(engine.services.store.session(&session.id).unwrap().resolved_build_id, version);
    // Repeated preparation is metadata-only too. An initializer that traps is
    // structurally valid; its execution failure belongs to its local owner.
    let repeated = resolved(&engine, &session.id, &trapping_package());
    engine.finish_update(&session.id, Ok(repeated));
}

#[test]
fn candidate_adoption_rechecks_current_revocation_and_personal_selection() {
    for revoke in [false, true] {
        let (_directory, mut engine, sessions) = fixture(1);
        let old = &sessions[0];
        let candidate = resolved(&engine, &old.id, &trapping_package());
        if revoke {
            engine.services.store.record_session_denial(old, "revoked during preparation").unwrap();
        } else {
            let personal = engine.services.store.create_volume("Personal", true).unwrap();
            engine
                .services
                .store
                .select_personal_volumes(
                    &candidate.candidate.id,
                    vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: false }],
                )
                .unwrap();
        }
        engine.finish_update(&old.id, Ok(candidate));
        assert_eq!(engine.updates[&old.id].kind, UpdateKind::Failed);
        assert_eq!(engine.services.store.session(&old.id).unwrap(), *old);
    }
}

#[test]
fn candidate_keeps_session_mounts_and_rejects_stale_selection() {
    let (_directory, mut engine, sessions) = fixture(1);
    let old = &sessions[0];
    let personal = engine.services.store.create_volume("Personal", true).unwrap();
    engine
        .services
        .store
        .select_personal_volumes(
            &old.id,
            vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: true }],
        )
        .unwrap();
    let candidate = resolved(&engine, &old.id, &trapping_package());
    let target = candidate.candidate.resolved_build_id.clone();
    engine.finish_update(&old.id, Ok(candidate));
    assert!(!engine.updates.contains_key(&old.id));
    assert_eq!(engine.services.store.session(&old.id).unwrap().resolved_build_id, target);
    assert_eq!(
        engine.services.store.personal_volume_grants(&old.id).unwrap().unwrap()[0].volume,
        personal.id
    );
    let stale = resolved(&engine, &old.id, &trapping_package());
    let mut package = crate::engine::tests::button_package();
    package = package.renamed("Newer selection").unwrap();
    let newer = resolved(&engine, &old.id, &package);
    engine.finish_update(&old.id, Ok(newer));
    let retained = engine.services.store.session(&old.id).unwrap();
    engine.finish_update(&old.id, Ok(stale));
    assert_eq!(engine.updates[&old.id].kind, UpdateKind::Failed);
    assert_eq!(engine.services.store.session(&old.id).unwrap(), retained);
}

#[test]
fn candidate_validation_rejects_wrong_identity_and_malformed_guest_before_adoption() {
    let (_directory, engine, sessions) = fixture(1);
    let old = &sessions[0];
    let mut malformed = crate::engine::tests::button_package();
    malformed = Package::new(
        "test".into(),
        malformed.name().to_owned(),
        b"\0asm\x01\0\0\0\x01\xff".to_vec(),
        Default::default(),
    )
    .unwrap();
    let malformed =
        crate::package::signature::testing::signed(&engine.services.store, malformed).unwrap();
    let version = malformed.id().unwrap();
    fs::write(
        engine.services.path.join(crate::paths::BUILDS).join(&version),
        malformed.to_bytes().unwrap(),
    )
    .unwrap();
    let resolution = serde_json::json!({"resolved_build_id":version,
        "selector":BuildSelector::Exact {build_id:version.clone()}});
    assert!(
        updates::prepare_candidate(
            &engine.runtime,
            &engine.services.store,
            &old.id,
            &engine.services.path.join(crate::paths::BUILDS),
            resolution
        )
        .is_err()
    );
    fs::write(
        engine.services.path.join(crate::paths::BUILDS).join(&old.resolved_build_id),
        malformed.to_bytes().unwrap(),
    )
    .unwrap();
    let resolution =
        serde_json::json!({"resolved_build_id":old.resolved_build_id,"selector":old.selector});
    assert!(
        updates::prepare_candidate(
            &engine.runtime,
            &engine.services.store,
            &old.id,
            &engine.services.path.join(crate::paths::BUILDS),
            resolution
        )
        .is_err()
    );
    assert_eq!(engine.services.store.session(&old.id).unwrap(), *old);
}

#[test]
fn promoted_automatic_session_adopts_code_without_admitting_stale_guest_access() {
    // Use the developer creation/Root-first-use path, then adopt validated code.
    // Both implicit World and an explicit root retain data and current authority.
    use crate::package::{AutomaticVolumeRole as Role, VolumeInitializers};
    use crate::store::{EntryId, InitialDatabase, InitialDatabaseKind, RootInitialization};
    for overridden in [false, true] {
        for change in ["none", "revoked", "authority", "consent", "selection"] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let store = engine.services.store.clone();
            let declaration = RootInitialization {
                permissions: Default::default(),
                databases: vec![InitialDatabase {
                    id: EntryId::from_bytes([8; 15]).unwrap(),
                    name: "data".into(),
                    kind: InitialDatabaseKind::KvDb {},
                }],
                blobs: vec![],
            };
            let package = crate::package::signature::testing::signed(
                &store,
                Package::create(
                    "test".into(),
                    "Automatic update".into(),
                    crate::engine::tests::button_package().wasm().unwrap(),
                    Default::default(),
                    &VolumeInitializers::single(Role::Root, declaration.clone()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
            let source = directory.path().join("automatic.pbapp");
            fs::write(&source, package.to_bytes().unwrap()).unwrap();
            let root = overridden.then(|| {
                store
                    .initialize_root(Id::generate().unwrap(), "Explicit root", true, &declaration)
                    .unwrap()
                    .id
            });
            let old = developer::create_session(
                &store,
                &engine.runtime,
                &engine.services.path,
                &source,
                root,
                None,
            )
            .unwrap();
            store.open_automatic_volume(&old.id, Role::Root).unwrap();
            assert!(!store.is_local_session(&old.id).unwrap());
            assert!(store.has_automatic_scopes(&old.id).unwrap());
            let entry = crate::store::volume_index::EntryAddress {
                volume: old.root_volume,
                entry: EntryId::from_bytes([8; 15]).unwrap(),
            };
            let prior_access =
                Access::session_version(store.account().id, &old.id, &old.resolved_build_id);
            store.kv_put(&prior_access, entry, b"task", b"retained draft").unwrap();
            let candidate = resolved(&engine, &old.id, &trapping_package());
            let target = candidate.candidate.resolved_build_id.clone();
            let candidate_access = Access::session_version(store.account().id, &old.id, &target);
            assert!(matches!(
                store.kv_read(&candidate_access, entry, b"task", 0, 64, None),
                Err(crate::store::Error::AccessDenied("local session build changed"))
            ));
            match change {
                "revoked" => {
                    store.record_session_denial(&old, "revoked during preparation").unwrap();
                }
                "authority" => {
                    store.invalidate_authority(&prior_access, old.root_volume).unwrap();
                }
                "consent" => {
                    let personal = store.create_volume("Personal", true).unwrap();
                    store
                        .select_personal_volumes(
                            &old.id,
                            vec![crate::store::PersonalVolumeGrant {
                                volume: personal.id,
                                write: false,
                            }],
                        )
                        .unwrap();
                }
                "selection" => {
                    let newer =
                        resolved(&engine, &old.id, &trapping_package().renamed("Newer").unwrap());
                    engine.finish_update(&old.id, Ok(newer));
                    assert!(!engine.updates.contains_key(&old.id));
                }
                "none" => {}
                _ => unreachable!(),
            }
            let retained = store.session(&old.id).unwrap();
            engine.finish_update(&old.id, Ok(candidate));
            if change == "none" {
                assert_eq!(
                    engine.updates.get(&old.id).map(|update| update.message.as_str()),
                    None,
                    "automatic session candidate must be admitted after Root promotion"
                );
                assert_eq!(store.session(&old.id).unwrap().resolved_build_id, target);
                assert_eq!(
                    store.kv_read(&candidate_access, entry, b"task", 0, 64, None).unwrap().bytes,
                    b"retained draft"
                );
                assert!(matches!(
                    store.kv_read(&prior_access, entry, b"task", 0, 64, None),
                    Err(crate::store::Error::AccessDenied("local session build changed"))
                ));
            } else {
                assert_eq!(engine.updates[&old.id].kind, UpdateKind::Failed);
                assert_eq!(store.session(&old.id).unwrap(), retained);
                if change == "authority" {
                    assert!(store.kv_read(&prior_access, entry, b"task", 0, 64, None).is_err());
                } else {
                    assert_eq!(
                        store
                            .kv_read(
                                &Access::owner(store.account().id),
                                entry,
                                b"task",
                                0,
                                64,
                                None
                            )
                            .unwrap()
                            .bytes,
                        b"retained draft"
                    );
                }
            }
        }
    }
}

#[test]
fn automatic_foreign_root_update_requires_candidate_provisioning() {
    // Import a read-only foreign root, then create a normal automatic binding
    // against it. Current-version access must not admit an unprovisioned candidate.
    fn controls(
        source: &crate::store::LocalStore,
        target: &crate::store::LocalStore,
        volume: Id,
        scratch: &Path,
    ) {
        for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
            source
                .with_replica(volume, database, |source| {
                    target.with_replica(volume, database, |target| {
                        // This isolated fixture transfers a trusted complete control
                        // snapshot under both stores' operation locks; no entries exist.
                        if database == crate::store::DatabaseId::Index {
                            crate::store::merge_index_replica(
                                scratch,
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            crate::store::merge_replica(scratch, source, target, kvdb::Lww, |_| {
                                Ok(())
                            })
                        }
                    })
                })
                .unwrap();
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let store = engine.services.store.clone();
    let owner_directory = tempfile::tempdir().unwrap();
    let owner = crate::store::LocalStore::open(owner_directory.path()).unwrap();
    let owner_access = Access::owner(owner.account().id);
    let root = owner.create_volume("Foreign root", true).unwrap();
    owner.grant_volume(&owner_access, root.id, store.account().id, false).unwrap();
    let package =
        crate::package::signature::testing::signed(&store, crate::engine::tests::button_package())
            .unwrap();
    let version = package.id().unwrap();
    owner.provision_version(&owner_access, root.id, &version).unwrap();
    let shared_package =
        crate::package::signature::testing::signed(&owner, crate::engine::tests::button_package())
            .unwrap();
    let shared_build = shared_package.id().unwrap();
    let shared = owner
        .create_session(
            shared_package.identity().unwrap(),
            BuildSelector::Exact { build_id: shared_build.clone() },
            &shared_build,
            root.id,
        )
        .unwrap();
    store
        .import_invitation_link(
            &owner.invitation(&shared.id, engine.channel()).unwrap(),
            &owner.session_envelope(&shared.id).unwrap(),
            engine.channel(),
        )
        .unwrap();
    controls(&owner, &store, root.id, directory.path());
    let source = directory.path().join("foreign-root.pbapp");
    fs::write(&source, package.to_bytes().unwrap()).unwrap();
    let old = developer::create_session(
        &store,
        &engine.runtime,
        &engine.services.path,
        &source,
        Some(root.id),
        None,
    )
    .unwrap();
    assert!(!store.is_local_session(&old.id).unwrap());
    assert!(store.has_automatic_scopes(&old.id).unwrap());
    let prior_access = Access::session_version(store.account().id, &old.id, &old.resolved_build_id);
    store
        .authorize_replication(
            &prior_access,
            root.id,
            crate::store::object_contract::Permission::Read,
        )
        .unwrap();
    let candidate = resolved(&engine, &old.id, &trapping_package());
    let target = candidate.candidate.resolved_build_id.clone();
    engine.finish_update(&old.id, Ok(candidate));
    assert_eq!(engine.updates[&old.id].message, "access denied: root version provisioning");
    assert_eq!(store.session(&old.id).unwrap(), old);
    owner.provision_version(&owner_access, root.id, &target).unwrap();
    controls(&owner, &store, root.id, directory.path());
    let candidate = resolved(&engine, &old.id, &trapping_package());
    engine.finish_update(&old.id, Ok(candidate));
    assert_eq!(engine.updates.get(&old.id).map(|update| update.message.as_str()), None);
    let selected = store.session(&old.id).unwrap();
    assert_eq!(selected.resolved_build_id, target);
    assert!(matches!(
        store.authorize_replication(
            &prior_access,
            root.id,
            crate::store::object_contract::Permission::Read
        ),
        Err(crate::store::Error::AccessDenied("local session build changed"))
    ));
    store
        .authorize_replication(
            &Access::session_version(store.account().id, &old.id, &target),
            root.id,
            crate::store::object_contract::Permission::Read,
        )
        .unwrap();
}
