//! Fragmentation, allocation and end-of-stream tests for the transport-free reader.
#![allow(clippy::unwrap_used, reason = "Tests assert fixture setup and protocol outcomes.")]
use super::*;
use crate::network::ap::{MAX_FRAME_LEN, frame_type};

fn frame() -> Frame {
    Frame::new(frame_type::DIRECT, 0, b"service", b"account", vec![1; 64]).unwrap()
}
fn feed(reader: &mut FrameReader, input: &[u8], quantum: usize) -> Result<usize> {
    let mut consumed = 0;
    while consumed < input.len() {
        let buffer = reader.buffer()?;
        let count = buffer.len().min(quantum).min(input.len() - consumed);
        buffer[..count].copy_from_slice(&input[consumed..consumed + count]);
        consumed += count;
        if reader.advance(count)? {
            break;
        }
        assert!(count > 0);
    }
    Ok(consumed)
}
#[test]
fn fragments_and_coalesced_frames_preserve_exact_boundaries() {
    // Every small read quantum yields the same frame and leaves the next intact.
    let first = frame();
    let wire = first.encode().unwrap();
    let mut two = wire.clone();
    two.extend_from_slice(&wire);
    for quantum in 1..=two.len() {
        let mut reader = FrameReader::new(64);
        assert_eq!(feed(&mut reader, &two, quantum).unwrap(), wire.len());
        assert_eq!(reader.buffer().unwrap().len(), 0);
        assert_eq!(reader.finish().unwrap(), first);
    }
}
#[test]
fn oversized_and_malformed_headers_never_allocate_payload() {
    // Header completion must reject before a body buffer is made available.
    let wire = frame().encode().unwrap();
    let header: [u8; 8] = wire[..8].try_into().unwrap();
    assert_eq!(admitted_length(&header, 64).unwrap(), wire.len());
    let mut short = header;
    short[..4].copy_from_slice(&23u32.to_le_bytes());
    let mut huge = header;
    huge[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    for (bytes, limit) in [(header, 63), (short, 64), (huge, usize::MAX)] {
        let mut reader = FrameReader::new(limit);
        assert!(feed(&mut reader, &bytes, 1).is_err());
        assert_eq!(reader.encoded.capacity(), 0);
        assert!(reader.buffer().is_err());
        assert!(reader.advance(0).is_err());
        assert!(reader.finish().is_err());
    }
}
#[test]
fn eof_rejects_every_truncated_frame() {
    // Empty input and every partial header/body remain incomplete at EOF.
    let wire = frame().encode().unwrap();
    for end in 0..wire.len() {
        let mut reader = FrameReader::new(64);
        assert_eq!(feed(&mut reader, &wire[..end], 3).unwrap(), end);
        assert_eq!(reader.finish(), Err(Error::UnexpectedEnd));
    }
}
#[test]
fn pending_reads_and_invalid_counts_do_not_fabricate_progress() {
    // Zero-byte progress preserves both phases; impossible counts poison them.
    let wire = frame().encode().unwrap();
    for end in [0, 3, 8, 11] {
        let mut reader = FrameReader::new(64);
        feed(&mut reader, &wire[..end], 3).unwrap();
        let filled = reader.filled;
        assert!(!reader.advance(0).unwrap());
        assert_eq!(reader.filled, filled);
        assert_eq!(reader.advance(usize::MAX), Err(Error::InvalidLength));
        assert_eq!(reader.filled, filled);
        assert!(reader.buffer().is_err());
        assert_eq!(reader.finish(), Err(Error::InvalidLength));
    }
}
#[test]
fn completed_bytes_still_require_semantic_and_padding_validation() {
    // Receiving the declared length does not grant authority to malformed frames.
    let wire = frame().encode().unwrap();
    for (offset, byte, error) in [(22, 1, Error::InvalidPadding), (4, 99, Error::InvalidType(99))] {
        let mut bad = wire.clone();
        bad[offset] = byte;
        let mut reader = FrameReader::new(64);
        assert_eq!(feed(&mut reader, &bad, 5).unwrap(), bad.len());
        assert_eq!(reader.finish(), Err(error));
    }
}
#[test]
fn zero_body_and_maximum_wire_admission_include_name_alignment() {
    // Body budgets exclude names/padding but never override the total wire bound.
    for names in [0, 1, 7, 8, 15, 16, 254, 255] {
        let offset = body_offset(names, names);
        for body in [0, MAX_FRAME_LEN - offset] {
            let frame =
                Frame::new(frame_type::HELLO, 0, vec![1; names], vec![2; names], vec![3; body])
                    .unwrap();
            let wire = frame.encode().unwrap();
            let mut reader = FrameReader::new(body);
            assert_eq!(feed(&mut reader, &wire, 4096).unwrap(), wire.len());
            assert_eq!(reader.finish().unwrap(), frame);
        }
    }
}
