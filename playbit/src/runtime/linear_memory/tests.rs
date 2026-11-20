use super::layout::data::{field_input as f, value_input as v};
use super::*;
use crate::runtime::syscall::{FieldChange, Value};
use playbit_api::abi::syscall as contract;

fn word(memory: &mut [u8], offset: usize, value: u32) {
    memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn field(memory: &mut [u8], offset: usize, values: u32, count: u32) {
    word(memory, offset, f::SIZEOF);
    memory[offset + f::ID..offset + f::ID + 16].fill(7);
    word(memory, offset + f::VALUES, values);
    word(memory, offset + f::COUNT, count);
    word(memory, offset + f::STRIDE, v::SIZEOF);
    word(memory, offset + f::PRESENT, 1);
}

#[test]
fn offsets_counts_alignment_and_empty_spans_are_checked_without_overflow() {
    assert_eq!(span(0, u32::MAX, 0, 56, 8, 0), Ok(Region { start: 0, end: 0 }));
    assert_eq!(span(64, 8, 1, 56, 8, 1), Ok(Region { start: 8, end: 64 }));
    for (pointer, count, stride) in
        [(0, 1, 1), (3, 1, 8), (8, 2, 56), (u32::MAX, u32::MAX, u32::MAX)]
    {
        assert_eq!(span(64, pointer, count, stride, 8, u32::MAX), Err(HostError::InvalidArgument));
    }
    assert_eq!(span(64, 8, 2, 1, 1, 1), Err(HostError::LimitExceeded));
    let a = span(64, 8, 16, 1, 1, 16).expect("a");
    let b = span(64, 24, 16, 1, 1, 16).expect("b");
    assert!(!a.overlaps(b));
    assert!(a.overlaps(span(64, 23, 1, 1, 1, 1).expect("overlap")));
    assert!(!a.overlaps(span(0, 8, 0, 1, 1, 0).expect("empty")));
}

#[test]
fn input_cardinality_payload_and_fuel_are_bounded_before_effects() {
    let mut bytes = vec![0; 1024];
    field(&mut bytes, 32, 128, contract::MAX_VALUES + 1);
    let mut memory = Memory { data: &mut bytes, fuel: None };
    assert_eq!(input::changes(&mut memory, 32, 1, f::SIZEOF, None), Err(HostError::LimitExceeded));
    word(memory.data, 32 + f::COUNT, 1);
    word(memory.data, 128, v::SIZEOF);
    word(memory.data, 128 + v::KIND, 1); // Text.
    word(memory.data, 128 + v::LENGTH, contract::MAX_DATA_BYTES as u32 + 1);
    word(memory.data, 128 + v::DATA, 256);
    assert_eq!(input::changes(&mut memory, 32, 1, f::SIZEOF, None), Err(HostError::LimitExceeded));
    word(memory.data, 128 + v::LENGTH, 3);
    memory.data[256..259].copy_from_slice(b"abc");
    memory.fuel = Some(1);
    assert_eq!(input::changes(&mut memory, 32, 1, f::SIZEOF, None), Err(HostError::LimitExceeded));
    assert_eq!(memory.fuel, Some(0));
}

#[test]
fn all_value_tags_decode_from_bytes_with_absent_and_empty_fields_distinct() {
    let mut bytes = vec![0; 1024];
    field(&mut bytes, 32, 128, 7);
    for index in 0..7 {
        word(&mut bytes, 128 + index * 80, v::SIZEOF);
        word(&mut bytes, 128 + index * 80 + v::KIND, index as u32 + 1);
    }
    word(&mut bytes, 128 + v::LENGTH, 2);
    word(&mut bytes, 128 + v::DATA, 700);
    bytes[700..702].copy_from_slice("é".as_bytes());
    word(&mut bytes, 128 + 80 + v::SCALAR, 1);
    bytes[128 + 160 + v::SCALAR..128 + 160 + v::SCALAR + 8]
        .copy_from_slice(&(-7_i64).to_le_bytes());
    bytes[128 + 240 + v::SCALAR..128 + 240 + v::SCALAR + 8].copy_from_slice(&3.5_f64.to_le_bytes());
    word(&mut bytes, 128 + 320 + v::LENGTH, 2);
    word(&mut bytes, 128 + 320 + v::DATA, 702);
    bytes[702..704].copy_from_slice(&[0, 255]);
    bytes[128 + 400 + v::ID1..128 + 400 + v::ID2 + 16].fill(6);
    bytes[128 + 400 + v::ENTRY_ID..128 + 400 + v::ENTRY_ID + 15].fill(5);
    bytes[128 + 480 + v::ID1..128 + 480 + v::ID1 + 16].fill(6);
    bytes[128 + 480 + v::ENTRY_ID..128 + 480 + v::ENTRY_ID + 15].fill(7);
    let mut memory = Memory { data: &mut bytes, fuel: None };
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None).expect("values"),
        vec![FieldChange {
            field_id: [7; 16],
            values: Some(vec![
                Value::Text("é".into()),
                Value::Boolean(true),
                Value::Integer(-7),
                Value::Float(3.5),
                Value::Bytes(vec![0, 255]),
                Value::Reference { volume_id: [6; 16], database_id: [5; 15], object_id: [6; 16] },
                Value::File { volume_id: [6; 16], entry_id: [7; 15] }
            ]),
        }]
    );
    word(memory.data, 32 + f::COUNT, 0);
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None).expect("empty")[0].values,
        Some(vec![])
    );
    word(memory.data, 32 + f::PRESENT, 0);
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None).expect("absent")[0].values,
        None
    );
}

#[test]
fn malformed_tags_bits_text_nested_spans_and_aliases_are_rejected() {
    let mut base = vec![0; 1024];
    field(&mut base, 32, 128, 1);
    word(&mut base, 128, v::SIZEOF);
    word(&mut base, 128 + v::KIND, 1);
    word(&mut base, 128 + v::LENGTH, 3);
    word(&mut base, 128 + v::DATA, 256);
    base[256..259].copy_from_slice(b"abc");
    let corruptions = [
        (32 + f::PRESENT, 2),
        (32 + f::VALUES, 129),
        (32 + f::VALUES, 0),
        (128 + v::KIND, 0),
        (128 + v::KIND, 8),
        (128 + v::SCALAR, 1),
        (128 + v::ID1, 1),
        (128 + v::DATA, u32::MAX),
    ];
    for (offset, value) in corruptions {
        let mut bytes = base.clone();
        word(&mut bytes, offset, value);
        let mut memory = Memory { data: &mut bytes, fuel: None };
        assert_eq!(
            input::changes(&mut memory, 32, 1, f::SIZEOF, None),
            Err(HostError::InvalidArgument),
            "{offset}/{value}"
        );
    }
    for output in [32, 48, 128, 192, 256] {
        let mut bytes = base.clone();
        let mut memory = Memory { data: &mut bytes, fuel: None };
        let output = memory.admit(output, 1, 16, 1, 1).expect("output");
        assert_eq!(
            input::changes(&mut memory, 32, 1, f::SIZEOF, Some(output)),
            Err(HostError::InvalidArgument)
        );
    }
    base[256] = 255;
    let mut memory = Memory { data: &mut base, fuel: None };
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None),
        Err(HostError::InvalidArgument)
    );
    word(memory.data, 128 + v::KIND, 4);
    word(memory.data, 128 + v::LENGTH, 0);
    word(memory.data, 128 + v::DATA, 0);
    memory.data[128 + v::SCALAR..128 + v::SCALAR + 8].copy_from_slice(&f64::NAN.to_le_bytes());
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None),
        Err(HostError::InvalidArgument)
    );
}

#[test]
fn aggregate_limits_apply_to_repeated_guest_spans_and_boundary_values_fit() {
    let mut bytes = vec![0; contract::MAX_DATA_BYTES + 4096 * 80 + 4096];
    field(&mut bytes, 32, 256, contract::MAX_VALUES);
    for index in 0..contract::MAX_VALUES as usize {
        word(&mut bytes, 256 + index * 80, v::SIZEOF);
        word(&mut bytes, 256 + index * 80 + v::KIND, 2); // False Boolean.
    }
    let mut memory = Memory { data: &mut bytes, fuel: None };
    assert_eq!(
        input::changes(&mut memory, 32, 1, f::SIZEOF, None).expect("max values")[0]
            .values
            .as_ref()
            .expect("present")
            .len(),
        4096
    );
    word(memory.data, 32 + f::COUNT, 1);
    word(memory.data, 256 + v::KIND, 5); // Bytes.
    word(memory.data, 256 + v::LENGTH, contract::MAX_DATA_BYTES as u32);
    word(memory.data, 256 + v::DATA, 512);
    assert!(input::changes(&mut memory, 32, 1, f::SIZEOF, None).is_ok());
    field(memory.data, 72, 256, 1);
    // Two fields reuse the same readable region; logical payload still counts twice.
    assert_eq!(input::changes(&mut memory, 32, 2, f::SIZEOF, None), Err(HostError::LimitExceeded));
}

#[test]
fn record_prefixes_default_missing_fields_and_reject_semantic_extension_tails() {
    let layout = record::Layout {
        minimum: 12,
        size: 16,
        alignment: 4,
        fields: &[(0, 4), (4, 4), (8, 4), (12, 4)],
    };
    let mut data = [0; 40];
    word(&mut data, 8, 12);
    word(&mut data, 16, 7);
    let mut memory = Memory { data: &mut data, fuel: None };
    let old = memory.record(8, layout, true, None).expect("old allocation");
    assert_eq!(old.u32(memory.data, 8), 7);
    assert_eq!(old.field::<4>(memory.data, 12), [0; 4]);
    word(memory.data, 8, 14);
    assert_eq!(memory.record(8, layout, true, None), Err(HostError::InvalidArgument));
    word(memory.data, 8, 24);
    assert!(memory.record(8, layout, true, None).is_ok());
    memory.data[31] = 1;
    assert_eq!(memory.record(8, layout, true, None), Err(HostError::Unsupported));
    // Output extensions belong to the caller and are never interpreted as input.
    let future = memory.record(8, layout, false, None).expect("future output allocation");
    future.initialize(memory.data, layout);
    future.publish(memory.data, layout);
    assert_eq!(future.u32(memory.data, 0), 24);
    assert_eq!(future.u32(memory.data, 4), 16);
    assert_eq!(memory.data[31], 1);
    word(memory.data, 8, 8);
    assert_eq!(memory.record(8, layout, false, None), Err(HostError::BufferTooSmall));
}

#[test]
fn record_arrays_admit_every_caller_slot_before_writes_and_preserve_stride_gaps() {
    use layout::data::range;
    let mut data = [0xa5; 128];
    word(&mut data, 32, 16);
    word(&mut data, 56, 20);
    let before = data;
    let mut memory = Memory { data: &mut data, fuel: None };
    let array =
        memory.record_array(32, 2, 24, range::LAYOUT, false, 2, None).expect("caller slots");
    let first = array.slot(memory.data, 0, 24);
    let second = array.slot(memory.data, 1, 24);
    first.initialize(memory.data, range::LAYOUT);
    first.publish(memory.data, range::LAYOUT);
    second.clear(memory.data);
    assert_eq!(first.u32(memory.data, 0), 16);
    assert_eq!(first.u32(memory.data, 4), 16);
    assert_eq!(second.u32(memory.data, 0), 20);
    assert_eq!(second.u32(memory.data, 4), 0);
    assert_eq!(&memory.data[48..56], &before[48..56]);
    assert_eq!(&memory.data[72..80], &before[72..80]);
    word(memory.data, 56, 28);
    let before = memory.data.to_vec();
    assert_eq!(
        memory.record_array(32, 2, 24, range::LAYOUT, false, 2, None),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(memory.data, before);
}
