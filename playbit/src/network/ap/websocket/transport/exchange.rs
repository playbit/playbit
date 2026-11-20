//! Bounded first-stream I/O shared by saved-token and enrollment exchanges.
//! The typed owner validates replies, chooses when to send FIN, and drops this
//! connection on any error. No credential policy or persistence lives here.
use super::{Connection, Error, StreamRead};
use crate::network::ap::{self, ClientError, Frame, receive::FrameReader};
use std::io::{Read, Write};
pub(super) const BODY_MAX: usize = 4096;
#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error(transparent)]
    Transport(#[from] Error),
    #[error(transparent)]
    Frame(#[from] ap::Error),
    #[error(transparent)]
    Identity(#[from] ClientError),
    #[error("AP authentication requires a fresh connection")]
    UsedConnection,
}
pub(super) type Result<T> = std::result::Result<T, AuthenticationError>;
pub(super) enum Reply {
    Frame(Frame),
    End,
}
pub(super) struct Exchange<S: Read + Write> {
    pub connection: Connection<S>,
    request: Vec<u8>,
    sent: usize,
    reader: Option<FrameReader>,
    pub continue_now: bool,
}
impl<S: Read + Write> Exchange<S> {
    pub fn new(mut connection: Connection<S>, request: Frame) -> Result<Self> {
        if request.body.len() > BODY_MAX {
            return Err(ap::Error::FrameTooLarge.into());
        }
        let request = request.encode()?;
        if connection.open()? != 1 {
            return Err(AuthenticationError::UsedConnection);
        }
        Ok(Self {
            connection,
            request,
            sent: 0,
            reader: Some(FrameReader::new(BODY_MAX)),
            continue_now: true,
        })
    }
    pub fn finish(&mut self) -> Result<()> {
        self.connection.finish(1)?;
        self.continue_now = true;
        Ok(())
    }
    pub fn release(mut self) -> Result<Connection<S>> {
        self.connection.release(1)?;
        Ok(self.connection)
    }
    pub fn advance(&mut self) -> Result<Option<Reply>> {
        self.continue_now = self.connection.advance()?.continue_now;
        assert!(self.sent <= self.request.len());
        if self.sent < self.request.len() {
            let count = self.connection.write(1, &self.request[self.sent..])?;
            self.sent += count;
            self.continue_now |= count != 0;
        }
        self.receive()
    }
    fn receive(&mut self) -> Result<Option<Reply>> {
        // A nonempty scratch span observes EOF or rejects trailing stream bytes.
        let mut trailing = [0];
        let output = match &mut self.reader {
            Some(reader) => reader.buffer()?,
            None => &mut trailing,
        };
        match self.connection.read(1, output)? {
            StreamRead::Pending => Ok(None),
            StreamRead::Finished => Ok(Some(Reply::End)),
            StreamRead::Data(count) => {
                let reader = self.reader.as_mut().ok_or(ap::Error::InvalidLength)?;
                self.continue_now = true;
                if !reader.advance(count)? {
                    return Ok(None);
                }
                if self.sent != self.request.len() {
                    return Err(ap::Error::UnexpectedEnd.into());
                }
                let reader = self.reader.take().expect("reader consumed only on completion");
                let (reply, _) = ap::timing::auth::decode(reader.finish()?)?;
                Ok(Some(Reply::Frame(reply)))
            }
        }
    }
}
