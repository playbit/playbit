#![allow(
    clippy::unwrap_used,
    reason = "Tests exercise File ownership transitions and terminal results."
)]
use super::super::pending_data::tests::{Records, state};
use super::*;
use std::collections::BTreeMap;

fn fixture() -> (State, Rc<RefCell<Records>>, FileHandle) {
    let (mut state, records, _) = state();
    let root = state.files.enumerate(&mut state.capabilities, &[[1; 16]]).unwrap()[0];
    (state, records, root)
}
fn complete(records: &Rc<RefCell<Records>>, index: u64, output: Output) {
    records.borrow_mut().file_results.insert((1 << 63) | index, Ok(Arc::new(output)));
}
fn open(
    state: &mut State,
    records: &Rc<RefCell<Records>>,
    root: FileHandle,
    mode: u32,
    engine: u64,
) -> FileHandle {
    let pending = begin(state, root, Action::Open { path: b"note".to_vec(), mode }).unwrap();
    let index = records.borrow().file_calls.len() as u64;
    complete(records, index, Output::File { handle: engine });
    let slot = take(state, pending, Opcode::FileOpenTake, |_, slot| Ok(slot.unwrap())).unwrap();
    FileHandle(slot)
}
#[test]
fn duplicate_host_token_preserves_the_older_operation_until_fail_stop_cleanup() {
    let (mut state, records, root) = fixture();
    let first = begin(&mut state, root, Action::Status).unwrap();
    // Deliberately violate the host contract by recycling its live token.
    records.borrow_mut().file_calls.clear();
    assert_eq!(begin(&mut state, root, Action::Status), Err(HostError::Internal));
    assert!(state.halted);
    assert!(records.borrow().file_released.is_empty());
    assert!(state.capabilities.operation(super::super::handles::raw_operation(first)).is_ok());
    assert_eq!(state.capabilities.table.reserved(), 0);
    drop(state);
    assert_eq!(records.borrow().file_released, vec![((1 << 63) | 1, false)]);
}

#[test]
fn close_seals_writer_and_transfers_its_credit_to_recovery_even_at_the_file_ceiling() {
    let (mut state, records, root) = fixture();
    let writer = open(&mut state, &records, root, FILE_WRITE, 11);
    let fill = state
        .files
        .capacity()
        .reserve(crate::runtime::host::files::capacity::Kind::File, 509)
        .unwrap();
    assert!(matches!(
        state.files.reserve(&mut state.capabilities, Kind::Reader),
        Err(HostError::LimitExceeded)
    ));
    assert_eq!(
        begin(&mut state, writer, Action::Close { flags: FILE_CLOSE_SYNC | FILE_CLOSE_DISCARD }),
        Err(HostError::InvalidArgument)
    );
    let status = begin(&mut state, writer, Action::Status).unwrap();
    let close = begin(&mut state, writer, Action::Close { flags: 0 }).unwrap();
    assert_eq!(
        begin(&mut state, writer, Action::Write { offset: 0, bytes: vec![] }),
        Err(HostError::StaleHandle)
    );
    complete(&records, 2, FileStatus::private(4, false).into());
    take(&mut state, status, Opcode::FileStatusTake, |_, _| Ok(())).unwrap();
    complete(
        &records,
        3,
        Output::Close {
            error: HostError::Conflict as i32,
            publication: NOT_PUBLISHED,
            recovery: Some(12),
        },
    );
    let slot = take(&mut state, close, Opcode::FileCloseTake, |output, slot| {
        assert!(matches!(output, Output::Close { recovery: Some(12), .. }));
        Ok(slot.unwrap())
    })
    .unwrap();
    let recovery = FileHandle(slot);
    assert_eq!(state.files.file(&state.capabilities, recovery).unwrap().kind, Kind::Recovery);
    assert_eq!(
        begin(&mut state, recovery, Action::Write { offset: 0, bytes: vec![] }),
        Err(HostError::InvalidArgument)
    );
    assert!(matches!(
        state.files.reserve(&mut state.capabilities, Kind::Reader),
        Err(HostError::LimitExceeded)
    ));
    drop(fill);
    assert_eq!(records.borrow().file_released.last(), Some(&((1 << 63) | 3, true)));
}
#[test]
fn only_successful_listing_delivery_advances_and_close_settles_a_pending_next() {
    let (mut state, records, root) = fixture();
    let pending = begin(&mut state, root, Action::ListOpen).unwrap();
    complete(&records, 1, Output::List { handle: 11 });
    let slot =
        take(&mut state, pending, Opcode::FileListOpenTake, |_, slot| Ok(slot.unwrap())).unwrap();
    let list = FileListHandle(slot);
    let next = list_next(&mut state, list).unwrap();
    assert_eq!(list_next(&mut state, list), Err(HostError::Busy));
    complete(&records, 2, Output::Entry { kind: FILE_KIND_FILE, name: b"note".to_vec() });
    assert_eq!(
        take::<()>(&mut state, next, Opcode::FileListNextTake, |_, _| Err(
            HostError::BufferTooSmall
        )),
        Err(HostError::BufferTooSmall)
    );
    assert_eq!(list_next(&mut state, list), Err(HostError::Busy));
    dispose(&mut state, next).unwrap();
    let retry = list_next(&mut state, list).unwrap();
    assert_eq!(records.borrow().file_calls[2].action, Action::ListNext { index: 0 });
    complete(&records, 3, Output::Entry { kind: FILE_KIND_FILE, name: b"note".to_vec() });
    take(&mut state, retry, Opcode::FileListNextTake, |_, _| Ok(())).unwrap();
    let next = list_next(&mut state, list).unwrap();
    assert_eq!(records.borrow().file_calls[3].action, Action::ListNext { index: 1 });
    list_close(&mut state, list).unwrap();
    assert_eq!(
        take::<()>(&mut state, next, Opcode::FileListNextTake, |_, _| panic!(
            "closed list cannot deliver"
        )),
        Err(HostError::Closed)
    );
    assert_eq!(records.borrow().file_cleanups.len(), 1);
    assert_eq!(state.pending_data_bytes, 0);
    assert!(state.capabilities.operations().next().is_none());
}
#[test]
fn root_alias_close_is_local_and_ready_reads_cannot_cross_revocation() {
    let (mut state, records, root) = fixture();
    let second = state.files.enumerate(&mut state.capabilities, &[[1; 16]]).unwrap()[0];
    let pending =
        begin(&mut state, root, Action::Open { path: b"note".to_vec(), mode: FILE_READ }).unwrap();
    let close = begin(&mut state, root, Action::Close { flags: 0 }).unwrap();
    take(&mut state, close, Opcode::FileCloseTake, |output, slot| {
        assert!(matches!(output, Output::Close { error: 0, recovery: None, .. }));
        assert_eq!(slot, None);
        Ok(())
    })
    .unwrap();
    assert_eq!(records.borrow().file_calls.len(), 1);
    assert!(state.files.file(&state.capabilities, second).is_ok());
    complete(&records, 1, Output::File { handle: 11 });
    let slot =
        take(&mut state, pending, Opcode::FileOpenTake, |_, slot| Ok(slot.unwrap())).unwrap();
    let reader = FileHandle(slot);
    let read = begin(&mut state, reader, Action::Read { offset: 0, length: 8 }).unwrap();
    complete(&records, 2, Output::Read { bytes: b"private".to_vec() });
    let _revoked = state.files.revoke([1; 16]).unwrap();
    let peeks = records.borrow().file_peeks;
    assert_eq!(
        take::<()>(&mut state, read, Opcode::FileReadTake, |_, _| panic!("revoked output")),
        Err(HostError::AccessDenied)
    );
    assert_eq!(records.borrow().file_peeks, peeks);
    assert_eq!(records.borrow().file_released.last(), Some(&((1 << 63) | 2, false)));
    assert!(state.capabilities.operations().next().is_none());
}
#[test]
fn file_and_data_calls_share_pending_limits_and_disposal_keeps_accepted_credit() {
    let (mut state, records, root) = fixture();
    let reader = open(&mut state, &records, root, FILE_READ, 11);
    let volume = state.volume_handle([1; 16]).unwrap();
    let data = super::super::pending_data::begin(
        &mut state,
        volume,
        crate::runtime::host::DataOperation::Read {
            entry_id: [5; 15],
            object_id: [2; 16],
            fields: vec![[6; 16]],
        },
        Opcode::ObjectReadBegin,
    )
    .unwrap();
    let mut reads = Vec::new();
    for _ in 0..63 {
        reads.push(begin(&mut state, reader, Action::Read { offset: 0, length: 1 }).unwrap());
    }
    assert_eq!(begin(&mut state, reader, Action::Status), Err(HostError::LimitExceeded));
    dispose(&mut state, reads[0]).unwrap();
    // The host still owns the unfinished request and its source reservation.
    assert!(records.borrow().file_retention.contains_key(&((1 << 63) | 2)));
    let status = begin(&mut state, reader, Action::Status).unwrap();
    assert_eq!(status.slot, reads[0].slot);
    assert_ne!(status.generation, reads[0].generation);
    dispose(&mut state, data).unwrap();
    assert_eq!(records.borrow().released, vec![1]);
    let before = records.borrow().file_calls.len();
    state.pending_data_bytes = MAX_RETAINED_DATA_BYTES;
    assert_eq!(
        begin(&mut state, reader, Action::Read { offset: 0, length: 1 }),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(records.borrow().file_calls.len(), before);
    // Restore the fixture's exact accounting before ordinary destruction.
    state.pending_data_bytes = state
        .capabilities
        .operations()
        .map(|(_, pending)| pending)
        .map(|pending| match pending {
            PendingOperation::File(pending) => pending.bytes,
            _ => 0,
        })
        .sum();
    records.borrow_mut().file_results = BTreeMap::new();
}
