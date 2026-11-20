//! Native Mac network ownership: one operation thread, coalesced readiness, and
//! one retained status. App callbacks never execute here or wait for AP replies.
use super::SessionLaunch;
use crate::engine::{
    Services,
    blob::BlobTransport,
    connected::{Connected, Notifications, Stop},
};
use crate::ui::desktop::alert::Failure;
use crate::ui::desktop::sign_in::flow::ProfileChange;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Instant,
};

/// Change enrollment or resolve replacement after closing the account owner.
pub(super) type ChangeProfile<'a> =
    dyn FnMut(&mut Option<Connected>, ProfileChange) -> Result<(), String> + 'a;

// The UI may admit one explicit retry only while an error is offered. A
// coalesced readiness wake cannot lose it, and repeated clicks cannot queue work.
#[derive(Clone)]
pub(super) struct Retry {
    state: Arc<AtomicU8>, // 0 disabled, 1 offered, 2 requested
    wake: mpsc::SyncSender<()>,
}
impl Retry {
    pub fn request(&self) -> bool {
        if self.state.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        let _ = self.wake.try_send(());
        true
    }
    fn take(&self) -> bool {
        self.state.compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
}
pub(super) struct Status {
    pub message: String,
    pub retry: Option<Retry>,
}

pub(super) struct Prepared {
    pub services: Services,
    pub notifications: Notifications,
    pub blobs: BlobTransport,
    pub launches: Vec<SessionLaunch>,
}
pub(super) struct Network {
    stop: Arc<Mutex<Option<Stop>>>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    wake: mpsc::SyncSender<()>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<Option<Status>>>,
    retry: Retry,
    retire: Option<mpsc::SyncSender<()>>,
}
impl Network {
    pub fn start(
        profile: PathBuf,
        app_wake: mpsc::SyncSender<()>,
        prepare: impl FnOnce(
            &mut Option<Connected>,
            &mut ChangeProfile<'_>,
        ) -> Result<Vec<SessionLaunch>, String>
        + Send
        + 'static,
    ) -> std::io::Result<(Self, mpsc::Receiver<Result<Prepared, Failure>>)> {
        let (wake, receive) = mpsc::sync_channel(1);
        let (send, prepared) = mpsc::sync_channel(1);
        let retry = Retry { state: Arc::new(AtomicU8::new(0)), wake: wake.clone() };
        let shared_retry = retry.clone();
        let (retire, retired) = mpsc::sync_channel(1);
        let stop = Arc::new(Mutex::new(None));
        let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let status = Arc::new(Mutex::new(None));
        let signal = wake.clone();
        let shared_stop = stop.clone();
        let shared_stopping = stopping.clone();
        let shared_status = status.clone();
        let thread =
            std::thread::Builder::new().name("native-network".into()).spawn(move || {
                let connected = match open(
                    profile.clone(),
                    signal.clone(),
                    app_wake.clone(),
                    &shared_stop,
                    &shared_stopping,
                ) {
                    Ok(owner) => owner,
                    Err(error) => {
                        let _ = send.send(Err(error.into()));
                        return;
                    }
                };
                let _recorder = crate::trace::Recorder::for_profile(
                    connected.services.profile_path(),
                    &(connected.services.profile_path()).join(crate::paths::TRACE),
                );
                let mut connected = Some(connected);
                let mut committed_change = None;
                let mut change = |owner: &mut Option<Connected>, change: ProfileChange| {
                    change_profile(&profile, owner, change, &mut committed_change, || {
                        open(
                            profile.clone(),
                            signal.clone(),
                            app_wake.clone(),
                            &shared_stop,
                            &shared_stopping,
                        )
                    })
                };
                let launches = match prepare(&mut connected, &mut change) {
                    Ok(launches) => launches,
                    Err(error) => {
                        let _ = send.send(Err(error.into()));
                        return;
                    }
                };
                let Some(mut connected) = connected else {
                    let _ = send.send(Err("Account profile reset did not finish".into()));
                    return;
                };
                let ready = Prepared {
                    services: connected.services.clone(),
                    notifications: connected.notifications(),
                    blobs: connected.blob_transport(),
                    launches,
                };
                if send.send(Ok(ready)).is_err() {
                    return;
                }
                drive(
                    &mut connected,
                    receive,
                    &shared_stopping,
                    &shared_status,
                    &app_wake,
                    &shared_retry,
                );
                // The app worker releases callbacks and blob handles before the
                // network runtime and its completion callback are destroyed.
                let _ = retired.recv();
            })?;
        Ok((
            Self {
                stop,
                stopping,
                wake,
                thread: Some(thread),
                status,
                retry,
                retire: Some(retire),
            },
            prepared,
        ))
    }
    pub fn status_handle(&self) -> Arc<Mutex<Option<Status>>> {
        self.status.clone()
    }
    pub fn request_stop(&self) {
        self.retry.state.store(0, Ordering::Release);
        self.stopping.store(true, std::sync::atomic::Ordering::Release);
        if let Some(stop) = self.stop.lock().expect("network stop gate is not poisoned").as_ref() {
            stop.request();
        }
        let _ = self.wake.try_send(());
    }
    pub fn shutdown(&mut self) {
        self.request_stop();
        if let Some(retire) = self.retire.take() {
            let _ = retire.try_send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("native network owner must not panic");
        }
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        self.shutdown();
    }
}
// Retain a completed decision until its new owner opens. A failed reopen must
// retry only opening, never repeat a committed change or authorize another choice.
pub(super) fn change_profile(
    profile: &std::path::Path,
    owner: &mut Option<Connected>,
    change: ProfileChange,
    committed_change: &mut Option<ProfileChange>,
    open: impl FnOnce() -> Result<Connected, String>,
) -> Result<(), String> {
    if let Some(committed) = *committed_change {
        if change != committed {
            crate::platform::failure::log(
                profile,
                "reopen changed sign-in profile",
                &format!("requested={change:?} committed={committed:?}"),
            );
            return Err(committed.failure_message(true).into());
        }
    } else {
        if let Some(old) = owner.take() {
            old.stop_handle().request();
            drop(old);
        }
        // All app/store handles are created only after setup finishes.
        let result = match change {
            ProfileChange::Replace(replace) => {
                crate::engine::sign_out::resolve_replacement(profile, replace)
            }
            ProfileChange::Reset => crate::engine::sign_out::resume(profile)
                .and_then(|()| crate::engine::sign_out::remove_profile(profile)),
            ProfileChange::SignOut => crate::engine::sign_out::keep_profile(profile),
        };
        if let Err(error) = result {
            crate::platform::failure::log(
                profile,
                "change sign-in profile",
                &format!("change={change:?} error={error:?}"),
            );
            return Err(change.failure_message(false).into());
        }
        *committed_change = Some(change);
    }
    *owner = Some(open().map_err(|error| {
        crate::platform::failure::log(
            profile,
            "reopen changed sign-in profile",
            &format!("change={change:?} error={error}"),
        );
        change.failure_message(true).to_owned()
    })?);
    *committed_change = None;
    Ok(())
}

fn open(
    profile: PathBuf,
    network_wake: mpsc::SyncSender<()>,
    app_wake: mpsc::SyncSender<()>,
    stop: &Mutex<Option<Stop>>,
    stopping: &std::sync::atomic::AtomicBool,
) -> Result<Connected, String> {
    let connected = Connected::open(profile, move || {
        let _ = network_wake.try_send(());
        let _ = app_wake.try_send(());
    })
    .map_err(|error| error.to_string())?;
    let mut stop = stop.lock().expect("network stop gate is not poisoned");
    *stop = Some(connected.stop_handle());
    if stopping.load(std::sync::atomic::Ordering::Acquire) {
        connected.stop_handle().request();
    }
    Ok(connected)
}

fn drive(
    connected: &mut Connected,
    receive: mpsc::Receiver<()>,
    stopping: &std::sync::atomic::AtomicBool,
    status: &Mutex<Option<Status>>,
    app_wake: &mpsc::SyncSender<()>,
    retry: &Retry,
) {
    // Bootstrap is explicit and starts once, independently of the first local
    // scene. Further pulls are only retries, reconnects or gap recovery.
    *status.lock().expect("network status gate is not poisoned") =
        Some(Status { message: "Connecting…".into(), retry: None });
    let _ = app_wake.try_send(());
    let mut result = if connected.enrolled() {
        connected.synchronize().map(|()| true)
    } else {
        Err("Sign in to sync your changes".into())
    };
    let mut previous = String::new();
    let mut error = None;
    while !stopping.load(std::sync::atomic::Ordering::Acquire) {
        match result {
            Ok(true) => error = None,
            Ok(false) => {}
            Err(failure) => error = Some(failure.to_string().chars().take(512).collect::<String>()),
        }
        let message = match &error {
            Some(error) => format!("Sync unavailable: {error}. Local changes are saved."),
            None => connected.availability().to_owned(),
        };
        if message != previous {
            previous = message.clone();
            let offered = error.is_some() && connected.enrolled();
            retry.state.store(u8::from(offered), Ordering::Release);
            *status.lock().expect("network status gate is not poisoned") =
                Some(Status { message, retry: offered.then(|| retry.clone()) });
            let _ = app_wake.try_send(());
        }
        match connected.wake_after(Instant::now()) {
            Some(delay) => {
                let _ = receive.recv_timeout(delay);
            }
            None => {
                let _ = receive.recv();
            }
        }
        if stopping.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        result = if retry.take() {
            crate::trace::mark("replication.explicit_retry");
            previous = "Retrying sync…".into();
            *status.lock().expect("network status gate is not poisoned") =
                Some(Status { message: previous.clone(), retry: None });
            let _ = app_wake.try_send(());
            connected.synchronize().map(|()| true)
        } else {
            connected.advance(Instant::now())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_retry_survives_coalesced_wake_and_admits_only_one_request() {
        let (wake, receive) = mpsc::sync_channel(1);
        let retry = Retry { state: Arc::new(AtomicU8::new(0)), wake };
        assert!(!retry.request());
        retry.state.store(1, Ordering::Release);
        retry.wake.try_send(()).expect("fill with unrelated readiness");
        assert!(retry.request());
        for _ in 0..32 {
            assert!(!retry.request());
        }
        receive.try_recv().expect("original wake retained");
        assert!(retry.take());
        assert!(!retry.take());
        assert!(!retry.request());
        retry.state.store(1, Ordering::Release); // Another failed operation.
        assert!(retry.request());
        retry.state.store(0, Ordering::Release); // Shutdown retires admission.
        assert!(!retry.take());
        assert!(!retry.request());
    }
}
