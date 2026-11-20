//! Portable database references contain identity only. Resolution always checks
//! current volume and entry authority; possession of a reference grants nothing.
use super::{EntryId, Error, Id, Result};
use std::fmt;

const PREFIX: &str = "playbit:database:";
/// Maximum canonical UTF-8 size, including the final newline.
pub const MAX_DATABASE_REFERENCE_BYTES: usize = PREFIX.len() + 22 + 1 + 21 + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabaseReference {
    pub volume: Id,
    pub entry: EntryId,
}

impl DatabaseReference {
    // fuzz: store_contract
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        const INVALID: Error = Error::InvalidInput("invalid Playbit database reference");
        if bytes.len() > MAX_DATABASE_REFERENCE_BYTES {
            return Err(INVALID);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| INVALID)?;
        let text = text.strip_prefix(PREFIX).and_then(|s| s.strip_suffix('\n')).ok_or(INVALID)?;
        let (volume, entry) = text.split_once(':').ok_or(INVALID)?;
        let volume = Id::parse(volume)?;
        if volume == Id::from_bytes([0; 16]) {
            return Err(INVALID);
        }
        Ok(Self { volume, entry: EntryId::parse(entry)? })
    }
}

impl fmt::Display for DatabaseReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{PREFIX}{}:{}", self.volume, self.entry)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert reference codec outcomes.")]
mod tests {
    use super::*;

    #[test]
    fn exported_reference_roundtrips_and_rejects_alternate_or_extended_payloads() {
        // Export a qualified identity, then reject encodings that could hide data or authority.
        let reference = DatabaseReference {
            volume: Id::from_bytes([0xab; 16]),
            entry: EntryId::from_bytes([0xcd; 15]).unwrap(),
        };
        let text = reference.to_string();
        assert!(text.len() <= MAX_DATABASE_REFERENCE_BYTES);
        for zeros in 0..15 {
            let mut volume = [0; 16];
            let mut entry = [0; 15];
            volume[zeros] = 1;
            entry[zeros] = 1;
            let short = DatabaseReference {
                volume: Id::from_bytes(volume),
                entry: EntryId::from_bytes(entry).unwrap(),
            };
            assert_eq!(DatabaseReference::parse(short.to_string().as_bytes()).unwrap(), short);
        }
        assert_eq!(DatabaseReference::parse(text.as_bytes()).unwrap(), reference);
        for invalid in [
            text.to_uppercase(),
            text.trim_end().to_owned(),
            format!("{text}credentials"),
            text.replace(&reference.volume.to_string(), &"00".repeat(16)),
            text.replace(&reference.entry.to_string(), &"00".repeat(15)),
            text.replace('\n', "\r\n"),
        ] {
            assert!(DatabaseReference::parse(invalid.as_bytes()).is_err());
        }
        assert!(DatabaseReference::parse(&[0xff; MAX_DATABASE_REFERENCE_BYTES]).is_err());
    }
}
