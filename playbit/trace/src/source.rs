//! Process bootstrap owns allocation and attachment. Emission uses thread-local
//! queue ownership and fixed atomic storage, without waiting or allocating.
use crate::{notify, queue, region::Region};
use std::{
    cell::Cell,
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
};
pub static SOURCE: OnceLock<Source> = OnceLock::new();
include!(concat!(env!("OUT_DIR"), "/catalog.rs"));
pub struct Source {
    pub region: Arc<Region>,
    names: Vec<&'static str>,
    pid: u32,
    leases: crate::lease::Bank,
    sockets: Mutex<[Option<UnixStream>; 2]>,
    profile: OnceLock<PathBuf>,
}
#[derive(Clone, Copy)]
struct Producer {
    queue: usize,
    tid: u32,
    last: u64,
}
struct Local(Cell<Producer>);
thread_local! {static LOCAL:Local=const{Local(Cell::new(Producer{queue:usize::MAX,tid:0,last:0}))};}
impl Drop for Local {
    fn drop(&mut self) {
        if let Some(source) = SOURCE.get().filter(|s| s.pid == std::process::id()) {
            let p = self.0.get();
            if p.queue < queue::MAX_PRODUCERS {
                source.region.header().queues[p.queue].state.store(2, Ordering::Release);
                source.wake();
            }
        }
    }
}
impl Source {
    pub fn create(kind: u64, extra: &[&'static str]) -> io::Result<Self> {
        if extra.len() > crate::region::MAX_NAMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace name catalog exceeds capacity",
            ));
        }
        let mut names = Vec::with_capacity(crate::region::MAX_NAMES);
        names.extend_from_slice(NATIVE_NAMES);
        for name in extra {
            if !names.contains(name) {
                if names.len() == crate::region::MAX_NAMES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trace name catalog exceeds capacity",
                    ));
                }
                names.push(name);
            }
        }
        names.sort_unstable();
        let region = Arc::new(Region::create(kind, &names)?);
        Ok(Self {
            region,
            names,
            pid: std::process::id(),
            leases: crate::lease::Bank::new(),
            sockets: Mutex::new([None, None]),
            profile: OnceLock::new(),
        })
    }
    pub fn generation(&self) -> u64 {
        if self.pid != std::process::id()
            || self.region.header().disabled.load(Ordering::Acquire) != 0
        {
            return 0;
        }
        self.region.header().recording.load(Ordering::Acquire)
    }
    pub fn name(&self, name: &str) -> Option<u16> {
        self.names.binary_search(&name).ok().map(|n| n as u16)
    }
    pub fn bind_profile(&self, profile: &Path) -> io::Result<()> {
        if profile.as_os_str().len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace profile path exceeds 4096 bytes",
            ));
        }
        if self.profile.get_or_init(|| profile.to_owned()) != profile {
            let header = self.region.header();
            header.disabled.store(1, Ordering::Release);
            header.recording.store(0, Ordering::Release);
            for app in &header.apps {
                app.selection.store(2, Ordering::Release);
                app.generation.store(0, Ordering::Release);
            }
            self.wake();
            return Err(io::Error::other(
                "trace source cannot change profiles; restart to record another profile",
            ));
        }
        Ok(())
    }
    pub fn attach(&self, socket: UnixStream) -> io::Result<()> {
        let mut sockets =
            self.sockets.lock().map_err(|_| io::Error::other("trace attachment poisoned"))?;
        let generation = self.leases.prepare().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "trace attachment still used")
        })?;
        let fd = socket.as_raw_fd();
        sockets[generation as usize & 1] = Some(socket);
        self.leases.publish(generation, fd);
        self.wake();
        Ok(())
    }
    pub fn wake(&self) {
        self.region.header().activity.fetch_add(1, Ordering::Release);
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.region.header().sleeping.load(Ordering::SeqCst) != 0
            && let Some(lease) = self.leases.pin()
        {
            let _ = notify::send(lease.descriptor());
        }
    }
    pub fn timestamp(&self) -> Option<(u32, u64)> {
        LOCAL.with(|local| {
            let mut p = local.0.get();
            if p.tid == 0 {
                let tid = self
                    .region
                    .header()
                    .next_tid
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |tid| {
                        (tid != 0 && tid <= u64::from(u32::MAX)).then(|| tid + 1)
                    })
                    .ok()?;
                p.tid = tid as u32;
            }
            let time = playbit_api::abi::trace::make_timestamp(&mut p.last, crate::clock_ns())?;
            local.0.set(p);
            Some((p.tid, time))
        })
    }
    pub fn emit(&self, scope: u32, tid: u32, generation: u64, event: &[u8]) -> bool {
        if self.pid != std::process::id()
            || self.region.header().disabled.load(Ordering::Acquire) != 0
        {
            return false;
        }
        let header = self.region.header();
        let effective = if scope == 0 {
            header.recording.load(Ordering::Acquire)
        } else {
            header
                .apps
                .iter()
                .find(|app| {
                    app.scope.load(Ordering::Acquire) == u64::from(scope)
                        && app.state.load(Ordering::Acquire) == 2
                })
                .map_or(0, |app| app.effective_generation(header.recording.load(Ordering::Acquire)))
        };
        if effective == 0 || effective != generation {
            header.lost.fetch_add(1, Ordering::Relaxed);
            self.wake();
            return false;
        }
        let result = LOCAL.with(|local| {
            let mut p = local.0.get();
            if p.queue == usize::MAX {
                let Some(index) = self.region.header().queues.iter().position(|q| {
                    q.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok()
                }) else {
                    return false;
                };
                p.queue = index;
                local.0.set(p);
            }
            let mut prefix = [0; 16];
            prefix[..4].copy_from_slice(&scope.to_le_bytes());
            prefix[4..8].copy_from_slice(&tid.to_le_bytes());
            prefix[8..].copy_from_slice(&generation.to_le_bytes());
            self.region.header().queues[p.queue].push(&prefix, event)
        });
        if !result {
            self.region.header().lost.fetch_add(1, Ordering::Relaxed);
        }
        self.wake();
        result
    }
}
pub fn initialize(kind: u64, extra: &[&'static str]) -> io::Result<()> {
    if SOURCE.get().is_some() {
        return Ok(());
    }
    let _ = crate::clock_ns();
    SOURCE
        .set(Source::create(kind, extra)?)
        .map_err(|_| io::Error::other("trace initialized concurrently"))
}

// Charge catalog capacity, per-thread producer state, profile path and the one
// bounded temporary attachment stack in addition to the shared mapping.
pub const PRIVATE_BYTES: usize = std::mem::size_of::<Source>()
    + crate::region::MAX_NAMES * std::mem::size_of::<&str>()
    + queue::MAX_PRODUCERS * std::mem::size_of::<Local>()
    + 4096
    + 64 * 1024;
