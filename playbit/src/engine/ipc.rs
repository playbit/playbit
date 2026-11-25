//! Bounded local IPC. This socket is never a remote service endpoint.
pub mod codec;
pub(crate) mod request_memory;
use crate::engine::{
    Request, Result,
    reply::{ArchivedReply, Reply},
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};
/// Increment when local request/response semantics change incompatibly. This is
/// independent of the AP protocol and stored data format; no version fallback.
pub const LOCAL_PROTOCOL: u32 = codec::ABI;
/// Includes the prefix; reserved independently of ordinary reply payloads.
pub const CHANGE_REPLY_CREDIT: usize = 256;
/// Includes the prefix and length field for the fixed change-wait request.
const CHANGE_REQUEST_LIMIT: usize = 256;
pub(crate) const REQUEST_QUEUE_FULL: &str =
    "Local request queue is full; this request did not start";
pub(crate) const OVERSIZED_REPLY: &str = "Local response exceeds its limit; operation was dispatched and must not be retried automatically";
pub const INCOMPATIBLE_ENGINE: &str = "The local Playbit engine uses a different protocol.";

/// A small launch-only exchange. Never use the general 4 MiB/300 s call limits
/// before compatibility is known, and never interpret failure as signed out.
pub fn check_protocol(stream: &mut UnixStream) -> Result<()> {
    let _timing = crate::trace::Span::new("ipc.check_protocol");
    verify_peer(stream)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write(stream, &codec::encode(&Request::LocalProtocol, 512 - codec::HEADER_LEN)?)?;
    stream.set_nonblocking(true)?;
    let mut header = [0; codec::HEADER_LEN];
    read_protocol_bytes(stream, &mut header[..4], deadline)?;
    let length = u32::from_le_bytes(header[..4].try_into().expect("fixed length")) as usize;
    if length > 512 {
        return Err(INCOMPATIBLE_ENGINE.into());
    }
    let archive_len = codec::archive_length(length as u32).map_err(|_| INCOMPATIBLE_ENGINE)?;
    read_protocol_bytes(stream, &mut header[4..], deadline)?;
    codec::check_prefix(header[4..].try_into().expect("fixed prefix"))
        .map_err(|_| INCOMPATIBLE_ENGINE)?;
    let mut bytes = codec::Bytes::with_capacity(archive_len);
    bytes.resize(archive_len, 0);
    read_protocol_bytes(stream, &mut bytes, deadline)?;
    match codec::access::<ArchivedReply>(&bytes).map_err(|_| INCOMPATIBLE_ENGINE)? {
        ArchivedReply::Protocol { protocol, version }
            if protocol.to_native() == LOCAL_PROTOCOL && version.as_str() == crate::VERSION => {}
        _ => return Err(INCOMPATIBLE_ENGINE.into()),
    }

    Ok(())
}

#[allow(
    unsafe_code,
    reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
)]
fn read_protocol_bytes(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    const UNAVAILABLE: &str = "Playbit’s background engine did not respond. Reopen Playbit to retry; your local data is retained.";
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or(UNAVAILABLE)?;
        use std::os::fd::AsRawFd;
        let mut ready = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one initialized poll descriptor, borrowed for this call only.
        let result =
            unsafe { libc::poll(&mut ready, 1, remaining.as_millis().saturating_add(1) as i32) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(UNAVAILABLE.into());
        }
        if result == 0 {
            continue;
        }
        match stream.read(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "engine exited during protocol identification",
                )
                .into());
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(_) => return Err(UNAVAILABLE.into()),
        }
    }
    Ok(())
}

/// The engine writes this once to stdout after its local admission is ready.
/// Diagnostics remain on stderr. This is launch completion, not an IPC reply.
pub const ENGINE_READY: &[u8] = b"{\"ready\":true}\n";
/// Failed startup: followed by a little-endian u32 length and bounded JSON diagnostic.
pub const ENGINE_FAILED: &[u8] = b"{\"error\":true}\n";
const _: () = assert!(ENGINE_READY.len() == ENGINE_FAILED.len());
pub const PROFILE_OPEN_ERROR: &str = "Playbit could not open this local profile. Your data has been retained. See the profile’s logs for details.";
pub const CORRUPT_STORE_ERROR: &str = "The local store is corrupt.";
pub fn write(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    Ok(codec::write(stream, bytes)?)
}
pub fn read(stream: &mut UnixStream) -> Result<codec::Bytes> {
    Ok(codec::read(stream, codec::MAX_ARCHIVE)?)
}

/// Owns the immutable archive while callers borrow its fields and payloads.
/// Materialize only results which must outlive this buffer.
pub struct Response {
    bytes: codec::Bytes,
}

/// A complete, validated engine rejection, distinct from an uncertain exchange.
#[derive(Debug)]
pub(crate) struct RejectedReply(pub(super) String);

impl std::fmt::Display for RejectedReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RejectedReply {}

impl Response {
    pub fn new(bytes: codec::Bytes) -> Result<Self> {
        if let ArchivedReply::Error { error } = codec::access::<ArchivedReply>(&bytes)? {
            return Err(Box::new(RejectedReply(error.to_string())));
        }
        Ok(Self { bytes })
    }
    #[allow(unsafe_code, reason = "Response owns an immutable, validated archive.")]
    pub fn archived(&self) -> &ArchivedReply {
        // SAFETY: new validates the complete ArchivedReply before constructing
        // Response. Its private aligned buffer is never mutated or exposed for
        // mutation. Moving Response preserves the allocation and relative
        // pointers; the returned borrow cannot outlive its owner.
        unsafe { rkyv::access_unchecked::<ArchivedReply>(&self.bytes) }
    }
    pub fn into_owned(self) -> Result<Reply> {
        Ok(rkyv::api::low::deserialize::<Reply, rkyv::rancor::Error>(self.archived())?)
    }
}

/// The local endpoint authorizes the OS account, not arbitrary remote peers or
/// separate trust domains inside one account. Check before sending any payload.
#[allow(
    unsafe_code,
    reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
)]
pub fn verify_peer(stream: &UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: pointers address initialized uid/gid outputs for this socket.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: getsockopt receives the size and writable credential object.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(credentials).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if size as usize != std::mem::size_of_val(&credentials) {
            return Err("invalid local peer credentials".into());
        }
        credentials.uid
    };
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux")))]
    compile_error!("local peer credential verification is not implemented for this target");
    // SAFETY: geteuid takes no arguments and returns this process's OS identity.
    require_uid(uid, unsafe { libc::geteuid() })
}
fn require_uid(peer: libc::uid_t, expected: libc::uid_t) -> Result<()> {
    if peer != expected {
        return Err("local endpoint belongs to a different OS account".into());
    }
    Ok(())
}

/// One decoded engine change notification.
#[derive(Debug)]
pub struct ChangeReply {
    pub token: crate::engine::ChangeToken,
}

/// One change wait owned by the GUI's worker. No thread is created here.
pub struct ChangeWait {
    stream: UnixStream,
}
impl ChangeWait {
    pub fn connect(profile: &Path, after: Option<crate::engine::ChangeToken>) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        socket
            .connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || matches!(e.raw_os_error(), Some(libc::EINPROGRESS | libc::EALREADY))
                {
                    "Local change endpoint is busy; this wait did not start".to_owned()
                } else {
                    format!("Local change wait did not start for {}: {e}", profile.display())
                }
            })?;
        let mut stream: UnixStream = socket.into();
        verify_peer(&stream)?;
        // Configure before sending: macOS can reject socket option changes
        // after the peer has written its reply and closed the connection.
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let request = codec::encode(
            &Request::WaitChanges { after },
            CHANGE_REQUEST_LIMIT - codec::HEADER_LEN,
        )?;
        // A partial read-only wait can be discarded safely. Do not wait for
        // socket space before the caller can acquire its cancellation handle.
        write(&mut stream, &request).map_err(|e| {
            format!("Local change request could not be sent; this wait did not start: {e}")
        })?;
        stream.set_nonblocking(false)?;
        Ok(Self { stream })
    }

    /// Call shutdown(Both) on this handle to interrupt receive, including when
    /// the worker has not entered receive yet. Dropping a clone does not cancel.
    pub fn cancellation_handle(&self) -> Result<UnixStream> {
        Ok(self.stream.try_clone()?)
    }

    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    pub fn receive_archive(mut self) -> Result<Response> {
        use std::os::fd::AsRawFd;
        // No receive syscall or socket deadline runs while the engine is
        // unchanged. Peer closure and cancellation also wake this readiness
        // wait. The subsequent reads retain the ordinary framing timeout.
        let mut descriptor =
            libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        loop {
            // SAFETY: descriptor is initialized and its socket remains owned
            // throughout poll. A negative timeout waits without an idle timer.
            if unsafe { libc::poll(&mut descriptor, 1, -1) } > 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        let bytes = codec::read(&mut self.stream, CHANGE_REPLY_CREDIT - codec::PREFIX_LEN)?;
        let response = Response::new(bytes)?;
        if !matches!(response.archived(), ArchivedReply::Changes(_)) {
            return Err("unexpected local change reply".into());
        }
        Ok(response)
    }
    pub fn receive(self) -> Result<ChangeReply> {
        let response = self.receive_archive()?;
        let ArchivedReply::Changes(token) = response.archived() else { unreachable!() };
        Ok(ChangeReply {
            token: rkyv::api::low::deserialize::<crate::engine::ChangeToken, rkyv::rancor::Error>(
                token,
            )?,
        })
    }
}

pub fn call_archive(profile: &Path, request: &Request) -> Result<Response> {
    let _timing = crate::trace::Span::new("ipc.call");
    if let Request::WaitChanges { after } = request {
        return ChangeWait::connect(profile, *after)?.receive_archive();
    }
    let mut stream = UnixStream::connect(profile.join(crate::paths::ENGINE_SOCKET))
        .map_err(|e| format!("start playbit-engine for {}: {e}", profile.display()))?;
    call_stream_archive(&mut stream, request)
}

/// Execute one request; a duplicated descriptor may cancel the wait. Never
/// replay after sending, even if validation of the returned frame fails.
pub fn call_stream_archive(stream: &mut UnixStream, request: &Request) -> Result<Response> {
    verify_peer(stream)?;
    // Preference saves drain during normal quit; a stalled local owner must not
    // retain that drain for the general long-running operation timeout.
    let preference = matches!(request, Request::UiZoom { .. } | Request::BrowserDrafts { .. });
    let receiving = matches!(request, Request::Syscall { call, .. }
        if matches!(call.operation, crate::runtime::host::DataOperation::ChannelEvents { .. }));
    stream.set_read_timeout(if receiving {
        None
    } else {
        Some(Duration::from_secs(if preference { 3 } else { 300 }))
    })?;
    stream.set_write_timeout(Some(Duration::from_secs(if preference { 3 } else { 30 })))?;
    exchange_archive(stream, request)
}

pub(super) fn exchange_archive(
    stream: &mut (impl Read + Write),
    request: &Request,
) -> Result<Response> {
    {
        let bytes = {
            let _timing = crate::trace::Span::new("ipc.request_encode");
            codec::encode(request, codec::MAX_ARCHIVE)?
        };
        let _timing = crate::trace::Span::new("ipc.request_write");
        codec::write(stream, &bytes)?;
    }
    let bytes = {
        let _timing = crate::trace::Span::new("ipc.response_read");
        let limit = if matches!(request, Request::GetAccountDisplayNames { .. }) {
            crate::network::ap::operation::ACCOUNT_DISPLAY_NAMES_RESPONSE_BYTES
        } else {
            codec::MAX_ARCHIVE
        };
        codec::read(stream, limit)?
    };
    let _timing = crate::trace::Span::new("ipc.response_decode");
    let response = Response::new(bytes)?;
    if !response.archived().matches_request(request) {
        return Err(
            "The local engine returned an unexpected reply; do not retry a mutation automatically"
                .into(),
        );
    }
    Ok(response)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::io::Write;
    use std::{os::unix::net::UnixListener, sync::mpsc, thread, time::Instant};

    #[test]
    fn engine_exit_during_identification_is_retryable() {
        let (mut client, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let error =
            read_protocol_bytes(&mut client, &mut [0; 4], Instant::now() + Duration::from_secs(1))
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof,
        );
        assert!(!crate::engine::restart::terminal(&error.to_string()));
    }

    #[test]
    fn change_and_fallback_replies_fit_reserved_credit() {
        let changes = crate::engine::ChangeToken {
            generation: crate::store::Id::from_bytes([255; 16]),
            sequence: u64::MAX,
        };
        for reply in [Reply::Changes(changes), Reply::Error { error: OVERSIZED_REPLY.into() }] {
            let bytes = codec::encode(&reply, CHANGE_REPLY_CREDIT - codec::PREFIX_LEN).unwrap();
            assert!(bytes.capacity() + codec::PREFIX_LEN <= CHANGE_REPLY_CREDIT);
            let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
            let owned =
                rkyv::api::low::deserialize::<Reply, rkyv::rancor::Error>(archived).unwrap();
            assert_eq!(serde_json::to_value(owned).unwrap(), serde_json::to_value(reply).unwrap());
        }
    }

    #[test]
    fn change_requests_fit_the_current_header_allowance() {
        for after in [
            None,
            Some(crate::engine::ChangeToken {
                generation: crate::store::Id::from_bytes([255; 16]),
                sequence: u64::MAX,
            }),
        ] {
            let bytes = codec::encode(
                &Request::WaitChanges { after },
                CHANGE_REQUEST_LIMIT - codec::HEADER_LEN,
            )
            .unwrap();
            assert!(bytes.len() + codec::HEADER_LEN <= CHANGE_REQUEST_LIMIT);
        }
    }

    #[test]
    fn response_validates_before_borrowing_and_keeps_storage_when_moved() {
        assert!(Response::new(codec::Bytes::new()).is_err());
        let reply = Reply::Protocol { protocol: LOCAL_PROTOCOL, version: "v".repeat(256) };
        let bytes = codec::encode(&reply, codec::MAX_ARCHIVE).unwrap();
        let mut corrupt = bytes.clone();
        corrupt.fill(0xff);
        assert!(Response::new(corrupt).is_err());
        let response = Response::new(bytes).unwrap();
        let ArchivedReply::Protocol { version, .. } = response.archived() else {
            panic!("protocol reply")
        };
        let address = version.as_ptr();
        let moved = Box::new(response);
        for _ in 0..3 {
            let ArchivedReply::Protocol { version, .. } = moved.archived() else {
                panic!("protocol reply")
            };
            assert_eq!(version.as_ptr(), address);
            assert_eq!(version.as_str(), "v".repeat(256));
        }
        let Reply::Protocol { version, .. } = moved.into_owned().unwrap() else {
            panic!("owned protocol reply")
        };
        assert_eq!(version, "v".repeat(256));
    }

    #[test]
    fn launch_probe_requires_current_build_abi_and_version() {
        for case in 0..6 {
            let (mut client, mut peer) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                let request = read(&mut peer).unwrap();
                assert!(matches!(
                    codec::access::<crate::engine::ArchivedRequest>(&request).unwrap(),
                    crate::engine::ArchivedRequest::LocalProtocol
                ));
                let reply = Reply::Protocol {
                    protocol: if case == 1 { LOCAL_PROTOCOL + 1 } else { LOCAL_PROTOCOL },
                    version: if case == 2 {
                        "different version".into()
                    } else {
                        crate::VERSION.into()
                    },
                };
                let archive = codec::encode(&reply, 512 - codec::PREFIX_LEN).unwrap();
                let mut frame = Vec::new();
                codec::write(&mut frame, &archive).unwrap();
                if case == 3 {
                    frame[8] ^= 1;
                }
                if case == 5 {
                    frame[12] ^= 1; // Build identity; ABI and version still match.
                }
                if case == 4 {
                    // The scrollbar branch used JSON protocol 3. Its number
                    // cannot make it compatible with the new binary ABI.
                    let old = br#"{"protocol":3}"#;
                    frame = (old.len() as u32).to_le_bytes().to_vec();
                    frame.extend_from_slice(old);
                }
                peer.write_all(&frame).unwrap();
            });
            assert_eq!(check_protocol(&mut client).is_ok(), case == 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn valid_archive_of_wrong_reply_type_is_rejected() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            read(&mut peer).unwrap();
            let reply = codec::encode(&Reply::Disconnected { disconnected: true }, 1024).unwrap();
            write(&mut peer, &reply).unwrap();
        });
        let error = call_stream_archive(
            &mut client,
            &Request::SelectPersonalVolumes {
                session: crate::identifier::encode(&[0xab; 32]),
                grants: vec![],
            },
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("unexpected reply") && error.contains("do not retry"));
        worker.join().unwrap();
    }

    #[test]
    fn local_peer_identity_is_verified() {
        let (left, right) = UnixStream::pair().unwrap();
        verify_peer(&left).unwrap();
        verify_peer(&right).unwrap();
        assert!(require_uid(501, 502).is_err());
        require_uid(501, 501).unwrap();
    }

    #[test]
    fn change_wait_and_generic_call_receive_tokens_from_verified_peer() {
        let profile = tempfile::tempdir().unwrap();
        crate::paths::private_directory(profile.path(), "control").unwrap();
        let listener =
            UnixListener::bind(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let token = crate::engine::ChangeToken {
            generation: crate::store::Id::from_bytes([7; 16]),
            sequence: 42,
        };
        let server = thread::spawn(move || {
            for after in [None, Some(token)] {
                let (mut stream, _) = listener.accept().unwrap();
                verify_peer(&stream).unwrap();
                let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                    &read(&mut stream).unwrap(),
                )
                .unwrap();
                let Request::WaitChanges { after: actual } = request else {
                    panic!("expected a change wait");
                };
                assert_eq!(actual, after);
                let reply = codec::encode(&Reply::Changes(token), 248).unwrap();
                write(&mut stream, &reply).unwrap();
            }
        });
        let wait = ChangeWait::connect(profile.path(), None).unwrap();
        assert_eq!(wait.stream.read_timeout().unwrap(), Some(Duration::from_secs(300)));
        assert_eq!(wait.stream.write_timeout().unwrap(), Some(Duration::from_secs(30)));
        assert_eq!(wait.receive().unwrap().token, token);
        let response =
            call_archive(profile.path(), &Request::WaitChanges { after: Some(token) }).unwrap();
        let Reply::Changes(actual) = response.into_owned().unwrap() else {
            panic!("expected change token")
        };
        assert_eq!(actual, token);
        server.join().unwrap();
    }

    #[test]
    fn change_wait_rejects_full_connection_backlog_without_waiting() {
        let profile = tempfile::tempdir().unwrap();
        crate::paths::private_directory(profile.path(), "control").unwrap();
        let address =
            socket2::SockAddr::unix(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let listener =
            socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        listener.bind(&address).unwrap();
        listener.listen(1).unwrap();
        let mut connected = Vec::new();
        let mut full = false;
        for _ in 0..16 {
            let socket =
                socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
            socket.set_nonblocking(true).unwrap();
            match socket.connect(&address) {
                Ok(()) => connected.push(socket),
                Err(error) => {
                    // Darwin reports ECONNREFUSED for a full Unix backlog;
                    // Linux can report EAGAIN. Both must fail without waiting.
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ConnectionRefused
                        ) || matches!(
                            error.raw_os_error(),
                            Some(libc::EINPROGRESS | libc::EALREADY)
                        ),
                        "full local backlog: {error:?}"
                    );
                    crate::diagnostic!("full local backlog: {error:?}");
                    full = true;
                    break;
                }
            }
        }
        assert!(full && !connected.is_empty(), "small backlog was not filled");
        let path = profile.path().to_owned();
        let (send, receive) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            send.send(ChangeWait::connect(&path, None).map(|_| ()).map_err(|e| e.to_string()))
                .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        // Closing the listener also releases a regressed blocking connect,
        // before joining the worker and checking the original result.
        drop(listener);
        drop(connected);
        worker.join().unwrap();
        assert!(result.unwrap().unwrap_err().contains("did not start"));
    }

    #[test]
    fn change_wait_rejects_oversized_header_before_reading_body() {
        for length in [257u32, u32::MAX] {
            let (stream, mut server) = UnixStream::pair().unwrap();
            server.write_all(&length.to_le_bytes()).unwrap();
            // Keep the writer open without a body. A body read would block;
            // the outer deadline closes the peer if the limit check regresses.
            let wait = ChangeWait { stream };
            let (send, receive) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                send.send(wait.receive().map_err(|e| e.to_string())).unwrap();
            });
            let result = receive.recv_timeout(Duration::from_secs(2));
            let _ = server.shutdown(std::net::Shutdown::Both);
            worker.join().unwrap();
            assert!(result.unwrap().is_err());
        }
    }

    #[test]
    fn change_wait_rejects_invalid_or_error_reply() {
        let error =
            codec::encode(&Reply::Error { error: "Local change waiter limit reached".into() }, 248)
                .unwrap();
        let wrong = codec::encode(&Reply::Disconnected { disconnected: true }, 248).unwrap();
        for (reply, expected) in [
            (b"not an archive".as_slice(), None),
            (&wrong, Some("unexpected")),
            (&error, Some("waiter limit")),
        ] {
            let (stream, mut server) = UnixStream::pair().unwrap();
            write(&mut server, reply).unwrap();
            let error = ChangeWait { stream }.receive().unwrap_err().to_string();
            if let Some(expected) = expected {
                assert!(error.contains(expected), "{error}");
            }
        }
    }

    #[test]
    fn change_wait_cancellation_interrupts_idle_receive_and_precedes_receive() {
        for cancel_before_receive in [false, true] {
            let (stream, server) = UnixStream::pair().unwrap();
            let wait = ChangeWait { stream };
            let cancel = wait.cancellation_handle().unwrap();
            if cancel_before_receive {
                cancel.shutdown(std::net::Shutdown::Both).unwrap();
            }
            let (started, ready) = mpsc::sync_channel(1);
            let (send, receive) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                started.send(()).unwrap();
                send.send(wait.receive().map_err(|e| e.to_string())).unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            if !cancel_before_receive {
                assert!(matches!(
                    receive.recv_timeout(Duration::from_millis(20)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                cancel.shutdown(std::net::Shutdown::Both).unwrap();
            }
            let result = receive.recv_timeout(Duration::from_secs(2));
            // Ensure a failed assertion cannot strand the test worker.
            let _ = server.shutdown(std::net::Shutdown::Both);
            worker.join().unwrap();
            assert!(result.unwrap().is_err());
        }
    }

    #[test]
    fn channel_publish_and_queued_local_request_accept_response_after_thirty_seconds() {
        let profile = tempfile::tempdir().unwrap();
        crate::paths::private_directory(profile.path(), "control").unwrap();
        let listener =
            UnixListener::bind(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let (started, queued) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &read(&mut first).unwrap(),
            )
            .unwrap();
            assert!(
                matches!(request, Request::Syscall { call, .. } if matches!(call.operation, crate::runtime::host::DataOperation::ChannelCommand { .. }))
            );
            started.send(()).unwrap();
            // Real elapsed time is necessary: Unix socket receive deadlines do
            // not use Tokio's virtual clock. Both responses exceed the old 30s.
            thread::sleep(Duration::from_secs(32));
            write(
                &mut first,
                &codec::encode(
                    &Reply::Syscall(crate::engine::service_call::Outcome {
                        context: crate::engine::service_call::Context::Tool {
                            handle: crate::store::Id::from_bytes([1; 16]),
                            incarnation: crate::store::Id::from_bytes([2; 16]),
                        },
                        volume: crate::store::Id::from_bytes([3; 16]),
                        operation: 1,
                        mutation: 1,
                        kind: 16,
                        revision: Some((crate::store::Id::from_bytes([4; 16]), 0)),
                        status: 0,
                        result: Some(crate::runtime::host::DataResult::ChannelCompleted),
                    }),
                    1024,
                )
                .unwrap(),
            )
            .unwrap();
            let (mut second, _) = listener.accept().unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &read(&mut second).unwrap(),
            )
            .unwrap();
            assert!(matches!(request, Request::LocalProtocol));
            write(
                &mut second,
                &codec::encode(
                    &Reply::Protocol { protocol: LOCAL_PROTOCOL, version: crate::VERSION.into() },
                    504,
                )
                .unwrap(),
            )
            .unwrap();
        });
        let path = profile.path().to_owned();
        let sending = thread::spawn(move || {
            call_archive(
                &path,
                &Request::Syscall {
                    context: crate::engine::service_call::Context::Tool {
                        handle: crate::store::Id::from_bytes([1; 16]),
                        incarnation: crate::store::Id::from_bytes([2; 16]),
                    },
                    operation: 1,
                    mutation: 1,
                    call: Box::new(crate::runtime::host::DataCall {
                        volume: [3; 16],
                        operation: crate::runtime::host::DataOperation::ChannelCommand {
                            instance: [1; 16],
                            name: "test".into(),
                            action: 1,
                            data: b"delayed AP response".to_vec(),
                        },
                    }),
                },
            )
        });
        queued.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiting_since = Instant::now();
        let status = call_archive(profile.path(), &Request::LocalProtocol).unwrap();
        assert!(waiting_since.elapsed() > Duration::from_secs(30));
        let Reply::Protocol { protocol, version } = status.into_owned().unwrap() else {
            panic!("expected protocol")
        };
        assert_eq!(protocol, LOCAL_PROTOCOL);
        assert_eq!(version, crate::VERSION);
        let Reply::Syscall(outcome) = sending.join().unwrap().unwrap().into_owned().unwrap() else {
            panic!("expected syscall outcome");
        };
        assert!(matches!(outcome.result, Some(crate::runtime::host::DataResult::ChannelCompleted)));
        server.join().unwrap();
    }
}
