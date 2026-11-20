//! Credential handoff for the iOS containing app. The host serializes this with
//! account actions and writes only to its protected, company App Group ledger.
use super::*;

/// # Safety
/// Same handle rules as pb_ios_open. Outputs are writable; return is an owned
/// error or null. A nonnull snapshot is freed with pb_ios_string_free and must
/// never be logged. Null snapshot means no catalog/account change.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_provider_state(
    handle: *mut IosEngine,
    known: *const u8,
    known_length: usize,
    output: *mut *mut c_char,
) -> *mut c_char {
    native_result(|| {
        if output.is_null() {
            return Err("Missing provider state output".into());
        }
        if known.is_null() || known_length > 8192 {
            return Err("Invalid File Provider known locations".into());
        }
        let known: Vec<crate::store::Id> =
            serde_json::from_slice(unsafe { std::slice::from_raw_parts(known, known_length) })
                .map_err(|_| "Invalid File Provider location identities")?;
        if known.len() > crate::store::file_provider::PROVIDER_VOLUME_LIMIT {
            return Err("File Provider known location limit exceeded".into());
        }
        unsafe {
            *output = std::ptr::null_mut();
        }
        let host = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = host.0.lock().map_err(|_| "runtime lock poisoned")?;
        let previous = (state.provider_known == known).then_some(state.provider_revision).flatten();
        let snapshot = crate::engine::file_provider::app_snapshot(&state.engine, previous, &known)
            .map_err(|error| error.to_string())?;
        if let Some(account) = &snapshot.account {
            let bytes = serde_json::to_string(account).map_err(|error| error.to_string())?;
            if bytes.len() > crate::store::file_provider::PROVIDER_REPLY_BYTES {
                return Err("File Provider account state exceeds limit".into());
            }
            let bytes = CString::new(bytes).map_err(|_| "Invalid File Provider account state")?;
            unsafe {
                *output = bytes.into_raw();
            }
        }
        if snapshot.account.is_some() {
            state.provider_pending_revision = Some(snapshot.revision);
            state.provider_pending_known = Some(known);
        } else {
            state.provider_revision = Some(snapshot.revision);
        }
        Ok(())
    })
}

/// # Safety
/// Same handle rules as pb_ios_open. Acknowledge only after durable ledger
/// publication; a failed publication leaves the previous fence for retry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_provider_state_committed(handle: *mut IosEngine) -> *mut c_char {
    native_result(|| {
        let host = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = host.0.lock().map_err(|_| "runtime lock poisoned")?;
        state.provider_revision = Some(
            state
                .provider_pending_revision
                .take()
                .ok_or("No File Provider account state awaits publication")?,
        );
        state.provider_known = state
            .provider_pending_known
            .take()
            .ok_or("No File Provider locations await publication")?;
        Ok(())
    })
}

/// One detached transport job, with no engine handle, store Arc or profile lock.
/// Its caller admits at most one running job and one latest pending token/cut.
pub struct ProviderPushRegistration {
    // A Handle alone does not keep Tokio's timer driver alive across sign-out.
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
    connection: std::sync::Arc<crate::network::ap::ApConnection>,
    request: crate::network::ap::operation::ServiceRequest,
}

/// # Safety
/// Handle remains open for this preparation only. Text spans are readable and
/// copied, outputs writable. The returned job is consumed exactly once by run.
/// Empty accounts register normally so their first volume can trigger discovery.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_provider_push_prepare(
    handle: *mut IosEngine,
    account: *const c_char,
    token: *const u8,
    length: usize,
    invalidated: u8,
    bundle: *const c_char,
    sandbox: u8,
    output: *mut *mut ProviderPushRegistration,
) -> *mut c_char {
    native_result(|| {
        if account.is_null()
            || bundle.is_null()
            || output.is_null()
            || token.is_null()
            || length == 0
            || length > 128
            || invalidated > 1
            || sandbox > 1
        {
            return Err("Invalid File Provider push registration".into());
        }
        unsafe {
            *output = std::ptr::null_mut();
        }
        let account =
            unsafe { CStr::from_ptr(account) }.to_str().map_err(|_| "Invalid provider account")?;
        let account = crate::store::Id::parse(account).map_err(|_| "Invalid provider account")?;
        let bundle =
            unsafe { CStr::from_ptr(bundle) }.to_str().map_err(|_| "Invalid provider bundle")?;
        if bundle.len() > 128 {
            return Err("Invalid provider bundle".into());
        }
        let token = unsafe { std::slice::from_raw_parts(token, length) };
        let token = token.iter().map(|byte| format!("{byte:02x}")).collect();
        let host = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let state = host.0.lock().map_err(|_| "runtime lock poisoned")?;
        let engine = &state.engine;
        if engine.channel() != crate::release_channel::ReleaseChannel::Public
            || !engine.enrolled()
            || !engine.network.account_initialized()
            || engine.services.store.account().id != account
        {
            return Err("File Provider account is no longer signed in".into());
        }
        engine.services.store.check_account_authority().map_err(|error| error.to_string())?;
        let request = crate::engine::file_provider::push_request(
            &engine.services.store,
            token,
            invalidated != 0,
            bundle.into(),
            sandbox != 0,
        )
        .map_err(|error| error.to_string())?;
        let job = ProviderPushRegistration {
            runtime: engine.network.runtime.clone(),
            connection: engine
                .blob_transport()
                .provider_push_connection()
                .map_err(|error| error.to_string())?,
            request,
        };
        unsafe {
            *output = Box::into_raw(Box::new(job));
        }
        Ok(())
    })
}

/// # Safety
/// Job is owned, nonnull and not previously consumed. No engine handle is used.
/// Run on the dedicated registration lane; return is freed with string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_provider_push_run(
    job: *mut ProviderPushRegistration,
) -> *mut c_char {
    native_result(|| {
        if job.is_null() {
            return Err("Missing File Provider registration".into());
        }
        let job = unsafe { Box::from_raw(job) };
        job.runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                crate::network::ap::service_client::request(&job.connection, &job.request),
            )
            .await
            .map_err(|_| "File Provider push registration timed out".to_owned())?
            .map_err(|error| error.to_string())
        })?;
        Ok(())
    })
}

/// # Safety
/// Handle remains open for this local observation; no network I/O is performed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_provider_push_generation(handle: *mut IosEngine) -> u64 {
    let Some(host) = (unsafe { handle.as_ref() }) else { return 0 };
    let Ok(state) = host.0.lock() else { return 0 };
    if !state.engine.network.account_initialized()
        || state.engine.services.store.check_account_authority().is_err()
    {
        return 0;
    }
    state.engine.network.connection_status.live_generation().unwrap_or(0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert actual native owner and detached-job lifetime."
)]
mod tests {
    use super::*;
    use crate::network::ap::{ApConnection, ClientIdentity, testing};
    use std::{sync::Arc, time::Duration};

    #[test]
    fn detached_push_releases_profile_and_runs_safely_after_native_close() {
        // Two real detached jobs capture the native owner's network resources.
        // Close/rebind the profile while one AP response is held, then run the
        // other after close. No exclusive-store-only test covers this lifetime.
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        let opened = unsafe { pb_ios_open(path.as_ptr(), 0, &mut handle) };
        assert!(!handle.is_null());
        unsafe { pb_ios_reply_free(opened) };
        let ap = testing::silent_authenticating_server();
        let (first, second, connection, weak_runtime) = {
            let state = unsafe { &*handle }.0.lock().unwrap();
            let engine = &state.engine;
            let runtime = engine.network.runtime.clone();
            let mut identity = ClientIdentity::from_seed(
                "push@provider.invalid",
                "Test",
                engine.services.store.account_seed().unwrap(),
            )
            .unwrap();
            identity.auth_token = engine
                .services
                .store
                .account()
                .id
                .as_bytes()
                .iter()
                .copied()
                .chain([0; 56])
                .collect();
            identity.instance_id = crate::store::Id::from_bytes([1; 16]).to_string().into_bytes();
            identity.ca_certificate = vec![1];
            identity.instance_certificate = vec![1];
            let connection = Arc::new(
                runtime
                    .block_on(ApConnection::connect_authenticated(&ap.config, &mut identity))
                    .unwrap(),
            );
            let make = || {
                Box::into_raw(Box::new(ProviderPushRegistration {
                    runtime: runtime.clone(),
                    connection: connection.clone(),
                    request: crate::engine::file_provider::push_request(
                        &engine.services.store,
                        "08".repeat(32),
                        true,
                        "playbit.engine".into(),
                        false,
                    )
                    .unwrap(),
                }))
            };
            (make() as usize, make() as usize, connection, Arc::downgrade(&runtime))
        };
        let running = std::thread::spawn(move || {
            let error = unsafe { pb_ios_provider_push_run(first as *mut ProviderPushRegistration) };
            assert!(!error.is_null());
            unsafe { pb_ios_string_free(error) };
        });
        ap.request_held.recv_timeout(Duration::from_secs(5)).unwrap();
        unsafe { pb_ios_close(handle) };
        let mut reopened = crate::store::LocalStore::open(directory.path()).unwrap();
        reopened.bind_authenticated_account(crate::store::Id::from_bytes([42; 16])).unwrap();
        drop(reopened);
        assert!(weak_runtime.upgrade().is_some(), "prepared job keeps the timer driver alive");
        assert!(ap.closed.try_recv().is_err(), "native close did not wait for AP acknowledgment");
        connection.shutdown();
        running.join().unwrap();
        let error = unsafe { pb_ios_provider_push_run(second as *mut ProviderPushRegistration) };
        assert!(!error.is_null());
        unsafe { pb_ios_string_free(error) };
        assert!(weak_runtime.upgrade().is_none(), "both consumed jobs release the network runtime");
    }
}
