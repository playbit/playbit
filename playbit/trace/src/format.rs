//! Durable bounded event blocks. Historical semantic payload tags are unsupported,
//! never reinterpreted as this event format. Existing segment bytes are preserved.
mod app;
pub use app::{
    APP_LABEL_BYTES, APP_NAMES, AppDefinition, AppIdentity, AppNameKind, AppState, valid_app_name,
};
pub const SEGMENT_HEADER_BYTES: usize = 48;
pub const BLOCK_HEADER_BYTES: usize = 64;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_BLOCK_BYTES: usize = BLOCK_HEADER_BYTES + MAX_PAYLOAD_BYTES;
const MAGIC: &[u8; 8] = b"PBTRAC01";
const BLOCK_MAGIC: &[u8; 4] = b"PBTB";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Incomplete,
    Invalid,
    Unsupported,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    pub epoch: [u64; 2],
    pub ordinal: u64,
}

impl Segment {
    pub fn encode(self) -> [u8; SEGMENT_HEADER_BYTES] {
        let mut bytes = [0; SEGMENT_HEADER_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        put(&mut bytes, 8, self.epoch[0]);
        put(&mut bytes, 16, self.epoch[1]);
        put(&mut bytes, 24, self.ordinal);
        put(&mut bytes, 32, SEGMENT_HEADER_BYTES as u64);
        let checksum = checksum(&bytes[..40]);
        bytes[40..44].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < SEGMENT_HEADER_BYTES {
            return Err(DecodeError::Incomplete);
        }
        if &bytes[..8] != MAGIC {
            return Err(DecodeError::Unsupported);
        }
        if get(bytes, 32) != SEGMENT_HEADER_BYTES as u64
            || bytes[44..48] != [0; 4]
            || u32::from_le_bytes(bytes[40..44].try_into().expect("header checksum"))
                != checksum(&bytes[..40])
        {
            return Err(DecodeError::Invalid);
        }
        let result = Self { epoch: [get(bytes, 8), get(bytes, 16)], ordinal: get(bytes, 24) };
        if result.epoch == [0, 0] || result.ordinal == 0 {
            return Err(DecodeError::Invalid);
        }
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BlockKind {
    Events = 13,
    Source = 15,
    Loss = 16,
    Epoch = 5,
    AppDefinition = 14,
    AppState = 18,
    Recording = 17,
}

impl TryFrom<u8> for BlockKind {
    type Error = DecodeError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            13 => Ok(Self::Events),
            15 => Ok(Self::Source),
            16 => Ok(Self::Loss),
            5 => Ok(Self::Epoch),
            14 => Ok(Self::AppDefinition),
            18 => Ok(Self::AppState),
            17 => Ok(Self::Recording),
            _ => Err(DecodeError::Unsupported),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Envelope {
    pub kind: BlockKind,
    pub epoch: [u64; 2],
    pub first: u64,
    pub count: u32,
    pub source: [u64; 2],
    pub time_base: u64,
}

impl Envelope {
    pub fn encode(self, payload: &[u8], output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if payload.len() > MAX_PAYLOAD_BYTES
            || output.capacity() < BLOCK_HEADER_BYTES + payload.len()
        {
            return Err(DecodeError::Invalid);
        }
        self.validate()?;
        output.clear();
        output.resize(BLOCK_HEADER_BYTES, 0);
        output[..4].copy_from_slice(BLOCK_MAGIC);
        output[4] = self.kind as u8;
        // Codec zero is compact raw. Other codecs require explicit decoding.
        output[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        output[6..8].copy_from_slice(&(self.count as u16).to_le_bytes());
        for (offset, value) in [
            (16, self.epoch[0]),
            (24, self.epoch[1]),
            (32, self.first),
            (40, self.source[0]),
            (48, self.source[1]),
            (56, self.time_base),
        ] {
            put(output, offset, value);
        }
        output.extend_from_slice(payload);
        let crc = checksum(output);
        output[12..16].copy_from_slice(&crc.to_le_bytes());
        Ok(())
    }

    fn validate(self) -> Result<(), DecodeError> {
        if self.epoch == [0, 0]
            || self.first == 0
            || self.count == 0
            || self.first.checked_add(u64::from(self.count) - 1).is_none()
            || (self.source == [0, 0]) != (self.kind == BlockKind::Epoch)
            || (self.kind == BlockKind::Events
                && (self.count as usize > 1 || self.source == [0, 0]))
            || (self.kind != BlockKind::Events && self.count != 1)
        {
            return Err(DecodeError::Invalid);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), DecodeError> {
        let length = Self::encoded_length(bytes)?;
        if bytes.len() < length {
            return Err(DecodeError::Incomplete);
        }
        if bytes.len() != length {
            return Err(DecodeError::Invalid);
        }
        let expected = u32::from_le_bytes(bytes[12..16].try_into().expect("block checksum"));
        let mut crc = !0;
        for (i, byte) in bytes.iter().enumerate() {
            crc = checksum_byte(crc, if (12..16).contains(&i) { 0 } else { *byte });
        }
        if !crc != expected {
            return Err(DecodeError::Invalid);
        }
        let envelope = Self {
            kind: bytes[4].try_into()?,
            epoch: [get(bytes, 16), get(bytes, 24)],
            first: get(bytes, 32),
            count: u32::from(u16::from_le_bytes(bytes[6..8].try_into().expect("record count"))),
            source: [get(bytes, 40), get(bytes, 48)],
            time_base: get(bytes, 56),
        };
        envelope.validate()?;
        Ok((envelope, &bytes[BLOCK_HEADER_BYTES..]))
    }

    pub fn encoded_length(bytes: &[u8]) -> Result<usize, DecodeError> {
        if bytes.len() < BLOCK_HEADER_BYTES {
            return Err(DecodeError::Incomplete);
        }
        if &bytes[..4] != BLOCK_MAGIC || bytes[5] != 0 {
            return Err(DecodeError::Unsupported);
        }
        let length = u32::from_le_bytes(bytes[8..12].try_into().expect("payload length")) as usize;
        if length > MAX_PAYLOAD_BYTES {
            return Err(DecodeError::Invalid);
        }
        Ok(BLOCK_HEADER_BYTES + length)
    }
}

fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn get(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8].try_into().expect("validated fixed-width envelope"),
    )
}

// One read-only KiB shared by all checksum callers. Compute the polynomial
// table at compile time; collection and tail decoding do one lookup per byte.
static CHECKSUM_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = (value >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(value & 1)));
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
};

fn checksum_byte(crc: u32, byte: u8) -> u32 {
    (crc >> 8) ^ CHECKSUM_TABLE[usize::from(crc as u8 ^ byte)]
}

pub fn checksum(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0, |crc, byte| checksum_byte(crc, *byte))
}

pub fn integer(mut value: u64, output: &mut Vec<u8>) -> Result<(), DecodeError> {
    // Ten bytes cover the entire u64 domain; the buffer never reallocates.
    for _ in 0..10 {
        if output.len() == output.capacity() || output.len() == MAX_PAYLOAD_BYTES {
            return Err(DecodeError::Invalid);
        }
        let byte = value as u8 & 0x7f;
        value >>= 7;
        output.push(byte | if value != 0 { 0x80 } else { 0 });
        if value == 0 {
            return Ok(());
        }
    }
    Err(DecodeError::Invalid)
}

pub fn read_integer(input: &mut &[u8]) -> Result<u64, DecodeError> {
    let mut value = 0;
    for index in 0..10 {
        let Some((&byte, rest)) = input.split_first() else {
            return Err(DecodeError::Incomplete);
        };
        *input = rest;
        if index == 9 && byte > 1 {
            return Err(DecodeError::Invalid);
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            if index != 0 && byte == 0 {
                return Err(DecodeError::Invalid);
            }
            return Ok(value);
        }
    }
    Err(DecodeError::Invalid)
}

/// One complete event and its trusted producer attribution. The source incarnation
/// lives in the block envelope; scope identifies a guest execution within it.
pub struct Events<'a> {
    pub scope: u32,
    pub tid: u32,
    pub generation: u64,
    pub event: crate::input::Event<'a>,
}
impl<'a> Events<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if bytes.len() < 16 {
            return Err(DecodeError::Incomplete);
        }
        let scope = u32::from_le_bytes(bytes[..4].try_into().expect("fixed scope"));
        let tid = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed tid"));
        let generation = get(bytes, 8);
        if tid == 0 || generation == 0 {
            return Err(DecodeError::Invalid);
        }
        // Decoding only borrows the original payload and does not retain scratch.
        let (event, tail) =
            crate::input::decode_event(&bytes[16..]).map_err(|_| DecodeError::Invalid)?;
        if event.header.reference_eid != 0
            && event.header.reference_tid == tid
            && event.header.reference_eid >= event.header.timestamp
        {
            return Err(DecodeError::Invalid);
        }
        if !tail.is_empty() {
            return Err(DecodeError::Invalid);
        }
        Ok(Self { scope, tid, generation, event })
    }
}
