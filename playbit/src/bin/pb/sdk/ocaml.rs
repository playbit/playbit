//! Small dependency-ordered OCaml driver; no package manager or shell at build time.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn execute(program: &str, arguments: &[OsString], directory: &Path) -> Result<()> {
    let mut child = command(program);
    child.args(arguments).current_dir(directory);
    crate::developer::execute(child)
}
fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn fingerprint_file(hash: &mut blake3::Hasher, path: &Path) -> Result<()> {
    let name = path.file_name().ok_or("missing fingerprint filename")?.as_encoded_bytes();
    let bytes = read(path, 8 * 1024 * 1024)?;
    hash.update(&(name.len() as u64).to_le_bytes());
    hash.update(name);
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(&bytes);
    Ok(())
}
fn sdk_objects(build: &Path, api: &Path, directory: &Path) -> Result<()> {
    fs::create_dir_all(build)?;
    for module in ["playbit_generated", "playbit"] {
        for (extension, output) in [("mli", "cmi"), ("ml", "cmo")] {
            let mut arguments = args(&["-I"]);
            arguments.push(build.into());
            arguments.extend(args(&["-c", "-o"]));
            arguments.push(build.join(format!("{module}.{output}")).into());
            arguments.push(api.join(format!("{module}.{extension}")).into());
            execute("ocamlc", &arguments, directory)?;
        }
    }
    Ok(())
}
fn link(
    build: &Path,
    api: &Path,
    modules: &[PathBuf],
    release: bool,
    directory: &Path,
) -> Result<PathBuf> {
    let bytecode = build.join("app.byte");
    let staging = build.join("staging.wasm");
    let mut arguments = args(&["-no-check-prims", "-I"]);
    arguments.push(build.into());
    arguments.push(build.join("playbit_generated.cmo").into());
    arguments.push(build.join("playbit.cmo").into());
    arguments.extend(modules.iter().map(|p| p.as_os_str().to_owned()));
    arguments.push("-o".into());
    arguments.push(bytecode.as_os_str().to_owned());
    execute("ocamlc", &arguments, directory)?;
    let mut arguments =
        args(&["--target", "playbit", "--opt", if release { "3" } else { "1" }, "-o"]);
    arguments.push(staging.as_os_str().to_owned());
    arguments.push(api.join("playbit_raw.wat").into());
    arguments.push(api.join("playbit_ffi.wat").into());
    arguments.push(bytecode.into());
    execute("wasm_of_ocaml", &arguments, directory)?;
    let bytes = read(&staging, playbit::package::MAX_BUNDLE_BYTES as u64)?;
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator.validate_all(&bytes)?;
    let mut imports = 0;
    let mut exports = BTreeSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        match payload? {
            wasmparser::Payload::StartSection { .. } => {
                return Err("OCaml Playbit module must not have a start section".into());
            }
            wasmparser::Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import?;
                    imports += 1;
                    if import.module != "playbit" || import.name != "pb_syscall" {
                        return Err("unsupported OCaml host primitive".into());
                    }
                }
            }
            wasmparser::Payload::ExportSection(reader) => {
                for export in reader {
                    exports.insert(export?.name.to_owned());
                }
            }
            _ => {}
        }
    }
    if imports != 1 || exports != BTreeSet::from(["memory".to_owned(), "pb_main".to_owned()]) {
        return Err("invalid OCaml Playbit module imports/exports".into());
    }
    Ok(staging)
}
pub(super) fn compile(root: &Path, api: &Path, source: &str, release: bool) -> Result<()> {
    let build = root.join("objects");
    sdk_objects(&build, api, root)?;
    let source_file = root.join("probe.ml");
    fs::write(&source_file, source)?;
    let object = build.join("probe.cmo");
    let mut arguments = args(&["-I"]);
    arguments.push(build.as_os_str().to_owned());
    arguments.extend(args(&["-c", "-o"]));
    arguments.push(object.as_os_str().to_owned());
    arguments.push(source_file.into());
    execute("ocamlc", &arguments, root)?;
    link(&build, api, &[object], release, root)?;
    Ok(())
}

pub(super) fn build(directory: &Path, api: &Path, release: bool) -> Result<()> {
    let directory = directory.canonicalize()?;
    let identity = ACTIVE
        .with_borrow(|tools| tools.as_ref().expect("selected OCaml toolchain").identity.clone());
    let mut hash = blake3::Hasher::new();
    hash.update(identity.as_bytes());
    hash_tree(api, &mut hash)?;
    let build = directory
        .join("target/playbit/ocaml")
        .join(playbit::identifier::encode(hash.finalize().as_bytes()))
        .join(if release { "release" } else { "debug" });
    fs::create_dir_all(&build)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(build.join("lock"))?;
    lock.lock()?;
    if !build.join("sdk.ok").is_file() {
        sdk_objects(&build, api, &directory)?;
        atomic(&build.join("sdk.ok"), b"1")?;
    }
    let mut sources = BTreeMap::<String, BTreeMap<String, PathBuf>>::new();
    let mut total = 0;
    for (index, entry) in fs::read_dir(directory.join("src"))?.enumerate() {
        if index >= 512 {
            return Err("OCaml src directory exceeds 512 entries".into());
        }
        let entry = entry?;
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(extension, "ml" | "mli") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err("OCaml sources must be regular files".into());
        }
        let stem = path.file_stem().and_then(|p| p.to_str()).ok_or("invalid OCaml source name")?;
        if stem.is_empty()
            || !stem.as_bytes()[0].is_ascii_lowercase()
            || !stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("OCaml source names must be lowercase module identifiers".into());
        }
        total += path.metadata()?.len();
        if total > 8 * 1024 * 1024 {
            return Err("OCaml sources exceed 256 modules or 8 MiB".into());
        }
        let module = format!("{}{}", stem[..1].to_ascii_uppercase(), &stem[1..]);
        if !sources.contains_key(&module) && sources.len() >= 256 {
            return Err("OCaml sources exceed 256 modules or 8 MiB".into());
        }
        sources.entry(module).or_default().insert(extension.into(), path);
    }
    if !sources.get("App").is_some_and(|files| files.contains_key("ml")) {
        return Err("OCaml project requires src/app.ml".into());
    }
    for files in sources.values() {
        if !files.contains_key("ml") {
            return Err("OCaml interface requires a matching implementation".into());
        }
    }
    let mut dependency = command("ocamldep");
    dependency.arg("-modules").current_dir(&directory);
    for files in sources.values() {
        for path in files.values() {
            dependency.arg(path.strip_prefix(&directory)?);
        }
    }
    let result = dependency.output()?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).to_string().into());
    }
    if result.stdout.len() > 1024 * 1024 {
        return Err("OCaml dependency output exceeds 1 MiB".into());
    }
    let text = String::from_utf8(result.stdout)?;
    let mut dependencies = BTreeMap::<String, BTreeSet<String>>::new();
    for line in text.lines() {
        let (file, modules) = line.split_once(':').ok_or("invalid ocamldep output")?;
        let stem = Path::new(file)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("invalid dependency module")?;
        let module = format!("{}{}", stem[..1].to_ascii_uppercase(), &stem[1..]);
        for dependency in modules.split_whitespace() {
            if dependency != module && sources.contains_key(dependency) {
                dependencies.entry(module.clone()).or_default().insert(dependency.into());
            }
        }
    }
    let mut pending: BTreeSet<_> = sources.keys().cloned().collect();
    let mut compiled = BTreeSet::new();
    let mut objects = Vec::new();
    while !pending.is_empty() {
        let module = pending
            .iter()
            .find(|m| dependencies.get(*m).is_none_or(|deps| deps.is_subset(&compiled)))
            .cloned()
            .ok_or("cyclic OCaml module dependencies")?;
        pending.remove(&module);
        let files = &sources[&module];
        let stem = files["ml"].file_stem().ok_or("missing module name")?.to_string_lossy();
        let mut fingerprint = blake3::Hasher::new();
        for source in files.values() {
            fingerprint_file(&mut fingerprint, source)?;
        }
        if let Some(deps) = dependencies.get(&module) {
            for dependency in deps {
                let path = &sources[dependency]["ml"];
                fingerprint_file(
                    &mut fingerprint,
                    &build
                        .join(path.file_stem().ok_or("missing dependency name")?)
                        .with_extension("cmi"),
                )?;
            }
        }
        let fingerprint = fingerprint.finalize();
        let stamp = build.join(format!("{stem}.stamp"));
        let object = build.join(format!("{stem}.cmo"));
        if fs::read(&stamp).ok().as_deref() != Some(fingerprint.as_bytes().as_slice())
            || !object.is_file()
        {
            for (extension, output) in [("mli", "cmi"), ("ml", "cmo")] {
                if let Some(source) = files.get(extension) {
                    let mut arguments = args(&["-I"]);
                    arguments.push(build.as_os_str().to_owned());
                    arguments.extend(args(&["-c", "-o"]));
                    arguments.push(build.join(format!("{stem}.{output}")).into());
                    arguments.push(source.as_os_str().to_owned());
                    execute("ocamlc", &arguments, &directory)?;
                }
            }
            atomic(&stamp, fingerprint.as_bytes())?;
        }
        objects.push(object);
        compiled.insert(module);
    }
    let staging = link(&build, api, &objects, release, &directory)?;
    let output = directory.join("target").join(if release { "release" } else { "debug" });
    fs::create_dir_all(&output)?;
    fs::rename(staging, output.join("app.wasm"))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests construct cache inputs and compare fingerprints.")]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_track_interface_presence_names_and_content_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let implementation = root.path().join("helper.ml");
        let interface = root.path().join("helper.mli");
        let fingerprint = |paths: &[&Path]| {
            let mut hash = blake3::Hasher::new();
            for path in paths {
                fingerprint_file(&mut hash, path).unwrap();
            }
            hash.finalize()
        };
        fs::write(&implementation, "let label = \"hello\"\n").unwrap();
        let inferred = fingerprint(&[&implementation]);
        fs::write(&interface, "").unwrap();
        assert_ne!(inferred, fingerprint(&[&implementation, &interface]));
        fs::remove_file(&interface).unwrap();
        assert_eq!(inferred, fingerprint(&[&implementation]));
        fs::write(&implementation, "ab").unwrap();
        fs::write(&interface, "c").unwrap();
        let split = fingerprint(&[&implementation, &interface]);
        fs::write(&implementation, "a").unwrap();
        fs::write(&interface, "bc").unwrap();
        assert_ne!(split, fingerprint(&[&implementation, &interface]));
        let renamed = root.path().join("other.mli");
        fs::copy(&interface, &renamed).unwrap();
        assert_ne!(fingerprint(&[&interface]), fingerprint(&[&renamed]));
    }
}
