//! Ephemeral channels scoped to the current session. Persistent data belongs in its root volume.
use super::{Completion, Context, Error, Event, EventKind, OperationHandle, status};
use crate::abi::{
    dispatch::{pack_handle, unpack_handle},
    session as invocation,
    syscall::Opcode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session {
    pub id: [u8; 32],
    generation: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct Channel(pub u64);
#[derive(Clone, Copy, Debug)]
pub struct ChannelMessage<'a> {
    pub channel: Channel,
    pub sender: super::Participant,
    pub message_id: [u8; 16],
    pub data: &'a [u8],
}
impl Context<'_> {
    pub fn session(&mut self) -> Result<Session, Error> {
        let identity = self.identity()?;
        // SAFETY: the operation has no pointer arguments and borrows this callback.
        let generation = unsafe { invocation::call(self.raw.as_ptr(), Opcode::Session, [0; 6]) };
        if generation < 0 {
            status(generation)?;
        }
        let generation = u32::try_from(generation).map_err(|_| Error::Internal)?;
        if generation == 0 {
            return Err(Error::Internal);
        }
        Ok(Session { id: identity.session_id, generation })
    }
    pub fn take_channel(&mut self, operation: OperationHandle) -> Result<Completion<()>, Error> {
        // SAFETY: operation handles are scalar, generation-scoped capabilities.
        let result = unsafe {
            invocation::call(self.raw.as_ptr(), Opcode::ChannelTake, [operation.0, 0, 0, 0, 0, 0])
        };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        Ok(Completion::Ready(()))
    }
}
impl Session {
    pub fn channel(&self, context: &mut Context<'_>, name: &str) -> Result<Channel, Error> {
        if context.session()?.generation != self.generation {
            return Err(Error::StaleHandle);
        }
        if name.is_empty() || name.len() > 223 {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            invocation::call(
                context.raw.as_ptr(),
                Opcode::ChannelOpen,
                [name.as_ptr() as usize as u64, name.len() as u64, 0, 0, 0, 0],
            )
        };
        Ok(Channel(super::pending::handle_result(result)?))
    }
}
impl Channel {
    pub const fn new(generation: u32, slot: u32) -> Self {
        Self(pack_handle(generation, slot))
    }
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }
    fn begin(
        self,
        context: &mut Context<'_>,
        action: u32,
        data: &[u8],
    ) -> Result<OperationHandle, Error> {
        if data.len() > 32 * 1024 {
            return Err(Error::LimitExceeded);
        }
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            invocation::call(
                context.raw.as_ptr(),
                Opcode::ChannelBegin,
                [self.0, u64::from(action), data.as_ptr() as usize as u64, data.len() as u64, 0, 0],
            )
        };
        super::pending::admitted(result)
    }
    pub fn publish(self, context: &mut Context<'_>, data: &[u8]) -> Result<OperationHandle, Error> {
        self.begin(context, 1, data)
    }
    pub fn subscribe(self, context: &mut Context<'_>) -> Result<OperationHandle, Error> {
        self.begin(context, 2, &[])
    }
    pub fn unsubscribe(self, context: &mut Context<'_>) -> Result<OperationHandle, Error> {
        self.begin(context, 3, &[])
    }
    pub fn close(self, context: &mut Context<'_>) -> Result<OperationHandle, Error> {
        self.begin(context, 4, &[])
    }
}
impl<'a> Event<'a> {
    pub fn channel_message(&self) -> Result<ChannelMessage<'a>, Error> {
        if self.kind != EventKind::ChannelMessage || self.target != 0 || self.data.len() < 104 {
            return Err(Error::InvalidArgument);
        }
        let handle = u64::from_le_bytes(self.data[..8].try_into().expect("fixed handle"));
        let (generation, slot) = unpack_handle(handle);
        if generation == 0 || generation > i32::MAX as u32 || slot == 0 {
            return Err(Error::StaleHandle);
        }
        Ok(ChannelMessage {
            channel: Channel::new(generation, slot),
            sender: super::Participant {
                size: core::mem::size_of::<super::Participant>() as u32,
                written: 0,
                account_id: self.data[8..24].try_into().expect("account"),
                device_id: self.data[24..40].try_into().expect("device"),
                instance_id: self.data[40..56].try_into().expect("instance"),
                build_id: self.data[56..88].try_into().expect("build"),
            },
            message_id: self.data[88..104].try_into().expect("message"),
            data: &self.data[104..],
        })
    }
}

impl Event<'_> {
    /// A terminal subscription failure. Reconnect restores desired subscriptions;
    /// LimitExceeded reports dropped live messages and Unavailable a disconnect.
    pub fn channel_status(&self) -> Result<Error, Error> {
        if self.kind != EventKind::ChannelStatus || self.target != 0 || self.data.len() != 4 {
            return Err(Error::InvalidArgument);
        }
        Error::try_from(i32::from_le_bytes(self.data.try_into().expect("status")))
            .map_err(|_| Error::InvalidArgument)
    }
}
