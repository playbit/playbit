//! Byte/call admission below the WebSocket parser, including empty fragments.
use std::io::{self, Read, Write};
pub(super) const BYTES: usize = 32 * 1024;
const CALLS: usize = 32;
const HANDSHAKE_BYTES: usize = 8 * 1024;

pub(super) struct Bounded<S> {
    pub stream: S,
    bytes: usize,
    calls: usize,
    handshake: Option<usize>,
    pub yielded: bool,
    pub write_blocked: bool,
}
impl<S> Bounded<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            bytes: BYTES,
            calls: CALLS,
            handshake: Some(HANDSHAKE_BYTES),
            yielded: false,
            write_blocked: false,
        }
    }
    pub fn begin(&mut self) {
        self.bytes = BYTES;
        self.calls = CALLS;
        self.yielded = false;
        self.write_blocked = false;
    }
    pub fn upgraded(&mut self) {
        self.handshake = None;
        self.begin();
    }
    fn admit(&mut self) -> bool {
        if self.calls == 0 || self.bytes == 0 {
            self.yielded = true;
            return false;
        }
        self.calls -= 1;
        true
    }
}
impl<S: Read> Read for Bounded<S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.handshake == Some(0) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "AP upgrade exceeds 8 KiB"));
        }
        if !self.admit() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let length = output.len().min(self.bytes).min(self.handshake.unwrap_or(usize::MAX));
        let count = self.stream.read(&mut output[..length])?;
        assert!(count <= length, "Read implementation exceeded its output span");
        self.bytes -= count;
        if let Some(left) = &mut self.handshake {
            *left -= count;
        }
        Ok(count)
    }
}
impl<S: Write> Write for Bounded<S> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        if !self.admit() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let length = input.len().min(self.bytes);
        let result = self.stream.write(&input[..length]);
        self.write_blocked |= result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock);
        let count = result?;
        assert!(count <= length, "Write implementation exceeded its input span");
        self.bytes -= count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.admit() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let result = self.stream.flush();
        self.write_blocked |= result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock);
        result
    }
}
