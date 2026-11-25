//! One kernel queue wakes the source watcher for coalesced work or a live
//! installer. The installer wake is independent of the FSEvents stream.
#![allow(unsafe_code, reason = "Native kqueue operations borrow live owned descriptors.")]
use playbit::engine::{Result, installation::RunInterrupt};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr,
    time::Instant,
};

const SOURCE_READY: usize = 0;
const VNODE_LOST: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;

pub(super) struct Wake {
    // The queue drops before the vnode descriptor it watches.
    queue: OwnedFd,
    interrupt: Option<RunInterrupt>,
}

impl Wake {
    pub fn new(interrupt: Option<RunInterrupt>) -> Result<Self> {
        // SAFETY: a successful call returns a new descriptor, owned immediately.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        rustix::io::fcntl_setfd(&queue, rustix::io::FdFlags::CLOEXEC)?;
        let wake = Self { queue, interrupt };
        wake.change(event(SOURCE_READY, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0))?;
        if let Some(interrupt) = &wake.interrupt {
            wake.change(event(
                interrupt.as_fd().as_raw_fd() as usize,
                libc::EVFILT_VNODE,
                libc::EV_ADD | libc::EV_CLEAR,
                libc::NOTE_ATTRIB | VNODE_LOST,
            ))?;
        }
        Ok(wake)
    }

    pub fn update_requested(&self) -> Result<bool> {
        self.interrupt.as_ref().map_or(Ok(false), RunInterrupt::requested)
    }

    pub fn source_ready(&self) {
        self.change(event(SOURCE_READY, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER))
            .expect("the owned kqueue has a registered source event");
    }

    fn change(&self, change: libc::kevent) -> io::Result<()> {
        loop {
            // SAFETY: the queue and any registered vnode descriptor remain live.
            if unsafe {
                libc::kevent(self.queue.as_raw_fd(), &change, 1, ptr::null_mut(), 0, ptr::null())
            } >= 0
            {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    pub fn wait(&self, deadline: Option<Instant>) -> Result<Option<i16>> {
        let mut ready = event(0, 0, 0, 0);
        loop {
            let timeout = deadline.map(|deadline| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                libc::timespec {
                    tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
                    tv_nsec: remaining.subsec_nanos().into(),
                }
            });
            // SAFETY: the owned queue and initialized single output event stay live.
            let count = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    ptr::null(),
                    0,
                    &mut ready,
                    1,
                    timeout.as_ref().map_or(ptr::null(), ptr::from_ref),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if count == 0 {
                return Ok(None);
            }
            if ready.flags & libc::EV_ERROR != 0 {
                return Err("Source watcher notification failed".into());
            }
            if ready.filter == libc::EVFILT_VNODE && ready.fflags & VNODE_LOST != 0 {
                return Err("Playbit update request file was removed or replaced".into());
            }
            return Ok(Some(ready.filter));
        }
    }
}

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent { ident, filter, flags, fflags, data: 0, udata: ptr::null_mut() }
}
