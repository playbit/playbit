//! Structural launch admission without starting an AppKit event loop.
use super::*;
use crate::store::{EntryId, InitialDatabase, InitialDatabaseKind};
use std::ffi::c_void;

unsafe extern "C" fn init(
    _: *mut playbit_api::abi::operations::Context,
    _: *mut c_void,
    _: *mut *mut c_void,
) -> i32 {
    0
}
unsafe extern "C" fn turn(_: *mut playbit_api::abi::operations::Context, _: *mut c_void) -> i32 {
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
fn app() -> App {
    App {
        size: std::mem::size_of::<App>() as u32,
        written: 0,
        init: Some(init),
        turn: Some(turn),
        destroy: Some(destroy),
        user_data: std::ptr::null_mut(),
    }
}
fn declaration() -> Vec<u8> {
    serde_json::to_vec(&RootInitialization {
        permissions: Default::default(),
        databases: vec![InitialDatabase {
            id: EntryId::from_bytes([5; 15]).expect("valid fixture entry ID"),
            name: "records".into(),
            kind: InitialDatabaseKind::KvDb {},
        }],
        blobs: vec![],
    })
    .expect("valid fixture declaration")
}

#[cfg(feature = "connected-engine")]
#[test]
fn portable_worker_verifies_and_retains_the_exact_package() {
    use crate::{package::Package, ui::desktop::native::portable::Portable};
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("app.pbapp");
    let package = Package::new(
        "test".into(),
        "Verified app".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        std::collections::BTreeMap::new(),
    )
    .expect("fixture package");
    std::fs::write(&path, package.to_bytes().expect("fixture bytes")).expect("write package");
    let source = Portable { path: path.clone(), expected: package.digest(), app: app() };
    let retained = std::thread::spawn(move || source.load())
        .join()
        .expect("preparation worker")
        .expect("verified package");
    assert_eq!(retained.digest(), package.digest());
    assert!(Portable { path: path.clone(), expected: [0; 32], app: app() }.load().is_err());
    std::fs::remove_file(&path).expect("remove original path");
    assert!(Portable { path, expected: package.digest(), app: app() }.load().is_err());
    assert_eq!(retained.wasm().expect("retained executable"), b"\0asm\x01\0\0\0");
}

#[test]
fn portable_profile_selection_has_one_nullable_encoding() {
    // Only null+zero requests runtime selection; malformed spans still reject
    // during structural admission, before filesystem or account work.
    let app = app();
    let declaration = declaration();
    let title = b"Fixture";
    // SAFETY: buffers/descriptor remain live; null is admitted only with zero length.
    unsafe {
        let selected = Configuration::read(
            std::ptr::null(),
            0,
            title.as_ptr(),
            title.len() as u32,
            declaration.as_ptr(),
            declaration.len() as u32,
            &app,
        )
        .expect("runtime profile selection");
        assert!(selected.profile.is_none());
        assert!(
            Configuration::read(
                std::ptr::null(),
                1,
                title.as_ptr(),
                title.len() as u32,
                declaration.as_ptr(),
                declaration.len() as u32,
                &app
            )
            .is_err()
        );
        assert!(config(b"", title, &declaration, &app).is_err());
    }
}

unsafe fn config(
    profile: &[u8],
    title: &[u8],
    declaration: &[u8],
    app: *const App,
) -> Result<Configuration, Error> {
    // SAFETY: test fixtures lend stable slices and, except explicit null admission, a record.
    unsafe {
        Configuration::read(
            profile.as_ptr(),
            profile.len() as u32,
            title.as_ptr(),
            title.len() as u32,
            declaration.as_ptr(),
            declaration.len() as u32,
            app,
        )
    }
}
#[test]
fn launch_copies_inputs_and_rejects_invalid_arguments() {
    // Accepted launch inputs own their text/declaration before any app callback;
    // invalid spans, encoding, limits and callback tables reject without files.
    let app = app();
    let root = declaration();
    let mut profile = b"/tmp/native-sdk-admission".to_vec();
    let mut title = b"Test app".to_vec();
    let mut borrowed_root = root.clone();

    // SAFETY: all pointers refer to the listed live test fixtures.
    unsafe {
        let input = config(&profile, &title, &borrowed_root, &app).expect("valid launch");
        profile.fill(0);
        title.fill(0);
        borrowed_root.fill(0);

        assert_eq!(input.profile, Some(PathBuf::from("/tmp/native-sdk-admission")));
        assert_eq!(input.title, "Test app");
        assert_eq!(input.declaration.databases.len(), 1);
        assert_eq!(input.declaration.databases[0].name, "records");
        assert!(matches!(input.declaration.databases[0].kind, InitialDatabaseKind::KvDb {}));

        for path in [&b"relative"[..], b"", b"/tmp/a\0b", b"/tmp/\xff"] {
            assert!(matches!(config(path, b"Test app", &root, &app), Err(Error::InvalidArgument)));
        }
        for title in [&b""[..], b"   ", b"bad\0title", b"\xff"] {
            assert!(matches!(config(b"/tmp/a", title, &root, &app), Err(Error::InvalidArgument)));
        }
        assert!(matches!(
            config(b"/tmp/a", &vec![b'x'; 257], &root, &app),
            Err(Error::LimitExceeded)
        ));
        assert!(matches!(
            config(&vec![b'/'; 4097], b"Test app", &root, &app),
            Err(Error::LimitExceeded)
        ));
        assert!(matches!(
            config(b"/tmp/a", b"Test app", &vec![b' '; 65537], &app),
            Err(Error::LimitExceeded)
        ));
        assert!(matches!(
            config(b"/tmp/a", b"Test app", b"invalid", &app),
            Err(Error::InvalidArgument)
        ));
        assert!(matches!(
            config(b"/tmp/a", b"Test app", &root, std::ptr::null()),
            Err(Error::InvalidArgument)
        ));
        let incomplete = App { turn: None, ..app };
        assert!(matches!(
            config(b"/tmp/a", b"Test app", &root, &incomplete),
            Err(Error::InvalidArgument)
        ));
        #[repr(C, align(8))]
        struct Header {
            size: u32,
            written: u32,
        }
        let short = Header { size: 8, written: 0 };
        assert!(matches!(
            config(b"/tmp/a", b"Test app", &root, (&short as *const Header).cast()),
            Err(Error::InvalidArgument)
        ));
        #[repr(C)]
        struct Future {
            app: App,
            tail: [u8; 8],
        }
        let mut future = Future { app, tail: [0; 8] };
        future.app.size = std::mem::size_of::<Future>() as u32;
        future.app.written = std::mem::size_of::<App>() as u32;
        assert!(config(b"/tmp/a", b"Test app", &root, &future.app).is_ok());
        future.tail[0] = 1;
        assert!(matches!(
            config(b"/tmp/a", b"Test app", &root, &future.app),
            Err(Error::Unsupported)
        ));
    }
    assert!(!STARTED.load(Ordering::Acquire));
}
#[test]
fn off_main_thread_launch_is_rejected_before_reading_pointers() {
    // Tests execute on worker threads. Thread admission must precede every
    // foreign-pointer dereference and leave the event loop available to main.
    assert_eq!(
        unsafe {
            pb_mac_run(
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        },
        Error::Conflict as i32
    );
    assert!(!STARTED.load(Ordering::Acquire));
}

#[test]
#[cfg(feature = "connected-engine")]
fn off_main_thread_portable_launch_rejects_before_reading_package() {
    // SAFETY: the off-main admission gate rejects before consulting pointers.
    assert_eq!(
        unsafe {
            pb_mac_run_portable(
                std::ptr::null(),
                0,
                std::ptr::null(),
                u32::MAX,
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        Error::Conflict as i32
    );
    assert!(!STARTED.load(Ordering::Acquire));
}
