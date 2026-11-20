//! Compile the paired Wasm app before the native executable records its digest.
use super::Result;
use crate::developer;
use playbit::engine::{
    Request,
    reply::{DeveloperReply, Reply},
};
use playbit::package::{BuildMode, Manifest, codec::MAX_WASM_BYTES};
use std::{fs, io::Read, path::Path, process::Command};

pub(super) fn build(
    source: &Path,
    manifest: &Manifest,
    output: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<()> {
    let target = output.join("paired-wasm");
    let mut command = Command::new("sh");
    command
        .current_dir(source)
        .arg("-c")
        .arg(manifest.build_command(BuildMode { release: true, native: false })?);
    command.env("CARGO_TARGET_DIR", &target);
    command.env_remove("CARGO_MAKEFLAGS").env_remove("MAKEFLAGS");
    developer::execute(command)?;
    let artifact =
        developer::artifact(source, BuildMode { release: true, native: false }, Some(&target))?;
    let mut wasm = Vec::new();
    fs::File::open(&artifact)?.take(MAX_WASM_BYTES as u64 + 1).read_to_end(&mut wasm)?;
    if wasm.len() > MAX_WASM_BYTES {
        return Err("paired Wasm exceeds 64 MiB file limit".into());
    }
    let path = output.join("native-app.pbapp");
    let temporary = output.join(format!("native-app-{}.staging", playbit::store::Id::generate()?));
    let result = (|| -> Result<()> {
        let mut file = fs::File::create_new(&temporary)?;
        manifest.write_bundle(source, wasm, &mut file)?;
        file.sync_all()?;
        drop(file);
        let Reply::Developer(DeveloperReply::PackageSigned { build_id }) =
            call(Request::SignPackage { bundle: temporary.clone() })?
        else {
            return Err("expected authenticated paired-package signing confirmation".into());
        };
        if playbit::package::Package::open(&temporary)?.id()? != build_id {
            return Err("paired package identity differs from its signing receipt".into());
        }
        fs::rename(&temporary, &path)?;
        fs::File::open(output)?.sync_all()?;
        Ok(())
    })();
    if temporary.exists() {
        fs::remove_file(temporary)?;
    }
    result
}
