//! Host-owned guest lifecycle shared by native and headless hosts.
//!
//! Calls run on the host's serialized worker, never on its UI thread. A return
//! from `pump` is a safe boundary. A deadline-poll guest may retain a suspended
//! Wasm stack; dropping that guest cancels the wait without running old code.
//! Pending host work may outlive a turn; its handles and callbacks belong to the
//! guest generation that began it and never transfer into a replacement guest.
//! Candidate startup can have durable effects; recovery restores the previous
//! executable and presentation, and does not claim to undo those effects.

use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use crate::runtime::syscall::{
    Event, GuestContext, GuestInstance, Host, HostError, Participant, PendingUpdate,
};
use crate::runtime::{Frame, Runtime, RuntimeError, ViewNode};

const MAX_LOG_ENTRIES: usize = 256;
const MAX_LOG_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReloadStage {
    Preparation,
    Startup,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReloadFailure {
    pub stage: ReloadStage,
    pub message: String,
}

pub struct ManagedGuest {
    runtime: Runtime,
    context: GuestContext,
    active: GuestInstance,
    view: Option<ViewNode>,
    view_bytes: Option<Vec<u8>>,
    logs: VecDeque<String>,
    log_bytes: usize,
    log_generation: u64,
    log_sequence: u64,
    reload_failure: Option<ReloadFailure>,
    validate_view: fn(&ViewNode) -> Result<(), String>,
}

impl ManagedGuest {
    pub fn create(
        runtime: Runtime,
        wasm: &[u8],
        context: GuestContext,
        host: Box<dyn Host>,
        validate_view: fn(&ViewNode) -> Result<(), String>,
    ) -> Result<Self, RuntimeError> {
        let mut active = runtime.create_guest(wasm, context.clone(), host)?;
        let frame = active.pump()?;
        let mut managed = Self {
            runtime,
            context,
            active,
            view: None,
            view_bytes: None,
            logs: VecDeque::new(),
            log_bytes: 0,
            log_generation: 0,
            log_sequence: 0,
            reload_failure: None,
            validate_view,
        };
        managed.accept(frame, managed.generation())?;
        Ok(managed)
    }

    pub fn generation(&self) -> u64 {
        self.active.generation()
    }

    pub fn context(&self) -> &GuestContext {
        &self.context
    }

    pub fn view(&self) -> Option<&ViewNode> {
        self.view.as_ref()
    }

    /// Encoding of the accepted scene, never a rejected candidate submission.
    pub fn view_bytes(&self) -> Option<&[u8]> {
        self.view_bytes.as_deref()
    }

    /// Native presentation reads only references in the accepted scene. This
    /// never gives a renderer ambient access to arbitrary objects or volumes.
    pub fn read_file(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
        max_bytes: usize,
    ) -> Result<([u8; 32], Vec<u8>), HostError> {
        self.require_scene_file(generation, file)?;
        self.active.read_file(generation, file, max_bytes)
    }

    /// Metadata-only current authorization for a cached presentation image.
    pub fn file_descriptor(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
    ) -> Result<(u64, [u8; 32]), HostError> {
        self.require_scene_file(generation, file)?;
        self.active.file_descriptor(generation, file)
    }

    fn require_scene_file(
        &self,
        generation: u64,
        file: playbit_api::ui::FileReference,
    ) -> Result<(), HostError> {
        fn contains(view: &ViewNode, file: playbit_api::ui::FileReference) -> bool {
            match view {
                ViewNode::FileImage { file: candidate, .. } => *candidate == file,
                ViewNode::Theme { child, .. }
                | ViewNode::ContextMenu { child, .. }
                | ViewNode::Scroll { child, .. } => contains(child, file),
                ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
                    children.iter().any(|child| contains(child, file))
                }
                _ => false,
            }
        }
        if generation != self.generation() {
            return Err(HostError::StaleHandle);
        }
        if !self.view.as_ref().is_some_and(|view| contains(view, file)) {
            return Err(HostError::AccessDenied);
        }
        Ok(())
    }

    pub fn logs(&self) -> &VecDeque<String> {
        &self.logs
    }

    /// Accepted log sequence within the current guest generation. Replacement
    /// resets this cursor; the ordinary local logs() ring retains its history.
    pub fn log_sequence(&self) -> u64 {
        self.log_sequence
    }

    /// Real accepted entries, never inferred by comparing possibly repeated
    /// strings. Eviction is visible in the first returned sequence number.
    pub fn current_log_entries(&self) -> impl Iterator<Item = (u64, &str)> {
        let count = self.logs.len().min(usize::try_from(self.log_sequence).unwrap_or(usize::MAX));
        let first = (self.log_sequence - count as u64).saturating_add(1);
        self.logs
            .iter()
            .skip(self.logs.len() - count)
            .enumerate()
            .map(move |(index, text)| (first + index as u64, text.as_str()))
    }

    pub fn reload_failure(&self) -> Option<&ReloadFailure> {
        self.reload_failure.as_ref()
    }

    /// Caller validates the immutable bundle's app, target, host, and digest.
    /// Runtime identity, session, mount choices, and startup arguments cannot be
    /// changed here. A different execution context requires a new host instance.
    pub fn replace(
        &mut self,
        wasm: &[u8],
        trace_catalog: Option<std::sync::Arc<[u8]>>,
        required_capabilities: BTreeSet<String>,
        host: Box<dyn Host>,
    ) -> Result<(), RuntimeError> {
        let mut context = self.context.clone();
        context.trace_catalog = trace_catalog;
        context.required_capabilities = required_capabilities;
        let prepared = self.runtime.create_guest(wasm, context.clone(), host);
        let mut candidate = match prepared {
            Ok(candidate) => candidate,
            Err(error) => {
                self.reload_failure = Some(ReloadFailure {
                    stage: ReloadStage::Preparation,
                    message: error.to_string(),
                });
                return Err(error);
            }
        };
        let frame = match candidate.pump() {
            Ok(frame) => frame,
            Err(error) => {
                self.reload_failure = Some(ReloadFailure {
                    stage: ReloadStage::Startup,
                    message: format!(
                        "{error}. The previous app is retained; writes made during startup may have persisted."
                    ),
                });
                return Err(error);
            }
        };
        let has_view = frame.view.is_some();
        if let Err(error) = self.accept(frame, candidate.generation()) {
            self.reload_failure = Some(ReloadFailure {
                stage: ReloadStage::Startup,
                message: format!(
                    "{error}. The previous app is retained; writes made during startup may have persisted."
                ),
            });
            return Err(error);
        }
        self.active = candidate;
        self.context = context;
        if !has_view {
            self.view = None;
            self.view_bytes = None;
        }
        self.reload_failure = None;
        Ok(())
    }

    pub fn pump(&mut self) -> Result<(), RuntimeError> {
        let frame = self.active.pump()?;
        self.accept(frame, self.active.generation())
    }

    pub fn wake_due(&self, now: Instant) -> bool {
        self.active.wake_due(now)
    }

    pub fn wake_after(&self, now: Instant) -> Option<std::time::Duration> {
        self.active.wake_after(now)
    }

    pub fn queue_event(&mut self, generation: u64, event: Event) -> Result<(), HostError> {
        self.active.queue_event(generation, event)
    }

    pub fn notify_data(&mut self, generation: u64, token: u64) -> Result<(), HostError> {
        self.active.notify_data(generation, token)
    }

    pub fn notify_creation(&mut self, generation: u64, token: u64) -> Result<(), HostError> {
        self.active.notify_creation(generation, token)
    }

    pub fn notify_file_copy(&mut self, generation: u64, token: u64) -> Result<(), HostError> {
        self.active.notify_file_copy(generation, token)
    }

    pub fn notify_file_selection(&mut self, generation: u64, token: u64) -> Result<(), HostError> {
        self.active.notify_file_selection(generation, token)
    }

    pub fn notify_update(
        &mut self,
        generation: u64,
        update: PendingUpdate,
    ) -> Result<(), HostError> {
        self.active.notify_update(generation, update)
    }

    pub fn deferred_update(&self) -> Option<u64> {
        self.active.deferred_update()
    }

    pub fn set_participants(
        &mut self,
        generation: u64,
        participants: Vec<Participant>,
    ) -> Result<(), HostError> {
        self.active.set_participants(generation, participants.clone())?;
        self.context.participants = participants;
        Ok(())
    }

    fn accept(&mut self, frame: Frame, generation: u64) -> Result<(), RuntimeError> {
        if let Some(view) = frame.view.as_ref() {
            (self.validate_view)(view).map_err(RuntimeError::InvalidView)?;
        }
        let previous = if self.log_generation == generation { self.log_sequence } else { 0 };
        let sequence = previous
            .checked_add(frame.logs.len() as u64)
            .ok_or_else(|| RuntimeError::Engine("accepted log sequence exhausted".into()))?;
        self.log_generation = generation;
        self.log_sequence = sequence;
        for entry in frame.logs {
            self.log_bytes += entry.len();
            self.logs.push_back(entry);
        }
        while self.logs.len() > MAX_LOG_ENTRIES || self.log_bytes > MAX_LOG_BYTES {
            if let Some(entry) = self.logs.pop_front() {
                self.log_bytes -= entry.len();
            }
        }
        if frame.view.is_some() {
            self.view = frame.view;
            self.view_bytes = frame.view_bytes;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::runtime::ExecutionBackend;
    use crate::runtime::syscall::{AvailableVolume, EventKind, Identity, Opcode, VolumeRights};

    struct NoData;
    impl Host for NoData {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
    }

    fn context() -> GuestContext {
        GuestContext {
            app_name: "Test app".into(),
            app_build: None,
            identity: Identity {
                account_id: [1; 16],
                is_service: false,
                device_id: [2; 16],
                instance_id: [5; 16],
                session_id: Some([6; 32]),
            },
            volumes: vec![AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [7; 16],
                name: "Development".into(),
                tags: vec![],
                rights: VolumeRights::READ,
                is_root: true,
                role: 0,
            }],
            startup_arguments: [("mode".into(), "test".into())].into(),
            trace_catalog: None,
            required_capabilities: BTreeSet::new(),
            participants: vec![],
        }
    }

    fn guest(message: &str, trap: bool) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
            (import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
            (memory (export "memory") 1)
            (data (i32.const 8) "{message}")
            (func (export "pb_main")
                (drop (call $log (i32.const 8) (i32.const {})))
                {}))"#,
            message.len(),
            if trap { "unreachable" } else { "" }
        ))
        .unwrap()
    }

    fn view_guest(text: &str) -> Vec<u8> {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.text(playbit_api::ui::TextStyle::Title, text);
        encoder.column_end();
        let bytes = encoder.finish();
        let encoded: String = bytes.iter().map(|byte| format!("\\{byte:02x}")).collect();
        wat::parse_str(format!(
            r#"(module
            (import "playbit" "pb_syscall" (func $dispatch_view (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $view (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_view (i32.const 3)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
            (memory (export "memory") 1)
            (data (i32.const 8) "{encoded}")
            (func (export "pb_main")
                (drop (call $view (i32.const 8) (i32.const {})))))"#,
            bytes.len()
        ))
        .unwrap()
    }

    #[test]
    fn accepted_log_cursors_count_repeated_entries_and_reset_only_after_replacement() {
        for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
            let mut managed = ManagedGuest::create(
                Runtime::with_backend(backend).unwrap(),
                &guest("same", false),
                context(),
                Box::new(NoData),
                |_| Ok(()),
            )
            .unwrap();
            for _ in 1..300 {
                managed.pump().unwrap();
            }
            assert_eq!(managed.log_sequence(), 300);
            let entries: Vec<_> = managed.current_log_entries().collect();
            assert_eq!(entries.len(), MAX_LOG_ENTRIES);
            assert_eq!(entries.first(), Some(&(45, "same")));
            assert_eq!(entries.last(), Some(&(300, "same")));
            let generation = managed.generation();
            assert!(
                managed
                    .replace(&guest("failed", true), None, BTreeSet::new(), Box::new(NoData))
                    .is_err()
            );
            assert_eq!(managed.generation(), generation);
            assert_eq!(managed.log_sequence(), 300);
            assert!(!managed.logs().iter().any(|text| text == "failed"));
            managed
                .replace(&guest("same", false), None, BTreeSet::new(), Box::new(NoData))
                .unwrap();
            assert_ne!(managed.generation(), generation);
            assert_eq!(managed.log_sequence(), 1);
            assert_eq!(managed.current_log_entries().collect::<Vec<_>>(), vec![(1, "same")]);
            assert_eq!(managed.logs().len(), MAX_LOG_ENTRIES);
        }
    }

    #[test]
    fn unsupported_presentation_is_rejected_before_replacement() {
        fn validate(view: &ViewNode) -> Result<(), String> {
            match view {
                ViewNode::Theme { child, .. } => validate(child),
                ViewNode::Column(children) => {
                    for child in children {
                        validate(child)?;
                    }
                    Ok(())
                }
                ViewNode::Text { text, .. } if text == "supported" => Ok(()),
                _ => Err("unsupported control".into()),
            }
        }
        let mut managed = ManagedGuest::create(
            Runtime::new().unwrap(),
            &view_guest("supported"),
            context(),
            Box::new(NoData),
            validate,
        )
        .unwrap();
        let previous_view = managed.view().cloned();
        let previous_bytes = managed.view_bytes().unwrap().to_vec();
        assert_eq!(
            crate::runtime::decode_view(&previous_bytes).unwrap(),
            previous_view.clone().unwrap()
        );
        let previous_generation = managed.generation();
        assert!(matches!(
            managed.replace(&view_guest("rejected"), None, BTreeSet::new(), Box::new(NoData)),
            Err(RuntimeError::InvalidView(_))
        ));
        assert_eq!(managed.generation(), previous_generation);
        assert_eq!(managed.view(), previous_view.as_ref());
        assert_eq!(managed.view_bytes(), Some(previous_bytes.as_slice()));
        managed.pump().unwrap();
        managed
            .replace(&guest("without view", false), None, BTreeSet::new(), Box::new(NoData))
            .unwrap();
        assert!(managed.view().is_none());
        assert!(managed.view_bytes().is_none());
    }

    #[test]
    fn replacement_preserves_host_context_and_rejects_old_callbacks_on_both_backends() {
        for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
            let runtime = Runtime::with_backend(backend).unwrap();
            let original = context();
            let mut managed = ManagedGuest::create(
                runtime,
                &guest("first", false),
                original.clone(),
                Box::new(NoData),
                |_| Ok(()),
            )
            .unwrap();
            let first_generation = managed.generation();
            managed
                .queue_event(
                    first_generation,
                    Event { kind: EventKind::Click, target: 1, data: vec![] },
                )
                .unwrap();
            managed
                .replace(&guest("second", false), None, BTreeSet::new(), Box::new(NoData))
                .unwrap();
            assert_ne!(managed.generation(), first_generation);
            assert_eq!(managed.context().identity, original.identity);
            assert_eq!(managed.context().volumes, original.volumes);
            assert_eq!(managed.context().startup_arguments, original.startup_arguments);
            assert!(managed.context().trace_catalog.is_none());
            assert_eq!(
                managed.queue_event(
                    first_generation,
                    Event { kind: EventKind::Click, target: 1, data: vec![] }
                ),
                Err(HostError::StaleHandle)
            );
            assert_eq!(managed.logs().back().unwrap(), "second");
        }
    }

    #[test]
    fn validation_and_startup_failures_keep_previous_executable_usable() {
        for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
            let runtime = Runtime::with_backend(backend).unwrap();
            let mut managed = ManagedGuest::create(
                runtime,
                &guest("running", false),
                context(),
                Box::new(NoData),
                |_| Ok(()),
            )
            .unwrap();
            let generation = managed.generation();
            assert!(managed.replace(b"not wasm", None, BTreeSet::new(), Box::new(NoData)).is_err());
            assert_eq!(managed.reload_failure().unwrap().stage, ReloadStage::Preparation);
            assert_eq!(managed.generation(), generation);
            managed.pump().unwrap();
            assert_eq!(managed.logs().back().unwrap(), "running");
            assert!(
                managed
                    .replace(&guest("candidate", true), None, BTreeSet::new(), Box::new(NoData))
                    .is_err()
            );
            assert_eq!(managed.reload_failure().unwrap().stage, ReloadStage::Startup);
            assert!(managed.reload_failure().unwrap().message.contains("may have persisted"));
            assert_eq!(managed.generation(), generation);
            managed.pump().unwrap();
            assert_eq!(managed.logs().back().unwrap(), "running");
        }
    }
}
