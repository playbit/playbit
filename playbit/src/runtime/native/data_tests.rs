use super::*;

fn scalar(kind: u32, scalar: u64) -> abi::ValueInput {
    abi::ValueInput {
        size: std::mem::size_of::<abi::ValueInput>() as u32,
        written: 0,
        kind,
        scalar,
        length: 0,
        id1: [0; 16],
        id2: [0; 16],
        entry_id: [0; 15],
        reserved: 0,
        data: std::ptr::null(),
    }
}
#[test]
fn nested_values_preserve_types_and_empty_deleted_fields() {
    let text = "Héllo\0世界";
    let binary = [0, 255, 1];
    let mut text_value = scalar(abi::TEXT, 0);
    text_value.data = text.as_ptr();
    text_value.length = text.len() as u32;
    let mut bytes_value = scalar(abi::BYTES, 0);
    bytes_value.data = binary.as_ptr();
    bytes_value.length = binary.len() as u32;
    let mut reference = scalar(abi::REFERENCE, 0);
    reference.id1 = [1; 16];
    reference.id2 = [2; 16];
    reference.entry_id = [5; 15];
    let mut file = scalar(abi::FILE, 0);
    file.id1 = [3; 16];
    file.entry_id = [7; 15];
    let values = [
        text_value,
        bytes_value,
        scalar(abi::BOOLEAN, 1),
        scalar(abi::INTEGER, i64::MIN as u64),
        scalar(abi::FLOAT, (-0.0f64).to_bits()),
        reference,
        file,
    ];
    let fields = [
        abi::FieldInput {
            size: std::mem::size_of::<abi::FieldInput>() as u32,
            written: 0,
            stride: std::mem::size_of::<abi::ValueInput>() as u32,
            id: [1; 16],
            values: values.as_ptr(),
            count: values.len() as u32,
            present: 1,
        },
        abi::FieldInput {
            size: std::mem::size_of::<abi::FieldInput>() as u32,
            written: 0,
            stride: std::mem::size_of::<abi::ValueInput>() as u32,
            id: [2; 16],
            values: std::ptr::null(),
            count: 0,
            present: 1,
        },
        abi::FieldInput {
            size: std::mem::size_of::<abi::FieldInput>() as u32,
            written: 0,
            stride: std::mem::size_of::<abi::ValueInput>() as u32,
            id: [3; 16],
            values: std::ptr::null(),
            count: 0,
            present: 0,
        },
    ];
    // SAFETY: every input and nested allocation remains valid through conversion.
    let decoded = unsafe {
        decode_changes(
            fields.as_ptr(),
            fields.len() as u32,
            std::mem::size_of::<abi::FieldInput>() as u32,
        )
    }
    .expect("valid records");
    assert_eq!(
        decoded[0].values,
        Some(vec![
            rt::Value::Text(text.into()),
            rt::Value::Bytes(binary.to_vec()),
            rt::Value::Boolean(true),
            rt::Value::Integer(i64::MIN),
            rt::Value::Float(-0.0),
            rt::Value::Reference { volume_id: [1; 16], database_id: [5; 15], object_id: [2; 16] },
            rt::Value::File { volume_id: [3; 16], entry_id: [7; 15] }
        ])
    );
    assert_eq!(decoded[1].values, Some(vec![]));
    assert_eq!(decoded[2].values, None);
}
#[test]
fn invalid_tags_scalars_utf8_and_nested_counts_fail_before_effects() {
    let invalid = [
        scalar(0, 0),
        scalar(abi::BOOLEAN, 2),
        scalar(abi::FLOAT, f64::NAN.to_bits()),
        scalar(abi::REFERENCE, 1),
        scalar(abi::TEXT, 1),
    ];
    for value in invalid {
        let mut remaining = contract::MAX_DATA_BYTES;
        // SAFETY: malformed scalar tags are rejected without reading any pointer.
        assert_eq!(
            unsafe { decode_value(&value, &mut remaining) },
            Err(HostError::InvalidArgument)
        );
    }
    let invalid_utf8 = [0xff];
    let mut value = scalar(abi::TEXT, 0);
    value.length = 1;
    value.data = invalid_utf8.as_ptr();
    let mut remaining = contract::MAX_DATA_BYTES;
    // SAFETY: the invalid UTF-8 is nevertheless a live one-byte allocation.
    assert_eq!(unsafe { decode_value(&value, &mut remaining) }, Err(HostError::InvalidArgument));
    let field = abi::FieldInput {
        size: std::mem::size_of::<abi::FieldInput>() as u32,
        written: 0,
        stride: std::mem::size_of::<abi::ValueInput>() as u32,
        id: [1; 16],
        values: std::ptr::null(),
        count: contract::MAX_VALUES + 1,
        present: 1,
    };
    // SAFETY: excessive count fails before its nested null pointer is borrowed.
    assert_eq!(
        unsafe { decode_changes(&field, 1, std::mem::size_of::<abi::FieldInput>() as u32) },
        Err(HostError::LimitExceeded)
    );
    let field = abi::FieldInput { count: 1, present: 0, ..field };
    assert_eq!(
        unsafe { decode_changes(&field, 1, std::mem::size_of::<abi::FieldInput>() as u32) },
        Err(HostError::InvalidArgument)
    );
}
#[test]
fn aggregate_payload_is_bounded_before_copying_the_next_value() {
    let data = vec![0; contract::MAX_DATA_BYTES];
    let mut value = scalar(abi::BYTES, 0);
    value.data = data.as_ptr();
    value.length = data.len() as u32;
    let values = [value, scalar(abi::BOOLEAN, 1)];
    let field = abi::FieldInput {
        size: std::mem::size_of::<abi::FieldInput>() as u32,
        written: 0,
        stride: std::mem::size_of::<abi::ValueInput>() as u32,
        id: [1; 16],
        values: values.as_ptr(),
        count: 2,
        present: 1,
    };
    // SAFETY: the maximum-size byte input and record array are live for this call.
    assert_eq!(
        unsafe { decode_changes(&field, 1, std::mem::size_of::<abi::FieldInput>() as u32) },
        Err(HostError::LimitExceeded)
    );
}
#[test]
fn output_regions_allow_empty_and_adjacent_but_never_overlap() {
    assert_eq!(disjoint(&[1..5, 5..10, 3..3]), Ok(()));
    assert_eq!(disjoint(&[1..5, 4..10]), Err(HostError::InvalidArgument));
    assert_eq!(disjoint(&[1..10, 3..5]), Err(HostError::InvalidArgument));
}

#[test]
fn object_output_round_trips_all_value_types_and_rejects_overlap_without_writes() {
    use crate::runtime::{
        host::DataResult, native::Context as NativeContext, syscall::pending_data::tests::state,
    };
    use playbit_api::{abi::pending as ffi, context::Completion};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    let record = rt::ObjectRecord {
        id: [5; 16],
        type_id: [6; 16],
        fields: vec![
            rt::FieldChange {
                field_id: [1; 16],
                values: Some(vec![
                    rt::Value::Text("Héllo\0世界".into()),
                    rt::Value::Bytes(vec![0, 255]),
                    rt::Value::Boolean(true),
                    rt::Value::Integer(i64::MIN),
                    rt::Value::Float(-0.0),
                    rt::Value::Reference {
                        volume_id: [7; 16],
                        database_id: [5; 15],
                        object_id: [8; 16],
                    },
                    rt::Value::File { volume_id: [6; 16], entry_id: [7; 15] },
                ]),
            },
            rt::FieldChange { field_id: [2; 16], values: Some(vec![]) },
            rt::FieldChange { field_id: [3; 16], values: None },
        ],
    };
    let (state, records, handle) = state();
    let handle = abi::VolumeHandle::new(handle.generation, handle.slot);
    let handle = abi::EntryHandle {
        size: std::mem::size_of::<abi::EntryHandle>() as u32,
        written: 0,
        volume: handle.0,
        entry_id: [5; 15],
        reserved: 0,
    };
    let context = Box::new(NativeContext {
        state: Mutex::new(state),
        owner: std::thread::current().id(),
        active: AtomicBool::new(true),
    });
    let raw = (&*context as *const NativeContext).cast_mut().cast();
    let projection = [[1; 16], [2; 16], [3; 16]];
    let mut fields = [abi::Field::default(); 3];
    let mut values = [abi::Value::default(); 7];
    let mut bytes = [0; 32];
    let mut output = abi::Object {
        type_id: [0x55; 16],
        ..abi::Object::new(
            abi::Buffer::new(fields.as_mut_ptr(), 3),
            abi::Buffer::new(values.as_mut_ptr(), 0),
            abi::Buffer::new(bytes.as_mut_ptr(), 32),
        )
    };
    let mut pending = ffi::OperationHandle::default();
    // SAFETY: Begin owns the live input; its fixed handle output is disjoint.
    assert_eq!(
        unsafe {
            crate::runtime::native::test_handle(
                ffi::pb_object_read_begin(raw, &handle, &[5; 16], projection.as_ptr(), 3),
                &mut pending,
            )
        },
        0
    );
    // SAFETY: output arrays are valid; no completion has been supplied yet.
    assert_eq!(
        unsafe { ffi::pb_object_read_take(raw, pending, &mut output) },
        HostError::WouldBlock as i64
    );
    assert_eq!((output.fields_count, output.values_count, output.bytes_count), (0, 0, 0));
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Object(record))));
    // SAFETY: context, inputs and all output allocations are live and structurally admitted.
    assert_eq!(
        unsafe { ffi::pb_object_read_take(raw, pending, &mut output) },
        HostError::BufferTooSmall as i64
    );
    assert_eq!((output.fields_count, output.values_count, output.bytes_count), (3, 7, 15));
    assert_eq!(fields, [abi::Field::default(); 3]);
    assert_eq!(output.type_id, [0x55; 16]);
    output.values_capacity = 7;
    output.bytes_data = fields.as_mut_ptr().cast();
    // SAFETY: all allocations are live; overlapping outputs must be rejected before writes.
    assert_eq!(
        unsafe { ffi::pb_object_read_take(raw, pending, &mut output) },
        HostError::InvalidArgument as i64
    );
    assert_eq!(fields, [abi::Field::default(); 3]);
    assert_eq!(output.type_id, [0x55; 16]);
    // SAFETY: the SDK borrows the live active context synchronously on this test thread.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut sdk, _) = unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state) }
        .expect("context");
    use playbit_api::context::HostLimit;
    assert_eq!(sdk.host_limit(HostLimit::QueryRecords), Ok(64));
    assert_eq!(sdk.host_limit(HostLimit::QueryValueBytes), Ok(1048576));
    assert_eq!(sdk.host_limit(HostLimit::ViewBytes), Ok(contract::MAX_CALL_BYTES as u32));
    // SAFETY: the context is live; unknown scalar IDs must not touch guest memory.
    assert_eq!(unsafe { playbit_api::abi::environment::pb_host_limit(raw, u32::MAX) }, -7);
    let platform = sdk.host_platform().expect("host platform");
    #[cfg(target_os = "macos")]
    assert_eq!(platform, playbit_api::context::HostPlatform::MacOs);
    assert!(playbit_api::context::HostPlatform::try_from(platform as i32).is_ok());
    let Completion::Ready(result) =
        sdk.take_read_object(pending, &mut fields, &mut values, &mut bytes).expect("record")
    else {
        panic!("ready retained record")
    };
    use playbit_api::context::ValueRef;
    let field = result.field([1; 16]).expect("field");
    let values = field.values().expect("values").collect::<Vec<_>>();
    assert_eq!(
        values,
        vec![
            ValueRef::Text("Héllo\0世界"),
            ValueRef::Bytes(&[0, 255]),
            ValueRef::Boolean(true),
            ValueRef::Integer(i64::MIN),
            ValueRef::Float(-0.0),
            ValueRef::Reference { volume: [7; 16], database: [5; 15], object: [8; 16] },
            ValueRef::File { volume: [6; 16], entry: [7; 15] }
        ]
    );
    assert_eq!(result.field([2; 16]).expect("empty field").values().expect("present").len(), 0);
    assert!(result.field([3; 16]).expect("deleted field").values().is_none());
    assert_eq!(records.borrow().released, vec![1]);
    assert_eq!(records.borrow().calls.len(), 1);
}
