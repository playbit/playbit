//! Nonblocking socket notification. Full buffers already contain a wake; a
//! terminal failure invalidates attachment instead of silently losing readiness.
#![allow(
    unsafe_code,
    reason = "The notification syscall borrows one byte and a descriptor retained by the attachment generation."
)]
use std::{
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
};

pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
    let (producer, collector) = UnixStream::pair()?;
    for socket in [&producer, &collector] {
        socket.set_nonblocking(true)?;
        configure(socket, 1024)?;
    }
    Ok((producer, collector))
}

/// Apply finite socket buffers and suppress process-wide SIGPIPE on Darwin.
pub fn configure(socket: &UnixStream, bytes: libc::c_int) -> io::Result<()> {
    // SAFETY: setsockopt copies a valid initialized scalar; socket is live.
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&bytes).cast(),
                std::mem::size_of_val(&bytes) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_vendor = "apple")]
    {
        let enabled: libc::c_int = 1;
        // SAFETY: initialized scalar, borrowed live socket, no retained pointer.
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                std::ptr::from_ref(&enabled).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn send(descriptor: i32) -> bool {
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::MSG_DONTWAIT;
    // At most two attempts. Exhausted EINTR is a failed attachment, not a lost
    // wake followed by an indefinite wait. Only bootstrap can replace it.
    for _ in 0..2 {
        // SAFETY: the caller's attachment lease keeps descriptor open; one
        // initialized byte is copied synchronously and no pointer is retained.
        let sent = unsafe { libc::send(descriptor, [1u8].as_ptr().cast(), 1, flags) };
        if sent == 1 {
            return true;
        }
        if sent == 0 {
            return false;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            // A full Unix socket can report WouldBlock before its peer's
            // terminal write error. Existing bytes are a wake only while that
            // endpoint remains live. One immediate readiness check, no wait.
            let mut fd = libc::pollfd { fd: descriptor, events: 0, revents: 0 };
            return unsafe { libc::poll(&mut fd, 1, 0) } == 0 && fd.revents == 0;
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_full_socket_remains_a_wake_and_peer_exit_does_not_signal() {
        // Saturate the bounded socket and then close its peer. Sending must
        // neither block on fullness nor terminate the process with SIGPIPE.
        let (producer, collector) = pair().expect("notification pair");
        for _ in 0..4096 {
            assert!(send(producer.as_raw_fd()));
        }
        // Shutdown establishes terminal socket state even if a concurrently
        // spawning test briefly inherits the descriptor before exec.
        collector.shutdown(std::net::Shutdown::Both).expect("peer shutdown");
        drop(collector);
        assert!(!send(producer.as_raw_fd()));
    }
}
