//! Scalar syscall opcode and slot admission shared by native and Wasm hosts.
//! No memory is accessed here. Packed handle and offset slots keep all 64 bits;
//! narrow values, pointers and unused slots are checked before dispatch.
use super::syscall::{Error, Opcode};

/// A handle is one scalar with slot in the low word and generation in the high
/// word (the sign bit is always zero). Explicit shifts avoid compiler-dependent by-value struct lowering.
pub const fn pack_handle(generation: u32, slot: u32) -> u64 {
    ((generation as u64) << 32) | slot as u64
}
pub const fn unpack_handle(handle: u64) -> (u32, u32) {
    ((handle >> 32) as u32, handle as u32)
}

/// Exact scalar layout. Bits name argument slots a..f, least significant first.
/// Narrow values are zero-extended u32; pointers have the target address width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub count: u8,
    pub pointers: u8,
    pub wide: u8,
    pub handle: bool,
}

#[inline]
pub const fn layout(opcode: Opcode) -> Layout {
    use Opcode::*;
    let (count, pointers, wide, handle) = match opcode {
        AudioOpenBegin => (1, 0, 0, false),
        AudioOpenTake => (1, 0, 1, true),
        AudioClose | AudioSampleRate | AudioVolume | SoundClose => (1, 0, 1, true),
        AudioSetVolume | SoundStop => (2, 0, 1, true),
        SoundOpenPcm => (5, 0b001000, 1, true),
        SoundPlay => (6, 0, 0b011001, true),
        SoundTell => (3, 0b000100, 1, true),
        SoundSeek => (3, 0, 0b000101, true),
        SoundSetGain | SoundSetBalance => (3, 0, 1, true),
        Session => (0, 0, 0, false),
        ChannelOpen => (2, 0b000001, 0, false),
        ChannelBegin => (4, 0b000100, 0b000001, true),
        ChannelTake => (1, 0, 0b000001, true),
        KvKeysBegin => (4, 0b000111, 0, false),
        KvReadBegin => (4, 0b001011, 0, false),
        KvPutBegin => (5, 0b001011, 0, false),
        KvDeleteBegin => (3, 0b000011, 0, false),
        KvKeysTake | KvReadTake => (4, 0b001010, 0b000001, true),
        KvPutTake | KvDeleteTake => (1, 0, 0b000001, true),
        BloblogAppendBegin => (6, 0b100110, 0b000001, true),
        BloblogReadBegin => (4, 0b000110, 0b000001, true),
        BloblogAppendTake | BloblogReadTake => (4, 0b001010, 0b000001, true),
        ObjectCreateBegin | ObjectEditBegin => (5, 0b000111, 0, false),
        ObjectReadBegin => (4, 0b000111, 0, false),
        ObjectCreatePoll | ObjectReadTake | ObjectQueryTake | SchemaReadTake | EntryResolveTake => {
            (2, 0b000010, 0b000001, true)
        }
        SchemaReadBegin => (1, 0b000001, 0, false),
        ObjectEditTake | SchemaEstablishTake => (1, 0, 0b000001, true),
        SchemaEstablishBegin => (4, 0b000011, 0, false),
        ObjectQueryBegin => (2, 0b000011, 0, false),
        EntryResolveBegin => (3, 0b000010, 0b000001, true),
        FileCopyBegin => (2, 0b000011, 0b000000, false),
        FileCopyPoll => (2, 0b000010, 0b000001, true),
        FileSelectBegin => (0, 0b000000, 0b000000, false),
        FileSelectPoll => (2, 0b000010, 0b000001, true),
        FileReadBegin => (3, 0b000000, 0b000011, true),
        FileReadTake => (4, 0b001010, 0b000001, true),
        FileWriteBegin => (4, 0b000100, 0b000011, true),
        FileWriteTake => (1, 0b000000, 0b000001, true),
        FileOpenBegin => (4, 0b000010, 0b000001, true),
        FileOpenTake | FileListOpenTake => (1, 0, 0b000001, true),
        FileStatusTake | FileCloseTake | FileListNextTake => (2, 0b000010, 0b000001, true),
        FileStatusBegin | FileListNextBegin => (1, 0b000000, 0b000001, true),
        FileCloseBegin | FileListOpenBegin => (2, 0b000000, 0b000001, true),
        FileListClose | OperationDispose | FileDispose => (1, 0, 0b000001, true),
        AvailableVolumes => (1, 0b000001, 0b000000, false),
        VolumesObserve => (1, 0, 1, false),
        ObjectFieldReadAuthenticatedBegin => (3, 0b000111, 0, false),
        ObjectFieldReadAuthenticatedTake => (2, 0b000010, 0b000001, true),
        HostPlatform => (0, 0b000000, 0b000000, false),
        TraceRecord => (4, 0b000001, 0b001100, false),
        ClockRead => (0, 0, 0, false),
        RandomFill => (2, 0b000001, 0, false),
        HostLimit | QuitOnLastWindow => (1, 0b000000, 0b000000, false),
        Capabilities => (1, 0b000001, 0b000000, false),
        StartupArguments => (1, 0b000001, 0b000000, false),
        Presence => (4, 0b001001, 0b000000, false),
        Log | ViewSubmit => (2, 0b000001, 0b000000, false),
        EventPoll => (3, 0b000011, 0b000000, false),
        EventPollDeadline => (4, 0b000011, 0b001000, false),
        Identity | SessionInfo => (1, 0b000001, 0b000000, false),
        WakeAfter => (1, 0b000000, 0b000000, false),
        UpdateDefer => (1, 0b000000, 0b000001, false),
    };
    Layout { count, pointers, wide, handle }
}

/// Reject noncanonical slots rather than truncating attacker-controlled bits.
/// Only 32- and 64-bit pointer targets are supported by this ABI.
#[inline]
pub fn admit(operation: u32, arguments: &[u64; 6], pointer_bits: u32) -> Result<Opcode, Error> {
    let opcode = Opcode::from_raw(operation).ok_or(Error::Unsupported)?;
    let shape = layout(opcode);
    if !slots_fit(shape, arguments, pointer_bits) {
        return Err(Error::InvalidArgument);
    }
    Ok(opcode)
}

#[inline]
fn slots_fit(shape: Layout, arguments: &[u64; 6], pointer_bits: u32) -> bool {
    if pointer_bits != 32 && pointer_bits != 64 {
        return false;
    }
    for (index, &value) in arguments.iter().enumerate() {
        let bit = 1 << index;
        let full_width = shape.wide & bit != 0 || (pointer_bits == 64 && shape.pointers & bit != 0);
        if (index >= shape.count as usize && value != 0) || (!full_width && value > u32::MAX as u64)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::syscall::REGISTRY;

    #[test]
    fn every_operation_has_an_exact_nonoverlapping_slot_layout() {
        // Every assigned slot is a pointer, full-width scalar or narrow scalar;
        // no unused slot or absent handle may influence the operation.
        for &(opcode, _) in REGISTRY {
            let shape = layout(opcode);
            assert!(shape.count <= 6);
            assert_eq!(shape.pointers & shape.wide, 0);
            assert_eq!((shape.pointers | shape.wide) >> shape.count, 0);
            for bits in [32, 64] {
                assert_eq!(admit(opcode as u32, &[0; 6], bits), Ok(opcode));
                for index in 0..6 {
                    let bit = 1 << index;
                    let mut arguments = [0; 6];
                    arguments[index] = u64::MAX;
                    let accepted = index < shape.count as usize
                        && (shape.wide & bit != 0 || (bits == 64 && shape.pointers & bit != 0));
                    assert_eq!(admit(opcode as u32, &arguments, bits).is_ok(), accepted);
                    arguments[index] = 1;
                    assert_eq!(
                        admit(opcode as u32, &arguments, bits).is_ok(),
                        index < shape.count as usize
                    );
                }
            }
        }
        assert_eq!(admit(u32::MAX, &[0; 6], 64), Err(Error::Unsupported));
        assert_eq!(admit(Opcode::Log as u32, &[0; 6], 16), Err(Error::InvalidArgument));
    }

    #[test]
    fn handles_preserve_generation_and_slot_bits() {
        // Both full-width u32 fields survive a single scalar argument.
        let generation = 0x7edc_ba98;
        let slot = 0x89ab_cdef;
        let packed = pack_handle(generation, slot);
        assert_eq!(packed, 0x7edc_ba98_89ab_cdef);
        assert_eq!(unpack_handle(packed), (generation, slot));
        assert_eq!(
            admit(Opcode::EntryResolveBegin as u32, &[packed, 0, 0, 0, 0, 0], 32),
            Ok(Opcode::EntryResolveBegin)
        );
    }
}
