//! Audio and Sound objects use the guest's shared capability namespace.
//! Workers retain only Send completion state; guest-owned counted references
//! never leave their thread. Revocation is immediate, device retirement async.
use super::{
    audio::{Endpoint, Presentation},
    capabilities::{AUDIO, Capabilities, Object, SOUND},
    host::HostResult,
};
use crate::capability::{Handle, Rights};
use playbit_api::abi::syscall::Error as HostError;
use std::{
    cell::{RefCell, RefMut},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
const AUDIO_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::WRITE.0);

pub(super) struct Open {
    pub reservation: Handle,
    active: Arc<AtomicBool>,
    result: Mutex<Option<HostResult<Endpoint>>>,
    ready: AtomicBool,
    pub notified: AtomicBool,
}
impl Open {
    fn begin(
        reservation: Handle,
        presentation: Arc<Presentation>,
        rate: u32,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> HostResult<Arc<Self>> {
        // Both guest slots are already admitted; reserve the process device
        // credit before a worker can initialize the audio driver.
        let mut endpoint = Endpoint::new(presentation, rate)?;
        let open = Arc::new(Self {
            reservation,
            active: Arc::new(AtomicBool::new(true)),
            result: Mutex::new(None),
            ready: AtomicBool::new(false),
            notified: AtomicBool::new(false),
        });
        let completion = open.clone();
        std::thread::Builder::new()
            .name("playbit-audio-open".into())
            .spawn(move || {
                let result = endpoint.initialize().map(|()| endpoint);
                *completion.result.lock().expect("audio open completion poisoned") = Some(result);
                completion.ready.store(true, Ordering::Release);
                wake();
            })
            .map_err(|_| HostError::Unavailable)?;
        Ok(open)
    }
    pub fn cancel(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub fn cancel_result<P>(&self, capabilities: &mut Capabilities<P>) {
        // Publication consumes the reservation. Successful Take must preserve
        // the active grant now owned by the published Audio and its children.
        if capabilities.cancel(self.reservation).is_ok() {
            self.cancel();
        }
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    fn take(&self) -> HostResult<Endpoint> {
        if !self.active.load(Ordering::Acquire) {
            return Err(HostError::AccessDenied);
        }
        if !self.ready() {
            return Err(HostError::WouldBlock);
        }
        self.result
            .lock()
            .expect("audio open completion poisoned")
            .take()
            .ok_or(HostError::StaleHandle)?
    }
}
pub struct Device {
    active: Arc<AtomicBool>,
    endpoint: RefCell<Endpoint>,
}
impl Device {
    pub(super) fn require(&self) -> HostResult<()> {
        if self.active.load(Ordering::Acquire) { Ok(()) } else { Err(HostError::AccessDenied) }
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}
pub struct Sound {
    pub(super) device: Rc<Device>,
}
/// The current-device handle is discovery metadata; every resource object and
/// identity is owned by Capabilities, including Sound's counted parent reference.
pub(super) struct Table {
    device: Option<Handle>,
}
impl Table {
    pub fn new() -> Self {
        Self { device: None }
    }
    pub fn begin<P>(
        &self,
        capabilities: &mut Capabilities<P>,
        presentation: Arc<Presentation>,
        rate: u32,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> HostResult<Arc<Open>> {
        if self.device.is_some() {
            return Err(HostError::Conflict);
        }
        let reservation = capabilities.reserve(AUDIO, AUDIO_RIGHTS)?;
        match Open::begin(reservation, presentation, rate, wake) {
            Ok(open) => Ok(open),
            Err(error) => {
                capabilities.cancel(reservation).expect("owned audio result reservation");
                Err(error)
            }
        }
    }
    pub fn take<P>(&mut self, capabilities: &mut Capabilities<P>, open: &Open) -> HostResult<u64> {
        if self.device.is_some() {
            return Err(HostError::Conflict);
        }
        let endpoint = open.take()?;
        capabilities.table.insert(
            open.reservation,
            Object::Audio(Rc::new(Device {
                active: open.active.clone(),
                endpoint: RefCell::new(endpoint),
            })),
        )?;
        self.device = Some(open.reservation);
        Ok(open.reservation.raw())
    }
    fn device<'a, P>(
        &self,
        capabilities: &'a Capabilities<P>,
        handle: u64,
        rights: Rights,
    ) -> HostResult<RefMut<'a, Endpoint>> {
        let device = capabilities.audio(Handle(handle), rights)?;
        assert_eq!(self.device, Some(Handle(handle)), "capability identifies the owned output");
        Ok(device.endpoint.borrow_mut())
    }
    pub fn close<P>(&mut self, capabilities: &mut Capabilities<P>, handle: u64) -> HostResult<()> {
        let device = capabilities.audio(Handle(handle), Rights::WRITE)?.clone();
        device.active.store(false, Ordering::Release);
        capabilities.table.remove(Handle(handle), AUDIO)?;
        let children: Vec<_> = capabilities
            .table
            .entries()
            .filter_map(|(id, _, _, object)| match object {
                Object::Sound(sound) if Rc::ptr_eq(&sound.device, &device) => Some(id),
                _ => None,
            })
            .collect();
        for child in children {
            capabilities.table.remove(child, SOUND).expect("audio retains its sound children");
        }
        self.device = None;
        // Last counted Device drops Endpoint, which signals its driver without joining.
        Ok(())
    }
    pub fn sample_rate<P>(&self, capabilities: &Capabilities<P>, handle: u64) -> HostResult<u32> {
        Ok(self.device(capabilities, handle, Rights::READ)?.sample_rate())
    }
    pub fn volume<P>(&self, capabilities: &Capabilities<P>, handle: u64) -> HostResult<f32> {
        Ok(self.device(capabilities, handle, Rights::READ)?.volume())
    }
    pub fn set_volume<P>(
        &self,
        capabilities: &Capabilities<P>,
        handle: u64,
        value: f32,
    ) -> HostResult<()> {
        self.device(capabilities, handle, Rights::WRITE)?.set_volume(value)
    }
    pub fn open_sound<P>(
        &mut self,
        capabilities: &mut Capabilities<P>,
        audio: u64,
        channels: u32,
        rate: u32,
        frames: u32,
        bytes: &[u8],
    ) -> HostResult<u64> {
        let device = capabilities.audio(Handle(audio), Rights::WRITE)?.clone();
        let reservation = capabilities.reserve(SOUND, AUDIO_RIGHTS)?;
        // The endpoint queues Close(old key) before this slot can be reused.
        // FIFO commands and counted PCM references isolate old playback from a
        // later sound using the same internal key; guest identity includes generation.
        if let Err(error) = device.endpoint.borrow_mut().open_sound(
            reservation.index(),
            channels,
            rate,
            frames,
            bytes,
        ) {
            capabilities.cancel(reservation).expect("owned sound result reservation");
            return Err(error);
        }
        capabilities
            .table
            .insert(reservation, Object::Sound(Rc::new(Sound { device })))
            .expect("admitted sound reservation remains owned");
        Ok(reservation.raw())
    }
    pub fn sound<'a, P>(
        &self,
        capabilities: &'a Capabilities<P>,
        handle: u64,
        rights: Rights,
    ) -> HostResult<(RefMut<'a, Endpoint>, u32)> {
        let sound = capabilities.sound(Handle(handle), rights)?;
        Ok((sound.device.endpoint.borrow_mut(), Handle(handle).index()))
    }
    pub fn close_sound<P>(
        &mut self,
        capabilities: &mut Capabilities<P>,
        handle: u64,
    ) -> HostResult<()> {
        {
            let (mut endpoint, key) = self.sound(capabilities, handle, Rights::WRITE)?;
            endpoint.close_sound(key)?;
        }
        capabilities.table.remove(Handle(handle), SOUND)?;
        Ok(())
    }
    pub fn halt<P>(&self, capabilities: &Capabilities<P>) {
        if let Some(handle) = self.device {
            capabilities
                .audio(handle, Rights::NONE)
                .expect("owned audio output")
                .endpoint
                .borrow()
                .halt();
        }
    }
}

/// A stopped, device-free result exercises the actual take/control adapters.
#[cfg(test)]
pub(in crate::runtime) fn prepare_open(
    state: &mut super::instance::State,
    rate: u32,
) -> HostResult<playbit_api::abi::syscall::OperationHandle> {
    state.supported.extend(playbit_api::abi::audio::OPCODES);
    let slot = super::syscall::handles::reserve_operation(state)?;
    let reservation = match state.capabilities.reserve(AUDIO, AUDIO_RIGHTS) {
        Ok(reservation) => reservation,
        Err(error) => {
            super::syscall::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    let presentation = Presentation::new([213; 16]);
    let endpoint = match Endpoint::new(presentation, rate) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            state.capabilities.cancel(reservation).expect("owned test audio result");
            super::syscall::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    super::audio::set_instance_active([213; 16], true);
    let open = Arc::new(Open {
        reservation,
        active: Arc::new(AtomicBool::new(true)),
        result: Mutex::new(Some(Ok(endpoint))),
        ready: AtomicBool::new(true),
        notified: AtomicBool::new(false),
    });
    super::syscall::handles::register_operation(
        state,
        slot,
        super::instance::PendingOperation::AudioOpen(open),
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests exercise admitted resources without opening a device."
)]
mod tests {
    use super::*;
    use crate::runtime::{
        audio,
        instance::PendingOperation,
        syscall::{self, Event, EventKind, OperationHandle, handles},
    };
    use playbit_api::abi::audio as abi;

    fn endpoint() -> Endpoint {
        let presentation = Presentation::new([213; 16]);
        let endpoint = Endpoint::new(presentation, 44_100).unwrap();
        audio::set_instance_active([213; 16], true);
        endpoint
    }
    fn pending<P>(
        capabilities: &mut Capabilities<P>,
        result: Option<HostResult<Endpoint>>,
    ) -> Arc<Open> {
        Arc::new(Open {
            reservation: capabilities.reserve(AUDIO, AUDIO_RIGHTS).unwrap(),
            active: Arc::new(AtomicBool::new(true)),
            ready: AtomicBool::new(result.is_some()),
            notified: AtomicBool::new(false),
            result: Mutex::new(result),
        })
    }
    #[test]
    fn audio_and_sound_capabilities_cannot_alias_files_and_close_revokes_children() {
        // Exercise central identity/type/rights checks around the actual endpoint;
        // no device initialization or realtime callback is needed for admission.
        let _serial = audio::TEST_SERIAL.lock().unwrap();
        let (mut state, _, _) = syscall::pending_data::tests::state();
        let open = pending(&mut state.capabilities, Some(Ok(endpoint())));
        let output = state.audio.take(&mut state.capabilities, &open).unwrap();
        assert!(matches!(
            state.audio.take(&mut state.capabilities, &open),
            Err(HostError::Conflict)
        ));
        let sound =
            state.audio.open_sound(&mut state.capabilities, output, 2, 44_100, 2, &[0; 8]).unwrap();
        assert_ne!(output, sound);
        assert!(matches!(
            state.audio.sample_rate(&state.capabilities, sound),
            Err(HostError::StaleHandle)
        ));
        assert!(matches!(
            state.audio.sound(&state.capabilities, output, Rights::READ),
            Err(HostError::StaleHandle)
        ));
        assert!(matches!(
            state.files.file(&state.capabilities, playbit_api::abi::files::FileHandle(sound)),
            Err(HostError::StaleHandle)
        ));
        {
            let (mut playback_endpoint, key) =
                state.audio.sound(&state.capabilities, sound, Rights::WRITE).unwrap();
            playback_endpoint.play(key, 1234, abi::UNITY_GAIN, 0, 45_352, abi::SOUND_LOOP).unwrap();
        }
        state.capabilities.table.update_rights(Handle(sound), SOUND, Rights::READ).unwrap();
        assert!(matches!(
            state.audio.sound(&state.capabilities, sound, Rights::WRITE),
            Err(HostError::AccessDenied)
        ));
        state.capabilities.table.update_rights(Handle(sound), SOUND, AUDIO_RIGHTS).unwrap();
        state.audio.close_sound(&mut state.capabilities, sound).unwrap();
        let replacement =
            state.audio.open_sound(&mut state.capabilities, output, 2, 44_100, 2, &[0; 8]).unwrap();
        assert_eq!(Handle(sound).index(), Handle(replacement).index());
        assert_ne!(sound, replacement);
        assert!(matches!(
            state.audio.sound(&state.capabilities, sound, Rights::READ),
            Err(HostError::StaleHandle)
        ));
        let sound = replacement;
        let retained =
            state.capabilities.sound(Handle(sound), Rights::READ).unwrap().device.active.clone();
        state.audio.close(&mut state.capabilities, output).unwrap();
        assert!(!retained.load(Ordering::Acquire));
        assert!(matches!(
            state.audio.sound(&state.capabilities, sound, Rights::READ),
            Err(HostError::StaleHandle)
        ));
        assert!(matches!(
            state.audio.sample_rate(&state.capabilities, output),
            Err(HostError::StaleHandle)
        ));
        let next = pending(&mut state.capabilities, Some(Ok(endpoint())));
        let next = state.audio.take(&mut state.capabilities, &next).unwrap();
        assert_ne!(next, output);
        state.audio.close(&mut state.capabilities, next).unwrap();
    }
    #[test]
    fn asynchronous_audio_completion_retries_a_full_event_queue_and_consumes_once() {
        // Publish a worker result after begin, fill event admission, then retry.
        // Successful take consumes its work root/event without replaying open.
        let _serial = audio::TEST_SERIAL.lock().unwrap();
        let (mut state, _, _) = syscall::pending_data::tests::state();
        state.supported.extend(abi::OPCODES);
        let open = pending(&mut state.capabilities, None);
        let slot = handles::reserve_operation(&mut state).unwrap();
        let operation = handles::register_operation(
            &mut state,
            slot,
            PendingOperation::AudioOpen(open.clone()),
        )
        .unwrap();
        assert_eq!(syscall::audio::open_take(&mut state, operation), Err(HostError::WouldBlock));
        assert!(!syscall::audio::ready(&state));
        *open.result.lock().unwrap() = Some(Ok(endpoint()));
        open.ready.store(true, Ordering::Release);
        while state.events.len() < crate::runtime::work::MAX_QUEUED_EVENTS {
            syscall::events::push_event(
                &mut state,
                Event { object_id: 0, kind: EventKind::Wake, target: 0, data: vec![] },
            )
            .unwrap();
        }
        syscall::audio::pump(&mut state);
        assert!(syscall::audio::ready(&state));
        syscall::events::take_event(&mut state).unwrap();
        syscall::audio::pump(&mut state);
        assert!(!syscall::audio::ready(&state));
        assert!(state.events.iter().any(|event| event.kind == EventKind::DataReady));
        let output = syscall::audio::open_take(&mut state, operation).unwrap();
        assert_eq!(state.audio.sample_rate(&state.capabilities, output).unwrap(), 44_100);
        assert!(!state.events.iter().any(|event| event.kind == EventKind::DataReady));
        assert_eq!(syscall::audio::open_take(&mut state, operation), Err(HostError::StaleHandle));
        state.assert_invariants();
    }
    #[test]
    fn failed_and_disposed_opens_release_reservations_without_minting_audio() {
        // Terminal device failure consumes once. Disposal before completion
        // removes the operation; a late result cannot deliver a capability.
        let _serial = audio::TEST_SERIAL.lock().unwrap();
        let (mut state, _, _) = syscall::pending_data::tests::state();
        state.supported.extend(abi::OPCODES);
        for result in [Some(Err(HostError::Unavailable)), None] {
            let open = pending(&mut state.capabilities, result);
            let identity = open.reservation;
            let slot = handles::reserve_operation(&mut state).unwrap();
            let operation = handles::register_operation(
                &mut state,
                slot,
                PendingOperation::AudioOpen(open.clone()),
            )
            .unwrap();
            let foreign = OperationHandle { generation: operation.generation + 1, ..operation };
            assert_eq!(syscall::audio::open_take(&mut state, foreign), Err(HostError::StaleHandle));
            if open.ready() {
                assert_eq!(
                    syscall::audio::open_take(&mut state, operation),
                    Err(HostError::Unavailable)
                );
            } else {
                syscall::pending_files::dispose(&mut state, operation).unwrap();
                assert!(matches!(
                    state.audio.take(&mut state.capabilities, &open),
                    Err(HostError::AccessDenied)
                ));
                let replacement = prepare_open(&mut state, 44_100).unwrap();
                let PendingOperation::AudioOpen(replacement_open) =
                    state.capabilities.operation(handles::raw_operation(replacement)).unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(replacement_open.reservation.index(), identity.index());
                assert_ne!(replacement_open.reservation, identity);
                *open.result.lock().unwrap() = Some(Ok(endpoint()));
                open.ready.store(true, Ordering::Release);
                syscall::audio::pump(&mut state);
                let ready: Vec<_> = state
                    .events
                    .iter()
                    .filter(|event| event.kind == EventKind::DataReady)
                    .collect();
                assert_eq!(ready.len(), 1);
                assert_eq!(ready[0].data, handles::raw_operation(replacement).raw().to_le_bytes());
                let output = syscall::audio::open_take(&mut state, replacement).unwrap();
                state.audio.close(&mut state.capabilities, output).unwrap();
            }
            assert_eq!(
                syscall::audio::open_take(&mut state, operation),
                Err(HostError::StaleHandle)
            );
            assert!(matches!(
                state.capabilities.audio(identity, Rights::NONE),
                Err(HostError::StaleHandle)
            ));
            state.assert_invariants();
            assert_eq!(state.capabilities.table.reserved(), 0);
        }
    }
    #[test]
    fn retiring_execution_cancels_pending_audio_without_drain_failure() {
        // Device initialization is not a durable mutation. Retirement cancels
        // its accepted dependency and ignores completion arriving afterward.
        let _serial = audio::TEST_SERIAL.lock().unwrap();
        for halt in [false, true] {
            let (mut state, _, _) = syscall::pending_data::tests::state();
            state.supported.extend(abi::OPCODES);
            syscall::events::take_event(&mut state).unwrap();
            let open = pending(&mut state.capabilities, None);
            let slot = handles::reserve_operation(&mut state).unwrap();
            let operation = handles::register_operation(
                &mut state,
                slot,
                PendingOperation::AudioOpen(open.clone()),
            )
            .unwrap();
            state.finish_frame();
            assert!(state.drain_pending());
            if halt {
                state.halt();
            } else {
                state.deactivate();
            }
            assert!(!state.drain_pending());
            assert!(!state.drain_failed());
            *open.result.lock().unwrap() = Some(Ok(endpoint()));
            open.ready.store(true, Ordering::Release);
            syscall::audio::pump(&mut state);
            assert!(state.events.is_empty());
            assert_eq!(
                syscall::audio::open_take(&mut state, operation),
                Err(HostError::StaleHandle)
            );
            assert!(matches!(
                state.audio.take(&mut state.capabilities, &open),
                Err(HostError::AccessDenied)
            ));
            state.assert_invariants();
        }
    }
}
