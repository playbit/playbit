//! Native profile resources. An explicit profile never shares its mutable cache.
use std::{
    io,
    path::{Path, PathBuf},
};

pub const PROFILE_NAME: &str = "playbit.engine";

pub const PROFILE_DB: &str = "profile.kvdb";
pub const ACCOUNT_DB: &str = "account.kvdb";
pub const VOLUMES: &str = "volumes";
pub const STAGING: &str = "staging";
pub const BLOBS: &str = "blobs";
pub const BUILDS: &str = "builds";
pub const RECEIVED: &str = "received";
pub const NATIVE_APPS: &str = "apps";
pub const APP_DISPLAYS: &str = "cache/app-displays";
pub const CORRUPT_BLOBS: &str = "received/corrupt-blobs";
pub const ENGINE_SOCKET: &str = "control/engine.sock";
pub const LOCAL_LOCK: &str = "control/local.lock";
pub const TRACE: &str = "traces";

/// Create profile-owned directory components without following internal symlinks.
/// The root may be a user-selected alias; children always belong to that root.
pub(crate) fn private_directory(profile: &Path, relative: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut path = profile.canonicalize()?;
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::other("invalid profile directory"));
        };
        path.push(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other("profile directory is not an ordinary directory"));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

pub struct Paths {
    pub blob_cache: PathBuf,
    #[cfg(any(test, feature = "process", all(unix, feature = "connected-engine")))]
    pub launch_lock: PathBuf,
    pub engine_log: PathBuf,
}

impl Paths {
    /// Resolving diagnostics never creates a profile or an identity.
    pub fn diagnostic_log(profile: &Path) -> io::Result<PathBuf> {
        Ok(Self::at(&std::path::absolute(profile)?).engine_log)
    }

    /// Canonical aliases share locks, diagnostics and storage.
    pub fn for_profile(profile: &Path) -> io::Result<Self> {
        Ok(Self::at(&profile.canonicalize()?))
    }

    fn at(profile: &Path) -> Self {
        Self {
            blob_cache: profile.join("cache/blobs"),
            #[cfg(any(test, feature = "process", all(unix, feature = "connected-engine")))]
            launch_lock: profile.join("control/launch.lock"),
            engine_log: profile.join("logs/engine.log"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn distinct_profiles_with_extensions_do_not_share_logs_or_locks() {
        let first = Paths::at(Path::new("/tmp/profile.one"));
        let second = Paths::at(Path::new("/tmp/profile.two"));
        assert_ne!(first.launch_lock, second.launch_lock);
        assert_ne!(first.engine_log, second.engine_log);
        assert_ne!(first.blob_cache, second.blob_cache);
        assert_eq!(first.engine_log, Path::new("/tmp/profile.one/logs/engine.log"));
    }

    #[test]
    fn symlinked_profile_uses_the_same_launcher_lock() {
        let root = tempfile::tempdir().unwrap();
        let actual = root.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&actual, &link).unwrap();
        let first = Paths::for_profile(&actual).unwrap();
        let second = Paths::for_profile(&link).unwrap();
        assert_eq!(first.launch_lock, second.launch_lock);
        assert_eq!(first.engine_log, second.engine_log);
        assert_eq!(first.blob_cache, second.blob_cache);
    }
    #[test]
    #[ignore = "reference-device path resolution timing"]
    fn benchmark_profile_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            let paths = Paths::for_profile(root.path()).unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            assert!(paths.blob_cache.ends_with("cache/blobs"));
        }
        samples.sort_by(f64::total_cmp);
        crate::diagnostic!(
            "profile-paths n=1000 p50={:.6} p99={:.6} max={:.6} ms",
            samples[499],
            samples[989],
            samples[999]
        );
    }
}
