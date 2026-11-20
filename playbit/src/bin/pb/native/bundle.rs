//! Package the already-linked arm64 Mac app with authenticated package identity.
//! Prepare and verify a complete signed bundle before replacing the named output.
//! Failures leave the last successful output intact.
use crate::developer;
use image::ImageEncoder;
use playbit::engine::{
    Request, Result,
    reply::{DeveloperReply, Reply},
};
use playbit::package::{BuildMode, Manifest, metadata::MAX_NATIVE_BYTES};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

const LICENSES: &[(&str, &[u8])] = &[
    ("AccessKit-MIT.txt", include_bytes!("../../../../../vendor/accesskit_macos/LICENSE-MIT")),
    ("playbit-api.txt", include_bytes!("../../../../../playbit-api/LICENSE")),
    ("Playbit.txt", include_bytes!("../../../../../LICENSE")),
];

pub(crate) fn supported() -> Result<()> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("native app packaging currently requires an arm64 Mac".into());
    }
    Ok(())
}

pub(crate) fn bundle(
    directory: &Path,
    manifest: &Manifest,
    release: bool,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<PathBuf> {
    supported()?;
    let mode = BuildMode { release, native: true };
    let executable = developer::artifact(directory, mode, None)?;
    let output = developer::product_path(&executable, mode, &manifest.name)?;
    let builds = output.parent().ok_or("missing bundle directory")?;
    fs::create_dir_all(builds)?;
    let paired = directory.join("target/playbit/native-app.pbapp");
    let Reply::Developer(DeveloperReply::PackageIdentity { app: identity, build_id, .. }) =
        call(Request::AuthenticatePackage { bundle: paired.clone() })?
    else {
        return Err("expected authenticated native package identity".into());
    };
    if playbit::package::Package::open(&paired)?.id()? != build_id
        || identity.app_id != manifest.app_id
    {
        return Err("native package identity differs from its authentication receipt".into());
    }
    let staging = builds.join(format!(".staging-{}", playbit::store::Id::generate()?));
    fs::create_dir(&staging)?;
    let result = (|| {
        let app = staging.join(format!("{}.app", manifest.name));
        let tool = std::env::current_exe()?.canonicalize()?;
        let contents = tool.parent().and_then(Path::parent).ok_or("missing app contents")?;
        populate(
            directory,
            manifest,
            &identity,
            &build_id,
            &executable,
            &app,
            &contents.join("Resources/guest-icon"),
        )?;
        sign(&app)?;
        publish(&app, &output)?;
        playbit::diagnostic!("Creating {}", output.display());
        Ok(output)
    })();
    // After replacement, staging holds the previous bundle; after failure, it
    // holds only this invocation's unpublished bundle.
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    result
}

fn publish(app: &Path, output: &Path) -> Result<()> {
    if !output.exists() {
        fs::rename(app, output)?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let app = CString::new(app.as_os_str().as_bytes())?;
        let output = CString::new(output.as_os_str().as_bytes())?;
        // SAFETY: both paths are live NUL-terminated strings. The atomic swap
        // leaves the prior bundle untouched if the filesystem operation fails.
        #[allow(unsafe_code, reason = "macOS atomically exchanges nonempty app directories.")]
        let result = unsafe { libc::renamex_np(app.as_ptr(), output.as_ptr(), libc::RENAME_SWAP) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    Err("native app packaging currently requires an arm64 Mac".into())
}

fn read_bytes(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()).into());
    }
    Ok(bytes)
}

fn populate(
    directory: &Path,
    manifest: &Manifest,
    identity: &playbit::package::AppIdentity,
    expected_build: &str,
    executable: &Path,
    app: &Path,
    default_icon: &Path,
) -> Result<()> {
    let binaries = app.join("Contents/MacOS");
    let resources = app.join("Contents/Resources");
    fs::create_dir_all(&binaries)?;
    fs::create_dir_all(resources.join("licenses"))?;
    let bytes = read_bytes(executable, MAX_NATIVE_BYTES)?;
    // Admit a 64-bit arm64 Mach-O executable, not a library or a Wasm artifact.
    if bytes.len() < 32
        || bytes[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || bytes[4..8] != 0x0100_000cu32.to_le_bytes()
        || bytes[12..16] != 2u32.to_le_bytes()
    {
        return Err("native artifact must be an arm64 Mach-O executable".into());
    }
    let paired = directory.join("target/playbit/native-app.pbapp");
    let package = playbit::package::Package::open(&paired)?;
    if package.id()? != expected_build {
        return Err("native package changed after authentication".into());
    }
    if package.initializers().to_json()?
        != playbit::package::read_volume_initializers(directory)?.to_json()?
    {
        return Err("native package initializer differs from source; run pb build --native".into());
    }
    playbit::package::metadata::check_native(manifest, &package, &bytes)?;
    let destination = resources.join("app.pbapp");
    package.write_to(&mut fs::File::create(&destination)?)?;
    if playbit::package::Package::open(&destination)?.digest() != package.digest() {
        return Err("native package changed while copying".into());
    }
    let binary = binaries.join("app");
    fs::write(&binary, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
    }
    let icon = icon(directory, manifest)?;
    let plist = plist(manifest, identity, icon.is_some());
    fs::write(app.join("Contents/Info.plist"), plist.as_bytes())?;
    if let Some(icon) = icon {
        fs::write(resources.join("app.icns"), &icon)?;
    } else {
        for name in ["Assets.car", "icon-guest.icns"] {
            fs::copy(default_icon.join(name), resources.join(name))?;
        }
    }
    for (name, bytes) in LICENSES {
        fs::write(resources.join("licenses").join(name), bytes)?;
    }
    Ok(())
}

fn plist(manifest: &Manifest, identity: &playbit::package::AppIdentity, icon: bool) -> String {
    let name = manifest
        .name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    let icon = if icon {
        "<key>CFBundleIconFile</key><string>app.icns</string>"
    } else {
        "<key>CFBundleIconFile</key><string>icon-guest</string><key>CFBundleIconName</key><string>icon-guest</string>"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>playbit.native.{}</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundleDisplayName</key><string>{name}</string>
<key>CFBundleExecutable</key><string>app</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>LSMinimumSystemVersion</key><string>{}</string>
<key>NSHighResolutionCapable</key><true/>
{icon}
</dict></plist>
"#,
        identity.short_bundle_id(),
        env!("MACOSX_DEPLOYMENT_TARGET")
    )
}

fn icon(directory: &Path, manifest: &Manifest) -> Result<Option<Vec<u8>>> {
    let Some(name) = &manifest.icon else {
        return Ok(None);
    };
    if name.is_empty()
        || Path::new(name).is_absolute()
        || Path::new(name).components().any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("icon must be a file inside the source directory".into());
    }
    let path = directory.join(name).canonicalize()?;
    if !path.starts_with(directory.canonicalize()?) {
        return Err("icon leaves source directory".into());
    }
    let bytes = read_bytes(&path, playbit::package::codec::MAX_ICON_BYTES)?;
    let image = playbit::package::artwork::decode(&bytes, 512)?;
    let mut output = b"icns\0\0\0\0".to_vec();
    for (kind, side) in [(b"ic07", 128), (b"ic09", 512)] {
        let resized;
        let pixels = if side == 512 {
            &image
        } else {
            resized =
                image::imageops::resize(&image, side, side, image::imageops::FilterType::Lanczos3);
            &resized
        };
        let mut png = Vec::new();
        // Lossless fast encoding avoids a filter search over every icon pixel.
        image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        )
        .write_image(pixels.as_raw(), side, side, image::ExtendedColorType::Rgba8)?;
        output.extend_from_slice(kind);
        let length = u32::try_from(png.len())?.checked_add(8).ok_or("icon chunk exceeds u32")?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&png);
    }
    let length = u32::try_from(output.len())?;
    output[4..8].copy_from_slice(&length.to_be_bytes());
    Ok(Some(output))
}

fn sign(app: &Path) -> Result<()> {
    let mut command = Command::new("/usr/bin/codesign");
    command.args(["--force", "--sign", "-"]).arg(app);
    developer::execute(command)?;
    verify(app)
}

fn verify(app: &Path) -> Result<()> {
    let mut command = Command::new("/usr/bin/codesign");
    command.args(["--verify", "--deep", "--strict"]).arg(app);
    developer::execute(command)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest::from_source(
            "app_id = \"test\"\nname = \"Robin's <TODO> & 🐚\"\nbuild_cmd = 'cargo build'\n",
        )
        .unwrap()
    }

    fn identity() -> playbit::package::AppIdentity {
        playbit::package::AppIdentity {
            publisher: playbit::store::Id::from_bytes([2; 16]),
            app_id: "test".into(),
        }
    }

    #[test]
    fn native_bundle_validates_executable_and_resource_boundaries() {
        // Mach-O headers and bounded source icons fail explicitly.
        let source = tempfile::tempdir().unwrap();
        let executable = source.path().join("exe");
        fs::write(&executable, b"not an executable").unwrap();
        assert!(
            populate(
                source.path(),
                &manifest(),
                &identity(),
                "unused-invalid-artifact",
                &executable,
                &source.path().join("bad.app"),
                source.path()
            )
            .is_err()
        );
        assert!(read_bytes(&executable, 3).is_err());
        let mut manifest = manifest();
        manifest.icon = Some("../icon.svg".into());
        assert!(icon(source.path(), &manifest).is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("icon.svg"), b"outside").unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("icon.svg"),
                source.path().join("icon.svg"),
            )
            .unwrap();
            manifest.icon = Some("icon.svg".into());
            assert!(icon(source.path(), &manifest).is_err());
        }
    }

    #[test]
    fn native_bundle_contains_metadata_artwork_executable_and_licenses() {
        let source = tempfile::tempdir().unwrap();
        let executable = source.path().join("exe");
        let mut manifest = manifest();
        let declaration = playbit::package::VolumeInitializers::from_json(include_bytes!(
            "../../../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap();
        fs::write(
            source.path().join(playbit::package::VOLUME_INITIALIZERS_FILE),
            serde_json::to_vec(&declaration).unwrap(),
        )
        .unwrap();

        let package = playbit::package::Package::create(
            manifest.app_id.clone(),
            manifest.name.clone(),
            b"\0asm\x01\0\0\0".to_vec(),
            Default::default(),
            &declaration,
        )
        .unwrap()
        .to_bytes()
        .unwrap();
        let digest = *blake3::hash(&package).as_bytes();
        let build = playbit::identifier::encode(&digest);
        let paired = source.path().join("target/playbit/native-app.pbapp");
        fs::create_dir_all(paired.parent().unwrap()).unwrap();
        fs::write(paired, &package).unwrap();
        // One arm64 Mach-O segment carries the expected portable package digest.
        let mut bytes = vec![0; 32 + 72 + 2 * 80];
        for (offset, value) in [
            (0, 0xfeedfacf_u32),
            (4, 0x0100_000c),
            (12, 2),
            (16, 1),
            (20, 232),
            (32, 0x19),
            (36, 232),
            (96, 2),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (index, (name, data)) in
            [("__pbbundle", digest.as_slice()), ("__unused", b"unused".as_slice())]
                .iter()
                .enumerate()
        {
            let start = 104 + index * 80;
            bytes[start..start + name.len()].copy_from_slice(name.as_bytes());
            bytes[start + 40..start + 48].copy_from_slice(&(data.len() as u64).to_le_bytes());
            let offset = bytes.len() as u32;
            bytes[start + 48..start + 52].copy_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(data);
        }
        fs::write(&executable, &bytes).unwrap();
        manifest.icon = Some("icon.svg".into());
        fs::write(
            source.path().join("icon.svg"),
            b"<svg width='24' height='24'><rect width='24' height='24' fill='red'/></svg>",
        )
        .unwrap();
        let app = source.path().join("first.app");
        let default_icon = source.path().join("default-icon");
        assert!(
            populate(
                source.path(),
                &manifest,
                &identity(),
                "another-build",
                &executable,
                &source.path().join("changed.app"),
                &default_icon
            )
            .is_err()
        );
        populate(source.path(), &manifest, &identity(), &build, &executable, &app, &default_icon)
            .unwrap();
        assert_eq!(fs::read(app.join("Contents/MacOS/app")).unwrap(), bytes);
        let xml = fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
        assert!(xml.contains("Robin&apos;s &lt;TODO&gt; &amp; 🐚"));
        assert!(xml.contains("CFBundleIconFile"));
        let icns = fs::read(app.join("Contents/Resources/app.icns")).unwrap();
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());
        let mut offset = 8;
        for (kind, side) in [(b"ic07", 128), (b"ic09", 512)] {
            assert_eq!(&icns[offset..offset + 4], kind);
            let length =
                u32::from_be_bytes(icns[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let image = image::load_from_memory(&icns[offset + 8..offset + length]).unwrap();
            assert_eq!(image.width(), side);
            assert_eq!(image.height(), side);
            offset += length;
        }
        assert_eq!(offset, icns.len());
        for (name, bytes) in LICENSES {
            assert_eq!(
                fs::read(app.join("Contents/Resources/licenses").join(name)).unwrap(),
                *bytes
            );
        }
        manifest.icon = None;
        let fallback = source.path().join("fallback.app");
        assert!(
            populate(
                source.path(),
                &manifest,
                &identity(),
                &build,
                &executable,
                &fallback,
                &default_icon
            )
            .is_err()
        );
        fs::create_dir(&default_icon).unwrap();
        fs::write(default_icon.join("Assets.car"), b"layered guest icon").unwrap();
        fs::write(default_icon.join("icon-guest.icns"), b"guest icon").unwrap();
        populate(
            source.path(),
            &manifest,
            &identity(),
            &build,
            &executable,
            &fallback,
            &default_icon,
        )
        .unwrap();
        let xml = fs::read_to_string(fallback.join("Contents/Info.plist")).unwrap();
        assert!(xml.contains("<key>CFBundleIconName</key><string>icon-guest</string>"));
        assert_eq!(
            fs::read(fallback.join("Contents/Resources/Assets.car")).unwrap(),
            b"layered guest icon"
        );
        assert!(!fallback.join("Contents/Resources/app.icns").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_publish_replaces_a_complete_bundle_and_retains_it_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let staged = directory.path().join("staged.app");
        let output = directory.path().join("Example.app");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("content"), b"first").unwrap();
        publish(&staged, &output).unwrap();
        assert!(!staged.exists());
        assert_eq!(fs::read(output.join("content")).unwrap(), b"first");

        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("content"), b"second").unwrap();
        publish(&staged, &output).unwrap();
        assert_eq!(fs::read(output.join("content")).unwrap(), b"second");
        assert_eq!(fs::read(staged.join("content")).unwrap(), b"first");

        assert!(publish(&directory.path().join("missing.app"), &output).is_err());
        assert_eq!(fs::read(output.join("content")).unwrap(), b"second");
    }
}
