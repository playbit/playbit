#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::{
    engine::{
        Request, ipc, launch,
        reply::{DeveloperReply, Reply},
    },
    package::Package,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Child, Command, Output},
};

struct EngineProcess(Child);
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(profile: &Path) -> EngineProcess {
    EngineProcess(
        launch::ensure(profile, Path::new(env!("CARGO_BIN_EXE_playbit-engine"))).unwrap().unwrap(),
    )
}
fn call(profile: &Path, request: Request) -> Reply {
    ipc::call_archive(profile, &request).unwrap().into_owned().unwrap()
}
fn pb(profile: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pb"))
        .env("PLAYBIT_PROFILE", profile)
        .args(args)
        .output()
        .unwrap()
}
fn success(output: Output) -> Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn command_forgets_only_one_session_and_notifies_observers() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path();
    // Install explicit isolated offline signing authority before handing this
    // profile to its engine. No AP enrollment or consent is fabricated.
    let mut view_state = playbit_api::context::ContextState::default();
    let mut view = view_state.view();
    view.column_start();
    view.button(1, "Still running", playbit_api::ui::ButtonStyle::Default, false);
    view.column_end();
    let bytes = view.finish();
    let encoded: String = bytes.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let wasm = wat::parse_str(format!(
        r#"(module
        (import "playbit" "pb_syscall" (func $dispatch_view (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
(func $view (param i32 i32) (result i32)
    (i32.wrap_i64 (call $dispatch_view (i32.const 3)
        (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
        (memory (export "memory") 1) (data (i32.const 1024) "{encoded}")
        (func (export "pb_main")
            (drop (call $view (i32.const 1024) (i32.const {})))))"#,
        bytes.len()
    ))
    .unwrap();
    let package = Package::new("test".into(), "Retained".into(), wasm, BTreeMap::new()).unwrap();
    let store = playbit::store::LocalStore::open(profile).unwrap();
    store.set_work_offline(true).unwrap();
    let package = playbit::package::signature::testing::signed(&store, package).unwrap();
    let bundle = profile.join("retained.pbapp");
    std::fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
    drop(store);
    // After setup, only the engine owns credentials and all session mutations
    // use IPC, as they do for the CLI.
    let engine = start(profile);
    let (a, b, a_code, b_code, volumes) = {
        let Reply::Volume(root) = call(
            profile,
            Request::CreateVolume { share_key_with_service: true, name: "Keep this data".into() },
        ) else {
            panic!("expected volume")
        };
        let create = || {
            let Reply::Developer(DeveloperReply::Session(session)) = call(
                profile,
                Request::CreateSession { bundle: bundle.clone(), root: Some(root.id), user: None },
            ) else {
                panic!("expected session")
            };
            session
        };
        let a = create();
        let b = create();
        let code = |session: &str| {
            let Reply::SessionReference { reference } =
                call(profile, Request::SessionReference { session: session.into() })
            else {
                panic!("expected retained local session reference")
            };
            reference.validate().unwrap();
            assert_eq!(reference.internal_id(), session);
            assert_eq!(reference.code().len(), 22);
            reference.code()
        };
        let a_code = code(&a.id);
        let b_code = code(&b.id);
        let volumes =
            serde_json::to_value(call(profile, Request::Status)).unwrap()["volumes"].clone();
        (a, b, a_code, b_code, volumes)
    };
    let invitation =
        success(pb(profile, &["invite", &a_code]))["invitation"].as_str().unwrap().to_owned();
    for session in [&a.id, &b.id] {
        ipc::call_archive(profile, &Request::PrepareSession { session: session.clone() }).unwrap();
    }
    let token = ipc::ChangeWait::connect(profile, None).unwrap().receive().unwrap().token;
    let observer = ipc::ChangeWait::connect(profile, Some(token)).unwrap();
    assert_eq!(success(pb(profile, &["session", "delete", &a_code])), json!({"forgotten":a_code}));
    assert_ne!(observer.receive().unwrap().token, token);
    let status = success(pb(profile, &["status"]));
    assert_eq!(status["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(status["sessions"][0]["id"], b_code);
    assert_eq!(status["volumes"], volumes);
    let prepared = ipc::call_archive(profile, &Request::PrepareSession { session: b.id.clone() })
        .unwrap()
        .into_owned()
        .unwrap();
    let playbit::engine::reply::Reply::SessionPrepared(prepared) = prepared else {
        panic!("preparation returns metadata without executing a guest")
    };
    assert_eq!(prepared.session, b);
    let missing = pb(profile, &["session", "delete", &"0".repeat(22)]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(!missing.stderr.is_empty());
    drop(engine);
    let _engine = start(profile);
    assert_eq!(success(pb(profile, &["invite", &a_code]))["invitation"], invitation);
    let status = success(pb(profile, &["status"]));
    assert_eq!(status["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(status["sessions"][0]["id"], b_code);
    assert_eq!(status["volumes"], volumes);
    // Explicit preparation retains the already-authorized session metadata again.
    let mut restore = Request::PrepareSession { session: a_code.clone() };
    playbit::engine::session_presentation::normalize_request(
        &mut restore,
        playbit::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    let Reply::SessionPrepared(prepared) = call(profile, restore) else {
        panic!("existing local code prepares retained metadata without executing a guest")
    };
    assert_eq!(prepared.session, a);
    assert_eq!(success(pb(profile, &["status"]))["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(success(pb(profile, &["session", "delete", &a_code])), json!({"forgotten":a_code}));
}
