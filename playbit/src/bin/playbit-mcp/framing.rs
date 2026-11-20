//! One bounded newline frame. Never drain an over-limit or unterminated stream.
use std::io::{self, BufRead, Read};

pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;
pub const BUFFER_CAPACITY: usize = MAX_MESSAGE + 2; // LF or CRLF delimiter.

pub fn read(input: &mut impl BufRead, buffer: &mut Vec<u8>) -> io::Result<bool> {
    buffer.clear();
    if input.take(BUFFER_CAPACITY as u64).read_until(b'\n', buffer)? == 0 {
        return Ok(false);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
        if buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
    }
    if buffer.len() > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP message exceeds 4 MiB"));
    }
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn fragmented_utf8_crlf_and_eof_preserve_message_boundaries() {
        let mut input = BufReader::with_capacity(1, Cursor::new("日本語\r\n\nlast\r".as_bytes()));
        let mut buffer = Vec::with_capacity(BUFFER_CAPACITY);
        for expected in ["日本語", "", "last\r"] {
            assert!(read(&mut input, &mut buffer).unwrap());
            assert_eq!(buffer, expected.as_bytes());
        }
        assert!(!read(&mut input, &mut buffer).unwrap());
        assert!(buffer.is_empty());
    }
    #[test]
    fn exact_limit_accepts_each_legal_ending_without_growing_the_buffer() {
        for ending in [b"\n".as_slice(), b"\r\n", b""] {
            let mut bytes = vec![b'x'; MAX_MESSAGE];
            bytes.extend_from_slice(ending);
            let mut input = Cursor::new(bytes);
            let mut buffer = Vec::with_capacity(BUFFER_CAPACITY);
            let capacity = buffer.capacity();
            assert!(read(&mut input, &mut buffer).unwrap());
            assert_eq!(buffer.len(), MAX_MESSAGE);
            assert_eq!(buffer.capacity(), capacity);
        }
    }
    #[test]
    fn unending_input_is_rejected_without_draining_or_allocating_more() {
        struct Unending {
            consumed: usize,
        }
        impl Read for Unending {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                unreachable!()
            }
        }
        impl BufRead for Unending {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Ok(&[b'x'; 8192])
            }
            fn consume(&mut self, count: usize) {
                self.consumed += count;
                assert!(self.consumed <= BUFFER_CAPACITY, "read past the frame boundary");
            }
        }
        let mut input = Unending { consumed: 0 };
        let mut buffer = Vec::with_capacity(BUFFER_CAPACITY);
        let capacity = buffer.capacity();
        assert_eq!(read(&mut input, &mut buffer).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(input.consumed, BUFFER_CAPACITY);
        assert_eq!(buffer.capacity(), capacity);
    }
    #[test]
    fn over_limit_final_message_and_following_operation_are_not_consumed_together() {
        for ending in [b"".as_slice(), b"\n{\"method\":\"initialize\"}\n"] {
            let mut bytes = vec![b'x'; MAX_MESSAGE + 1];
            bytes.extend_from_slice(ending);
            let mut input = Cursor::new(bytes);
            let mut buffer = Vec::with_capacity(BUFFER_CAPACITY);
            assert!(read(&mut input, &mut buffer).is_err());
            assert!(input.position() <= BUFFER_CAPACITY as u64);
        }
    }
    #[test]
    #[ignore = "reference-device framing benchmark, never run in CI"]
    fn benchmark_ready_framing() {
        let frame = [b'x'; 4096];
        let mut buffer = Vec::with_capacity(BUFFER_CAPACITY);
        let mut times = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let mut input = Cursor::new(&frame);
            let started = std::time::Instant::now();
            assert!(read(&mut input, &mut buffer).unwrap());
            std::hint::black_box(&buffer);
            times.push(started.elapsed().as_nanos());
        }
        times.sort_unstable();
        playbit::diagnostic!(
            "ready 4 KiB framing n=1000 p50={} p99={} max={} ns",
            times[499],
            times[989],
            times[999]
        );
        assert!(times[989] < 100_000);
    }
}
