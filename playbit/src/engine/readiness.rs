//! Deadline-bounded descriptor readiness shared by native engine owners.
use crate::engine::Result;
use std::{io, os::fd::AsRawFd, time::Instant};

/// Inspect the original connected stream. Hangup includes already buffered
/// data: a persistent reader can finish those frames without waiting on a peer.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
#[allow(unsafe_code, reason = "poll borrows one initialized descriptor with zero timeout.")]
pub(super) fn peer_hung_up(stream: &std::os::unix::net::UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    let hangup = libc::POLLHUP | libc::POLLRDHUP;
    #[cfg(not(target_os = "linux"))]
    let hangup = libc::POLLHUP;
    let mut event = libc::pollfd { fd: stream.as_raw_fd(), events: hangup, revents: 0 };
    loop {
        // SAFETY: the borrowed stream and descriptor remain live for the call.
        let result = unsafe { libc::poll(&mut event, 1, 0) };
        if result >= 0 {
            return event.revents & hangup != 0;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Wait for `fd` to become readable: Ok(false) once `deadline` passes first.
#[allow(
    unsafe_code,
    reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
)]
pub(crate) fn wait_readable(fd: &impl AsRawFd, deadline: Instant) -> Result<bool> {
    loop {
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
        else {
            return Ok(false);
        };
        let mut event = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live descriptor and initialized output, borrowed for poll.
        let result =
            unsafe { libc::poll(&mut event, 1, remaining.as_millis().saturating_add(1) as i32) };
        if result > 0 {
            if event.revents & libc::POLLIN != 0 {
                return Ok(true);
            }
            return Err("descriptor stopped being readable while waiting on it".into());
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error().into());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    #[test]
    fn readiness_does_not_consume_completion() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[42]).unwrap();
        assert!(wait_readable(&reader, Instant::now() + Duration::from_secs(1)).unwrap());
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [42]);
    }

    #[test]
    fn quiet_descriptor_reaches_deadline() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        assert!(!wait_readable(&reader, deadline).unwrap());
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn expired_deadline_wins_even_if_readable() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[42]).unwrap();
        // Exercise equal clock ticks as well as deadlines already in the past.
        for _ in 0..1000 {
            assert!(!wait_readable(&reader, Instant::now()).unwrap());
        }
    }
}
