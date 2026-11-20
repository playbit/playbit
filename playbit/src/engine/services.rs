//! Local data/account services, independent of app execution and process transport.
//! An embedding host supplies notification delivery and uses the typed store API.
//! The app supervisor composes the same owner with a Unix readiness adapter.
use crate::{
    package::Package,
    store::{BuildSelector, Id, LocalStore, Result, Session},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Clones share the same store; complete fresh-account enrollment before
/// lending clones to other owners, since account binding requires exclusivity.
#[derive(Clone)]
pub struct Services {
    /// Trusted management access. Portable apps receive scoped runtime contexts,
    /// not this handle. Workers may clone the Arc; the store closes only after
    /// its last owner releases it, so retire workers before tearing down the host.
    pub store: Arc<LocalStore>,
    #[cfg(feature = "file-provider")]
    pub(super) provider_store: Arc<parking_lot::RwLock<std::sync::Weak<LocalStore>>>,
    pub(super) path: PathBuf,
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    files: Arc<std::sync::Mutex<Option<Arc<super::files::owner::Owner>>>>,
}

impl Services {
    /// Open local services without a VM, network connection, process or IPC endpoint.
    /// The wake callback schedules a later bounded drain of the store's change
    /// receiver; follow `LocalStore::open_with_wake`'s lifetime/reentrancy contract.
    pub fn open(path: impl AsRef<Path>, wake: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let wake = Arc::new(wake);
        let notify = wake.clone();
        let store = Arc::new(LocalStore::open_with_wake(&path, move || notify())?);
        Ok(Self {
            store,
            #[cfg(feature = "file-provider")]
            provider_store: Default::default(),
            path,
            #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
            files: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    pub(crate) fn file_owner(&self) -> std::io::Result<Arc<super::files::owner::Owner>> {
        let mut files =
            self.files.lock().map_err(|_| std::io::Error::other("File owner lock poisoned"))?;
        if let Some(owner) = files.as_ref() {
            return Ok(owner.clone());
        }
        let owner = Arc::new(super::files::owner::Owner::new(self.path.clone())?);
        *files = Some(owner.clone());
        Ok(owner)
    }

    /// Retain an exact Wasm package for distribution and create its session.
    /// The embedding host must separately validate its chosen executor: this
    /// method caches content and metadata without constructing a Wasm compiler.
    pub fn create_session(&self, package: Package, root: Id) -> super::Result<Session> {
        let version = self.cache_package(&package)?;
        let path = self.path.join(crate::paths::BUILDS).join(&version);
        self.store.queue_build_upload(&version, &path)?;
        let session = self.store.create_session(
            &crate::package::AppIdentity {
                publisher: self.store.account().id,
                app_id: package.app_id().into(),
            },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root,
        )?;
        Ok(session)
    }

    fn cache_package(&self, package: &Package) -> super::Result<String> {
        let version = package.id()?;
        let directory = self.path.join(crate::paths::BUILDS);
        fs::create_dir_all(&directory)?;
        let path = directory.join(&version);
        if path.exists() {
            if Package::open(&path)?.id()? != version {
                return Err("cached app content does not match its identity".into());
            }
        } else {
            let temp = directory.join(format!(".{}.candidate", Id::generate()?));
            let result = (|| -> super::Result<()> {
                let mut file = fs::File::create_new(&temp)?;
                package.write_to(&mut file)?;
                if Package::open(&temp)?.digest() != package.digest() {
                    return Err("app content changed while importing".into());
                }
                rustix::io::retry_on_intr(|| rustix::fs::fsync(&file))
                    .map_err(std::io::Error::from)?;
                fs::rename(&temp, &path)?;
                Ok(())
            })();
            if temp.exists() {
                fs::remove_file(&temp)?;
            }
            result?;
            {
                let file = fs::File::open(&directory)?;
                rustix::io::retry_on_intr(|| rustix::fs::fsync(&file))
                    .map_err(std::io::Error::from)?;
            }
        }
        Ok(version)
    }

    /// A native creation intent already owns this seed. Replays retain the same
    /// local binding; automatic scopes initialize only when an app uses them.
    #[cfg(all(
        target_os = "macos",
        feature = "desktop-ui",
        feature = "native-runtime",
        feature = "connected-engine"
    ))]
    pub(crate) fn create_local_session(
        &self,
        package: &Package,
        seed: crate::store::Seed,
    ) -> super::Result<Session> {
        let package =
            self.store.authenticate_package(package.clone(), crate::store::now_unix_secs())?;
        let app = package.identity()?.clone();
        let build = self.cache_package(&package)?;
        let path = self.path.join(crate::paths::BUILDS).join(&build);
        self.store.queue_build_upload(&build, &path)?;
        self.store.remember_imported_app(&package)?;
        let root = app.volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)?;
        self.store
            .create_session_binding_with_seed(
                crate::store::SessionCreation {
                    app,
                    selector: BuildSelector::Exact { build_id: build.clone() },
                    resolved_build_id: build,
                    root_volume: root,
                    sync_shared: package
                        .initializers()
                        .declares(crate::package::AutomaticVolumeRole::Session),
                    user_volume: None,
                },
                seed,
            )
            .map_err(Into::into)
    }

    pub fn profile_path(&self) -> &Path {
        &self.path
    }

    /// Drain provider store access before rebinding. Other retained store owners
    /// still prevent mutation of an identity they may be using.
    #[cfg(feature = "connected-engine")]
    pub(super) fn bind_authenticated_account(&mut self, account: Id) -> super::Result<()> {
        #[cfg(feature = "file-provider")]
        let mut provider = self.provider_store.write();
        #[cfg(feature = "file-provider")]
        {
            *provider = std::sync::Weak::new();
        }
        let result = (|| -> super::Result<()> {
            Arc::get_mut(&mut self.store)
                .ok_or("finish active app or file operations before enrolling a different account")?
                .bind_authenticated_account(account)?;
            Ok(())
        })();
        #[cfg(feature = "file-provider")]
        {
            *provider = Arc::downgrade(&self.store);
        }
        result
    }

    /// The existing Unix supervisor selects descriptor readiness explicitly.
    #[cfg(feature = "connected-engine")]
    pub(super) fn open_unix(
        path: &Path,
        testing: Option<&crate::network::ap::hello::TestingCredentials>,
    ) -> Result<Self> {
        Ok(Self {
            store: Arc::new(LocalStore::open_admitted(path, |path, fresh| {
                super::testing_profile::admit(path, fresh, testing)
                    .map_err(|error| crate::store::Error::Credentials(error.to_string()))
            })?),
            #[cfg(feature = "file-provider")]
            provider_store: Default::default(),
            path: path.to_path_buf(),
            #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
            files: Arc::new(std::sync::Mutex::new(None)),
        })
    }
}
