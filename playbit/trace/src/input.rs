//! Host-side decoding and validation of guest trace submissions and event bytes.
use playbit_api::abi::{
    syscall::Error,
    trace::{END, HEADER_BYTES, INSTANT, MAX_BATCH_BYTES, START, TraceEvent, event_bytes},
};

#[derive(Clone, Copy, Debug)]
pub struct Event<'a> {
    pub header: TraceEvent,
    pub metadata: &'a [u8],
    pub bytes: &'a [u8],
}
pub struct Batch<'a> {
    events: &'a [u8],
}
impl<'a> Batch<'a> {
    pub fn events(&self) -> impl Iterator<Item = Event<'a>> + use<'a> {
        let mut rest = self.events;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let (event, tail) = decode_event(rest).expect("batch was completely validated");
            rest = tail;
            Some(event)
        })
    }
}
fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("admitted integer extent"))
}

pub fn decode_event(bytes: &[u8]) -> Result<(Event<'_>, &[u8]), Error> {
    let h = bytes.get(..HEADER_BYTES).ok_or(Error::InvalidArgument)?;
    let header = TraceEvent {
        timestamp: word(h, 0),
        reference_eid: word(h, 8),
        value: word(h, 16),
        reference_tid: u32::from_le_bytes(h[24..28].try_into().expect("field")),
        metadata_len: u32::from_le_bytes(h[28..32].try_into().expect("field")),
        name: u16::from_le_bytes(h[32..34].try_into().expect("field")),
        kind: h[34],
        reserved: h[35..40].try_into().expect("field"),
    };
    let extent = event_bytes(header.metadata_len as usize)?;
    let whole = bytes.get(..extent).ok_or(Error::InvalidArgument)?;
    let end = HEADER_BYTES + header.metadata_len as usize;
    if header.reserved != [0; 5]
        || whole[end..].iter().any(|&byte| byte != 0)
        || header.timestamp == 0
        || !matches!(header.kind, START | END | INSTANT)
        || (header.reference_eid == 0 && header.reference_tid != 0)
        || (header.reference_eid != 0
            && header.reference_tid == 0
            && header.reference_eid >= header.timestamp)
        || (header.kind == END
            && (header.reference_eid == 0 || header.name != 0 || header.value != 0))
        || (header.kind == START && header.value != 0)
    {
        return Err(Error::InvalidArgument);
    }
    Ok((Event { header, metadata: &whole[HEADER_BYTES..end], bytes: whole }, &bytes[extent..]))
}

/// Validate the entire batch before any publication. Names are checked by the
/// host's immutable declaration catalog; references need no retained history.
pub fn validate_batch(
    bytes: &[u8],
    mut valid_name: impl FnMut(u16, u8) -> bool,
) -> Result<Batch<'_>, Error> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(Error::LimitExceeded);
    }
    let batch = Batch { events: bytes };
    let mut rest = batch.events;
    while !rest.is_empty() {
        let (event, tail) = decode_event(rest)?;
        if event.header.reference_tid != 0 {
            return Err(Error::Unsupported);
        }
        if event.header.kind != END && !valid_name(event.header.name, event.header.kind) {
            return Err(Error::InvalidArgument);
        }
        rest = tail;
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use playbit_api::abi::trace::{MAX_METADATA_BYTES, encode_header};
    #[test]
    fn metadata_events_validate_without_reference_history() {
        // Round-trip each event kind and reject truncated extents without history.
        for kind in [START, END, INSTANT] {
            let metadata = br#"{"actor_type":"funny_monster","position":{"x":12.5}}"#;
            let mut bytes = vec![0; event_bytes(metadata.len()).expect("size")];
            let header = TraceEvent {
                timestamp: 100,
                reference_eid: 42,
                kind,
                name: if kind == END { 0 } else { 1 },
                metadata_len: metadata.len() as u32,
                ..TraceEvent::default()
            };
            encode_header(header, &mut bytes).expect("header");
            bytes[HEADER_BYTES..][..metadata.len()].copy_from_slice(metadata);
            let batch = validate_batch(&bytes, |name, _| name == 1)
                .expect("out of order reference accepted");
            assert_eq!(batch.events().next().expect("event").metadata, metadata);
            for length in 1..bytes.len() {
                assert!(validate_batch(&bytes[..length], |_, _| true).is_err());
            }
            bytes[24] = 1;
            assert_eq!(validate_batch(&bytes, |_, _| true).err(), Some(Error::Unsupported));
        }
    }
    #[test]
    fn whole_batch_rejects_bad_padding_tags_names_and_extents() {
        // Mutate one valid event to exercise independent structural rejection paths.
        let mut bytes = vec![0; 48];
        encode_header(
            TraceEvent { timestamp: 1, kind: INSTANT, metadata_len: 1, ..TraceEvent::default() },
            &mut bytes,
        )
        .expect("header");
        bytes[40] = b'{'; // JSON validation belongs to readers.
        assert!(validate_batch(&bytes, |_, _| true).is_ok());
        assert!(validate_batch(&bytes, |_, _| false).is_err());
        for offset in [34, 35, 47] {
            let mut invalid = bytes.clone();
            invalid[offset] = 255;
            assert!(validate_batch(&invalid, |_, _| true).is_err());
        }
        assert_eq!(event_bytes(MAX_METADATA_BYTES + 1), Err(Error::LimitExceeded));
    }
}
