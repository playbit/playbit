//! Test event loop beside the real local Wasm owner, independent of Engine IPC.
#![allow(clippy::unwrap_used, reason = "Fixtures own known instances and bounded waits.")]
use super::*;
use crate::ui::local_guest::Apps;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

pub(crate) struct LocalApps {
    pub apps: Apps,
    wake: Arc<dyn Fn() + Send + Sync>,
    receive: mpsc::Receiver<()>,
}
impl Default for LocalApps {
    fn default() -> Self {
        let (send, receive) = mpsc::sync_channel(1);
        Self {
            apps: Apps::default(),
            wake: Arc::new(move || {
                let _ = send.try_send(());
            }),
            receive,
        }
    }
}
impl LocalApps {
    pub fn open(&mut self, engine: &mut Engine, session: &str) -> Result<Id> {
        let blobs = engine
            .remote
            .as_ref()
            .ok_or("test owner needs retained blob transport")?
            .blob_transport();
        let id = self.apps.open(&engine.services, session, blobs, self.wake.clone())?;
        engine.embedded_sessions = self.apps.sessions();
        Ok(id)
    }
    pub fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.apps.advance(true);
            if self.apps.settled() {
                return;
            }
            let now = Instant::now();
            assert!(now < deadline, "local guest did not settle");
            if self.apps.wake_after(now) != Some(Duration::ZERO) {
                self.receive.recv_timeout(deadline.saturating_duration_since(now)).unwrap();
            }
        }
    }
    pub fn input(&mut self, id: Id, event: rt::Event) -> Result<()> {
        self.apps.input(id, event)?;
        self.settle();
        Ok(())
    }
    pub fn quit(&mut self, engine: &mut Engine, id: Id) -> Result<()> {
        self.apps.deactivate(id)?;
        self.settle();
        if self.apps.drain_pending(id)? {
            return Err("accepted work did not drain".into());
        }
        self.apps.close(id)?;
        engine.embedded_sessions = self.apps.sessions();
        Ok(())
    }
}

#[test]
fn frame_reads_preserve_guest_turns_and_control_validation() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Local controls", false).unwrap();
    let session = engine.create_session(super::tests::button_package(), root.id).unwrap();
    let mut owners = LocalApps::default();
    let instance = owners.open(&mut engine, &session.id).unwrap();
    owners.settle();
    let initial = owners.apps.frame_bytes(instance).unwrap().to_vec();
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn"]);
    for _ in 0..16 {
        assert_eq!(owners.apps.frame_bytes(instance).unwrap(), initial);
        assert_eq!(owners.apps.logs(instance).unwrap(), ["turn"]);
    }
    owners.input(instance, input_event(2, 1, vec![]).unwrap()).unwrap();
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
    assert!(owners.input(instance, input_event(2, 4242, vec![]).unwrap()).is_err());
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
    owners.quit(&mut engine, instance).unwrap();
    assert!(owners.apps.frame_bytes(instance).is_err());
}

#[test]
fn independent_instances_share_one_session_and_release_capacity_on_known_quit() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Independent instances", false).unwrap();
    let session = engine.create_session(super::tests::button_package(), root.id).unwrap();
    let mut owners = LocalApps::default();
    let ids: Vec<_> = (0..4).map(|_| owners.open(&mut engine, &session.id).unwrap()).collect();
    owners.settle();
    assert_eq!(ids.iter().collect::<std::collections::BTreeSet<_>>().len(), 4);
    assert!(owners.open(&mut engine, &session.id).unwrap_err().to_string().contains("Four apps"));
    owners.input(ids[0], input_event(2, 1, vec![]).unwrap()).unwrap();
    assert_eq!(owners.apps.logs(ids[0]).unwrap(), ["turn", "turn"]);
    for id in &ids[1..] {
        assert_eq!(owners.apps.logs(*id).unwrap(), ["turn"]);
    }
    owners.quit(&mut engine, ids[0]).unwrap();
    let fresh = owners.open(&mut engine, &session.id).unwrap();
    owners.settle();
    assert!(!ids.contains(&fresh));
    assert_eq!(owners.apps.logs(fresh).unwrap(), ["turn"]);
    for id in ids[1..].iter().copied().chain([fresh]) {
        owners.quit(&mut engine, id).unwrap();
    }
    assert!(engine.active_sessions().is_empty());
}

#[test]
fn guest_deadline_runs_in_the_local_event_loop_without_engine_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let wasm = wat::parse_str(r#"(module
            (import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (import "playbit" "pb_syscall" (func $dispatch_event (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (import "playbit" "pb_syscall" (func $dispatch_wake (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $event (param i32 i32 i32) (result i32)
        (i32.store (local.get 0) (i32.const 24))
        (i32.store offset=4 (local.get 0) (i32.const 0))
        (i32.wrap_i64 (call $dispatch_event (i32.const 4)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $wake (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_wake (i32.const 11)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
            (memory (export "memory") 1)
            (global $started (mut i32) (i32.const 0))
            (data (i32.const 8) "turn")
            (func (export "pb_main") (local $status i32)
                (drop (call $log (i32.const 8) (i32.const 4)))
                (if (i32.eqz (global.get $started)) (then
                    (global.set $started (i32.const 1))
                    (if (call $wake (i32.const 1000)) (then unreachable))))
                (loop $events
                    (local.set $status (call $event (i32.const 8192) (i32.const 8224) (i32.const 1024)))
                    (if (i32.eq (local.get $status) (i32.const -6)) (then return))
                    (br $events))))"#).unwrap();
    let package =
        crate::engine::Package::new("test".into(), "Wake".into(), wasm, Default::default())
            .unwrap();
    let root = engine.services.store.create_volume("Root", true).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let mut owners = LocalApps::default();
    let instance = owners.open(&mut engine, &session.id).unwrap();
    owners.settle();
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn"]);
    let started = Instant::now();
    let failure_deadline = started + Duration::from_millis(2500);
    while owners.apps.logs(instance).unwrap().len() < 2 {
        let now = Instant::now();
        assert!(now < failure_deadline, "local guest deadline did not run");
        let wait = owners
            .apps
            .wake_after(now)
            .unwrap_or(failure_deadline - now)
            .min(failure_deadline - now);
        match owners.receive.recv_timeout(wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(error) => panic!("local completion owner retired: {error}"),
        }
        owners.apps.advance(true);
    }
    assert_eq!(owners.apps.wake_after(Instant::now()), None);
    assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
    owners.quit(&mut engine, instance).unwrap();
}
