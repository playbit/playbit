// Explicit wasm32 layouts verified against the independent Rust/C compiler fixture.
// Append fields only; retain the published minimum and complete field boundaries.
export interface RecordLayout { minimum: number; size: number; alignment: number; fields: readonly (readonly [number, number])[]; optionalOutputField?: number }
export const ABI = {
    entryHandle: {
        minimum: 32, size: 32, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 8], [16, 15], [31, 1]],
        offsets: { size: 0, written: 4, volume: 8, entry_id: 16, reserved: 31 },
    },
    range: {
        minimum: 16, size: 16, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4]],
        offsets: { size: 0, written: 4, offset: 8, count: 12 },
    },
    volume: {
        minimum: 80, size: 88, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 8], [16, 16], [32, 4], [36, 4], [40, 4], [44, 4], [48, 4], [52, 4], [56, 8], [64, 16], [80, 4]],
        offsets: { size: 0, written: 4, handle: 8, id: 16, name_offset: 32, name_count: 36, tags_offset: 40, tags_count: 44, rights: 48, flags: 52, file: 56, grant_id: 64, role: 80 },
    },
    volumes: {
        minimum: 64, size: 64, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 8], [16, 4], [20, 4], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4], [48, 4], [52, 4], [56, 4]],
        offsets: { size: 0, written: 4, revision: 8, volumes_data: 16, volumes_capacity: 20, volumes_count: 24, volumes_stride: 28, tags_data: 32, tags_capacity: 36, tags_count: 40, tags_stride: 44, bytes_data: 48, bytes_capacity: 52, bytes_count: 56 },
    },
    valueInput: {
        minimum: 80, size: 80, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 8], [24, 16], [40, 16], [56, 15], [71, 1], [72, 4]],
        offsets: { size: 0, written: 4, kind: 8, length: 12, scalar: 16, id1: 24, id2: 40, entry_id: 56, reserved: 71, data: 72 },
    },
    fieldInput: {
        minimum: 40, size: 40, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4]],
        offsets: { size: 0, written: 4, id: 8, values: 24, count: 28, stride: 32, present: 36 },
    },
    value: {
        minimum: 80, size: 80, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 8], [24, 16], [40, 16], [56, 15], [71, 1], [72, 4], [76, 4]],
        offsets: { size: 0, written: 4, kind: 8, reserved: 12, scalar: 16, id1: 24, id2: 40, entry_id: 56, entry_reserved: 71, bytes_offset: 72, bytes_count: 76 },
    },
    field: {
        minimum: 40, size: 40, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4]],
        offsets: { size: 0, written: 4, id: 8, values_offset: 24, values_count: 28, present: 32, reserved: 36 },
    },
    object: {
        minimum: 68, size: 68, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4], [48, 4], [52, 4], [56, 4], [60, 4], [64, 4]],
        offsets: { size: 0, written: 4, type_id: 8, fields_data: 24, fields_capacity: 28, fields_count: 32, fields_stride: 36, values_data: 40, values_capacity: 44, values_count: 48, values_stride: 52, bytes_data: 56, bytes_capacity: 60, bytes_count: 64 },
    },
    query: {
        minimum: 60, size: 60, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 16], [40, 4], [44, 4], [48, 4], [52, 4], [56, 4]],
        offsets: { size: 0, written: 4, type_id: 8, after: 24, flags: 40, limit: 44, fields: 48, count: 52, reserved: 56 },
    },
    objectInfo: {
        minimum: 48, size: 48, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 16], [40, 4], [44, 4]],
        offsets: { size: 0, written: 4, id: 8, type_id: 24, fields_offset: 40, fields_count: 44 },
    },
    page: {
        minimum: 92, size: 92, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4], [48, 4], [52, 4], [56, 4], [60, 4], [64, 4], [68, 4], [72, 4], [76, 4], [80, 4], [84, 4], [88, 4]],
        offsets: { size: 0, written: 4, next: 8, has_next: 24, projected: 28, objects_data: 32, objects_capacity: 36, objects_count: 40, objects_stride: 44, fields_data: 48, fields_capacity: 52, fields_count: 56, fields_stride: 60, values_data: 64, values_capacity: 68, values_count: 72, values_stride: 76, bytes_data: 80, bytes_capacity: 84, bytes_count: 88 },
    },
    schemaFieldInput: {
        minimum: 56, size: 56, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 16]],
        offsets: { size: 0, written: 4, id: 8, name_data: 24, name_length: 28, kind: 32, flags: 36, reference_type: 40 },
    },
    schemaTypeInput: {
        minimum: 48, size: 48, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4]],
        offsets: { size: 0, written: 4, id: 8, name_data: 24, name_length: 28, fields: 32, count: 36, stride: 40, flags: 44 },
    },
    schemaField: {
        minimum: 56, size: 56, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 16]],
        offsets: { size: 0, written: 4, id: 8, name_offset: 24, name_count: 28, kind: 32, flags: 36, reference_type: 40 },
    },
    schemaType: {
        minimum: 48, size: 48, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4]],
        offsets: { size: 0, written: 4, id: 8, name_offset: 24, name_count: 28, fields_offset: 32, fields_count: 36, flags: 40, reserved: 44 },
    },
    schemaSchema: {
        minimum: 52, size: 52, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4], [24, 4], [28, 4], [32, 4], [36, 4], [40, 4], [44, 4], [48, 4]],
        offsets: { size: 0, written: 4, types_data: 8, types_capacity: 12, types_count: 16, types_stride: 20, fields_data: 24, fields_capacity: 28, fields_count: 32, fields_stride: 36, bytes_data: 40, bytes_capacity: 44, bytes_count: 48 },
    },
    capabilities: {
        minimum: 36, size: 36, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4], [24, 4], [28, 4], [32, 4]],
        offsets: { size: 0, written: 4, names_data: 8, names_capacity: 12, names_count: 16, names_stride: 20, bytes_data: 24, bytes_capacity: 28, bytes_count: 32 },
    },
    argument: {
        minimum: 24, size: 24, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4]],
        offsets: { size: 0, written: 4, name_offset: 8, name_count: 12, value_offset: 16, value_count: 20 },
    },
    arguments: {
        minimum: 36, size: 36, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4], [24, 4], [28, 4], [32, 4]],
        offsets: { size: 0, written: 4, arguments_data: 8, arguments_capacity: 12, arguments_count: 16, arguments_stride: 20, bytes_data: 24, bytes_capacity: 28, bytes_count: 32 },
    },
    participant: {
        minimum: 88, size: 344, alignment: 4, optionalOutputField: 88,
        fields: [[0, 4], [4, 4], [8, 16], [24, 16], [40, 16], [56, 32], [88, 256]],
        offsets: { size: 0, written: 4, account_id: 8, device_id: 24, instance_id: 40, build_id: 56, display_name: 88 },
    },
    fileReference: {
        minimum: 32, size: 32, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 8], [16, 15], [31, 1]],
        offsets: { size: 0, written: 4, volume: 8, entry_id: 16, reserved: 31 },
    },
    fileStatus: {
        minimum: 32, size: 32, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 8], [24, 8]],
        offsets: { size: 0, written: 4, kind: 8, flags: 12, file_size: 16, modification_time: 24 },
    },
    fileCloseResult: {
        minimum: 24, size: 24, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 8]],
        offsets: { size: 0, written: 4, error: 8, publication: 12, recovery: 16 },
    },
    fileListEntry: {
        minimum: 28, size: 28, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4], [24, 4]],
        offsets: { size: 0, written: 4, outcome: 8, kind: 12, name_data: 16, name_capacity: 20, name_count: 24 },
    },
    selection: {
        minimum: 40, size: 40, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 8], [24, 15], [39, 1]],
        offsets: { size: 0, written: 4, present: 8, reserved: 12, volume: 16, entry_id: 24, entry_reserved: 39 },
    },
    event: {
        minimum: 24, size: 32, alignment: 8, optionalOutputField: 24,
        fields: [[0, 4], [4, 4], [8, 8], [16, 4], [20, 4], [24, 4]],
        offsets: { size: 0, written: 4, generation: 8, kind: 16, target: 20, object_id: 24 },
    },
    identity: {
        minimum: 104, size: 360, alignment: 8, optionalOutputField: 104,
        fields: [[0, 4], [4, 4], [8, 8], [16, 16], [32, 16], [48, 16], [64, 32], [96, 4], [100, 4], [104, 256]],
        offsets: { size: 0, written: 4, generation: 8, account_id: 16, device_id: 32, instance_id: 48, session_id: 64, flags: 96, reserved: 100, display_name: 104 },
    },
    app: {
        minimum: 24, size: 24, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4]],
        offsets: { size: 0, written: 4, init: 8, turn: 12, destroy: 16, user_data: 20 },
    },
    authenticatedMetadata: {
        minimum: 168, size: 168, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 16], [24, 16], [40, 16], [56, 16], [72, 32], [104, 16], [120, 16], [136, 4], [140, 4], [144, 4], [148, 4], [152, 15], [167, 1]],
        offsets: { size: 0, written: 4, volume_id: 8, object_id: 24, type_id: 40, field_id: 56, revision: 72, principal_id: 104, device_id: 120, state: 136, provenance: 140, principal_kind: 144, reserved: 148, entry_id: 152, entry_reserved: 167 },
    },
    authenticatedField: {
        minimum: 40, size: 40, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 4], [20, 4], [24, 4], [28, 4], [32, 4], [36, 4]],
        offsets: { size: 0, written: 4, metadata: 8, values_data: 12, values_capacity: 16, values_count: 20, values_stride: 24, bytes_data: 28, bytes_capacity: 32, bytes_count: 36 },
    },
    kvQuery: {
        minimum: 56, size: 56, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 32], [44, 4], [48, 4], [52, 4]],
        offsets: { size: 0, written: 4, root_level: 8, root_digest: 12, has_root: 44, limit: 48, reserved: 52 },
    },
    kvRead: {
        minimum: 64, size: 64, alignment: 8,
        fields: [[0, 4], [4, 4], [8, 8], [16, 4], [20, 4], [24, 4], [28, 32], [60, 4]],
        offsets: { size: 0, written: 4, offset: 8, limit: 16, has_root: 20, root_level: 24, root_digest: 28, reserved: 60 },
    },
    bloblogBlob: {
        minimum: 16, size: 16, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4]],
        offsets: { size: 0, written: 4, data: 8, length: 12 },
    },
    sessionInfo: {
        minimum: 64, size: 64, alignment: 4,
        fields: [[0, 4], [4, 4], [8, 4], [12, 4], [16, 32], [48, 16]],
        offsets: { size: 0, written: 4, flags: 8, reserved: 12, internal_id: 16, key_secret: 48 },
    },
} as const satisfies Record<string, RecordLayout & { offsets: Readonly<Record<string, number>> }>;
