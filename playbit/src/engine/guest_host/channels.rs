//! Retry idempotent channel cleanup on the existing channel command lane.
use super::{Failure, Shared, State, record_failure};
use crate::engine::{Request, Result, ipc};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

const ATTEMPTS: usize = 4;
const RETRY_DELAY: Duration = Duration::from_millis(25);

fn active(state: &State) -> bool {
    !state.disposed && state.failure.is_none()
}

pub(super) fn send_control(profile: &Path, shared: &Shared, request: Request) {
    let Err(error) = send(profile, shared, &request) else { return };
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    if active(&state) {
        record_failure(&mut state, Failure::Exchange);
        drop(state);
        crate::diagnostic!("channel control exchange failed: {error}");
        (shared.wake)();
    } else {
        drop(state);
    }
    // An exhausted receipt can leave credit in an otherwise live endpoint.
    // Close it even if the guest stops before calling its normal deactivation.
    if retryable(error.as_ref())
        && let Request::InstanceChannelsAck { context, .. } = request
        && let Err(error) = send(profile, shared, &Request::InstanceChannelsClose { context })
    {
        crate::diagnostic!("channel owner retirement exchange failed: {error}");
    }
}

fn send(profile: &Path, shared: &Shared, request: &Request) -> Result<()> {
    let mut remaining = ATTEMPTS;
    let closing = matches!(request, Request::InstanceChannelsClose { .. });
    loop {
        if !closing && !active(&shared.state.lock().expect("guest completion lock poisoned")) {
            return Ok(());
        }
        let Err(error) = call(profile, request) else { return Ok(()) };
        remaining -= 1;
        if remaining == 0 || !retryable(error.as_ref()) {
            return Err(error);
        }
        std::thread::sleep(RETRY_DELAY);
    }
}

/// A receipt or close owns a short absolute deadline on the channel worker.
/// These requests are idempotent for the exact authenticated owner context.
fn call(profile: &Path, request: &Request) -> Result<ipc::Response> {
    const ATTEMPT_LIMIT: Duration = Duration::from_millis(200);
    let deadline = Instant::now() + ATTEMPT_LIMIT;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    // A full Unix listen backlog must fail without blocking this lane in connect.
    socket.set_nonblocking(true)?;
    socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)?;
    let stream: UnixStream = socket.into();
    ipc::verify_peer(&stream)?;
    ipc::exchange_archive(&mut ChannelControlStream { stream, deadline }, request)
}

fn retryable(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(rejection) = error.downcast_ref::<ipc::RejectedReply>() {
        return rejection.0 == ipc::REQUEST_QUEUE_FULL;
    }
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::WouldBlock
                | io::ErrorKind::Interrupted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
        ) || matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EALREADY))
    })
}

struct ChannelControlStream {
    stream: UnixStream,
    deadline: Instant,
}
impl ChannelControlStream {
    #[allow(unsafe_code, reason = "Poll borrows one initialized descriptor.")]
    fn wait(&self, events: libc::c_short) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd { fd: self.stream.as_raw_fd(), events, revents: 0 };
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(io::ErrorKind::TimedOut)?;
            // SAFETY: the owned socket and initialized descriptor outlive this call.
            let ready =
                unsafe { libc::poll(&mut descriptor, 1, (remaining.as_millis() + 1) as i32) };
            if ready > 0 {
                return Ok(());
            }
            if ready == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
impl Read for ChannelControlStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.wait(libc::POLLIN)?;
        self.stream.read(bytes)
    }
}
impl Write for ChannelControlStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.wait(libc::POLLOUT)?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
