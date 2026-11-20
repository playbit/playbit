#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use std::{
    fs,
    io::Read,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct EngineProcess(Child);
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn template_commands_ignore_profile_discovery_and_require_no_engine() {
    // Real CLI dispatch must bypass even invalid profile/channel inputs before packaging source.
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("playbit.toml"),
        "app_id='template'\nname='Template'\nbuild_cmd='sh build.sh'\nbuild_artifact='app.wasm'\n[release]\nbuild_cmd='sh build.sh'\nbuild_artifact='app.wasm'\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("build.sh"),
        "printf '\\000asm\\001\\000\\000\\000' > app.wasm\n",
    )
    .unwrap();
    fs::write(directory.path().join("playbit-volumes.json"), "{}").unwrap();
    let home = directory.path().join("absent-home");
    for command in ["build", "bundle"] {
        let result = Command::new(env!("CARGO_BIN_EXE_pb"))
            .args([command, "--template"])
            .current_dir(directory.path())
            .env("HOME", &home)
            .env("PLAYBIT_PROFILE", "")
            .env("PLAYBIT_CHANNEL", "invalid-channel")
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["template"], true);
        assert!(value.get("build_id").is_none());
        let package =
            playbit::package::Package::open(Path::new(value["bundle"].as_str().unwrap())).unwrap();
        assert!(package.publisher_certificate().is_none());
        assert!(package.admit_execution(|_| true).is_err());
    }
    assert!(!home.exists());
}

#[test]
fn unsupported_bundle_version_reports_error_with_build_details() {
    // Exercise the real CLI and engine so IPC propagation and stderr formatting agree.
    let directory = tempfile::tempdir().unwrap();
    let mut header = [0; 32];
    header[..8].copy_from_slice(playbit::package::codec::MAGIC);
    header[8..12].copy_from_slice(&99u32.to_le_bytes());
    fs::write(directory.path().join("some.pbapp"), header).unwrap();
    let profile = directory.path().join("profile");
    // This fixture can be the helper's first launch after building, including
    // macOS executable assessment. Launcher deadlines are tested in startup.rs.
    let (mut ready, output) = UnixStream::pair().unwrap();
    ready.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let output: OwnedFd = output.into();
    let engine = EngineProcess(
        Command::new(env!("CARGO_BIN_EXE_playbit-engine"))
            .env("PLAYBIT_PROFILE", &profile)
            .stdout(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let mut greeting = [0; playbit::engine::ipc::ENGINE_READY.len()];
    ready.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, playbit::engine::ipc::ENGINE_READY);
    let result = Command::new(env!("CARGO_BIN_EXE_pb"))
        .args(["run", "some.pbapp"])
        .current_dir(directory.path())
        .env("PLAYBIT_PROFILE", &profile)
        .output();
    drop(engine);
    let result = result.unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("pb: error: unsupported app bundle\n"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "Playbit v{} ({}-{})",
            playbit::VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
        "{stderr}"
    );
    assert!(stderr.contains("support@playbit.app"), "{stderr}");
}

#[cfg(target_os = "macos")]
#[test]
fn incomplete_package_does_not_use_a_flat_engine() {
    // A stale executable at the removed path must not hide a broken installation.
    let directory = tempfile::tempdir().unwrap();
    let macos = directory.path().join("Playbit.app/Contents/MacOS");
    fs::create_dir_all(&macos).unwrap();
    let pb = macos.join("pb");
    fs::copy(env!("CARGO_BIN_EXE_pb"), &pb).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_playbit-engine"), macos.join("playbit-engine")).unwrap();
    let profile = directory.path().join("absent-profile");
    let result = Command::new(pb).arg("status").env("PLAYBIT_PROFILE", &profile).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("installation is incomplete"));
    assert!(!profile.join("profile.kvdb").exists());
    let log = fs::read_to_string(profile.join("logs/engine.log")).unwrap();
    assert!(log.contains("] engine helper missing: "));
    assert!(log.contains("executable="));
}
