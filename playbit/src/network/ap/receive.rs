//! Incremental AP frame reception without a transport or executor dependency.
//! One reader belongs to one stream and consumes exactly one frame. Only eight
//! header bytes are retained before length admission; afterward one wire buffer
//! is allocated. Completion uses the ordinary Frame decoder (temporarily holding
//! wire and decoded bytes). Hosts own deadlines, scheduling and aggregate limits.
use super::{Error, Frame, HEADER_LEN, Header, Result, body_offset};

pub struct FrameReader {
    header: [u8; HEADER_LEN],
    encoded: Vec<u8>,
    filled: usize,
    max_body: usize,
    error: Option<Error>,
}
impl FrameReader {
    /// `max_body` is this operation's bound, additionally capped by MAX_FRAME_LEN.
    /// No payload allocation or transport operation occurs during construction.
    pub fn new(max_body: usize) -> Self {
        Self { header: [0; HEADER_LEN], encoded: Vec::new(), filled: 0, max_body, error: None }
    }
    /// Fill only this span, then call advance with the number actually received.
    /// Pending I/O preserves this reader. EOF requires finish, which rejects any
    /// partial frame; a failed reader must be dropped with its stream operation.
    pub fn buffer(&mut self) -> Result<&mut [u8]> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if self.encoded.is_empty() {
            Ok(&mut self.header[self.filled..])
        } else {
            Ok(&mut self.encoded[self.filled..])
        }
    }
    /// Record progress, returning true only when the complete wire frame arrived.
    /// This does no socket I/O or looping; callers bound each read's work quantum.
    // fuzz: ap_codec
    pub fn advance(&mut self, count: usize) -> Result<bool> {
        if count > self.buffer()?.len() {
            self.error = Some(Error::InvalidLength);
            return Err(Error::InvalidLength);
        }
        self.filled += count;
        if self.encoded.is_empty() && self.filled == HEADER_LEN {
            let length = match admitted_length(&self.header, self.max_body) {
                Ok(length) => length,
                Err(error) => {
                    self.error = Some(error.clone());
                    return Err(error);
                }
            };
            self.encoded = vec![0; length];
            self.encoded[..HEADER_LEN].copy_from_slice(&self.header);
        }
        Ok(!self.encoded.is_empty() && self.filled == self.encoded.len())
    }
    /// Consume this operation's reader. Padding, names and type use the shared
    /// decoder; neither trailing stream bytes nor another frame are consumed.
    pub fn finish(self) -> Result<Frame> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.encoded.is_empty() || self.filled != self.encoded.len() {
            return Err(Error::UnexpectedEnd);
        }
        Frame::decode(&self.encoded)
    }
}

fn admitted_length(header: &[u8; HEADER_LEN], max_body: usize) -> Result<usize> {
    let decoded = Header::decode(header)?;
    let offset = body_offset(decoded.source_len, decoded.destination_len);
    let body_len = decoded.frame_len.checked_sub(offset).ok_or(Error::InvalidHeader)?;
    if body_len > max_body {
        return Err(Error::FrameTooLarge);
    }
    Ok(decoded.frame_len)
}

#[cfg(test)]
#[path = "receive/tests.rs"]
mod tests;
