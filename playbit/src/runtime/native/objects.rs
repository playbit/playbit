//! Native creation and paged projection using the shared checked data operations.
use super::data::{append_bytes, output_value};
use crate::runtime::syscall::{self as rt, HostResult};
use playbit_api::abi::{data as abi, syscall as contract};
const MAX_PAGE_FIELDS: usize = contract::MAX_PAGE_OBJECTS as usize * contract::MAX_FIELDS as usize;
const MAX_PAGE_VALUES: usize = contract::MAX_PAGE_VALUES as usize;
const _: () = assert!(
    MAX_PAGE_FIELDS * std::mem::size_of::<abi::Field>()
        + MAX_PAGE_VALUES * std::mem::size_of::<abi::Value>()
        + contract::MAX_DATA_BYTES
        + contract::MAX_PAGE_OBJECTS as usize * std::mem::size_of::<abi::ObjectInfo>()
        <= contract::MAX_QUERY_SCRATCH_BYTES
);

pub(super) fn counts(page: &rt::ObjectPage, result: &mut abi::Page) {
    result.objects_count = page.objects.len() as u32;
    result.fields_count = 0;
    result.values_count = 0;
    result.bytes_count = 0;
    for object in page.records.iter().flatten() {
        result.fields_count += object.fields.len() as u32;
        for field in &object.fields {
            if let Some(values) = &field.values {
                result.values_count += values.len() as u32;
                for value in values {
                    if let Some(bytes) = output_value(value).1 {
                        result.bytes_count += bytes.len() as u32;
                    }
                }
            }
        }
    }
    assert!(result.fields_count as usize <= MAX_PAGE_FIELDS);
    assert!(result.values_count as usize <= MAX_PAGE_VALUES);
    assert!(result.bytes_count as usize <= contract::MAX_DATA_BYTES);
}
pub(super) unsafe fn write_page(
    page: &rt::ObjectPage,
    type_id: rt::Id,
    result: &abi::Page,
) -> HostResult<()> {
    let mut fields = 0;
    let mut values = 0;
    let mut bytes = 0;
    // SAFETY: all aggregate capacities were admitted, and shared query validation bounds records.
    unsafe {
        for (index, id) in page.objects.iter().enumerate() {
            let record = page.records.as_ref().map(|records| &records[index]);
            let object_fields = record.map_or(&[][..], |record| record.fields.as_slice());
            super::record::element(
                result.objects_data,
                result.objects_stride,
                index,
                abi::ObjectInfo {
                    id: *id,
                    type_id,
                    fields_offset: fields,
                    fields_count: object_fields.len() as u32,
                    ..abi::ObjectInfo::default()
                },
            )?;
            for field in object_fields {
                let field_values = field.values.as_deref().unwrap_or_default();
                super::record::element(
                    result.fields_data,
                    result.fields_stride,
                    fields as usize,
                    abi::Field {
                        id: field.field_id,
                        values_offset: values,
                        values_count: field_values.len() as u32,
                        present: u32::from(field.values.is_some()),
                        reserved: 0,
                        ..abi::Field::default()
                    },
                )?;
                fields += 1;
                for value in field_values {
                    let (mut record, payload) = output_value(value);
                    if let Some(payload) = payload {
                        record.set_bytes(append_bytes(result.bytes_data, &mut bytes, payload));
                    }
                    super::record::element(
                        result.values_data,
                        result.values_stride,
                        values as usize,
                        record,
                    )?;
                    values += 1;
                }
            }
        }
    }
    assert_eq!(
        (fields, values, bytes),
        (result.fields_count, result.values_count, result.bytes_count)
    );
    Ok(())
}
