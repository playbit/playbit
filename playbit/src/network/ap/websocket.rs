//! Single-owner AP WebSocket stream mapping for the bounded native client.
//!
//! Connection credit bounds input across 1,024 lazily allocated stream rings
//! to 2 MiB. Stream credit separately bounds each ring to 32 KiB. There is no outgoing queue: the
//! driver supplies one reusable packet buffer and must send each returned packet
//! in order before producing another, or close on failure. Socket readiness,
//! verified TLS, WebSocket framing/ping and AP authentication belong to the driver.
//! All methods run bounded work without waiting; copying bytes is the data plane.

#[cfg(all(feature = "ap-websocket", not(target_arch = "wasm32")))]
pub mod transport;

use std::{collections::VecDeque, fmt};

pub const STREAM_LIMIT: usize = 1024;
pub const CONNECTION_WINDOW: usize = 2 * 1024 * 1024;
pub const WINDOW: usize = 32 * 1024;
pub const CHUNK: usize = 16 * 1024;
pub const PACKET_MAX: usize = 5 + CHUNK;
const OPEN: u8 = 0;
const DATA: u8 = 1;
const FIN: u8 = 2;
const STOP: u8 = 3;
const CREDIT: u8 = 4;
const _: () = assert!(CHUNK <= WINDOW && WINDOW <= u32::MAX as usize);
const _: () = assert!(STREAM_LIMIT * WINDOW >= CONNECTION_WINDOW);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Closed,
    StreamLimit,
    IdExhausted,
    UnknownStream,
    WriteClosed,
    ReadClosed,
    OutputTooSmall,
    MalformedPacket,
    FlowControl,
    AfterFin,
}
impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AP WebSocket: {self:?}")
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Read {
    Data(usize),
    Pending,
    Finished,
}

struct Stream {
    id: u32,
    incoming: VecDeque<u8>,
    receive_credit: usize,
    return_credit: usize,
    send_credit: usize,
    open: bool,
    stop: bool,
    released: bool,
    received_fin: bool,
    finish: bool,
    sent_fin: bool,
}
impl Stream {
    fn new(id: u32) -> Self {
        Self {
            id,
            incoming: VecDeque::new(),
            receive_credit: WINDOW,
            return_credit: 0,
            send_credit: WINDOW,
            open: true,
            stop: false,
            released: false,
            received_fin: false,
            finish: false,
            sent_fin: false,
        }
    }

    fn receive(&mut self, kind: u8, body: &[u8]) -> Result<()> {
        match kind {
            DATA => {
                if self.received_fin {
                    return Err(Error::AfterFin);
                }
                self.receive_credit = debit(self.receive_credit, body.len())?;
                if !self.released {
                    assert!(self.incoming.len() + body.len() <= WINDOW);
                    self.incoming.extend(body);
                    assert!(self.incoming.capacity() <= WINDOW);
                }
            }
            FIN => {
                if self.received_fin {
                    return Err(Error::AfterFin);
                }
                self.received_fin = true;
            }
            STOP => self.finish = true,
            CREDIT => {
                let count = u32::from_le_bytes(body.try_into().expect("validated credit width"));
                self.send_credit = credit(self.send_credit, count as usize)?;
            }
            _ => unreachable!("receive validated the packet kind"),
        }
        Ok(())
    }

    fn next_packet(&mut self, output: &mut [u8]) -> usize {
        if self.open {
            self.open = false;
            packet(output, OPEN, self.id, &[])
        } else if self.stop {
            self.stop = false;
            packet(output, STOP, self.id, &[])
        } else if self.return_credit != 0 {
            let count = self.return_credit;
            self.receive_credit = credit(self.receive_credit, count)
                .expect("only consumed receive bytes return credit");
            self.return_credit = 0;
            packet(output, CREDIT, self.id, &(count as u32).to_le_bytes())
        } else if self.finish && !self.sent_fin {
            self.sent_fin = true;
            packet(output, FIN, self.id, &[])
        } else {
            0
        }
    }

    fn retired(&self) -> bool {
        self.released && self.received_fin && self.sent_fin && !self.stop && !self.open
    }
}

/// Driver-owned state; stream IDs are valid only within this connection.
/// A released slot remains occupied until both FINs have crossed. A peer that
/// never completes cancellation therefore causes explicit admission pressure;
/// the driver owns the connection liveness/cancellation deadline.
pub struct Client {
    streams: Box<[Option<Stream>]>,
    receive_credit: usize,
    send_credit: usize,
    return_credit: usize,
    last_id: u32,
    next_control: usize,
    closed: bool,
}
const _: () = assert!(STREAM_LIMIT * std::mem::size_of::<Option<Stream>>() <= 128 * 1024);
impl Default for Client {
    fn default() -> Self {
        Self {
            streams: (0..STREAM_LIMIT).map(|_| None).collect(),
            receive_credit: CONNECTION_WINDOW,
            send_credit: CONNECTION_WINDOW,
            return_credit: 0,
            last_id: 0,
            next_control: 0,
            closed: false,
        }
    }
}

impl Client {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate one receive ring. Drain OPEN with next_packet before writing.
    pub fn open(&mut self) -> Result<u32> {
        if self.closed {
            return Err(Error::Closed);
        }
        let slot = self.streams.iter_mut().find(|slot| slot.is_none()).ok_or(Error::StreamLimit)?;
        let id = self.last_id.checked_add(1).ok_or(Error::IdExhausted)?;
        *slot = Some(Stream::new(id));
        self.last_id = id;
        Ok(id)
    }

    /// Accept one complete binary WebSocket message. Any protocol error is
    /// terminal and drops all streams, so a caller cannot resume poisoned state.
    // fuzz: ap_codec
    pub fn receive(&mut self, bytes: &[u8]) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        let result = self.receive_packet(bytes);
        if result.is_err() {
            self.close();
        }
        result
    }

    fn receive_packet(&mut self, bytes: &[u8]) -> Result<()> {
        if !(5..=PACKET_MAX).contains(&bytes.len()) {
            return Err(Error::MalformedPacket);
        }
        let kind = bytes[0];
        let id = u32::from_le_bytes(bytes[1..5].try_into().expect("validated header width"));
        let body = &bytes[5..];
        let shape = match kind {
            DATA => !body.is_empty(),
            CREDIT => body.len() == 4,
            FIN | STOP => body.is_empty(),
            _ => false, // Only the client opens streams.
        };
        if shape && id == 0 && kind == CREDIT {
            let count =
                u32::from_le_bytes(body.try_into().expect("validated credit width")) as usize;
            if count == 0 || count > CONNECTION_WINDOW - self.send_credit {
                return Err(Error::FlowControl);
            }
            self.send_credit += count;
            return Ok(());
        }
        if !shape || id == 0 || id > self.last_id {
            return Err(Error::MalformedPacket);
        }
        if kind == DATA {
            if body.len() > self.receive_credit {
                return Err(Error::FlowControl);
            }
            self.receive_credit -= body.len();
        }
        let Some(index) = self.streams.iter().position(|s| s.as_ref().is_some_and(|s| s.id == id))
        else {
            // Ordered packets may cross retirement. Match the service and web
            // client: validate shape first, then ignore an already retired ID.
            if kind == DATA {
                self.return_credit += body.len();
            }
            return Ok(());
        };
        let stream = self.streams[index].as_mut().expect("located stream");
        if stream.open {
            return Err(Error::MalformedPacket);
        }
        stream.receive(kind, body)?;
        if kind == DATA && stream.released {
            self.return_credit += body.len();
        }
        self.retire(index);
        Ok(())
    }

    /// Emit at most one control packet, rotating among streams. Zero means no
    /// ready control. The entire returned prefix must be sent before reentry.
    pub fn next_packet(&mut self, output: &mut [u8]) -> Result<usize> {
        self.check_output(output)?;
        if self.return_credit != 0 {
            let count = std::mem::take(&mut self.return_credit);
            self.receive_credit += count;
            assert!(self.receive_credit <= CONNECTION_WINDOW);
            return Ok(packet(output, CREDIT, 0, &(count as u32).to_le_bytes()));
        }
        for offset in 0..STREAM_LIMIT {
            let index = (self.next_control + offset) % STREAM_LIMIT;
            let Some(stream) = self.streams[index].as_mut() else { continue };
            let count = stream.next_packet(output);
            if count != 0 {
                self.next_control = (index + 1) % STREAM_LIMIT;
                self.retire(index);
                return Ok(count);
            }
        }
        Ok(0)
    }

    /// Emit one DATA packet, consuming returned_length - 5 input bytes. Zero
    /// means empty input, pending OPEN or exhausted credit; retry only on the
    /// corresponding driver readiness. There is no retained copy of input.
    pub fn write(&mut self, id: u32, bytes: &[u8], output: &mut [u8]) -> Result<usize> {
        self.check_output(output)?;
        let connection_credit = self.send_credit;
        let stream = self.stream(id)?;
        if stream.finish || stream.released {
            return Err(Error::WriteClosed);
        }
        if stream.open {
            return Ok(0);
        }
        let count = bytes.len().min(CHUNK).min(stream.send_credit).min(connection_credit);
        if count == 0 {
            return Ok(0);
        }
        stream.send_credit = debit(stream.send_credit, count).expect("count is within credit");
        self.send_credit -= count;
        Ok(packet(output, DATA, id, &bytes[..count]))
    }

    /// Consume up to one window. Credit becomes ready only for consumed bytes.
    /// Empty output is a zero-byte read, never EOF or a readiness signal.
    pub fn read(&mut self, id: u32, output: &mut [u8]) -> Result<Read> {
        let stream = self.stream(id)?;
        if stream.released {
            return Err(Error::ReadClosed);
        }
        if output.is_empty() {
            return Ok(Read::Data(0));
        }
        let count = output.len().min(stream.incoming.len());
        if count == 0 {
            return Ok(if stream.received_fin { Read::Finished } else { Read::Pending });
        }
        let (first, second) = stream.incoming.as_slices();
        let first_count = count.min(first.len());
        output[..first_count].copy_from_slice(&first[..first_count]);
        output[first_count..count].copy_from_slice(&second[..count - first_count]);
        stream.incoming.drain(..count);
        stream.return_credit += count;
        assert!(stream.return_credit + stream.receive_credit + stream.incoming.len() == WINDOW);
        self.return_credit += count;
        Ok(Read::Data(count))
    }

    /// Half-close the writer after all caller-owned bytes have been emitted.
    pub fn finish(&mut self, id: u32) -> Result<()> {
        self.stream(id)?.finish = true;
        Ok(())
    }

    /// Drop both application halves. Unconsumed bytes are discarded without
    /// granting more credit; STOP and FIN are emitted by next_packet.
    pub fn release(&mut self, id: u32) -> Result<()> {
        let stream = self.stream(id)?;
        stream.released = true;
        let discarded = stream.incoming.len();
        stream.incoming.clear();
        stream.return_credit = 0;
        stream.stop |= !stream.received_fin;
        stream.finish = true;
        self.return_credit += discarded;
        for index in 0..STREAM_LIMIT {
            self.retire(index);
        }
        Ok(())
    }

    /// Terminal socket/TLS/cancellation failure. Reconnect needs a fresh client
    /// and fresh authenticated AP streams, never replay of buffered operations.
    pub fn close(&mut self) {
        self.closed = true;
        self.streams.fill_with(|| None);
    }

    fn stream(&mut self, id: u32) -> Result<&mut Stream> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.streams.iter_mut().flatten().find(|stream| stream.id == id).ok_or(Error::UnknownStream)
    }

    fn check_output(&self, output: &[u8]) -> Result<()> {
        if self.closed {
            Err(Error::Closed)
        } else if output.len() < PACKET_MAX {
            Err(Error::OutputTooSmall)
        } else {
            Ok(())
        }
    }

    fn retire(&mut self, index: usize) {
        if self.streams[index].as_ref().is_some_and(Stream::retired) {
            self.streams[index] = None;
        }
    }
}

fn packet(output: &mut [u8], kind: u8, id: u32, body: &[u8]) -> usize {
    assert!(output.len() >= PACKET_MAX);
    assert!(body.len() <= CHUNK);
    output[0] = kind;
    output[1..5].copy_from_slice(&id.to_le_bytes());
    output[5..5 + body.len()].copy_from_slice(body);
    5 + body.len()
}

fn debit(available: usize, count: usize) -> Result<usize> {
    assert!(available <= WINDOW);
    if count == 0 || count > available {
        return Err(Error::FlowControl);
    }
    Ok(available - count)
}

fn credit(available: usize, count: usize) -> Result<usize> {
    assert!(available <= WINDOW);
    if count == 0 || count > WINDOW - available {
        return Err(Error::FlowControl);
    }
    Ok(available + count)
}

#[cfg(test)]
mod tests;
