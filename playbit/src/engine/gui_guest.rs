//! One GUI process's serialized guest owner. Storage belongs to the engine;
//! only an immutable, digest-checked package is read from the shared profile.
use super::{
    Request, Result,
    guest_host::{EventEnd, GuestHost, Inbox},
    ipc,
    reply::Reply,
    session_instance::{Admission, Context},
};
use crate::{
    package::Package,
    runtime::{
        Runtime, ViewNode,
        syscall::{
            Event, EventKind, GuestContext, GuestInstance, HostError, Identity, route_input,
        },
    },
    store::Id,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// Reconstruction consumes a target once. A lost admission must never retry it.
#[derive(Debug)]
pub enum RecoveryError {
    AlreadyAttempted,
    DrainPending,
    ContextUnavailable,
}
impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AlreadyAttempted => {
                "This engine incarnation cannot reconstruct the stopped app at this selected \
                 build; uncertain actions were not repeated."
            }
            Self::DrainPending => {
                "Earlier accepted work is still finishing; no action was repeated."
            }
            Self::ContextUnavailable => {
                super::service_events::StopReason::ContextUnavailable.message()
            }
        })
    }
}
impl std::error::Error for RecoveryError {}

pub struct Frame {
    pub view: Option<ViewNode>,
    pub logs: Vec<String>,
    pub encoded: Option<Vec<u8>>,
    pub quit_on_last_window: bool,
}
struct Prepared {
    admission: Admission,
    wasm: Vec<u8>,
    context: GuestContext,
    host: GuestHost,
    inbox: Inbox,
    title: String,
    compiled: Option<crate::runtime::compiled::Image>,
    runtime: Runtime,
    profile: std::path::PathBuf,
    report_startup: bool,
}
impl Prepared {
    fn load_compiled(&mut self) -> Result<()> {
        let _timing = crate::trace::Span::new("guest.compiled.admission");
        let profile = &self.profile;
        let runtime = &self.runtime;
        let admission = &self.admission;
        let mut descriptor = ipc::call_archive(
            profile,
            &Request::InstanceCompiled {
                context: Box::new(admission.context),
                wasm: *blake3::hash(&self.wasm).as_bytes(),
                compatibility: runtime.compatibility(),
            },
        )?
        .into_owned()?;
        let mut compiled = None;
        // A descriptor can race eviction or disk corruption. Ask the same
        // bounded compiler lane to repair once, then retain ordinary Wasm
        // execution if this disposable cache remains unavailable.
        for attempt in 0..2 {
            match descriptor {
                Reply::CompiledModule(Some(ref expected)) => {
                    match super::compiled_cache::read(profile, expected.clone()) {
                        Ok(image) => {
                            compiled = Some(image);
                            break;
                        }
                        Err(error) => eprintln!("Compiled image unavailable: {error}"),
                    }
                }
                Reply::CompiledModule(None) => {}
                _ => return Err("Unexpected compiled image admission reply".into()),
            }
            if attempt == 0 {
                descriptor = ipc::call_archive(
                    profile,
                    &Request::InstanceCompile {
                        context: admission.context,
                        fuel_limit: runtime.fuel_limit(),
                    },
                )?
                .into_owned()?;
            }
        }
        self.compiled = compiled;
        Ok(())
    }

    fn instantiate(self) -> Result<Guest> {
        let Self {
            admission,
            wasm,
            context,
            host,
            inbox,
            title,
            compiled,
            runtime,
            profile,
            report_startup,
        } = self;
        let prepared = match compiled {
            Some(image) => runtime.prepare_compiled(&wasm, image)?,
            None => runtime.prepare_guest(&wasm)?,
        };
        if report_startup {
            startup(
                &profile,
                admission.context,
                None,
                super::session_instance::StartupPhase::Instantiating,
            )?;
        }
        let guest = prepared.instantiate(context, Box::new(host))?;
        Ok(Guest { admission, guest, inbox, scene: None, draining: false, title })
    }
}

#[derive(Clone, Copy, Default)]
pub struct GuestOptions {
    pub fuel_limit: Option<std::num::NonZeroU64>,
    pub renderer: super::GuestRenderer,
}

// Existing callers that supply only a fuel limit use the embedded renderer.
impl From<Option<std::num::NonZeroU64>> for GuestOptions {
    fn from(fuel_limit: Option<std::num::NonZeroU64>) -> Self {
        Self { fuel_limit, ..Self::default() }
    }
}

pub struct Guest {
    admission: Admission,
    guest: GuestInstance,
    inbox: Inbox,
    scene: Option<ViewNode>,
    draining: bool,
    title: String,
}
impl Guest {
    /// Call on the resident guest worker, never AppKit. Claim is explicit: a
    /// failure cannot select another instance or replay an uncertain launch.
    pub fn open(
        profile: &Path,
        instance: Id,
        session: &str,
        root: Id,
        wake: Arc<dyn Fn() + Send + Sync>,
        options: impl Into<GuestOptions>,
    ) -> Result<Self> {
        let admission = claim(profile, instance, session, root)?;
        let package = read_package(profile, &admission.instance.build)?;
        Self::create(profile, admission, package, wake, options.into())
    }
    /// Reconstruction after restart or selected-build adoption. Record
    /// the exact target before admission: a lost reply or failed construction
    /// cannot execute another VM for that target.
    pub fn recover(
        profile: &Path,
        prior: &Admission,
        attempted: &mut (Id, String),
        wake: Arc<dyn Fn() + Send + Sync>,
        options: impl Into<GuestOptions>,
        stop_reason: Option<super::service_events::StopReason>,
    ) -> Result<Self> {
        let Reply::InstanceStatus { incarnation, .. } =
            ipc::call_archive(profile, &Request::InstanceInspect { request: None })?
                .into_owned()?
        else {
            return Err("Unexpected engine recovery-incarnation reply".into());
        };
        if stop_reason == Some(super::service_events::StopReason::ContextUnavailable)
            && incarnation == prior.context.incarnation
        {
            // A stale syscall can precede the ordered selected-build notice.
            // Probe this exact owner independently of catalog visibility/access.
            let Reply::InstanceStopReason { incarnation: observed, reason, .. } =
                ipc::call_archive(
                    profile,
                    &Request::InstanceStopReason { context: prior.context },
                )?
                .into_owned()?
            else {
                return Err("Unexpected stopped-owner classification reply".into());
            };
            if observed != incarnation {
                return Err("The engine changed during app-stop classification; \
                    inspect it again before recovery"
                    .into());
            }
            match reason {
                super::service_events::StopReason::ContextUnavailable => {
                    return Err(RecoveryError::ContextUnavailable.into());
                }
                super::service_events::StopReason::SelectedBuildChanged => {}
            }
        }
        let Reply::SessionPrepared(prepared) = ipc::call_archive(
            profile,
            &Request::PrepareSession { session: prior.instance.session.clone() },
        )?
        .into_owned()?
        else {
            return Err("Unexpected prepared reconstruction reply".into());
        };
        let target = (incarnation, prepared.session.resolved_build_id.clone());
        if target == *attempted {
            return Err(RecoveryError::AlreadyAttempted.into());
        }
        let previous_attempt = std::mem::replace(attempted, target.clone());
        let admission = match ipc::call_archive(
            profile,
            &Request::InstanceReconstruct {
                context: prior.context,
                target: Box::new(super::session_instance::ReconstructionTarget {
                    incarnation,
                    build: target.1.clone(),
                }),
            },
        )?
        .into_owned()?
        {
            Reply::InstanceAdmission(admission) => admission,
            Reply::InstanceDrainPending { .. } => {
                // This typed reply proves ownership was not consumed. Only a
                // later recovery attempt may try after terminal old work.
                *attempted = previous_attempt;
                return Err(RecoveryError::DrainPending.into());
            }
            _ => return Err("Unexpected reconstruction admission reply".into()),
        };
        if !fresh_context(prior.context, admission.context, target.1 != prior.instance.build)
            || admission.context.incarnation != incarnation
            || admission.instance.build != target.1
            || admission.instance.account != prior.instance.account
            || admission.instance.session != prior.instance.session
            || admission.instance.root != prior.instance.root
            || admission.instance.extra_volumes != prior.instance.extra_volumes
            || admission.instance.host_mounts != prior.instance.host_mounts
        {
            return Err(
                "Recovery did not establish a fresh context for this exact session instance".into(),
            );
        }
        let package = read_package(profile, &admission.instance.build)?;
        Self::create(profile, admission, package, wake, options.into())
    }
    fn create(
        profile: &Path,
        admission: Admission,
        package: Package,
        wake: Arc<dyn Fn() + Send + Sync>,
        options: GuestOptions,
    ) -> Result<Self> {
        let context = admission.context;
        startup(profile, context, None, super::session_instance::StartupPhase::Loading)?;
        let result =
            Self::prepare(profile, admission, package, wake, options).and_then(|mut prepared| {
                let inbox = prepared.inbox.clone();
                let result = prepared.load_compiled().and_then(|()| {
                    prepared.report_startup = true;
                    prepared.instantiate()
                });
                result.map_err(|error| inbox.event_end().map_or(error, Into::into))
            });
        if result.as_ref().is_err_and(|error| {
            !matches!(error.downcast_ref::<EventEnd>(), Some(EventEnd::Disconnected(_)))
        }) {
            let _ = startup(profile, context, None, super::session_instance::StartupPhase::Failed);
        }
        result
    }
    fn prepare(
        profile: &Path,
        mut admission: Admission,
        package: Package,
        wake: Arc<dyn Fn() + Send + Sync>,
        options: GuestOptions,
    ) -> Result<Prepared> {
        admission.session_reference.validate()?;
        if admission.context.instance != admission.instance.id
            || admission.context.owner_generation != admission.instance.owner_generation
            || admission.instance.state != super::session_instance::State::Owned
            || admission.session_reference.internal_id() != admission.instance.session
            || admission.session.id != admission.instance.session
            || admission.session.resolved_build_id != admission.instance.build
            || admission.session.root_volume != admission.instance.root
            || !admission
                .volumes
                .iter()
                .any(|volume| volume.is_root && volume.id == *admission.instance.root.as_bytes())
        {
            return Err(
                "The admitted guest context has a different session-instance binding".into()
            );
        }
        if package.id()? != admission.instance.build {
            return Err("The guest package does not match its admitted build".into());
        }
        let title = package.name().to_owned();
        let mut context = GuestContext {
            app_name: title.clone(),
            app_build: Some(super::session_context::decode_session_id(&admission.instance.build)?),
            identity: Identity {
                display_name: admission.display_name.clone(),
                account_id: *admission.instance.account.as_bytes(),
                is_service: false,
                device_id: *admission.instance.account.as_bytes(),
                instance_id: *admission.instance.id.as_bytes(),
                session_id: Some(super::session_context::decode_session_id(
                    &admission.instance.session,
                )?),
            },
            volumes: admission.volumes.clone(),
            startup_arguments: BTreeMap::new(),
            trace_catalog: package.trace().cloned(),
            required_capabilities: package.required_host_features().iter().cloned().collect(),
            participants: vec![],
        };
        let runtime = Runtime::with_fuel_limit(options.fuel_limit)?;
        let wasm = package.wasm()?;
        let (mut host, inbox) = GuestHost::start(profile.to_owned(), admission.context, wake)?;
        host.renderer = options.renderer;
        host.session_key_secret = Some(admission.session_reference.key_secret());
        context.volumes =
            match host.prepare_files(context.volumes, admission.instance.restore_host_mounts) {
                Ok(volumes) => volumes,
                Err(error) => {
                    drop(host);
                    if let Some(end) = inbox.event_end() {
                        return Err(end.into());
                    }
                    return Err(format!("File root preparation failed: {error:?}").into());
                }
            };
        admission.volumes = context.volumes.clone();
        Ok(Prepared {
            admission,
            wasm,
            context,
            host,
            inbox,
            title,
            compiled: None,
            runtime,
            profile: profile.to_owned(),
            report_startup: false,
        })
    }
    pub fn session_identity(&self) -> [u8; 32] {
        super::session_context::decode_session_id(&self.admission.instance.session)
            .expect("admitted session identity was validated")
    }
    pub fn generation(&self) -> u64 {
        self.guest.generation()
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    /// Retain across VM destruction so accepted, not-yet-transmitted File work
    /// reaches the old engine context before reconstruction consumes ownership.
    pub fn file_drain(&self) -> Inbox {
        self.inbox.clone()
    }
    pub fn admission(&self) -> &Admission {
        &self.admission
    }
    pub fn scene(&self) -> Option<&ViewNode> {
        self.scene.as_ref()
    }
    pub fn input(&mut self, event: Event) -> std::result::Result<(), HostError> {
        self.admit_input(event, None)
    }
    pub(crate) fn admit_input(
        &mut self,
        event: Event,
        closing: Option<&mut crate::runtime::work::ClosingInputs>,
    ) -> std::result::Result<(), HostError> {
        if self.draining && closing.is_none() {
            return Err(HostError::Conflict);
        }
        if self.inbox.failure().is_some() {
            return Err(HostError::Unavailable);
        }
        let event = if event.kind == EventKind::SurfaceChanged {
            event
        } else {
            let Some(event) = route_input(self.scene.as_ref().ok_or(HostError::Conflict)?, event)?
            else {
                return Ok(());
            };
            event
        };
        // Surface geometry is checked by the runtime independently of a scene.
        self.guest.queue_input(self.generation(), event, closing)
    }
    /// Readiness injection and guest execution stay on this exact owner thread.
    /// Consuming a completion can begin a dependent read on the same turn.
    pub fn pump(&mut self) -> Result<Frame> {
        if let Some(failure) = self.inbox.failure() {
            return Err(failure.into());
        }
        if let Some(name) = self.inbox.take_display_name() {
            self.guest
                .set_display_name(&name)
                .map_err(|error| format!("Invalid account display name: {error:?}"))?;
            self.admission.display_name = name;
        }
        if let Some(volumes) = self.inbox.take_volumes() {
            self.guest
                .replace_volumes(volumes.clone())
                .map_err(|error| format!("Guest mount adoption failed: {error:?}"))?;
            self.admission.volumes = volumes;
        }
        if self.inbox.take_changed()
            && !self.draining
            && let Err(error) = self.guest.queue_event(
                self.generation(),
                Event { object_id: 0, kind: EventKind::DataChanged, target: 0, data: vec![] },
            )
        {
            self.inbox.defer_changed();
            if error != HostError::LimitExceeded {
                return Err(format!("Guest data-change delivery failed: {error:?}").into());
            }
        }
        for ready in self.inbox.ready() {
            let result = match ready.kind {
                EventKind::ObjectCreationReady => {
                    self.guest.notify_creation(ready.generation, ready.token)
                }
                EventKind::DataReady => self.guest.notify_data(ready.generation, ready.token),
                _ => unreachable!("data host produces only data and creation readiness"),
            };

            if let Err(error) = result {
                self.inbox.defer(ready);
                if !matches!(error, HostError::LimitExceeded | HostError::WouldBlock) {
                    return Err(format!(
                        "Guest completion belongs to a retired operation: {error:?}"
                    )
                    .into());
                }
            }
        }
        let frame = self.guest.pump()?;
        let quit_on_last_window = frame.quit_on_last_window();
        let (logs, view, encoded) = frame.into_parts();
        if let Some(view) = &view {
            // UI and input validation own accepted semantic scenes. Explicit
            // tools may inspect the local encoding; nothing is sent to Engine.
            self.scene = Some(view.clone());
        }
        Ok(Frame { view, logs, encoded, quit_on_last_window })
    }
    pub fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.guest.wake_after(now)
    }
    pub fn ready(&self, now: Instant) -> bool {
        self.inbox.failure().is_some() || self.inbox.has_ready() || self.guest.has_ready_work(now)
    }
    pub fn accepted_work_pending(&self) -> bool {
        self.guest.drain_pending() || !self.inbox.idle()
    }
    pub(crate) fn deactivate_with_inputs(
        &mut self,
        count: usize,
    ) -> std::result::Result<crate::runtime::work::ClosingInputs, HostError> {
        let closing = self.guest.deactivate_with_inputs(count)?;
        self.draining = true;
        Ok(closing)
    }
    pub fn deactivate(&mut self) {
        self.deactivate_with_inputs(0).expect("an empty closing mailbox always fits");
    }
    pub fn drain_failed(&self) -> bool {
        // I/O may finish between these observations. Check waiting first so
        // that a newly completed operation remains visible to has_ready.
        self.guest.drain_failed()
            || self.inbox.failure().is_some()
            || (self.draining
                && self.guest.drain_pending()
                && !self.guest.has_ready_work(Instant::now())
                && !self.inbox.waiting()
                && !self.inbox.has_ready())
    }
    pub fn failure(&self) -> Option<&'static str> {
        self.inbox.failure()
    }
    pub fn stop_reason(&self) -> Option<super::service_events::StopReason> {
        self.inbox.stop_reason()
    }
}

fn fresh_context(prior: Context, next: Context, build_changed: bool) -> bool {
    next.instance == prior.instance
        && (next.incarnation != prior.incarnation || build_changed)
        && next.handle != prior.handle
        && next.owner_generation > prior.owner_generation
}

fn claim(profile: &Path, instance: Id, session: &str, root: Id) -> Result<Admission> {
    let reply = ipc::call_archive(profile, &Request::InstanceClaim { instance })?.into_owned()?;
    let Reply::InstanceAdmission(admission) = reply else {
        return Err("Unexpected session-instance claim reply".into());
    };
    if admission.instance.id != instance
        || admission.context.instance != instance
        || admission.instance.session != session
        || admission.instance.root != root
    {
        return Err("The claimed session instance does not match this launch".into());
    }
    Ok(admission)
}

fn read_package(profile: &Path, build: &str) -> Result<Package> {
    if !crate::identifier::valid(build, 32) {
        return Err("Invalid admitted build identity".into());
    }
    let package = Package::open(&profile.join(crate::paths::BUILDS).join(build))?;
    if package.id()? != build {
        return Err("Guest package content differs from the admitted build".into());
    }
    Ok(package)
}

#[cfg(test)]
#[path = "gui_guest_tests.rs"]
pub(crate) mod tests;

/// The native UI and resident worker report only for their authenticated owner.
pub(crate) fn startup(
    profile: &Path,
    context: Context,
    presentation: Option<u64>,
    phase: super::session_instance::StartupPhase,
) -> Result<()> {
    match ipc::call_archive(profile, &Request::InstanceStartup { context, presentation, phase })?
        .into_owned()?
    {
        Reply::Instance(instance) if instance.id == context.instance => Ok(()),
        _ => Err("Unexpected startup acknowledgment".into()),
    }
}
