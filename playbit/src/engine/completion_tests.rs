//! End-to-end completion delivery through embedding hosts, with real guest turns.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::Engine;
use crate::ui::embedded::{EmbeddedOwner, Reply, Request};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn callback_hosts_advance_independent_guest_turns_and_retire_pending_wakes() {
    let mut contexts = Vec::new();
    for _ in 0..2 {
        let directory = tempfile::tempdir().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let mut engine = Engine::open_with_wake(directory.path(), move || {
            captured.fetch_add(1, Ordering::Relaxed);
            // A host event already queued is sufficient; never wait for it.
            match send.try_send(()) {
                Ok(()) | Err(mpsc::TrySendError::Full(())) => (),
                Err(mpsc::TrySendError::Disconnected(())) => panic!("host retired too soon"),
            }
        })
        .unwrap();
        assert!(engine.completion_socket().is_none() && engine.wake_fd().is_none());
        let root = engine.services.store.create_volume("Callback host", false).unwrap();
        let session = engine.create_session(super::tests::button_package(), root.id).unwrap();
        let mut owner = EmbeddedOwner::new();
        owner.submit(0, Request::Open { session: session.id.clone() }).unwrap();
        let Reply::Opened { instance, .. } =
            owner.advance(&mut engine, false, true, true).response.unwrap().1.unwrap()
        else {
            panic!("explicit Open returns its instance identity")
        };
        contexts.push((directory, engine, owner, receive, count, instance));
    }
    for (_, engine, owner, receive, _, instance) in &mut contexts {
        for id in 1..=3 {
            owner
                .submit(
                    id,
                    Request::Input { instance: *instance, kind: 2, target: 1, data: vec![] },
                )
                .unwrap();
            let mut step = owner.advance(engine, true, true, true);
            while step.response.is_none() {
                assert!(step.error.is_none(), "{:?}", step.error);
                let ready = step.wake_after == Some(Duration::ZERO);
                if !ready {
                    receive.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                step = owner.advance(engine, !ready, true, true);
            }
            let (response_id, result) = step.response.unwrap();
            assert_eq!(response_id, id);
            assert!(result.is_ok(), "{result:?}");
        }
        assert!(engine.poll_shutdown().unwrap());
    }
    for (_, engine, owner, receive, count, _) in contexts {
        let sender = engine.completion.wake.clone();
        sender.notify();
        drop(owner);
        drop(engine);
        let retired_count = count.load(Ordering::Relaxed);
        assert_eq!(Arc::strong_count(&count), 1);
        drop(receive);
        sender.notify();
        assert_eq!(count.load(Ordering::Relaxed), retired_count);
    }
}
