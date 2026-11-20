#![no_main]
use libfuzzer_sys::fuzz_target;
// Coverage retains native SDK calls; link their production C ABI definitions.
use playbit as _;
use playbit_api::abi::trace;
use playbit_trace::{format, input};

fuzz_target!(|bytes: &[u8]| {
    // Exercise the byte-level codecs without requiring valid outer checksums.
    if let Ok((event, tail)) = input::decode_event(bytes) {
        let mut header = [0; trace::HEADER_BYTES];
        trace::encode_header(event.header, &mut header).expect("decoded header encodes");
        assert_eq!(header, bytes[..trace::HEADER_BYTES]);
        assert_eq!(event.bytes.len() + tail.len(), bytes.len());
        assert_eq!(event.metadata.len(), event.header.metadata_len as usize);
    }
    if let Ok(batch) = input::validate_batch(bytes, |name, _| name < 256) {
        let total: usize = batch.events().map(|event| event.bytes.len()).sum();
        assert_eq!(total, bytes.len());
    }
    let _ = format::Events::decode(bytes);
    let _ = format::AppDefinition::decode(bytes);
    let _ = format::AppState::decode(bytes);
    if let Ok(segment) = format::Segment::decode(bytes) {
        assert_eq!(segment.encode().as_slice(), &bytes[..format::SEGMENT_HEADER_BYTES]);
    }
    if let Ok(status) = <&[u8; playbit_trace::control::STATUS_BYTES]>::try_from(bytes) {
        let _ = playbit_trace::control::Status::decode(status);
    }
    let mut integer = bytes;
    let _ = format::read_integer(&mut integer);
    if let Ok((envelope, payload)) = format::Envelope::decode(bytes) {
        let mut encoded = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        envelope.encode(payload, &mut encoded).expect("admitted block re-encodes");
        assert_eq!(encoded, bytes);
        if envelope.kind == format::BlockKind::Events {
            let _ = format::Events::decode(payload);
        }
    }
});
