//! Named C control entry points for the in-process host.
use super::{IosEngine, IosReply, LocalRequest, Request, owner_response, submit, submit_local};

unsafe fn text(bytes: *const u8, len: usize) -> Result<String, String> {
    if len > 4 * 1024 * 1024 || (len != 0 && bytes.is_null()) {
        return Err("invalid control text span".into());
    }
    let bytes = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(bytes, len) } };
    Ok(std::str::from_utf8(bytes).map_err(|_| "invalid control UTF-8")?.into())
}

unsafe fn identifier(bytes: *const u8, len: usize) -> Result<crate::store::Id, String> {
    if !(16..=22).contains(&len) {
        return Err("invalid identifier length".into());
    }
    crate::store::Id::parse(&unsafe { text(bytes, len)? }).map_err(|error| error.to_string())
}
/// # Safety
/// Handle must remain open; a nonempty session span must be readable and is copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_prepare_session(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PrepareSession { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_status(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::Status)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_prepare_sign_in(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PrepareSignIn)
    })
}

/// # Safety
/// Handle must remain open; the account span must be readable for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_accept_agreement(
    handle: *mut IosEngine,
    id: u64,
    account: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(
            handle,
            id,
            Request::AcceptAgreement {
                account: unsafe { identifier(account, len)? },
                digest: crate::network::ap::registration::terms_digest(),
            },
        )
    })
}

/// Build capability only; this does not access Keychain.
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_saved_account_available() -> u8 {
    #[cfg(target_vendor = "apple")]
    {
        u8::from(crate::credentials::available())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        0
    }
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_saved_account(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::SavedAccount)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_retry_account_key_backup(
    handle: *mut IosEngine,
    id: u64,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::RetryAccountKeyBackup)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_transfer_status(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::TransferStatus)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_cancel_transfer(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::CancelTransfer)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_recovery_status(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::RecoveryStatus)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_prepare_recovery(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PrepareRecovery)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_save_recovery(handle: *mut IosEngine, id: u64) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::SaveRecovery)
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_app_frame(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit_local(
            handle,
            id,
            LocalRequest::Frame { instance: unsafe { identifier(bytes, len)? } },
        )
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_open_app(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit_local(handle, id, LocalRequest::Open { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_quit_app(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit_local(
            handle,
            id,
            LocalRequest::Quit { instance: unsafe { identifier(bytes, len)? } },
        )
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_check_updates(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::CheckUpdates { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_personal_volumes(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PersonalVolumes { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_invitation(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::Invitation { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_forget_session(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::ForgetSession { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_watch_invitation(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::WatchInvitation { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_resume_invitation(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::ResumeInvitation { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_pause_invitation(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PauseInvitation { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_start_join(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::StartJoin { session: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_preview_invitation(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::PreviewInvitation { invitation: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_set_display_name(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::SetDisplayName { name: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_sign_in(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::SignIn { code: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_request_code(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::RequestCode { email: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open for the call; no concurrent close is allowed.
/// A non-null text span must be readable for its length. It is copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_recover_credentials(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit(handle, id, Request::RecoverCredentials { secret: unsafe { text(bytes, len)? } })
    })
}

/// # Safety
/// Handle must remain open; a nonempty span must be readable for its length.
/// Input is copied before return and no pointers are retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_app_logs(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        submit_local(
            handle,
            id,
            LocalRequest::Logs { instance: unsafe { identifier(bytes, len)? }, after: None },
        )
    })
}

/// # Safety
/// Handle must remain open; a nonempty cursor span must be readable for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_list_apps(
    handle: *mut IosEngine,
    id: u64,
    bytes: *const u8,
    len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if len > crate::package::MAX_APP_ID_BYTES {
            return Err("app cursor exceeds its limit".into());
        }
        let after = if len == 0 {
            None
        } else {
            let value = unsafe { text(bytes, len)? };
            if !crate::package::valid_app_id(&value) {
                return Err("invalid app cursor".into());
            }
            Some(value)
        };
        submit(handle, id, Request::ListApps { after })
    })
}

unsafe fn app_identity(
    publisher: *const u8,
    publisher_len: usize,
    app_id: *const u8,
    app_id_len: usize,
) -> Result<crate::package::AppIdentity, String> {
    if !(16..=22).contains(&publisher_len)
        || !(1..=crate::package::MAX_APP_ID_BYTES).contains(&app_id_len)
    {
        return Err("invalid published app identity span".into());
    }
    let identity = crate::package::AppIdentity {
        publisher: crate::store::Id::parse(&unsafe { text(publisher, publisher_len)? })
            .map_err(|error| error.to_string())?,
        app_id: unsafe { text(app_id, app_id_len)? },
    };
    identity.validate().map_err(str::to_owned)?;
    Ok(identity)
}

/// # Safety
/// Handle and nonempty identity spans must remain valid for the call. Inputs are copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_create_app_session(
    handle: *mut IosEngine,
    id: u64,
    publisher: *const u8,
    publisher_len: usize,
    app_id: *const u8,
    app_id_len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let app = unsafe { app_identity(publisher, publisher_len, app_id, app_id_len)? };
        submit(handle, id, Request::CreateAppSession { app, root: None, user: None })
    })
}

/// # Safety
/// Handle and nonempty spans must remain valid for the call. Inputs are copied.
#[allow(
    clippy::too_many_arguments,
    reason = "The C ABI passes publisher, app ID and build as separately bounded text spans."
)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_get_app_display(
    handle: *mut IosEngine,
    id: u64,
    publisher: *const u8,
    publisher_len: usize,
    app_id: *const u8,
    app_id_len: usize,
    build: *const u8,
    build_len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if build_len > 44 {
            return Err("published build identity exceeds its limit".into());
        }
        let app = unsafe { app_identity(publisher, publisher_len, app_id, app_id_len)? };
        let build = unsafe { text(build, build_len)? };
        if !crate::identifier::valid(&build, 32) {
            return Err("invalid published build identity".into());
        }
        submit(handle, id, Request::GetAppDisplay { app, build })
    })
}

/// # Safety
/// Handle must remain open; nonempty spans must be readable for their lengths.
/// Input is copied before return and no pointers are retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_sign_in_with_key(
    handle: *mut IosEngine,
    id: u64,
    first: *const u8,
    first_len: usize,
    second: *const u8,
    second_len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if first_len.checked_add(second_len).is_none_or(|n| n > 4 * 1024 * 1024) {
            return Err("control text exceeds 4 MiB".into());
        }
        let first = unsafe { text(first, first_len)? };
        let second = unsafe { text(second, second_len)? };
        submit(handle, id, Request::SignInWithKey { email: first, account_key: second })
    })
}

/// # Safety
/// Handle must remain open. For 1..=256 entries, sessions points to count * 32
/// readable bytes, one raw 32-byte session ID per entry. Nothing is retained after copying.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_order_sessions(
    handle: *mut IosEngine,
    id: u64,
    sessions: *const u8,
    count: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if count > 256 || (count != 0 && sessions.is_null()) {
            return Err("invalid session order span".into());
        }
        let bytes = if count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(sessions, count * 32) }
        };
        let mut sessions = Vec::with_capacity(count);
        for bytes in bytes.as_chunks::<32>().0 {
            sessions.push(crate::identifier::encode(bytes));
        }
        submit(handle, id, Request::OrderSessions { sessions })
    })
}

/// # Safety
/// Handle must remain open. Session is readable for its length. For 1..=128
/// grants, volumes points to count * 16 readable ID bytes and writes to count
/// readable flags (0 or 1). Inputs are copied and no pointers are retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_select_personal_volumes(
    handle: *mut IosEngine,
    id: u64,
    session: *const u8,
    session_len: usize,
    volumes: *const u8,
    writes: *const u8,
    count: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if session_len > crate::identifier::MAX_TEXT
            || count > 128
            || (count != 0 && (volumes.is_null() || writes.is_null()))
        {
            return Err("invalid personal-volume spans".into());
        }
        let session = unsafe { text(session, session_len)? };
        let (volumes, writes) = if count == 0 {
            (&[][..], &[][..])
        } else {
            unsafe {
                (
                    std::slice::from_raw_parts(volumes, count * 16),
                    std::slice::from_raw_parts(writes, count),
                )
            }
        };
        let mut grants = Vec::with_capacity(count);
        for (volume, write) in volumes.as_chunks::<16>().0.iter().zip(writes) {
            let volume = crate::store::Id::from_bytes(*volume);
            let write = match write {
                0 => false,
                1 => true,
                _ => return Err("invalid write flag".into()),
            };
            grants.push(crate::store::PersonalVolumeGrant { volume, write });
        }
        submit(handle, id, Request::SelectPersonalVolumes { session, grants })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests check bounded C span conversion.")]
mod tests {
    use super::*;

    #[test]
    fn text_spans_preserve_utf8_and_nul_and_reject_invalid_bounds() {
        let value = "λ\0hello";
        assert_eq!(unsafe { text(value.as_ptr(), value.len()) }.unwrap(), value);
        assert_eq!(unsafe { text(std::ptr::null(), 0) }.unwrap(), "");
        assert!(unsafe { text(std::ptr::null(), 1) }.is_err());
        assert!(unsafe { text(std::ptr::null(), 4 * 1024 * 1024 + 1) }.is_err());
        assert!(unsafe { text([0xff].as_ptr(), 1) }.is_err());
    }
}
