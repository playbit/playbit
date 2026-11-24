//! Checked instance operations with ordinary typed arguments and results.
//! Adapters validate their memory and output storage before calling these.
//! A packed scene is presentation data, independent of the syscall transport.
use super::{
    HostError, HostResult, Opcode, State, abi, files::validate_scene_capabilities,
    require_supported,
};
use crate::runtime::decode_view;
use std::time::{Duration, Instant};

pub(in crate::runtime) fn log(state: &mut State, message: &str) -> HostResult<()> {
    require_supported(state, Opcode::Log)?;
    state.logs.push(message.to_owned());
    Ok(())
}

fn last_window_choice(current: Option<bool>, intent: u32) -> HostResult<bool> {
    if intent > 1 {
        return Err(HostError::InvalidArgument);
    }
    let intent = intent != 0;
    if current.is_some_and(|previous| previous != intent) {
        return Err(HostError::Conflict);
    }
    Ok(intent)
}
pub(in crate::runtime) fn quit_on_last_window(state: &mut State, intent: u32) -> HostResult<()> {
    require_supported(state, Opcode::QuitOnLastWindow)?;
    state.quit_on_last_window = Some(last_window_choice(state.quit_on_last_window, intent)?);
    Ok(())
}

pub(in crate::runtime) fn view_submit(state: &mut State, scene: &[u8]) -> HostResult<()> {
    let _timing = crate::trace::Span::new("guest.view.submit");
    require_supported(state, Opcode::ViewSubmit)?;
    assert_eq!(state.view.is_some(), state.view_bytes.is_some());
    if scene.len() > state.view_bytes_limit as usize {
        return Err(HostError::LimitExceeded);
    }
    let view = decode_view(scene).map_err(|_| HostError::InvalidArgument)?;
    validate_scene_capabilities(state, &view)?;
    state.view = Some(view);
    state.view_bytes = Some(scene.to_vec());
    assert!(state.view.is_some());
    assert_eq!(state.view_bytes.as_ref().map(Vec::len), Some(scene.len()));
    Ok(())
}

/// `now` is supplied by the trusted execution adapter, not by app input.
pub(in crate::runtime) fn wake_after(
    state: &mut State,
    delay: u32,
    now: Instant,
) -> HostResult<()> {
    require_supported(state, Opcode::WakeAfter)?;
    if state.work.draining {
        return Err(HostError::Conflict);
    }
    if !(abi::MIN_WAKE_DELAY_MS..=abi::MAX_WAKE_DELAY_MS).contains(&delay) {
        return Err(HostError::InvalidArgument);
    }
    let deadline = now
        .checked_add(Duration::from_millis(u64::from(delay)))
        .ok_or(HostError::InvalidArgument)?;
    // Replace the one pending wake, including an expired event that
    // the app has not consumed yet. There are no timer handles.
    state.events.retain(|queued| {
        let event = &queued.event;
        if event.kind == abi::EventKind::Wake {
            state.queued_bytes -= 20;
            false
        } else {
            true
        }
    });
    state.wake_deadline = Some(deadline);
    assert_eq!(state.wake_deadline, Some(deadline));
    debug_assert!(state.events.iter().all(|queued| queued.event.kind != abi::EventKind::Wake));
    Ok(())
}

pub(in crate::runtime) fn update_defer(state: &mut State, generation: u64) -> HostResult<()> {
    require_supported(state, Opcode::UpdateDefer)?;
    if state.pending_update.is_none_or(|update| update.generation != generation) {
        return Err(HostError::Conflict);
    }
    state.deferred_update = Some(generation);
    assert_eq!(state.deferred_update, Some(generation));
    assert!(state.pending_update.is_some_and(|update| update.generation == generation));
    Ok(())
}

/// One registration and at most one queued notification; no retained snapshots.
pub(in crate::runtime) fn observe_volumes(state: &mut State, revision: u64) -> HostResult<()> {
    require_supported(state, Opcode::VolumesObserve)?;
    if revision == 0 || revision > state.volume_revision {
        return Err(HostError::InvalidArgument);
    }
    if state.work.draining {
        return Err(HostError::Conflict);
    }
    state.events.retain(|queued| {
        if queued.event.kind == abi::EventKind::VolumesChanged {
            state.queued_bytes -= 20 + queued.event.data.len();
            false
        } else {
            true
        }
    });
    state.volume_watch = Some(revision);
    queue_volume_change(state);
    Ok(())
}
pub(in crate::runtime) fn queue_volume_change(state: &mut State) {
    if state.work.draining
        || state.volume_watch.is_none_or(|revision| revision >= state.volume_revision)
    {
        return;
    }
    let event = super::Event {
        kind: abi::EventKind::VolumesChanged,
        target: 0,
        data: state.volume_revision.to_le_bytes().to_vec(),
    };
    if super::events::push_event(state, event).is_ok() {
        state.volume_watch = None;
    }
    // A full queue retains the registration. Consuming an event retries it.
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::runtime::syscall::{
        AvailableVolume, Event, GuestContext, Host, Identity, PendingUpdate, VolumeRights,
        push_event,
    };
    use std::collections::{BTreeMap, BTreeSet};

    struct LocalHost;
    impl Host for LocalHost {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
    }
    fn instance() -> State {
        State::new(
            GuestContext {
                app_name: "Test app".into(),
                app_build: None,
                identity: Identity {
                    display_name: String::new(),
                    account_id: [1; 16],
                    is_service: false,
                    device_id: [2; 16],
                    instance_id: [3; 16],
                    session_id: None,
                },
                volumes: vec![AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id: [7; 16],
                    name: "Root".into(),
                    tags: vec![],
                    rights: VolumeRights::ALL,
                    is_root: true,
                    role: 0,
                }],
                startup_arguments: BTreeMap::new(),
                trace_catalog: None,
                required_capabilities: BTreeSet::new(),
                participants: vec![],
            },
            Box::new(LocalHost),
        )
        .unwrap()
    }

    #[test]
    fn last_window_intent_is_immutable_and_available_while_draining() {
        let mut first = instance();
        let mut second = instance();
        assert!(first.finish_frame().quit_on_last_window(), "explicit host default");
        assert_eq!(quit_on_last_window(&mut first, 2), Err(HostError::InvalidArgument));
        assert_eq!(first.quit_on_last_window, None);
        quit_on_last_window(&mut first, 0).unwrap();
        quit_on_last_window(&mut second, 1).unwrap();
        assert!(!first.finish_frame().quit_on_last_window());
        assert!(second.finish_frame().quit_on_last_window());
        first.deactivate();
        quit_on_last_window(&mut first, 0).unwrap();
        assert_eq!(quit_on_last_window(&mut first, 1), Err(HostError::Conflict));
        assert_eq!(first.quit_on_last_window, Some(false));
        first.assert_invariants();
        second.assert_invariants();
    }

    #[test]
    fn logs_preserve_large_messages_and_instances_are_independent() {
        // Logging owns the exact UTF-8 text independently of the caller and instance.
        let mut a = instance();
        let mut b = instance();
        let message = "é".repeat(48 * 1024);
        log(&mut a, &message).unwrap();
        log(&mut a, &message).unwrap();
        assert_eq!(a.logs, [message.clone(), message]);
        log(&mut b, "é").unwrap();
        assert_eq!(b.logs, ["é"]);
        a.assert_invariants();
        b.assert_invariants();
    }

    #[test]
    fn frames_preserve_log_order_and_turns_have_separate_output() {
        // A burst, including empty messages, is returned once in its original order.
        let mut state = instance();
        let mut expected = Vec::new();
        for index in 0..1000 {
            let message = index.to_string();
            log(&mut state, &message).unwrap();
            log(&mut state, "").unwrap();
            expected.extend([message, String::new()]);
        }
        state.assert_invariants();
        let frame = state.finish_frame();
        assert_eq!(frame.logs(), expected);
        assert!(state.logs.is_empty());
        state.begin_turn(Instant::now()).unwrap();
        log(&mut state, "next turn").unwrap();
        assert_eq!(state.finish_frame().logs(), ["next turn"]);
        state.assert_invariants();
    }

    #[test]
    fn typed_scene_failure_preserves_the_accepted_scene_and_encoding() {
        let mut state = instance();
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.column_end();
        let scene = encoder.finish();
        view_submit(&mut state, &scene).unwrap();
        let before = state.view.clone();
        assert_eq!(view_submit(&mut state, &scene[..3]), Err(HostError::InvalidArgument));
        assert_eq!(state.view, before);
        assert_eq!(state.view_bytes.as_deref(), Some(scene.as_slice()));
        state.assert_invariants();
    }

    #[test]
    fn typed_wake_replacement_keeps_other_events_and_rejects_before_mutation() {
        let mut state = instance();
        let now = Instant::now();
        push_event(&mut state, Event { kind: abi::EventKind::Wake, target: 0, data: vec![] })
            .unwrap();
        push_event(
            &mut state,
            Event { kind: abi::EventKind::DataChanged, target: 0, data: vec![1, 2, 3] },
        )
        .unwrap();
        let before = state.events.clone();
        assert_eq!(wake_after(&mut state, 0, now), Err(HostError::InvalidArgument));
        assert_eq!(state.events, before);
        assert_eq!(state.queued_bytes, 63);
        assert_eq!(state.wake_deadline, None);
        wake_after(&mut state, abi::MIN_WAKE_DELAY_MS, now).unwrap();
        assert_eq!(state.events.len(), 2);
        assert_eq!(state.queued_bytes, 43);
        assert_eq!(state.events[1].event.data, [1, 2, 3]);
        assert_eq!(
            state.wake_deadline,
            Some(now + Duration::from_millis(u64::from(abi::MIN_WAKE_DELAY_MS)))
        );
        state.pending_update = Some(PendingUpdate { generation: 7, build_id: [4; 32] });
        update_defer(&mut state, 7).unwrap();
        assert_eq!(update_defer(&mut state, 8), Err(HostError::Conflict));
        assert_eq!(state.deferred_update, Some(7));
        state.assert_invariants();
    }
    #[test]
    fn root_watch_detects_prior_changes_and_survives_a_full_queue() {
        let mut state = instance();
        observe_volumes(&mut state, 1).unwrap();
        assert_eq!(state.volume_watch, Some(1));
        let mut volumes = state.context.volumes.clone();
        volumes[0].name = "Renamed".into();
        state.replace_volumes(volumes.clone()).unwrap();
        assert_eq!(state.volume_revision, 2);
        assert_eq!(state.volume_watch, None);
        assert_eq!(state.events.back().unwrap().event.kind, abi::EventKind::VolumesChanged);
        assert_eq!(state.events.back().unwrap().event.data, 2_u64.to_le_bytes());
        assert!(!state.replace_volumes(volumes.clone()).unwrap());
        assert_eq!(state.volume_revision, 2);
        observe_volumes(&mut state, 2).unwrap();
        assert!(state.events.iter().all(|event| event.kind != abi::EventKind::VolumesChanged));
        while state.events.len() < super::super::MAX_QUEUED_EVENTS {
            push_event(
                &mut state,
                Event { kind: abi::EventKind::DataChanged, target: 0, data: vec![] },
            )
            .unwrap();
        }
        volumes[0].name = "Latest".into();
        state.replace_volumes(volumes).unwrap();
        assert_eq!(state.volume_watch, Some(2));
        super::super::events::take_event(&mut state).unwrap();
        assert_eq!(state.volume_watch, None);
        assert_eq!(state.events.back().unwrap().event.data, 3_u64.to_le_bytes());
        observe_volumes(&mut state, 1).unwrap();
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| event.kind == abi::EventKind::VolumesChanged)
                .count(),
            1
        );
        assert_eq!(observe_volumes(&mut state, 4), Err(HostError::InvalidArgument));
        state.work.finish_turn(!state.events.is_empty());
        state.deactivate();
        assert!(state.events.iter().all(|event| event.kind != abi::EventKind::VolumesChanged));
        assert_eq!(observe_volumes(&mut state, 3), Err(HostError::Conflict));
    }
}
