use core::mem::{align_of, size_of};
use playbit_api::abi::record::Record;
use playbit_api::abi::{audio, authenticated, data, environment, files, kv, operations, pending, schema};
#[repr(C, align(8))]
struct Storage([u8; 4096]);
static mut STORAGE: Storage = Storage([0; 4096]);
#[unsafe(no_mangle)]
pub extern "C" fn probe_base() -> u32 {
    core::ptr::addr_of_mut!(STORAGE) as u32
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_layout(index: u32) -> u32 {
    const LAYOUT: [u32; 699] = [
    size_of::<playbit_api::abi::data::EntryHandle>() as u32,
    align_of::<playbit_api::abi::data::EntryHandle>() as u32,
    5,
    core::mem::offset_of!(playbit_api::abi::data::EntryHandle, size) as u32,
    <playbit_api::abi::data::EntryHandle as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::EntryHandle, written) as u32,
    <playbit_api::abi::data::EntryHandle as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::EntryHandle, volume) as u32,
    <playbit_api::abi::data::EntryHandle as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::EntryHandle, entry_id) as u32,
    <playbit_api::abi::data::EntryHandle as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::EntryHandle, reserved) as u32,
    <playbit_api::abi::data::EntryHandle as Record>::FIELDS[4].width,
    size_of::<playbit_api::abi::data::Range>() as u32,
    align_of::<playbit_api::abi::data::Range>() as u32,
    4,
    core::mem::offset_of!(playbit_api::abi::data::Range, size) as u32,
    <playbit_api::abi::data::Range as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Range, written) as u32,
    <playbit_api::abi::data::Range as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Range, offset) as u32,
    <playbit_api::abi::data::Range as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Range, count) as u32,
    <playbit_api::abi::data::Range as Record>::FIELDS[3].width,
    size_of::<playbit_api::abi::data::Volume>() as u32,
    align_of::<playbit_api::abi::data::Volume>() as u32,
    13,
    core::mem::offset_of!(playbit_api::abi::data::Volume, size) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, written) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, handle) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, id) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, name_offset) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, name_count) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, tags_offset) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, tags_count) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, rights) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, flags) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, file) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, grant_id) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::data::Volume, role) as u32,
    <playbit_api::abi::data::Volume as Record>::FIELDS[12].width,
    size_of::<playbit_api::abi::data::Volumes>() as u32,
    align_of::<playbit_api::abi::data::Volumes>() as u32,
    14,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, size) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, written) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, revision) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, volumes_data) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, volumes_capacity) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, volumes_count) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, volumes_stride) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, tags_data) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, tags_capacity) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, tags_count) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, tags_stride) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, bytes_data) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, bytes_capacity) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[12].width,
    core::mem::offset_of!(playbit_api::abi::data::Volumes, bytes_count) as u32,
    <playbit_api::abi::data::Volumes as Record>::FIELDS[13].width,
    size_of::<playbit_api::abi::data::ValueInput>() as u32,
    align_of::<playbit_api::abi::data::ValueInput>() as u32,
    10,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, size) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, written) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, kind) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, length) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, scalar) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, id1) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, id2) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, entry_id) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, reserved) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::ValueInput, data) as u32,
    <playbit_api::abi::data::ValueInput as Record>::FIELDS[9].width,
    size_of::<playbit_api::abi::data::FieldInput>() as u32,
    align_of::<playbit_api::abi::data::FieldInput>() as u32,
    7,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, size) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, written) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, id) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, values) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, count) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, stride) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::FieldInput, present) as u32,
    <playbit_api::abi::data::FieldInput as Record>::FIELDS[6].width,
    size_of::<playbit_api::abi::data::Value>() as u32,
    align_of::<playbit_api::abi::data::Value>() as u32,
    11,
    core::mem::offset_of!(playbit_api::abi::data::Value, size) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, written) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, kind) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, reserved) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, scalar) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, id1) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, id2) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, entry_id) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, entry_reserved) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, bytes_offset) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::data::Value, bytes_count) as u32,
    <playbit_api::abi::data::Value as Record>::FIELDS[10].width,
    size_of::<playbit_api::abi::data::Field>() as u32,
    align_of::<playbit_api::abi::data::Field>() as u32,
    7,
    core::mem::offset_of!(playbit_api::abi::data::Field, size) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, written) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, id) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, values_offset) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, values_count) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, present) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Field, reserved) as u32,
    <playbit_api::abi::data::Field as Record>::FIELDS[6].width,
    size_of::<playbit_api::abi::data::Object>() as u32,
    align_of::<playbit_api::abi::data::Object>() as u32,
    14,
    core::mem::offset_of!(playbit_api::abi::data::Object, size) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, written) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, type_id) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, fields_data) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, fields_capacity) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, fields_count) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, fields_stride) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, values_data) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, values_capacity) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, values_count) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, values_stride) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, bytes_data) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, bytes_capacity) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[12].width,
    core::mem::offset_of!(playbit_api::abi::data::Object, bytes_count) as u32,
    <playbit_api::abi::data::Object as Record>::FIELDS[13].width,
    size_of::<playbit_api::abi::data::Query>() as u32,
    align_of::<playbit_api::abi::data::Query>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::data::Query, size) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, written) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, type_id) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, after) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, flags) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, limit) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, fields) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, count) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Query, reserved) as u32,
    <playbit_api::abi::data::Query as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::data::ObjectInfo>() as u32,
    align_of::<playbit_api::abi::data::ObjectInfo>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, size) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, written) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, id) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, type_id) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, fields_offset) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::ObjectInfo, fields_count) as u32,
    <playbit_api::abi::data::ObjectInfo as Record>::FIELDS[5].width,
    size_of::<playbit_api::abi::data::Page>() as u32,
    align_of::<playbit_api::abi::data::Page>() as u32,
    20,
    core::mem::offset_of!(playbit_api::abi::data::Page, size) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, written) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, next) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, has_next) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, projected) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, objects_data) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, objects_capacity) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, objects_count) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, objects_stride) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, fields_data) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, fields_capacity) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, fields_count) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, fields_stride) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[12].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, values_data) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[13].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, values_capacity) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[14].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, values_count) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[15].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, values_stride) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[16].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, bytes_data) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[17].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, bytes_capacity) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[18].width,
    core::mem::offset_of!(playbit_api::abi::data::Page, bytes_count) as u32,
    <playbit_api::abi::data::Page as Record>::FIELDS[19].width,
    size_of::<playbit_api::abi::schema::FieldInput>() as u32,
    align_of::<playbit_api::abi::schema::FieldInput>() as u32,
    8,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, size) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, written) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, id) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, name_data) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, name_length) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, kind) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, flags) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::schema::FieldInput, reference_type) as u32,
    <playbit_api::abi::schema::FieldInput as Record>::FIELDS[7].width,
    size_of::<playbit_api::abi::schema::TypeInput>() as u32,
    align_of::<playbit_api::abi::schema::TypeInput>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, size) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, written) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, id) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, name_data) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, name_length) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, fields) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, count) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, stride) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::schema::TypeInput, flags) as u32,
    <playbit_api::abi::schema::TypeInput as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::schema::Field>() as u32,
    align_of::<playbit_api::abi::schema::Field>() as u32,
    8,
    core::mem::offset_of!(playbit_api::abi::schema::Field, size) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, written) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, id) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, name_offset) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, name_count) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, kind) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, flags) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::schema::Field, reference_type) as u32,
    <playbit_api::abi::schema::Field as Record>::FIELDS[7].width,
    size_of::<playbit_api::abi::schema::Type>() as u32,
    align_of::<playbit_api::abi::schema::Type>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::schema::Type, size) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, written) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, id) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, name_offset) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, name_count) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, fields_offset) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, fields_count) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, flags) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::schema::Type, reserved) as u32,
    <playbit_api::abi::schema::Type as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::schema::Schema>() as u32,
    align_of::<playbit_api::abi::schema::Schema>() as u32,
    13,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, size) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, written) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, types_data) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, types_capacity) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, types_count) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, types_stride) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, fields_data) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, fields_capacity) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, fields_count) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, fields_stride) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, bytes_data) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, bytes_capacity) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::schema::Schema, bytes_count) as u32,
    <playbit_api::abi::schema::Schema as Record>::FIELDS[12].width,
    size_of::<playbit_api::abi::environment::Capabilities>() as u32,
    align_of::<playbit_api::abi::environment::Capabilities>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, size) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, written) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, names_data) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, names_capacity) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, names_count) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, names_stride) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, bytes_data) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, bytes_capacity) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::environment::Capabilities, bytes_count) as u32,
    <playbit_api::abi::environment::Capabilities as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::environment::Argument>() as u32,
    align_of::<playbit_api::abi::environment::Argument>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, size) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, written) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, name_offset) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, name_count) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, value_offset) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::environment::Argument, value_count) as u32,
    <playbit_api::abi::environment::Argument as Record>::FIELDS[5].width,
    size_of::<playbit_api::abi::environment::Arguments>() as u32,
    align_of::<playbit_api::abi::environment::Arguments>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, size) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, written) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, arguments_data) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, arguments_capacity) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, arguments_count) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, arguments_stride) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, bytes_data) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, bytes_capacity) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::environment::Arguments, bytes_count) as u32,
    <playbit_api::abi::environment::Arguments as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::environment::Participant>() as u32,
    align_of::<playbit_api::abi::environment::Participant>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, size) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, written) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, account_id) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, device_id) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, instance_id) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::environment::Participant, build_id) as u32,
    <playbit_api::abi::environment::Participant as Record>::FIELDS[5].width,
    size_of::<playbit_api::abi::files::FileReference>() as u32,
    align_of::<playbit_api::abi::files::FileReference>() as u32,
    5,
    core::mem::offset_of!(playbit_api::abi::files::FileReference, size) as u32,
    <playbit_api::abi::files::FileReference as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::files::FileReference, written) as u32,
    <playbit_api::abi::files::FileReference as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::files::FileReference, volume) as u32,
    <playbit_api::abi::files::FileReference as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::files::FileReference, entry_id) as u32,
    <playbit_api::abi::files::FileReference as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::files::FileReference, reserved) as u32,
    <playbit_api::abi::files::FileReference as Record>::FIELDS[4].width,
    size_of::<playbit_api::abi::files::FileStatus>() as u32,
    align_of::<playbit_api::abi::files::FileStatus>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, size) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, written) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, kind) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, flags) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, file_size) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::files::FileStatus, modification_time) as u32,
    <playbit_api::abi::files::FileStatus as Record>::FIELDS[5].width,
    size_of::<playbit_api::abi::files::FileCloseResult>() as u32,
    align_of::<playbit_api::abi::files::FileCloseResult>() as u32,
    5,
    core::mem::offset_of!(playbit_api::abi::files::FileCloseResult, size) as u32,
    <playbit_api::abi::files::FileCloseResult as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::files::FileCloseResult, written) as u32,
    <playbit_api::abi::files::FileCloseResult as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::files::FileCloseResult, error) as u32,
    <playbit_api::abi::files::FileCloseResult as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::files::FileCloseResult, publication) as u32,
    <playbit_api::abi::files::FileCloseResult as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::files::FileCloseResult, recovery) as u32,
    <playbit_api::abi::files::FileCloseResult as Record>::FIELDS[4].width,
    size_of::<playbit_api::abi::files::FileListEntry>() as u32,
    align_of::<playbit_api::abi::files::FileListEntry>() as u32,
    7,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, size) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, written) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, outcome) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, kind) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, name_data) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, name_capacity) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::files::FileListEntry, name_count) as u32,
    <playbit_api::abi::files::FileListEntry as Record>::FIELDS[6].width,
    size_of::<playbit_api::abi::pending::Selection>() as u32,
    align_of::<playbit_api::abi::pending::Selection>() as u32,
    7,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, size) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, written) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, present) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, reserved) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, volume) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, entry_id) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::pending::Selection, entry_reserved) as u32,
    <playbit_api::abi::pending::Selection as Record>::FIELDS[6].width,
    size_of::<playbit_api::abi::operations::Event>() as u32,
    align_of::<playbit_api::abi::operations::Event>() as u32,
    5,
    core::mem::offset_of!(playbit_api::abi::operations::Event, size) as u32,
    <playbit_api::abi::operations::Event as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::operations::Event, written) as u32,
    <playbit_api::abi::operations::Event as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::operations::Event, generation) as u32,
    <playbit_api::abi::operations::Event as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::operations::Event, kind) as u32,
    <playbit_api::abi::operations::Event as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::operations::Event, target) as u32,
    <playbit_api::abi::operations::Event as Record>::FIELDS[4].width,
    size_of::<playbit_api::abi::operations::Identity>() as u32,
    align_of::<playbit_api::abi::operations::Identity>() as u32,
    9,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, size) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, written) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, generation) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, account_id) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, device_id) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, instance_id) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, session_id) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, flags) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::operations::Identity, reserved) as u32,
    <playbit_api::abi::operations::Identity as Record>::FIELDS[8].width,
    size_of::<playbit_api::abi::operations::App>() as u32,
    align_of::<playbit_api::abi::operations::App>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::operations::App, size) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::operations::App, written) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::operations::App, init) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::operations::App, turn) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::operations::App, destroy) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::operations::App, user_data) as u32,
    <playbit_api::abi::operations::App as Record>::FIELDS[5].width,
    size_of::<playbit_api::abi::authenticated::Metadata>() as u32,
    align_of::<playbit_api::abi::authenticated::Metadata>() as u32,
    15,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, size) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, written) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, volume_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, object_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, type_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, field_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, revision) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, principal_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, device_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, state) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[9].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, provenance) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[10].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, principal_kind) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[11].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, reserved) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[12].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, entry_id) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[13].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Metadata, entry_reserved) as u32,
    <playbit_api::abi::authenticated::Metadata as Record>::FIELDS[14].width,
    size_of::<playbit_api::abi::authenticated::Field>() as u32,
    align_of::<playbit_api::abi::authenticated::Field>() as u32,
    10,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, size) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, written) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, metadata) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, values_data) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, values_capacity) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, values_count) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, values_stride) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, bytes_data) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[7].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, bytes_capacity) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[8].width,
    core::mem::offset_of!(playbit_api::abi::authenticated::Field, bytes_count) as u32,
    <playbit_api::abi::authenticated::Field as Record>::FIELDS[9].width,
    size_of::<playbit_api::abi::kv::Query>() as u32,
    align_of::<playbit_api::abi::kv::Query>() as u32,
    7,
    core::mem::offset_of!(playbit_api::abi::kv::Query, size) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, written) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, root_level) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, root_digest) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, has_root) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, limit) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::kv::Query, reserved) as u32,
    <playbit_api::abi::kv::Query as Record>::FIELDS[6].width,
    size_of::<playbit_api::abi::kv::Read>() as u32,
    align_of::<playbit_api::abi::kv::Read>() as u32,
    8,
    core::mem::offset_of!(playbit_api::abi::kv::Read, size) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, written) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, offset) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, limit) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, has_root) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, root_level) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[5].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, root_digest) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[6].width,
    core::mem::offset_of!(playbit_api::abi::kv::Read, reserved) as u32,
    <playbit_api::abi::kv::Read as Record>::FIELDS[7].width,
    size_of::<playbit_api::abi::bloblog::Blob>() as u32,
    align_of::<playbit_api::abi::bloblog::Blob>() as u32,
    4,
    core::mem::offset_of!(playbit_api::abi::bloblog::Blob, size) as u32,
    <playbit_api::abi::bloblog::Blob as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::bloblog::Blob, written) as u32,
    <playbit_api::abi::bloblog::Blob as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::bloblog::Blob, data) as u32,
    <playbit_api::abi::bloblog::Blob as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::bloblog::Blob, length) as u32,
    <playbit_api::abi::bloblog::Blob as Record>::FIELDS[3].width,
    size_of::<playbit_api::abi::session::SessionInfo>() as u32,
    align_of::<playbit_api::abi::session::SessionInfo>() as u32,
    6,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, size) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[0].width,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, written) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[1].width,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, flags) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[2].width,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, reserved) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[3].width,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, internal_id) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[4].width,
    core::mem::offset_of!(playbit_api::abi::session::SessionInfo, key_secret) as u32,
    <playbit_api::abi::session::SessionInfo as Record>::FIELDS[5].width,
];
    LAYOUT.get(index as usize).copied().unwrap_or(u32::MAX)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn probe_imports() -> u32 {
    let ctx = 1usize as *mut operations::Context;
    let base = probe_base() as usize;
    let volume = data::VolumeHandle::new(0x7edcba98, 0x89abcdef);
    let handle = pending::OperationHandle(volume.0);
    let file = files::FileHandle(volume.0);
    let list = files::FileListHandle(volume.0);
    let entry = (base + 64) as *mut data::EntryHandle;
    unsafe { entry.write(data::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() }); }
    if unsafe { operations::pb_log(ctx, (base + 128) as _, 0x80000000u32 + 2) } != -7 {
        return 1;
    }
    if unsafe { operations::pb_view_submit(ctx, (base + 128) as _, 0x80000000u32 + 2) } != -7 {
        return 2;
    }
    if unsafe {
        operations::pb_event_poll(ctx, (base + 128) as _, (base + 256) as _, 0x80000000u32 + 3)
    } != -7
    {
        return 3;
    }
    if unsafe {
        operations::pb_event_poll_deadline(
            ctx, (base + 128) as _, (base + 256) as _, 0x80000003, 0x8877665544332204,
        )
    } != -7
    {
        return 114;
    }
    if unsafe { operations::pb_identity(ctx, (base + 128) as _) } != -7 {
        return 4;
    }
    if unsafe { playbit_api::abi::session::pb_session_info(ctx, (base + 128) as _) } != -7 {
        return 0x163;
    }
    if unsafe { operations::pb_wake_after(ctx, 0x80000000u32 + 1) } != -7 {
        return 5;
    }
    if unsafe { operations::pb_update_defer(ctx, 0x8877665544332200u64 + 1) } != -7 {
        return 6;
    }
    if unsafe { data::pb_volumes(ctx, (base + 128) as _) } != -7 {
        return 9;
    }
    if unsafe { data::pb_volumes_observe(ctx, 0x8877665544332201) } != -7 {
        return 64;
    }
    if unsafe { environment::pb_capabilities(ctx, (base + 128) as _) } != -7 {
        return 14;
    }
    if unsafe { environment::pb_startup_arguments(ctx, (base + 128) as _) } != -7 {
        return 15;
    }
    if unsafe { environment::pb_presence(ctx, (base + 128) as _, 0x80000002, 0x80000003, (base + 640) as _) } != -7 {
        return 16;
    }
    if unsafe { files::pb_file_read_begin(ctx, file, 0x8877665544332200u64 + 2, 0x80000000u32 + 3) } != 0x7edc_ba98_89ab_cdefi64 { return 18; }
    if unsafe { files::pb_file_read_take(ctx, handle, (base + 512) as _, 0x80000003, (base + 640) as _) } != -7 { return 40; }
    if unsafe { files::pb_file_write_begin(ctx, file, 0x8877665544332200u64 + 2, (base + 256) as _, 0x80000000u32 + 3) } != 0x7edc_ba98_89ab_cdefi64 { return 19; }
    if unsafe { files::pb_file_write_take(ctx, handle) } != -7 { return 41; }
    if unsafe { files::pb_file_open_begin(ctx, file, (base + 128) as _, 0x80000000u32 + 2, 0x80000000u32 + 3) } != 0x7edc_ba98_89ab_cdefi64 { return 80; }
    if unsafe { files::pb_file_open_take(ctx, handle) } != 0x7edc_ba98_89ab_cdefi64 { return 81; }
    if unsafe { files::pb_file_status_begin(ctx, file) } != 0x7edc_ba98_89ab_cdefi64 { return 82; }
    if unsafe { files::pb_file_status_take(ctx, handle, (base + 512) as _) } != -7 { return 83; }
    if unsafe { files::pb_file_close_begin(ctx, file, 0x80000000u32 + 2) } != 0x7edc_ba98_89ab_cdefi64 { return 84; }
    if unsafe { files::pb_file_close_take(ctx, handle, (base + 512) as _) } != -7 { return 85; }
    if unsafe { files::pb_file_list_open_begin(ctx, file, 0x80000000u32 + 2) } != 0x7edc_ba98_89ab_cdefi64 { return 86; }
    if unsafe { files::pb_file_list_open_take(ctx, handle) } != 0x7edc_ba98_89ab_cdefi64 { return 87; }
    if unsafe { files::pb_file_list_next_begin(ctx, list) } != 0x7edc_ba98_89ab_cdefi64 { return 88; }
    if unsafe { files::pb_file_list_next_take(ctx, handle, (base + 512) as _) } != -7 { return 89; }
    if unsafe { files::pb_file_list_close(ctx, list) } != -7 { return 90; }
    if unsafe { files::pb_operation_dispose(ctx, handle) } != -7 { return 91; }
    if unsafe { files::pb_file_dispose(ctx, file) } != -7 { return 92; }
    if unsafe {
        pending::pb_object_create_begin(
            ctx,
            entry,
            (base + 256) as _,
            (base + 384) as _,
            0x80000000u32 + 4,
            0x80000000u32 + 5)
    } != 0x7edc_ba98_89ab_cdefi64
    {
        return 20;
    }
    if unsafe { pending::pb_object_create_poll(ctx, handle, (base + 256) as _) } != -7 {
        return 21;
    }
    if unsafe {
        pending::pb_file_copy_begin(ctx, (base + 128) as _, (base + 256) as _)
    } != 0x7edc_ba98_89ab_cdefi64
    {
        return 22;
    }
    if unsafe { pending::pb_file_copy_poll(ctx, handle, (base + 256) as _) } != -7 {
        return 23;
    }
    if unsafe {
        pending::pb_file_select_begin(ctx)
    } != 0x7edc_ba98_89ab_cdefi64
    {
        return 24;
    }
    if unsafe { pending::pb_file_select_poll(ctx, handle, (base + 256) as _) } != -7 {
        return 25;
    }
    if unsafe { pending::pb_object_read_begin(ctx, entry, (base + 256) as _, (base + 384) as _, 0x80000004) } != 0x7edc_ba98_89ab_cdefi64 { return 30; }
    if unsafe { pending::pb_object_read_take(ctx, handle, (base + 256) as _) } != -7 { return 31; }
    if unsafe { pending::pb_object_edit_begin(ctx, entry, (base + 256) as _, (base + 384) as _, 0x80000004, 0x80000005) } != 0x7edc_ba98_89ab_cdefi64 { return 32; }
    if unsafe { pending::pb_object_edit_take(ctx, handle) } != -7 { return 33; }
    if unsafe { pending::pb_object_query_begin(ctx, entry, (base + 256) as _) } != 0x7edc_ba98_89ab_cdefi64 { return 34; }
    if unsafe { pending::pb_object_query_take(ctx, handle, (base + 256) as _) } != -7 { return 35; }
    if unsafe { schema::pb_schema_read_begin(ctx, entry) } != 0x7edc_ba98_89ab_cdefi64 { return 36; }
    if unsafe { schema::pb_schema_read_take(ctx, handle, (base + 256) as _) } != -7 { return 37; }
    if unsafe { schema::pb_schema_establish_begin(ctx, entry, (base + 256) as _, 0x80000003, 0x80000004) } != 0x7edc_ba98_89ab_cdefi64 { return 38; }
    if unsafe { schema::pb_schema_establish_take(ctx, handle) } != -7 { return 39; }
    if unsafe {
        authenticated::pb_object_field_read_authenticated_begin(
            ctx,
            entry,
            (base + 256) as _,
            (base + 384) as _)
    } != 0x7edc_ba98_89ab_cdefi64
    {
        return 26;
    }
    if unsafe { authenticated::pb_object_field_read_authenticated_take(ctx, handle, (base + 512) as _) } != -7 { return 40; }
    if unsafe { environment::pb_host_platform(ctx) } != -7 {
        return 27;
    }
    if unsafe { environment::pb_host_limit(ctx, 0x80000001) } != -7 {
        return 28;
    }
    if unsafe { data::pb_entry_resolve_begin(ctx, volume, (base + 128) as _, 0x80000000u32 + 2) } != 0x7edc_ba98_89ab_cdefi64 { return 29; }
    if unsafe { data::pb_entry_resolve_take(ctx, handle, (base + 256) as _) } != -7 { return 42; }
    if unsafe { kv::pb_kv_keys_begin(ctx, entry, (base + 256) as _, (base + 384) as _, 0x80000004) } != 0x7edc_ba98_89ab_cdefi64 { return 43; }
    if unsafe { kv::pb_kv_keys_take(ctx, handle, (base + 256) as _, 0x80000003, (base + 640) as _) } != -7 { return 44; }
    if unsafe { kv::pb_kv_read_begin(ctx, entry, (base + 256) as _, 0x80000003, (base + 384) as _) } != 0x7edc_ba98_89ab_cdefi64 { return 45; }
    if unsafe { kv::pb_kv_read_take(ctx, handle, (base + 256) as _, 0x80000003, (base + 640) as _) } != -7 { return 46; }
    if unsafe { kv::pb_kv_put_begin(ctx, entry, (base + 128) as _, 0x80000003, (base + 256) as _, 0x80000005) } != 0x7edc_ba98_89ab_cdefi64 { return 47; }
    if unsafe { kv::pb_kv_put_take(ctx, handle) } != -7 { return 48; }
    if unsafe { kv::pb_kv_delete_begin(ctx, entry, (base + 128) as _, 0x80000003) } != 0x7edc_ba98_89ab_cdefi64 { return 49; }
    if unsafe { kv::pb_kv_delete_take(ctx, handle) } != -7 { return 50; }
    if unsafe {
        operations::pb_trace_record(
            ctx,
            (base + 128) as _,
            0x80000001,
            0x8877665544332200u64 + 2,
            0x8877665544332200u64 + 3,
        )
    } != -7
    {
        return 51;
    }
    if unsafe { operations::pb_clock_read(ctx) } != 0x8877665544332201u64 {
        return 52;
    }
    if unsafe { audio::pb_audio_open_begin(ctx, 0x80000001) } != 0x7edc_ba98_89ab_cdefi64 { return 100; }
    if unsafe { audio::pb_audio_open_take(ctx, handle) } != 0x7edc_ba98_89ab_cdefi64 { return 101; }
    if unsafe { audio::pb_audio_close(ctx, volume.0) } != -7 { return 102; }
    if unsafe { audio::pb_audio_sample_rate(ctx, volume.0) } != -7 { return 103; }
    if unsafe { audio::pb_audio_volume(ctx, volume.0) } != -7 { return 104; }
    if unsafe { audio::pb_audio_set_volume(ctx, volume.0, f32::from_bits(0x80000001)) } != -7 { return 105; }
    if unsafe { audio::pb_sound_open_pcm(ctx, volume.0, 0x80000001, 0x80000002, (base + 128) as _, 0x80000003) } != 0x7edc_ba98_89ab_cdefi64 { return 106; }
    if unsafe { audio::pb_sound_close(ctx, volume.0) } != -7 { return 107; }
    if unsafe { audio::pb_sound_play(ctx, volume.0, 0x80000001, 0x80000002, 0x8877665544332201, 0x8877665544332202, 0x80000003) } != -7 { return 108; }
    if unsafe { audio::pb_sound_tell(ctx, volume.0, 0x80000001, (base + 512) as _) } != -7 { return 109; }
    if unsafe { audio::pb_sound_seek(ctx, volume.0, 0x80000001, 0x8877665544332202) } != -7 { return 110; }
    if unsafe { audio::pb_sound_set_gain(ctx, volume.0, 0x80000001, 0x80000002) } != -7 { return 111; }
    if unsafe { audio::pb_sound_set_balance(ctx, volume.0, 0x80000001, f32::from_bits(0x80000002)) } != -7 { return 112; }
    if unsafe { audio::pb_sound_stop(ctx, volume.0, 0x80000001) } != -7 { return 113; }
    0
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_invalid() -> u32 {
    let mut output = data::Volumes::new(data::Buffer::new(core::ptr::null_mut(), 0), data::Buffer::new(core::ptr::null_mut(), 0), data::Buffer::new(core::ptr::null_mut(), 0));
    let entry = data::EntryHandle { volume: data::VolumeHandle::new(1, 1).0, entry_id: [5; 15], ..Default::default() };
    unsafe {
        if environment::pb_host_platform(core::ptr::null_mut()) != -3 {
            return 3;
        }
        if data::pb_volumes(core::ptr::null_mut(), &mut output) != -3 {
            return 1;
        }
        if pending::pb_object_query_begin(
            core::ptr::null_mut(),
            &entry,
            core::ptr::null()) != -3
        {
            return 2;
        }
    }
    0
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_sdk() -> i32 {
    let mut state = playbit_api::context::ContextState::default();
    playbit_api::context::Context::guest(&mut state)
        .expect("valid events").0
        .wake_after(1234)
        .map_or_else(|e| e as i32, |()| 0)
}
