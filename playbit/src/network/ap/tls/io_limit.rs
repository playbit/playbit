//! One bounded external socket operation; vectored writes use this same admission.
use std::io::{self, Read, Write};
const BYTES: usize = 16 * 1024;
pub(super) struct Limited<'a, S> {
    stream: &'a mut S,
    used: bool,
}
impl<'a, S> Limited<'a, S> {
    pub fn new(stream: &'a mut S) -> Self {
        Self { stream, used: false }
    }
    fn admit(&mut self, length: usize) -> Option<usize> {
        if self.used {
            return None;
        }
        self.used = true;
        Some(length.min(BYTES))
    }
}
impl<S: Read> Read for Limited<'_, S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let length = self.admit(output.len()).ok_or(io::ErrorKind::WouldBlock)?;
        let count = self.stream.read(&mut output[..length])?;
        assert!(count <= length, "socket exceeded read span");
        Ok(count)
    }
}
impl<S: Write> Write for Limited<'_, S> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let length = self.admit(input.len()).ok_or(io::ErrorKind::WouldBlock)?;
        let count = self.stream.write(&input[..length])?;
        assert!(count <= length, "socket exceeded write span");
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::WouldBlock.into())
    }
}
