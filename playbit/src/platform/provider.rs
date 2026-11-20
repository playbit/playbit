//! Native extension ABI. The system owns the process lifetime; no subprocess or
//! app runtime is needed. Local calls and the serial network lane share only
//! gated store leases. The host stops admission and joins callbacks before close.
#![cfg_attr(
    all(target_os = "macos", feature = "macos-shared-runtime"),
    allow(
        dead_code,
        reason = "The shared GUI omits separate SDK/provider C exports; their unused Rust support is removed by the linker."
    )
)]
#![allow(unsafe_code, reason = "C ABI borrows host buffers and owns opaque handles.")]
use crate::{
    engine::{
        connected::Stop,
        file_provider::replica::{Local, Replica},
    },
    store::Id,
};
use std::{
    ffi::{CStr, CString, c_char},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    sync::Mutex,
    time::Instant,
};

pub struct Provider {
    replica: Mutex<Replica>,
    local: Local,
    stop: Stop,
    wake: UnixStream,
}

fn text(value: impl AsRef<str>) -> *mut c_char {
    CString::new(value.as_ref().replace('\0', "�")).expect("NUL removed").into_raw()
}
fn result(operation: impl FnOnce() -> crate::engine::Result<()>) -> *mut c_char {
    match operation() {
        Ok(()) => std::ptr::null_mut(),
        Err(error) => text(error.to_string()),
    }
}
unsafe fn string<'a>(value: *const c_char, limit: usize) -> crate::engine::Result<&'a str> {
    if value.is_null() {
        return Err("missing provider text".into());
    }
    // SAFETY: The ABI caller supplies a live terminated UTF-8 string.
    let value = unsafe { CStr::from_ptr(value) }.to_str()?;
    if value.len() > limit {
        return Err("provider text exceeds limit".into());
    }
    Ok(value)
}

/// # Safety
/// Path is a live terminated string. All outputs point
/// to writable storage. Baseline owns JSON revision/anchor, freed with response_free.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_open(
    path: *const c_char,
    output: *mut *mut Provider,
    descriptor: *mut i32,
    baseline: *mut *mut c_char,
) -> *mut c_char {
    result(|| {
        if output.is_null() || descriptor.is_null() || baseline.is_null() {
            return Err("missing provider output".into());
        }
        // SAFETY: Both outputs were checked; the caller owns their storage.
        unsafe {
            *output = std::ptr::null_mut();
            *descriptor = -1;
            *baseline = std::ptr::null_mut();
        }
        let path = Path::new(unsafe { string(path, 4096)? });
        if !path.is_absolute() {
            return Err("provider profile must be absolute".into());
        }
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let replica = Replica::open(path, move || {
            // Readiness coalesces in a bounded kernel buffer; an already-full
            // buffer already guarantees a readable descriptor for the host.
            if let Err(error) = (&writer).write(&[1])
                && error.kind() != std::io::ErrorKind::WouldBlock
            {
                crate::diagnostic!("Provider readiness channel closed: {error}");
            }
        })?;
        let revision = replica
            .connected
            .services
            .store
            .delivery_provider_revision(None, None)
            .0
            .ok_or("provider change history is unavailable")?;
        let anchor = replica.connected.services.store.provider_account_anchor()?;
        let baseline_value =
            serde_json::to_string(&serde_json::json!({"revision":revision,"anchor":anchor}))?;
        let provider = Provider {
            local: replica.local(),
            stop: replica.connected.stop_handle(),
            replica: Mutex::new(replica),
            wake: reader,
        };
        // SAFETY: dup borrows a live descriptor and returns a host-owned copy.
        let fd = unsafe { libc::dup(provider.wake.as_raw_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            *descriptor = fd;
            *baseline = text(baseline_value);
            *output = Box::into_raw(Box::new(provider));
        }
        Ok(())
    })
}

/// # Safety
/// Handle remains open throughout the call; account and secret contain 16 bytes;
/// email is a live terminated UTF-8 string. Call only on the serial network lane.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_connect(
    handle: *const Provider,
    account: *const u8,
    email: *const c_char,
    secret: *const u8,
) -> *mut c_char {
    result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        if account.is_null() || secret.is_null() {
            return Err("missing provider credential".into());
        }
        // SAFETY: The caller supplies both fixed-width spans until return.
        let account: [u8; 16] = unsafe { std::slice::from_raw_parts(account, 16) }.try_into()?;
        let secret: &[u8; 16] = unsafe { std::slice::from_raw_parts(secret, 16) }.try_into()?;
        let email = unsafe { string(email, 254)? };
        host.replica.lock().map_err(|_| "provider network owner poisoned")?.connect(
            Id::from_bytes(account),
            email,
            secret,
        )
    })
}

/// # Safety
/// The open handle and `count * 16` volume bytes remain live until return.
/// Count is at most two. Call only on the serial network lane.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_select(
    handle: *const Provider,
    volumes: *const u8,
    count: u32,
) -> *mut c_char {
    result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        if count as usize > crate::engine::remote::PROVIDER_VOLUME_ADMISSION
            || (count != 0 && volumes.is_null())
        {
            return Err("invalid provider volume admission".into());
        }
        let mut selected = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let bytes: [u8; 16] =
                unsafe { std::slice::from_raw_parts(volumes.add(index * 16), 16) }.try_into()?;
            selected.push(Id::from_bytes(bytes));
        }
        host.replica
            .lock()
            .map_err(|_| "provider network owner poisoned")?
            .connected
            .select_provider_volumes(&selected)
    })
}

/// # Safety
/// Bytes remain live for the call. Returns only the canonical reference from
/// the supplied copy file; parsing grants no authority to its database.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_copy_source(bytes: *const u8, length: usize) -> *mut c_char {
    let value = (|| -> crate::engine::Result<serde_json::Value> {
        if bytes.is_null() || length > 65536 {
            return Err("invalid provider request size".into());
        }
        let source = crate::engine::file_provider::database_copy_source(unsafe {
            std::slice::from_raw_parts(bytes, length)
        })?;
        Ok(source.map_or_else(
            || serde_json::json!({}),
            |source| serde_json::json!({"volume":source.volume,"entry":source.entry,"reference":source.to_string()}),
        ))
    })().unwrap_or_else(|error| serde_json::json!({"error":error.to_string()}));
    match serde_json::to_string(&value) {
        Ok(value) if value.len() <= 4096 => text(value),
        _ => text("{\"error\":\"invalid database copy reference\"}"),
    }
}

/// # Safety
/// The handle and fixed-width account/volume (16-byte) and entry (15-byte)
/// spans remain live. Call only on the serial network lane. Source hydration
/// never keeps an account-transition lease or a destination writer lock.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_prepare_copy(
    handle: *const Provider,
    account: *const u8,
    destination: *const u8,
    volume: *const u8,
    entry: *const u8,
) -> *mut c_char {
    result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        if account.is_null() || destination.is_null() || volume.is_null() || entry.is_null() {
            return Err("missing database copy source".into());
        }
        let account: [u8; 16] = unsafe { std::slice::from_raw_parts(account, 16) }.try_into()?;
        let volume: [u8; 16] = unsafe { std::slice::from_raw_parts(volume, 16) }.try_into()?;
        let destination: [u8; 16] =
            unsafe { std::slice::from_raw_parts(destination, 16) }.try_into()?;
        let entry: [u8; 15] = unsafe { std::slice::from_raw_parts(entry, 15) }.try_into()?;
        let mut replica = host.replica.lock().map_err(|_| "provider network owner poisoned")?;
        if replica.connected.services.store.account().id != Id::from_bytes(account) {
            return Err("File Provider account is no longer signed in".into());
        }
        replica.connected.prepare_provider_copy(
            Id::from_bytes(destination),
            crate::store::volume_index::EntryAddress {
                volume: Id::from_bytes(volume),
                entry: crate::store::EntryId::from_bytes(entry)?,
            },
        )
    })
}

/// # Safety
/// Handle remains open; both outputs point to writable u64 storage. One caller on
/// the network lane drives readiness/deadlines; u64::MAX means no deadline.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_advance(
    handle: *const Provider,
    deadline: *mut u64,
    connection: *mut u64,
) -> *mut c_char {
    result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        if deadline.is_null() || connection.is_null() {
            return Err("missing provider deadline".into());
        }
        let mut buffer = [0; 256];
        // Drain only the bounded kernel readiness buffer. This is not polling.
        for _ in 0..64 {
            match (&host.wake).read(&mut buffer) {
                Ok(0) => return Err("provider readiness channel closed".into()),
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        let mut replica = host.replica.lock().map_err(|_| "provider network owner poisoned")?;
        let now = Instant::now();
        let advanced = replica.connected.advance(now);
        let next = replica.connected.wake_after(Instant::now());
        unsafe {
            *deadline =
                next.map_or(u64::MAX, |wait| wait.as_nanos().min(u64::MAX as u128 - 1) as u64);
            *connection = if replica.connected.account_initialized() {
                replica.connected.provider_connection_generation().unwrap_or(0)
            } else {
                0
            };
        }
        advanced.map(|_| ())
    })
}

/// # Safety
/// Handle remains open. Call on the serial network lane after the local reply;
/// no account-publication lease or file worker is held while awaiting delivery.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_flush(handle: *const Provider) -> *mut c_char {
    result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        host.replica
            .lock()
            .map_err(|_| "provider network owner poisoned")?
            .connected
            .flush_provider()
    })
}

/// # Safety
/// Handle and bytes remain live for this call. Calls are bounded by the native
/// host's admission gate. The returned JSON string is freed with response_free.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_request(
    handle: *const Provider,
    bytes: *const u8,
    length: usize,
) -> *mut c_char {
    let response = (|| -> crate::engine::Result<serde_json::Value> {
        let host = unsafe { handle.as_ref() }.ok_or("provider is closed")?;
        if bytes.is_null() || length > 65536 {
            return Err("invalid provider request size".into());
        }
        host.local.execute(unsafe { std::slice::from_raw_parts(bytes, length) })
    })();
    let value = response
        .unwrap_or_else(|error| crate::engine::file_provider::error_response(error.as_ref()));
    match serde_json::to_string(&value) {
        Ok(value) if value.len() <= crate::store::file_provider::PROVIDER_REPLY_BYTES => {
            text(value)
        }
        _ => text("{\"error\":\"provider response exceeds limit; a mutation may have completed\"}"),
    }
}

/// # Safety
/// Handle remains open. Stop can run while a network call is blocked.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_stop(handle: *const Provider) {
    if let Some(host) = unsafe { handle.as_ref() } {
        host.stop.request();
    }
}
/// # Safety
/// Stop admission, call stop, and finish all admitted calls before closing once.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_close(handle: *mut Provider) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}
/// # Safety
/// Free exactly once a nonnull string returned by this module.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_provider_response_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}
