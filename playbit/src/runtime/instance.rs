//! Per-app authority, handles and host-owned state without VM-owned resources.
//!
//! Context validation precedes guest-incarnation allocation. Capability slots
//! are reused with independent generations; event and scene accounting is asserted at
//! operation boundaries. VM memory limits and transport scratch belong to the
//! execution adapter, not to an app instance.

use super::capabilities::{Capabilities, OPERATION};
use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use super::RuntimeError;
use super::syscall::{
    AvailableVolume, GuestContext, Host, HostError, HostResult, Participant, VolumeRights,
};
use playbit_api::abi::syscall::{self as abi, EventKind, Id, Opcode, VolumeHandle};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    /// UI element identity for Click, Submit, Pointer, Key and Frame; zero for
    /// host state notifications.
    pub target: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QueuedEvent {
    pub(super) event: Event,
    pub(super) root: u64,
}
impl QueuedEvent {
    pub(super) fn new(event: Event, root: u64) -> Self {
        Self { event, root }
    }
}
impl std::ops::Deref for QueuedEvent {
    type Target = Event;
    fn deref(&self) -> &Event {
        &self.event
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingUpdate {
    pub generation: u64,
    pub build_id: [u8; 32],
}

pub(super) use super::work::{MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS};

#[derive(Clone)]
pub(super) enum PendingOperation {
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    AudioOpen(std::sync::Arc<super::audio_handles::Open>),
    File(super::syscall::pending_files::Pending),
    FileCopy {
        token: u64,
        destination_entry: [u8; 15],
    },
    FileSelection {
        token: u64,
    },
    Data {
        token: u64,
        volume: VolumeHandle,
        call: std::sync::Arc<super::host::DataCall>,
        take: Opcode,
        bytes: usize,
    },
}

impl PendingOperation {
    pub(super) fn token(&self) -> Option<u64> {
        match self {
            Self::FileCopy { token, .. }
            | Self::FileSelection { token, .. }
            | Self::Data { token, .. } => Some(*token),
            Self::File(pending) => pending.token,
            #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
            Self::AudioOpen(_) => None,
        }
    }
    pub(super) fn ready_kind(&self) -> EventKind {
        match self {
            Self::File(_) => EventKind::DataReady,
            #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
            Self::AudioOpen(_) => EventKind::DataReady,
            Self::FileCopy { .. } => EventKind::FileCopyReady,
            Self::FileSelection { .. } => EventKind::FileSelectionReady,
            Self::Data { call, .. } => match call.operation {
                super::host::DataOperation::Create { .. } => EventKind::ObjectCreationReady,
                _ => EventKind::DataReady,
            },
        }
    }
}

mod lifecycle;
use super::work;
#[cfg(test)]
mod work_tests;

pub(super) struct State {
    pub(super) halted: bool,
    pub(super) tracing: crate::trace::AppScope,
    pub(super) trace_config: Event,
    pub(super) trace_config_pending: bool,
    pub(super) context: GuestContext,
    pub(super) volume_handles: [u64; abi::MAX_VOLUMES],
    pub(super) volume_revision: u64,
    pub(super) volume_watch: Option<u64>,
    pub(super) host: Box<dyn Host>,
    pub(super) files: super::file_handles::Table,
    pub(super) generation: u32,
    pub(super) query_limits: super::host::QueryLimits,
    pub(super) view_bytes_limit: u32,
    pub(super) supported: BTreeSet<Opcode>,
    pub(super) capability_names: Vec<String>,
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    pub(super) audio: super::audio_handles::Table,
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    pub(super) audio_presentation: Option<std::sync::Arc<super::audio::Presentation>>,
    pub(super) events: VecDeque<QueuedEvent>,
    pub(super) work: work::Work,
    pub(super) queued_bytes: usize,
    pub(super) pending_update: Option<PendingUpdate>,
    pub(super) deferred_update: Option<u64>,
    pub(super) wake_deadline: Option<Instant>,
    pub(super) capabilities: Capabilities<PendingOperation>,
    pub(super) channel_listener: Option<u64>,
    pub(super) pending_data_bytes: usize,
    pub(super) logs: Vec<String>,
    pub(super) view: Option<crate::runtime::ViewNode>,
    pub(super) view_bytes: Option<Vec<u8>>,
    pub(super) quit_on_last_window: Option<bool>,
}

fn valid_view_bytes_limit(limit: u32) -> bool {
    limit > 0 && limit as usize <= abi::MAX_CALL_BYTES
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(token) = self.channel_listener.take() {
            self.host.release_data(token);
        }
        for operation in self.capabilities.operations().map(|(_, operation)| operation) {
            match operation {
                PendingOperation::Data { token, .. } => self.host.release_data(*token),
                PendingOperation::File(pending) => {
                    if let Some(token) = pending.token {
                        self.host.release_file(token, false);
                    }
                }
                #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
                PendingOperation::AudioOpen(open) => open.cancel(),
                _ => {}
            }
        }
        for resource in self.files.drain(&mut self.capabilities) {
            if !resource.alias
                && let Some(engine) = resource.engine
            {
                let retention = super::host::files::capacity::Retention {
                    source: Some(resource.credit),
                    result: None,
                };
                // Failure is owned by the host: its retiring context retains
                // cleanup accounting and destroys uncollected capabilities.
                let _ = self.host.discard_file(
                    engine,
                    resource.kind == super::file_handles::Kind::List,
                    retention,
                );
            }
        }
    }
}

impl State {
    // These fields are mutated across different opcode and host-event paths.
    // Check their relationships at control-plane dispatch boundaries, including
    // error returns. Exact byte-accounting recounts also run in the fuzz fixture.
    pub(super) fn assert_invariants(&self) {
        assert!(self.events.len() <= MAX_QUEUED_EVENTS, "event queue exceeded its count limit");
        assert!(self.queued_bytes <= MAX_QUEUED_EVENT_BYTES, "event queue exceeded its byte limit");
        assert!(self.queued_bytes >= 20 * self.events.len(), "each event owns a 20-byte header");
        assert!(
            self.capabilities.count(OPERATION) <= abi::MAX_PENDING_OPERATIONS,
            "pending operation limit"
        );
        assert!(self.pending_data_bytes <= super::host::MAX_RETAINED_DATA_BYTES);
        assert_eq!(self.work.pending.len(), self.capabilities.count(OPERATION));
        assert!(
            self.capabilities.operations().all(|(id, _)| self.work.pending.contains_key(&id.raw()))
        );
        assert!(self.events.iter().all(|event| self.work.remaining.contains_key(&event.root)));
        assert!(self.view_bytes.as_ref().is_none_or(|v| v.len() <= self.view_bytes_limit as usize));
        assert_eq!(
            self.view.is_some(),
            self.view_bytes.is_some(),
            "scene and encoding must move together"
        );
        assert!(
            self.deferred_update.is_none_or(|generation| self
                .pending_update
                .is_some_and(|update| update.generation == generation)),
            "deferral must name the pending update"
        );
    }

    pub(super) fn new(
        context: GuestContext,
        mut host: Box<dyn Host>,
    ) -> Result<Self, RuntimeError> {
        let supported = validate_context(&context, host.as_ref()).map_err(|error| {
            RuntimeError::Instantiation(format!("invalid guest context: {error:?}"))
        })?;
        let (query_limits, view_bytes_limit, capability_names) =
            admit_host_parameters(host.as_ref(), &supported)?;
        let tracing = app_tracing(&context)?;
        let generation = allocate_generation(host.as_mut())?;
        let (capabilities, volume_handles) = admit_volumes(&context.volumes)?;
        let files = admit_files(
            host.as_mut(),
            generation,
            &context.volumes,
            supported.contains(&Opcode::FileOpenBegin),
        )?;
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        let audio_presentation = host
            .supports_audio()
            .then(|| super::audio::Presentation::new(context.identity.instance_id));
        let mut state = State {
            halted: false,
            tracing,
            trace_config: Event { kind: EventKind::TraceConfig, target: 0, data: vec![0; 8] },
            trace_config_pending: false,
            context,
            volume_handles,
            volume_revision: 1,
            volume_watch: None,
            host,
            generation,
            query_limits,
            view_bytes_limit,
            supported,
            capability_names,
            #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
            audio: super::audio_handles::Table::new(),
            #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
            audio_presentation,
            files,
            events: VecDeque::from([QueuedEvent::new(
                Event { kind: EventKind::Started, target: 0, data: Vec::new() },
                1,
            )]),
            work: work::Work::new(),
            queued_bytes: 20,
            pending_update: None,
            deferred_update: None,
            wake_deadline: None,
            capabilities,
            channel_listener: None,
            pending_data_bytes: 0,
            logs: Vec::new(),
            view: None,
            view_bytes: None,
            quit_on_last_window: None,
        };
        crate::runtime::syscall::channels::activate(&mut state);
        Ok(state)
    }
}

fn admit_volumes(
    volumes: &[AvailableVolume],
) -> Result<(Capabilities<PendingOperation>, [u64; abi::MAX_VOLUMES]), RuntimeError> {
    let mut capabilities = Capabilities::new();
    let mut handles = [0; abi::MAX_VOLUMES];
    for (index, volume) in volumes.iter().enumerate() {
        handles[index] = capabilities
            .add_volume(volume.clone())
            .map_err(|error| {
                RuntimeError::Instantiation(format!("volume capability admission: {error:?}"))
            })?
            .raw();
    }
    Ok((capabilities, handles))
}

fn admit_host_parameters(
    host: &dyn Host,
    supported: &BTreeSet<Opcode>,
) -> Result<(super::host::QueryLimits, u32, Vec<String>), RuntimeError> {
    let query_limits = host.query_limits();
    let mut capability_names: Vec<String> =
        supported.iter().map(|opcode| opcode.capability().to_owned()).collect();
    capability_names.extend(host.supported_features().iter().map(|name| (*name).to_owned()));
    query_limits.validate().map_err(|error| {
        RuntimeError::Instantiation(format!("invalid host query limits: {error:?}"))
    })?;
    let view_bytes_limit = host.view_bytes_limit();
    if !valid_view_bytes_limit(view_bytes_limit) {
        return Err(RuntimeError::Instantiation("invalid host view byte limit".into()));
    }
    Ok((query_limits, view_bytes_limit, capability_names))
}

fn allocate_generation(host: &mut dyn Host) -> Result<u32, RuntimeError> {
    let generation = host.allocate_generation().map_err(|error| {
        RuntimeError::Instantiation(format!("could not allocate guest generation: {error:?}"))
    })?;
    if generation == 0 {
        return Err(RuntimeError::Instantiation("guest generation must be a nonzero u32".into()));
    }
    Ok(generation)
}

// Root ownership is established before an instance can run guest code.
fn admit_files(
    host: &mut dyn Host,
    generation: u32,
    volumes: &[AvailableVolume],
    enabled: bool,
) -> Result<super::file_handles::Table, RuntimeError> {
    let mut files = super::file_handles::Table::with_capacity(generation, host.file_capacity());
    let roots = host.file_roots(volumes).map_err(|error| {
        RuntimeError::Instantiation(format!("File root admission failed: {error:?}"))
    })?;
    let admission =
        files.replace_roots(&Capabilities::<PendingOperation>::new(), volumes, &roots, enabled);
    if let Err(error) = admission {
        // All transferred roots, including ones not yet inserted, remain
        // the failed host's cleanup responsibility. No guest can run.
        for root in roots {
            let _ = host.discard_file(
                root.engine,
                false,
                super::host::files::capacity::Retention { source: Some(root.credit), result: None },
            );
        }
        return Err(RuntimeError::Instantiation(format!("Invalid File root batch: {error:?}")));
    }
    Ok(files)
}

/// Validate volume records and their text budget before changing an instance.
pub fn validate_volumes(volumes: &[AvailableVolume]) -> HostResult<()> {
    if volumes.is_empty()
        || volumes.len() > abi::MAX_VOLUMES
        || volumes.iter().filter(|volume| volume.is_root).count() != 1
        || volumes
            .iter()
            .filter(|volume| volume.backing == playbit_api::abi::data::VOLUME_BUNDLE)
            .count()
            > 1
    {
        return Err(HostError::InvalidArgument);
    }
    let mut ids = BTreeSet::new();
    let mut roles = 0u8;
    for volume in volumes {
        if !ids.insert(volume.id)
            || !matches!(
                volume.backing,
                playbit_api::abi::data::VOLUME_PLAYBIT
                    | playbit_api::abi::data::VOLUME_HOST_FS
                    | playbit_api::abi::data::VOLUME_BUNDLE
            )
            || (volume.is_root && volume.backing != playbit_api::abi::data::VOLUME_PLAYBIT)
            || (volume.backing == playbit_api::abi::data::VOLUME_BUNDLE
                && volume.rights != VolumeRights::READ)
            || volume.rights.0 & !VolumeRights::ALL.0 != 0
            || volume.name.is_empty()
            || volume.name.len() > 256
            || volume.tags.len() > 64
            || volume.tags.iter().any(|tag| tag.is_empty() || tag.len() > 128)
        {
            return Err(HostError::InvalidArgument);
        }
        validate_volume_role(volume, &mut roles)?;
    }
    // Records and text have separate caller-owned arrays. The full text must
    // fit its advertised budget so the instance can obtain its volume handles.
    if abi::available_volumes_text_bytes(
        volumes.iter().map(|volume| (volume.name.as_str(), volume.tags.iter().map(String::as_str))),
    )
    .is_none()
    {
        return Err(HostError::LimitExceeded);
    }
    Ok(())
}

fn validate_volume_role(volume: &AvailableVolume, roles: &mut u8) -> HostResult<()> {
    if volume.role == 0 {
        return Ok(());
    }
    let role = playbit_api::abi::data::VolumeRole::from_raw(volume.role)
        .ok_or(HostError::InvalidArgument)?;
    let bit = 1u8 << volume.role;
    if *roles & bit != 0
        || volume.backing != playbit_api::abi::data::VOLUME_PLAYBIT
        || (role == playbit_api::abi::data::VolumeRole::Root) != volume.is_root
    {
        return Err(HostError::InvalidArgument);
    }
    *roles |= bit;
    Ok(())
}

pub(super) fn validate_context(
    context: &GuestContext,
    host: &dyn Host,
) -> HostResult<BTreeSet<Opcode>> {
    validate_volumes(&context.volumes)?;
    if context.startup_arguments.len() > abi::MAX_STARTUP_ARGUMENTS
        || context.required_capabilities.len() > 256
    {
        return Err(HostError::LimitExceeded);
    }
    for (name, value) in &context.startup_arguments {
        if !abi::valid_startup_argument_name(name) || value.len() > abi::MAX_STRING_BYTES {
            return Err(HostError::InvalidArgument);
        }
    }
    if abi::startup_arguments_json_bytes(
        context.startup_arguments.iter().map(|(name, value)| (name.as_str(), value.as_str())),
    )
    .is_none()
    {
        return Err(HostError::LimitExceeded);
    }
    validate_participants(&context.participants)?;
    let mut supported: BTreeSet<_> =
        abi::REGISTRY.iter().map(|(opcode, _)| *opcode).filter(|opcode| opcode.is_core()).collect();
    for opcode in host.supported_operations() {
        if opcode.is_core() || !supported.insert(*opcode) {
            return Err(HostError::InvalidArgument);
        }
    }
    if host.supports_audio() {
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        supported.extend(playbit_api::abi::audio::OPCODES);
    }
    let features = host.supported_features();
    if supported.len() + features.len() > 256
        || features.iter().any(|name| !abi::valid_capability_name(name))
        || features.iter().enumerate().any(|(index, name)| {
            features[..index].contains(name) || supported.iter().any(|op| op.capability() == *name)
        })
    {
        return Err(HostError::InvalidArgument);
    }
    for capability in &context.required_capabilities {
        if !supported.iter().any(|opcode| opcode.capability() == capability)
            && !features.contains(&capability.as_str())
        {
            return Err(HostError::Unsupported);
        }
    }
    Ok(supported)
}

pub(super) fn validate_participants(participants: &[Participant]) -> HostResult<()> {
    if participants.len() > 1024 {
        return Err(HostError::LimitExceeded);
    }
    let mut ids = BTreeSet::new();
    if participants.iter().any(|participant| {
        !ids.insert((participant.account_id, participant.device_id, participant.instance_id))
    }) {
        return Err(HostError::InvalidArgument);
    }
    Ok(())
}

fn app_tracing(context: &GuestContext) -> Result<crate::trace::AppScope, RuntimeError> {
    use crate::{
        package::trace_catalog::{self, Kind},
        trace::{AppNameKind, AppScope},
    };
    let names = if let Some(bytes) = &context.trace_catalog {
        let declarations = trace_catalog::TraceCatalog::decode(bytes).map_err(|error| {
            RuntimeError::Instantiation(format!("invalid app trace declarations: {error}"))
        })?;
        declarations
            .names
            .into_iter()
            .map(|name| {
                let kind = match name.kind {
                    Kind::Span => AppNameKind::Span,
                    Kind::Mark => AppNameKind::Mark,
                };
                (kind, name.name)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let borrowed = names.iter().map(|(kind, name)| (*kind, name.as_str())).collect::<Vec<_>>();
    Ok(AppScope::new(
        &context.app_name,
        crate::trace::AppIdentity {
            session: context.identity.session_id,
            build: context.app_build,
        },
        &borrowed,
        context.identity.instance_id,
    ))
}
