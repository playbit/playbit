#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{notify, transfer};
use super::*;
use std::time::Duration;

#[test]
fn callback_shutdown_rearms_after_unrelated_wake_and_keeps_enrollment_alive() {
    for enrollment in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (wake, wakes) = mpsc::sync_channel(1);
        let mut engine = Engine::open_with_wake(directory.path(), move || {
            wake.try_send(()).unwrap();
        })
        .unwrap();
        let (send, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().enrollment = enrollment;
        assert!(engine.shutdown(Duration::ZERO).is_err());
        engine.completion.wake.notify();
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!engine.poll_shutdown().unwrap());
        assert!(engine.background.as_ref().unwrap().canceled.raised());
        assert!(engine.remote.is_none());
        // The same mailbox/wake ordering as a worker. Enrollment failure has no
        // issued credentials; actual durable enrollment is tested in remote.
        send.send((remote, Err("held operation canceled".into()), BTreeMap::new())).unwrap();
        notify(&engine);
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(engine.poll_shutdown().unwrap());
        if enrollment {
            assert!(engine.remote.is_some());
        }
    }
}

#[test]
fn callback_shutdown_collects_guest_completion_while_enrollment_is_pending() {
    let directory = tempfile::tempdir().unwrap();
    let (wake, wakes) = mpsc::sync_channel(1);
    let mut engine = Engine::open_with_wake(directory.path(), move || match wake.try_send(()) {
        Ok(()) | Err(mpsc::TrySendError::Full(())) => (),
        Err(mpsc::TrySendError::Disconnected(())) => panic!("host retired too soon"),
    })
    .unwrap();
    let root = engine.services.store.create_volume("Pending enrollment", false).unwrap();
    let session = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
    let mut owners = crate::engine::local_owner_test_support::LocalApps::default();
    let instance = owners.open(&mut engine, &session.id).unwrap();
    owners.settle();
    let (send, remote) = transfer(&mut engine);
    engine.background.as_mut().unwrap().enrollment = true;
    owners.input(instance, input_event(2, 1, vec![]).unwrap()).unwrap();
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
    assert!(!engine.poll_shutdown().unwrap());
    owners.quit(&mut engine, instance).unwrap();
    assert!(engine.remote.is_none(), "enrollment is still held independently");
    send.send((remote, Err("held enrollment canceled".into()), BTreeMap::new())).unwrap();
    notify(&engine);
    wakes.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(engine.poll_shutdown().unwrap());
}
