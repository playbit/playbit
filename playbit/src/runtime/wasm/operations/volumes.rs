use super::{StoreData, call, output::Buffer};
use crate::runtime::linear_memory::layout::data::{
    entry_handle, range as range_layout, volume as volume_layout, volumes as descriptor_layout,
};
use crate::runtime::syscall::{HostError, environment};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

fn write_volumes(
    state: &crate::runtime::instance::State,
    memory: &mut crate::runtime::linear_memory::Memory<'_>,
    volumes: &Buffer,
    tags: &Buffer,
    bytes: &Buffer,
    files: &[playbit_api::abi::files::FileHandle],
    metadata: &[([u8; 16], u32)],
) -> crate::runtime::host::HostResult<(usize, usize)> {
    let mut used_bytes = 0;
    let mut used_tags = 0;
    for (index, (handle, volume)) in environment::available_volumes(state)?.enumerate() {
        let name = bytes.append(memory.data, &mut used_bytes, volume.name.as_bytes());
        let record = volumes.element(memory.data, index);
        record.write(
            memory.data,
            volume_layout::HANDLE,
            &playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot).to_le_bytes(),
        );
        record.write(memory.data, volume_layout::ID, &volume.id);
        record.write(memory.data, volume_layout::NAME_OFFSET, &name);
        record.write(memory.data, volume_layout::TAGS_OFFSET, &(used_tags as u32).to_le_bytes());
        record.write(
            memory.data,
            volume_layout::TAGS_COUNT,
            &(volume.tags.len() as u32).to_le_bytes(),
        );
        record.write(memory.data, volume_layout::RIGHTS, &volume.rights.0.to_le_bytes());
        record.write(
            memory.data,
            volume_layout::FLAGS,
            &(u32::from(volume.is_root) | metadata[index].1).to_le_bytes(),
        );
        record.write(memory.data, volume_layout::FILE, &files[index].0.to_le_bytes());
        record.write(memory.data, volume_layout::GRANT_ID, &metadata[index].0);
        if record.len() >= volume_layout::ROLE + 4 {
            record.write(memory.data, volume_layout::ROLE, &volume.role.to_le_bytes());
        }
        volumes.publish(memory.data, index);
        for tag in &volume.tags {
            let range = bytes.append(memory.data, &mut used_bytes, tag.as_bytes());
            tags.element(memory.data, used_tags).write(memory.data, range_layout::OFFSET, &range);
            tags.publish(memory.data, used_tags);
            used_tags += 1;
        }
    }
    Ok((used_bytes, used_tags))
}
pub(super) fn volumes(caller: Caller<'_, impl StoreData>, pointer: u32) -> i64 {
    call(caller, |state, memory| {
        let descriptor = memory.record(pointer, descriptor_layout::LAYOUT, false, None)?;
        let volumes = Buffer::records(
            memory,
            descriptor.sub(descriptor_layout::VOLUMES_DATA, 16),
            volume_layout::LAYOUT,
            contract::MAX_VOLUMES as u32,
        )?;
        let tags = Buffer::records(
            memory,
            descriptor.sub(descriptor_layout::TAGS_DATA, 16),
            range_layout::LAYOUT,
            contract::MAX_VOLUMES as u32 * 64,
        )?;
        let bytes = Buffer::admit(
            memory,
            descriptor.sub(descriptor_layout::BYTES_DATA, 12),
            1,
            1,
            contract::MAX_DATA_BYTES as u32,
        )?;
        crate::runtime::linear_memory::record::outputs(&[
            descriptor,
            volumes.data,
            tags.data,
            bytes.data,
        ])?;
        descriptor.clear(memory.data);
        volumes.clear(memory.data);
        tags.clear(memory.data);
        let mut count = 0;
        let mut tag_count = 0;
        let mut size = 0;
        for (_, volume) in environment::available_volumes(state)? {
            count += 1;
            tag_count += volume.tags.len();
            size += volume.name.len() + volume.tags.iter().map(String::len).sum::<usize>();
        }
        if !volumes.fits(count) || !tags.fits(tag_count) || !bytes.fits(size) {
            volumes.count(memory.data, count);
            tags.count(memory.data, tag_count);
            bytes.count(memory.data, size);
            return Err(HostError::BufferTooSmall);
        }
        let grants = state.context.volumes.iter().map(|volume| volume.id).collect::<Vec<_>>();
        let metadata = grants
            .iter()
            .map(|id| state.files.root_metadata(*id))
            .collect::<crate::runtime::host::HostResult<Vec<_>>>()?;
        let files = state.files.enumerate(&mut state.capabilities, &grants)?;
        descriptor.write(
            memory.data,
            descriptor_layout::REVISION,
            &state.volume_revision.to_le_bytes(),
        );
        volumes.count(memory.data, count);
        tags.count(memory.data, tag_count);
        bytes.count(memory.data, size);

        let (used_bytes, used_tags) =
            write_volumes(state, memory, &volumes, &tags, &bytes, &files, &metadata)?;
        assert_eq!((used_bytes, used_tags), (size, tag_count));
        descriptor.publish(memory.data, descriptor_layout::LAYOUT);
        Ok(0)
    })
}
pub(super) fn resolve_begin(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    name: u32,
    length: u32,
) -> i64 {
    call(caller, |state, memory| {
        use crate::runtime::{
            host::DataOperation,
            syscall::{Opcode, VolumeHandle, pending_data},
        };
        let volume = VolumeHandle { generation, slot };
        pending_data::admit_begin(state, volume, Opcode::EntryResolveBegin)?;
        let name = memory.input(
            name,
            length,
            1,
            1,
            playbit_api::abi::files::MAX_FILE_PATH_BYTES as u32,
            None,
        )?;
        let name = std::str::from_utf8(name.bytes(memory.data))
            .map_err(|_| HostError::InvalidArgument)?
            .to_owned();
        let pending = pending_data::begin(
            state,
            volume,
            DataOperation::ResolveEntry { name },
            Opcode::EntryResolveBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}

pub(super) fn resolve_take(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
) -> i64 {
    call(caller, |state, memory| {
        use crate::runtime::syscall::{Opcode, OperationHandle, pending_data};
        let pending = OperationHandle { generation, slot };
        pending_data::admit_take(state, pending, Opcode::EntryResolveTake)?;
        let output = memory.record(output, entry_handle::LAYOUT, false, None)?;
        output.clear(memory.data);
        let entry = pending_data::resolve_entry_take(state, pending)?;
        output.write(memory.data, entry_handle::VOLUME, &entry.volume.to_le_bytes());
        output.write(memory.data, entry_handle::ENTRY_ID, &entry.entry_id);
        output.write(memory.data, entry_handle::RESERVED, &[0]);
        output.publish(memory.data, entry_handle::LAYOUT);
        Ok(0)
    })
}
