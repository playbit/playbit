#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

#[test]
fn app_name_is_retained_without_a_build_and_survives_restart() {
    // Persist presentation beside the session; deleting cached code cannot erase it.
    let (dir, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xcd; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    assert!(!store.session_app_names().unwrap().contains_key(&session.id));
    store.remember_app_name(&build, "Notes").unwrap();
    assert_eq!(store.session_app_names().unwrap()[&session.id], "Notes");
    assert!(!dir.path().join(crate::paths::BUILDS).join(&build).exists());
    assert_eq!(store.session(&session.id).unwrap(), session);
    drop(store);
    let reopened = LocalStore::open(dir.path()).unwrap();
    assert_eq!(reopened.session_app_names().unwrap()[&session.id], "Notes");
}

#[test]
fn local_order_and_leave_survive_restart_without_changing_capabilities() {
    let (dir, store, root, owner) = setup();
    let build = crate::identifier::encode(&[0xcd; 32]);
    let a = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let b = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let invitation =
        store.invitation(&a.id, crate::release_channel::ReleaseChannel::Public).unwrap();
    let pending = serde_json::to_value(store.pending_operations().unwrap()).unwrap();
    let object = store
        .create_object_with_fields(&owner, data(&store, root.id), id(1), &title("kept"))
        .unwrap();
    let catalog = store.recovery_catalog().unwrap();
    store.order_sessions(&[b.id.clone(), a.id.clone()]).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), [b.clone(), a.clone()]);
    store.forget_session(&a.id).unwrap();
    store.forget_session(&a.id).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), *std::slice::from_ref(&b));
    assert_eq!(
        store.invitation(&a.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        invitation
    );
    assert_eq!(store.session(&a.id).unwrap(), a);
    assert_eq!(serde_json::to_value(store.pending_operations().unwrap()).unwrap(), pending);
    assert!(store.order_sessions(std::slice::from_ref(&a.id)).is_err());
    assert!(store.order_sessions(&[b.id.clone(), b.id.clone()]).is_err());
    assert!(store.order_sessions(&[crate::identifier::encode(&[0x00; 32])]).is_err());
    assert_eq!(store.retained_sessions().unwrap(), *std::slice::from_ref(&b));
    assert_eq!(store.recovery_catalog().unwrap(), catalog);
    store.import_recovery_catalog(&catalog, || true).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), *std::slice::from_ref(&b));
    // A repeated account projection/re-import must not undo device-local Leave.
    store
        .import_invitation_link(
            &invitation,
            &store.session_envelope(&a.id).unwrap(),
            crate::release_channel::ReleaseChannel::Public,
        )
        .unwrap();
    assert_eq!(store.retained_sessions().unwrap(), *std::slice::from_ref(&b));
    assert!(
        store
            .query_objects(&owner, data(&store, root.id), id(1), None, 64)
            .unwrap()
            .objects
            .iter()
            .any(|o| o.object_id == object.object_id)
    );
    drop(store);
    let store = LocalStore::open(dir.path()).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), *std::slice::from_ref(&b));
    store.retain_session(&a.id).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), [b.clone(), a.clone()]);
    store.forget_session(&a.id).unwrap();
    store.complete_invitation(&a.id).unwrap();
    assert_eq!(store.retained_sessions().unwrap(), [b, a]);
    assert!(store.retain_session(&crate::identifier::encode(&[0x00; 32])).is_err());
}

#[test]
fn stale_order_appends_new_sessions_and_retains_explicit_order() {
    let (_dir, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xcd; 32]);
    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(
            store
                .create_session(
                    &crate::package::AppIdentity {
                        publisher: store.account().id,
                        app_id: "test".into(),
                    },
                    BuildSelector::Exact { build_id: build.clone() },
                    &build,
                    root.id,
                )
                .unwrap(),
        );
    }
    let ids = [sessions[1].id.clone(), sessions[0].id.clone()];
    store.order_sessions(&ids).unwrap();
    assert_eq!(
        store.retained_sessions().unwrap(),
        [sessions[1].clone(), sessions[0].clone(), sessions[2].clone()]
    );
    assert!(store.order_sessions(&vec![ids[0].clone(); 257]).is_err());
}

#[test]
#[ignore = "reference-device local session-list latency; never run as a CI benchmark"]
fn benchmark_session_list_projection_and_durable_mutations() {
    fn report(name: &str, count: usize, mut samples: Vec<f64>, budget: Option<f64>) {
        samples.sort_by(f64::total_cmp);
        let p99 = samples[989];
        crate::diagnostic!(
            "{name} rows={count} n=1000 p50={:.6} p99={p99:.6} max={:.6} ms",
            samples[499],
            samples[999]
        );
        if let Some(budget) = budget {
            assert!(p99 < budget, "{name} p99 exceeds {budget}ms");
        }
    }
    for count in [4, 128, 256] {
        let (_dir, store, root, _) = setup();
        let build = crate::identifier::encode(&[0xcd; 32]);
        let mut ids = Vec::new();
        for _ in 0..count {
            ids.push(
                store
                    .create_session(
                        &crate::package::AppIdentity {
                            publisher: store.account().id,
                            app_id: "test".into(),
                        },
                        BuildSelector::Exact { build_id: build.clone() },
                        &build,
                        root.id,
                    )
                    .unwrap()
                    .id,
            );
        }
        let mut baseline = Vec::new();
        let mut project = Vec::new();
        let mut order = Vec::new();
        let mut leave = Vec::new();
        let mut retain = Vec::new();
        let mut usage = Vec::new();
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            assert_eq!(store.sessions().unwrap().len(), count);
            baseline.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            assert_eq!(store.retained_sessions().unwrap().len(), count);
            project.push(start.elapsed().as_secs_f64() * 1000.0);
            ids.rotate_left(1);
            let start = std::time::Instant::now();
            store.record_session_use(&ids[0]).unwrap();
            usage.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            store.order_sessions(&ids).unwrap();
            order.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            store.forget_session(&ids[0]).unwrap();
            leave.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            store.retain_session(&ids[0]).unwrap();
            retain.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        report("catalog-before", count, baseline, None);
        report("project", count, project, (count < 256).then_some(1.0));
        report("order", count, order, Some(2.0));
        report("leave", count, leave, Some(2.0));
        report("retain", count, retain, Some(2.0));
        report("record usage", count, usage, Some(2.0));
    }
}

#[test]
fn recent_usage_survives_restart_independently_of_sidebar_order() {
    // Repeated use moves one identity, preserving manual order and account authority.
    let (dir, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xcd; 32]);
    let a = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let b = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    store.order_sessions(&[a.id.clone(), b.id.clone()]).unwrap();
    store.record_session_use(&a.id).unwrap();
    store.record_session_use(&b.id).unwrap();
    store.record_session_use(&a.id).unwrap();
    store.record_session_use(&a.id).unwrap();
    assert_eq!(store.recent_sessions().unwrap(), [a.id.clone(), b.id.clone()]);
    assert_eq!(store.retained_sessions().unwrap(), [a.clone(), b.clone()]);
    drop(store);
    let store = LocalStore::open(dir.path()).unwrap();
    assert_eq!(store.recent_sessions().unwrap(), [a.id, b.id]);
}
