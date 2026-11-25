//! wasm32 offsets extracted from the actual Rust compiler fixture.
//! C and Rust probes and the frozen prefix manifest verify these declarations.
//! Native adapters use native Record metadata instead of these offsets.
#![allow(
    dead_code,
    reason = "Each target uses a subset of the complete compiler-verified ABI layout table."
)]
use super::record::Layout;

pub(crate) mod data {
    pub(crate) mod entry_handle {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 32;
        pub(crate) const SIZEOF: u32 = 32;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const VOLUME: usize = 8;
        pub(crate) const ENTRY_ID: usize = 16;
        pub(crate) const RESERVED: usize = 31;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[(SIZE, 4), (WRITTEN, 4), (VOLUME, 8), (ENTRY_ID, 15), (RESERVED, 1)],
        };
    }
    pub(crate) mod range {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 16;
        pub(crate) const SIZEOF: u32 = 16;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const OFFSET: usize = 8;
        pub(crate) const COUNT: usize = 12;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[(SIZE, 4), (WRITTEN, 4), (OFFSET, 4), (COUNT, 4)],
        };
    }
    pub(crate) mod volume {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 80;
        pub(crate) const SIZEOF: u32 = 88;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const HANDLE: usize = 8;
        pub(crate) const ID: usize = 16;
        pub(crate) const NAME_OFFSET: usize = 32;
        pub(crate) const NAME_COUNT: usize = 36;
        pub(crate) const TAGS_OFFSET: usize = 40;
        pub(crate) const TAGS_COUNT: usize = 44;
        pub(crate) const RIGHTS: usize = 48;
        pub(crate) const FLAGS: usize = 52;
        pub(crate) const FILE: usize = 56;
        pub(crate) const GRANT_ID: usize = 64;
        pub(crate) const ROLE: usize = 80;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (HANDLE, 8),
                (ID, 16),
                (NAME_OFFSET, 4),
                (NAME_COUNT, 4),
                (TAGS_OFFSET, 4),
                (TAGS_COUNT, 4),
                (RIGHTS, 4),
                (FLAGS, 4),
                (FILE, 8),
                (GRANT_ID, 16),
                (ROLE, 4),
            ],
        };
    }
    pub(crate) mod volumes {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 64;
        pub(crate) const SIZEOF: u32 = 64;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const REVISION: usize = 8;
        pub(crate) const VOLUMES_DATA: usize = 16;
        pub(crate) const VOLUMES_CAPACITY: usize = 20;
        pub(crate) const VOLUMES_COUNT: usize = 24;
        pub(crate) const VOLUMES_STRIDE: usize = 28;
        pub(crate) const TAGS_DATA: usize = 32;
        pub(crate) const TAGS_CAPACITY: usize = 36;
        pub(crate) const TAGS_COUNT: usize = 40;
        pub(crate) const TAGS_STRIDE: usize = 44;
        pub(crate) const BYTES_DATA: usize = 48;
        pub(crate) const BYTES_CAPACITY: usize = 52;
        pub(crate) const BYTES_COUNT: usize = 56;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (REVISION, 8),
                (VOLUMES_DATA, 4),
                (VOLUMES_CAPACITY, 4),
                (VOLUMES_COUNT, 4),
                (VOLUMES_STRIDE, 4),
                (TAGS_DATA, 4),
                (TAGS_CAPACITY, 4),
                (TAGS_COUNT, 4),
                (TAGS_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
    pub(crate) mod value_input {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 80;
        pub(crate) const SIZEOF: u32 = 80;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const KIND: usize = 8;
        pub(crate) const LENGTH: usize = 12;
        pub(crate) const SCALAR: usize = 16;
        pub(crate) const ID1: usize = 24;
        pub(crate) const ID2: usize = 40;
        pub(crate) const ENTRY_ID: usize = 56;
        pub(crate) const RESERVED: usize = 71;
        pub(crate) const DATA: usize = 72;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (KIND, 4),
                (LENGTH, 4),
                (SCALAR, 8),
                (ID1, 16),
                (ID2, 16),
                (ENTRY_ID, 15),
                (RESERVED, 1),
                (DATA, 4),
            ],
        };
    }
    pub(crate) mod field_input {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 40;
        pub(crate) const SIZEOF: u32 = 40;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const VALUES: usize = 24;
        pub(crate) const COUNT: usize = 28;
        pub(crate) const STRIDE: usize = 32;
        pub(crate) const PRESENT: usize = 36;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (VALUES, 4),
                (COUNT, 4),
                (STRIDE, 4),
                (PRESENT, 4),
            ],
        };
    }
    pub(crate) mod value {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 80;
        pub(crate) const SIZEOF: u32 = 80;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const KIND: usize = 8;
        pub(crate) const RESERVED: usize = 12;
        pub(crate) const SCALAR: usize = 16;
        pub(crate) const ID1: usize = 24;
        pub(crate) const ID2: usize = 40;
        pub(crate) const ENTRY_ID: usize = 56;
        pub(crate) const ENTRY_RESERVED: usize = 71;
        pub(crate) const BYTES_OFFSET: usize = 72;
        pub(crate) const BYTES_COUNT: usize = 76;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (KIND, 4),
                (RESERVED, 4),
                (SCALAR, 8),
                (ID1, 16),
                (ID2, 16),
                (ENTRY_ID, 15),
                (ENTRY_RESERVED, 1),
                (BYTES_OFFSET, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
    pub(crate) mod field {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 40;
        pub(crate) const SIZEOF: u32 = 40;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const VALUES_OFFSET: usize = 24;
        pub(crate) const VALUES_COUNT: usize = 28;
        pub(crate) const PRESENT: usize = 32;
        pub(crate) const RESERVED: usize = 36;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (VALUES_OFFSET, 4),
                (VALUES_COUNT, 4),
                (PRESENT, 4),
                (RESERVED, 4),
            ],
        };
    }
    pub(crate) mod object {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 68;
        pub(crate) const SIZEOF: u32 = 68;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const TYPE_ID: usize = 8;
        pub(crate) const FIELDS_DATA: usize = 24;
        pub(crate) const FIELDS_CAPACITY: usize = 28;
        pub(crate) const FIELDS_COUNT: usize = 32;
        pub(crate) const FIELDS_STRIDE: usize = 36;
        pub(crate) const VALUES_DATA: usize = 40;
        pub(crate) const VALUES_CAPACITY: usize = 44;
        pub(crate) const VALUES_COUNT: usize = 48;
        pub(crate) const VALUES_STRIDE: usize = 52;
        pub(crate) const BYTES_DATA: usize = 56;
        pub(crate) const BYTES_CAPACITY: usize = 60;
        pub(crate) const BYTES_COUNT: usize = 64;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (TYPE_ID, 16),
                (FIELDS_DATA, 4),
                (FIELDS_CAPACITY, 4),
                (FIELDS_COUNT, 4),
                (FIELDS_STRIDE, 4),
                (VALUES_DATA, 4),
                (VALUES_CAPACITY, 4),
                (VALUES_COUNT, 4),
                (VALUES_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
    pub(crate) mod query {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 60;
        pub(crate) const SIZEOF: u32 = 60;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const TYPE_ID: usize = 8;
        pub(crate) const AFTER: usize = 24;
        pub(crate) const FLAGS: usize = 40;
        pub(crate) const LIMIT: usize = 44;
        pub(crate) const FIELDS: usize = 48;
        pub(crate) const COUNT: usize = 52;
        pub(crate) const RESERVED: usize = 56;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (TYPE_ID, 16),
                (AFTER, 16),
                (FLAGS, 4),
                (LIMIT, 4),
                (FIELDS, 4),
                (COUNT, 4),
                (RESERVED, 4),
            ],
        };
    }
    pub(crate) mod object_info {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 48;
        pub(crate) const SIZEOF: u32 = 48;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const TYPE_ID: usize = 24;
        pub(crate) const FIELDS_OFFSET: usize = 40;
        pub(crate) const FIELDS_COUNT: usize = 44;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (TYPE_ID, 16),
                (FIELDS_OFFSET, 4),
                (FIELDS_COUNT, 4),
            ],
        };
    }
    pub(crate) mod page {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 92;
        pub(crate) const SIZEOF: u32 = 92;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const NEXT: usize = 8;
        pub(crate) const HAS_NEXT: usize = 24;
        pub(crate) const PROJECTED: usize = 28;
        pub(crate) const OBJECTS_DATA: usize = 32;
        pub(crate) const OBJECTS_CAPACITY: usize = 36;
        pub(crate) const OBJECTS_COUNT: usize = 40;
        pub(crate) const OBJECTS_STRIDE: usize = 44;
        pub(crate) const FIELDS_DATA: usize = 48;
        pub(crate) const FIELDS_CAPACITY: usize = 52;
        pub(crate) const FIELDS_COUNT: usize = 56;
        pub(crate) const FIELDS_STRIDE: usize = 60;
        pub(crate) const VALUES_DATA: usize = 64;
        pub(crate) const VALUES_CAPACITY: usize = 68;
        pub(crate) const VALUES_COUNT: usize = 72;
        pub(crate) const VALUES_STRIDE: usize = 76;
        pub(crate) const BYTES_DATA: usize = 80;
        pub(crate) const BYTES_CAPACITY: usize = 84;
        pub(crate) const BYTES_COUNT: usize = 88;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (NEXT, 16),
                (HAS_NEXT, 4),
                (PROJECTED, 4),
                (OBJECTS_DATA, 4),
                (OBJECTS_CAPACITY, 4),
                (OBJECTS_COUNT, 4),
                (OBJECTS_STRIDE, 4),
                (FIELDS_DATA, 4),
                (FIELDS_CAPACITY, 4),
                (FIELDS_COUNT, 4),
                (FIELDS_STRIDE, 4),
                (VALUES_DATA, 4),
                (VALUES_CAPACITY, 4),
                (VALUES_COUNT, 4),
                (VALUES_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
}

pub(crate) mod schema {
    pub(crate) mod field_input {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 56;
        pub(crate) const SIZEOF: u32 = 56;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const NAME_DATA: usize = 24;
        pub(crate) const NAME_LENGTH: usize = 28;
        pub(crate) const KIND: usize = 32;
        pub(crate) const FLAGS: usize = 36;
        pub(crate) const REFERENCE_TYPE: usize = 40;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (NAME_DATA, 4),
                (NAME_LENGTH, 4),
                (KIND, 4),
                (FLAGS, 4),
                (REFERENCE_TYPE, 16),
            ],
        };
    }
    pub(crate) mod type_input {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 48;
        pub(crate) const SIZEOF: u32 = 48;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const NAME_DATA: usize = 24;
        pub(crate) const NAME_LENGTH: usize = 28;
        pub(crate) const FIELDS: usize = 32;
        pub(crate) const COUNT: usize = 36;
        pub(crate) const STRIDE: usize = 40;
        pub(crate) const FLAGS: usize = 44;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (NAME_DATA, 4),
                (NAME_LENGTH, 4),
                (FIELDS, 4),
                (COUNT, 4),
                (STRIDE, 4),
                (FLAGS, 4),
            ],
        };
    }
    pub(crate) mod field {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 56;
        pub(crate) const SIZEOF: u32 = 56;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const NAME_OFFSET: usize = 24;
        pub(crate) const NAME_COUNT: usize = 28;
        pub(crate) const KIND: usize = 32;
        pub(crate) const FLAGS: usize = 36;
        pub(crate) const REFERENCE_TYPE: usize = 40;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (NAME_OFFSET, 4),
                (NAME_COUNT, 4),
                (KIND, 4),
                (FLAGS, 4),
                (REFERENCE_TYPE, 16),
            ],
        };
    }
    pub(crate) mod type_record {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 48;
        pub(crate) const SIZEOF: u32 = 48;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ID: usize = 8;
        pub(crate) const NAME_OFFSET: usize = 24;
        pub(crate) const NAME_COUNT: usize = 28;
        pub(crate) const FIELDS_OFFSET: usize = 32;
        pub(crate) const FIELDS_COUNT: usize = 36;
        pub(crate) const FLAGS: usize = 40;
        pub(crate) const RESERVED: usize = 44;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ID, 16),
                (NAME_OFFSET, 4),
                (NAME_COUNT, 4),
                (FIELDS_OFFSET, 4),
                (FIELDS_COUNT, 4),
                (FLAGS, 4),
                (RESERVED, 4),
            ],
        };
    }
    #[allow(clippy::module_inception, reason = "Domain and public record are both named Schema.")]
    pub(crate) mod schema {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 52;
        pub(crate) const SIZEOF: u32 = 52;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const TYPES_DATA: usize = 8;
        pub(crate) const TYPES_CAPACITY: usize = 12;
        pub(crate) const TYPES_COUNT: usize = 16;
        pub(crate) const TYPES_STRIDE: usize = 20;
        pub(crate) const FIELDS_DATA: usize = 24;
        pub(crate) const FIELDS_CAPACITY: usize = 28;
        pub(crate) const FIELDS_COUNT: usize = 32;
        pub(crate) const FIELDS_STRIDE: usize = 36;
        pub(crate) const BYTES_DATA: usize = 40;
        pub(crate) const BYTES_CAPACITY: usize = 44;
        pub(crate) const BYTES_COUNT: usize = 48;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (TYPES_DATA, 4),
                (TYPES_CAPACITY, 4),
                (TYPES_COUNT, 4),
                (TYPES_STRIDE, 4),
                (FIELDS_DATA, 4),
                (FIELDS_CAPACITY, 4),
                (FIELDS_COUNT, 4),
                (FIELDS_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
}

pub(crate) mod environment {
    pub(crate) mod capabilities {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 36;
        pub(crate) const SIZEOF: u32 = 36;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const NAMES_DATA: usize = 8;
        pub(crate) const NAMES_CAPACITY: usize = 12;
        pub(crate) const NAMES_COUNT: usize = 16;
        pub(crate) const NAMES_STRIDE: usize = 20;
        pub(crate) const BYTES_DATA: usize = 24;
        pub(crate) const BYTES_CAPACITY: usize = 28;
        pub(crate) const BYTES_COUNT: usize = 32;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (NAMES_DATA, 4),
                (NAMES_CAPACITY, 4),
                (NAMES_COUNT, 4),
                (NAMES_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
    pub(crate) mod argument {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 24;
        pub(crate) const SIZEOF: u32 = 24;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const NAME_OFFSET: usize = 8;
        pub(crate) const NAME_COUNT: usize = 12;
        pub(crate) const VALUE_OFFSET: usize = 16;
        pub(crate) const VALUE_COUNT: usize = 20;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (NAME_OFFSET, 4),
                (NAME_COUNT, 4),
                (VALUE_OFFSET, 4),
                (VALUE_COUNT, 4),
            ],
        };
    }
    pub(crate) mod arguments {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 36;
        pub(crate) const SIZEOF: u32 = 36;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ARGUMENTS_DATA: usize = 8;
        pub(crate) const ARGUMENTS_CAPACITY: usize = 12;
        pub(crate) const ARGUMENTS_COUNT: usize = 16;
        pub(crate) const ARGUMENTS_STRIDE: usize = 20;
        pub(crate) const BYTES_DATA: usize = 24;
        pub(crate) const BYTES_CAPACITY: usize = 28;
        pub(crate) const BYTES_COUNT: usize = 32;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ARGUMENTS_DATA, 4),
                (ARGUMENTS_CAPACITY, 4),
                (ARGUMENTS_COUNT, 4),
                (ARGUMENTS_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
    pub(crate) mod participant {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 88;
        pub(crate) const SIZEOF: u32 = 344;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ACCOUNT_ID: usize = 8;
        pub(crate) const DEVICE_ID: usize = 24;
        pub(crate) const INSTANCE_ID: usize = 40;
        pub(crate) const BUILD_ID: usize = 56;
        pub(crate) const DISPLAY_NAME: usize = 88;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ACCOUNT_ID, 16),
                (DEVICE_ID, 16),
                (INSTANCE_ID, 16),
                (BUILD_ID, 32),
                (DISPLAY_NAME, playbit_api::abi::environment::DISPLAY_NAME_BYTES),
            ],
        };
        pub(crate) const OUTPUT: Layout = LAYOUT.before_output_tail(DISPLAY_NAME);
    }
}

pub(crate) mod files {
    pub(crate) mod file_reference {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 32;
        pub(crate) const SIZEOF: u32 = 32;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const VOLUME: usize = 8;
        pub(crate) const ENTRY_ID: usize = 16;
        pub(crate) const RESERVED: usize = 31;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[(SIZE, 4), (WRITTEN, 4), (VOLUME, 8), (ENTRY_ID, 15), (RESERVED, 1)],
        };
    }
    pub(crate) mod file_status {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 32;
        pub(crate) const SIZEOF: u32 = 32;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const KIND: usize = 8;
        pub(crate) const FLAGS: usize = 12;
        pub(crate) const FILE_SIZE: usize = 16;
        pub(crate) const MODIFICATION_TIME: usize = 24;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (KIND, 4),
                (FLAGS, 4),
                (FILE_SIZE, 8),
                (MODIFICATION_TIME, 8),
            ],
        };
    }
    pub(crate) mod file_close_result {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 24;
        pub(crate) const SIZEOF: u32 = 24;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ERROR: usize = 8;
        pub(crate) const PUBLICATION: usize = 12;
        pub(crate) const RECOVERY: usize = 16;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[(SIZE, 4), (WRITTEN, 4), (ERROR, 4), (PUBLICATION, 4), (RECOVERY, 8)],
        };
    }
    pub(crate) mod file_list_entry {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 28;
        pub(crate) const SIZEOF: u32 = 28;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const OUTCOME: usize = 8;
        pub(crate) const KIND: usize = 12;
        pub(crate) const NAME_DATA: usize = 16;
        pub(crate) const NAME_CAPACITY: usize = 20;
        pub(crate) const NAME_COUNT: usize = 24;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (OUTCOME, 4),
                (KIND, 4),
                (NAME_DATA, 4),
                (NAME_CAPACITY, 4),
                (NAME_COUNT, 4),
            ],
        };
    }
}

pub(crate) mod pending {
    pub(crate) mod selection {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 40;
        pub(crate) const SIZEOF: u32 = 40;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const PRESENT: usize = 8;
        pub(crate) const RESERVED: usize = 12;
        pub(crate) const VOLUME: usize = 16;
        pub(crate) const ENTRY_ID: usize = 24;
        pub(crate) const ENTRY_RESERVED: usize = 39;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (PRESENT, 4),
                (RESERVED, 4),
                (VOLUME, 8),
                (ENTRY_ID, 15),
                (ENTRY_RESERVED, 1),
            ],
        };
    }
}

pub(crate) mod operations {
    pub(crate) mod event {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 24;
        pub(crate) const SIZEOF: u32 = 32;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const GENERATION: usize = 8;
        pub(crate) const KIND: usize = 16;
        pub(crate) const TARGET: usize = 20;
        pub(crate) const OBJECT_ID: usize = 24;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (GENERATION, 8),
                (KIND, 4),
                (TARGET, 4),
                (OBJECT_ID, 4),
            ],
        };
        pub(crate) const OUTPUT: Layout = LAYOUT.before_output_tail(OBJECT_ID);
    }
    pub(crate) mod identity {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 104;
        pub(crate) const SIZEOF: u32 = 360;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const GENERATION: usize = 8;
        pub(crate) const ACCOUNT_ID: usize = 16;
        pub(crate) const DEVICE_ID: usize = 32;
        pub(crate) const INSTANCE_ID: usize = 48;
        pub(crate) const SESSION_ID: usize = 64;
        pub(crate) const FLAGS: usize = 96;
        pub(crate) const RESERVED: usize = 100;
        pub(crate) const DISPLAY_NAME: usize = 104;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (GENERATION, 8),
                (ACCOUNT_ID, 16),
                (DEVICE_ID, 16),
                (INSTANCE_ID, 16),
                (SESSION_ID, 32),
                (FLAGS, 4),
                (RESERVED, 4),
                (DISPLAY_NAME, playbit_api::abi::environment::DISPLAY_NAME_BYTES),
            ],
        };
        pub(crate) const OUTPUT: Layout = LAYOUT.before_output_tail(DISPLAY_NAME);
    }
}

pub(crate) mod session {
    pub(crate) mod session_info {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 64;
        pub(crate) const SIZEOF: u32 = 64;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const FLAGS: usize = 8;
        pub(crate) const RESERVED: usize = 12;
        pub(crate) const INTERNAL_ID: usize = 16;
        pub(crate) const KEY_SECRET: usize = 48;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (FLAGS, 4),
                (RESERVED, 4),
                (INTERNAL_ID, 32),
                (KEY_SECRET, 16),
            ],
        };
    }
}

pub(crate) mod authenticated {
    pub(crate) mod metadata {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 168;
        pub(crate) const SIZEOF: u32 = 168;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const VOLUME_ID: usize = 8;
        pub(crate) const OBJECT_ID: usize = 24;
        pub(crate) const TYPE_ID: usize = 40;
        pub(crate) const FIELD_ID: usize = 56;
        pub(crate) const REVISION: usize = 72;
        pub(crate) const PRINCIPAL_ID: usize = 104;
        pub(crate) const DEVICE_ID: usize = 120;
        pub(crate) const STATE: usize = 136;
        pub(crate) const PROVENANCE: usize = 140;
        pub(crate) const PRINCIPAL_KIND: usize = 144;
        pub(crate) const RESERVED: usize = 148;
        pub(crate) const ENTRY_ID: usize = 152;
        pub(crate) const ENTRY_RESERVED: usize = 167;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (VOLUME_ID, 16),
                (OBJECT_ID, 16),
                (TYPE_ID, 16),
                (FIELD_ID, 16),
                (REVISION, 32),
                (PRINCIPAL_ID, 16),
                (DEVICE_ID, 16),
                (STATE, 4),
                (PROVENANCE, 4),
                (PRINCIPAL_KIND, 4),
                (RESERVED, 4),
                (ENTRY_ID, 15),
                (ENTRY_RESERVED, 1),
            ],
        };
    }
    pub(crate) mod field {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 40;
        pub(crate) const SIZEOF: u32 = 40;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const METADATA: usize = 8;
        pub(crate) const VALUES_DATA: usize = 12;
        pub(crate) const VALUES_CAPACITY: usize = 16;
        pub(crate) const VALUES_COUNT: usize = 20;
        pub(crate) const VALUES_STRIDE: usize = 24;
        pub(crate) const BYTES_DATA: usize = 28;
        pub(crate) const BYTES_CAPACITY: usize = 32;
        pub(crate) const BYTES_COUNT: usize = 36;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (METADATA, 4),
                (VALUES_DATA, 4),
                (VALUES_CAPACITY, 4),
                (VALUES_COUNT, 4),
                (VALUES_STRIDE, 4),
                (BYTES_DATA, 4),
                (BYTES_CAPACITY, 4),
                (BYTES_COUNT, 4),
            ],
        };
    }
}

pub(crate) mod kv {
    pub(crate) mod query {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 56;
        pub(crate) const SIZEOF: u32 = 56;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const ROOT_LEVEL: usize = 8;
        pub(crate) const ROOT_DIGEST: usize = 12;
        pub(crate) const HAS_ROOT: usize = 44;
        pub(crate) const LIMIT: usize = 48;
        pub(crate) const RESERVED: usize = 52;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (ROOT_LEVEL, 4),
                (ROOT_DIGEST, 32),
                (HAS_ROOT, 4),
                (LIMIT, 4),
                (RESERVED, 4),
            ],
        };
    }
    pub(crate) mod read {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 64;
        pub(crate) const SIZEOF: u32 = 64;
        pub(crate) const ALIGNMENT: u32 = 8;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const OFFSET: usize = 8;
        pub(crate) const LIMIT: usize = 16;
        pub(crate) const HAS_ROOT: usize = 20;
        pub(crate) const ROOT_LEVEL: usize = 24;
        pub(crate) const ROOT_DIGEST: usize = 28;
        pub(crate) const RESERVED: usize = 60;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[
                (SIZE, 4),
                (WRITTEN, 4),
                (OFFSET, 8),
                (LIMIT, 4),
                (HAS_ROOT, 4),
                (ROOT_LEVEL, 4),
                (ROOT_DIGEST, 32),
                (RESERVED, 4),
            ],
        };
    }
}

pub(crate) mod bloblog {
    pub(crate) mod blob {
        use super::super::Layout;
        pub(crate) const MIN_SIZE: u32 = 16;
        pub(crate) const SIZEOF: u32 = 16;
        pub(crate) const ALIGNMENT: u32 = 4;
        pub(crate) const SIZE: usize = 0;
        pub(crate) const WRITTEN: usize = 4;
        pub(crate) const DATA: usize = 8;
        pub(crate) const LENGTH: usize = 12;
        pub(crate) const LAYOUT: Layout = Layout {
            minimum: MIN_SIZE,
            size: SIZEOF,
            alignment: ALIGNMENT,
            fields: &[(SIZE, 4), (WRITTEN, 4), (DATA, 4), (LENGTH, 4)],
        };
    }
}
