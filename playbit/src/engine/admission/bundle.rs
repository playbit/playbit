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
                        developer::cache_package(&store, &runtime, &path, &bundle)?;
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
        Ok(Self { token, receiver, cancelled_read })
    }

    pub fn changes_state(&self) -> bool {
        self.cancelled_read.is_none()
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
