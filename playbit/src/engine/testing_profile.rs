//! Explicit developer testing setup. The profile retains only its namespace;
//! credentials are read from a private file and remain in process memory.
use super::Result;
use crate::{network::ap::hello::TestingCredentials, release_channel::ReleaseChannel, store::Id};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub const CREDENTIALS_ENV: &str = "PLAYBIT_TESTING_CREDENTIALS";
const MARKER: &str = "testing-namespace";

pub fn from_environment() -> Result<Option<TestingCredentials>> {
    let Some(path) = std::env::var_os(CREDENTIALS_ENV) else {
        return Ok(None);
    };
    let profile = std::env::var_os("PLAYBIT_PROFILE")
        .filter(|path| !path.is_empty())
        .ok_or("testing credentials require an explicit isolated PLAYBIT_PROFILE")?;
    if super::profile::channel_from_environment()? != Some(ReleaseChannel::Testing) {
        return Err("integration credentials require PLAYBIT_CHANNEL=testing".into());
    }
    require_isolated(Path::new(&profile))?;
    read(Path::new(&path)).map(Some)
}

fn private_record(path: &Path, limit: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != super::profile::current_uid()
        || metadata.nlink() != 1
        || metadata.len() > limit
    {
        return Err(
            "integration credentials and namespace records must be bounded private files".into()
        );
    }
    Ok(file)
}

pub fn read(path: &Path) -> Result<TestingCredentials> {
    let file = private_record(path, 1024)?;
    let mut bytes = Vec::new();
    file.take(1025).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 {
        return Err("testing credentials exceed 1024 bytes".into());
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Credentials {
        key: String,
        namespace: String,
    }
    let value: Credentials =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid integration credentials file")?;
    let key = *Id::parse(&value.key).map_err(|_| "Invalid integration key")?.as_bytes();
    let namespace =
        *Id::parse(&value.namespace).map_err(|_| "Invalid integration namespace")?.as_bytes();
    if key == [0; 16] || namespace == [0; 16] {
        return Err("testing key and namespace must be nonzero".into());
    }
    Ok(TestingCredentials { key, namespace })
}

fn require_isolated(profile: &Path) -> Result<()> {
    let profile = super::profile::canonical_profile(profile)?;
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?.canonicalize()?;
    for name in [".playbit", ".playbit-testing"] {
        if profile == super::profile::canonical_profile(&home.join(name))? {
            return Err("Integration credentials require a custom isolated profile".into());
        }
    }
    #[cfg(target_os = "macos")]
    for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
        if super::profile::is_default_profile(&profile, channel)? {
            return Err("Integration credentials cannot use a support-user profile".into());
        }
    }
    Ok(())
}

/// Return whether a new marker is needed. Refuse to reinterpret an ordinary
/// populated profile or let a testing profile fall back to the ordinary AP.
pub(crate) fn validate(profile: &Path, testing: Option<&TestingCredentials>) -> Result<bool> {
    let channel = super::profile::read(&profile.join(super::profile::PROFILE_MARKER))?;
    if let Some(testing) = testing {
        require_isolated(profile)?;
        if testing.key == [0; 16] || testing.namespace == [0; 16] {
            return Err("integration key and namespace must be nonzero".into());
        }
        if channel == Some(ReleaseChannel::Public) {
            return Err("Integration credentials require a testing-channel profile".into());
        }
    }
    let marker = profile.join(MARKER);
    match private_record(&marker, 22) {
        Ok(file) => {
            if channel != Some(ReleaseChannel::Testing) {
                return Err("Integration namespace requires a testing-channel profile".into());
            }
            let mut value = String::new();
            file.take(23).read_to_string(&mut value)?;
            let expected =
                testing.ok_or("this testing profile requires PLAYBIT_TESTING_CREDENTIALS")?;
            if value != Id::from_bytes(expected.namespace).to_string() {
                return Err("testing profile belongs to another namespace".into());
            }
            Ok(false)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            if testing.is_none() {
                return Ok(false);
            }
            if profile.join(crate::paths::PROFILE_DB).try_exists()? {
                return Err("Integration credentials require a fresh profile; ordinary profiles cannot be reused".into());
            }
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Storage holds local.lock before checking freshness or writing either marker.
/// An ordinary opener racing this binding must acquire that same lock and
/// recheck the namespace before it can open or create any account metadata.
pub(crate) fn admit(
    profile: &Path,
    fresh: bool,
    testing: Option<&TestingCredentials>,
) -> Result<()> {
    if validate(profile, testing)? {
        if !fresh {
            return Err("Integration credentials require a fresh isolated profile".into());
        }
        super::profile::prepare(profile, ReleaseChannel::Testing)?;
        bind(profile, testing.expect("validated integration credentials").namespace)?;
    }
    Ok(())
}

/// Called with the ordinary exclusive profile store lock held.
fn bind(profile: &Path, namespace: [u8; 16]) -> Result<()> {
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(profile.join(MARKER))?;
    file.write_all(Id::from_bytes(namespace).to_string().as_bytes())?;
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&file))?;
    let directory = File::open(profile)?;
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn credentials_are_bounded_private_and_strict() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let valid = format!(
            r#"{{"key":"{}","namespace":"{}"}}"#,
            crate::identifier::encode(&[0x12; 16]),
            crate::identifier::encode(&[0x34; 16])
        );
        fs::write(&path, &valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&path).unwrap().namespace, [0x34; 16]);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for invalid in [
            "x".repeat(1025),
            valid.replace(
                &crate::identifier::encode(&[0x34; 16]),
                &crate::identifier::encode(&[0; 16]),
            ),
            valid.replace('}', ",\"extra\":true}"),
            "{}".into(),
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(read(&path).is_err());
        }
    }
    #[test]
    fn namespace_binding_never_reinterprets_an_ordinary_or_other_testing_profile() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let testing = TestingCredentials { key: [1; 16], namespace: [2; 16] };
        assert!(validate(&profile, Some(&testing)).unwrap());
        // A launcher may create diagnostics/control before durable admission.
        fs::create_dir_all(profile.join("logs")).unwrap();
        fs::write(profile.join("logs/engine.log"), b"launcher diagnostics").unwrap();
        let services = super::super::Services::open_unix(&profile, Some(&testing)).unwrap();
        assert_eq!(super::super::profile::for_profile(&profile).unwrap(), ReleaseChannel::Testing);
        assert!(super::super::Services::open_unix(&profile, None).is_err());
        drop(services);
        assert!(!validate(&profile, Some(&testing)).unwrap());
        assert!(validate(&profile, None).is_err());
        assert!(
            validate(&profile, Some(&TestingCredentials { namespace: [3; 16], ..testing }))
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(profile.join(MARKER)).unwrap(),
            crate::identifier::encode(&[0x02; 16])
        );
        let ordinary = root.path().join("ordinary");
        drop(crate::store::LocalStore::open(&ordinary).unwrap());
        let retained = fs::read(ordinary.join(crate::paths::PROFILE_DB)).unwrap();
        assert!(super::super::Services::open_unix(&ordinary, Some(&testing)).is_err());
        assert_eq!(fs::read(ordinary.join(crate::paths::PROFILE_DB)).unwrap(), retained);
        assert!(!validate(&ordinary, None).unwrap());
        assert!(super::super::Services::open_unix(&ordinary, None).is_ok());
    }
    #[test]
    fn fixture_binding_holds_the_store_lock_before_account_initialization() {
        let root = tempfile::tempdir().unwrap();
        let testing = TestingCredentials { key: [1; 16], namespace: [2; 16] };
        let store = crate::store::LocalStore::open_admitted(root.path(), |path, fresh| {
            assert!(fresh);
            // This competing ordinary opener would initialize the same profile
            // if the admission callback did not already own the store lock.
            assert!(super::super::Services::open_unix(path, None).is_err());
            admit(path, fresh, Some(&testing))
                .map_err(|error| crate::store::Error::Credentials(error.to_string()))
        })
        .unwrap();
        assert!(validate(root.path(), None).is_err());
        assert_eq!(store.profile_path(), root.path().canonicalize().unwrap());
    }
    #[test]
    #[ignore = "reference hardware only: 1000 private credentials and namespace checks"]
    fn benchmark_testing_profile_validation() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        fs::create_dir(&profile).unwrap();
        let path = directory.path().join("credentials.json");
        fs::write(
            &path,
            format!(
                r#"{{"key":"{}","namespace":"{}"}}"#,
                crate::identifier::encode(&[0x12; 16]),
                crate::identifier::encode(&[0x34; 16])
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let credentials = read(&path).unwrap();
        drop(super::super::Services::open_unix(&profile, Some(&credentials)).unwrap());
        let mut samples = Vec::new();
        for index in 0..1100 {
            let begin = std::time::Instant::now();
            let credentials = read(&path).unwrap();
            assert!(!validate(&profile, Some(&credentials)).unwrap());
            if index >= 100 {
                samples.push(begin.elapsed().as_nanos() as u64);
            }
        }
        samples.sort_unstable();
        println!(
            "{}",
            serde_json::json!({"benchmark":"testing_profile_validation", "optimized":!cfg!(debug_assertions),
            "samples":samples.len(),"p50_us":samples[499] as f64/1000.0,"p95_us":samples[949] as f64/1000.0,
            "p99_us":samples[989] as f64/1000.0,"max_us":samples[999] as f64/1000.0})
        );
        assert!(samples[989] < 2_000_000, "profile validation exceeds 2 ms p99");
    }
}
