//! Process-owned bounded shared storage. All cross-process fields are atomics;
//! immutable catalogs are published before their state changes to active.
#![allow(
    unsafe_code,
    reason = "POSIX mappings own fixed-size atomic fields and outlive borrowed views."
)]
use crate::queue::{MAX_PRODUCERS, Queue};
#[cfg(not(target_os = "ios"))]
use std::os::fd::FromRawFd;
use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};
pub const MAX_NAMES: usize = 1024;
pub const MAX_NAME_BYTES: usize = 48;
pub const APPS: usize = 5;
pub const APP_NAMES: usize = 256;
#[repr(C)]
pub struct Name {
    length: AtomicU64,
    words: [AtomicU64; 6],
}
impl Name {
    pub fn set(&self, text: &str) {
        assert!(text.len() <= MAX_NAME_BYTES);
        let mut bytes = [0; 48];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        for (word, chunk) in self.words.iter().zip(bytes.as_chunks::<8>().0.iter()) {
            word.store(u64::from_le_bytes(*chunk), Ordering::Relaxed);
        }
        self.length.store(text.len() as u64, Ordering::Release);
    }
    pub fn get<'a>(&self, out: &'a mut [u8; 48]) -> io::Result<&'a str> {
        let length = self.length.load(Ordering::Acquire) as usize;
        if length == 0 || length > 48 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        for (word, chunk) in self.words.iter().zip(out.as_chunks_mut::<8>().0.iter_mut()) {
            chunk.copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        std::str::from_utf8(&out[..length]).map_err(|_| io::ErrorKind::InvalidData.into())
    }
}
#[repr(C)]
pub struct App {
    pub state: AtomicU64,
    pub scope: AtomicU64,
    pub generation: AtomicU64,
    pub selection: AtomicU64,
    pub instance: [AtomicU64; 2],
    pub identity_flags: AtomicU64,
    pub identity: [AtomicU64; 8],
    pub label_length: AtomicU64,
    pub label: [AtomicU64; 32],
    pub count: AtomicU64,
    pub names: [Name; APP_NAMES],
    pub kinds: [AtomicU64; APP_NAMES],
    pub lost: AtomicU64,
    pub retire_heads: [AtomicU64; MAX_PRODUCERS],
}
impl App {
    pub fn effective_generation(&self, default: u64) -> u64 {
        match self.selection.load(Ordering::Acquire) {
            0 => default,
            1 => self.generation.load(Ordering::Acquire),
            _ => 0,
        }
    }
}
#[repr(C)]
pub struct Header {
    pub version: AtomicU64,
    pub source: [AtomicU64; 2],
    pub pid: AtomicU64,
    pub kind: AtomicU64,
    pub recording: AtomicU64,
    pub generation: AtomicU64,
    pub next_scope: AtomicU64,
    pub next_tid: AtomicU64,
    pub sleeping: AtomicU64,
    pub activity: AtomicU64,
    pub disabled: AtomicU64,
    pub lost: AtomicU64,
    pub name_count: AtomicU64,
    pub names: [Name; MAX_NAMES],
    pub apps: [App; APPS],
    pub queues: [Queue; MAX_PRODUCERS],
}
pub const MAX_REGION_BYTES: usize = std::mem::size_of::<Header>().div_ceil(16384) * 16384;
const _: () = assert!(MAX_REGION_BYTES <= 688128);
pub struct Region {
    mapping: NonNull<u8>,
    descriptor: Option<OwnedFd>,
}
unsafe impl Send for Region {}
unsafe impl Sync for Region {}
pub fn incarnation() -> io::Result<[u64; 2]> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let id = [
        u64::from_le_bytes(bytes[..8].try_into().expect("id")),
        u64::from_le_bytes(bytes[8..].try_into().expect("id")),
    ];
    if id == [0, 0] {
        return Err(io::Error::other("zero trace incarnation"));
    }
    Ok(id)
}
impl Region {
    pub fn create(kind: u64, names: &[&str]) -> io::Result<Self> {
        if names.len() > MAX_NAMES || names.iter().any(|n| n.is_empty() || n.len() > 48) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let id = incarnation()?;
        #[cfg(not(target_os = "ios"))]
        let descriptor = {
            let name =
                std::ffi::CString::new(format!("/pb-event-{:016x}", id[0])).expect("ascii name");
            let fd = unsafe {
                libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::shm_unlink(name.as_ptr()) } != 0
                || unsafe { libc::ftruncate(fd.as_raw_fd(), MAX_REGION_BYTES as libc::off_t) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Some(fd)
        };
        #[cfg(target_os = "ios")]
        let descriptor = None;
        let region = Self::map(descriptor)?;
        let h = region.header();
        h.source[0].store(id[0], Ordering::Relaxed);
        h.source[1].store(id[1], Ordering::Relaxed);
        h.pid.store(u64::from(std::process::id()), Ordering::Relaxed);
        h.kind.store(kind, Ordering::Relaxed);
        h.generation.store(1, Ordering::Relaxed);
        h.next_scope.store(1, Ordering::Relaxed);
        h.next_tid.store(1, Ordering::Relaxed);
        for (name, text) in h.names.iter().zip(names) {
            name.set(text);
        }
        h.name_count.store(names.len() as u64, Ordering::Relaxed);
        h.version.store(3, Ordering::Release);
        Ok(region)
    }
    pub fn attach(descriptor: OwnedFd, expected: [u64; 2]) -> io::Result<Self> {
        let metadata = File::from(descriptor.try_clone()?).metadata()?;
        if metadata.len() != MAX_REGION_BYTES as u64 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let region = Self::map(Some(descriptor))?;
        if region.header().version.load(Ordering::Acquire) != 3
            || region.source() != expected
            || expected == [0, 0]
            || region.header().name_count.load(Ordering::Relaxed) > MAX_NAMES as u64
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(region)
    }
    fn map(descriptor: Option<OwnedFd>) -> io::Result<Self> {
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MAX_REGION_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                if descriptor.is_some() {
                    libc::MAP_SHARED
                } else {
                    libc::MAP_PRIVATE | libc::MAP_ANON
                },
                descriptor.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            mapping: NonNull::new(address.cast())
                .ok_or_else(|| io::Error::other("null mapping"))?,
            descriptor,
        })
    }
    pub fn header(&self) -> &Header {
        unsafe { &*self.mapping.as_ptr().cast::<Header>() }
    }
    pub fn source(&self) -> [u64; 2] {
        self.header().source.each_ref().map(|n| n.load(Ordering::Relaxed))
    }
    pub fn descriptor(&self) -> io::Result<&OwnedFd> {
        self.descriptor.as_ref().ok_or_else(|| io::ErrorKind::Unsupported.into())
    }
}
impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mapping.as_ptr().cast(), MAX_REGION_BYTES);
        }
    }
}
