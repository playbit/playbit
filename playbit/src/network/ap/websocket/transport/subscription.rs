//! One bounded subscription on a retained authenticated connection. This owns
//! framing and stream lifetime, not initialization, durable receipt or continuity.
//! Hosts apply events before advancing Cursor, and own readiness and deadlines.
use super::{Connection, StreamRead};
use crate::network::ap::{
    self, Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceRequest},
    receive::FrameReader,
    replication::{self, ReplicaEvent, ReplicaEventKind, SubscriptionAck},
    service::{self, Result},
};
use std::io::{Read, Write};
const REPLY_BYTES: usize = 4096;

#[derive(Debug)]
pub enum SubscriptionItem {
    Acknowledged(SubscriptionAck),
    /// One validated envelope. Borrow its payload with ReplicaEvent::decode;
    /// no kvdb parsing, authorization or durable application is implied.
    Event(Vec<u8>),
}
pub struct Subscription<'a, S: Read + Write> {
    connection: &'a mut Connection<S>,
    stream: Option<u32>,
    operation: &'a str,
    outgoing: Vec<u8>,
    sent: usize,
    reader: Option<FrameReader>,
    event_bytes: usize,
    acknowledged: bool,
    continue_now: bool,
}
impl<'a, S: Read + Write> Subscription<'a, S> {
    /// The first response is bounded to 4 KiB. Later bodies use event_bytes,
    /// including the replica envelope header, admitted before allocation.
    pub fn new(
        connection: &'a mut Connection<S>,
        request: &'a ServiceRequest,
        event_bytes: usize,
    ) -> Result<Self> {
        if !matches!(request, ServiceRequest::SubscribeReplica { .. }) {
            return Err("request is not a replica subscription".into());
        }
        if !(replication::EVENT_HEADER_BYTES..=replication::MAX_EVENT_BYTES).contains(&event_bytes)
        {
            return Err(ap::Error::FrameTooLarge.into());
        }
        let request_frame = service::request(request)?;
        if request_frame.body.len() > REPLY_BYTES {
            return Err(ap::Error::FrameTooLarge.into());
        }
        let outgoing = request_frame.encode()?;
        let stream = connection.open()?;
        if stream == 1 {
            connection.close();
            return Err("subscription requires an authenticated connection".into());
        }
        Ok(Self {
            connection,
            stream: Some(stream),
            operation: request.operation_id(),
            outgoing,
            sent: 0,
            reader: Some(FrameReader::new(REPLY_BYTES)),
            event_bytes,
            acknowledged: false,
            continue_now: true,
        })
    }
    /// TLS progress and unrelated operations remain on this same connection.
    /// Do not release this subscription's stream through another owner.
    pub fn connection_mut(&mut self) -> &mut Connection<S> {
        self.connection
    }
    pub fn continue_now(&self) -> bool {
        self.continue_now
    }
    pub fn wants_write(&self) -> bool {
        self.connection.pending
    }
    /// Queue one explicit ordering barrier after initialization or recovery.
    /// This is never a timer-driven freshness request. Only one unsent control
    /// fits here; the host tracks its ordering boundary with Cursor and durable
    /// application, including duplicate CaughtUp envelopes.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.stream.is_none() || !self.acknowledged || !self.outgoing.is_empty() {
            return Err("subscription checkpoint is not ready".into());
        }
        self.outgoing =
            Frame::new(frame_type::DIRECT, 0, [], SERVICE_DESTINATION, replication::CHECKPOINT)?
                .encode()?;
        self.sent = 0;
        self.continue_now = true;
        Ok(())
    }
    /// One connection quantum, one write and one read; yields at most one item.
    /// Stop advancing while a consumer owns a delivery to bound aggregate memory.
    /// EOF, malformed/oversized frames and Gap/Revoked retire this stream. Other
    /// streams remain usable when the connection itself has not failed.
    pub fn advance(&mut self) -> Result<Option<SubscriptionItem>> {
        let result = self.step();
        if result.is_err() {
            self.retire();
        }
        result
    }
    fn step(&mut self) -> Result<Option<SubscriptionItem>> {
        let stream = self.stream.ok_or("subscription is complete or cancelled")?;
        self.continue_now = self.connection.advance()?.continue_now;
        if !self.outgoing.is_empty() {
            let count = self.connection.write(stream, &self.outgoing[self.sent..])?;
            self.sent += count;
            self.continue_now |= count != 0;
            if self.sent == self.outgoing.len() {
                self.outgoing = Vec::new();
                self.sent = 0;
            }
        }
        let reader = self.reader.as_mut().expect("active subscription owns its reader");
        match self.connection.read(stream, reader.buffer()?)? {
            StreamRead::Pending => Ok(None),
            StreamRead::Finished => Err(ap::Error::UnexpectedEnd.into()),
            StreamRead::Data(count) => {
                self.continue_now = true;
                if !reader.advance(count)? {
                    return Ok(None);
                }
                let frame = self.reader.take().expect("completed reader").finish()?;
                let item = self.received(frame)?;
                if self.stream.is_some() {
                    self.reader = Some(FrameReader::new(self.event_bytes));
                }
                Ok(Some(item))
            }
        }
    }
    fn received(&mut self, frame: Frame) -> Result<SubscriptionItem> {
        if !self.acknowledged {
            if !self.outgoing.is_empty() {
                return Err("subscription acknowledgment precedes request".into());
            }
            let response = service::response(frame, self.operation)?;
            let ack: SubscriptionAck = serde_json::from_value(
                response.data.ok_or("missing subscription acknowledgment")?,
            )?;
            ack.validate()?;
            self.acknowledged = true;
            return Ok(SubscriptionItem::Acknowledged(ack));
        }
        let bytes = service::data(frame)?;
        let event = ReplicaEvent::decode(&bytes)?;
        if matches!(event.kind, ReplicaEventKind::Gap | ReplicaEventKind::Terminal { .. }) {
            self.retire();
        }
        Ok(SubscriptionItem::Event(bytes))
    }
    fn retire(&mut self) {
        self.reader = None;
        self.outgoing = Vec::new();
        if let Some(stream) = self.stream.take()
            && self.connection.release(stream).is_err()
        {
            self.connection.close();
        }
        self.continue_now = false;
    }
}
impl<S: Read + Write> Drop for Subscription<'_, S> {
    fn drop(&mut self) {
        self.retire();
    }
}
