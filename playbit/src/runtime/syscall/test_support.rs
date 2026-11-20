//! Exercise operation imports through actual Wasm calls and guest memory.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

pub(super) fn call<P: wasmtime::WasmParams>(guest: &mut GuestInstance, name: &str, args: P) -> i32 {
    guest
        .instance
        .get_typed_func::<P, i32>(&mut guest.store, name)
        .unwrap()
        .call(&mut guest.store, args)
        .unwrap()
}

pub(super) fn set_volume_rights(state: &mut State, index: usize, rights: VolumeRights) {
    state.context.volumes[index].rights = rights;
    let volume = state.context.volumes[index].clone();
    state
        .capabilities
        .replace_volume(crate::capability::Handle(state.volume_handles[index]), volume)
        .unwrap();
}

pub(super) fn invoke(
    guest: &mut GuestInstance,
    name: &str,
    (opcode, a, b, c, d, e, f): (u32, u64, u64, u64, u64, u64, u64),
) -> i32 {
    let operation = Opcode::from_raw(opcode).unwrap();
    let mut args = [a, b, c, d, e, f];
    let output = if operation.capability().ends_with(".begin")
        || matches!(
            operation,
            Opcode::FileOpenTake | Opcode::FileListOpenTake | Opcode::ChannelOpen
        ) {
        let index = playbit_api::abi::dispatch::layout(operation).count as usize;
        let output = args[index] as usize;
        args[index] = 0;
        Some(output)
    } else {
        None
    };
    let result = guest
        .instance
        .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut guest.store, name)
        .unwrap()
        .call(&mut guest.store, (opcode, args[0], args[1], args[2], args[3], args[4], args[5]))
        .unwrap();
    if let Some(output) = output
        && result > 0
    {
        write(guest, output, &result.to_le_bytes());
        return 0;
    }
    result as i32
}

pub(super) fn write(guest: &mut GuestInstance, offset: usize, bytes: &[u8]) {
    let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
    memory.write(&mut guest.store, offset, bytes).unwrap();
}

pub(super) fn read(guest: &mut GuestInstance, offset: usize, size: usize) -> Vec<u8> {
    let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
    let mut bytes = vec![0; size];
    memory.read(&guest.store, offset, &mut bytes).unwrap();
    bytes
}

pub(super) fn operation_handle(guest: &mut GuestInstance, offset: usize) -> OperationHandle {
    let bytes = read(guest, offset, 8);
    OperationHandle {
        generation: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        slot: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
    }
}

pub(super) fn capabilities(guest: &mut GuestInstance) -> BTreeSet<String> {
    let mut descriptor = Vec::new();
    for value in [36u32, 0, 8192, 64, 0, 16, 9216, 4096, 0] {
        descriptor.extend(value.to_le_bytes());
    }
    write(guest, 64, &descriptor);
    for index in 0..64 {
        write(guest, 8192 + index * 16, &[16, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(call(guest, "capabilities", 64u32), 0);
    let count = u32::from_le_bytes(read(guest, 80, 4).try_into().unwrap());
    let records = read(guest, 8192, count as usize * 16);
    let bytes = read(guest, 9216, 4096);
    records
        .as_chunks::<16>()
        .0
        .iter()
        .map(|record| {
            let start = u32::from_le_bytes(record[8..12].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(record[12..].try_into().unwrap()) as usize;
            std::str::from_utf8(&bytes[start..start + length]).unwrap().to_owned()
        })
        .collect()
}

pub(super) fn entry_pointers(guest: &mut GuestInstance, handle: VolumeHandle) -> (u32, u32) {
    for (pointer, generation) in [(60000, handle.generation), (60032, handle.generation + 1)] {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend(32_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(handle.slot.to_le_bytes());
        bytes.extend(generation.to_le_bytes());
        bytes.extend([5; 15]);
        bytes.push(0);
        write(guest, pointer, &bytes);
    }
    (60000, 60032)
}
