//! UIKit's small in-process Engine bridge. Calls are serialized by the host worker.
#![allow(
    unsafe_code,
    reason = "This module owns the C bridge to Swift and documents pointer ownership at each call."
)]
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
mod audio;
#[cfg(feature = "phase1-qualification")]
mod keychain_qualification;
use crate::engine::{Engine, Request};
use crate::runtime::{ViewNode, decode_view};
fn decode_scene(bytes: &[u8]) -> Result<ViewNode, String> {
    let scene = decode_view(bytes)
        .map_err(|_| "The app scene is invalid. The previous view is retained.")?;
    scene::storage(&scene)?;
    Ok(scene)
}

/// Exercise the current scene adoption boundary with a zero-width canvas.
/// Only the simulator qualification build exposes this probe; no JSON parser
/// or second production decoder participates in native scene adoption.
#[cfg(feature = "phase1-qualification")]
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_qualification_malformed_scene() -> u8 {
    use playbit_api::ui::{VIEW_CANVAS, VIEW_COLUMN_END, VIEW_COLUMN_START, VIEW_MAGIC};
    let payload: Vec<u8> = [0u32, 8, 0, 0].into_iter().flat_map(u32::to_le_bytes).collect();
    let mut bytes = VIEW_MAGIC.to_vec();
    for (kind, id, payload) in [
        (VIEW_COLUMN_START, 0u32, &[][..]),
        (VIEW_CANVAS, 1, payload.as_slice()),
        (VIEW_COLUMN_END, 0, &[][..]),
    ] {
        bytes.push(kind);
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
    }
    u8::from(decode_scene(&bytes).is_ok())
}

// Deliberate host failure, only in the existing simulator qualification build.
#[cfg(feature = "phase1-qualification")]
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_qualification_panic() {
    panic!("W7 injected iOS engine panic");
}

use crate::ui::embedded::{EmbeddedOwner, Reply as LocalReply, Request as LocalRequest};
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    ffi::{CStr, CString, c_char},
    sync::Mutex,
};

pub struct IosEngine(Mutex<IosState>);
mod provider;
pub use provider::{
    pb_ios_provider_push_generation, pb_ios_provider_push_prepare, pb_ios_provider_push_run,
    pb_ios_provider_state, pb_ios_provider_state_committed,
};
mod artwork;
mod database;
mod developer;
mod sign_in;
pub use developer::{pb_ios_reply_app, pb_ios_reply_developer};
pub use sign_in::pb_ios_reply_sign_in;
mod control_reply;
mod controls;
pub use control_reply::pb_ios_reply_control;
pub use controls::*;
mod invitation;
mod logs;
mod status;
pub use status::{
    pb_ios_reply_status, pb_ios_status_example, pb_ios_status_invitation, pb_ios_status_session,
    pb_ios_status_volume, pb_ios_status_watch,
};
mod personal_volumes;
pub use invitation::pb_ios_reply_invitation;
pub use personal_volumes::{
    pb_ios_reply_personal_grant, pb_ios_reply_personal_volume, pb_ios_reply_personal_volumes,
};
mod presentation;
mod transfer;
pub use presentation::pb_ios_reply_presentation;
pub use transfer::pb_ios_reply_transfer;
mod recovery;
pub use logs::{pb_ios_reply_log_line, pb_ios_reply_logs};
mod scene;
pub use recovery::pb_ios_reply_recovery;
mod timing;
pub use scene::{pb_ios_scene_canvas_item, pb_ios_scene_child, pb_ios_scene_node};

#[derive(Default)]
struct ReplyData {
    database: Option<Vec<u8>>,
    request_id: Option<u64>,
    error: Option<String>,
    background_error: Option<String>,
    schedule: Option<(crate::engine::ChangeToken, Option<u64>)>,
    scene: Option<ViewNode>,
    recovery: Option<recovery::Payload>,
    logs: Option<crate::engine::LogBatch>,
    presentation: Option<presentation::Metadata>,
    transfer: Option<crate::engine::reply::TransferStatus>,
    control: Option<control_reply::ControlReply>,
    invitation: Option<invitation::Payload>,
    personal_volumes: Option<personal_volumes::Payload>,
    status: Option<status::Payload>,
    sign_in: Option<sign_in::Payload>,
    developer: Option<developer::Payload>,
}
pub struct IosReply {
    database: Option<Vec<u8>>,
    request_id: Option<u64>,
    error: Option<CString>,
    error_code: u32,
    background_error: Option<CString>,
    schedule: Option<(crate::engine::ChangeToken, Option<u64>)>,
    scene: Option<ViewNode>,
    recovery: Option<recovery::Payload>,
    logs: Option<crate::engine::LogBatch>,
    presentation: Option<presentation::Metadata>,
    transfer: Option<crate::engine::reply::TransferStatus>,
    control: Option<control_reply::ControlReply>,
    invitation: Option<invitation::Payload>,
    personal_volumes: Option<personal_volumes::Payload>,
    status: Option<status::Payload>,
    sign_in: Option<sign_in::Payload>,
    developer: Option<developer::Payload>,
}
struct IosState {
    // Diagnostic teardown retains its own ownership and never delays the engine.
    _recorder: Option<crate::trace::Recorder>,
    engine: Engine,
    provider_revision: Option<(crate::store::Id, u64)>,
    provider_pending_revision: Option<(crate::store::Id, u64)>,
    provider_known: Vec<crate::store::Id>,
    provider_pending_known: Option<Vec<crate::store::Id>>,
    owner: EmbeddedOwner,
    active: bool,
    delivery_ready: bool,
    // Fields drop in declaration order. Retain admission through engine teardown.
    _profile: crate::engine::profile::Selection,
}
impl IosState {
    fn advance(&mut self, completion: bool) -> ReplyData {
        let step =
            self.owner.advance(&mut self.engine, completion, self.active, self.delivery_ready);
        if step.response.is_some() {
            self.delivery_ready = false;
        }
        let mut retained_scene = None;
        let mut retained_database = None;
        let mut retained_recovery = None;
        let mut retained_logs = None;
        let mut retained_presentation = None;
        let mut retained_transfer = None;
        let mut retained_control = None;
        let mut retained_invitation = None;
        let mut retained_personal_volumes = None;
        let mut retained_status = None;
        let mut retained_sign_in = None;
        let mut retained_developer = None;
        let result = step.response.map(|(id, result)| {
            (
                id,
                result.and_then(|reply| {
                    use crate::engine::reply::Reply;
                    use control_reply::ControlReply;
                    let reply = match reply {
                        LocalReply::Database(bytes) => {
                            retained_database = Some(bytes);
                            return Ok(());
                        }
                        LocalReply::Management(reply) => reply,
                        LocalReply::Opened { instance, generation } => {
                            retained_control = Some(ControlReply::Opened {
                                instance: instance.to_string(),
                                generation,
                            });
                            return Ok(());
                        }
                        LocalReply::Frame(value) => {
                            if !value.view.is_empty() {
                                let scene = decode_scene(&value.view)?;
                                scene::storage(&scene)?;
                                retained_scene = Some(scene);
                            }
                            retained_presentation = Some(presentation::Metadata {
                                instance: value.instance,
                                generation: value.generation,
                                build: value.session.resolved_build_id,
                                invitation: value.invitation,
                                availability: value.availability,
                                update: value.update,
                            });
                            return Ok(());
                        }
                        LocalReply::Logs(value) => {
                            retained_logs = Some(value);
                            return Ok(());
                        }
                        LocalReply::Quit { instance, drain_failed } => {
                            retained_control = Some(ControlReply::Quit {
                                instance: instance.to_string(),
                                drain_failed,
                            });
                            return Ok(());
                        }
                    };
                    match reply {
                        Reply::SignedIn(value) => {
                            retained_sign_in = Some(sign_in::Payload::new(value));
                            Ok(())
                        }
                        Reply::SessionPrepared(value) => {
                            retained_control = Some(ControlReply::Prepared(value.session.id));
                            Ok(())
                        }
                        Reply::Session(value) => {
                            retained_control = Some(ControlReply::Session(value.id));
                            Ok(())
                        }
                        Reply::Developer(value) => {
                            use crate::engine::reply::DeveloperReply;
                            match value {
                                DeveloperReply::Session(value) => {
                                    retained_control = Some(ControlReply::Session(value.id))
                                }
                                DeveloperReply::Apps(value) => {
                                    retained_developer = Some(developer::Payload::page(value))
                                }
                                DeveloperReply::Display { build_id, name, .. } => {
                                    retained_developer =
                                        Some(developer::Payload::Display { build: build_id, name })
                                }
                                _ => return Err("unsupported iOS developer reply".into()),
                            }
                            Ok(())
                        }
                        Reply::Status(value) => {
                            retained_status = Some(status::Payload::new(*value));
                            Ok(())
                        }
                        Reply::PersonalVolumesSelected { personal_volumes_selected } => {
                            retained_control = Some(ControlReply::PersonalVolumesSelected(
                                personal_volumes_selected,
                            ));
                            Ok(())
                        }
                        Reply::AgreementAccepted => {
                            retained_control = Some(ControlReply::AgreementAccepted);
                            Ok(())
                        }
                        Reply::DisplayName { display_name } => {
                            retained_control = Some(ControlReply::DisplayName(display_name));
                            Ok(())
                        }
                        Reply::AccountKeyBackup { account_key_backup } => {
                            retained_control =
                                Some(ControlReply::AccountKeyBackup(account_key_backup));
                            Ok(())
                        }
                        Reply::SavedAccount { saved_account } => {
                            retained_control = Some(ControlReply::SavedAccount(saved_account));
                            Ok(())
                        }
                        Reply::SignInAvailable { sign_in_available } => {
                            retained_control =
                                Some(ControlReply::SignInAvailable(sign_in_available));
                            Ok(())
                        }
                        Reply::CodeSent { code_sent } => {
                            retained_control = Some(ControlReply::CodeSent(code_sent));
                            Ok(())
                        }
                        Reply::OrderSessions { ordered } => {
                            retained_control = Some(ControlReply::Ordered(ordered));
                            Ok(())
                        }
                        Reply::ForgetSession { forgotten } => {
                            retained_control = Some(ControlReply::Forgotten(forgotten));
                            Ok(())
                        }
                        Reply::InvitationPreview(value) => {
                            retained_invitation = Some(invitation::Payload::Preview {
                                session: value.session,
                                paused: value.paused,
                                retained: value.retained,
                                root_name: value.received.map(|received| received.root_name),
                            });
                            Ok(())
                        }
                        Reply::InvitationStatus { invitation_status } => {
                            retained_invitation = Some(invitation::Payload::Status {
                                paused: None,
                                status: invitation_status,
                            });
                            Ok(())
                        }
                        Reply::InvitationPaused { paused, invitation_status } => {
                            retained_invitation = Some(invitation::Payload::Status {
                                paused: Some(paused),
                                status: invitation_status,
                            });
                            Ok(())
                        }
                        Reply::Invitation { invitation, availability } => {
                            retained_invitation =
                                Some(invitation::Payload::Link { invitation, availability });
                            Ok(())
                        }
                        Reply::PersonalVolumes { session: version, volumes, grants } => {
                            retained_personal_volumes = Some(personal_volumes::Payload {
                                session: version,
                                volumes,
                                grants,
                            });
                            Ok(())
                        }
                        Reply::Transfer { transfer } => {
                            retained_transfer = Some(transfer);
                            Ok(())
                        }
                        Reply::Recovery(value) => {
                            retained_recovery = Some(recovery::Payload::Status(value));
                            Ok(())
                        }
                        Reply::PreparedRecovery(value) => {
                            retained_recovery = Some(recovery::Payload::Prepared(value));
                            Ok(())
                        }
                        Reply::Error { error } => Err(error.into()),
                        _ => Err("unsupported iOS runtime reply".into()),
                    }
                }),
            )
        });
        let (request_id, error) = match result {
            Some((id, Ok(()))) => (Some(id), None),
            Some((id, Err(error))) => (Some(id), Some(error.to_string())),
            None => (None, None),
        };
        ReplyData {
            database: retained_database,
            request_id,
            error,
            background_error: step.error,
            schedule: Some((
                step.changes,
                step.wake_after.map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64),
            )),
            scene: retained_scene,
            recovery: retained_recovery,
            logs: retained_logs,
            presentation: retained_presentation,
            transfer: retained_transfer,
            control: retained_control,
            invitation: retained_invitation,
            personal_volumes: retained_personal_volumes,
            status: retained_status,
            sign_in: retained_sign_in,
            developer: retained_developer,
        }
    }
}
fn owner_response(action: impl FnOnce() -> Result<ReplyData, String>) -> *mut IosReply {
    let mut data =
        action().unwrap_or_else(|error| ReplyData { error: Some(error), ..ReplyData::default() });
    let error_code = if data.error.as_deref() == Some("corrupt_store") {
        data.error = Some("The local store is corrupt.".into());
        2
    } else {
        1
    };
    let mut error = data.error.map(bounded_error);
    let background_error = data.background_error.map(bounded_error);
    let retained = std::mem::size_of::<IosReply>()
        .saturating_add(data.database.as_ref().map_or(0, Vec::capacity))
        .saturating_add(
            data.scene.as_ref().map_or(0, |scene| scene::storage(scene).expect("validated scene")),
        )
        .saturating_add(data.recovery.as_ref().map_or(0, recovery::Payload::storage))
        .saturating_add(data.logs.as_ref().map_or(0, logs::storage))
        .saturating_add(data.presentation.as_ref().map_or(0, presentation::Metadata::storage))
        .saturating_add(data.transfer.as_ref().map_or(0, transfer::storage))
        .saturating_add(data.control.as_ref().map_or(0, control_reply::ControlReply::storage))
        .saturating_add(data.invitation.as_ref().map_or(0, invitation::Payload::storage))
        .saturating_add(
            data.personal_volumes.as_ref().map_or(0, personal_volumes::Payload::storage),
        )
        .saturating_add(data.status.as_ref().map_or(0, status::Payload::storage))
        .saturating_add(data.sign_in.as_ref().map_or(0, sign_in::Payload::storage))
        .saturating_add(data.developer.as_ref().map_or(0, developer::Payload::storage));
    let error_bytes = error.as_ref().map_or(0, |text| text.as_bytes_with_nul().len())
        + background_error.as_ref().map_or(0, |text| text.as_bytes_with_nul().len());
    if MAX_RESPONSE_BYTES.checked_sub(retained.saturating_add(error_bytes)).is_none() {
        // Preserve native correlation and scheduling even if payload delivery fails.
        data.scene = None;
        data.database = None;
        data.recovery = None;
        data.logs = None;
        data.presentation = None;
        data.transfer = None;
        data.control = None;
        data.invitation = None;
        data.personal_volumes = None;
        data.status = None;
        data.sign_in = None;
        data.developer = None;
        error = Some(bounded_error(
            "The response exceeds 4 MiB. The action may have completed; \
             do not repeat it automatically."
                .into(),
        ));
    }
    Box::into_raw(Box::new(IosReply {
        database: data.database,
        request_id: data.request_id,
        error,
        error_code,
        background_error,
        scene: data.scene,
        recovery: data.recovery,
        logs: data.logs,
        presentation: data.presentation,
        transfer: data.transfer,
        control: data.control,
        invitation: data.invitation,
        personal_volumes: data.personal_volumes,
        status: data.status,
        sign_in: data.sign_in,
        developer: data.developer,
        schedule: data.schedule,
    }))
}
/// Correlation and errors borrow their strings until the reply is freed.
/// An error without a response ID is a bridge failure; with an ID it completes that request.
#[repr(C)]
pub struct IosCompletion {
    request_id: u64,
    has_response: u8,
    error_code: u32,
    error: *const c_char,
    background_error: *const c_char,
}
/// # Safety
/// Reply and borrowed strings must remain live until the caller copies them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_completion(reply: *const IosReply) -> IosCompletion {
    let Some(reply) = (unsafe { reply.as_ref() }) else {
        return IosCompletion {
            request_id: 0,
            has_response: 0,
            error_code: 1,
            error: c"missing reply".as_ptr(),
            background_error: std::ptr::null(),
        };
    };
    IosCompletion {
        request_id: reply.request_id.unwrap_or(0),
        has_response: u8::from(reply.request_id.is_some()),
        error_code: if reply.error.is_some() { reply.error_code } else { 0 },
        error: reply.error.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
        background_error: reply
            .background_error
            .as_ref()
            .map_or(std::ptr::null(), |text| text.as_ptr()),
    }
}
/// Fixed owner scheduling fields. Generation borrows 16 bytes from the reply;
/// null means this reply is not an owner step. A zero deadline is an immediate wake.
#[repr(C)]
pub struct IosSchedule {
    generation: *const u8,
    sequence: u64,
    wake_after_ns: u64,
    has_wake: u8,
}
/// # Safety
/// Reply and its borrowed generation must remain live until the caller copies it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_schedule(reply: *const IosReply) -> IosSchedule {
    match unsafe { reply.as_ref() }.and_then(|reply| reply.schedule.as_ref()) {
        Some((changes, wake)) => IosSchedule {
            generation: changes.generation.as_bytes().as_ptr(),
            sequence: changes.sequence,
            wake_after_ns: wake.unwrap_or(0),
            has_wake: u8::from(wake.is_some()),
        },
        None => {
            IosSchedule { generation: std::ptr::null(), sequence: 0, wake_after_ns: 0, has_wake: 0 }
        }
    }
}
/// # Safety
/// Returned scene and all descendants are borrowed until pb_ios_reply_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_scene(reply: *const IosReply) -> *const ViewNode {
    unsafe { reply.as_ref() }
        .and_then(|reply| reply.scene.as_ref())
        .map_or(std::ptr::null(), |scene| scene)
}
/// # Safety
/// Free an owned reply once, after all reads of its borrowed fields finish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reply_free(reply: *mut IosReply) {
    if !reply.is_null() {
        drop(unsafe { Box::from_raw(reply) });
    }
}

// Native lifecycle calls return null on success or one bounded, owned error.
fn native_result(action: impl FnOnce() -> Result<(), String>) -> *mut c_char {
    match action() {
        Ok(()) => std::ptr::null_mut(),
        Err(error) => bounded_error(error).into_raw(),
    }
}
fn bounded_error(error: String) -> CString {
    let mut text = error.replace('\0', "�");
    let mut end = text.len().min(508);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    CString::new(text).expect("error contains no NUL")
}

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
unsafe fn string<'a>(value: *const c_char) -> Result<&'a str, String> {
    if value.is_null() {
        return Err("missing string argument".into());
    }
    unsafe { CStr::from_ptr(value) }.to_str().map_err(|e| e.to_string())
}
/// # Safety
/// `profile` is a NUL-terminated UTF-8 absolute path; `output` points to writable handle storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_open(
    profile: *const c_char,
    channel: u8,
    output: *mut *mut IosEngine,
) -> *mut IosReply {
    // This entry point runs on EngineHost's worker. UIKit probes never initialize
    // the opt-in recorder or create its file on the main thread.
    if let Err(error) = crate::trace::initialize(5, &[]) {
        crate::diagnostic!("Playbit tracing unavailable: {error}");
    }
    let _timing = crate::trace::Span::new("ios.open");
    owner_response(|| {
        if output.is_null() {
            return Err("missing output handle".into());
        }
        unsafe {
            *output = std::ptr::null_mut();
        }
        let profile = unsafe { string(profile)? };
        if !std::path::Path::new(profile).is_absolute() {
            return Err("profile must be an absolute path".into());
        }
        let channel = match channel {
            0 => crate::release_channel::ReleaseChannel::Public,
            1 => crate::release_channel::ReleaseChannel::Testing,
            _ => return Err("Invalid runtime channel".into()),
        };
        let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
        let selection = crate::engine::profile::Selection::open(
            &home,
            Some(std::path::Path::new(profile)),
            Some(channel),
        )
        .map_err(|error| error.to_string())?;
        // Rebind (or explicitly disable) process tracing before opening any
        // account data. The previous collector may still be draining diagnostics.
        let recorder = crate::trace::Recorder::for_profile(
            std::path::Path::new(profile),
            &std::path::Path::new(profile).join(crate::paths::TRACE),
        );
        #[cfg(feature = "phase1-qualification")]
        let keychain_fixture = keychain_qualification::prepare(std::path::Path::new(profile))?;
        // Unit tests share one harness process with intentional assertion tests.
        // The installed hook is exercised in a child and in the packaged host.
        #[cfg(not(test))]
        crate::platform::failure::startup(std::path::Path::new(profile));
        #[cfg(not(test))]
        crate::platform::failure::install_native(std::path::Path::new(profile));
        #[allow(
            unused_mut,
            reason = "Qualification configures the test transport before opening the host."
        )]
        let mut engine = Engine::open(profile).map_err(|error| {
            if crate::engine::is_corrupt_store_open_error(error.as_ref()) {
                "corrupt_store".to_owned()
            } else {
                error.to_string()
            }
        })?;
        #[cfg(feature = "phase1-qualification")]
        {
            if keychain_fixture {
                let _offline = qualification_offline_transport(&mut engine)?;
            }
            // Explicit isolated loopback/LAN AP fixture, absent from product builds.
            // The packaged tests still exercise the real protocol and service.
            let path = std::path::Path::new(profile).join("qualification-ap.json");
            if path.exists() {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .map_err(|e| e.to_string())?
                    .take(32769)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 32768 {
                    return Err("qualification AP configuration is too large".into());
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                let address: std::net::SocketAddr = value["address"]
                    .as_str()
                    .ok_or("missing qualification address")?
                    .parse()
                    .map_err(|_| "invalid qualification address")?;
                let local = address.ip().is_loopback()
                    || matches!(address.ip(), std::net::IpAddr::V4(ip) if ip.is_private());
                if !local {
                    return Err("qualification AP must be loopback or private LAN".into());
                }
                let certificate: Vec<u8> = serde_json::from_value(value["certificate"].clone())
                    .map_err(|e| e.to_string())?;
                engine
                    .remote
                    .as_mut()
                    .ok_or("qualification AP requires idle network")?
                    .set_test_transport(crate::network::ap::ClientConfig::new(
                        address,
                        "localhost",
                        vec![certificate.into()],
                    ));
            }
        }
        unsafe {
            *output = Box::into_raw(Box::new(IosEngine(Mutex::new(IosState {
                _profile: selection,
                _recorder: recorder,
                engine,
                provider_revision: None,
                provider_pending_revision: None,
                provider_known: Vec::new(),
                provider_pending_known: None,
                owner: EmbeddedOwner::new(),
                active: true,
                delivery_ready: true,
            }))));
        }
        Ok(ReplyData::default())
    })
}
fn submit(handle: &IosEngine, id: u64, request: Request) -> Result<ReplyData, String> {
    submit_local(handle, id, LocalRequest::Management(request))
}
fn submit_local(handle: &IosEngine, id: u64, request: LocalRequest) -> Result<ReplyData, String> {
    let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
    if !state.active {
        return Err("Playbit is paused. Return to the app to continue.".into());
    }
    state.owner.submit(id, request).map_err(|e| e.to_string())?;
    Ok(state.advance(false))
}

/// Copy a guest input into the bounded owner queue without JSON encoding.
/// # Safety
/// Handle must remain open during the call. Nonempty byte spans must be readable
/// for their supplied lengths, or null (which is rejected for nonempty spans).
/// The caller retains ownership; no pointer is retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_app_input(
    handle: *mut IosEngine,
    id: u64,
    instance: *const u8,
    instance_len: usize,
    kind: u32,
    target: u32,
    data: *const u8,
    data_len: usize,
) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        if !(16..=22).contains(&instance_len)
            || instance.is_null()
            || data_len > playbit_api::abi::syscall::MAX_EVENT_BYTES
            || (data_len != 0 && data.is_null())
        {
            return Err("invalid input byte spans".into());
        }
        let instance = unsafe { std::slice::from_raw_parts(instance, instance_len) };
        let instance = std::str::from_utf8(instance).map_err(|_| "invalid instance UTF-8")?;
        let data = if data_len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(data, data_len) }
        };
        let instance = crate::store::Id::parse(instance).map_err(|error| error.to_string())?;
        submit_local(
            handle,
            id,
            LocalRequest::Input { instance, kind, target, data: data.to_vec() },
        )
    })
}

/// Duplicate the notification descriptor for a DispatchSourceRead on the same
/// serialized owner as pb_ios_open. The caller closes it after source cancel.
/// # Safety
/// Handle must be open and no close may run concurrently. `output` must point
/// to writable i32 storage. Success returns null and transfers one descriptor;
/// failure writes -1 and returns an error freed with pb_ios_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_completion_descriptor(
    handle: *mut IosEngine,
    output: *mut i32,
) -> *mut c_char {
    use std::os::fd::IntoRawFd;
    native_result(|| {
        let output = unsafe { output.as_mut() }.ok_or("missing descriptor output")?;
        *output = -1;
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        *output = state
            .engine
            .completion_socket()
            .expect("Unix engine completion")
            .try_clone()
            .map_err(|e| e.to_string())?
            .into_raw_fd();
        Ok(())
    })
}
/// Apply a signaled completion on the serialized runtime owner.
/// # Safety
/// Same open-handle rules as pb_ios_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_complete_background(handle: *mut IosEngine) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        Ok(state.advance(true))
    })
}

/// Pause unfinished transfers before iOS suspends the process. The transfer
/// owns Remote while running, so ordinary Disconnect cannot interrupt it.
/// # Safety
/// Same open-handle rules as `pb_ios_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_suspend(handle: *mut IosEngine) -> *mut c_char {
    crate::trace::mark("ios.suspending");
    native_result(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        state.active = false;
        let crate::engine::reply::Reply::Transfer { transfer } =
            state.engine.dispatch_reply(Request::CancelTransfer).map_err(|e| e.to_string())?
        else {
            return Err("unexpected cancel-transfer reply".into());
        };
        if transfer.kind != crate::engine::reply::TransferKind::Canceling {
            state.engine.dispatch_reply(Request::Disconnect).map_err(|e| e.to_string())?;
        }
        // A canceled worker disconnects when its completion is collected. Its
        // invitation stays paused until the participant explicitly resumes it.
        Ok(())
    })
}

/// Advance after an explicitly scheduled guest or reconnect deadline.
/// # Safety
/// Same open-handle rules as `pb_ios_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_wake(handle: *mut IosEngine) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        Ok(state.advance(false))
    })
}

/// Release the single response-delivery slot after its main-thread callback.
/// Runtime completion/deadline handling continues while delivery is blocked.
/// # Safety
/// Same open-handle rules as `pb_ios_open`. Acknowledge each response once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_delivery_ready(handle: *mut IosEngine) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        if state.delivery_ready {
            return Err("no iOS response awaits delivery".into());
        }
        state.delivery_ready = true;
        Ok(state.advance(false))
    })
}

/// Resume account replication once per process foreground transition.
/// # Safety
/// Same open-handle rules as `pb_ios_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_resume(handle: *mut IosEngine) -> *mut IosReply {
    owner_response(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        if !state.active {
            state.active = true;
            let IosState { owner, engine, .. } = &mut *state;
            owner.resume_network(engine);
        }
        Ok(state.advance(false))
    })
}
/// # Safety
/// Same open-handle rules as `pb_ios_open`; call on the host's runtime worker.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_stop_for_sign_out(handle: *mut IosEngine) -> *mut c_char {
    native_result(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let IosState { owner, engine, .. } = &mut *state;
        owner.drain_apps(engine, deadline).map_err(|e| e.to_string())?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match engine.shutdown(remaining).map_err(|e| e.to_string())? {
            crate::engine::Shutdown::Done => Ok(()),
            _ => Err("Sign-in is still finishing. Try again when it completes.".into()),
        }
    })
}

/// # Safety
/// `profile` is a terminated UTF-8 path. Close the engine before this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_keep_profile(profile: *const c_char) -> *mut c_char {
    native_result(|| {
        let path = std::path::Path::new(unsafe { string(profile)? });
        if !path.is_absolute() {
            return Err("Invalid profile path".into());
        }
        crate::engine::sign_out::keep_profile(path).map_err(|e| e.to_string())
    })
}

/// # Safety
/// Same open-handle rules as `pb_ios_open`; call on the host's runtime worker.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_stop_for_replacement(handle: *mut IosEngine) -> *mut c_char {
    native_result(|| {
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        if !state.engine.replacement_required() {
            return Err("No account replacement is waiting for confirmation".into());
        }
        match state.engine.shutdown(std::time::Duration::from_secs(3)).map_err(|e| e.to_string())? {
            crate::engine::Shutdown::Done => Ok(()),
            _ => Err("Sign-in is still finishing. Try again when it completes.".into()),
        }
    })
}

/// # Safety
/// `profile` is a terminated UTF-8 path. Close the engine before this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_reset_profile(profile: *const c_char) -> *mut c_char {
    native_result(|| {
        if profile.is_null() {
            return Err("Missing profile path".into());
        }
        let path = unsafe { CStr::from_ptr(profile) }.to_str().map_err(|e| e.to_string())?;
        crate::engine::sign_out::remove_profile(std::path::Path::new(path))
            .map_err(|e| e.to_string())
    })
}

/// # Safety
/// `profile` is a terminated UTF-8 path. Close the engine before this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_resolve_replacement(
    profile: *const c_char,
    replace: u8,
) -> *mut c_char {
    native_result(|| {
        let path = std::path::Path::new(unsafe { string(profile)? });
        if !path.is_absolute() || replace > 1 {
            return Err("Invalid account replacement request".into());
        }
        crate::engine::sign_out::resolve_replacement(path, replace != 0).map_err(|e| e.to_string())
    })
}

/// # Safety
/// Close each successful open once, with no calls in progress.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_close(handle: *mut IosEngine) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}
/// # Safety
/// Free each bridge response once; no other pointers are accepted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}
#[cfg(test)]
fn project(view: &ViewNode) -> Result<Value, String> {
    Ok(match view {
        // UIKit owns control appearance; retain the guest's semantic subtree.
        ViewNode::Theme { child, .. } => project(child)?,
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            json!({"kind":match view { ViewNode::Row(_) => "row", _ => "column" },"children":children.iter().map(project).collect::<Result<Vec<_>,_>>()?})
        }
        ViewNode::Text { text, style } => json!({"kind":"text","text":text,"style":*style as u8}),
        ViewNode::Button { id, label, disabled, style } => {
            json!({"kind":"button","id":id,"label":label,"disabled":disabled,"style":*style as u8})
        }
        ViewNode::TextField { id, value, placeholder, disabled, multiline, read_only } => {
            json!({"kind":"text-field","id":id,"value":value,"placeholder":placeholder,"disabled":disabled,"multiline":multiline,"read_only":read_only})
        }
        ViewNode::Checkbox { id, label, state, disabled } => {
            json!({"kind":"checkbox","id":id,"label":label,"state":*state as u8,"disabled":disabled})
        }
        ViewNode::Canvas { id, width, height, frames, items, .. } => {
            let items: Vec<_> = items.iter().map(|item| {
                let (kind, bounds) = match item {
                    crate::runtime::CanvasItem::Rect(rect) => ("rect", rect),
                    crate::runtime::CanvasItem::Text { bounds, .. } => ("text", bounds),
                    crate::runtime::CanvasItem::Pixels { bounds, .. } => ("pixels", bounds),
                };
                let mut value = json!({"kind":kind,"x":bounds.x,"y":bounds.y,"width":bounds.width,"height":bounds.height,"color":bounds.color.0});
                if let crate::runtime::CanvasItem::Text { font_size, bold, text, .. } = item {
                    value["font_size"] = json!(font_size); value["bold"] = json!(bold); value["text"] = json!(text);
                }
                if let crate::runtime::CanvasItem::Pixels { source_width, source_height, rgba, .. } = item {
                    value["source_width"] = json!(source_width); value["source_height"] = json!(source_height); value["pixel_bytes"] = json!(rgba.len());
                }
                value
            }).collect();
            json!({"kind":"canvas","id":id,"width":width,"height":height,"frames":*frames as u32,"items":items})
        }
        _ => return Err("This iPhone renderer does not support the submitted control".into()),
    })
}

#[cfg(test)]
fn qualification_todo_root() -> crate::store::Result<crate::store::RootInitialization> {
    crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../guests/todo/playbit-volumes.json"
    ))
    .map(|initializers| initializers.for_role(crate::package::AutomaticVolumeRole::Session))
    .map_err(|_| crate::store::Error::InvalidInput("invalid TODO Session declaration"))
}

// Retain this socket until seeding finishes: its reserved port must stay offline.
#[cfg(feature = "phase1-qualification")]
fn qualification_offline_transport(engine: &mut Engine) -> Result<std::net::UdpSocket, String> {
    // The seeded native-control fixture is explicitly offline. A separate
    // ordinary-launch test exercises sign-in presentation and explicit enrollment.
    // Keep its connection attempt off AP1 and deterministic after restart.
    let offline = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let mut config = crate::network::ap::ClientConfig::new(
        offline.local_addr().map_err(|e| e.to_string())?,
        "localhost",
        vec![],
    );
    config.connect_timeout = std::time::Duration::from_millis(50);
    engine
        .remote
        .as_mut()
        .ok_or("qualification starts before network work")?
        .set_test_transport(config);
    Ok(offline)
}

/// Explicit qualification fixture; excluded from ordinary product builds.
/// It initializes a fresh ordinary local root with the source starter schema.
/// # Safety
/// Same handle/string rules as `pb_ios_open`.
#[cfg(feature = "phase1-qualification")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_qualification_seed(
    handle: *mut IosEngine,
    bundle: *const c_char,
    personal_volumes: u8,
    account_profile: u8,
    session_list: u8,
) -> *mut IosReply {
    owner_response(|| {
        use crate::store::Id;
        let handle = unsafe { handle.as_ref() }.ok_or("local runtime is closed")?;
        let mut state = handle.0.lock().map_err(|_| "runtime lock poisoned")?;
        let engine = &mut state.engine;
        let _offline_transport = qualification_offline_transport(engine)?;
        if account_profile != 0
            && engine.services.store.display_name().map_err(|e| e.to_string())?.is_none()
        {
            // Isolated offline settings fixture, never an authenticated AP1
            // account or proof of the email/recovery onboarding journey.
            engine
                .services
                .store
                .save_recovery_state(&crate::store::RecoveryState {
                    secret: [42; 16],
                    acknowledged: true,
                    revision: 1,
                    synchronized_digest: None,
                    pending: None,
                })
                .map_err(|e| e.to_string())?;
            engine.services.store.set_display_name("Robin").map_err(|e| e.to_string())?;
        }
        if !engine.services.store.sessions().map_err(|e| e.to_string())?.is_empty() {
            return Ok(ReplyData::default());
        }
        let path = unsafe { string(bundle)? };
        let package = crate::package::Package::open(std::path::Path::new(&path))
            .map_err(|e| e.to_string())?;
        let mut declaration = package
            .initializer(crate::package::AutomaticVolumeRole::Session)
            .map_err(|e| e.to_string())?;
        declaration.permissions.grants.clear();
        let roots: &[&str] = if session_list != 0 {
            &["Production", "Development"]
        } else {
            &["Phase 1 starter root"]
        };
        for (index, name) in roots.iter().enumerate() {
            let id = Id::generate().map_err(|e| e.to_string())?;
            let root = engine
                .services
                .store
                .initialize_root(id, name, true, &declaration)
                .map_err(|e| e.to_string())?;
            engine.create_session(package.clone(), root.id).map_err(|e| e.to_string())?;
            if session_list != 0 && index == 0 {
                engine.create_session(package.clone(), root.id).map_err(|e| e.to_string())?;
            }
        }
        if personal_volumes != 0 {
            for name in ["Personal notes", "Travel plans", "Reading list", "Spare volume"] {
                if personal_volumes == 2 && name == "Personal notes" {
                    let reference =
                        qualification_database_volume(&engine.services.store, name, &declaration)
                            .map_err(|error| error.to_string())?;
                    std::fs::write(
                        engine.services.profile_path().join("qualification-settings.pbdb"),
                        reference.to_string(),
                    )
                    .map_err(|error| error.to_string())?;
                } else {
                    engine.services.store.create_volume(name, false).map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(ReplyData::default())
    })
}

#[cfg(feature = "phase1-qualification")]
fn qualification_database_volume(
    store: &crate::store::LocalStore,
    name: &str,
    declaration: &crate::store::RootInitialization,
) -> crate::store::Result<crate::store::DatabaseReference> {
    use crate::store::{
        Access, Id,
        volume_index::{EntryAddress, EntryKind},
    };
    let volume = store.initialize_personal_volume(Id::generate()?, name, false, declaration)?;
    let access = Access::owner(store.account().id);
    let database = store.create_database(&access, volume.id, "Settings", EntryKind::KvDb {})?;
    store.kv_put(
        &access,
        EntryAddress { volume: volume.id, entry: database.id },
        b"greeting",
        b"Hello",
    )?;
    Ok(crate::store::DatabaseReference { volume: volume.id, entry: database.id })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    unsafe fn take(value: *mut IosReply) -> Value {
        let mut data = Value::Null;
        if let Some(status) = unsafe { value.as_ref() }.and_then(|reply| reply.status.as_ref()) {
            let sessions: Vec<_> = status.sessions.iter().map(|row| json!({"id":row.id,"resolved_build_id":row.build,"root_volume":row.root,"app_name":row.name,"build_state":{"kind":row.build_kind},"build_message":row.build_message})).collect();
            let previews: Vec<_> = status.invitations.iter().map(|preview| {
                let invitation::Payload::Preview { session, paused, retained, root_name } = preview else { unreachable!() };
                json!({"session":session,"paused":paused,"retained":retained,"received":root_name.as_ref().map(|name|json!({"root_name":name}))})
            }).collect();
            let invitation::Payload::Status { status: watch, .. } = &status.watch else {
                unreachable!()
            };
            data = json!({"account":{"id":status.account},"enrolled":status.enrolled,"account_profile":status.profile,
                "sessions":sessions,"volumes":status.volumes,"examples":status.examples,"invitations":previews,"invitation_status":watch});
        }
        if let Some(volumes) =
            unsafe { value.as_ref() }.and_then(|reply| reply.personal_volumes.as_ref())
        {
            data = json!({"session":volumes.session,"volumes":volumes.volumes,"grants":volumes.grants});
        }
        if let Some(invitation) =
            unsafe { value.as_ref() }.and_then(|reply| reply.invitation.as_ref())
        {
            data = match invitation {
                invitation::Payload::Preview { session, paused, retained, root_name } => {
                    json!({"session":session,"paused":paused,"retained":retained,"received":root_name.as_ref().map(|name|json!({"root_name":name}))})
                }
                invitation::Payload::Status { paused, status } => {
                    let mut value = json!({"invitation_status":status});
                    if let Some(paused) = paused {
                        value["paused"] = json!(paused);
                    }
                    value
                }
                invitation::Payload::Link { invitation, availability } => {
                    json!({"invitation":invitation,"availability":availability})
                }
            };
        }
        if let Some(control) = unsafe { value.as_ref() }.and_then(|reply| reply.control.as_ref()) {
            use control_reply::ControlReply;
            data = match control {
                ControlReply::PersonalVolumesSelected(value) => {
                    json!({"personal_volumes_selected":value})
                }
                ControlReply::AgreementAccepted => json!({"agreement_accepted":true}),
                ControlReply::DisplayName(value) => json!({"display_name":value}),
                ControlReply::AccountKeyBackup(value) => json!({"account_key_backup":value}),
                ControlReply::SavedAccount(value) => json!({"saved_account":value}),
                ControlReply::SignInAvailable(value) => json!({"sign_in_available":value}),
                ControlReply::CodeSent(value) => json!({"code_sent":value}),
                ControlReply::Ordered(value) => json!({"ordered":value}),
                ControlReply::Forgotten(value) => json!({"forgotten":value}),
                ControlReply::Quit { instance, drain_failed } => {
                    json!({"quit":instance, "drain_failed":drain_failed})
                }
                ControlReply::Opened { instance, generation } => {
                    json!({"instance_id":instance,"generation":generation})
                }
                ControlReply::Prepared(value) => json!({"prepared_session":value}),
                ControlReply::Session(value) => json!({"id":value}),
            };
        }
        if let Some(transfer) = unsafe { value.as_ref() }.and_then(|reply| reply.transfer.as_ref())
        {
            data = json!({"transfer": transfer});
        }
        if let Some(metadata) =
            unsafe { value.as_ref() }.and_then(|reply| reply.presentation.as_ref())
        {
            data = json!({"instance_id":metadata.instance.to_string(), "generation":metadata.generation,
                "session":{"resolved_build_id":metadata.build}, "invitation":metadata.invitation,
                "availability":metadata.availability, "update":metadata.update});
        }
        if let Some(scene) = unsafe { pb_ios_reply_scene(value).as_ref() } {
            data["scene"] = project(scene).unwrap();
        }
        if let Some(payload) = unsafe { value.as_ref() }.and_then(|reply| reply.recovery.as_ref()) {
            data = match payload {
                recovery::Payload::Status(value) => serde_json::to_value(value).unwrap(),
                recovery::Payload::Prepared(value) => serde_json::to_value(value).unwrap(),
            };
        }
        if let Some(logs) = unsafe { value.as_ref() }.and_then(|reply| reply.logs.as_ref()) {
            data = serde_json::to_value(logs).unwrap();
        }
        let completion = unsafe { pb_ios_reply_completion(value) };
        let response = if completion.error.is_null() {
            json!({"ok":true,"data":data})
        } else {
            json!({"ok":false,"error":unsafe { CStr::from_ptr(completion.error) }.to_str().unwrap(),
                "code":if completion.error_code == 2 { "corrupt_store" } else { "error" }})
        };
        let mut result = response;
        if !unsafe { pb_ios_reply_schedule(value) }.generation.is_null() {
            let response = if completion.has_response != 0 {
                result["id"] = json!(completion.request_id);
                result
            } else {
                Value::Null
            };
            let error = if completion.background_error.is_null() {
                Value::Null
            } else {
                json!(unsafe { CStr::from_ptr(completion.background_error) }.to_str().unwrap())
            };
            result = json!({"ok":true,"data":{"response":response,"error":error}});
        }
        let schedule = unsafe { pb_ios_reply_schedule(value) };
        if !schedule.generation.is_null() {
            let generation: [u8; 16] =
                unsafe { std::slice::from_raw_parts(schedule.generation, 16) }.try_into().unwrap();
            result["data"]["changes"] = json!({"generation": crate::store::Id::from_bytes(generation), "sequence": schedule.sequence});
            result["data"]["wake_after_ns"] =
                if schedule.has_wake != 0 { json!(schedule.wake_after_ns) } else { Value::Null };
        }
        unsafe { pb_ios_reply_free(value) };
        result
    }
    #[test]
    fn native_completion_preserves_ids_and_bounds_error_strings() {
        for id in [0, u64::MAX] {
            let reply = owner_response(|| {
                Ok(ReplyData {
                    request_id: Some(id),
                    error: Some("failed\0é".repeat(200)),
                    background_error: Some("background\0é".repeat(200)),
                    ..ReplyData::default()
                })
            });
            unsafe {
                let completion = pb_ios_reply_completion(reply);
                assert_eq!(completion.has_response, 1);
                assert_eq!(completion.request_id, id);
                assert_eq!(completion.error_code, 1);
                for text in [completion.error, completion.background_error] {
                    let text = CStr::from_ptr(text).to_str().unwrap();
                    assert!(text.len() <= 508);
                    assert!(text.contains('�'));
                }
                pb_ios_reply_free(reply);
            }
        }
        let reply = owner_response(|| Err("corrupt_store".into()));
        unsafe {
            let completion = pb_ios_reply_completion(reply);
            assert_eq!(completion.has_response, 0);
            assert_eq!(completion.error_code, 2);
            assert_eq!(
                CStr::from_ptr(completion.error).to_str().unwrap(),
                "The local store is corrupt."
            );
            pb_ios_reply_free(reply);
            assert_eq!(pb_ios_reply_completion(std::ptr::null()).error_code, 1);
        }
    }

    #[test]
    fn native_schedule_preserves_full_width_fields_and_optional_deadline() {
        unsafe {
            assert!(pb_ios_reply_schedule(std::ptr::null()).generation.is_null());
        }
        for wake in [None, Some(0), Some(u64::MAX)] {
            let reply = owner_response(|| {
                Ok(ReplyData {
                    schedule: Some((
                        crate::engine::ChangeToken {
                            generation: crate::store::Id::from_bytes([255; 16]),
                            sequence: u64::MAX,
                        },
                        wake,
                    )),
                    scene: None,
                    ..ReplyData::default()
                })
            });
            unsafe {
                let schedule = pb_ios_reply_schedule(reply);
                assert_eq!(std::slice::from_raw_parts(schedule.generation, 16), &[255; 16]);
                assert_eq!(schedule.sequence, u64::MAX);
                assert_eq!(schedule.has_wake != 0, wake.is_some());
                assert_eq!(schedule.wake_after_ns, wake.unwrap_or(0));
                pb_ios_reply_free(reply);
            }
        }
        let reply = owner_response(|| Err("closed".into()));
        unsafe {
            assert!(pb_ios_reply_schedule(reply).generation.is_null());
            pb_ios_reply_free(reply);
        }
    }

    #[test]
    fn scenes_are_owned_separately_and_share_the_response_byte_limit() {
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(17),
                schedule: Some((
                    crate::engine::ChangeToken {
                        generation: crate::store::Id::from_bytes([7; 16]),
                        sequence: 2,
                    },
                    Some(100),
                )),
                scene: Some(ViewNode::Text {
                    text: "native scene".into(),
                    style: playbit_api::ui::TextStyle::Body,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert!(!pb_ios_reply_scene(reply).is_null());
            pb_ios_reply_free(reply);
        }
        let reply = owner_response(|| {
            Ok(ReplyData {
                request_id: Some(17),
                schedule: Some((
                    crate::engine::ChangeToken {
                        generation: crate::store::Id::from_bytes([7; 16]),
                        sequence: 2,
                    },
                    Some(100),
                )),
                scene: Some(ViewNode::Text {
                    text: "x".repeat(MAX_RESPONSE_BYTES),
                    style: playbit_api::ui::TextStyle::Body,
                }),
                ..ReplyData::default()
            })
        });
        unsafe {
            assert!(pb_ios_reply_scene(reply).is_null());
            let value = take(reply);
            assert_eq!(value["data"]["response"]["id"], 17);
            assert_eq!(value["data"]["response"]["ok"], false);
            assert_eq!(value["data"]["changes"]["sequence"], 2);
            assert_eq!(value["data"]["wake_after_ns"], 100);
        }
    }

    #[test]
    fn native_lifecycle_errors_are_bounded_and_do_not_transfer_descriptors() {
        let mut descriptor = 99;
        let error = unsafe { pb_ios_completion_descriptor(std::ptr::null_mut(), &mut descriptor) };
        assert_eq!(descriptor, -1);
        assert!(!error.is_null());
        assert_eq!(unsafe { CStr::from_ptr(error) }.to_str().unwrap(), "local runtime is closed");
        unsafe { pb_ios_string_free(error) };
        let error = unsafe { pb_ios_suspend(std::ptr::null_mut()) };
        assert!(!error.is_null());
        unsafe { pb_ios_string_free(error) };
        let error = native_result(|| Err(format!("a\0{}", "é".repeat(512))));
        let text = unsafe { CStr::from_ptr(error) }.to_str().unwrap();
        assert!(text.len() <= 508);
        assert!(text.starts_with("a�"));
        unsafe { pb_ios_string_free(error) };
    }

    // Drive only the descriptor or an explicit owner deadline, as UIKit does.
    // A response is delivered exactly once; snapshot reads cannot create wakes.
    unsafe fn request(handle: *mut IosEngine, value: Value) -> Value {
        use std::{
            os::fd::FromRawFd,
            time::{Duration, Instant},
        };
        let mut descriptor = -1;
        assert!(unsafe { pb_ios_completion_descriptor(handle, &mut descriptor) }.is_null());
        let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(descriptor) };
        let poller = polling::Poller::new().unwrap();
        unsafe {
            poller.add(&socket, polling::Event::readable(1)).unwrap();
        }
        let started = Instant::now();
        let mut state = unsafe {
            let op = value["op"].as_str().unwrap();
            take(match op {
                "open-app" => {
                    let session = value["session"].as_str().unwrap();
                    pb_ios_open_app(handle, 1, session.as_ptr(), session.len())
                }
                "app-frame" | "quit-app" | "app-logs" => {
                    let instance = value["instance_id"].as_str().unwrap();
                    match op {
                        "app-frame" => {
                            pb_ios_app_frame(handle, 1, instance.as_ptr(), instance.len())
                        }
                        "quit-app" => pb_ios_quit_app(handle, 1, instance.as_ptr(), instance.len()),
                        _ => pb_ios_app_logs(handle, 1, instance.as_ptr(), instance.len()),
                    }
                }
                "app-input" => {
                    let instance = value["instance_id"].as_str().unwrap();
                    let data = serde_json::from_value::<Vec<u8>>(value["data"].clone()).unwrap();
                    pb_ios_app_input(
                        handle,
                        1,
                        instance.as_ptr(),
                        instance.len(),
                        value["kind"].as_u64().unwrap().try_into().unwrap(),
                        value["target"].as_u64().unwrap().try_into().unwrap(),
                        data.as_ptr(),
                        data.len(),
                    )
                }
                _ => match serde_json::from_value::<Request>(value.clone()).unwrap() {
                    Request::Status => pb_ios_status(handle, 1),
                    Request::AcceptAgreement { account, .. } => {
                        let account = account.to_string();
                        pb_ios_accept_agreement(handle, 1, account.as_ptr(), account.len())
                    }
                    Request::PrepareSignIn => pb_ios_prepare_sign_in(handle, 1),
                    Request::TransferStatus => pb_ios_transfer_status(handle, 1),
                    Request::CancelTransfer => pb_ios_cancel_transfer(handle, 1),
                    Request::RecoveryStatus => pb_ios_recovery_status(handle, 1),
                    Request::PrepareRecovery => pb_ios_prepare_recovery(handle, 1),
                    Request::SaveRecovery => pb_ios_save_recovery(handle, 1),
                    Request::PrepareSession { session } => {
                        pb_ios_prepare_session(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::CheckUpdates { session } => {
                        pb_ios_check_updates(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::PersonalVolumes { session: version } => {
                        pb_ios_personal_volumes(handle, 1, version.as_ptr(), version.len())
                    }
                    Request::Invitation { session } => {
                        pb_ios_invitation(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::ForgetSession { session } => {
                        pb_ios_forget_session(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::WatchInvitation { session } => {
                        pb_ios_watch_invitation(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::ResumeInvitation { session } => {
                        pb_ios_resume_invitation(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::PauseInvitation { session } => {
                        pb_ios_pause_invitation(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::StartJoin { session } => {
                        pb_ios_start_join(handle, 1, session.as_ptr(), session.len())
                    }
                    Request::PreviewInvitation { invitation } => {
                        pb_ios_preview_invitation(handle, 1, invitation.as_ptr(), invitation.len())
                    }
                    Request::SetDisplayName { name } => {
                        pb_ios_set_display_name(handle, 1, name.as_ptr(), name.len())
                    }
                    Request::SignIn { code, .. } => {
                        pb_ios_sign_in(handle, 1, code.as_ptr(), code.len())
                    }
                    Request::RequestCode { email } => {
                        pb_ios_request_code(handle, 1, email.as_ptr(), email.len())
                    }
                    Request::RecoverCredentials { secret } => {
                        pb_ios_recover_credentials(handle, 1, secret.as_ptr(), secret.len())
                    }
                    Request::ListApps { after } => {
                        let after = after.unwrap_or_default();
                        pb_ios_list_apps(handle, 1, after.as_ptr(), after.len())
                    }
                    Request::CreateAppSession { app, root: None, user: None } => {
                        let publisher = app.publisher.to_string();
                        pb_ios_create_app_session(
                            handle,
                            1,
                            publisher.as_ptr(),
                            publisher.len(),
                            app.app_id.as_ptr(),
                            app.app_id.len(),
                        )
                    }
                    Request::GetAppDisplay { app, build } => {
                        let publisher = app.publisher.to_string();
                        pb_ios_get_app_display(
                            handle,
                            1,
                            publisher.as_ptr(),
                            publisher.len(),
                            app.app_id.as_ptr(),
                            app.app_id.len(),
                            build.as_ptr(),
                            build.len(),
                        )
                    }
                    Request::SignInWithKey { email, account_key } => pb_ios_sign_in_with_key(
                        handle,
                        1,
                        email.as_ptr(),
                        email.len(),
                        account_key.as_ptr(),
                        account_key.len(),
                    ),
                    Request::OrderSessions { sessions } => {
                        let bytes = sessions
                            .iter()
                            .flat_map(|id| crate::identifier::decode::<32>(id).unwrap())
                            .collect::<Vec<_>>();
                        pb_ios_order_sessions(handle, 1, bytes.as_ptr(), sessions.len())
                    }
                    Request::SelectPersonalVolumes { session: version, grants } => {
                        let volumes =
                            grants.iter().flat_map(|g| *g.volume.as_bytes()).collect::<Vec<_>>();
                        let writes = grants.iter().map(|g| u8::from(g.write)).collect::<Vec<_>>();
                        pb_ios_select_personal_volumes(
                            handle,
                            1,
                            version.as_ptr(),
                            version.len(),
                            volumes.as_ptr(),
                            writes.as_ptr(),
                            grants.len(),
                        )
                    }
                    // Fixture setup operations have no UIKit caller or exported C entry.
                    request => owner_response(|| submit(&*handle, 1, request)),
                },
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert_eq!(state["ok"], true, "{state}");
            if !state["data"]["response"].is_null() {
                let elapsed = started.elapsed().as_nanos() as u64;
                poller.delete(&socket).unwrap();
                let mut result = state["data"]["response"].take();
                result["test_elapsed_ns"] = elapsed.into();
                let next = unsafe { take(pb_ios_delivery_ready(handle)) };
                assert_eq!(next["ok"], true);
                assert!(next["data"]["response"].is_null());
                return result;
            }
            assert!(Instant::now() < deadline, "missing embedded response: {state}");
            let wait = state["data"]["wake_after_ns"]
                .as_u64()
                .map(Duration::from_nanos)
                .unwrap_or(Duration::from_secs(10))
                .min(deadline.saturating_duration_since(Instant::now()));
            let mut events = polling::Events::new();
            poller.wait(&mut events, Some(wait)).unwrap();
            state = unsafe {
                take(if events.is_empty() {
                    pb_ios_wake(handle)
                } else {
                    pb_ios_complete_background(handle)
                })
            };
            // polling registrations are one-shot. Creation and initialization
            // can produce separate notifications for the same request.
            poller.modify(&socket, polling::Event::readable(1)).unwrap();
        }
    }

    // Test observation waits on exact completion readiness; product scene
    // delivery remains notification driven and never polls for data freshness.
    unsafe fn settle_local_guests(handle: *mut IosEngine) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let socket = unsafe { &*handle }
            .0
            .lock()
            .unwrap()
            .engine
            .completion_socket()
            .unwrap()
            .try_clone()
            .unwrap();
        let poller = polling::Poller::new().unwrap();
        unsafe {
            poller.add(&socket, polling::Event::readable(1)).unwrap();
        }
        loop {
            if unsafe { &*handle }.0.lock().unwrap().owner.apps.settled() {
                break;
            }
            let now = std::time::Instant::now();
            assert!(now < deadline, "local guest completion did not settle");
            let ready = unsafe { &*handle }.0.lock().unwrap().owner.apps.wake_after(now)
                == Some(std::time::Duration::ZERO);
            let mut events = polling::Events::new();
            if !ready {
                poller.wait(&mut events, Some(deadline.saturating_duration_since(now))).unwrap();
            }
            let reply = unsafe {
                take(if events.is_empty() {
                    pb_ios_wake(handle)
                } else {
                    pb_ios_complete_background(handle)
                })
            };
            assert_eq!(reply["ok"], true);
            assert!(reply["data"]["response"].is_null());
            poller.modify(&socket, polling::Event::readable(1)).unwrap();
        }
        poller.delete(&socket).unwrap();
    }

    #[test]
    fn real_todo_bridge_delivers_shared_changes_and_retains_guest_draft_without_sync() {
        use std::collections::BTreeMap;
        // Both retained local-only bindings and new Shared bindings use the
        // same guest-owner delivery path, with distinct sharing authority.
        for sync_shared in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = CString::new(directory.path().to_str().unwrap()).unwrap();
            let mut handle = std::ptr::null_mut();
            assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) }["ok"], true);
            let session = {
                let state = unsafe { &*handle }.0.lock().unwrap();
                let store = &state.engine.services.store;
                let initializers = crate::package::VolumeInitializers::from_json(include_bytes!(
                    "../../../../guests/todo/playbit-volumes.json"
                ))
                .unwrap();
                let package = crate::package::signature::testing::signed(
                    store,
                    crate::package::Package::create(
                        "test".into(),
                        "TODO".into(),
                        playbit_test_fixtures::Guest::Todo.wasm().to_vec(),
                        BTreeMap::new(),
                        &initializers,
                    )
                    .unwrap(),
                )
                .unwrap();
                let app = package.identity().unwrap().clone();
                let build = package.id().unwrap();
                let builds = state.engine.services.profile_path().join(crate::paths::BUILDS);
                std::fs::create_dir_all(&builds).unwrap();
                std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
                store.remember_imported_app(&package).unwrap();
                let root = app
                    .volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)
                    .unwrap();
                store
                    .create_session_binding(crate::store::SessionCreation {
                        app,
                        selector: crate::store::BuildSelector::Exact { build_id: build.clone() },
                        resolved_build_id: build,
                        root_volume: root,
                        user_volume: None,
                        sync_shared,
                    })
                    .unwrap()
                    .id
            };
            fn control(scene: &serde_json::Value, kind: &str) -> Option<serde_json::Value> {
                if scene["kind"] == kind {
                    return Some(scene.clone());
                }
                scene["children"].as_array()?.iter().find_map(|child| control(child, kind))
            }
            let instances = std::array::from_fn::<_, 2, _>(|_| {
                let reply = unsafe { request(handle, json!({"op":"open-app","session":session})) };
                assert_eq!(reply["ok"], true, "{reply}");
                assert!(reply["data"]["generation"].as_u64().unwrap() > 0);
                reply["data"]["instance_id"].as_str().unwrap().to_owned()
            });
            assert_ne!(instances[0], instances[1]);
            unsafe { settle_local_guests(handle) };
            // The typed UIKit route must admit the maximum value plus all native metadata.
            let draft = "é".repeat(playbit_api::abi::text::MAX_TEXT_BYTES / 2);
            let draft = draft.as_str();
            let input = playbit_api::abi::text::TextInput::encode(
                draft,
                &[playbit_api::abi::text::TextEdit { start: 0, end: 2, inserted_len: 2 };
                    playbit_api::abi::text::MAX_TEXT_EDITS],
            )
            .unwrap();
            assert_eq!(
                unsafe {
                    request(
                        handle,
                        json!({"op":"app-input","instance_id":instances[0],
                        "kind":14,"target":1,"data":input}),
                    )
                }["ok"],
                true
            );
            let frame =
                unsafe { request(handle, json!({"op":"app-frame","instance_id":instances[1]})) };
            if sync_shared {
                let channel = unsafe { &*handle }.0.lock().unwrap().engine.channel();
                let reference = crate::store::SessionReference::from_link(
                    frame["data"]["invitation"].as_str().expect("Shared frame retains its link"),
                    channel,
                )
                .unwrap();
                assert_eq!(reference.internal_id(), session.as_str());
            } else {
                assert!(
                    frame["data"]["invitation"].is_null(),
                    "retained local-only bindings grant no sharing capability"
                );
            }
            let checkbox = control(&frame["data"]["scene"], "checkbox").unwrap();
            let result = unsafe {
                request(
                    handle,
                    json!({"op":"app-input","instance_id":instances[1],
            "kind":2,"target":checkbox["id"],"data":[]}),
                )
            };
            assert_eq!(result["ok"], true, "{result}");
            let quit =
                unsafe { request(handle, json!({"op":"quit-app","instance_id":instances[1]})) };
            assert_eq!(quit["ok"], true, "{quit}");
            assert_eq!(quit["data"]["quit"], instances[1]);
            unsafe { settle_local_guests(handle) };
            let frame =
                unsafe { request(handle, json!({"op":"app-frame","instance_id":instances[0]})) };
            assert_eq!(frame["ok"], true, "{frame}");
            assert_eq!(control(&frame["data"]["scene"], "checkbox").unwrap()["state"], 1);
            assert_eq!(control(&frame["data"]["scene"], "text-field").unwrap()["value"], draft);
            let quiet = unsafe { take(pb_ios_wake(handle)) };
            assert!(
                quiet["data"]["response"].is_null(),
                "a completed edit cannot be delivered twice"
            );
            assert!(quiet["data"]["wake_after_ns"].is_null(), "idle TODO has no timer");
            assert!(unsafe { pb_ios_suspend(handle) }.is_null());
            let resumed = unsafe { take(pb_ios_resume(handle)) };
            assert_eq!(resumed["ok"], true);
            assert!(
                resumed["data"]["wake_after_ns"].is_null(),
                "local resume cannot start polling"
            );
            unsafe { pb_ios_close(handle) };
        }
    }

    #[test]
    #[ignore = "run optimized explicitly; host bridge diagnostic, not UIKit/device qualification"]
    fn benchmark_todo_bridge() {
        use crate::store::{Id, InitialDatabaseKind, object_contract::Value};
        use std::collections::BTreeMap;
        for rows in [1, 32, 64] {
            let directory = tempfile::tempdir().unwrap();
            let path = CString::new(directory.path().to_str().unwrap()).unwrap();
            let mut handle = std::ptr::null_mut();
            assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) }["ok"], true);
            let session = {
                let mut state = unsafe { &*handle }.0.lock().unwrap();
                let engine = &mut state.engine;
                let mut declaration = qualification_todo_root().unwrap();
                let InitialDatabaseKind::ObjectDb { objects, .. } =
                    &mut declaration.databases[0].kind
                else {
                    panic!("TODO declares an object database")
                };
                let mut seed = objects[0].clone();
                seed.fields.insert(Id::from_bytes([2; 16]), vec![Value::Text("x".repeat(64))]);
                objects.clear();
                for _ in 0..rows {
                    seed.id = Id::generate().unwrap();
                    objects.push(seed.clone());
                }
                let root = engine
                    .services
                    .store
                    .initialize_root(Id::generate().unwrap(), "Benchmark TODO", false, &declaration)
                    .unwrap();
                let package = crate::package::Package::new(
                    "test".into(),
                    "TODO".into(),
                    playbit_test_fixtures::Guest::Todo.wasm().to_vec(),
                    BTreeMap::new(),
                )
                .unwrap();
                engine.create_session(package, root.id).unwrap().id
            };
            let opened = unsafe { request(handle, json!({"op":"open-app","session":session})) };
            assert_eq!(opened["ok"], true);
            let instance = opened["data"]["instance_id"].as_str().unwrap();
            unsafe { settle_local_guests(handle) };
            let frame =
                unsafe { request(handle, json!({"op":"app-frame","instance_id":instance})) };
            fn checkbox(scene: &serde_json::Value) -> Option<u64> {
                if scene["kind"] == "checkbox" {
                    return scene["id"].as_u64();
                }
                scene["children"].as_array()?.iter().find_map(checkbox)
            }
            let target = checkbox(&frame["data"]["scene"]).unwrap();
            let mut samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                let result = unsafe {
                    request(
                        handle,
                        json!({"op":"app-input","instance_id":instance,"kind":2,"target":target,"data":[]}),
                    )
                };
                assert_eq!(result["ok"], true, "{result}");
                samples.push(result["test_elapsed_ns"].as_u64().unwrap());
                unsafe { settle_local_guests(handle) };
            }
            samples.sort_unstable();
            println!(
                "{}",
                json!({"boundary":"input admission through correlated parsed response; storage completion may remain Pending; includes worker scheduling, guest turn, native projection and test-only assertion materialization; excludes caller preparation, descriptor setup, main delivery, UIKit/layout/presentation",
                "target_os":std::env::consts::OS,"rows":rows,"title_bytes":64,"sessions":1,"samples":1000,
                "p50_ns":samples[499],"p95_ns":samples[949],"p99_ns":samples[989],"maximum_ns":samples[999],"errors":0})
            );
            unsafe { pb_ios_close(handle) };
        }
    }

    #[test]
    fn oversized_response_preserves_request_identity_and_runtime_wakeup() {
        let pointer = owner_response(|| {
            Ok(ReplyData {
                control: Some(control_reply::ControlReply::DisplayName(
                    "x".repeat(MAX_RESPONSE_BYTES),
                )),
                request_id: Some(41),
                schedule: Some((
                    crate::engine::ChangeToken {
                        generation: crate::store::Id::from_bytes([7; 16]),
                        sequence: 7,
                    },
                    Some(123),
                )),
                scene: None,
                ..ReplyData::default()
            })
        });
        let encoded = unsafe { take(pointer) };
        assert_eq!(encoded["ok"], true);
        assert_eq!(encoded["data"]["response"]["id"], 41);
        assert_eq!(encoded["data"]["response"]["ok"], false);
        assert!(
            encoded["data"]["response"]["error"].as_str().unwrap().contains("may have completed")
        );
        assert_eq!(encoded["data"]["wake_after_ns"], 123);
        assert_eq!(encoded["data"]["changes"]["sequence"], 7);
        let initial = unsafe {
            take(owner_response(|| {
                Ok(ReplyData {
                    control: Some(control_reply::ControlReply::DisplayName(
                        "x".repeat(MAX_RESPONSE_BYTES),
                    )),
                    ..ReplyData::default()
                })
            }))
        };
        assert_eq!(initial["ok"], false);
    }

    #[test]
    fn native_projection_keeps_semantic_style_without_custom_theme_or_substitutions() {
        // A PBV1 themed column containing a disabled primary Add button.
        let mut bytes = b"PBV1".to_vec();
        for (kind, id, payload) in [
            (10u8, 0u32, &[12, 255, 51, 34, 17][..]),
            (1, 0, &[][..]),
            (3, 7, &[1, 1, b'A', b'd', b'd'][..]),
            (4, 0, &[][..]),
        ] {
            bytes.push(kind);
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
        }
        let view = crate::runtime::decode_view(&bytes).unwrap();
        assert_eq!(
            project(&view).unwrap(),
            json!({"kind":"column","children":[
                {"kind":"button","id":7,"label":"Add","disabled":true,"style":1}
            ]})
        );
        assert!(
            project(&ViewNode::Radio {
                id: 8,
                label: "Radio".into(),
                selected: true,
                disabled: false
            })
            .is_err()
        );
    }

    #[test]
    fn suspended_progress_wake_does_not_complete_a_pending_worker() {
        // Hold the worker result in the fixture so a progress-only wake always
        // arrives before cancellation completes, independent of thread timing.
        use crate::engine::{background::tests as background, reply::TransferKind};
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        let opened = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
        assert_eq!(opened["ok"], true, "{opened}");
        let (send, remote) = {
            let mut state = unsafe { &*handle }.0.lock().unwrap();
            background::transfer(&mut state.engine)
        };
        assert!(unsafe { pb_ios_suspend(handle) }.is_null());
        {
            let state = unsafe { &*handle }.0.lock().unwrap();
            state.engine.network.progress.set(
                Some(crate::engine::reply::TransferProgress::Connecting {
                    hostname: "suspend.invalid".into(),
                }),
                &state.engine.completion.wake,
            );
        }
        let progressed = unsafe { take(pb_ios_complete_background(handle)) };
        assert_eq!(progressed["ok"], true, "{progressed}");
        assert!(progressed["data"]["wake_after_ns"].is_null());
        {
            let state = unsafe { &*handle }.0.lock().unwrap();
            assert_eq!(state.engine.transfer_result.kind, TransferKind::Canceling);
            assert!(state.engine.background_pending());
            assert!(state.engine.remote.is_none());
            send.send((remote, Err("canceled fixture".into()), Default::default())).unwrap();
            background::notify(&state.engine);
        }
        let completed = unsafe { take(pb_ios_complete_background(handle)) };
        assert_eq!(completed["ok"], true, "{completed}");
        {
            let state = unsafe { &*handle }.0.lock().unwrap();
            assert_eq!(state.engine.transfer_result.kind, TransferKind::Paused);
            assert!(!state.engine.background_pending());
            assert!(state.engine.remote.is_some());
        }
        unsafe { pb_ios_close(handle) };
    }

    #[test]
    fn suspend_pauses_an_in_flight_join_without_losing_identity_or_invitation() {
        use crate::network::ap::{ClientConfig, ClientIdentity};
        use crate::store::{BuildSelector, LocalStore};
        use std::{
            net::UdpSocket,
            time::{Duration, Instant},
        };

        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("phone");
        let store = LocalStore::open(&profile).unwrap();
        let account = store.account().id;
        let root = store.create_volume("Saved shared root", true).unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let invitation =
            store.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
        store
            .preview_invitation(&invitation, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        // A structurally enrolled test identity makes the real QUIC transport
        // start. No authentication succeeds against the deliberately silent UDP
        // endpoint, and these values never leave this isolated test profile.
        let mut identity = ClientIdentity::from_seed(
            "phone@suspend.invalid",
            "Phone",
            store.account_seed().unwrap(),
        )
        .unwrap();
        identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.instance_id = crate::store::Id::from_bytes([1; 16]).to_string().into_bytes();
        identity.ca_certificate = vec![1];
        identity.instance_certificate = vec![1];
        identity.save(&profile.join("instance.json")).unwrap();
        drop(store);
        let path = CString::new(profile.to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        let opened = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
        assert_eq!(opened["ok"], true, "{opened}");
        let endpoint = UdpSocket::bind("127.0.0.1:0").unwrap();
        endpoint.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut config = ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![]);
        config.connect_timeout = Duration::from_millis(500);
        unsafe { &*handle }
            .0
            .lock()
            .unwrap()
            .engine
            .remote
            .as_mut()
            .unwrap()
            .set_test_transport(config);
        assert_eq!(
            unsafe { take(pb_ios_start_join(handle, 1, session.id.as_ptr(), session.id.len())) }["data"]
                ["response"]["data"]["transfer"]["kind"],
            "running"
        );
        assert_eq!(unsafe { take(pb_ios_delivery_ready(handle)) }["ok"], true);
        endpoint.recv_from(&mut [0; 2048]).expect("join must begin a real QUIC connection");
        assert!(unsafe { &*handle }.0.lock().unwrap().engine.remote.is_none());
        let started = Instant::now();
        assert!(unsafe { pb_ios_suspend(handle) }.is_null());
        let transfer = unsafe { &*handle }
            .0
            .lock()
            .unwrap()
            .engine
            .dispatch_reply(Request::TransferStatus)
            .unwrap();
        assert!(matches!(transfer, crate::engine::reply::Reply::Transfer { transfer }
            if transfer.kind == crate::engine::reply::TransferKind::Canceling));
        assert!(started.elapsed() < Duration::from_millis(250), "suspend waited for the network");
        use std::os::fd::FromRawFd;
        let mut fd = -1;
        assert!(unsafe { pb_ios_completion_descriptor(handle, &mut fd) }.is_null());
        let completion = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let poller = polling::Poller::new().unwrap();
        unsafe {
            poller.add(&completion, polling::Event::readable(1)).unwrap();
        }
        let mut events = polling::Events::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        // Progress and worker completion share this descriptor. Keep observing,
        // as the native host does, until the canceled worker returns Remote.
        while unsafe { &*handle }.0.lock().unwrap().engine.background_pending() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("canceled join must complete before the deadline");
            assert_eq!(poller.wait(&mut events, Some(remaining)).unwrap(), 1);
            let completed = unsafe { take(pb_ios_complete_background(handle)) };
            assert_eq!(completed["ok"], true, "{completed}");
            assert!(completed["data"]["wake_after_ns"].is_null());
            events.clear();
            poller.modify(&completion, polling::Event::readable(1)).unwrap();
        }
        poller.delete(&completion).unwrap();
        assert_eq!(
            unsafe { take(pb_ios_status(handle, 2)) }["ok"],
            false,
            "suspension must not admit a new request"
        );
        let mut state = unsafe { &*handle }.0.lock().unwrap();
        let engine = &mut state.engine;
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::TransferStatus).unwrap()).unwrap()
                ["transfer"]["kind"],
            "paused"
        );
        let value = serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap();
        assert_eq!(value["account"]["id"], account.to_string());
        assert_eq!(value["invitations"][0]["paused"], true);
        assert_eq!(
            engine
                .services
                .store
                .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
                .unwrap(),
            invitation
        );
        assert!(engine.services.store.pending_invitation_preview(&session.id).unwrap().retained);
        assert_eq!(engine.services.store.session(&session.id).unwrap().root_volume, root.id);
        assert_eq!(
            engine.services.store.session(&session.id).unwrap().root_volume,
            session.root_volume
        );
        drop(state);
        // Repeated lifecycle notifications also work after Remote has returned.
        assert!(unsafe { pb_ios_suspend(handle) }.is_null());
        unsafe { pb_ios_close(handle) };
    }
    #[test]
    fn sign_out_bridge_preserves_pending_writes_and_requires_closed_owners() {
        // The real C boundary clears enrollment only after releasing storage;
        // repeated launches retain unsynchronized data and the device identity.
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        let account = store.account().id;
        let seed = store.account_seed().unwrap();
        store.create_volume("Unsynchronized", true).unwrap();
        let pending = store.pending_operations().unwrap();
        assert!(!pending.is_empty());
        let mut identity =
            crate::network::ap::ClientIdentity::from_seed("robin@example.test", "Robin", seed)
                .unwrap();
        identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.instance_id = crate::store::Id::from_bytes([2; 16]).to_string().into_bytes();
        identity.ca_certificate = vec![1];
        identity.instance_certificate = vec![2];
        identity.save(&directory.path().join("instance.json")).unwrap();
        drop(store);
        let mut handle = std::ptr::null_mut();
        assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) }["ok"], true);
        assert!(unsafe { &*handle }.0.lock().unwrap().engine.enrolled());
        let before = std::fs::read(directory.path().join("instance.json")).unwrap();
        let error = unsafe { pb_ios_keep_profile(path.as_ptr()) };
        assert!(!error.is_null(), "live storage prevents credential mutation");
        unsafe { pb_ios_string_free(error) };
        assert_eq!(std::fs::read(directory.path().join("instance.json")).unwrap(), before);
        assert!(unsafe { pb_ios_stop_for_sign_out(handle) }.is_null());
        unsafe { pb_ios_close(handle) };
        assert!(unsafe { pb_ios_keep_profile(path.as_ptr()) }.is_null());
        for _ in 0..2 {
            let engine = Engine::open(directory.path()).unwrap();
            assert!(!engine.enrolled());
            assert_eq!(engine.services.store.account().id, account);
            assert_eq!(engine.services.store.account_seed().unwrap(), seed);
            assert_eq!(engine.services.store.pending_operations().unwrap(), pending);
        }
    }

    #[test]
    fn retained_session_names_are_delivered_on_status_after_open_and_reopen() {
        use crate::store::{BuildSelector, LocalStore};
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path();
        let store = LocalStore::open(profile).unwrap();
        let root = store.create_volume("Shared notes", false).unwrap();
        let package = crate::package::Package::new(
            "test".into(),
            "Notes".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            Default::default(),
        )
        .unwrap();
        let build = package.id().unwrap();
        std::fs::create_dir_all(profile.join(crate::paths::BUILDS)).unwrap();
        std::fs::write(
            profile.join(crate::paths::BUILDS).join(&build),
            package.to_bytes().unwrap(),
        )
        .unwrap();
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        drop(store);
        let path = CString::new(profile.to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        for _ in 0..2 {
            let opened = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
            assert_eq!(opened["ok"], true, "{opened}");
            assert!(opened["data"].is_null(), "open must not materialize unused status");
            let status = unsafe { request(handle, json!({"op":"status"})) };
            assert_eq!(status["ok"], true, "{status}");
            assert_eq!(status["data"]["sessions"][0]["id"], session.id);
            assert_eq!(status["data"]["sessions"][0]["app_name"], "Notes");
            assert_eq!(status["data"]["sessions"][0]["root_volume"], root.id.to_string());
            unsafe { pb_ios_close(handle) };
        }
    }

    #[test]
    fn typed_input_rejects_invalid_spans_before_admission() {
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) }["ok"], true);
        let instance = [b'a'; 32];
        for (instance_len, data_len) in
            [(31, 0), (32, 1), (32, playbit_api::abi::syscall::MAX_EVENT_BYTES + 1)]
        {
            let result = unsafe {
                take(pb_ios_app_input(
                    handle,
                    1,
                    instance.as_ptr(),
                    instance_len,
                    2,
                    1,
                    std::ptr::null(),
                    data_len,
                ))
            };
            assert_eq!(result["error"], "invalid input byte spans");
        }
        assert_eq!(unsafe { request(handle, json!({"op":"status"})) }["ok"], true);
        unsafe { pb_ios_close(handle) };
    }

    #[test]
    fn typed_collection_controls_reject_bounds_before_reading_arrays() {
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) }["ok"], true);
        for count in [1, 257, usize::MAX] {
            assert_eq!(
                unsafe { take(pb_ios_order_sessions(handle, 1, std::ptr::null(), count)) }["error"],
                "invalid session order span"
            );
        }
        let version = crate::identifier::encode(&[0xaa; 32]);
        for count in [1, 129, usize::MAX] {
            assert_eq!(
                unsafe {
                    take(pb_ios_select_personal_volumes(
                        handle,
                        1,
                        version.as_ptr(),
                        version.len(),
                        std::ptr::null(),
                        std::ptr::null(),
                        count,
                    ))
                }["error"],
                "invalid personal-volume spans"
            );
        }
        let volume = crate::identifier::encode(&[0xbb; 16]);
        assert_eq!(
            unsafe {
                take(pb_ios_select_personal_volumes(
                    handle,
                    1,
                    version.as_ptr(),
                    version.len(),
                    volume.as_ptr(),
                    [2].as_ptr(),
                    1,
                ))
            }["error"],
            "invalid write flag"
        );

        assert_eq!(
            unsafe { request(handle, json!({"op":"order-sessions", "sessions":[]})) }["ok"],
            true
        );
        unsafe { pb_ios_close(handle) };
    }

    #[test]
    fn changing_profile_stops_process_tracing_before_new_account_work() {
        const CHILD: &str = "PLAYBIT_IOS_CHANNEL_TRACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::ios::tests::changing_profile_stops_process_tracing_before_new_\
                     account_work",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut handle = std::ptr::null_mut();
        let public = CString::new(directory.path().join("public").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { take(pb_ios_open(public.as_ptr(), 0, &mut handle)) }["ok"], true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut control = loop {
            match crate::trace::control::connect(
                &directory.path().join("public"),
                crate::trace::control::Command::Start,
                None,
                false,
            ) {
                Ok(control) => break control,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("trace owner unavailable: {error}"),
            }
        };
        control.set_nonblocking(false).unwrap();
        control.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut status = [0; crate::trace::control::STATUS_BYTES];
        std::io::Read::read_exact(&mut control, &mut status).unwrap();
        assert!(crate::trace::control::Status::decode(&status).unwrap().recording);
        assert!(crate::trace::is_recording());
        assert!(unsafe { pb_ios_stop_for_sign_out(handle) }.is_null());
        unsafe { pb_ios_close(handle) };
        let testing = CString::new(directory.path().join("testing").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { take(pb_ios_open(testing.as_ptr(), 1, &mut handle)) }["ok"], true);
        assert!(!crate::trace::is_recording());
        assert_eq!(unsafe { request(handle, json!({"op":"status"})) }["ok"], true);
        assert!(!crate::trace::is_recording());
        unsafe { pb_ios_close(handle) };
    }

    #[test]
    fn runtime_channels_keep_distinct_identity_and_data_across_closed_owners() {
        let directory = tempfile::tempdir().unwrap();
        let mut identities = Vec::new();
        for cycle in 0..2 {
            for (channel, name) in [(0, "public"), (1, "testing")] {
                let profile = directory.path().join(name);
                let path = CString::new(profile.to_str().unwrap()).unwrap();
                let mut handle = std::ptr::null_mut();
                assert_eq!(
                    unsafe { take(pb_ios_open(path.as_ptr(), channel, &mut handle)) }["ok"],
                    true
                );
                let state = unsafe { &*handle }.0.lock().unwrap();
                let store = &state.engine.services.store;
                if cycle == 0 {
                    identities.push(store.account().id);
                    store.create_volume(name, true).unwrap();
                } else {
                    assert_eq!(store.account().id, identities[channel as usize]);
                    assert!(store.volumes().unwrap().iter().any(|volume| volume.name == name));
                    let other = if channel == 0 { "testing" } else { "public" };
                    assert!(!store.volumes().unwrap().iter().any(|volume| volume.name == other));
                }
                assert_eq!(crate::engine::profile::for_profile(&profile).unwrap().name(), name);
                drop(state);
                assert!(unsafe { pb_ios_stop_for_sign_out(handle) }.is_null());
                unsafe { pb_ios_close(handle) };
                let mut invalid = std::ptr::null_mut();
                let result = unsafe { take(pb_ios_open(path.as_ptr(), 1 - channel, &mut invalid)) };
                assert_eq!(result["ok"], false);
                assert!(invalid.is_null());
            }
        }
        assert_ne!(identities[0], identities[1]);
        let path = CString::new(directory.path().join("invalid").to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        assert_eq!(unsafe { take(pb_ios_open(path.as_ptr(), 2, &mut handle)) }["ok"], false);
        assert!(handle.is_null());
        assert!(!directory.path().join("invalid").exists());
    }

    #[test]
    fn corrupt_store_open_reports_reset_code_without_replacing_local_data() {
        // Remove the account record from a valid catalog, then exercise the real bridge.
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        drop(store);
        let catalog = crate::store::open_replica(
            &directory.path().join(crate::paths::PROFILE_DB),
            2 * 1024 * 1024,
        )
        .unwrap();
        let mut tx = catalog.begin_write(1).unwrap();
        tx.del(b"account").unwrap();
        tx.commit().unwrap();
        drop(catalog);
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        let result = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
        assert_eq!(result["code"], "corrupt_store");
        assert!(handle.is_null());
        assert!(directory.path().join(crate::paths::PROFILE_DB).exists());
    }

    #[test]
    fn bridge_keeps_local_identity_and_rejects_invalid_requests() {
        let directory = tempfile::tempdir().unwrap();
        let path = CString::new(directory.path().to_str().unwrap()).unwrap();
        let mut handle = std::ptr::null_mut();
        let opened = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
        assert_eq!(opened["ok"], true);
        assert!(opened["data"].is_null());
        let status = unsafe { request(handle, json!({"op":"status"})) };
        assert_eq!(status["ok"], true);
        let account = status["data"]["account"].clone();
        assert!(!account.is_null());
        assert_eq!(
            unsafe { take(pb_ios_set_display_name(handle, 1, [0xff].as_ptr(), 1)) }["ok"],
            false
        );
        unsafe {
            pb_ios_close(handle);
        }
        let reopened = unsafe { take(pb_ios_open(path.as_ptr(), 0, &mut handle)) };
        assert_eq!(reopened["ok"], true);
        assert!(reopened["data"].is_null());
        let status = unsafe { request(handle, json!({"op":"status"})) };
        assert_eq!(status["ok"], true);
        assert_eq!(account, status["data"]["account"]);
        unsafe {
            pb_ios_close(handle);
        }
    }
}
