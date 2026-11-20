//! Allocation-free metadata views; every returned reference borrows caller storage.
use super::{Context, Error, PollError, buffer, range, status};
use crate::abi::{data::Range, environment as ffi};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    Host(Error),
    BufferTooSmall { records: usize, bytes: usize },
}
impl From<Error> for MetadataError {
    fn from(error: Error) -> Self {
        Self::Host(error)
    }
}
pub struct Capabilities<'a> {
    names: &'a [Range],
    bytes: &'a [u8],
}
impl<'a> Capabilities<'a> {
    pub fn names(&self) -> impl ExactSizeIterator<Item = &'a str> + '_ {
        self.names.iter().map(|r| text(self.bytes, *r).expect("validated capability name"))
    }
}
pub struct Arguments<'a> {
    arguments: &'a [ffi::Argument],
    bytes: &'a [u8],
}
impl<'a> Arguments<'a> {
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&'a str, &'a str)> + '_ {
        self.arguments.iter().map(|a| {
            (
                text(self.bytes, a.name()).expect("validated argument name"),
                text(self.bytes, a.value()).expect("validated argument value"),
            )
        })
    }
}
fn text(bytes: &[u8], r: Range) -> Result<&str, Error> {
    core::str::from_utf8(range(bytes, r)?).map_err(|_| Error::Internal)
}
impl Context<'_> {
    /// Identify the current host at runtime, including for one portable Wasm build.
    pub fn host_platform(&mut self) -> Result<ffi::HostPlatform, Error> {
        // SAFETY: the callback context is live; no caller storage is borrowed.
        let value = unsafe { ffi::pb_host_platform(self.raw.as_ptr()) };
        ffi::HostPlatform::try_from(i32::try_from(value).map_err(|_| Error::Internal)?)
            .map_err(|value| Error::try_from(value).unwrap_or(Error::Internal))
    }

    /// Read a fixed per-instance ceiling. Unknown kinds or runtimes without this
    /// operation return Unsupported; zero is never a successful limit value.
    pub fn host_limit(&mut self, limit: ffi::HostLimit) -> Result<u32, Error> {
        // SAFETY: the callback context is live and the remaining argument is scalar.
        let value = unsafe { ffi::pb_host_limit(self.raw.as_ptr(), limit as u32) };
        if value > 0 {
            Ok(value as u32)
        } else {
            Err(Error::try_from(value).unwrap_or(Error::Internal))
        }
    }

    /// Fill a buffer with cryptographically secure randomness from the host.
    /// At most 64 KiB per call. Discard the buffer on any error.
    pub fn random_fill(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
        if bytes.len() > crate::abi::syscall::MAX_RANDOM_BYTES {
            return Err(Error::LimitExceeded);
        }
        // SAFETY: the live callback lends the exclusive output only for this call.
        status(unsafe {
            crate::abi::operations::pb_random_fill(
                self.raw.as_ptr(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
            )
        })
    }

    pub fn capabilities<'a>(
        &mut self,
        names: &'a mut [Range],
        bytes: &'a mut [u8],
    ) -> Result<Capabilities<'a>, MetadataError> {
        let mut output = ffi::Capabilities::new(buffer(names)?, buffer(bytes)?);
        // SAFETY: descriptor and both arrays are live exclusive disjoint borrows.
        let result = unsafe { ffi::pb_capabilities(self.raw.as_ptr(), &mut output) };
        if result == Error::BufferTooSmall as i64 {
            return Err(MetadataError::BufferTooSmall {
                records: output.names_count as usize,
                bytes: output.bytes_count as usize,
            });
        }
        status(result)?;
        let names = names.get(..output.names_count as usize).ok_or(Error::Internal)?;
        let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
        for r in names {
            text(bytes, *r)?;
        }
        Ok(Capabilities { names, bytes })
    }
    pub fn startup_arguments<'a>(
        &mut self,
        arguments: &'a mut [ffi::Argument],
        bytes: &'a mut [u8],
    ) -> Result<Arguments<'a>, MetadataError> {
        let mut output = ffi::Arguments::new(buffer(arguments)?, buffer(bytes)?);
        // SAFETY: initialized descriptor and independent exclusive output slices.
        let result = unsafe { ffi::pb_startup_arguments(self.raw.as_ptr(), &mut output) };
        if result == Error::BufferTooSmall as i64 {
            return Err(MetadataError::BufferTooSmall {
                records: output.arguments_count as usize,
                bytes: output.bytes_count as usize,
            });
        }
        status(result)?;
        let arguments = arguments.get(..output.arguments_count as usize).ok_or(Error::Internal)?;
        let bytes = bytes.get(..output.bytes_count as usize).ok_or(Error::Internal)?;
        for a in arguments {
            text(bytes, a.name())?;
            text(bytes, a.value())?;
        }
        Ok(Arguments { arguments, bytes })
    }
    pub fn presence<'a>(
        &mut self,
        participants: &'a mut [ffi::Participant],
    ) -> Result<&'a [ffi::Participant], PollError> {
        let mut output = buffer(participants).map_err(PollError::Host)?;
        // SAFETY: descriptor and participants are disjoint and exclusively borrowed.
        let result = unsafe {
            ffi::pb_presence(
                self.raw.as_ptr(),
                output.data,
                output.capacity,
                output.stride,
                &mut output.count,
            )
        };
        if result == Error::BufferTooSmall as i64 {
            return Err(PollError::BufferTooSmall(output.count as usize));
        }
        status(result).map_err(PollError::Host)?;
        participants.get(..output.count as usize).ok_or(PollError::Host(Error::Internal))
    }
}
