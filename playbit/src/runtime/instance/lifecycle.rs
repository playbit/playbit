//! Instance lifecycle shared by VM and native execution adapters.
use super::*;
use crate::runtime::Frame;
use crate::runtime::syscall::{
    MAX_PRESENTATION_FILE_BYTES, file_descriptor, push_event, queue_due_wake,
};
use playbit_api::abi::{files::MAX_FILE_READ, syscall::Writer};
use std::time::Duration;

impl State {
    pub(in crate::runtime) fn generation(&self) -> u64 {
        u64::from(self.generation)
    }

    pub(in crate::runtime) fn volume_handle(&self, id: Id) -> Option<VolumeHandle> {
        self.capabilities
            .volumes()
            .find(|(_, volume)| volume.id == id)
            .map(|(handle, _)| crate::runtime::syscall::handles::volume_handle(handle.raw()))
    }

    pub(in crate::runtime) fn replace_volumes(
        &mut self,
        volumes: Vec<AvailableVolume>,
    ) -> HostResult<bool> {
        let _timing = crate::trace::Span::new("guest.volumes.replace");
        validate_volumes(&volumes)?;
        let state = self;
        if state.context.volumes.iter().find(|v| v.is_root).map(|v| v.id)
            != volumes.iter().find(|v| v.is_root).map(|v| v.id)
        {
            return Err(HostError::InvalidArgument);
        }
        let next_revision = state.volume_revision.checked_add(1).ok_or(HostError::LimitExceeded)?;
        let metadata_changed = state.context.volumes != volumes;
        let (slots, added) = state.admit_volume_handles(&volumes)?;
        let retired = match state.replace_file_roots(&volumes) {
            Ok(retired) => retired,
            Err(error) => {
                state.cancel_volume_handles(added);
                return Err(error);
            }
        };
        let changed = metadata_changed || !retired.is_empty();
        let mut cleanup_error = None;
        for resource in retired {
            if let Some(engine) = resource.engine
                && let Err(error) = state.host.discard_file(
                    engine,
                    false,
                    super::super::host::files::capacity::Retention {
                        source: Some(resource.credit),
                        result: None,
                    },
                )
            {
                state.halted = true;
                cleanup_error = Some(error);
            }
        }
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        if changed {
            state.volume_revision = next_revision;
        }
        for raw in &state.volume_handles[..state.context.volumes.len()] {
            if !slots[..volumes.len()].contains(raw) {
                state
                    .capabilities
                    .table
                    .remove(crate::capability::Handle(*raw), super::super::capabilities::VOLUME)
                    .expect("retired volume owns its capability");
            }
        }
        for (raw, volume) in slots.iter().zip(&volumes) {
            state
                .capabilities
                .replace_volume(crate::capability::Handle(*raw), volume.clone())
                .expect("existing volume owns its capability");
        }
        state.context.volumes = volumes;
        state.volume_handles = slots;
        super::super::syscall::control::queue_volume_change(state);
        Ok(changed)
    }

    fn admit_volume_handles(
        &mut self,
        volumes: &[AvailableVolume],
    ) -> HostResult<([u64; abi::MAX_VOLUMES], Vec<crate::capability::Handle>)> {
        let mut slots = [0; abi::MAX_VOLUMES];
        let mut added = Vec::new();
        for (index, volume) in volumes.iter().enumerate() {
            if let Some(previous) = self.context.volumes.iter().position(|v| v.id == volume.id) {
                slots[index] = self.volume_handles[previous];
            } else {
                match self.capabilities.add_volume(volume.clone()) {
                    Ok(handle) => {
                        slots[index] = handle.raw();
                        added.push(handle);
                    }
                    Err(error) => {
                        self.cancel_volume_handles(added);
                        return Err(error);
                    }
                }
            }
        }
        Ok((slots, added))
    }

    fn cancel_volume_handles(&mut self, handles: Vec<crate::capability::Handle>) {
        for handle in handles {
            self.capabilities
                .table
                .remove(handle, super::super::capabilities::VOLUME)
                .expect("new volume owns its capability");
        }
    }

    fn replace_file_roots(
        &mut self,
        volumes: &[AvailableVolume],
    ) -> HostResult<Vec<super::super::file_handles::Resource>> {
        let roots = self.host.file_roots(volumes)?;
        match self.files.replace_roots(
            &self.capabilities,
            volumes,
            &roots,
            self.supported.contains(&Opcode::FileOpenBegin),
        ) {
            Ok(retired) => Ok(retired),
            Err(error) => {
                let mut discarded = std::collections::BTreeSet::new();
                for root in roots {
                    if root.engine != 0
                        && !self.files.owns_engine(&self.capabilities, root.engine)
                        && discarded.insert(root.engine)
                        && self
                            .host
                            .discard_file(
                                root.engine,
                                false,
                                super::super::host::files::capacity::Retention {
                                    source: Some(root.credit),
                                    result: None,
                                },
                            )
                            .is_err()
                    {
                        self.halted = true;
                    }
                }
                Err(error)
            }
        }
    }

    pub(in crate::runtime) fn queue_event(
        &mut self,
        generation: u64,
        event: Event,
    ) -> HostResult<()> {
        self.require_running_generation(generation)?;
        if matches!(
            event.kind,
            EventKind::Started
                | EventKind::UpdateAvailable
                | EventKind::ObjectCreationReady
                | EventKind::FileCopyReady
                | EventKind::FileSelectionReady
                | EventKind::DataReady
                | EventKind::Wake
        ) {
            return Err(HostError::InvalidArgument);
        }
        if event.kind == EventKind::SurfaceChanged {
            if event.target != 0 {
                return Err(HostError::InvalidArgument);
            }
            playbit_api::abi::environment::Surface::decode(&event.data)?;
        }
        push_event(self, event)
    }

    pub(in crate::runtime) fn notify_creation(
        &mut self,
        generation: u64,
        token: u64,
    ) -> HostResult<()> {
        self.notify_operation(generation, token, EventKind::ObjectCreationReady)
    }

    pub(in crate::runtime) fn notify_data(
        &mut self,
        generation: u64,
        token: u64,
    ) -> HostResult<()> {
        self.require_running_generation(generation)?;
        if crate::runtime::syscall::channels::receive(self, token)? {
            return Ok(());
        }
        self.notify_operation(generation, token, EventKind::DataReady)
    }

    pub(in crate::runtime) fn notify_file_copy(
        &mut self,
        generation: u64,
        token: u64,
    ) -> HostResult<()> {
        self.notify_operation(generation, token, EventKind::FileCopyReady)
    }

    pub(in crate::runtime) fn notify_file_selection(
        &mut self,
        generation: u64,
        token: u64,
    ) -> HostResult<()> {
        self.notify_operation(generation, token, EventKind::FileSelectionReady)
    }

    pub(in crate::runtime) fn notify_operation(
        &mut self,
        generation: u64,
        token: u64,
        kind: EventKind,
    ) -> HostResult<()> {
        self.require_running_generation(generation)?;
        let state = self;
        let slot = state
            .capabilities
            .operations()
            .find_map(|(slot, pending)| {
                (pending.token() == Some(token) && pending.ready_kind() == kind)
                    .then_some(slot.raw())
            })
            .ok_or(HostError::StaleHandle)?;
        let mut bytes = [0; 8];
        let mut writer = Writer::new(&mut bytes);
        writer.operation_handle(crate::runtime::syscall::handles::operation_handle(slot))?;
        let data = writer.finish();
        if state.events.iter().any(|queued| queued.event.kind == kind && queued.event.data == data)
        {
            return Ok(());
        }
        let root =
            *state.work.pending.get(&slot).expect("pending operation retains its accepted root");
        crate::runtime::syscall::events::push_work_event(
            state,
            Event { kind, target: 0, data: data.to_vec() },
            Some(root),
        )
    }

    pub(in crate::runtime) fn notify_update(
        &mut self,
        generation: u64,
        update: PendingUpdate,
    ) -> HostResult<()> {
        self.require_running_generation(generation)?;
        if update.generation == 0 {
            return Err(HostError::InvalidArgument);
        }
        let state = self;
        if let Some(previous) = state.pending_update {
            if previous == update {
                return Ok(());
            }
            if previous.generation >= update.generation {
                return Err(HostError::Conflict);
            }
        }
        let mut bytes = update.generation.to_le_bytes().to_vec();
        bytes.extend(update.build_id);
        push_event(state, Event { kind: EventKind::UpdateAvailable, target: 0, data: bytes })?;
        state.pending_update = Some(update);
        state.deferred_update = None;
        Ok(())
    }

    pub(in crate::runtime) fn set_participants(
        &mut self,
        generation: u64,
        participants: Vec<Participant>,
    ) -> HostResult<()> {
        self.require_running_generation(generation)?;
        validate_participants(&participants)?;
        let state = self;
        push_event(state, Event { kind: EventKind::PresenceChanged, target: 0, data: Vec::new() })?;
        state.context.participants = participants;
        Ok(())
    }

    pub(in crate::runtime) fn pending_update(&self) -> Option<PendingUpdate> {
        self.pending_update
    }

    pub(in crate::runtime) fn deferred_update(&self) -> Option<u64> {
        self.deferred_update
    }

    pub(in crate::runtime) fn has_ready_work(&self, now: Instant) -> bool {
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        if !self.halted && crate::runtime::syscall::audio::ready(self) {
            return true;
        }
        !self.halted && (!self.events.is_empty() || self.wake_due(now))
    }

    pub(in crate::runtime) fn wake_due(&self, now: Instant) -> bool {
        !self.halted
            && (self.wake_deadline.is_some_and(|deadline| deadline <= now)
                || self.poll_deadline.is_some_and(|deadline| {
                    deadline != u64::MAX && deadline <= crate::trace::clock_ns()
                }))
    }

    pub(in crate::runtime) fn wake_after(&self, now: Instant) -> Option<Duration> {
        if self.halted {
            return None;
        }
        let wake = self.wake_deadline.map(|deadline| deadline.saturating_duration_since(now));
        let poll = self.poll_deadline.filter(|deadline| *deadline != u64::MAX).map(|deadline| {
            Duration::from_nanos(deadline.saturating_sub(crate::trace::clock_ns()))
        });
        wake.into_iter().chain(poll).min()
    }

    /// Check after structural output admission, before consuming an event.
    /// A ready event wins even if the absolute deadline elapsed between pumps.
    pub(in crate::runtime) fn arm_poll_deadline(&mut self, deadline: u64) -> HostResult<bool> {
        crate::runtime::syscall::require_supported(self, Opcode::EventPollDeadline)?;
        self.poll_deadline = None;
        queue_due_wake(self, Instant::now());
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        crate::runtime::syscall::audio::pump(self);
        if self.trace_config_pending || !self.events.is_empty() {
            return Ok(false);
        }
        if self.work.draining {
            return Err(HostError::Conflict);
        }
        if deadline != u64::MAX && deadline <= crate::trace::clock_ns() {
            return Err(HostError::WouldBlock);
        }
        self.poll_deadline = Some(deadline);
        Ok(true)
    }

    pub(in crate::runtime) fn file_descriptor(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
    ) -> HostResult<(u64, [u8; 32])> {
        self.require_generation(generation)?;
        let state = self;
        if !state.supported.contains(&Opcode::FileReadBegin)
            || !state.supported.contains(&Opcode::FileReadTake)
        {
            return Err(HostError::Unsupported);
        }
        let address = crate::runtime::syscall::file_ops::address(state, file, VolumeRights::READ)?;
        let descriptor = file_descriptor(state.host.as_mut(), address)?;
        if descriptor.0 > MAX_PRESENTATION_FILE_BYTES as u64 {
            return Err(HostError::LimitExceeded);
        }
        Ok(descriptor)
    }

    pub(in crate::runtime) fn read_file(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
        max_bytes: usize,
    ) -> HostResult<([u8; 32], Vec<u8>)> {
        self.require_generation(generation)?;
        if max_bytes == 0 || max_bytes > MAX_PRESENTATION_FILE_BYTES {
            return Err(HostError::LimitExceeded);
        }
        let state = self;
        if !state.supported.contains(&Opcode::FileReadBegin)
            || !state.supported.contains(&Opcode::FileReadTake)
        {
            return Err(HostError::Unsupported);
        }
        let address = crate::runtime::syscall::file_ops::address(state, file, VolumeRights::READ)?;
        let (byte_len, digest) = file_descriptor(state.host.as_mut(), address)?;
        if byte_len > max_bytes as u64 {
            return Err(HostError::LimitExceeded);
        }
        let mut bytes = Vec::with_capacity(byte_len as usize);
        while bytes.len() < byte_len as usize {
            let limit = (byte_len as usize - bytes.len()).min(MAX_FILE_READ as usize) as u32;
            let chunk = state.host.read_file(address, bytes.len() as u64, limit)?;
            if chunk.total_bytes != byte_len || chunk.bytes.len() != limit as usize {
                return Err(HostError::Conflict);
            }
            bytes.extend_from_slice(&chunk.bytes);
        }
        if blake3::hash(&bytes).as_bytes() != &digest {
            return Err(HostError::Internal);
        }
        // Metadata reads reauthorize the entry. Revocation or replacement
        // during a multi-chunk read must discard the assembled plaintext.
        if file_descriptor(state.host.as_mut(), address)? != (byte_len, digest) {
            return Err(HostError::Conflict);
        }
        Ok((digest, bytes))
    }

    pub(in crate::runtime) fn require_running_generation(&self, generation: u64) -> HostResult<()> {
        self.require_generation(generation)?;
        if self.halted {
            return Err(HostError::Conflict);
        }
        Ok(())
    }

    pub(in crate::runtime) fn require_generation(&self, generation: u64) -> HostResult<()> {
        if generation != self.generation() {
            return Err(HostError::StaleHandle);
        }
        Ok(())
    }
    pub(in crate::runtime) fn begin_turn(&mut self, now: Instant) -> Result<(), RuntimeError> {
        if self.halted {
            return Err(RuntimeError::GuestHalted);
        }
        queue_due_wake(self, now);
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        crate::runtime::syscall::audio::pump(self);
        let generation = self.tracing.generation().to_le_bytes();
        if self.trace_config.data != generation {
            self.trace_config.data.copy_from_slice(&generation);
            self.trace_config_pending = true;
        }
        self.work.progress = false;
        self.logs.clear();
        self.view = None;
        self.view_bytes = None;
        Ok(())
    }
    pub(in crate::runtime) fn finish_frame(&mut self) -> Frame {
        self.work.finish_turn(!self.events.is_empty());
        self.reap_work();
        Frame {
            logs: std::mem::take(&mut self.logs),
            view: self.view.take(),
            view_bytes: self.view_bytes.take(),
            quit_on_last_window: self.quit_on_last_window.unwrap_or(true),
        }
    }
    pub(in crate::runtime) fn halt(&mut self) {
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        {
            self.audio.halt(&self.capabilities);
            crate::runtime::syscall::audio::cancel_open(self);
        }
        self.halted = true;
        self.poll_deadline = None;
        self.wake_deadline = None;
        self.work.active = None;
        self.work.failed |= self.work.draining;
        self.reap_work();
    }
    pub(in crate::runtime) fn deactivate(&mut self) {
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        {
            self.audio.halt(&self.capabilities);
            crate::runtime::syscall::audio::cancel_open(self);
        }
        crate::runtime::syscall::channels::deactivate(self);
        self.work.deactivate();
        self.poll_deadline = None;
        self.volume_watch = None;
        self.wake_deadline = None;
        self.events.retain(|event| {
            if matches!(event.kind, EventKind::Wake | EventKind::Frame | EventKind::VolumesChanged)
            {
                self.queued_bytes -= 20 + event.data.len();
                false
            } else {
                true
            }
        });
        self.reap_work();
    }
    pub(in crate::runtime) fn drain_pending(&self) -> bool {
        !self.work.remaining.is_empty()
    }
    pub(in crate::runtime) fn drain_failed(&self) -> bool {
        self.work.failed
    }
    pub(in crate::runtime) fn reap_work(&mut self) {
        self.work.reap(self.events.iter().map(|event| event.root));
    }
}
