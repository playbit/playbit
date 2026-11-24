//! One local preparation at a time, independent of the AP worker.
//! Copies/imports remain durable after disconnect; read-only size work cancels.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

pub(super) struct Task {
    pub token: usize,
    receiver: mpsc::Receiver<std::result::Result<Preparation, String>>,
    cancelled_read: Option<Arc<AtomicBool>>,
    changes_state: bool,
}

pub(super) enum Preparation {
    Reply(Box<Reply>),
    Network(Box<Request>),
}

struct Notify(Arc<crate::engine::completion::Wake>);
impl Drop for Notify {
    fn drop(&mut self) {
        self.0.notify();
    }
}

impl Task {
    pub fn start(engine: &Engine, token: usize, request: Request) -> Result<Self> {
        if !matches!(&request, Request::DuplicateVolume { .. } | Request::DuplicateEntry { .. }) {
            crate::engine::developer::validate(&request)?;
        }
        let build: Option<String> = match &request {
            Request::InstanceCompile { context, .. } => {
                #[cfg(all(not(target_os = "ios"), feature = "process"))]
                {
                    Some(engine.session_instances.descriptor(context.instance)?.build)
                }
                #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
                {
                    let _ = context;
                    return Err("Process compilation admission is unavailable in this build".into());
                }
            }
            _ => None,
        };
        if let Request::AppRemovalImpact { account, app }
        | Request::RemoveApp { account, app, .. } = &request
        {
            app.validate()?;
            if *account != engine.services.store.account().id {
                return Err("The selected app belongs to another account".into());
            }
        }
        let purge =
            matches!(&request, Request::RemoveApp { confirmed: true, delete_local_data: true, .. });
        let embedded = purge.then(|| engine.embedded_sessions.clone());
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        let owners = if purge {
            engine.session_instances.writer_candidates(engine.services.store.account().id)?
        } else {
            Vec::new()
        };
        let changes_state =
            !matches!(request, Request::PayloadSize { .. } | Request::AppRemovalImpact { .. });
        let store = engine.services.store.clone();
        let runtime = engine.runtime.clone();
        let path = engine.services.path.clone();
        let wake = engine.completion.wake.clone();
        let cancelled_read = matches!(request, Request::PayloadSize { .. })
            .then(|| Arc::new(AtomicBool::new(false)));
        let cancelled = cancelled_read.clone();

        let (send, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new().name("local-preparation".into()).spawn(move || {
            let _notify = Notify(wake);
            // Close the sender before notifying, including during unwinding.
            let send = send;
            let result = (|| -> crate::engine::Result<_> {
                use crate::engine::{developer, reply::DeveloperReply};
                if developer::requires_network_trust(&store, &request)? {
                    return Ok(Preparation::Network(Box::new(request)));
                }
                let reply = match request {
                    Request::AppRemovalImpact { app, .. } => {
                        Ok(Reply::AppRemovalImpact { impact: store.app_removal_impact(&app)? })
                    }
                    Request::RemoveApp { app, confirmed, delete_local_data, .. } => {
                        if let Some(embedded) = embedded {
                            // Another device can remove the row before this click arrives.
                            // In that case its still-running windows keep their local data.
                            if !store.preflight_app_erasure(&app)? {
                                store.finish_app_data_removal(&app)?;
                                let removed = crate::store::AppRemovalResult::Removed {
                                    erased_volumes: Vec::new(),
                                    cleanup_error: None,
                                };
                                return Ok(Preparation::Reply(Box::new(Reply::AppRemoved(removed))));
                            }
                            for session in embedded {
                                if store.catalog_session(&session)?.app == app {
                                    return Err("Close the running embedded app before removing its local data".into());
                                }
                            }
                            #[cfg(all(not(target_os = "ios"), feature = "process"))]
                            stop_writers(&path, &store, &app, &owners)?;
                        }
                        Ok(Reply::AppRemoved(store.remove_app(&app, confirmed, delete_local_data)?))
                    }
                    Request::OpenAppVolume { app, role } => {
                        store.open_app_volume(&app, role).map(Reply::Volume).map_err(Into::into)
                    }
                    Request::OpenAutomaticVolume { session, role } => store
                        .open_automatic_volume(&session, role)
                        .map(Reply::Volume)
                        .map_err(Into::into),
                    Request::SignPackage { bundle } => {
                        let build_id = developer::sign_package(&store, &bundle)?;
                        Ok(Reply::Developer(DeveloperReply::PackageSigned { build_id }))
                    }
                    Request::AuthenticatePackage { bundle } => {
                        let package = store.authenticate_package(
                            crate::package::Package::open(&bundle)?,
                            crate::store::now_unix_secs(),
                        )?;
                        Ok(Reply::Developer(developer::package_identity(&store, &package)?))
                    }
                    request
                    @ (Request::DuplicateVolume { .. } | Request::DuplicateEntry { .. }) => {
                        crate::engine::diagnostics::copy(request, &store).map(Reply::Diagnostic)
                    }
                    request @ Request::PayloadSize { volume, entry, .. } => {
                        crate::engine::diagnostics::payload_size(
                            &store,
                            &request.management_access(&store)?,
                            volume,
                            entry,
                            cancelled.as_deref().expect("size work owns cancellation"),
                        )
                        .map(Reply::Diagnostic)
                    }
                    Request::InstanceCompile { fuel_limit, .. } => {
                        let runtime = runtime.for_fuel_limit(fuel_limit)?;
                        let build = build.expect("admitted compilation has an exact build");
                        let source = path.join(crate::paths::BUILDS).join(&build);
                        let package = developer::prepare_app(&store, &runtime, &source)?;
                        if package.id()? != build {
                            return Err("compiled build identity mismatch".into());
                        }
                        Ok(Reply::CompiledModule(runtime.compiled_descriptor(
                            *blake3::hash(&package.wasm()?).as_bytes(),
                            runtime.compatibility(),
                        )))
                    }
                    Request::PrepareApp { bundle } => {
                        let package = developer::cache_package(&store, &runtime, &path, &bundle)?;
                        store.remember_imported_app(&package)?;
                        Ok(Reply::Developer(DeveloperReply::AppPrepared))
                    }
                    Request::CreateSession { bundle, root, user } => {
                        developer::create_session(&store, &runtime, &path, &bundle, root, user)
                            .map(|session| Reply::Developer(DeveloperReply::Session(session)))
                    }
                    _ => Err("expected local preparation".into()),
                }?;
                Ok(Preparation::Reply(Box::new(reply)))
            })()
            .map_err(|error| crate::engine::failure::message(&path, error.as_ref()));
            drop(store);
            let _ = send.send(result);
        })?;
        Ok(Self { token, receiver, cancelled_read, changes_state })
    }

    pub fn changes_state(&self) -> bool {
        self.changes_state
    }

    pub fn cancel_read(&self) {
        if let Some(cancelled) = &self.cancelled_read {
            cancelled.store(true, Ordering::Release);
        }
    }

    pub fn take(&self) -> Option<Preparation> {
        match self.receiver.try_recv() {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(error)) => Some(Preparation::Reply(Box::new(Reply::Error { error }))),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Preparation::Reply(Box::new(Reply::Error {
                    error: "Local preparation worker stopped; accepted work must not be replayed"
                        .into(),
                })))
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancel_read();
        // Shutdown cannot discard accepted durable work. After take(), the
        // sender is already closed and this returns immediately.
        let _ = self.receiver.recv();
    }
}

/// Wait on the authenticated owner's kernel exit descriptor. The Quit reply only
/// acknowledges intent, and the app can still be flushing its last writes.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
fn stop_writers(
    path: &std::path::Path,
    store: &crate::store::LocalStore,
    app: &crate::package::AppIdentity,
    owners: &[crate::engine::session_instance::LaunchOwner],
) -> Result<()> {
    use crate::ui::app_control::{Exchange, Operation, Reply};
    let deadline = Instant::now() + Duration::from_secs(10);
    for owner in owners {
        if store.catalog_session(&owner.instance.session)?.app != *app {
            continue;
        }
        let Some(identity) = &owner.process else { continue };
        let Some(process) = crate::engine::process_owner::Owner::recover(identity)? else {
            continue;
        };
        if Instant::now() >= deadline {
            return Err("Timed out stopping the app. Retry when its windows have closed.".into());
        }
        if !matches!(
            Exchange::connect(path, owner, Operation::Quit)?.receive()?,
            Reply::Accepted { .. }
        ) || !crate::engine::readiness::wait_readable(&process.exit, deadline)?
        {
            return Err("The app is still running. Close it before removing its local data.".into());
        }
    }
    Ok(())
}
