//! SDK conveniences build flat addressed records. Buffers and ranges passed to
//! these constructors are local values; their layouts never cross a syscall.
use super::data::*;

impl Range {
    pub const fn new(offset: u32, count: u32) -> Self {
        Self { size: core::mem::size_of::<Self>() as u32, written: 0, offset, count }
    }
}
impl Value {
    pub const fn bytes(&self) -> Range {
        Range::new(self.bytes_offset, self.bytes_count)
    }
    pub fn set_bytes(&mut self, range: Range) {
        self.bytes_offset = range.offset;
        self.bytes_count = range.count;
    }
}
impl Field {
    pub const fn values(&self) -> Range {
        Range::new(self.values_offset, self.values_count)
    }
}
impl ObjectInfo {
    pub const fn fields(&self) -> Range {
        Range::new(self.fields_offset, self.fields_count)
    }
}
impl Volume {
    pub const fn name(&self) -> Range {
        Range::new(self.name_offset, self.name_count)
    }
    pub const fn tags(&self) -> Range {
        Range::new(self.tags_offset, self.tags_count)
    }
    pub const fn handle(&self) -> VolumeHandle {
        let (generation, slot) = super::dispatch::unpack_handle(self.handle);
        VolumeHandle::new(generation, slot)
    }
    pub const fn file(&self) -> super::files::FileHandle {
        let (generation, slot) = super::dispatch::unpack_handle(self.file);
        super::files::FileHandle::new(generation, slot)
    }
}

impl Volumes {
    pub const fn new(volumes: Buffer<Volume>, tags: Buffer<Range>, bytes: Buffer<u8>) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            revision: 0,
            volumes_data: volumes.data,
            volumes_capacity: volumes.capacity,
            volumes_count: volumes.count,
            volumes_stride: volumes.stride,
            tags_data: tags.data,
            tags_capacity: tags.capacity,
            tags_count: tags.count,
            tags_stride: tags.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn volumes(&self) -> Buffer<Volume> {
        Buffer {
            data: self.volumes_data,
            capacity: self.volumes_capacity,
            count: self.volumes_count,
            stride: self.volumes_stride,
        }
    }
    pub const fn tags(&self) -> Buffer<Range> {
        Buffer {
            data: self.tags_data,
            capacity: self.tags_capacity,
            count: self.tags_count,
            stride: self.tags_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}
impl Object {
    pub const fn new(fields: Buffer<Field>, values: Buffer<Value>, bytes: Buffer<u8>) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            type_id: [0; 16],
            fields_data: fields.data,
            fields_capacity: fields.capacity,
            fields_count: fields.count,
            fields_stride: fields.stride,
            values_data: values.data,
            values_capacity: values.capacity,
            values_count: values.count,
            values_stride: values.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn fields(&self) -> Buffer<Field> {
        Buffer {
            data: self.fields_data,
            capacity: self.fields_capacity,
            count: self.fields_count,
            stride: self.fields_stride,
        }
    }
    pub const fn values(&self) -> Buffer<Value> {
        Buffer {
            data: self.values_data,
            capacity: self.values_capacity,
            count: self.values_count,
            stride: self.values_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}
impl Page {
    pub const fn new(
        objects: Buffer<ObjectInfo>,
        fields: Buffer<Field>,
        values: Buffer<Value>,
        bytes: Buffer<u8>,
    ) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            next: [0; 16],
            has_next: 0,
            projected: 0,
            objects_data: objects.data,
            objects_capacity: objects.capacity,
            objects_count: objects.count,
            objects_stride: objects.stride,
            fields_data: fields.data,
            fields_capacity: fields.capacity,
            fields_count: fields.count,
            fields_stride: fields.stride,
            values_data: values.data,
            values_capacity: values.capacity,
            values_count: values.count,
            values_stride: values.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn objects(&self) -> Buffer<ObjectInfo> {
        Buffer {
            data: self.objects_data,
            capacity: self.objects_capacity,
            count: self.objects_count,
            stride: self.objects_stride,
        }
    }
    pub const fn fields(&self) -> Buffer<Field> {
        Buffer {
            data: self.fields_data,
            capacity: self.fields_capacity,
            count: self.fields_count,
            stride: self.fields_stride,
        }
    }
    pub const fn values(&self) -> Buffer<Value> {
        Buffer {
            data: self.values_data,
            capacity: self.values_capacity,
            count: self.values_count,
            stride: self.values_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}

impl super::schema::Type {
    pub const fn name(&self) -> Range {
        Range::new(self.name_offset, self.name_count)
    }
    pub const fn fields(&self) -> Range {
        Range::new(self.fields_offset, self.fields_count)
    }
}
impl super::schema::Field {
    pub const fn name(&self) -> Range {
        Range::new(self.name_offset, self.name_count)
    }
}
impl super::schema::Schema {
    pub const fn new(
        types: Buffer<super::schema::Type>,
        fields: Buffer<super::schema::Field>,
        bytes: Buffer<u8>,
    ) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            types_data: types.data,
            types_capacity: types.capacity,
            types_count: types.count,
            types_stride: types.stride,
            fields_data: fields.data,
            fields_capacity: fields.capacity,
            fields_count: fields.count,
            fields_stride: fields.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn types(&self) -> Buffer<super::schema::Type> {
        Buffer {
            data: self.types_data,
            capacity: self.types_capacity,
            count: self.types_count,
            stride: self.types_stride,
        }
    }
    pub const fn fields(&self) -> Buffer<super::schema::Field> {
        Buffer {
            data: self.fields_data,
            capacity: self.fields_capacity,
            count: self.fields_count,
            stride: self.fields_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}

impl super::environment::Argument {
    pub const fn name(&self) -> Range {
        Range::new(self.name_offset, self.name_count)
    }
    pub const fn value(&self) -> Range {
        Range::new(self.value_offset, self.value_count)
    }
}
impl super::environment::Capabilities {
    pub const fn new(names: Buffer<Range>, bytes: Buffer<u8>) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            names_data: names.data,
            names_capacity: names.capacity,
            names_count: names.count,
            names_stride: names.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn names(&self) -> Buffer<Range> {
        Buffer {
            data: self.names_data,
            capacity: self.names_capacity,
            count: self.names_count,
            stride: self.names_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}
impl super::environment::Arguments {
    pub const fn new(arguments: Buffer<super::environment::Argument>, bytes: Buffer<u8>) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            arguments_data: arguments.data,
            arguments_capacity: arguments.capacity,
            arguments_count: arguments.count,
            arguments_stride: arguments.stride,
            bytes_data: bytes.data,
            bytes_capacity: bytes.capacity,
            bytes_count: bytes.count,
        }
    }
    pub const fn arguments(&self) -> Buffer<super::environment::Argument> {
        Buffer {
            data: self.arguments_data,
            capacity: self.arguments_capacity,
            count: self.arguments_count,
            stride: self.arguments_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}
impl super::files::FileReference {
    pub const fn volume_handle(&self) -> VolumeHandle {
        let (generation, slot) = super::dispatch::unpack_handle(self.volume);
        VolumeHandle::new(generation, slot)
    }
}
impl super::files::FileCloseResult {
    pub const fn recovery(&self) -> super::files::FileHandle {
        let (generation, slot) = super::dispatch::unpack_handle(self.recovery);
        super::files::FileHandle::new(generation, slot)
    }
}
impl super::pending::Selection {
    pub const fn file(&self) -> super::files::FileReference {
        super::files::FileReference {
            size: core::mem::size_of::<super::files::FileReference>() as u32,
            written: 0,
            volume: self.volume,
            entry_id: self.entry_id,
            reserved: self.entry_reserved,
        }
    }
}
impl super::authenticated::Field {
    pub const fn values(&self) -> Buffer<Value> {
        Buffer {
            data: self.values_data,
            capacity: self.values_capacity,
            count: self.values_count,
            stride: self.values_stride,
        }
    }
    pub const fn bytes(&self) -> Buffer<u8> {
        Buffer {
            data: self.bytes_data,
            capacity: self.bytes_capacity,
            count: self.bytes_count,
            stride: 1,
        }
    }
}
