//! Paged query results use caller-owned arrays and shared snapshot validation.
use super::{output::Buffer, read::write_value};
use crate::runtime::linear_memory::layout::data::{
    field as field_layout, object_info, page as page_layout, value as value_layout,
};
use crate::runtime::{
    linear_memory::{Memory, Region},
    syscall::{HostError, HostResult, Id, ObjectPage, Value},
};
use playbit_api::abi::syscall as contract;
const MAX_FIELDS: u32 = contract::MAX_PAGE_OBJECTS * contract::MAX_FIELDS;
const MAX_VALUES: u32 = contract::MAX_PAGE_VALUES;
const _: () = assert!(
    MAX_FIELDS as usize * field_layout::SIZEOF as usize
        + MAX_VALUES as usize * value_layout::SIZEOF as usize
        + contract::MAX_DATA_BYTES
        + contract::MAX_PAGE_OBJECTS as usize * object_info::SIZEOF as usize
        <= contract::MAX_QUERY_SCRATCH_BYTES
);
pub(super) struct Output {
    descriptor: Region,
    objects: Buffer,
    fields: Buffer,
    values: Buffer,
    bytes: Buffer,
}
impl Output {
    pub(super) fn admit(memory: &mut Memory<'_>, pointer: u32) -> HostResult<Self> {
        let descriptor = memory.record(pointer, page_layout::LAYOUT, false, None)?;
        let objects = Buffer::records(
            memory,
            descriptor.sub(page_layout::OBJECTS_DATA, 16),
            object_info::LAYOUT,
            contract::MAX_PAGE_OBJECTS,
        )?;
        let fields = Buffer::records(
            memory,
            descriptor.sub(page_layout::FIELDS_DATA, 16),
            field_layout::LAYOUT,
            MAX_FIELDS,
        )?;
        let values = Buffer::records(
            memory,
            descriptor.sub(page_layout::VALUES_DATA, 16),
            value_layout::LAYOUT,
            MAX_VALUES,
        )?;
        let bytes = Buffer::admit(
            memory,
            descriptor.sub(page_layout::BYTES_DATA, 12),
            1,
            1,
            contract::MAX_DATA_BYTES as u32,
        )?;
        // Read-only query and projection may overlap each other, never an output.
        crate::runtime::linear_memory::record::outputs(&[
            descriptor,
            objects.data,
            fields.data,
            values.data,
            bytes.data,
        ])?;
        descriptor.clear(memory.data);
        objects.clear(memory.data);
        fields.clear(memory.data);
        values.clear(memory.data);
        Ok(Self { descriptor, objects, fields, values, bytes })
    }
    pub(super) fn counts(&self, memory: &mut [u8], page: &ObjectPage) -> HostResult<[usize; 3]> {
        let mut counts = [0; 3];
        for object in page.records.iter().flatten() {
            counts[0] += object.fields.len();
            for value in object.fields.iter().filter_map(|field| field.values.as_ref()).flatten() {
                counts[1] += 1;
                counts[2] += match value {
                    Value::Text(text) => text.len(),
                    Value::Bytes(bytes) => bytes.len(),
                    _ => 0,
                };
            }
        }
        assert!(counts[0] <= MAX_FIELDS as usize && counts[1] <= MAX_VALUES as usize);
        assert!(counts[2] <= contract::MAX_DATA_BYTES);
        self.objects.count(memory, page.objects.len());
        self.fields.count(memory, counts[0]);
        self.values.count(memory, counts[1]);
        self.bytes.count(memory, counts[2]);
        if !self.objects.fits(page.objects.len())
            || !self.fields.fits(counts[0])
            || !self.values.fits(counts[1])
            || !self.bytes.fits(counts[2])
        {
            return Err(HostError::BufferTooSmall);
        }
        Ok(counts)
    }
    pub(super) fn write(&self, memory: &mut [u8], page: &ObjectPage, type_id: Id) -> [usize; 3] {
        let mut used = [0; 3];
        for (index, id) in page.objects.iter().enumerate() {
            let fields =
                page.records.as_ref().map_or(&[][..], |records| records[index].fields.as_slice());
            let object = self.objects.element(memory, index);
            object.write(memory, object_info::ID, id);
            object.write(memory, object_info::TYPE_ID, &type_id);
            object.write(memory, object_info::FIELDS_OFFSET, &(used[0] as u32).to_le_bytes());
            object.write(memory, object_info::FIELDS_COUNT, &(fields.len() as u32).to_le_bytes());
            self.objects.publish(memory, index);
            for field in fields {
                let record = self.fields.element(memory, used[0]);
                let values = field.values.as_deref().unwrap_or_default();
                record.write(memory, field_layout::ID, &field.field_id);
                record.write(memory, field_layout::VALUES_OFFSET, &(used[1] as u32).to_le_bytes());
                record.write(
                    memory,
                    field_layout::VALUES_COUNT,
                    &(values.len() as u32).to_le_bytes(),
                );
                record.write(
                    memory,
                    field_layout::PRESENT,
                    &u32::from(field.values.is_some()).to_le_bytes(),
                );
                record.write(memory, field_layout::RESERVED, &0_u32.to_le_bytes());
                self.fields.publish(memory, used[0]);
                for value in values {
                    let record = self.values.element(memory, used[1]);
                    write_value(memory, record, &self.bytes, &mut used[2], value);
                    used[1] += 1;
                }
                used[0] += 1;
            }
        }
        self.descriptor.write(memory, page_layout::NEXT, &page.next.unwrap_or([0; 16]));
        self.descriptor.write(
            memory,
            page_layout::HAS_NEXT,
            &u32::from(page.next.is_some()).to_le_bytes(),
        );
        self.descriptor.write(
            memory,
            page_layout::PROJECTED,
            &u32::from(page.records.is_some()).to_le_bytes(),
        );
        self.descriptor.publish(memory, page_layout::LAYOUT);
        used
    }
}
