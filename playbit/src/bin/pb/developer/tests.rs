//! Developer command parsing, atomic publication, source initialization and build boundaries.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use playbit::package::BuildCommand;

fn signing_fixture() -> (
    playbit::package::signature::PackageSigner,
    [u8; 32],
    playbit::package::signature::TrustedSigner,
) {
    use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};
    use playbit::package::signature::*;
    let issuer = SigningKey::from_bytes(&[7; 32]);
    let key = SigningKey::from_bytes(&[8; 32]);
    let claims = CertificateClaims {
        issuer: issuer.verifying_key().to_bytes(),
        publisher: Id::from_bytes([2; 16]),
        signer: key.verifying_key().to_bytes(),
        issued_at: 100,
        expires_at: 1000,
        revocation_epoch: 1,
    };
    let certificate =
        PublisherCertificate::issue(claims.clone(), issuer.to_pkcs8_der().unwrap().as_bytes())
            .unwrap();
    let status = TrustedSigner {
        issuer: claims.issuer,
        publisher: claims.publisher,
        signer: claims.signer,
        epoch: 1,
        revoked: false,
    };
    (PackageSigner::from_seed(certificate, &[8; 32]).unwrap(), claims.issuer, status)
}

fn sign_fixture(request: Request) -> Result<Reply> {
    let Request::SignPackage { bundle } = request else {
        return Err("expected source package signing".into());
    };
    let (signer, _, _) = signing_fixture();
    let package = playbit::package::Package::open(&bundle)?;
    let mut bytes = Vec::new();
    package.write_signed(&signer, 100, &mut bytes)?;
    fs::write(&bundle, &bytes)?;
    let build_id = playbit::package::Package::from_bytes(&bytes)?.id()?;
    Ok(Reply::Developer(DeveloperReply::PackageSigned { build_id }))
}

fn source_fixture(directory: &Path) {
    fs::write(directory.join("playbit.toml"), "app_id='example'\nname='Example'\nbuild_cmd='sh build.sh debug'\nbuild_artifact='_build/debug/app.wasm'\n[files]\nprofile='profile'\n[release]\nbuild_cmd='sh build.sh release'\nbuild_artifact='_build/release/app.wasm'\n").unwrap();
    fs::write(directory.join("build.sh"), "mkdir -p _build/$1\nprintf '\\000asm\\001\\000\\000\\000' > _build/$1/app.wasm\nprintf %s \"$1\" > profile\n").unwrap();
    fs::write(directory.join(playbit::package::VOLUME_INITIALIZERS_FILE), b"{}").unwrap();
}

#[test]
fn source_builds_sign_before_atomic_publication_and_preserve_prior_output_on_failure() {
    // Both profiles carry authenticated publisher identity. Failed signing leaves the last artifact intact.
    let source = tempfile::tempdir().unwrap();
    source_fixture(source.path());
    for release in [false, true] {
        let args =
            if release { vec!["--release".into(), "--dev".into()] } else { vec!["--dev".into()] };
        let output = build_command(&args, source.path(), &sign_fixture).unwrap();
        let path = Path::new(output["bundle"].as_str().unwrap());
        let (_, issuer, status) = signing_fixture();
        let package = playbit::package::Package::open(path)
            .unwrap()
            .authenticate(
                &playbit::package::signature::PublisherTrust {
                    issuers: &[issuer],
                    signers: &[status],
                },
                100,
            )
            .unwrap();
        assert_eq!(package.identity().unwrap().app_id, "example.dev");
        assert_eq!(package.identity().unwrap().publisher, Id::from_bytes([2; 16]));
        assert_eq!(
            package.read("profile", 0, 16).unwrap(),
            if release { b"release".as_slice() } else { b"debug" }
        );
        assert_eq!(package.id().unwrap(), output["build_id"]);
        assert_eq!(bundle_command(&args, source.path(), &sign_fixture).unwrap(), output);
        let prior = fs::read(path).unwrap();
        assert!(
            bundle_command(&args, source.path(), &|_| Err("certificate unavailable".into()))
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), prior);
        assert!(
            fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".staging-"))
        );
    }
    assert_eq!(Manifest::read(source.path()).unwrap().app_id, "example");
}

#[test]
fn source_templates_package_without_signing_and_stay_unexecutable() {
    // Both build and bundle reuse the codec without calling an engine. Invalid input retains the prior template.
    let source = tempfile::tempdir().unwrap();
    source_fixture(source.path());
    let no_engine = |_| -> Result<Reply> { panic!("template packaging reached the engine") };
    for release in [false, true] {
        let mut args = vec!["--template".into()];
        if release {
            args.push("--release".into());
        }
        let output = build_command(&args, source.path(), &no_engine).unwrap();
        assert_eq!(output["template"], true);
        assert!(output.get("build_id").is_none());
        let path = Path::new(output["bundle"].as_str().unwrap());
        let package = playbit::package::Package::open(path).unwrap();
        assert!(package.publisher_certificate().is_none());
        assert!(package.admit_execution(|_| true).is_err());
        assert_eq!(package.app_id(), "example");
        let prior = fs::read(path).unwrap();
        assert_eq!(bundle_command(&args, source.path(), &no_engine).unwrap(), output);
        assert_eq!(fs::read(path).unwrap(), prior);
        fs::write(
            source.path().join(if release {
                "_build/release/app.wasm"
            } else {
                "_build/debug/app.wasm"
            }),
            b"invalid Wasm",
        )
        .unwrap();
        assert!(bundle_command(&args, source.path(), &no_engine).is_err());
        assert_eq!(fs::read(path).unwrap(), prior);
    }
}

#[test]
fn template_native_combinations_fail_before_reading_or_building_source() {
    // Reject the unsupported mode before even requiring a source manifest.
    let source = tempfile::tempdir().unwrap();
    let args = vec!["--template".into(), "--native".into()];
    let no_engine = |_| -> Result<Reply> { panic!("invalid template reached the engine") };
    for result in [
        build_command(&args, source.path(), &no_engine),
        bundle_command(&args, source.path(), &no_engine),
    ] {
        assert_eq!(result.unwrap_err().to_string(), "--template supports only Wasm packages");
    }
    assert_eq!(fs::read_dir(source.path()).unwrap().count(), 0);
}

#[test]
fn publication_preserves_prebuilt_bytes_and_source_namespace_is_invocation_local() {
    // Publishing an exact package bypasses source compilation and cannot rewrite its identity.
    let source = tempfile::tempdir().unwrap();
    source_fixture(source.path());
    let observed = std::cell::RefCell::new(None);
    let result =
        publish(
            &["--dev".into(), "--notes=Preview".into()],
            source.path(),
            &|request| match request {
                Request::SignPackage { .. } => sign_fixture(request),
                Request::PublishApp { bundle, notes } => {
                    assert_eq!(notes, "Preview");
                    let package = playbit::package::Package::open(&bundle)?;
                    assert_eq!(package.app_id(), "example.dev");
                    *observed.borrow_mut() = Some((bundle, package.to_bytes()?));
                    Err("publication observed".into())
                }
                _ => panic!("unexpected publication request"),
            },
        )
        .unwrap_err();
    assert_eq!(result.to_string(), "publication observed");
    assert_eq!(Manifest::read(source.path()).unwrap().app_id, "example");
    let (bundle, bytes) = observed.into_inner().unwrap();
    publish(&[bundle.to_str().unwrap().into()], source.path(), &|request| {
        let Request::PublishApp { bundle, .. } = request else {
            panic!("a prebuilt package was rebuilt")
        };
        assert_eq!(fs::read(bundle)?, bytes);
        Ok(Reply::Developer(DeveloperReply::App(playbit::engine::reply::AppResponse {
            revision: 0,
            app: None,
        })))
    })
    .unwrap();
    assert!(
        publish(&[bundle.to_str().unwrap().into(), "--dev".into()], source.path(), &|_| panic!(
            "identity rewrite reached engine"
        ))
        .is_err()
    );
}

#[test]
fn existing_project_init_retains_source_and_selects_the_calling_publishers_root() {
    // A copied source project binds to each builder without carrying another author's root ID.
    let source = tempfile::tempdir().unwrap();
    let call_for = |publisher, request| -> Result<Reply> {
        let Request::InitializeApp { app_id, name, .. } = request else {
            return Err("expected app root".into());
        };
        let app = playbit::package::AppIdentity { publisher, app_id };
        Ok(Reply::Volume(playbit::store::Volume {
            id: app.volume_id(playbit::package::AutomaticVolumeRole::Root, None, None, None)?,
            owner: publisher,
            name,
            personal: false,
            share_key_with_service: true,
        }))
    };
    let first =
        init(&[], source.path(), &|request| call_for(Id::from_bytes([2; 16]), request)).unwrap();
    let manifest = fs::read(source.path().join("playbit.toml")).unwrap();
    let code = source.path().join("src/app.rs");
    fs::write(&code, "// user work\n").unwrap();
    let readme = source.path().join("README.md");
    fs::write(&readme, "# My app\n").unwrap();
    let again =
        init(&[], source.path(), &|request| call_for(Id::from_bytes([2; 16]), request)).unwrap();
    let copied =
        init(&[], source.path(), &|request| call_for(Id::from_bytes([3; 16]), request)).unwrap();
    assert_eq!(first["volume"], again["volume"]);
    assert_ne!(first["volume"], copied["volume"]);
    assert_eq!(fs::read(source.path().join("playbit.toml")).unwrap(), manifest);
    assert_eq!(fs::read_to_string(code).unwrap(), "// user work\n");
    assert_eq!(fs::read_to_string(readme).unwrap(), "# My app\n");
}

#[test]
fn run_selects_exact_or_followed_apps_and_retains_user_overrides() {
    // The engine receives explicit bindings; namespace conflicts fail before source work.
    let publisher = Id::from_bytes([2; 16]);
    let root = Id::from_bytes([3; 16]);
    let user = Id::from_bytes([4; 16]);
    let options = Options::parse(
        &[
            "--app".into(),
            format!("{publisher}/example"),
            "--root-volume".into(),
            root.to_string(),
            "--user-volume".into(),
            user.to_string(),
        ],
        &["--app", "--root-volume", "--user-volume"],
        true,
    )
    .unwrap();
    let Some(Request::CreateAppSession {
        app,
        root: Some(selected_root),
        user: Some(selected_user),
    }) = run_selection_request(Path::new("/unused"), &options).unwrap()
    else {
        panic!("expected followed app")
    };
    assert_eq!(app.publisher, publisher);
    assert_eq!(selected_root, root);
    assert_eq!(selected_user, user);
    for args in [
        vec!["--dev", "--namespace", "dev"],
        vec!["--namespace=dev", "--dev"],
        vec!["--namespace", "Invalid"],
    ] {
        assert!(
            Options::parse(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                &["--namespace", "--dev"],
                true
            )
            .is_err()
        );
    }
    let resumed = Options {
        session: Some(playbit::identifier::encode(&[5; 32])),
        root: Some(root),
        ..Default::default()
    };
    assert!(validate_run(&resumed).is_err());
}

fn init(
    args: &[String],
    directory: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Value> {
    let installation = crate::project::fixture();
    let executable = installation.path().join("Playbit.app/Contents/MacOS/pb");
    super::init(args, directory, &executable, directory, call)
}
fn initialize_fixture(request: Request) -> Result<Reply> {
    let Request::InitializeApp { .. } = &request else {
        return Err("expected app initialization".into());
    };
    let directory = tempfile::tempdir()?;
    let mut engine = playbit::engine::Engine::open(directory.path())?;
    let reply = engine.dispatch_reply(request)?;
    assert!(engine.services.store.volumes()?.is_empty());
    assert!(engine.services.store.recovery_state()?.is_none());
    Ok(reply)
}

#[test]
fn preparation_waits_for_cache_readiness_without_engine_execution() {
    use playbit::{
        engine::{
            ChangeToken,
            reply::{PreparedSession, TransferKind, TransferStatus},
        },
        store::{BuildSelector, Session, UpdateAuthority},
    };
    let session = Session {
        id: playbit::store::session_id(&[4; 16]).unwrap(),
        selector: BuildSelector::Exact { build_id: playbit::identifier::encode(&[0xcd; 32]) },
        resolved_build_id: playbit::identifier::encode(&[0xcd; 32]),
        root_volume: Id::from_bytes([1; 16]),
        shared_volume: None,
        app: playbit::package::AppIdentity {
            publisher: Id::from_bytes([3; 16]),
            app_id: "test".into(),
        },

        creator: Id::from_bytes([3; 16]),
        update_authority: UpdateAuthority::Creator,
    };
    let step = std::cell::Cell::new(0);
    let token = ChangeToken { generation: Id::from_bytes([4; 16]), sequence: 10 };
    let call = |request| {
        let index = step.get();
        step.set(index + 1);
        Ok(match (index, request) {
            (0, Request::WaitChanges { after: None }) => Reply::Changes(token),
            (1, Request::PrepareSession { session: id }) if id == session.id => Reply::Transfer {
                transfer: TransferStatus { kind: TransferKind::Running, ..Default::default() },
            },
            (2, Request::WaitChanges { after: Some(after) }) if after == token => {
                Reply::Changes(ChangeToken { sequence: 11, ..token })
            }
            (3, Request::TransferStatus) => Reply::Transfer {
                transfer: TransferStatus { kind: TransferKind::Running, ..Default::default() },
            },
            (4, Request::WaitChanges { after: Some(after) }) if after.sequence == 11 => {
                Reply::Changes(ChangeToken { sequence: 12, ..token })
            }
            (5, Request::TransferStatus) => Reply::Transfer {
                transfer: TransferStatus { kind: TransferKind::Complete, ..Default::default() },
            },
            (6, Request::PrepareSession { session: id }) if id == session.id => {
                Reply::SessionPrepared(PreparedSession {
                    session: session.clone(),
                    invitation: "link".into(),
                    availability: "Local".into(),
                    update: None,
                })
            }
            _ => panic!("Preparation created execution or repeated an unrelated operation"),
        })
    };
    assert_eq!(prepare(&session.id, &call).unwrap().session, session);
    assert_eq!(step.get(), 7);
    let calls = std::cell::Cell::new(0);
    let failed = |request| {
        let index = calls.get();
        calls.set(index + 1);
        Ok(match (index, request) {
            (0, Request::WaitChanges { after: None }) => Reply::Changes(token),
            (1, Request::PrepareSession { .. }) => Reply::Transfer {
                transfer: TransferStatus { kind: TransferKind::Running, ..Default::default() },
            },
            (2, Request::WaitChanges { after: Some(after) }) if after == token => {
                Reply::Changes(ChangeToken { sequence: 11, ..token })
            }
            (3, Request::TransferStatus) => Reply::Transfer {
                transfer: TransferStatus {
                    kind: TransferKind::Failed,
                    message: Some("Known failed download".into()),
                    ..Default::default()
                },
            },
            _ => panic!("Failed preparation was retried"),
        })
    };
    assert!(
        prepare(&session.id, &failed).unwrap_err().to_string().contains("Known failed download")
    );
    assert_eq!(calls.get(), 4);
}

#[test]
#[ignore = "builds an isolated generated app with Cargo; run without competing builds"]
fn starter_metadata_is_optional_and_removed_names_do_not_survive() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let installation = crate::project::fixture();
    let executable = installation.path().join("Playbit.app/Contents/MacOS/pb");
    let directory = tempfile::Builder::new()
        .prefix("trace-starter-")
        .tempdir_in(workspace.join("target"))
        .unwrap()
        .keep();
    crate::project::scaffold(&directory, "Trace starter", &executable).unwrap();
    let source = directory.join("src/app.rs");
    let original = fs::read_to_string(&source).unwrap();
    let generated = "#[allow(dead_code)] mod trace_names { include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/target/playbit/metadata.rs\")); }\n";
    fs::write(&source, format!("{original}\nconst _: u16 = trace_names::TODO_TURN;\n")).unwrap();
    let check = || {
        playbit::package::metadata::generate(&directory, &Manifest::read(&directory).unwrap())
            .unwrap();
        Command::new("cargo")
            .current_dir(&directory)
            .args(["check", "--offline", "--lib", "--target", "wasm32-unknown-unknown"])
            .env("CARGO_TARGET_DIR", workspace.join("target/trace-starter-check"))
            .env("RUSTFLAGS", "-D warnings")
            .output()
            .unwrap()
    };
    let accepted = check();
    fs::write(directory.join("accepted.log"), &accepted.stderr).unwrap();
    assert!(accepted.status.success(), "{}", String::from_utf8_lossy(&accepted.stderr));
    fs::write(&source, format!("{original}\nconst _: u16 = trace_names::UNDECLARED_TRACE;\n"))
        .unwrap();
    let refused = check();
    fs::write(directory.join("refused.log"), &refused.stderr).unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("UNDECLARED_TRACE"));
    // The installed build support also handles an uninstrumented app.
    // Keep the same target directory so stale constants would be detectable.
    // Adoption instruments the default TODO app. An uninstrumented app
    // removes those calls along with its declarations, as the guide requires.
    let plain = "pub struct App; impl App { pub const fn new() -> Self { Self } pub fn turn(&mut self, _: &mut playbit_api::context::Context<'_>, _: &mut playbit_api::ui::ViewEncoder<'_>) {} }\n";
    let mut manifest = Manifest::read(&directory).unwrap();
    manifest.trace = None;
    manifest.write(&directory).unwrap();
    fs::write(&source, format!("{plain}\n{generated}")).unwrap();
    let absent = check();
    fs::write(directory.join("absent.log"), &absent.stderr).unwrap();
    assert!(absent.status.success(), "{}", String::from_utf8_lossy(&absent.stderr));
    fs::write(&source, format!("{plain}\n{generated}const _: u16 = trace_names::TODO_TURN;\n"))
        .unwrap();
    let removed = check();
    fs::write(directory.join("removed.log"), &removed.stderr).unwrap();
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("TODO_TURN"));
    playbit::diagnostic!("App trace catalog build evidence: {}", directory.display());
}

#[test]
fn init_creates_destination_directories() {
    // Exercise relative, absolute and existing destinations through the
    // real scaffold, store request and Git initialization.
    for (path, existing, language) in [
        ("new/directory/path", false, "rust"),
        ("absolute/app", false, "ocaml"),
        ("existing", true, "rust"),
    ] {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join(path);
        if existing {
            fs::create_dir(&destination).unwrap();
        }
        let argument =
            if path.starts_with("absolute") { destination.to_str().unwrap() } else { path };
        let result = init(
            &[argument.into(), "--name=Nested app".into(), format!("--lang={language}")],
            parent.path(),
            &initialize_fixture,
        )
        .unwrap();
        assert_eq!(result["source"], destination.canonicalize().unwrap().to_str().unwrap());
        let manifest = Manifest::read(&destination).unwrap();
        assert_eq!(manifest.name, "Nested app");
        assert!(Id::parse(result["volume"].as_str().unwrap()).is_ok());
        assert!(destination.join(".git").is_dir());
        let readme = fs::read_to_string(destination.join("README.md")).unwrap();
        assert!(readme.contains("/Playbit.app/Contents/MacOS"));
        assert!(readme.contains("\npb run\n"));
        assert!(
            destination
                .join(if language == "rust" { "src/app.rs" } else { "src/app.ml" })
                .is_file()
        );
        assert!(!destination.join(".pb-init.json").exists());
        assert!(!parent.path().join("playbit.toml").exists());
    }
}

#[test]
fn init_defaults_to_the_resolved_directory_name() {
    // Exercise the four CLI forms against the real scaffold and store;
    // both the manifest and volume must receive the resolved basename.
    for argument in [Some("foo/bar"), Some("foo/bar/."), Some("."), None] {
        let parent = tempfile::tempdir().unwrap();
        let working_directory = if matches!(argument, Some(".") | None) {
            parent.path().join("foo/cat")
        } else {
            parent.path().to_path_buf()
        };
        fs::create_dir_all(&working_directory).unwrap();
        let expected = if matches!(argument, Some(".") | None) { "cat" } else { "bar" };
        let args: Vec<String> = argument.into_iter().map(String::from).collect();
        let result = init(&args, &working_directory, &|request| {
            let Request::InitializeApp { name, .. } = &request else { panic!() };
            assert_eq!(name, expected);
            initialize_fixture(request)
        })
        .unwrap();
        let source = Path::new(result["source"].as_str().unwrap());
        let manifest = Manifest::read(source).unwrap();
        assert_eq!(manifest.name, expected);
        assert_eq!(manifest.app_id, expected);
    }
}

#[test]
fn init_app_id_uses_the_source_directory_independently_of_display_name() {
    // Exercise defaults and exact overrides through real scaffolding;
    // unsupported filename characters must be removed rather than replaced.
    for (directory, explicit, expected) in [
        ("My App_2.testing", None, "myapp2.testing"),
        (".--APP..-v2---.", None, "app.v2"),
        ("foo-bar.baz", None, "foo-bar.baz"),
        ("你好", Some("custom.app-2"), "custom.app-2"),
    ] {
        let parent = tempfile::tempdir().unwrap();
        let mut args = vec![directory.into(), "--name=Display 🎉".into()];
        if let Some(app_id) = explicit {
            args.extend(["--app-id".into(), app_id.into()]);
        }
        let result = init(&args, parent.path(), &initialize_fixture).unwrap();
        let manifest = Manifest::read(Path::new(result["source"].as_str().unwrap())).unwrap();
        assert_eq!(manifest.name, "Display 🎉");
        assert_eq!(manifest.app_id, expected);
    }
}

#[test]
fn init_requires_an_explicit_app_id_when_the_directory_cannot_supply_one() {
    // Empty and overlong derived IDs fail before intent publication or IPC;
    // an explicit valid ID makes the same destination usable.
    for directory in ["你好!".to_owned(), "a".repeat(65)] {
        let parent = tempfile::tempdir().unwrap();
        let error = init(std::slice::from_ref(&directory), parent.path(), &|_| {
            panic!("invalid app_id reached IPC")
        })
        .unwrap_err();
        assert!(error.to_string().contains("--app-id"));
        assert_eq!(fs::read_dir(parent.path().join(&directory)).unwrap().count(), 0);
        let result =
            init(&[directory, "--app-id=custom".into()], parent.path(), &initialize_fixture)
                .unwrap();
        let manifest = Manifest::read(Path::new(result["source"].as_str().unwrap())).unwrap();
        assert_eq!(manifest.app_id, "custom");
    }
}

#[test]
fn init_supports_writable_destinations_beneath_search_only_ancestors() {
    // The same writable destination must work from inside it or from
    // elsewhere, whether the requested path is relative or absolute.
    use std::os::unix::fs::PermissionsExt;
    for (inside, absolute) in [(true, false), (false, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let restricted = root.path().join("restricted");
        let writable = restricted.join("writable");
        let source = writable.join("new/project");
        fs::create_dir_all(&writable).unwrap();
        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o111)).unwrap();
        let working_directory = if inside { writable.as_path() } else { root.path() };
        let argument = if absolute {
            source.to_str().unwrap()
        } else if inside {
            "new/project"
        } else {
            "restricted/writable/new/project"
        };
        let result = init(&[argument.into()], working_directory, &initialize_fixture);
        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(source.join("playbit.toml").is_file());
    }
}

#[test]
fn init_recovers_interrupted_directory_creation() {
    // Stop immediately after the first mkdir, before syncing its parent.
    // Retry must complete the path and initialize one usable project.
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("new/project");
    INTERRUPT_INIT_DIRECTORY.with(|interrupt| interrupt.set(true));
    assert!(create_init_directory(&destination).is_err());
    assert!(root.path().join("new").is_dir());
    assert!(!destination.exists());
    init(&["new/project".into()], root.path(), &initialize_fixture).unwrap();
    assert!(destination.join("playbit.toml").is_file());
}

#[test]
fn init_does_not_ignore_unreadable_writable_parents() {
    // A writable boundary may contain a mkdir interrupted before fsync.
    // Failure to open it must stop initialization before creating children.
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let restricted = root.path().join("restricted");
    let writable = restricted.join("writable");
    fs::create_dir_all(&writable).unwrap();
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o311)).unwrap();
    let result = init(&["restricted/writable/new/project".into()], root.path(), &|_| {
        panic!("unsynced directory reached IPC")
    });
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert!(!writable.join("new").exists());
}

#[test]
fn init_rejects_conflicts_and_invalid_arguments_before_requesting_a_root() {
    // A file or populated destination must retain user data. Invalid
    // command syntax must fail before creating the requested directory.
    let parent = tempfile::tempdir().unwrap();
    fs::write(parent.path().join("file"), "user data").unwrap();
    fs::create_dir(parent.path().join("occupied")).unwrap();
    fs::write(parent.path().join("occupied/notes"), "notes").unwrap();
    for arguments in [
        vec!["file"],
        vec!["file/child"],
        vec!["occupied"],
        vec!["new/path", "extra"],
        vec!["new/path", "--name"],
        vec!["new/path", "--name=A", "--name=B"],
        vec!["new/path", "--app-id"],
        vec!["new/path", "--app-id=Invalid"],
        vec!["new/path", "--app-id="],
        vec!["new/path", "--app-id=a", "--app-id=b"],
        vec!["new/path", "--lang=rust", "--lang=ocaml"],
        vec!["new/path", "--lang=unknown"],
    ] {
        let args: Vec<String> = arguments.iter().map(|arg| (*arg).into()).collect();
        assert!(
            init(&args, parent.path(), &|_| panic!("invalid destination reached IPC")).is_err()
        );
    }
    assert_eq!(fs::read_to_string(parent.path().join("file")).unwrap(), "user data");
    assert_eq!(fs::read_to_string(parent.path().join("occupied/notes")).unwrap(), "notes");
    assert!(!parent.path().join("new").exists());
}

#[test]
fn starter_uses_installed_sdk_and_does_not_overwrite_source() {
    // Cargo resolves the installed SDK and the single editable app source.
    let installation = crate::project::fixture();
    let executable = installation.path().join("Playbit.app/Contents/MacOS/pb");
    let directory = tempfile::tempdir().unwrap();
    crate::project::scaffold(directory.path(), "Robin's Pinball", &executable).unwrap();
    assert!(crate::project::scaffold(directory.path(), "Again", &executable).is_err());
    let cargo = fs::read_to_string(directory.path().join("Cargo.toml")).unwrap();
    assert!(!cargo.contains("@PLAYBIT_"));
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(directory.path().join(".cargo/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        config["env"]["MACOSX_DEPLOYMENT_TARGET"]["value"].as_str(),
        Some(env!("MACOSX_DEPLOYMENT_TARGET"))
    );
    assert_eq!(config["env"]["MACOSX_DEPLOYMENT_TARGET"]["force"].as_bool(), Some(true));
    let metadata = std::process::Command::new("cargo")
        .args(["metadata", "--offline", "--no-deps", "--format-version", "1"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(metadata.status.success(), "{}", String::from_utf8_lossy(&metadata.stderr));
    assert_eq!(fs::read_dir(directory.path().join("src")).unwrap().count(), 1);
    assert!(!directory.path().join(".playbit").exists());
    assert!(directory.path().join("playbit.toml").exists());
    let manifest: toml::Value = toml::from_str(&cargo).unwrap();
    assert_eq!(manifest["lib"]["path"].as_str(), Some("src/app.rs"));
    assert_eq!(
        Path::new(manifest["dependencies"]["playbit-api"]["path"].as_str().unwrap()),
        Path::new("target/playbit/sdk/playbit-api")
    );
}

#[test]
fn session_create_selects_bundle_and_optional_root() {
    // Creation accepts source-free paths and retains an explicitly selected root.
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("My app.pbapp");
    let root = Id::from_bytes([1; 16]);
    let root_text = root.to_string();
    for (args, expected_root) in [
        (vec!["My app.pbapp"], None),
        (vec![bundle.to_str().unwrap()], None),
        (vec!["My app.pbapp", "--root-volume", &root_text], Some(root)),
        (vec!["--root-volume", &root_text, "My app.pbapp"], Some(root)),
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let Request::CreateSession { bundle: selected_bundle, root: selected_root, user: None } =
            create_session(&args, directory.path()).unwrap()
        else {
            panic!("expected session creation");
        };
        assert_eq!(selected_bundle, bundle);
        assert_eq!(selected_root, expected_root);
    }
}

#[test]
fn session_create_rejects_invalid_selections() {
    // Missing or malformed selections must fail rather than create fresh data.
    let root = Id::from_bytes([1; 16]).to_string();
    let build = playbit::identifier::encode(&[1; 32]);
    for args in [
        vec![],
        vec!["--root-volume", &root],
        vec!["source-directory"],
        vec!["app.pbapp", "extra.pbapp"],
        vec!["app.pbapp", "--root-volume"],
        vec!["app.pbapp", "--root-volume", "invalid"],
        vec!["app.pbapp", "--root-volume", &build],
        vec!["app.pbapp", "--root-volume", &root, "--root-volume", &root],
        vec!["app.pbapp", "--new-instance"],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert!(create_session(&args, Path::new("/unused")).is_err(), "{args:?}");
    }
}

#[test]
fn bundle_run_needs_no_source_project_or_compiler() {
    // Stop at the request boundary: a bundle path must bypass Cargo and source watching.
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("todo.pbapp");
    let requests = std::cell::Cell::new(0);
    let error = run(
            &[bundle.to_str().unwrap().into()],
            directory.path(),
            directory.path(),
            &|request| {
                assert!(matches!(request, Request::CreateSession { bundle: ref path, root: None, user: None }
                    if path == &bundle));
                requests.set(requests.get() + 1);
                Err("fixture stopped after local bundle request".into())
            },
            playbit::release_channel::ReleaseChannel::Public,
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "fixture stopped after local bundle request");
    assert_eq!(requests.get(), 1);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn bundle_run_selects_existing_root_before_or_after_path() {
    // Both option orders must pass the explicit root through without reading source.
    let directory = tempfile::tempdir().unwrap();
    let root = Id::from_bytes([1; 16]);
    for args in [
        vec!["app.pbapp".into(), "--root-volume".into(), root.to_string()],
        vec!["--root-volume".into(), root.to_string(), "app.pbapp".into()],
        vec!["app.pbapp".into(), "--trace".into(), "--root-volume".into(), root.to_string()],
        vec!["--trace".into(), "--root-volume".into(), root.to_string(), "app.pbapp".into()],
        vec!["app.pbapp".into(), "--root-volume".into(), root.to_string(), "--gas-limit=1M".into()],
        vec![
            "--gas-limit".into(),
            "0".into(),
            "--root-volume".into(),
            root.to_string(),
            "app.pbapp".into(),
        ],
    ] {
        let error = run(
            &args,
            directory.path(),
            directory.path(),
            &|request| {
                assert!(
                    matches!(request, Request::CreateSession { bundle, root: Some(id), user: None }
                    if bundle == directory.path().join("app.pbapp") && id == root)
                );
                Err("fixture stopped after local bundle request".into())
            },
            playbit::release_channel::ReleaseChannel::Public,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "fixture stopped after local bundle request");
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn gas_limits_parse_unsigned_decimal_and_decimal_millions() {
    // Zero and omission select the same unmetered launch. Positive values
    // must retain their exact count through both supported flag spellings.
    assert!(Options::parse(&[], &["--gas-limit"], true).unwrap().gas_limit.is_none());
    for (text, expected) in [
        ("0", None),
        ("0M", None),
        ("1", NonZeroU64::new(1)),
        ("00012", NonZeroU64::new(12)),
        ("1M", NonZeroU64::new(1_000_000)),
        ("18446744073709551615", NonZeroU64::new(u64::MAX)),
        ("18446744073709M", NonZeroU64::new(18_446_744_073_709_000_000)),
    ] {
        for args in [vec![format!("--gas-limit={text}")], vec!["--gas-limit".into(), text.into()]] {
            let options = Options::parse(&args, &["--gas-limit"], true).unwrap();
            assert_eq!(options.gas_limit, expected, "{args:?}");
        }
    }
    for text in [
        "",
        "-1",
        "+1",
        "1.5M",
        "1m",
        "1MM",
        "M",
        "1_000",
        " 1",
        "1 ",
        "∞",
        "18446744073709551616",
        "18446744073710M",
    ] {
        let args = [format!("--gas-limit={text}")];
        let error = Options::parse(&args, &["--gas-limit"], true).unwrap_err();
        assert!(error.to_string().contains("invalid --gas-limit"), "{text}: {error}");
    }
    for args in [
        vec!["--gas-limit"],
        vec!["--gas-limit", "--trace"],
        vec!["--gas-limit=1", "--gas-limit", "2"],
        vec!["--gas-limit", "0", "--gas-limit=1"],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert!(Options::parse(&args, &["--gas-limit", "--trace"], true).is_err(), "{args:?}");
    }
    assert!(Options::parse(&["--gas-limit=1".into()], &[], true).is_err());
}

#[test]
fn positive_gas_limit_rejects_native_execution_before_engine_request() {
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["app.app", "--gas-limit=1M"],
        vec!["--native", "--gas-limit=1M"],
        vec!["--gas-limit", "1", "--native"],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let error = run(
            &args,
            directory.path(),
            directory.path(),
            &|_| panic!("native fuel limits cannot reach the engine"),
            playbit::release_channel::ReleaseChannel::Public,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be enforced by native executables"));
    }
}

#[test]
fn bundle_run_rejects_invalid_options_before_engine_request() {
    // Reject ambiguous selections and malformed roots without side effects.
    let directory = tempfile::tempdir().unwrap();
    let root = playbit::identifier::encode(&[0x01; 16]);
    let session = playbit::identifier::encode(&[0x02; 32]);
    for words in [
        vec!["app.pbapp", "--root-volume"],
        vec!["app.pbapp", "--root-volume", "invalid"],
        vec!["app.pbapp", "--root-volume", &root, "--root-volume", &root],
        vec!["app.pbapp", "--release"],
        vec!["app.pbapp", "--build", &session],
        vec!["app.pbapp", "--app", &root],
        vec!["app.pbapp", "other.pbapp"],
        vec!["not-a-bundle"],
    ] {
        let args = words.iter().map(|word| (*word).into()).collect::<Vec<_>>();
        assert!(
            run(
                &args,
                directory.path(),
                directory.path(),
                &|_| { panic!("invalid arguments must not reach the engine") },
                playbit::release_channel::ReleaseChannel::Public
            )
            .is_err(),
            "{words:?}"
        );
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn run_mounts_require_distinct_volumes_and_explicit_valid_rights() {
    let first = playbit::identifier::encode(&[0x01; 16]);
    let second = playbit::identifier::encode(&[0x02; 16]);
    let parse = |args: Vec<String>| Options::parse(&args, &["--mount"], false);
    let options =
        parse(vec!["--mount".into(), first.clone(), "--mount".into(), format!("{second}:r")])
            .unwrap();
    assert_eq!(
        options.mounts,
        vec![
            playbit::store::PersonalVolumeGrant { volume: Id::parse(&first).unwrap(), write: true },
            playbit::store::PersonalVolumeGrant {
                volume: Id::parse(&second).unwrap(),
                write: false
            },
        ]
    );
    for args in [
        vec!["--mount".into()],
        vec!["--mount".into(), "invalid".into()],
        vec!["--mount".into(), format!("{first}:write")],
        vec!["--mount".into(), first.clone(), "--mount".into(), format!("{first}:r")],
    ] {
        assert!(parse(args).is_err());
    }
}

#[test]
fn run_selection_rejects_ambiguous_and_repeated_options() {
    for args in [
        vec!["--incognito"],
        vec!["--release", "stable"],
        vec!["--release", "--release"],
        vec!["--build"],
    ] {
        assert!(
            Options::parse(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                &["--release", "--build"],
                false
            )
            .is_err()
        );
    }
    let options = Options::parse(&["--release".into()], &["--release"], false).unwrap();
    assert!(options.release);
    assert!(!options.trace);
    let options =
        Options::parse(&["--trace".into(), "--release".into()], &["--trace", "--release"], false)
            .unwrap();
    assert!(options.trace && options.release);
    assert!(Options::parse(&["--trace".into(), "--trace".into()], &["--trace"], false).is_err());
    assert!(Options::parse(&["--trace".into()], &["--release"], false).is_err());
    let options = Options::parse(
        &["--fs".into(), "photos".into(), "--fs".into(), "docs::rw".into()],
        &["--fs"],
        false,
    )
    .unwrap();
    assert_eq!(options.fs, ["photos", "docs::rw"]);
}

#[test]
fn verbose_selection_accepts_both_spellings_without_consuming_a_value() {
    for flag in ["-v", "--verbose"] {
        let options = Options::parse(
            &[flag.into(), "--release".into(), "--sdk=builtin".into()],
            &["--verbose", "--release", "--sdk"],
            false,
        )
        .unwrap();
        assert!(options.verbose && options.release);
        assert_eq!(options.sdk, crate::sdk::Policy::Builtin);
    }
    assert!(!Options::parse(&[], &["--verbose"], false).unwrap().verbose);
    assert!(Options::parse(&["-v".into(), "--verbose".into()], &["--verbose"], false).is_err());
    assert!(Options::parse(&["-v".into()], &[], false).is_err());
}

#[test]
fn failed_build_is_reported_and_does_not_touch_previous_bundle() {
    // Fail before engine admission for missing/invalid manifests; retain output on compiler failure.
    let directory = tempfile::tempdir().unwrap();
    let prior = directory.path().join("working.pbapp");
    fs::write(&prior, b"retained").unwrap();
    let no_engine = |_: Request| -> Result<Reply> {
        panic!("invalid source must fail before any engine request")
    };
    let missing = build_command(&[], directory.path(), &no_engine)
        .expect_err("a source build requires playbit.toml")
        .to_string();
    assert!(missing.contains("playbit.toml"));
    assert!(missing.contains(directory.path().to_str().expect("temporary path is UTF-8")));
    assert!(missing.contains("pb init"));
    assert_eq!(fs::read(&prior).expect("prior bundle remains readable"), b"retained");

    let manifest_path = directory.path().join("playbit.toml");
    fs::create_dir(&manifest_path).expect("create an unreadable manifest shape");
    let unreadable = build_command(&[], directory.path(), &no_engine)
        .expect_err("a directory is not a source manifest");
    assert_eq!(
        unreadable.downcast_ref::<std::io::Error>().expect("preserve typed I/O error").kind(),
        std::io::ErrorKind::IsADirectory
    );
    fs::remove_dir(&manifest_path).expect("remove manifest directory");
    fs::write(&manifest_path, "name = [").expect("write malformed source manifest");
    let malformed = build_command(&[], directory.path(), &no_engine)
        .expect_err("invalid TOML must fail before building");
    assert!(malformed.downcast_ref::<toml::de::Error>().is_some());
    let manifest = Manifest {
        app_id: "test".into(),
        name: "test".into(),
        required_host_features: Vec::new(),
        lang: None,
        icon: None,
        files: Default::default(),
        files_dir: None,
        build_cmd: "exit 9".into(),
        build_artifact: None,
        release: BuildCommand { build_cmd: "exit 9".into(), build_artifact: None },
        trace: None,
        native: None,
    };
    manifest.write(directory.path()).unwrap();
    assert!(
        build(
            directory.path(),
            manifest.build_command(BuildMode { release: false, native: false }).unwrap()
        )
        .is_err()
    );
    assert_eq!(fs::read(prior).unwrap(), b"retained");
}

#[test]
fn product_paths_use_cargo_target_profiles_and_validate_filenames() {
    let target = Path::new("/custom-target");
    for release in [false, true] {
        let profile = if release { "release" } else { "debug" };
        for native in [false, true] {
            let extension = if native { "app" } else { "pbapp" };
            let artifact = target.join("triple").join(profile).join("executable");
            let mode = BuildMode { release, native };
            let name = "Robin's <TODO> & 🐚";
            assert_eq!(
                product_path(&artifact, mode, name).unwrap(),
                target.join(profile).join(format!("{name}.{extension}"))
            );
            let limit = 255 - extension.len() - 1;
            product_path(&artifact, mode, &"a".repeat(limit)).unwrap();
            for name in ["", " ", "a/b", "a:b", "a\0b", "a\nb", &"a".repeat(limit + 1)] {
                assert!(product_path(&artifact, mode, name).is_err(), "{name:?}");
            }
        }
    }
}

#[test]
fn artifact_selection_keeps_native_binary_names_and_rejects_ambiguity() {
    // Cargo metadata selects one matching artifact, using its configured target directory.
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("Cargo.toml"), "").unwrap();
    let targets = json!([{"name":"my-app", "kind":["cdylib"], "crate_types":["cdylib"]}, {"name":"my-app", "kind":["bin"], "crate_types":["bin"]}, {"name":"build-script-build", "kind":["custom-build"], "crate_types":["bin"]}]);
    let mut metadata = json!({"packages":[{"manifest_path":source.path().join("Cargo.toml").canonicalize().unwrap(), "targets":targets}], "target_directory":source.path().join("custom-target")});
    for release in [false, true] {
        let profile = if release { "release" } else { "debug" };
        assert_eq!(
            artifact_path(source.path(), BuildMode { release, native: false }, &metadata).unwrap(),
            source
                .path()
                .join("custom-target/wasm32-unknown-unknown")
                .join(profile)
                .join("my_app.wasm")
        );
        assert_eq!(
            artifact_path(source.path(), BuildMode { release, native: true }, &metadata).unwrap(),
            source.path().join("custom-target/aarch64-apple-darwin").join(profile).join("my-app")
        );
    }
    metadata["packages"][0]["targets"][1]["name"] = json!("../outside");
    assert!(
        artifact_path(source.path(), BuildMode { release: false, native: true }, &metadata)
            .is_err()
    );
    metadata["packages"][0]["targets"] = json!([{"name":"one","kind":["bin"],"crate_types":["bin"]},{"name":"two","kind":["bin"],"crate_types":["bin"]}]);
    assert!(
        artifact_path(source.path(), BuildMode { release: false, native: true }, &metadata)
            .is_err()
    );
    assert!(
        artifact_path(source.path(), BuildMode { release: false, native: false }, &metadata)
            .is_err()
    );
}

#[test]
fn starter_builds_after_meta_gen_without_checkout_access() {
    // A short fixture-local socket isolates the sandboxed cache from later tests.
    let directory = tempfile::Builder::new().prefix("pb-sdk-").tempdir_in("/tmp").unwrap();
    let installation = crate::project::fixture();
    let executable = installation.path().join("Playbit.app/Contents/MacOS/pb");
    crate::project::scaffold(directory.path(), "Independent SDK", &executable).unwrap();
    playbit::package::metadata::generate(
        directory.path(),
        &Manifest::read(directory.path()).unwrap(),
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    let mut command = {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = format!(
            "(version 1) (allow default) (deny file-read* (subpath {}))",
            serde_json::to_string(repo.to_str().unwrap()).unwrap()
        );
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(policy).arg("cargo");
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("cargo");
    command
        .args(["rustc", "--offline", "--lib", "--crate-type", "cdylib"])
        .env("RUSTFLAGS", "-Dwarnings")
        .env("SCCACHE_SERVER_UDS", directory.path().join("_sccache.sock"))
        .env("SCCACHE_IDLE_TIMEOUT", "5")
        .current_dir(directory.path())
        .env("CARGO_TARGET_DIR", directory.path().join("target"));
    execute(command).unwrap();
    let wasm =
        fs::read(directory.path().join("target/wasm32-unknown-unknown/debug/playbit_app.wasm"))
            .unwrap();
    assert!(wasm.starts_with(b"\0asm\x01\0\0\0"));
    let mut manifest = Manifest::read(directory.path()).unwrap();
    assert!(manifest.bundle(directory.path(), wasm.clone()).is_ok());
    manifest.trace.as_mut().unwrap().names.swap(0, 1);
    assert!(
        manifest.bundle(directory.path(), wasm.clone()).is_err(),
        "old compiled IDs cannot use reordered declarations"
    );

    playbit::runtime::Runtime::new().unwrap().prepare_guest(&wasm).unwrap();
    let mut command = Command::new("cargo");
    command
        .args(["clippy", "--offline", "--lib", "--", "-Dwarnings"])
        .env("SCCACHE_SERVER_UDS", directory.path().join("_sccache.sock"))
        .env("SCCACHE_IDLE_TIMEOUT", "5")
        .current_dir(directory.path())
        .env("CARGO_TARGET_DIR", directory.path().join("target"));
    execute(command).unwrap();
}
