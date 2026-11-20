//! Canonical text for opaque identifiers: Bitcoin base58, preserving leading
//! zero bytes. Binary protocols keep their bytes; text boundaries use this codec.
//! Conversion loops are bounded by the largest identifier (32 bytes/44 digits).
use std::fmt;

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
pub const MAX_BYTES: usize = 32;
pub const MAX_TEXT: usize = 44;
const GROUP_DIGITS: usize = 4;
const RADIX: u32 = 58u32.pow(GROUP_DIGITS as u32);
// Both conversions carry at most 256 * RADIX - 1 in one arithmetic step.
const _: () = assert!(RADIX <= u32::MAX / 256);
const DIGITS: [u8; 128] = {
    let mut digits = [u8::MAX; 128];
    let mut index = 0;
    while index < ALPHABET.len() {
        digits[ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    digits
};

/// Allocation-free formatting for an identifier of at most 32 bytes.
pub struct Text<'a>(pub &'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        assert!(!self.0.is_empty() && self.0.len() <= MAX_BYTES, "identifier width");
        // Four digits per limb keep conversion in u32 and retain 88 bytes of
        // scratch: eleven limbs plus the existing 44-byte output buffer.
        let mut groups = [0u32; MAX_TEXT.div_ceil(GROUP_DIGITS)];
        let mut used = 0;
        for &byte in self.0 {
            let mut carry = u32::from(byte);
            for group in &mut groups[..used] {
                carry += *group * 256;
                *group = carry % RADIX;
                carry /= RADIX;
            }
            if carry != 0 {
                groups[used] = carry;
                used += 1;
            }
        }
        let mut encoded = [b'1'; MAX_TEXT];
        let mut start = MAX_TEXT;
        for (index, &group) in groups[..used].iter().enumerate() {
            let mut group = group;
            for _ in 0..GROUP_DIGITS {
                if index + 1 == used && group == 0 {
                    break;
                }
                start -= 1;
                encoded[start] = ALPHABET[(group % 58) as usize];
                group /= 58;
            }
        }
        start -= self.0.iter().take_while(|&&byte| byte == 0).count();
        output.write_str(std::str::from_utf8(&encoded[start..]).expect("base58 ASCII"))
    }
}

pub fn encode(bytes: &[u8]) -> String {
    Text(bytes).to_string()
}

/// Trace epoch/source words use their binary protocol's little-endian layout.
pub fn trace_text(value: [u64; 2]) -> String {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&value[0].to_le_bytes());
    bytes[8..].copy_from_slice(&value[1].to_le_bytes());
    encode(&bytes)
}

/// Decode exactly N bytes, rejecting wrong widths and noncanonical spellings.
// fuzz: store_contract
pub fn decode<const N: usize>(text: &str) -> Result<[u8; N], &'static str> {
    assert!(N > 0 && N <= MAX_BYTES, "identifier width");
    const INVALID: &str = "identifier must be canonical base58 with the expected byte width";
    if text.is_empty() || text.len() > MAX_TEXT {
        return Err(INVALID);
    }
    let mut bytes = [0; N];
    for group in text.as_bytes().chunks(GROUP_DIGITS) {
        let mut carry = 0u32;
        let mut radix = 1u32;
        for &letter in group {
            let digit = *DIGITS.get(usize::from(letter)).ok_or(INVALID)?;
            if digit == u8::MAX {
                return Err(INVALID);
            }
            carry = carry * 58 + u32::from(digit);
            radix *= 58;
        }
        for byte in bytes.iter_mut().rev() {
            carry += u32::from(*byte) * radix;
            *byte = carry as u8;
            carry >>= 8;
        }
        if carry != 0 {
            return Err(INVALID);
        }
    }
    let zeros = bytes.iter().take_while(|&&byte| byte == 0).count();
    if text.bytes().take_while(|&byte| byte == b'1').count() != zeros {
        return Err(INVALID);
    }
    Ok(bytes)
}

pub fn valid(text: &str, bytes: usize) -> bool {
    match bytes {
        15 => decode::<15>(text).is_ok(),
        16 => decode::<16>(text).is_ok(),
        32 => decode::<32>(text).is_ok(),
        _ => panic!("unsupported identifier width"),
    }
}
