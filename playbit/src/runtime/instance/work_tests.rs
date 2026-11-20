#![allow(
    clippy::unwrap_used,
    reason = "Tests drive accepted roots through exact typed completions."
)]
use super::*;
use crate::runtime::{
    host::{DataOperation, DataResult, FieldChange, ObjectRecord, Value},
    syscall::{
        events,
        pending_data::{self, tests::state},
    },
};
use std::sync::Arc;

#[test]
fn deactivation_preserves_an_edits_readback_root_and_rejects_new_input_and_timers() {
    let (mut state, records, volume) = state();
    events::take_event(&mut state).unwrap();
    let edit = pending_data::begin(
        &mut state,
        volume,
        DataOperation::Edit {
            entry_id: [5; 15],
            object_id: [2; 16],
            fields: vec![FieldChange {
                field_id: [3; 16],
                values: Some(vec![Value::Boolean(true)]),
            }],
        },
        Opcode::ObjectEditBegin,
    )
    .unwrap();
    let root =
        state.work.pending[&crate::capability::Handle::new(edit.generation, edit.slot).raw()];
    crate::runtime::syscall::control::wake_after(&mut state, 1000, Instant::now()).unwrap();
    state.finish_frame();
    state.deactivate();
    assert!(state.drain_pending());
    assert!(state.wake_deadline.is_none());
    let generation = state.generation();
    assert_eq!(
        state.queue_event(generation, Event { kind: EventKind::Click, target: 1, data: vec![] }),
        Err(HostError::Conflict)
    );
    assert_eq!(
        crate::runtime::syscall::control::wake_after(&mut state, 1000, Instant::now()),
        Err(HostError::Conflict)
    );
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Edited)));
    state.notify_data(generation, 1).unwrap();
    assert_eq!(state.events.front().unwrap().root, root);
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::DataReady);
    pending_data::take(&mut state, edit, Opcode::ObjectEditTake, |_, _| Ok(())).unwrap();
    let read = pending_data::begin(
        &mut state,
        volume,
        DataOperation::Read { entry_id: [5; 15], object_id: [2; 16], fields: vec![[3; 16]] },
        Opcode::ObjectReadBegin,
    )
    .unwrap();
    assert_eq!(
        state.work.pending[&crate::capability::Handle::new(read.generation, read.slot).raw()],
        root
    );
    state.finish_frame();
    assert!(state.drain_pending());
    records.borrow_mut().results.insert(
        2,
        Ok(Arc::new(DataResult::Object(ObjectRecord {
            id: [2; 16],
            type_id: [4; 16],
            fields: vec![],
        }))),
    );
    state.notify_data(generation, 2).unwrap();
    events::take_event(&mut state).unwrap();
    pending_data::take(&mut state, read, Opcode::ObjectReadTake, |_, _| Ok(())).unwrap();
    state.finish_frame();
    assert!(!state.drain_pending());
    assert!(!state.drain_failed());
    assert_eq!(records.borrow().calls.len(), 2);
}

#[test]
fn taking_a_completion_directly_restores_its_root_before_a_dependent_begin() {
    let (mut state, records, volume) = state();
    events::take_event(&mut state).unwrap();
    let pending = pending_data::begin(
        &mut state,
        volume,
        DataOperation::SchemaRead { entry_id: [5; 15] },
        Opcode::SchemaReadBegin,
    )
    .unwrap();
    let root =
        state.work.pending[&crate::capability::Handle::new(pending.generation, pending.slot).raw()];
    events::push_event(&mut state, Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    state.finish_frame();
    state.deactivate();
    events::take_event(&mut state).unwrap();
    assert_ne!(state.work.active, Some(root));
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Schema(Default::default()))));
    pending_data::take(&mut state, pending, Opcode::SchemaReadTake, |_, _| Ok(())).unwrap();
    let read = pending_data::begin(
        &mut state,
        volume,
        DataOperation::SchemaRead { entry_id: [5; 15] },
        Opcode::SchemaReadBegin,
    )
    .unwrap();
    assert_eq!(
        state.work.pending[&crate::capability::Handle::new(read.generation, read.slot).raw()],
        root
    );
}

#[test]
fn a_recursive_completion_chain_exhausts_before_another_host_begin_and_reports_failed_drain() {
    let (mut state, records, volume) = state();
    events::take_event(&mut state).unwrap();
    let mut pending = pending_data::begin(
        &mut state,
        volume,
        DataOperation::SchemaRead { entry_id: [5; 15] },
        Opcode::SchemaReadBegin,
    )
    .unwrap();
    state.finish_frame();
    state.deactivate();
    for token in 1..=u64::from(work::DRAIN_STEPS) {
        records
            .borrow_mut()
            .results
            .insert(token, Ok(Arc::new(DataResult::Schema(Default::default()))));
        state.notify_data(state.generation(), token).unwrap();
        events::take_event(&mut state).unwrap();
        pending_data::take(&mut state, pending, Opcode::SchemaReadTake, |_, _| Ok(())).unwrap();
        pending = pending_data::begin(
            &mut state,
            volume,
            DataOperation::SchemaRead { entry_id: [5; 15] },
            Opcode::SchemaReadBegin,
        )
        .unwrap();
        state.finish_frame();
    }
    let token = u64::from(work::DRAIN_STEPS) + 1;
    records
        .borrow_mut()
        .results
        .insert(token, Ok(Arc::new(DataResult::Schema(Default::default()))));
    state.notify_data(state.generation(), token).unwrap();
    events::take_event(&mut state).unwrap();
    pending_data::take(&mut state, pending, Opcode::SchemaReadTake, |_, _| Ok(())).unwrap();
    let calls = records.borrow().calls.len();
    assert_eq!(
        pending_data::begin(
            &mut state,
            volume,
            DataOperation::SchemaRead { entry_id: [5; 15] },
            Opcode::SchemaReadBegin
        ),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(records.borrow().calls.len(), calls);
    state.finish_frame();
    assert!(!state.drain_pending());
    assert!(state.drain_failed(), "exhaustion cannot count as successful Quit");
}

#[test]
fn deactivation_cancels_unstarted_recurring_events_but_keeps_accepted_input() {
    let (mut state, _records, _volume) = state();
    events::take_event(&mut state).unwrap();
    for kind in [EventKind::Wake, EventKind::Frame, EventKind::Click] {
        events::push_event(&mut state, Event { kind, target: 1, data: vec![] }).unwrap();
    }
    state.finish_frame();
    state.deactivate();
    assert_eq!(state.events.len(), 1);
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::Click);
    state.finish_frame();
    assert!(!state.drain_pending());
    assert!(!state.drain_failed());
}

#[test]
fn an_unconsumed_accepted_event_reports_stalled_drain_instead_of_an_idle_success() {
    let (mut state, _records, _volume) = state();
    state.deactivate();
    state.begin_turn(Instant::now()).unwrap();
    // A cooperative return without consuming the accepted startup event.
    state.finish_frame();
    assert!(state.drain_pending());
    assert!(state.drain_failed());
}

#[test]
fn quit_drains_a_supported_file_and_full_readback_at_guest_chunk_size() {
    use crate::runtime::{
        file_handles::Kind,
        host::files::{Action, Output},
        syscall::pending_files,
    };
    use playbit_api::abi::files::FileHandle;
    let (mut state, records, _) = state();
    events::take_event(&mut state).unwrap();
    let reservation = state.files.reserve(&mut state.capabilities, Kind::Reader).unwrap();
    let slot = state.files.insert(&mut state.capabilities, &reservation, 11, [1; 16]).unwrap();
    let reader = FileHandle(slot);
    let total = 64 * 1024 * 1024u64 - 29;
    let chunk = 64 * 1024u32;
    let mut pending =
        pending_files::begin(&mut state, reader, Action::Read { offset: 0, length: chunk })
            .unwrap();
    state.finish_frame();
    state.deactivate();
    let mut token = 1;
    for pass in 0..2 {
        let mut offset = 0;
        while offset < total {
            let count = (total - offset).min(u64::from(chunk)) as usize;
            records
                .borrow_mut()
                .file_results
                .insert((1 << 63) | token, Ok(Arc::new(Output::Read { bytes: vec![7; count] })));
            state.notify_data(state.generation(), (1 << 63) | token).unwrap();
            events::take_event(&mut state).unwrap();
            pending_files::take(&mut state, pending, Opcode::FileReadTake, |_, _| Ok(())).unwrap();
            offset += count as u64;
            token += 1;
            if offset < total || pass == 0 {
                pending = pending_files::begin(
                    &mut state,
                    reader,
                    Action::Read { offset: if offset < total { offset } else { 0 }, length: chunk },
                )
                .unwrap();
            }
            state.finish_frame();
        }
    }
    assert!(!state.drain_pending());
    assert!(!state.drain_failed());
    assert_eq!(records.borrow().file_calls.len(), 2048);
}
