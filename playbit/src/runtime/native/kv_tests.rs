#![allow(clippy::unwrap_used, reason = "Tests assert actual native SDK ownership and framing.")]
use super::*;
use crate::runtime::{
    host::{DataOperation, DataResult},
    syscall::pending_data::tests::state,
};
use playbit_api::{
    abi::{data::EntryHandle, kv},
    context::Completion,
};
use std::sync::Arc;

#[test]
fn native_kv_owns_binary_inputs_and_short_output_retains_the_snapshot() {
    let (state, records, volume) = state();
    let context = Box::new(Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const Context).cast_mut().cast();
    // SAFETY: the context lives on its active owner thread for the entire test.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
    let entry = EntryHandle {
        size: std::mem::size_of::<EntryHandle>() as u32,
        written: 0,
        volume: playbit_api::abi::data::VolumeHandle::new(volume.generation, volume.slot).0,
        entry_id: [5; 15],
        reserved: 0,
    };
    let mut key = vec![0, 255, 1];
    let mut value = vec![0, 128, 255];
    let put = sdk.begin_kv_put(entry, &key, &value).unwrap();
    key.fill(9);
    value.fill(9);
    assert_eq!(
        records.borrow().calls[0].operation,
        DataOperation::KvPut { entry_id: [5; 15], key: vec![0, 255, 1], value: vec![0, 128, 255] }
    );
    assert_eq!(sdk.take_kv_put(put), Ok(Completion::Pending));
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::KvWritten)));
    assert_eq!(sdk.take_kv_delete(put), Err(HostError::StaleHandle));
    assert_eq!(sdk.take_kv_put(put), Ok(Completion::Ready(())));
    let keys = sdk.begin_kv_keys(entry, None, None, 64).unwrap();
    let root = crate::store::key_value::Root { level: 0, digest: [7; 32] };
    records.borrow_mut().results.insert(
        2,
        Ok(Arc::new(DataResult::KvKeys(crate::store::key_value::Page {
            root: Some(root),
            next: None,
            keys: vec![crate::store::key_value::Key {
                bytes: vec![0, 255, 1],
                len_bytes: 3,
                preview: vec![],
            }],
        }))),
    );
    assert!(matches!(sdk.take_kv_keys(keys, &mut [0; 4]), Err(HostError::BufferTooSmall)));
    assert_eq!(records.borrow().released, vec![1]);
    let mut bytes = [0; 64];
    let Completion::Ready(page) = sdk.take_kv_keys(keys, &mut bytes).unwrap() else { panic!() };
    assert_eq!(page.keys.collect::<Vec<_>>(), [kv::Key { bytes: &[0, 255, 1], len_bytes: 3 }]);
    assert_eq!(page.root.unwrap().digest, [7; 32]);
    assert_eq!(records.borrow().released, vec![1, 2]);
    let read = sdk.begin_kv_read(entry, &[0, 255, 1], 0, 3, None).unwrap();
    records.borrow_mut().results.insert(
        3,
        Ok(Arc::new(DataResult::KvRead(crate::store::key_value::Chunk {
            root,
            offset: 0,
            total_bytes: 3,
            bytes: vec![0, 128, 255],
        }))),
    );
    let Completion::Ready(chunk) = sdk.take_kv_read(read, &mut bytes).unwrap() else { panic!() };
    assert_eq!(chunk.bytes, [0, 128, 255]);
    let delete = sdk.begin_kv_delete(entry, &[0, 255, 1]).unwrap();
    records.borrow_mut().results.insert(4, Ok(Arc::new(DataResult::KvDeleted)));
    assert_eq!(sdk.take_kv_delete(delete), Ok(Completion::Ready(())));
    assert_eq!(records.borrow().calls.len(), 4);
}

#[test]
fn native_kv_rejects_bad_admission_and_consumes_a_current_denial_without_copying() {
    let (state, records, volume) = state();
    let context = Box::new(Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const Context).cast_mut().cast();
    // SAFETY: the borrowed active context and all buffers live on this thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
    let entry = EntryHandle {
        size: std::mem::size_of::<EntryHandle>() as u32,
        written: 0,
        volume: playbit_api::abi::data::VolumeHandle::new(volume.generation, volume.slot).0,
        entry_id: [5; 15],
        reserved: 0,
    };
    assert_eq!(sdk.begin_kv_put(entry, &[], &[]), Err(HostError::InvalidArgument));
    assert_eq!(sdk.begin_kv_keys(entry, Some(&[1]), None, 1), Err(HostError::InvalidArgument));
    assert_eq!(sdk.begin_kv_read(entry, &[1], 1, 1, None), Err(HostError::InvalidArgument));
    assert_eq!(
        sdk.begin_kv_keys(entry, None, Some(kv::Root { level: 256, digest: [0; 32] }), 1),
        Err(HostError::InvalidArgument)
    );
    assert!(records.borrow().calls.is_empty());
    let read = sdk.begin_kv_read(entry, &[1], 0, 1, None).unwrap();
    records.borrow_mut().results.insert(1, Err(HostError::AccessDenied));
    let mut output = [0xa5; 64];
    assert!(matches!(sdk.take_kv_read(read, &mut output), Err(HostError::AccessDenied)));
    assert_eq!(output, [0xa5; 64]);
    assert_eq!(records.borrow().released, vec![1]);
    assert!(matches!(sdk.take_kv_read(read, &mut output), Err(HostError::StaleHandle)));
}
