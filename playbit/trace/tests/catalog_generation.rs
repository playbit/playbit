//! Exercise the actual build executable across checkout relocation and absent inputs.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const BUILD_SCRIPT: &str = include_str!("../build.rs");
const DIRECTORIES: [&str; 3] = ["playbit/src", "playbit/trace/src", "vendor/gpui/src"];
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    builder: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pt-catalog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create isolated catalog fixture");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("make catalog fixture private");
        let old = root.join("old");
        for (directory, name) in
            DIRECTORIES.into_iter().zip(["storage.commit", "ipc.receive", "gpui.scene.build"])
        {
            fs::create_dir_all(old.join(directory))
                .expect("create public source fixture directory");
            fs::write(old.join(directory).join("site.rs"), format!("const NAME: &str = {name:?};"))
                .expect("write static native source fixture");
        }
        fs::create_dir_all(old.join("service/ap/src"))
            .expect("create private service source fixture directory");
        fs::write(
            old.join("service/ap/src/private.rs"),
            "const NAME: &str = \"ap.private.forbidden\";",
        )
        .expect("write excluded private service source");
        let source = root.join("builder.rs");
        let builder = root.join("builder");
        fs::write(&source, BUILD_SCRIPT).expect("write actual catalog build script");
        let compile = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&builder)
            .env("CARGO_MANIFEST_DIR", old.join("playbit/trace"))
            .output()
            .expect("launch Rust compiler for catalog build executable");
        assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
        Self { root, builder }
    }

    fn run(&self, checkout: &Path, label: &str) -> (Output, PathBuf) {
        let output = self.root.join(label);
        fs::create_dir(&output).expect("create unique catalog output directory");
        let result = Command::new(&self.builder)
            .env("CARGO_MANIFEST_DIR", checkout.join("playbit/trace"))
            .env("OUT_DIR", &output)
            .output()
            .expect("run actual catalog build executable");
        fs::write(output.join("stdout.log"), &result.stdout)
            .expect("retain catalog builder stdout");
        fs::write(output.join("stderr.log"), &result.stderr)
            .expect("retain catalog builder stderr");
        (result, output.join("catalog.rs"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retained failing catalog fixture: {}", self.root.display());
        } else {
            fs::remove_dir_all(&self.root).expect("remove passed isolated catalog fixture");
        }
    }
}

#[test]
fn cached_catalog_builder_uses_relocated_runtime_manifest() {
    let fixture = Fixture::new();
    let relocated = fixture.root.join("relocated");
    fs::rename(fixture.root.join("old"), &relocated).expect("relocate isolated catalog checkout");
    let (result, catalog) = fixture.run(&relocated, "relocated-output");
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let catalog = fs::read_to_string(catalog).expect("read generated native catalog");
    for name in ["gpui.scene.build", "ipc.receive", "storage.commit"] {
        assert!(catalog.contains(&format!("{name:?}")));
    }
    assert_eq!(catalog.lines().filter(|line| line.contains('"')).count(), 3);
    assert!(!catalog.contains("ap.private.forbidden"));
    let directives = String::from_utf8(result.stdout).expect("catalog directives use UTF-8 paths");
    for directory in DIRECTORIES {
        let expected = relocated
            .join(directory)
            .canonicalize()
            .expect("resolve existing relocated public directory");
        assert!(directives.lines().any(|line| {
            line.strip_prefix("cargo:rerun-if-changed=").is_some_and(|path| {
                Path::new(path).canonicalize().is_ok_and(|path| path == expected)
            })
        }));
    }
}

#[test]
fn catalog_builder_rejects_missing_or_empty_public_inputs() {
    let fixture = Fixture::new();
    let checkout = fixture.root.join("old");
    fs::remove_dir_all(checkout.join("vendor/gpui/src"))
        .expect("remove public input to exercise rejection");
    let (missing, missing_catalog) = fixture.run(&checkout, "missing-output");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("catalog directory is missing"));
    assert!(!missing_catalog.exists());
    fs::create_dir_all(checkout.join("vendor/gpui/src"))
        .expect("restore public directory for empty-input rejection");
    for directory in ["playbit/src", "playbit/trace/src"] {
        fs::remove_file(checkout.join(directory).join("site.rs"))
            .expect("remove static source name for empty-input rejection");
    }
    let (empty, empty_catalog) = fixture.run(&checkout, "empty-output");
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("native trace catalog is empty"));
    assert!(!empty_catalog.exists());
}
