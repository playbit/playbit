//! Actual native scalar/SDK borrows against the retained completion fixture.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::runtime::{
    host::{DataOperation, DataResult},
    syscall::pending_data::tests::state,
};
use playbit_api::abi::{data as abi, pending as ffi};
use std::sync::{Mutex, atomic::AtomicBool};

#[test]
fn native_schema_sdk_owns_inputs_and_retains_the_same_read_snapshot() {
    use playbit_api::context::{Completion, SchemaType};
    let (state, records, volume) = state();
    let context = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const crate::runtime::native::Context).cast_mut().cast();
    let volume = abi::EntryHandle {
        size: std::mem::size_of::<abi::EntryHandle>() as u32,
        written: 0,
        volume: abi::VolumeHandle::new(volume.generation, volume.slot).0,
        entry_id: [5; 15],
        reserved: 0,
    };
    // SAFETY: this test retains the active native context on its owner thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) = unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }
        .expect("active context");
    let mut name = String::from("Initial type");
    let pending = sdk
        .begin_establish_schema(
            volume,
            &[SchemaType::new([4; 16], &name, &[], false).expect("type")],
        )
        .expect("schema begin");
    name.clear();
    assert!(name.is_empty());
    {
        let retained = records.borrow();
        let DataOperation::SchemaEstablish { schema, .. } = &retained.calls[0].operation else {
            panic!("schema input")
        };
        assert_eq!(schema.types[0].name, "Initial type");
    }
    assert_eq!(sdk.take_establish_schema(pending), Ok(Completion::Pending));
    records.borrow_mut().results.insert(1, Ok(std::sync::Arc::new(DataResult::SchemaEstablished)));
    assert_eq!(sdk.take_establish_schema(pending), Ok(Completion::Ready(())));
    let pending = sdk.begin_read_schema(volume).expect("schema read begin");
    records.borrow_mut().results.insert(
        2,
        Ok(std::sync::Arc::new(DataResult::Schema(rt::Schema {
            types: vec![rt::TypeDeclaration {
                id: [4; 16],
                name: "Snapshot".into(),
                deprecated: false,
                fields: vec![],
            }],
        }))),
    );
    assert!(matches!(
        sdk.take_read_schema(pending, &mut [], &mut [], &mut []),
        Err(playbit_api::context::ReadError::BufferTooSmall { records: 1, values: 0, bytes: 8 })
    ));
    assert_eq!(records.borrow().released, vec![1]);
    let mut types = [playbit_api::abi::schema::Type::default()];
    let mut bytes = [0; 8];
    let Completion::Ready(schema) =
        sdk.take_read_schema(pending, &mut types, &mut [], &mut bytes).expect("schema take")
    else {
        panic!("ready schema")
    };
    assert_eq!(schema.types().next().expect("type").name, "Snapshot");
    assert_eq!(records.borrow().released, vec![1, 2]);
    assert_eq!(records.borrow().calls.len(), 2);
}

#[test]
fn native_begin_owns_inputs_and_take_admits_outputs_before_consuming() {
    let (state, records, volume) = state();
    let context = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const crate::runtime::native::Context).cast_mut().cast();
    let volume = abi::EntryHandle {
        size: std::mem::size_of::<abi::EntryHandle>() as u32,
        written: 0,
        volume: abi::VolumeHandle::new(volume.generation, volume.slot).0,
        entry_id: [5; 15],
        reserved: 0,
    };
    let object = [2; 16];
    let mut ids = [[3; 16]];
    let mut pending = ffi::OperationHandle::default();
    // SAFETY: all live inputs and the exclusive output are disjoint.
    assert_eq!(
        unsafe {
            crate::runtime::native::test_handle(
                ffi::pb_object_read_begin(raw, &volume, &object, ids.as_ptr(), 1),
                &mut pending,
            )
        },
        0
    );
    ids[0] = [9; 16];
    assert_eq!(ids[0], [9; 16]);
    assert_eq!(
        records.borrow().calls[0].operation,
        DataOperation::Read { entry_id: [5; 15], object_id: object, fields: vec![[3; 16]] }
    );
    records.borrow_mut().results.insert(
        1,
        Ok(std::sync::Arc::new(DataResult::Object(rt::ObjectRecord {
            id: object,
            type_id: [4; 16],
            fields: vec![rt::FieldChange {
                field_id: [3; 16],
                values: Some(vec![rt::Value::Text("snapshot".into())]),
            }],
        }))),
    );
    let mut fields = [abi::Field::default(); 1];
    let mut values = [abi::Value::default(); 1];
    let mut bytes = [0; 16];
    let mut output = abi::Object {
        type_id: [0; 16],
        ..abi::Object::new(
            abi::Buffer::new(fields.as_mut_ptr(), 1),
            abi::Buffer::new(values.as_mut_ptr(), 1),
            abi::Buffer::new(bytes.as_mut_ptr(), 0),
        )
    };
    // SAFETY: zero-capacity bytes are a valid size probe; descriptor is separate.
    assert_eq!(
        unsafe { ffi::pb_object_read_take(raw, pending, &mut output) },
        rt::HostError::BufferTooSmall as i64
    );
    assert_eq!((output.fields_count, output.values_count, output.bytes_count), (1, 1, 8));
    assert!(records.borrow().released.is_empty());
    output.bytes_capacity = 16;
    output.bytes_data = fields.as_mut_ptr().cast();
    let peeks = records.borrow().peeks.len();
    // SAFETY: overlapping spans are intentionally malformed, still within live allocations.
    assert_eq!(
        unsafe { ffi::pb_object_read_take(raw, pending, &mut output) },
        rt::HostError::InvalidArgument as i64
    );
    assert_eq!(records.borrow().peeks.len(), peeks);
    assert!(records.borrow().released.is_empty());
    // SAFETY: this test owns an active context on its current thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) = unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }
        .expect("active context");
    let playbit_api::context::Completion::Ready(read) =
        sdk.take_read_object(pending, &mut fields, &mut values, &mut bytes).expect("retained read")
    else {
        panic!("completion is ready")
    };
    assert_eq!(read.type_id, [4; 16]);
    assert!(matches!(
        read.field([3; 16]).expect("field").values().expect("values").next(),
        Some(playbit_api::context::ValueRef::Text("snapshot"))
    ));
    assert_eq!(records.borrow().calls.len(), 1);
    assert_eq!(records.borrow().released, vec![1]);
}

#[test]
fn native_query_retains_projected_page_and_edit_has_a_terminal_completion() {
    use playbit_api::context::{
        Completion, Context, FieldChange, PageStorage, Query, QueryError, Value,
    };
    let (state, records, volume) = state();
    let context = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const crate::runtime::native::Context).cast_mut().cast();
    let volume = abi::EntryHandle {
        size: std::mem::size_of::<abi::EntryHandle>() as u32,
        written: 0,
        volume: abi::VolumeHandle::new(volume.generation, volume.slot).0,
        entry_id: [5; 15],
        reserved: 0,
    };
    // SAFETY: this test owns the active context on its current thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { Context::from_raw(raw, &mut context_state) }.expect("active context");
    let query = sdk
        .begin_query_objects(
            volume,
            Query { type_id: [4; 16], after: None, limit: 2, fields: Some(&[[3; 16]]) },
        )
        .expect("query admitted");
    records.borrow_mut().results.insert(
        1,
        Ok(std::sync::Arc::new(DataResult::Page(rt::ObjectPage {
            objects: vec![[2; 16]],
            next: None,
            records: Some(vec![rt::ObjectRecord {
                id: [2; 16],
                type_id: [4; 16],
                fields: vec![rt::FieldChange {
                    field_id: [3; 16],
                    values: Some(vec![rt::Value::Text("snapshot".into())]),
                }],
            }]),
        }))),
    );
    assert!(matches!(
        sdk.take_query_objects(
            query,
            PageStorage { objects: &mut [], fields: &mut [], values: &mut [], bytes: &mut [] }
        ),
        Err(QueryError::BufferTooSmall { objects: 1, fields: 1, values: 1, bytes: 8 })
    ));
    assert!(records.borrow().released.is_empty());
    let mut objects = [abi::ObjectInfo::default(); 1];
    let mut fields = [abi::Field::default(); 1];
    let mut values = [abi::Value::default(); 1];
    let mut bytes = [0; 16];
    let Completion::Ready(page) = sdk
        .take_query_objects(
            query,
            PageStorage {
                objects: &mut objects,
                fields: &mut fields,
                values: &mut values,
                bytes: &mut bytes,
            },
        )
        .expect("retained page")
    else {
        panic!("page is ready")
    };
    assert_eq!(page.iter().next().expect("object").id, [2; 16]);
    assert_eq!(page.iter().next().expect("object").record.expect("projection").type_id, [4; 16]);
    let mut text = String::from("owned edit");
    let edit = {
        let values = [Value::text(&text).expect("value")];
        let fields = [FieldChange::set([3; 16], &values).expect("field")];
        sdk.begin_edit_object(volume, [2; 16], &fields).expect("edit admitted")
    };
    text.clear();
    assert_eq!(
        records.borrow().calls[1].operation,
        DataOperation::Edit {
            entry_id: [5; 15],
            object_id: [2; 16],
            fields: vec![rt::FieldChange {
                field_id: [3; 16],
                values: Some(vec![rt::Value::Text("owned edit".into())])
            }]
        }
    );
    assert_eq!(sdk.take_edit_object(edit), Ok(Completion::Pending));
    records.borrow_mut().results.insert(2, Ok(std::sync::Arc::new(DataResult::Edited)));
    assert_eq!(sdk.take_edit_object(edit), Ok(Completion::Ready(())));
    assert_eq!(sdk.take_edit_object(edit), Err(rt::HostError::StaleHandle));
    assert_eq!(records.borrow().calls.len(), 2);
    assert_eq!(records.borrow().released, vec![1, 2]);
}

#[test]
fn native_file_sdk_owns_write_input_and_retains_one_read_until_terminal_take() {
    use crate::runtime::host::files::{Action, Output};
    use playbit_api::context::{Completion, PollError};
    let (mut state, records, _) = state();
    let root = state.files.enumerate(&mut state.capabilities, &[[1; 16]]).unwrap()[0];
    let context = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const crate::runtime::native::Context).cast_mut().cast();
    // SAFETY: fixture retains a live active context on its callback thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
    let open = sdk.begin_open_file(root, b"note", playbit_api::abi::files::FILE_WRITE).unwrap();
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 1, Ok(std::sync::Arc::new(Output::File { handle: 11 })));
    let Completion::Ready(writer) = sdk.take_open_file(open).unwrap() else { panic!("writer") };
    let mut bytes = b"accepted".to_vec();
    let write = sdk.begin_write_file(writer, 0, &bytes).unwrap();
    bytes.fill(0);
    assert!(
        matches!(&records.borrow().file_calls[1].action, Action::Write { bytes, .. } if bytes == b"accepted")
    );
    assert_eq!(sdk.take_write_file(write), Ok(Completion::Pending));
    let close = sdk.begin_close_file(writer, 0).unwrap();
    assert_eq!(sdk.begin_write_file(writer, 0, b"late"), Err(rt::HostError::StaleHandle));
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 2, Ok(std::sync::Arc::new(Output::Written)));
    assert_eq!(sdk.take_write_file(write), Ok(Completion::Ready(())));
    records.borrow_mut().file_results.insert(
        (1 << 63) | 3,
        Ok(std::sync::Arc::new(Output::Close { error: 0, publication: 1, recovery: None })),
    );
    assert!(
        matches!(sdk.take_close_file(close), Ok(Completion::Ready(receipt)) if receipt.publication == 1)
    );
    let open = sdk.begin_open_file(root, b"note", playbit_api::abi::files::FILE_READ).unwrap();
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 4, Ok(std::sync::Arc::new(Output::File { handle: 12 })));
    let Completion::Ready(reader) = sdk.take_open_file(open).unwrap() else { panic!("reader") };
    let read = sdk.begin_read_file(reader, 2, 8).unwrap();
    assert_eq!(sdk.take_read_file(read, &mut []), Ok(Completion::Pending));
    records.borrow_mut().file_results.insert(
        (1 << 63) | 5,
        Ok(std::sync::Arc::new(Output::Read { bytes: b"snapshot".to_vec() })),
    );
    assert_eq!(sdk.take_read_file(read, &mut []), Err(PollError::BufferTooSmall(8)));
    let mut count = 55;
    let peeks = records.borrow().file_peeks;
    // SAFETY: malformed null span is rejected before any payload access.
    assert_eq!(
        unsafe {
            playbit_api::abi::files::pb_file_read_take(
                raw,
                read,
                std::ptr::null_mut(),
                1,
                &mut count,
            )
        },
        rt::HostError::InvalidArgument as i64
    );
    assert_eq!(records.borrow().file_peeks, peeks);
    assert_eq!(count, 55);
    let mut storage = [0; 8];
    assert_eq!(
        sdk.take_read_file(read, &mut storage),
        Ok(Completion::Ready(b"snapshot".as_slice()))
    );
    assert_eq!(
        sdk.take_read_file(read, &mut storage),
        Err(PollError::Host(rt::HostError::StaleHandle))
    );
}

#[test]
fn native_name_resolution_owns_utf8_and_returns_the_complete_stable_entry_id() {
    use playbit_api::context::Completion;
    let (state, records, volume) = state();
    let context = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*context as *const crate::runtime::native::Context).cast_mut().cast();
    // SAFETY: the fixture retains its active context on the owner thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) = unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }
        .expect("active native fixture context");
    // A maximum-length directory path crosses the SDK and native boundary intact.
    let expected = ("a".repeat(255) + "/").repeat(15) + &"b".repeat(253) + "/é";
    let mut name = expected.clone();
    assert_eq!(name.len(), playbit_api::abi::files::MAX_FILE_PATH_BYTES);
    assert_eq!(
        sdk.begin_resolve_entry(
            abi::VolumeHandle::new(volume.generation, volume.slot),
            &(name.clone() + "x")
        ),
        Err(rt::HostError::InvalidArgument)
    );
    let pending = sdk
        .begin_resolve_entry(
            playbit_api::context::VolumeHandle::new(volume.generation, volume.slot),
            &name,
        )
        .expect("valid entry-name request");
    name.clear();
    assert_eq!(records.borrow().calls[0].operation, DataOperation::ResolveEntry { name: expected });
    assert_eq!(sdk.take_resolve_entry(pending), Ok(Completion::Pending));
    records
        .borrow_mut()
        .results
        .insert(1, Ok(std::sync::Arc::new(DataResult::EntryResolved([7; 15]))));
    let Completion::Ready(entry) =
        sdk.take_resolve_entry(pending).expect("completed entry-name request")
    else {
        panic!("ready")
    };
    assert_eq!(entry.entry_id, [7; 15]);
    assert_eq!(
        (entry.volume_handle().generation(), entry.volume_handle().slot()),
        (volume.generation, volume.slot)
    );
    assert_eq!(records.borrow().released, [1]);
    assert_eq!(sdk.take_resolve_entry(pending), Err(rt::HostError::StaleHandle));
}

#[test]
fn owned_files_cross_callbacks_and_drop_without_publishing_or_spending_operation_slots() {
    use crate::runtime::host::files::Output;
    use playbit_api::{
        abi::files::FILE_WRITE,
        context::{Completion, FileResources},
    };
    let (state, records, _) = state();
    let native = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*native as *const crate::runtime::native::Context).cast_mut().cast();
    let owner = FileResources::default();
    let root = {
        // SAFETY: the fixture's native owner remains live on this serialized thread.
        let mut context_state = playbit_api::context::ContextState::default();
        let (mut sdk, _) =
            unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
        sdk.attach_files(&owner).unwrap();
        let mut rows = [abi::Volume::default()];
        let mut names = [0; 4];
        for _ in 0..600 {
            let volumes = sdk.available_volumes(&mut rows, &mut [], &mut names).unwrap();
            assert!(volumes.file(0).is_some());
            drop(volumes);
            sdk.flush_files().unwrap();
        }
        let mut volumes = sdk.available_volumes(&mut rows, &mut [], &mut names).unwrap();
        volumes.take_file(0).unwrap()
    };
    // A callback borrow ends; ownership retains only cleanup metadata.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
    sdk.attach_files(&owner).unwrap();
    let mut opened = root.open(&mut sdk, b"private", FILE_WRITE).unwrap();
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 1, Ok(std::sync::Arc::new(Output::File { handle: 11 })));
    let Completion::Ready(writer) = opened.take_open(&mut sdk).unwrap() else {
        panic!("opened writer")
    };
    let mut operations = Vec::new();
    for _ in 0..64 {
        operations.push(writer.write(&mut sdk, 0, b"accepted").unwrap());
    }
    assert!(matches!(writer.status(&mut sdk), Err(rt::HostError::LimitExceeded)));
    drop(writer);
    sdk.flush_files().unwrap();
    assert_eq!(records.borrow().file_cleanups.len(), 1);
    assert_eq!(records.borrow().file_cleanups[0].0, 11);
    assert!(
        records
            .borrow()
            .file_calls
            .iter()
            .all(|call| !matches!(call.action, crate::runtime::host::files::Action::Close { .. }))
    );
    drop(operations);
    sdk.flush_files().unwrap();
    assert!(native.state.lock().unwrap().capabilities.operations().next().is_none());
    assert!(records.borrow().file_retention.contains_key(&((1 << 63) | 2)));
    drop(root);
    sdk.flush_files().unwrap();
}

#[test]
fn owned_file_takes_accept_large_storage_and_preserve_short_buffer_retries() {
    use crate::runtime::host::files::Output;
    use playbit_api::{
        abi::files::{FILE_KIND_FILE, FILE_READ, MAX_FILE_READ},
        context::{Completion, FileResources, PollError},
    };
    let (state, records, _) = state();
    let native = Box::new(crate::runtime::native::Context {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
        poll_yielder: Default::default(),
    });
    let raw = (&*native as *const crate::runtime::native::Context).cast_mut().cast();
    let owner = FileResources::default();
    // SAFETY: the fixture owns an active native context on this serialized thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }.unwrap();
    sdk.attach_files(&owner).unwrap();
    let mut rows = [abi::Volume::default()];
    let mut names = [0; 4];
    let root = sdk.available_volumes(&mut rows, &mut [], &mut names).unwrap().take_file(0).unwrap();
    let mut open = root.open(&mut sdk, b"note", FILE_READ).unwrap();
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 1, Ok(std::sync::Arc::new(Output::File { handle: 11 })));
    let Completion::Ready(reader) = open.take_open(&mut sdk).unwrap() else { panic!("reader") };
    let mut read = reader.read(&mut sdk, 0, MAX_FILE_READ).unwrap();
    records.borrow_mut().file_results.insert(
        (1 << 63) | 2,
        Ok(std::sync::Arc::new(Output::Read { bytes: vec![42; MAX_FILE_READ as usize] })),
    );
    assert_eq!(
        read.take_read(&mut sdk, &mut []),
        Err(PollError::BufferTooSmall(MAX_FILE_READ as usize))
    );
    let mut storage = vec![0x55; MAX_FILE_READ as usize + 32];
    let Completion::Ready(bytes) = read.take_read(&mut sdk, &mut storage).unwrap() else {
        panic!("read")
    };
    assert_eq!(bytes, vec![42; MAX_FILE_READ as usize]);
    assert_eq!(&storage[MAX_FILE_READ as usize..], &[0x55; 32]);
    let mut open = root.list(&mut sdk).unwrap();
    records
        .borrow_mut()
        .file_results
        .insert((1 << 63) | 3, Ok(std::sync::Arc::new(Output::List { handle: 12 })));
    let Completion::Ready(list) = open.take_list(&mut sdk).unwrap() else { panic!("list") };
    let mut next = list.next(&mut sdk).unwrap();
    records.borrow_mut().file_results.insert(
        (1 << 63) | 4,
        Ok(std::sync::Arc::new(Output::Entry { kind: FILE_KIND_FILE, name: vec![b'x'; 255] })),
    );
    assert!(matches!(next.take_next(&mut sdk, &mut []), Err(PollError::BufferTooSmall(255))));
    let mut name = [0x55; 300];
    let Completion::Ready(Some(entry)) = next.take_next(&mut sdk, &mut name).unwrap() else {
        panic!("entry")
    };
    assert_eq!(entry.name, &[b'x'; 255]);
    assert_eq!(&name[255..], &[0x55; 45]);
    drop((reader, list, root));
    sdk.flush_files().unwrap();
    assert!(native.state.lock().unwrap().capabilities.operations().next().is_none());
}
