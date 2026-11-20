//! Bounded authenticated descriptor transfer. Rejecting a registration closes
//! every received descriptor, including unexpected ancillary rights.
#![allow(
    unsafe_code,
    reason = "Unix ancillary APIs borrow aligned bounded control buffers and owned socket descriptors; received rights immediately acquire RAII ownership."
)]
use std::{
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
};

pub const REGISTRATION_BYTES: usize = 64;
const CONTROL_WORDS: usize = 32;
const MAX_RIGHTS: usize = 2;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Registration is attempted only on bootstrap/external reconnect work. A
/// full listen backlog cannot leave its one temporary thread blocked in connect.
pub fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    let path = path.as_os_str().as_bytes();
    // SAFETY: initialized sockaddr with a checked, NUL-terminated local path.
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    if path.is_empty() || path.len() >= address.sun_path.len() || path.contains(&0) {
        return Err(invalid("trace socket path exceeds native address bounds"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(path) {
        *target = *byte as libc::c_char;
    }
    let length = mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    #[cfg(target_vendor = "apple")]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: fresh local stream socket, transferred immediately to RAII.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(descriptor);
    stream.set_nonblocking(true)?;
    // SAFETY: bounded initialized sockaddr and owned live descriptor. Pending
    // admission is a refused attempt, never an implicit wait or timer retry.
    if unsafe { libc::connect(raw, std::ptr::from_ref(&address).cast(), length as libc::socklen_t) }
        != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

pub fn peer(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(target_vendor = "apple")]
    let (uid, pid) = {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: initialized credential outputs, live socket.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut length = mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: bounded initialized pid output and length.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                std::ptr::from_mut(&mut pid).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != mem::size_of_val(&pid) {
            return Err(invalid("invalid trace peer pid"));
        }
        (uid, pid)
    };
    #[cfg(target_os = "linux")]
    let (uid, pid) = {
        // SAFETY: zero is a valid initial credential structure.
        let mut credentials: libc::ucred = unsafe { mem::zeroed() };
        let mut length = mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: initialized bounded output, live socket and no retained pointers.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::from_mut(&mut credentials).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != mem::size_of_val(&credentials) {
            return Err(invalid("invalid trace peer credentials"));
        }
        (credentials.uid, credentials.pid)
    };
    // SAFETY: geteuid returns the current OS principal without caller memory.
    if uid != unsafe { libc::geteuid() } || pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace peer is not this OS account",
        ));
    }
    Ok(pid as u32)
}

pub fn send(
    stream: &UnixStream,
    bytes: &[u8; REGISTRATION_BYTES],
    rights: [&OwnedFd; MAX_RIGHTS],
) -> io::Result<()> {
    let mut control = [0usize; CONTROL_WORDS];
    let mut vector =
        libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    // SAFETY: null pointers and zero lengths are valid before fields are assigned.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE describes a bounded two-fd ancillary message.
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE((MAX_RIGHTS * mem::size_of::<i32>()) as _) } as _;
    assert!(message.msg_controllen as usize <= mem::size_of_val(&control));
    // SAFETY: control is sufficiently aligned and large; all fields and data
    // are initialized before sendmsg, which retains no borrowed pointer.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((MAX_RIGHTS * mem::size_of::<i32>()) as _) as _;
        let data = libc::CMSG_DATA(header).cast::<i32>();
        for (i, fd) in rights.iter().enumerate() {
            data.add(i).write_unaligned(fd.as_raw_fd());
        }
    }
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::MSG_DONTWAIT;
    // SAFETY: message points only into live stack buffers and borrowed descriptors.
    let count = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, flags) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize != bytes.len() {
        return Err(invalid("partial trace registration send"));
    }
    Ok(())
}

pub fn receive(
    stream: &UnixStream,
) -> io::Result<([u8; REGISTRATION_BYTES], [OwnedFd; MAX_RIGHTS])> {
    let mut bytes = [0; REGISTRATION_BYTES];
    let mut control = [0usize; CONTROL_WORDS];
    let mut vector = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
    // SAFETY: zero header is filled with valid initialized buffers before recvmsg.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = mem::size_of_val(&control) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: all writable buffers are bounded by their actual lengths.
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, flags) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let rights = collect_rights(&message, &control)?;
    if count as usize != bytes.len()
        || message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0
    {
        return Err(invalid("partial trace registration or truncated rights"));
    }
    Ok((bytes, rights))
}

fn collect_rights(
    message: &libc::msghdr,
    control: &[usize; CONTROL_WORDS],
) -> io::Result<[OwnedFd; MAX_RIGHTS]> {
    let mut rights: [Option<OwnedFd>; MAX_RIGHTS] = [None, None];
    let mut count = 0;
    let mut bad = false;
    // SAFETY: recvmsg populated this bounded and aligned control buffer.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    let base = control.as_ptr() as usize;
    let end = base + mem::size_of_val(control);
    // Each header consumes at least one machine word. Bound traversal even if
    // the kernel or a future caller supplies an unexpected ancillary layout.
    for _ in 0..CONTROL_WORDS {
        if header.is_null() {
            break;
        }
        let position = header as usize;
        if position < base || position + mem::size_of::<libc::cmsghdr>() > end {
            return Err(invalid("invalid ancillary header"));
        }
        // SAFETY: header bounds and alignment are established above.
        let h = unsafe { &*header };
        let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
        if (h.cmsg_len as usize) < minimum || h.cmsg_len as usize > end - position {
            return Err(invalid("invalid ancillary length"));
        }
        if h.cmsg_level == libc::SOL_SOCKET && h.cmsg_type == libc::SCM_RIGHTS {
            let size = h.cmsg_len as usize - minimum;
            bad |= !size.is_multiple_of(mem::size_of::<i32>());
            for index in 0..size / mem::size_of::<i32>() {
                // SAFETY: SCM_RIGHTS contains owned descriptor integers in the
                // kernel-validated data extent; immediately arrange closure.
                let fd =
                    unsafe { libc::CMSG_DATA(header).cast::<i32>().add(index).read_unaligned() };
                if fd < 0 {
                    bad = true;
                    continue;
                }
                let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                    bad = true;
                }
                if count < MAX_RIGHTS {
                    rights[count] = Some(owned);
                } else {
                    bad = true;
                }
                count += 1;
            }
        } else {
            bad = true;
        }
        // SAFETY: next header stays within message.msg_controllen; validate it
        // again on the next iteration before reading any fields.
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
    if bad || count != MAX_RIGHTS {
        return Err(invalid("unexpected trace ancillary rights"));
    }
    Ok(rights.map(|fd| fd.expect("exactly two validated descriptors")))
}
