//! Signed launcher templates and exact runtime constraints for generated apps.
use crate::engine::Result;
use core_foundation::{
    base::{CFType, TCFType},
    data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef},
    string::{CFString, CFStringRef},
    url::CFURL,
};
use object::{Object, ObjectSection};
use security_framework::os::macos::code_signing::{Flags, SecCode, SecRequirement, SecStaticCode};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const RUNTIME_PATH_BYTES: usize = 4096;
const MAX_LAUNCHER_BYTES: u64 = 128 * 1024;

pub(super) fn runtime_for_engine(engine: &Path) -> Result<PathBuf> {
    let app = engine
        .ancestors()
        .filter(|path| path.extension().is_some_and(|v| v == "app"))
        .last()
        .ok_or("Missing packaged runtime installation")?;
    Ok(app.join("Contents/Frameworks/libplaybit_runtime.dylib").canonicalize()?)
}

pub(super) fn cdhash(path: &Path) -> Result<[u8; 20]> {
    let url = CFURL::from_path(path, false).ok_or("Invalid runtime URL")?;
    let code = SecStaticCode::from_path(&url, Flags::NONE)?;
    let requirement: SecRequirement = "always".parse()?;
    code.check_validity(Flags::STRICT_VALIDATE, &requirement)?;
    signing_hash(&code)
}

/// The dynamic code requirement binds these exact template bytes to the
/// running app's sealed Info.plist, even if its installation path changes.
pub(super) fn verify_template(
    example: &super::new_session::Example,
    seal_suffix: &str,
    bytes: &[u8],
) -> Result<()> {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let digest = crate::identifier::encode(digest.as_ref());
    let requirement: SecRequirement =
        format!("info[{}{seal_suffix}] = \"{digest}\"", example.seal_key).parse()?;
    SecCode::for_self(Flags::NONE)?.check_validity(Flags::NONE, &requirement).map_err(|_| {
        format!("Playbit couldn’t verify the bundled {} template.", example.name).into()
    })
}

/// dyld has already checked this image against the launcher's signed library
/// constraint before calling its entry point. Read that identity without hashing
/// the entire runtime again. Wrapper creation and updates still validate disk
/// contents through cdhash; no validated result is cached across installations.
pub(super) fn loaded_cdhash() -> Result<[u8; 20]> {
    let path = loaded_path()?;
    let url = CFURL::from_path(path, false).ok_or("Invalid loaded runtime URL")?;
    let code = SecStaticCode::from_path(&url, Flags::NONE)?;
    signing_hash(&code)
}

#[allow(unsafe_code, reason = "Security.framework returns owned, dynamically checked CF values.")]
fn signing_hash(code: &SecStaticCode) -> Result<[u8; 20]> {
    use security_framework_sys::code_signing::SecStaticCodeRef;
    unsafe extern "C" {
        fn SecCodeCopySigningInformation(
            code: SecStaticCodeRef,
            flags: u32,
            info: *mut CFDictionaryRef,
        ) -> i32;
        static kSecCodeInfoUnique: CFStringRef;
    }
    let mut info = std::ptr::null();
    // SAFETY: live code object; successful copy returns an owned dictionary.
    let status = unsafe { SecCodeCopySigningInformation(code.as_concrete_TypeRef(), 2, &mut info) };
    if status != 0 || info.is_null() {
        return Err(format!("Cannot inspect runtime signature ({status})").into());
    }
    let info: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(info) };
    let key = unsafe { CFString::wrap_under_get_rule(kSecCodeInfoUnique) };
    let data = info
        .find(key)
        .and_then(|value| value.downcast::<CFData>())
        .ok_or("Missing runtime CDHash")?;
    Ok(data.bytes().try_into().map_err(|_| "Invalid runtime CDHash length")?)
}

#[allow(
    unsafe_code,
    reason = "dladdr borrows a live function address and returns a loader-owned string."
)]
pub(super) fn loaded_path() -> Result<PathBuf> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: this function is in the runtime; the returned name remains owned by dyld.
    if unsafe { libc::dladdr(loaded_path as *const () as *const _, &mut info) } == 0
        || info.dli_fname.is_null()
    {
        return Err("Cannot locate the loaded Playbit runtime".into());
    }
    let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    use std::os::unix::ffi::OsStrExt;
    Ok(Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).canonicalize()?)
}

pub(super) fn launcher(template: &Path, runtime: &Path) -> Result<Vec<u8>> {
    if fs::metadata(template)?.len() > MAX_LAUNCHER_BYTES {
        return Err("Launcher template exceeds size limit".into());
    }
    let mut bytes = fs::read(template)?;
    let file = object::File::parse(bytes.as_slice())?;
    if file.architecture() != object::Architecture::Aarch64
        || file.kind() != object::ObjectKind::Executable
    {
        return Err("Invalid launcher architecture or file type".into());
    }
    let section =
        file.section_by_name("__pb_runtime").ok_or("Missing launcher runtime path section")?;
    if section.segment_name()? != Some("__TEXT") {
        return Err("Launcher path is not in read-only code".into());
    }
    let (start, size) = section.file_range().ok_or("Missing launcher path bytes")?;
    if size != RUNTIME_PATH_BYTES as u64 {
        return Err("Invalid launcher runtime path capacity".into());
    }
    let start = usize::try_from(start)?;
    let end = start.checked_add(RUNTIME_PATH_BYTES).ok_or("Invalid launcher path offset")?;
    let slot = bytes.get_mut(start..end).ok_or("Truncated launcher path section")?;
    let marker = b"UNCONFIGURED_PLAYBIT_RUNTIME\0";
    if !slot.starts_with(marker) || slot[marker.len()..].iter().any(|v| *v != 0) {
        return Err("Launcher template has already been configured".into());
    }
    use std::os::unix::ffi::OsStrExt;
    let path = runtime.as_os_str().as_bytes();
    if !runtime.is_absolute() || path.len() >= RUNTIME_PATH_BYTES || path.contains(&0) {
        return Err("Invalid shared runtime path".into());
    }
    slot.fill(0);
    slot[..path.len()].copy_from_slice(path);
    Ok(bytes)
}

/// The exception is never used without an OS-enforced exact library constraint.
/// System libraries are implicitly admitted by macOS library-load constraints.
pub(super) fn sign(bundle: &Path, entitlements: &Path, hash: &[u8; 20]) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(hash);
    let constraint = bundle.join("Contents/Resources/runtime-constraint.plist");
    fs::write(
        &constraint,
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>cdhash</key><data>{encoded}</data></dict></plist>"
        ),
    )?;
    // The optional --enforce-constraint-validity validates signer input, not
    // library loads. macOS 27 rejects this valid rule with that flag. Keep the
    // fixed CDHash schema; the native probe verifies enforcement before init.
    let output = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--options",
            "runtime",
            "--signature-size",
            "64",
            "--entitlements",
        ])
        .arg(entitlements)
        .arg("--library-constraint")
        .arg(&constraint)
        .arg(bundle)
        .output()?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)]);
        return Err(format!(
            "Cannot sign the guest with its required runtime constraint ({}): {}",
            output.status,
            diagnostic.trim()
        )
        .into());
    }
    Ok(())
}

#[allow(unsafe_code, reason = "Launch Services synchronously borrows a live CFURL.")]
pub(super) fn register(bundle: &Path) -> Result<()> {
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSRegisterURL(url: core_foundation::url::CFURLRef, update: bool) -> i32;
    }
    let url = CFURL::from_path(bundle, true).ok_or("Invalid generated app URL")?;
    let status = unsafe { LSRegisterURL(url.as_concrete_TypeRef(), true) };
    if status != 0 {
        return Err(format!("Cannot register generated app ({status})").into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "The security probe asserts real OS rejection before constructors execute."
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn loaded_image_identity_matches_its_validated_signature() {
        // The current signed test image exercises both identity queries against
        // the same loaded code; preparing an unloaded file still validates it.
        assert_eq!(loaded_cdhash().unwrap(), cdhash(&loaded_path().unwrap()).unwrap());
    }

    #[test]
    fn generated_launcher_rejects_a_substituted_library_before_its_constructor() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Probe.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
              CFBundleIdentifier</key><string>playbit.app.probe</string><key>\
              CFBundleExecutable</key><string>Playbit</string><key>\
              CFBundlePackageType</key><string>APPL</string></dict></plist>",
        )
        .unwrap();
        for (name, message) in [("runtime", "EXPECTED"), ("other", "UNRELATED")] {
            let source = root.path().join(format!("{name}.c"));
            fs::write(
                &source,
                format!(
                    "#include <stdio.h>\n__attribute__((constructor)) static void init(void) {{ \
                     puts(\"{message}\"); }}\nvoid playbit_main(void) {{}}\n"
                ),
            )
            .unwrap();
            let library = root.path().join(format!("{name}.dylib"));
            assert!(
                Command::new("clang")
                    .args([
                        "-Os",
                        "-dynamiclib",
                        concat!("-mmacosx-version-min=", env!("MACOSX_DEPLOYMENT_TARGET"))
                    ])
                    .arg(source)
                    .arg("-o")
                    .arg(&library)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(&library)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let template = root.path().join("template");
        let platform = Path::new(env!("CARGO_MANIFEST_DIR")).join("platform/macos");
        assert!(
            Command::new("clang")
                .args([
                    "-std=c17",
                    "-Oz",
                    concat!("-mmacosx-version-min=", env!("MACOSX_DEPLOYMENT_TARGET"))
                ])
                .arg(platform.join("launcher.c"))
                .arg("-o")
                .arg(&template)
                .status()
                .unwrap()
                .success()
        );
        let runtime = root.path().join("runtime.dylib");
        let executable = app.join("Contents/MacOS/Playbit");
        fs::write(&executable, launcher(&template, &runtime).unwrap()).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        sign(&app, &platform.join("guest-entitlements.plist"), &cdhash(&runtime).unwrap()).unwrap();
        let accepted = Command::new(&executable).output().unwrap();
        assert!(accepted.status.success(), "{}", String::from_utf8_lossy(&accepted.stderr));
        assert!(accepted.stdout.starts_with(b"EXPECTED"));
        fs::rename(root.path().join("other.dylib"), &runtime).unwrap();
        let rejected = Command::new(&executable).output().unwrap();
        assert!(!rejected.status.success());
        assert!(!String::from_utf8_lossy(&rejected.stdout).contains("UNRELATED"));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("library load"));
        fs::remove_file(&runtime).unwrap();
        assert!(!Command::new(&executable).output().unwrap().status.success());
    }
}
