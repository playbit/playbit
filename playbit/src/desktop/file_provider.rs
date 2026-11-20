//! The containing application owns domain, PushKit and Finder-opening APIs.
//! Filesystem preparation stays off the UI owner; native completion wakes it.
use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

const OPEN_ERROR_BYTES_MAX: usize = 4096;
type OpenResult = Result<(), String>;
type OpenCompletion = extern "C" fn(*mut libc::c_void, *const libc::c_char);
type OpenVolume = unsafe extern "C" fn(
    *const libc::c_char,
    *const libc::c_char,
    *const libc::c_char,
    *mut libc::c_void,
    OpenCompletion,
);
static OPEN_VOLUME: OnceLock<OpenVolume> = OnceLock::new();
static OPEN_ENTRY: OnceLock<OpenVolume> = OnceLock::new();
static REVEAL_ENTRY: OnceLock<OpenVolume> = OnceLock::new();
static OPENING: AtomicBool = AtomicBool::new(false);

struct OpenGuard;

#[derive(Clone, Copy)]
enum OpenKind {
    Volume,
    Entry,
    Reveal,
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        OPENING.store(false, Ordering::Release);
    }
}

pub(super) fn open_volume(account: crate::store::Id, volume: crate::store::Id, cx: &mut gpui::App) {
    open_item(account, volume.to_string(), OpenKind::Volume, cx);
}

pub(super) fn open_entry(
    account: crate::store::Id,
    address: crate::store::volume_index::EntryAddress,
    cx: &mut gpui::App,
) {
    let item =
        crate::store::file_provider::provider_entry_identifier(address.volume, address.entry);
    open_item(account, item, OpenKind::Entry, cx);
}

pub(super) fn reveal_entry(
    account: crate::store::Id,
    address: crate::store::volume_index::EntryAddress,
    cx: &mut gpui::App,
) {
    let item =
        crate::store::file_provider::provider_entry_identifier(address.volume, address.entry);
    open_item(account, item, OpenKind::Reveal, cx);
}

fn open_item(account: crate::store::Id, item: String, kind: OpenKind, cx: &mut gpui::App) {
    if OPENING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        crate::ui::desktop::alert::show(
            crate::ui::desktop::alert::Kind::FinderOpen,
            "Finder is busy opening another item. Wait for it to finish, then try again.",
            cx,
        );
        return;
    }
    let opening = OpenGuard;
    let prepared = cx.background_executor().spawn(async move { prepare_open(kind) });
    cx.spawn(async move |cx| {
        let _opening = opening;
        let result = match prepared.await {
            Ok((profile, open)) => {
                let account = CString::new(account.to_string()).expect("base58 has no NUL");
                let item = CString::new(item).expect("base58 has no NUL");
                let (send, receive) = async_channel::bounded(1);
                if cx.update(|_| start_open(open, &profile, &account, &item, send)).is_err() {
                    return;
                }
                receive
                    .recv()
                    .await
                    .unwrap_or_else(|_| Err("Finder did not finish opening the item.".into()))
            }
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = result {
            let _ = cx.update(|cx| {
                crate::ui::desktop::alert::show(
                    crate::ui::desktop::alert::Kind::FinderOpen,
                    &error,
                    cx,
                );
            });
        }
    })
    .detach();
}

#[allow(unsafe_code, reason = "Load one fixed C entry point from the signed application library.")]
fn prepare_open(kind: OpenKind) -> crate::engine::Result<(CString, OpenVolume)> {
    let profile = crate::engine::profile_path();
    crate::engine::file_provider::require_system_profile(&profile)?;
    let profile = crate::engine::profile::canonical_profile(&profile)?;
    let profile = CString::new(profile.to_str().ok_or("File Provider profile is not UTF-8")?)?;
    let (cached, symbol_name) = match kind {
        OpenKind::Volume => (&OPEN_VOLUME, c"playbit_open_volume"),
        OpenKind::Entry => (&OPEN_ENTRY, c"playbit_open_entry"),
        OpenKind::Reveal => (&REVEAL_ENTRY, c"playbit_reveal_entry"),
    };
    if let Some(open) = cached.get() {
        return Ok((profile, *open));
    }
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("Open in Finder requires the packaged Playbit application")?;
    let path =
        CString::new(contents.join("Frameworks/PlaybitDrives.dylib").as_os_str().as_bytes())?;
    // SAFETY: the fixed library is inside this executable's signed bundle. Keep
    // its image loaded for the process lifetime, including native callbacks.
    let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() {
        return Err("Could not load packaged File Provider integration".into());
    }
    // SAFETY: Drives.swift exports this fixed symbol with the OpenVolume C ABI.
    let symbol = unsafe { libc::dlsym(library, symbol_name.as_ptr()) };
    if symbol.is_null() {
        // SAFETY: no callback or function pointer escaped this owned image.
        unsafe { libc::dlclose(library) };
        return Err("Packaged File Provider integration cannot open volumes in Finder".into());
    }
    // SAFETY: the fixed exported symbol has the declared C signature.
    let open: OpenVolume = unsafe { std::mem::transmute(symbol) };
    let _ = cached.set(open);
    Ok((profile, open))
}

#[allow(
    unsafe_code,
    reason = "The signed Swift entry copies inputs and completes this one context."
)]
fn start_open(
    open: OpenVolume,
    profile: &CString,
    account: &CString,
    item: &CString,
    send: async_channel::Sender<OpenResult>,
) {
    let context = Box::into_raw(Box::new(send)).cast();
    // SAFETY: Swift copies these bounded strings before returning, then calls
    // complete_open exactly once. Its context remains owned until that callback.
    unsafe {
        open(profile.as_ptr(), account.as_ptr(), item.as_ptr(), context, complete_open);
    }
}

#[allow(
    unsafe_code,
    reason = "Swift returns this owned context once with a bounded C error string."
)]
extern "C" fn complete_open(context: *mut libc::c_void, error: *const libc::c_char) {
    // SAFETY: only start_open creates this context, and Swift returns it once.
    let send = unsafe { Box::from_raw(context.cast::<async_channel::Sender<OpenResult>>()) };
    let result = if error.is_null() {
        Ok(())
    } else {
        // SAFETY: Swift provides a NUL-terminated error of at most 4096 bytes
        // that stays alive through this callback. Copy it before returning.
        let count = unsafe { libc::strnlen(error, OPEN_ERROR_BYTES_MAX + 1) };
        if count > OPEN_ERROR_BYTES_MAX {
            Err("Finder returned an oversized error message.".into())
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(error.cast::<u8>(), count) };
            Err(String::from_utf8_lossy(bytes).into_owned())
        }
    };
    let _ = send.try_send(result);
}

pub fn requested() -> bool {
    matches!(
        std::env::args_os().nth(1).as_deref().and_then(std::ffi::OsStr::to_str),
        Some("--file-provider-drives" | "--file-provider-domains" | "--file-provider-retire")
    )
}

#[allow(
    unsafe_code,
    reason = "Load one fixed, signed library from this application bundle and call its fixed C entry points."
)]
pub fn run() -> crate::engine::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next().ok_or("Missing File Provider operation")?;
    let command = command.into_string().map_err(|_| "File Provider operation is not UTF-8")?;
    if !matches!(
        command.as_str(),
        "--file-provider-drives" | "--file-provider-domains" | "--file-provider-retire"
    ) {
        return Err("Unknown File Provider operation".into());
    }
    let domain = if command == "--file-provider-retire" {
        let domain = args.next().ok_or("Expected --file-provider-retire ACCOUNT-VOLUME")?;
        // Two canonical 16-byte IDs require at most 22 bytes each plus '-'.
        if domain.len() > 45 {
            return Err("Legacy File Provider domain identifier exceeds 45 bytes".into());
        }
        let domain = domain.into_string().map_err(|_| "File Provider domain is not UTF-8")?;
        Some(CString::new(domain)?)
    } else {
        None
    };
    if args.next().is_some() {
        return Err("Unexpected File Provider argument".into());
    }
    let management = command != "--file-provider-drives";
    crate::engine::profile::initialize()?;
    let profile = crate::engine::profile_path();
    crate::engine::file_provider::require_system_profile(&profile)?;
    if !management {
        super::process_log::initialize(&profile)?;
    }
    let profile = crate::engine::profile::canonical_profile(&profile)?;
    let profile = CString::new(profile.to_str().ok_or("File Provider profile is not UTF-8")?)?;
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("File Provider requires the packaged Playbit application")?;
    let path =
        CString::new(contents.join("Frameworks/PlaybitDrives.dylib").as_os_str().as_bytes())?;
    // SAFETY: The path is NUL-terminated and derived from this executable, not
    // request input. Hardened-runtime library validation checks the company
    // signature. Retain the loaded image for the entire foreign call.
    let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() {
        return Err("Could not load packaged File Provider integration".into());
    }
    let name =
        if management { c"playbit_manage_file_provider" } else { c"playbit_run_file_provider" };
    // SAFETY: These fixed symbols are exported with the C ABI by Drives.swift.
    let symbol = unsafe { libc::dlsym(library, name.as_ptr()) };
    if symbol.is_null() {
        // SAFETY: dlopen returned this owned, non-null image handle.
        unsafe {
            libc::dlclose(library);
        }
        return Err("Packaged File Provider integration is missing its entry point".into());
    }
    let result = if management {
        // SAFETY: The management entry point has this C signature. Both C strings
        // remain owned here through its synchronous completion/deadline; null
        // requests a read-only listing. The call never unwinds into Rust.
        let run: unsafe extern "C" fn(*const libc::c_char, *const libc::c_char) -> i32 =
            unsafe { std::mem::transmute(symbol) };
        unsafe { run(profile.as_ptr(), domain.as_ref().map_or(std::ptr::null(), |id| id.as_ptr())) }
    } else {
        // SAFETY: The coordinator entry point has this C signature and does not
        // escape pointers or unwind into Rust. Its run loop owns its callbacks.
        let run: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(symbol) };
        unsafe { run() }
    };
    // Retain this one image until process exit, including any Foundation
    // callbacks installed before a startup failure.
    if result != 0 {
        return Err("File Provider operation failed".into());
    }
    Ok(())
}
