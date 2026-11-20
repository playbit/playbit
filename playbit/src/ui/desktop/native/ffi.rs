//! Concrete C Mac application host. It copies bounded launch inputs before
//! starting one main-thread event loop and joins the app worker before AppKit exits.
#![allow(unsafe_code, reason = "The caller follows the stable playbit/mac.h C contract.")]
use super::{Launch, run};
use crate::{
    engine::ffi::{
        memory::{input, read_native},
        store_error,
    },
    store::{MAX_ROOT_INITIALIZATION_BYTES, RootInitialization},
};
use playbit_api::abi::{operations::App, syscall::Error};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

static STARTED: AtomicBool = AtomicBool::new(false);
struct Configuration {
    profile: Option<PathBuf>,
    title: String,
    declaration: RootInitialization,
    app: App,
}
// SAFETY: mac.h requires callback code and user_data to remain accessible on
// the worker through return. No app-owned Rust value crosses this boundary.
unsafe impl Send for Configuration {}

unsafe fn text(pointer: *const u8, length: u32, limit: u32) -> Result<String, Error> {
    input(pointer, length, limit)?;
    // SAFETY: admitted region is stable readable storage by the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) };
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidArgument)?;
    if text.contains('\0') {
        return Err(Error::InvalidArgument);
    }
    Ok(text.to_owned())
}

impl Configuration {
    unsafe fn read(
        profile: *const u8,
        profile_length: u32,
        title: *const u8,
        title_length: u32,
        declaration: *const u8,
        declaration_length: u32,
        app: *const App,
    ) -> Result<Self, Error> {
        // SAFETY: the caller lends the declared record; admission copies fields
        // and refuses nonzero unsupported extension bytes before any launch work.
        let app = unsafe { read_native(app, true)? };
        if app.init.is_none() || app.turn.is_none() || app.destroy.is_none() {
            return Err(Error::InvalidArgument);
        }
        let profile = if profile.is_null() && profile_length == 0 {
            None
        } else {
            Some(PathBuf::from(unsafe { text(profile, profile_length, 4096)? }))
        };
        let title = unsafe { text(title, title_length, 256)? };
        if profile.as_ref().is_some_and(|profile| !profile.is_absolute()) || title.trim().is_empty()
        {
            return Err(Error::InvalidArgument);
        }
        input(declaration, declaration_length, MAX_ROOT_INITIALIZATION_BYTES as u32)?;
        // SAFETY: length and address admitted; borrowed bytes never escape parsing.
        let bytes = unsafe { std::slice::from_raw_parts(declaration, declaration_length as usize) };
        let declaration = RootInitialization::from_json(bytes).map_err(store_error)?;
        Ok(Self { profile, title, declaration, app })
    }
    unsafe fn run(self) -> Result<(), Error> {
        let profile = self.profile.clone().ok_or(Error::InvalidArgument)?;
        crate::platform::failure::startup(&profile);
        crate::platform::failure::install_native(&profile);
        // SAFETY: the C caller retains callback code/user_data until the joined
        // event loop returns. Capture the whole Send configuration, not a raw field.
        unsafe { run(profile, move |services| self.prepare(services)) }.map_err(|_| Error::Internal)
    }
    fn prepare(self, services: &crate::engine::Services) -> Result<Vec<Launch>, String> {
        let root = services
            .store
            .prepare_app_root(&self.title, &self.declaration)
            .map_err(|error| error.to_string())?
            .id;
        Ok(vec![Launch { title: self.title, root, app: self.app }])
    }
}

#[cfg(feature = "connected-engine")]
fn select_profile(profile: Option<&std::path::Path>) -> Result<PathBuf, Error> {
    let select = || -> crate::engine::Result<PathBuf> {
        if let Some(profile) = profile {
            let channel = crate::engine::profile::channel_from_environment()?
                .unwrap_or(crate::engine::profile::for_profile(profile)?);
            crate::engine::profile::initialize_selected(profile, channel)?;
        } else {
            crate::engine::profile::initialize()?;
        }
        Ok(crate::engine::profile_path())
    };
    select().map_err(|error| {
        crate::diagnostic!("Cannot select Playbit environment: {error}");
        Error::InvalidArgument
    })
}

/// # Safety
/// Pointers and callback lifetimes obey playbit/mac.h. Invoke on the process's
/// main thread; callback code/user_data remain valid until process termination or an early return.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_mac_run(
    profile: *const u8,
    profile_length: u32,
    title: *const u8,
    title_length: u32,
    declaration: *const u8,
    declaration_length: u32,
    app: *const App,
) -> i32 {
    let execute = || {
        // SAFETY: libc's thread inquiry has no pointer/lifetime preconditions.
        if unsafe { libc::pthread_main_np() } == 0 || STARTED.load(Ordering::Acquire) {
            return Err(Error::Conflict);
        }
        // SAFETY: C lends stable records and strings for this synchronous admission.
        let configuration = unsafe {
            Configuration::read(
                profile,
                profile_length,
                title,
                title_length,
                declaration,
                declaration_length,
                app,
            )
        }?;
        if configuration.profile.is_none() {
            return Err(Error::InvalidArgument);
        }
        if STARTED.swap(true, Ordering::AcqRel) {
            return Err(Error::Conflict);
        }
        // SAFETY: thread and process lifetime admitted; foreign callbacks obey C contract.
        unsafe { configuration.run() }
    };
    execute().map_or_else(|error| error as i32, |()| 0)
}

/// # Safety
/// The caller lends stable input buffers through admission and retains callbacks
/// implementing this exact package through process termination. Call on main.
#[cfg(feature = "connected-engine")]
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_mac_run_portable(
    profile: *const u8,
    profile_length: u32,
    package_path: *const u8,
    package_path_length: u32,
    digest: *const u8,
    app: *const App,
) -> i32 {
    let execute = || {
        // SAFETY: the thread inquiry has no pointer preconditions.
        if unsafe { libc::pthread_main_np() } == 0 || STARTED.load(Ordering::Acquire) {
            return Err(Error::Conflict);
        }
        // SAFETY: the caller lends a readable path and 32-byte digest through admission.
        let path = unsafe { text(package_path, package_path_length, 4096)? };
        input(digest, 32, 32)?;
        let expected = unsafe { std::ptr::read(digest.cast::<[u8; 32]>()) };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: the caller lends the declared record for bounded field admission.
        let app = unsafe { read_native(app, true)? };
        if app.init.is_none() || app.turn.is_none() || app.destroy.is_none() {
            return Err(Error::InvalidArgument);
        }
        let profile = if profile.is_null() && profile_length == 0 {
            None
        } else {
            Some(PathBuf::from(unsafe { text(profile, profile_length, 4096)? }))
        };
        if profile.as_ref().is_some_and(|profile| !profile.is_absolute()) {
            return Err(Error::InvalidArgument);
        }
        if STARTED.swap(true, Ordering::AcqRel) {
            return Err(Error::Conflict);
        }
        let profile = select_profile(profile.as_deref())?;
        crate::platform::failure::startup(&profile);
        crate::platform::failure::install_native(&profile);
        // Only bounded launch inputs cross the UI admission boundary. The setup
        // worker verifies and retains the package before any native callback.
        let portable = super::portable::Portable { path, expected, app };
        // SAFETY: the caller retains matching callbacks through both owner joins.
        unsafe { super::run_portable(profile, portable) }.map_err(|_| Error::Internal)
    };
    execute().map_or_else(|error| error as i32, |()| 0)
}

#[cfg(test)]
mod tests;
