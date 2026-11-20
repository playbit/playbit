//! Kernel-backed process lifetime observation. Socket loss is attachment loss,
//! never evidence that a producer exited or that its PID cannot be reused.
#![allow(
    unsafe_code,
    reason = "Kernel process handles and initialized poll/kevent buffers are owned for the complete watch lifetime."
)]
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub struct Watch {
    descriptor: OwnedFd,
}

pub fn background() -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: changes only the current recorder/observer thread's class.
        let error =
            unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    #[cfg(target_os = "linux")]
    {
        lower_priority(10)?;
    }
    Ok(())
}

pub fn collector_priority() -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        background()
    }
    #[cfg(target_os = "linux")]
    {
        lower_priority(2)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn lower_priority(nice: i32) -> io::Result<()> {
    // SAFETY: Linux PRIO_PROCESS with id zero changes only this trace thread.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod priority_tests {
    use super::*;

    #[test]
    fn collector_stays_below_product_threads_and_above_other_trace_workers() {
        let priority = |set: fn() -> io::Result<()>| {
            std::thread::spawn(move || {
                set().expect("trace thread priority");
                // SAFETY: id zero reads only the calling Linux thread's nice value.
                unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
            })
            .join()
            .expect("priority worker")
        };
        assert_eq!(priority(collector_priority), 2);
        assert_eq!(priority(background), 10);
    }
}

impl Watch {
    pub fn new(pid: u32) -> io::Result<Self> {
        #[cfg(target_vendor = "apple")]
        {
            // SAFETY: kqueue returns a fresh owned descriptor or -1.
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let descriptor = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: initialized process event registration and live kqueue.
            if unsafe { libc::kevent(fd, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: pidfd_open uses scalar arguments and yields a kernel
            // process identity retained independently of later PID reuse.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor: unsafe { OwnedFd::from_raw_fd(fd) } })
        }
    }

    pub fn descriptor(&self) -> i32 {
        self.descriptor.as_raw_fd()
    }

    pub fn exited(&self) -> io::Result<bool> {
        #[cfg(target_vendor = "apple")]
        {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let immediate = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: one bounded event output and a zero timeout. This runs
            // only after kernel readiness, never from an idle scanning timer.
            let count = unsafe {
                libc::kevent(self.descriptor(), std::ptr::null(), 0, &mut event, 1, &immediate)
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(count == 1
                && event.filter == libc::EVFILT_PROC
                && event.fflags & libc::NOTE_EXIT != 0)
        }
        #[cfg(target_os = "linux")]
        {
            let mut fd = libc::pollfd { fd: self.descriptor(), events: libc::POLLIN, revents: 0 };
            // SAFETY: initialized bounded poll array and owned pidfd.
            let count = unsafe { libc::poll(&mut fd, 1, 0) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(fd.revents & libc::POLLIN != 0)
        }
    }
}

pub fn wait(descriptors: &mut [libc::pollfd], blocking: bool) -> io::Result<()> {
    if descriptors.len() > 32 {
        return Err(io::Error::other("trace readiness capacity exceeded"));
    }
    // SAFETY: poll only writes revents within the borrowed initialized array.
    let count = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            if blocking { -1 } else { 0 },
        )
    };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}
