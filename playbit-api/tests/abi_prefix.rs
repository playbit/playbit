//! Published syscall/1 prefixes. These literal fixtures are never regenerated
//! when records grow. Append a new release fixture while retaining this one.
use playbit_api::abi::{self, record::Record};

const fn same(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut index = 0;
    while index < a.len() {
        if a[index] != b[index] {
            return false;
        }
        index += 1;
    }
    true
}
const fn prefix<T: Record>(size: u32, alignment: usize, fields: &[(&str, &str, u32, u32)]) {
    assert!(core::mem::size_of::<T>() >= size as usize);
    assert!(core::mem::align_of::<T>() == alignment);
    assert!(T::MIN_SIZE == size);
    assert!(T::FIELDS.len() >= fields.len());
    let mut index = 0;
    while index < fields.len() {
        let actual = &T::FIELDS[index];
        let expected = fields[index];
        assert!(same(actual.name, expected.0));
        assert!(same(actual.kind, expected.1));
        assert!(actual.offset == expected.2 && actual.width == expected.3);
        index += 1;
    }
    while index < T::FIELDS.len() {
        assert!(T::FIELDS[index].offset >= size); // Never reuse former tail padding.
        index += 1;
    }
}

#[cfg(target_pointer_width = "32")]
const _: () = {
    prefix::<abi::data::EntryHandle>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume", "u64", 8, 8),
            ("entry_id", "[u8; 15]", 16, 15),
            ("reserved", "u8", 31, 1),
        ],
    );
    prefix::<abi::data::Range>(
        16,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("offset", "u32", 8, 4),
            ("count", "u32", 12, 4),
        ],
    );
    prefix::<abi::data::Volume>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("handle", "u64", 8, 8),
            ("id", "[u8; 16]", 16, 16),
            ("name_offset", "u32", 32, 4),
            ("name_count", "u32", 36, 4),
            ("tags_offset", "u32", 40, 4),
            ("tags_count", "u32", 44, 4),
            ("rights", "u32", 48, 4),
            ("flags", "u32", 52, 4),
            ("file", "u64", 56, 8),
            ("grant_id", "[u8; 16]", 64, 16),
        ],
    );
    prefix::<abi::data::Volumes>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("revision", "u64", 8, 8),
            ("volumes_data", "*mut Volume", 16, 4),
            ("volumes_capacity", "u32", 20, 4),
            ("volumes_count", "u32", 24, 4),
            ("volumes_stride", "u32", 28, 4),
            ("tags_data", "*mut Range", 32, 4),
            ("tags_capacity", "u32", 36, 4),
            ("tags_count", "u32", 40, 4),
            ("tags_stride", "u32", 44, 4),
            ("bytes_data", "*mut u8", 48, 4),
            ("bytes_capacity", "u32", 52, 4),
            ("bytes_count", "u32", 56, 4),
        ],
    );
    prefix::<abi::data::ValueInput>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("length", "u32", 12, 4),
            ("scalar", "u64", 16, 8),
            ("id1", "[u8; 16]", 24, 16),
            ("id2", "[u8; 16]", 40, 16),
            ("entry_id", "[u8; 15]", 56, 15),
            ("reserved", "u8", 71, 1),
            ("data", "*const u8", 72, 4),
        ],
    );
    prefix::<abi::data::FieldInput>(
        40,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("values", "*const ValueInput", 24, 4),
            ("count", "u32", 28, 4),
            ("stride", "u32", 32, 4),
            ("present", "u32", 36, 4),
        ],
    );
    prefix::<abi::data::Value>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("reserved", "u32", 12, 4),
            ("scalar", "u64", 16, 8),
            ("id1", "[u8; 16]", 24, 16),
            ("id2", "[u8; 16]", 40, 16),
            ("entry_id", "[u8; 15]", 56, 15),
            ("entry_reserved", "u8", 71, 1),
            ("bytes_offset", "u32", 72, 4),
            ("bytes_count", "u32", 76, 4),
        ],
    );
    prefix::<abi::data::Field>(
        40,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("values_offset", "u32", 24, 4),
            ("values_count", "u32", 28, 4),
            ("present", "u32", 32, 4),
            ("reserved", "u32", 36, 4),
        ],
    );
    prefix::<abi::data::Object>(
        68,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("type_id", "[u8; 16]", 8, 16),
            ("fields_data", "*mut Field", 24, 4),
            ("fields_capacity", "u32", 28, 4),
            ("fields_count", "u32", 32, 4),
            ("fields_stride", "u32", 36, 4),
            ("values_data", "*mut Value", 40, 4),
            ("values_capacity", "u32", 44, 4),
            ("values_count", "u32", 48, 4),
            ("values_stride", "u32", 52, 4),
            ("bytes_data", "*mut u8", 56, 4),
            ("bytes_capacity", "u32", 60, 4),
            ("bytes_count", "u32", 64, 4),
        ],
    );
    prefix::<abi::data::Query>(
        60,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("type_id", "[u8; 16]", 8, 16),
            ("after", "[u8; 16]", 24, 16),
            ("flags", "u32", 40, 4),
            ("limit", "u32", 44, 4),
            ("fields", "*const [u8; 16]", 48, 4),
            ("count", "u32", 52, 4),
            ("reserved", "u32", 56, 4),
        ],
    );
    prefix::<abi::data::ObjectInfo>(
        48,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("type_id", "[u8; 16]", 24, 16),
            ("fields_offset", "u32", 40, 4),
            ("fields_count", "u32", 44, 4),
        ],
    );
    prefix::<abi::data::Page>(
        92,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("next", "[u8; 16]", 8, 16),
            ("has_next", "u32", 24, 4),
            ("projected", "u32", 28, 4),
            ("objects_data", "*mut ObjectInfo", 32, 4),
            ("objects_capacity", "u32", 36, 4),
            ("objects_count", "u32", 40, 4),
            ("objects_stride", "u32", 44, 4),
            ("fields_data", "*mut Field", 48, 4),
            ("fields_capacity", "u32", 52, 4),
            ("fields_count", "u32", 56, 4),
            ("fields_stride", "u32", 60, 4),
            ("values_data", "*mut Value", 64, 4),
            ("values_capacity", "u32", 68, 4),
            ("values_count", "u32", 72, 4),
            ("values_stride", "u32", 76, 4),
            ("bytes_data", "*mut u8", 80, 4),
            ("bytes_capacity", "u32", 84, 4),
            ("bytes_count", "u32", 88, 4),
        ],
    );
    prefix::<abi::schema::FieldInput>(
        56,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_data", "*const u8", 24, 4),
            ("name_length", "u32", 28, 4),
            ("kind", "u32", 32, 4),
            ("flags", "u32", 36, 4),
            ("reference_type", "[u8; 16]", 40, 16),
        ],
    );
    prefix::<abi::schema::TypeInput>(
        48,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_data", "*const u8", 24, 4),
            ("name_length", "u32", 28, 4),
            ("fields", "*const FieldInput", 32, 4),
            ("count", "u32", 36, 4),
            ("stride", "u32", 40, 4),
            ("flags", "u32", 44, 4),
        ],
    );
    prefix::<abi::schema::Field>(
        56,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_offset", "u32", 24, 4),
            ("name_count", "u32", 28, 4),
            ("kind", "u32", 32, 4),
            ("flags", "u32", 36, 4),
            ("reference_type", "[u8; 16]", 40, 16),
        ],
    );
    prefix::<abi::schema::Type>(
        48,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_offset", "u32", 24, 4),
            ("name_count", "u32", 28, 4),
            ("fields_offset", "u32", 32, 4),
            ("fields_count", "u32", 36, 4),
            ("flags", "u32", 40, 4),
            ("reserved", "u32", 44, 4),
        ],
    );
    prefix::<abi::schema::Schema>(
        52,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("types_data", "*mut Type", 8, 4),
            ("types_capacity", "u32", 12, 4),
            ("types_count", "u32", 16, 4),
            ("types_stride", "u32", 20, 4),
            ("fields_data", "*mut Field", 24, 4),
            ("fields_capacity", "u32", 28, 4),
            ("fields_count", "u32", 32, 4),
            ("fields_stride", "u32", 36, 4),
            ("bytes_data", "*mut u8", 40, 4),
            ("bytes_capacity", "u32", 44, 4),
            ("bytes_count", "u32", 48, 4),
        ],
    );
    prefix::<abi::environment::Capabilities>(
        36,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("names_data", "*mut Range", 8, 4),
            ("names_capacity", "u32", 12, 4),
            ("names_count", "u32", 16, 4),
            ("names_stride", "u32", 20, 4),
            ("bytes_data", "*mut u8", 24, 4),
            ("bytes_capacity", "u32", 28, 4),
            ("bytes_count", "u32", 32, 4),
        ],
    );
    prefix::<abi::environment::Argument>(
        24,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("name_offset", "u32", 8, 4),
            ("name_count", "u32", 12, 4),
            ("value_offset", "u32", 16, 4),
            ("value_count", "u32", 20, 4),
        ],
    );
    prefix::<abi::environment::Arguments>(
        36,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("arguments_data", "*mut Argument", 8, 4),
            ("arguments_capacity", "u32", 12, 4),
            ("arguments_count", "u32", 16, 4),
            ("arguments_stride", "u32", 20, 4),
            ("bytes_data", "*mut u8", 24, 4),
            ("bytes_capacity", "u32", 28, 4),
            ("bytes_count", "u32", 32, 4),
        ],
    );
    prefix::<abi::environment::Participant>(
        88,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("account_id", "[u8; 16]", 8, 16),
            ("device_id", "[u8; 16]", 24, 16),
            ("instance_id", "[u8; 16]", 40, 16),
            ("build_id", "[u8; 32]", 56, 32),
        ],
    );
    prefix::<abi::files::FileReference>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume", "u64", 8, 8),
            ("entry_id", "[u8; 15]", 16, 15),
            ("reserved", "u8", 31, 1),
        ],
    );
    prefix::<abi::files::FileStatus>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("flags", "u32", 12, 4),
            ("file_size", "u64", 16, 8),
            ("modification_time", "i64", 24, 8),
        ],
    );
    prefix::<abi::files::FileCloseResult>(
        24,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("error", "i32", 8, 4),
            ("publication", "u32", 12, 4),
            ("recovery", "u64", 16, 8),
        ],
    );
    prefix::<abi::files::FileListEntry>(
        28,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("outcome", "u32", 8, 4),
            ("kind", "u32", 12, 4),
            ("name_data", "*mut u8", 16, 4),
            ("name_capacity", "u32", 20, 4),
            ("name_count", "u32", 24, 4),
        ],
    );
    prefix::<abi::pending::Selection>(
        40,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("present", "u32", 8, 4),
            ("reserved", "u32", 12, 4),
            ("volume", "u64", 16, 8),
            ("entry_id", "[u8; 15]", 24, 15),
            ("entry_reserved", "u8", 39, 1),
        ],
    );
    prefix::<abi::operations::Event>(
        24,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("generation", "u64", 8, 8),
            ("kind", "u32", 16, 4),
            ("target", "u32", 20, 4),
        ],
    );
    prefix::<abi::operations::Identity>(
        104,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("generation", "u64", 8, 8),
            ("account_id", "[u8; 16]", 16, 16),
            ("device_id", "[u8; 16]", 32, 16),
            ("instance_id", "[u8; 16]", 48, 16),
            ("session_id", "[u8; 32]", 64, 32),
            ("flags", "u32", 96, 4),
            ("reserved", "u32", 100, 4),
        ],
    );
    prefix::<abi::operations::App>(
        24,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("init", "Option<Init>", 8, 4),
            ("turn", "Option<Turn>", 12, 4),
            ("destroy", "Option<Destroy>", 16, 4),
            ("user_data", "*mut c_void", 20, 4),
        ],
    );
    prefix::<abi::authenticated::Metadata>(
        168,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume_id", "[u8; 16]", 8, 16),
            ("object_id", "[u8; 16]", 24, 16),
            ("type_id", "[u8; 16]", 40, 16),
            ("field_id", "[u8; 16]", 56, 16),
            ("revision", "[u8; 32]", 72, 32),
            ("principal_id", "[u8; 16]", 104, 16),
            ("device_id", "[u8; 16]", 120, 16),
            ("state", "u32", 136, 4),
            ("provenance", "u32", 140, 4),
            ("principal_kind", "u32", 144, 4),
            ("reserved", "u32", 148, 4),
            ("entry_id", "[u8; 15]", 152, 15),
            ("entry_reserved", "u8", 167, 1),
        ],
    );
    prefix::<abi::authenticated::Field>(
        40,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("metadata", "*mut Metadata", 8, 4),
            ("values_data", "*mut Value", 12, 4),
            ("values_capacity", "u32", 16, 4),
            ("values_count", "u32", 20, 4),
            ("values_stride", "u32", 24, 4),
            ("bytes_data", "*mut u8", 28, 4),
            ("bytes_capacity", "u32", 32, 4),
            ("bytes_count", "u32", 36, 4),
        ],
    );
    prefix::<abi::kv::Query>(
        56,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("root_level", "u32", 8, 4),
            ("root_digest", "[u8; 32]", 12, 32),
            ("has_root", "u32", 44, 4),
            ("limit", "u32", 48, 4),
            ("reserved", "u32", 52, 4),
        ],
    );
    prefix::<abi::kv::Read>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("offset", "u64", 8, 8),
            ("limit", "u32", 16, 4),
            ("has_root", "u32", 20, 4),
            ("root_level", "u32", 24, 4),
            ("root_digest", "[u8; 32]", 28, 32),
            ("reserved", "u32", 60, 4),
        ],
    );
};

#[cfg(target_pointer_width = "64")]
const _: () = {
    prefix::<abi::data::EntryHandle>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume", "u64", 8, 8),
            ("entry_id", "[u8; 15]", 16, 15),
            ("reserved", "u8", 31, 1),
        ],
    );
    prefix::<abi::data::Range>(
        16,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("offset", "u32", 8, 4),
            ("count", "u32", 12, 4),
        ],
    );
    prefix::<abi::data::Volume>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("handle", "u64", 8, 8),
            ("id", "[u8; 16]", 16, 16),
            ("name_offset", "u32", 32, 4),
            ("name_count", "u32", 36, 4),
            ("tags_offset", "u32", 40, 4),
            ("tags_count", "u32", 44, 4),
            ("rights", "u32", 48, 4),
            ("flags", "u32", 52, 4),
            ("file", "u64", 56, 8),
            ("grant_id", "[u8; 16]", 64, 16),
        ],
    );
    prefix::<abi::data::Volumes>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("revision", "u64", 8, 8),
            ("volumes_data", "*mut Volume", 16, 8),
            ("volumes_capacity", "u32", 24, 4),
            ("volumes_count", "u32", 28, 4),
            ("volumes_stride", "u32", 32, 4),
            ("tags_data", "*mut Range", 40, 8),
            ("tags_capacity", "u32", 48, 4),
            ("tags_count", "u32", 52, 4),
            ("tags_stride", "u32", 56, 4),
            ("bytes_data", "*mut u8", 64, 8),
            ("bytes_capacity", "u32", 72, 4),
            ("bytes_count", "u32", 76, 4),
        ],
    );
    prefix::<abi::data::ValueInput>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("length", "u32", 12, 4),
            ("scalar", "u64", 16, 8),
            ("id1", "[u8; 16]", 24, 16),
            ("id2", "[u8; 16]", 40, 16),
            ("entry_id", "[u8; 15]", 56, 15),
            ("reserved", "u8", 71, 1),
            ("data", "*const u8", 72, 8),
        ],
    );
    prefix::<abi::data::FieldInput>(
        48,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("values", "*const ValueInput", 24, 8),
            ("count", "u32", 32, 4),
            ("stride", "u32", 36, 4),
            ("present", "u32", 40, 4),
        ],
    );
    prefix::<abi::data::Value>(
        80,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("reserved", "u32", 12, 4),
            ("scalar", "u64", 16, 8),
            ("id1", "[u8; 16]", 24, 16),
            ("id2", "[u8; 16]", 40, 16),
            ("entry_id", "[u8; 15]", 56, 15),
            ("entry_reserved", "u8", 71, 1),
            ("bytes_offset", "u32", 72, 4),
            ("bytes_count", "u32", 76, 4),
        ],
    );
    prefix::<abi::data::Field>(
        40,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("values_offset", "u32", 24, 4),
            ("values_count", "u32", 28, 4),
            ("present", "u32", 32, 4),
            ("reserved", "u32", 36, 4),
        ],
    );
    prefix::<abi::data::Object>(
        88,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("type_id", "[u8; 16]", 8, 16),
            ("fields_data", "*mut Field", 24, 8),
            ("fields_capacity", "u32", 32, 4),
            ("fields_count", "u32", 36, 4),
            ("fields_stride", "u32", 40, 4),
            ("values_data", "*mut Value", 48, 8),
            ("values_capacity", "u32", 56, 4),
            ("values_count", "u32", 60, 4),
            ("values_stride", "u32", 64, 4),
            ("bytes_data", "*mut u8", 72, 8),
            ("bytes_capacity", "u32", 80, 4),
            ("bytes_count", "u32", 84, 4),
        ],
    );
    prefix::<abi::data::Query>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("type_id", "[u8; 16]", 8, 16),
            ("after", "[u8; 16]", 24, 16),
            ("flags", "u32", 40, 4),
            ("limit", "u32", 44, 4),
            ("fields", "*const [u8; 16]", 48, 8),
            ("count", "u32", 56, 4),
            ("reserved", "u32", 60, 4),
        ],
    );
    prefix::<abi::data::ObjectInfo>(
        48,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("type_id", "[u8; 16]", 24, 16),
            ("fields_offset", "u32", 40, 4),
            ("fields_count", "u32", 44, 4),
        ],
    );
    prefix::<abi::data::Page>(
        120,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("next", "[u8; 16]", 8, 16),
            ("has_next", "u32", 24, 4),
            ("projected", "u32", 28, 4),
            ("objects_data", "*mut ObjectInfo", 32, 8),
            ("objects_capacity", "u32", 40, 4),
            ("objects_count", "u32", 44, 4),
            ("objects_stride", "u32", 48, 4),
            ("fields_data", "*mut Field", 56, 8),
            ("fields_capacity", "u32", 64, 4),
            ("fields_count", "u32", 68, 4),
            ("fields_stride", "u32", 72, 4),
            ("values_data", "*mut Value", 80, 8),
            ("values_capacity", "u32", 88, 4),
            ("values_count", "u32", 92, 4),
            ("values_stride", "u32", 96, 4),
            ("bytes_data", "*mut u8", 104, 8),
            ("bytes_capacity", "u32", 112, 4),
            ("bytes_count", "u32", 116, 4),
        ],
    );
    prefix::<abi::schema::FieldInput>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_data", "*const u8", 24, 8),
            ("name_length", "u32", 32, 4),
            ("kind", "u32", 36, 4),
            ("flags", "u32", 40, 4),
            ("reference_type", "[u8; 16]", 44, 16),
        ],
    );
    prefix::<abi::schema::TypeInput>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_data", "*const u8", 24, 8),
            ("name_length", "u32", 32, 4),
            ("fields", "*const FieldInput", 40, 8),
            ("count", "u32", 48, 4),
            ("stride", "u32", 52, 4),
            ("flags", "u32", 56, 4),
        ],
    );
    prefix::<abi::schema::Field>(
        56,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_offset", "u32", 24, 4),
            ("name_count", "u32", 28, 4),
            ("kind", "u32", 32, 4),
            ("flags", "u32", 36, 4),
            ("reference_type", "[u8; 16]", 40, 16),
        ],
    );
    prefix::<abi::schema::Type>(
        48,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("id", "[u8; 16]", 8, 16),
            ("name_offset", "u32", 24, 4),
            ("name_count", "u32", 28, 4),
            ("fields_offset", "u32", 32, 4),
            ("fields_count", "u32", 36, 4),
            ("flags", "u32", 40, 4),
            ("reserved", "u32", 44, 4),
        ],
    );
    prefix::<abi::schema::Schema>(
        72,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("types_data", "*mut Type", 8, 8),
            ("types_capacity", "u32", 16, 4),
            ("types_count", "u32", 20, 4),
            ("types_stride", "u32", 24, 4),
            ("fields_data", "*mut Field", 32, 8),
            ("fields_capacity", "u32", 40, 4),
            ("fields_count", "u32", 44, 4),
            ("fields_stride", "u32", 48, 4),
            ("bytes_data", "*mut u8", 56, 8),
            ("bytes_capacity", "u32", 64, 4),
            ("bytes_count", "u32", 68, 4),
        ],
    );
    prefix::<abi::environment::Capabilities>(
        48,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("names_data", "*mut Range", 8, 8),
            ("names_capacity", "u32", 16, 4),
            ("names_count", "u32", 20, 4),
            ("names_stride", "u32", 24, 4),
            ("bytes_data", "*mut u8", 32, 8),
            ("bytes_capacity", "u32", 40, 4),
            ("bytes_count", "u32", 44, 4),
        ],
    );
    prefix::<abi::environment::Argument>(
        24,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("name_offset", "u32", 8, 4),
            ("name_count", "u32", 12, 4),
            ("value_offset", "u32", 16, 4),
            ("value_count", "u32", 20, 4),
        ],
    );
    prefix::<abi::environment::Arguments>(
        48,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("arguments_data", "*mut Argument", 8, 8),
            ("arguments_capacity", "u32", 16, 4),
            ("arguments_count", "u32", 20, 4),
            ("arguments_stride", "u32", 24, 4),
            ("bytes_data", "*mut u8", 32, 8),
            ("bytes_capacity", "u32", 40, 4),
            ("bytes_count", "u32", 44, 4),
        ],
    );
    prefix::<abi::environment::Participant>(
        88,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("account_id", "[u8; 16]", 8, 16),
            ("device_id", "[u8; 16]", 24, 16),
            ("instance_id", "[u8; 16]", 40, 16),
            ("build_id", "[u8; 32]", 56, 32),
        ],
    );
    prefix::<abi::files::FileReference>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume", "u64", 8, 8),
            ("entry_id", "[u8; 15]", 16, 15),
            ("reserved", "u8", 31, 1),
        ],
    );
    prefix::<abi::files::FileStatus>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("kind", "u32", 8, 4),
            ("flags", "u32", 12, 4),
            ("file_size", "u64", 16, 8),
            ("modification_time", "i64", 24, 8),
        ],
    );
    prefix::<abi::files::FileCloseResult>(
        24,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("error", "i32", 8, 4),
            ("publication", "u32", 12, 4),
            ("recovery", "u64", 16, 8),
        ],
    );
    prefix::<abi::files::FileListEntry>(
        32,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("outcome", "u32", 8, 4),
            ("kind", "u32", 12, 4),
            ("name_data", "*mut u8", 16, 8),
            ("name_capacity", "u32", 24, 4),
            ("name_count", "u32", 28, 4),
        ],
    );
    prefix::<abi::pending::Selection>(
        40,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("present", "u32", 8, 4),
            ("reserved", "u32", 12, 4),
            ("volume", "u64", 16, 8),
            ("entry_id", "[u8; 15]", 24, 15),
            ("entry_reserved", "u8", 39, 1),
        ],
    );
    prefix::<abi::operations::Event>(
        24,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("generation", "u64", 8, 8),
            ("kind", "u32", 16, 4),
            ("target", "u32", 20, 4),
        ],
    );
    prefix::<abi::operations::Identity>(
        104,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("generation", "u64", 8, 8),
            ("account_id", "[u8; 16]", 16, 16),
            ("device_id", "[u8; 16]", 32, 16),
            ("instance_id", "[u8; 16]", 48, 16),
            ("session_id", "[u8; 32]", 64, 32),
            ("flags", "u32", 96, 4),
            ("reserved", "u32", 100, 4),
        ],
    );
    prefix::<abi::operations::App>(
        40,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("init", "Option<Init>", 8, 8),
            ("turn", "Option<Turn>", 16, 8),
            ("destroy", "Option<Destroy>", 24, 8),
            ("user_data", "*mut c_void", 32, 8),
        ],
    );
    prefix::<abi::authenticated::Metadata>(
        168,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("volume_id", "[u8; 16]", 8, 16),
            ("object_id", "[u8; 16]", 24, 16),
            ("type_id", "[u8; 16]", 40, 16),
            ("field_id", "[u8; 16]", 56, 16),
            ("revision", "[u8; 32]", 72, 32),
            ("principal_id", "[u8; 16]", 104, 16),
            ("device_id", "[u8; 16]", 120, 16),
            ("state", "u32", 136, 4),
            ("provenance", "u32", 140, 4),
            ("principal_kind", "u32", 144, 4),
            ("reserved", "u32", 148, 4),
            ("entry_id", "[u8; 15]", 152, 15),
            ("entry_reserved", "u8", 167, 1),
        ],
    );
    prefix::<abi::authenticated::Field>(
        56,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("metadata", "*mut Metadata", 8, 8),
            ("values_data", "*mut Value", 16, 8),
            ("values_capacity", "u32", 24, 4),
            ("values_count", "u32", 28, 4),
            ("values_stride", "u32", 32, 4),
            ("bytes_data", "*mut u8", 40, 8),
            ("bytes_capacity", "u32", 48, 4),
            ("bytes_count", "u32", 52, 4),
        ],
    );
    prefix::<abi::kv::Query>(
        56,
        4,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("root_level", "u32", 8, 4),
            ("root_digest", "[u8; 32]", 12, 32),
            ("has_root", "u32", 44, 4),
            ("limit", "u32", 48, 4),
            ("reserved", "u32", 52, 4),
        ],
    );
    prefix::<abi::kv::Read>(
        64,
        8,
        &[
            ("size", "u32", 0, 4),
            ("written", "u32", 4, 4),
            ("offset", "u64", 8, 8),
            ("limit", "u32", 16, 4),
            ("has_root", "u32", 20, 4),
            ("root_level", "u32", 24, 4),
            ("root_digest", "[u8; 32]", 28, 32),
            ("reserved", "u32", 60, 4),
        ],
    );
};

#[test]
fn published_prefixes_compile() {}

#[cfg(target_pointer_width = "32")]
const _: () = prefix::<abi::bloblog::Blob>(
    16,
    4,
    &[
        ("size", "u32", 0, 4),
        ("written", "u32", 4, 4),
        ("data", "*const u8", 8, 4),
        ("length", "u32", 12, 4),
    ],
);
#[cfg(target_pointer_width = "64")]
const _: () = prefix::<abi::bloblog::Blob>(
    24,
    8,
    &[
        ("size", "u32", 0, 4),
        ("written", "u32", 4, 4),
        ("data", "*const u8", 8, 8),
        ("length", "u32", 16, 4),
    ],
);

const _: () = prefix::<abi::session::SessionInfo>(
    64,
    4,
    &[
        ("size", "u32", 0, 4),
        ("written", "u32", 4, 4),
        ("flags", "u32", 8, 4),
        ("reserved", "u32", 12, 4),
        ("internal_id", "[u8; 32]", 16, 32),
        ("key_secret", "[u8; 16]", 48, 16),
    ],
);
