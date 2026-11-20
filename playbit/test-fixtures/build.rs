//! Build guest libraries separately so nested Cargo never takes the parent's build lock.

use std::{io, path::PathBuf, process::Command};

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_WASM").is_none() {
        return Ok(());
    }

    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let root = manifest.parent().and_then(|path| path.parent()).expect("workspace root");
    for input in [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "playbit-api",
        "guests/todo",
        "guests/shared-note",
        "guests/ui-gallery",
        "guests/little-list",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(input).display());
    }

    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    for (guest, target, feature) in [
        ("todo", "target", None),
        ("shared-note", "target", None),
        ("ui-gallery", "target", None),
        ("little-list", "target", None),
        ("todo", "root-target", Some("fixture-root")),
    ] {
        let features: Vec<_> = feature.into_iter().flat_map(|name| ["--features", name]).collect();
        let status = Command::new(std::env::var_os("CARGO").expect("Cargo executable"))
            .current_dir(root)
            .args([
                "rustc",
                "--locked",
                "--release",
                "--lib",
                "--crate-type",
                "cdylib",
                "--target",
                "wasm32-unknown-unknown",
                "--jobs",
                "1",
                "-p",
                guest,
                "--target-dir",
            ])
            .arg(output.join(target))
            .args(features)
            // The child cannot share its waiting parent's jobserver token or native flags.
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MAKEFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .status()?;
        if !status.success() {
            return Err(io::Error::other(
                "Wasm test fixture build failed; run scripts/prepare-apps.sh and install the wasm32-unknown-unknown target",
            ));
        }
    }
    Ok(())
}
