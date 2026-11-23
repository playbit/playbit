#![allow(
    clippy::unwrap_used,
    reason = "Tests exercise the actual guest SDK and retained completions."
)]
use super::*;
use crate::runtime::{
    host::{DataOperation, DataResult},
    syscall::pending_data::tests::state,
};
use playbit_api::{
    abi::{bloblog, data::EntryHandle},
    context::Completion,
};
use std::sync::Arc;

#[test]
fn native_bloblog_admits_caller_strides_and_rejects_unknown_semantics_before_append() {
    let (state, records, volume) = state();
    let context = Box::new(Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const Context).cast_mut().cast();
    let volume = playbit_api::abi::data::VolumeHandle::new(volume.generation, volume.slot);
    #[repr(C)]
    struct Extended {
        blob: bloblog::Blob,
        extension: [u8; 8],
    }
    let stride = std::mem::size_of::<Extended>() as u32;
    let mut inputs = [
        Extended { blob: bloblog::Blob::default(), extension: [0xa5; 8] },
        Extended { blob: bloblog::Blob { size: stride, ..Default::default() }, extension: [0; 8] },
    ];
    let mut output = playbit_api::abi::pending::OperationHandle::default();
    for (size, expected) in
        [(7, HostError::InvalidArgument), (stride + 1, HostError::InvalidArgument)]
    {
        inputs[1].blob.size = size;
        // SAFETY: each claimed span is rejected before any out-of-slot field access.
        assert_eq!(
            unsafe {
                super::test_handle(
                    bloblog::pb_bloblog_append_begin(
                        raw,
                        volume,
                        [5; 15].as_ptr(),
                        &inputs[0].blob,
                        2,
                        stride,
                        [1; 16].as_ptr(),
                    ),
                    &mut output,
                )
            },
            expected as i64
        );
    }
    inputs[1].blob.size = stride;
    inputs[1].extension[0] = 1;
    // SAFETY: both stable records and their stride slots remain live.
    assert_eq!(
        unsafe {
            super::test_handle(
                bloblog::pb_bloblog_append_begin(
                    raw,
                    volume,
                    [5; 15].as_ptr(),
                    &inputs[0].blob,
                    2,
                    stride,
                    [1; 16].as_ptr(),
                ),
                &mut output,
            )
        },
        HostError::Unsupported as i64
    );
    assert!(records.borrow().calls.is_empty());
    assert_eq!(output.0, 0);
    inputs[1].extension[0] = 0;
    // SAFETY: zero semantic extension and nonzero stride padding are distinct.
    assert_eq!(
        unsafe {
            super::test_handle(
                bloblog::pb_bloblog_append_begin(
                    raw,
                    volume,
                    [5; 15].as_ptr(),
                    &inputs[0].blob,
                    2,
                    stride,
                    [1; 16].as_ptr(),
                ),
                &mut output,
            )
        },
        0
    );
    assert_eq!(
        records.borrow().calls[0].operation,
        DataOperation::BloblogAppend {
            append_id: [1; 16],
            log: [5; 15],
            blobs: vec![vec![], vec![]]
        }
    );
}

#[test]
fn native_bloblog_owns_inputs_and_short_take_never_repeats_append() {
    // Drive owned inputs and retained cursor/page receipts through the native SDK boundary.
    let (state, records, volume) = state();
    let context = Box::new(Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const Context).cast_mut().cast();
    let mut sdk_state = playbit_api::context::ContextState::default();
    // SAFETY: this active context and all borrowed buffers remain on their owner thread.
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut sdk_state) }.unwrap();
    let entry = EntryHandle {
        volume: playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot),
        entry_id: [5; 15],
        ..Default::default()
    };
    let mut payload = [0, 255, 128];
    let mut append_id = [1; 16];
    let append = sdk.begin_bloblog_append(entry, &append_id, &[&payload, &[]]).unwrap();
    payload.fill(9);
    append_id.fill(8);
    assert_eq!(
        records.borrow().calls[0].operation,
        DataOperation::BloblogAppend {
            append_id: [1; 16],
            log: [5; 15],
            blobs: vec![vec![0, 255, 128], vec![]]
        }
    );
    let mut cursor_bytes = [0; bloblog::MAX_CURSOR_BYTES];
    assert_eq!(sdk.take_bloblog_append(append, &mut cursor_bytes), Ok(Completion::Pending));
    let cursor = crate::store::bloblog::Cursor::parse(&format!(
        "1/{}/{}/{}/0/{}",
        crate::store::Id::from_bytes([1; 16]),
        crate::store::EntryId::from_bytes([5; 15]).unwrap(),
        crate::identifier::encode(&[1; 8]),
        crate::store::Id::from_bytes([0; 16])
    ))
    .unwrap();
    records
        .borrow_mut()
        .results
        .insert(1, Ok(Arc::new(DataResult::BloblogAppended(cursor.clone()))));
    assert!(matches!(sdk.take_bloblog_read(append, &mut [0; 128]), Err(HostError::StaleHandle)));
    assert_eq!(sdk.take_bloblog_append(append, &mut [0; 1]), Err(HostError::BufferTooSmall));
    assert!(records.borrow().released.is_empty());
    assert_eq!(
        sdk.take_bloblog_append(append, &mut cursor_bytes),
        Ok(Completion::Ready(cursor.as_str()))
    );
    assert_eq!(sdk.take_bloblog_append(append, &mut cursor_bytes), Err(HostError::StaleHandle));
    let read = sdk.begin_bloblog_read(entry, Some(cursor.as_str())).unwrap();
    records.borrow_mut().results.insert(
        2,
        Ok(Arc::new(DataResult::BloblogPage(crate::store::bloblog::Page {
            entries: vec![],
            cursor: cursor.clone(),
            more: false,
        }))),
    );
    let mut bytes = [0xa5; 256];
    assert!(matches!(sdk.take_bloblog_read(read, &mut bytes[..1]), Err(HostError::BufferTooSmall)));
    assert_eq!(bytes, [0xa5; 256]);
    let Completion::Ready(page) = sdk.take_bloblog_read(read, &mut bytes).unwrap() else {
        panic!()
    };
    assert_eq!(page.cursor, cursor.as_str());
    assert_eq!(page.entries.len(), 0);
    assert!(!page.more);
    assert_eq!(records.borrow().calls.len(), 2);
    assert_eq!(records.borrow().released, [1, 2]);
}

#[test]
fn native_bloblog_rejects_bad_inputs_and_authority_without_copying_output() {
    // Check semantic bounds and terminal denial at the real syscall.
    let (state, records, volume) = state();
    let context = Box::new(Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const Context).cast_mut().cast();
    let mut sdk_state = playbit_api::context::ContextState::default();
    // SAFETY: the context lives on this active owner thread.
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut sdk_state) }.unwrap();
    let entry = EntryHandle {
        volume: playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot),
        entry_id: [5; 15],
        ..Default::default()
    };
    assert_eq!(sdk.begin_bloblog_append(entry, &[1; 16], &[]), Err(HostError::InvalidArgument));
    assert_eq!(
        sdk.begin_bloblog_append(entry, &[1; 16], &[&[] as &[u8]; bloblog::MAX_BLOBS + 1]),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(sdk.begin_bloblog_read(entry, Some("invalid")), Err(HostError::InvalidArgument));
    assert_eq!(sdk.begin_bloblog_read(entry, Some("")), Err(HostError::InvalidArgument));
    assert!(records.borrow().calls.is_empty());
    let read = sdk.begin_bloblog_read(entry, None).unwrap();
    records.borrow_mut().results.insert(1, Err(HostError::AccessDenied));
    let mut bytes = [0xa5; 128];
    assert!(matches!(sdk.take_bloblog_read(read, &mut bytes), Err(HostError::AccessDenied)));
    assert_eq!(bytes, [0xa5; 128]);
    assert_eq!(records.borrow().released, [1]);
    assert!(matches!(sdk.take_bloblog_read(read, &mut bytes), Err(HostError::StaleHandle)));
}
