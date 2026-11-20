//! One bounded publication on a retained authenticated connection. Payload is
//! borrowed so its caller can retain a committed-change lease through completion.
//! The host owns TLS progress and the whole-operation deadline; drop cancels.
use super::{Connection, StreamRead};
use crate::network::ap::{
    self, Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceRequest},
    receive::FrameReader,
    replication::{self, PublicationAck},
    service::{self, Result},
};
use std::io::{Read, Write};
const REPLY_BYTES: usize = 4096;

pub struct Publication<'a, S: Read + Write> {
    connection: &'a mut Connection<S>,
    stream: Option<u32>,
    operation: &'a str,
    payload: &'a [u8],
    request: Vec<u8>,
    header: Vec<u8>,
    part: usize,
    sent: usize,
    reader: Option<FrameReader>,
    acknowledgment: Option<PublicationAck>,
    continue_now: bool,
}
impl<'a, S: Read + Write> Publication<'a, S> {
    /// Supply an already authenticated connection. Validation and allocation
    /// precede stream admission. A request/payload is never automatically retried.
    pub fn new(
        connection: &'a mut Connection<S>,
        request: &'a ServiceRequest,
        payload: &'a [u8],
    ) -> Result<Self> {
        match request {
            ServiceRequest::PublishReplica { .. } | ServiceRequest::PublishAccount { .. } => {
                replication::validate_delta(payload)?
            }
            _ => return Err("request is not a replica publication".into()),
        }
        let frame = service::request(request)?;
        if frame.body.len() > REPLY_BYTES {
            return Err(ap::Error::FrameTooLarge.into());
        }
        let encoded = frame.encode()?;
        let header = payload_header(payload.len())?;
        let stream = connection.open()?;
        // Stream one is reserved for authentication/enrollment, never data.
        if stream == 1 {
            connection.close();
            return Err("publication requires an authenticated connection".into());
        }
        Ok(Self {
            connection,
            stream: Some(stream),
            operation: request.operation_id(),
            payload,
            request: encoded,
            header,
            part: 0,
            sent: 0,
            reader: Some(FrameReader::new(REPLY_BYTES)),
            acknowledgment: None,
            continue_now: true,
        })
    }
    /// Access for TLS readiness and unrelated streams on this same connection.
    /// Do not close or release this operation's stream through another owner.
    pub fn connection_mut(&mut self) -> &mut Connection<S> {
        self.connection
    }
    /// Combine WebSocket output readiness with the underlying TLS/socket state.
    /// Refresh both after each turn, including writes performed by other streams.
    pub fn wants_write(&self) -> bool {
        self.connection.pending
    }
    pub fn continue_now(&self) -> bool {
        self.continue_now
    }
    /// One connection quantum, one bounded write and one bounded read. Schedule
    /// another turn only for readiness/local work/continue_now. Success requires
    /// the validated reply followed by FIN, with no trailing bytes.
    pub fn advance(&mut self) -> Result<Option<PublicationAck>> {
        let result = self.step();
        if result.is_err() {
            self.retire();
        }
        result
    }
    fn step(&mut self) -> Result<Option<PublicationAck>> {
        let stream = self.stream.ok_or("publication is complete or cancelled")?;
        self.continue_now = self.connection.advance()?.continue_now;
        if self.part < 3 {
            let bytes = match self.part {
                0 => &self.request[..],
                1 => &self.header[..],
                2 => self.payload,
                _ => unreachable!(),
            };
            assert!(self.sent <= bytes.len());
            let count = self.connection.write(stream, &bytes[self.sent..])?;
            self.sent += count;
            self.continue_now |= count != 0;
            if self.sent == bytes.len() {
                self.part += 1;
                self.sent = 0;
                self.continue_now = true;
                if self.part == 3 {
                    self.connection.finish(stream)?;
                }
            }
        }
        self.receive(stream)
    }
    fn receive(&mut self, stream: u32) -> Result<Option<PublicationAck>> {
        let mut trailing = [0];
        let output = match &mut self.reader {
            Some(reader) => reader.buffer()?,
            None => &mut trailing,
        };
        match self.connection.read(stream, output)? {
            StreamRead::Pending => Ok(None),
            StreamRead::Finished => {
                let acknowledgment = self.acknowledgment.take().ok_or(ap::Error::UnexpectedEnd)?;
                self.retire();
                Ok(Some(acknowledgment))
            }
            StreamRead::Data(count) => {
                self.continue_now = true;
                let reader = self.reader.as_mut().ok_or(ap::Error::InvalidLength)?;
                if reader.advance(count)? {
                    let frame = self.reader.take().expect("completed reader").finish()?;
                    let response = service::response(frame, self.operation)?;
                    if self.part != 3 {
                        return Err(ap::Error::UnexpectedEnd.into());
                    }
                    let ack: PublicationAck = serde_json::from_value(
                        response.data.ok_or("missing durable publication acknowledgment")?,
                    )?;
                    ack.validate()?;
                    self.acknowledgment = Some(ack);
                }
                Ok(None)
            }
        }
    }
    fn retire(&mut self) {
        if let Some(stream) = self.stream.take()
            && self.connection.release(stream).is_err()
        {
            self.connection.close();
        }
        self.continue_now = false;
    }
}
impl<S: Read + Write> Drop for Publication<'_, S> {
    fn drop(&mut self) {
        self.retire();
    }
}
fn payload_header(body_bytes: usize) -> Result<Vec<u8>> {
    if body_bytes == 0 || body_bytes > replication::MAX_DELTA_BYTES {
        return Err(ap::Error::FrameTooLarge.into());
    }
    // Use the shared encoder for type, names and padding; extend only its length.
    // The admitted 64-KiB body and fixed service header fit the AP/u32 ceilings.
    let mut header = Frame::new(frame_type::DIRECT, 0, [], SERVICE_DESTINATION, [])?.encode()?;
    let length = header.len() + body_bytes;
    assert!(length <= ap::MAX_FRAME_LEN && length <= u32::MAX as usize);
    header[..4].copy_from_slice(&(length as u32).to_le_bytes());
    Ok(header)
}
