//! Owned File adapter for a native resident. Callback methods only admit or
//! move bounded memory; the shared owner executes and retires backend resources.
use crate::{
    engine::{
        Services,
        blob::BlobTransport,
        files::{
            authority::Authority,
            executor::{self, Abandoned, Retirement, Ticket},
            owner::Owner,
        },
    },
    runtime::host::{
        AvailableVolume, HostResult, VolumeRights,
        files::{
            Call, Output, Root,
            capacity::{Capacity, Credit, Kind, Retention},
        },
    },
    store::{Access, Id, LocalStore},
};
use playbit_api::abi::syscall::Error;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct Entry {
    bytes: usize,
    generation: u64,
    ticket: Ticket,
    result: Option<HostResult<Arc<Output>>>,
    retention: Retention,
    notified: bool,
    close: bool,
    recovery_cleanup: Option<Retirement>,
}
enum Disposal {
    Cleanup(Retirement),
    Abandoned(Abandoned),
}
struct Retiring {
    bytes: usize,
    disposal: Disposal,
    _retention: Retention,
}
struct Held {
    root: Root,
    epoch: [u8; 16],
    authority: Arc<Authority>,
    claimed: bool,
}
pub(super) struct Adapter {
    owner: Arc<Owner>,
    id: Id,
    store: Arc<LocalStore>,
    access: Access,
    session: Option<String>,
    transport: BlobTransport,
    pub capacity: Arc<Capacity>,
    roots: BTreeMap<[u8; 16], Held>,
    prepared: Vec<AvailableVolume>,
    deferred: Option<Vec<AvailableVolume>>,
    capacity_released: Arc<AtomicBool>,
    progress: Arc<AtomicBool>,
    completions: std::sync::Weak<std::sync::Mutex<super::State>>,
    retiring: Vec<Retiring>,
    cleanup_completed: bool,
    generation: Option<u64>,
    failed: bool,
}
impl Adapter {
    pub fn new(
        services: &Services,
        access: Access,
        session: Option<String>,
        transport: Option<BlobTransport>,
        wake: Arc<dyn Fn() + Send + Sync>,
        completions: std::sync::Weak<std::sync::Mutex<super::State>>,
    ) -> HostResult<Self> {
        let owner = services.file_owner().map_err(|_| Error::Unavailable)?;
        let id = Id::generate().map_err(crate::runtime::store_bridge::error)?;
        let progress = Arc::new(AtomicBool::new(false));
        let advanced = Arc::downgrade(&progress);
        let progress_wake = wake.clone();
        owner.register(
            id,
            Arc::new(move || {
                if let Some(advanced) = advanced.upgrade() {
                    advanced.store(true, Ordering::Release);
                    progress_wake();
                }
            }),
        )?;
        let capacity = Arc::new(Capacity::default());
        let capacity_released = Arc::new(AtomicBool::new(false));
        let released = Arc::downgrade(&capacity_released);
        capacity.on_release(Arc::new(move || {
            if let Some(released) = released.upgrade()
                && !released.swap(true, Ordering::AcqRel)
            {
                wake();
            }
        }))?;
        Ok(Self {
            transport: transport.unwrap_or_else(|| owner.transport.clone()),
            owner,
            id,
            store: services.store.clone(),
            access,
            session,
            capacity,
            roots: BTreeMap::new(),
            prepared: vec![],
            deferred: None,
            capacity_released,
            progress,
            completions,
            retiring: vec![],
            cleanup_completed: false,
            generation: None,
            failed: false,
        })
    }
    /// Volume metadata was admitted by the session binding. Creating its root
    /// capability performs no filesystem work or pending-operation admission.
    pub fn prepare(&mut self, volumes: &[AvailableVolume]) -> HostResult<Vec<AvailableVolume>> {
        if !self.prepared.is_empty() {
            return Err(Error::Conflict);
        }
        self.request_roots(volumes)
    }
    pub fn request_roots(
        &mut self,
        volumes: &[AvailableVolume],
    ) -> HostResult<Vec<AvailableVolume>> {
        crate::runtime::syscall::validate_volumes(volumes)?;
        if volumes.iter().any(|volume| {
            !matches!(
                volume.backing,
                playbit_api::abi::data::VOLUME_PLAYBIT | playbit_api::abi::data::VOLUME_BUNDLE
            )
        }) {
            return Err(Error::Unsupported);
        }
        if volumes.iter().any(|volume| !volume.rights.contains(VolumeRights::READ)) {
            return Err(Error::AccessDenied);
        }
        if self.failed {
            return Err(Error::Unavailable);
        }
        for (id, held) in &self.roots {
            if !volumes.iter().any(|volume| volume.id == *id && same_grant(volume, held)) {
                held.authority.revoke().map_err(|_| Error::Internal)?;
            }
        }
        let unused: Vec<_> = self
            .roots
            .iter()
            .filter_map(|(id, held)| {
                (!held.claimed && held.authority.check(false).is_err()).then_some(*id)
            })
            .collect();
        for id in unused {
            let old = self.roots.remove(&id).expect("selected unused root");
            self.discard(
                old.root.engine,
                false,
                Retention { source: Some(old.root.credit), result: None },
            )?;
        }
        self.deferred = Some(volumes.to_vec());
        self.advance_roots()?;
        Ok(self.prepared.clone())
    }
    fn advance_roots(&mut self) -> HostResult<()> {
        self.capacity_released.store(false, Ordering::Release);
        self.progress.store(false, Ordering::Release);
        self.collect();
        let Some(volumes) = self.deferred.clone() else { return Ok(()) };
        for volume in &volumes {
            if self
                .roots
                .get(&volume.id)
                .is_some_and(|held| same_grant(volume, held) && held.authority.check(false).is_ok())
            {
                continue;
            }
            let credit = match self.capacity.one(Kind::File) {
                Ok(credit) => credit,
                Err(Error::LimitExceeded) if !volume.is_root => continue,
                Err(error) => return Err(error),
            };
            let held = self.admit_root(volume, credit)?;
            if let Some(old) = self.roots.insert(volume.id, held)
                && !old.claimed
            {
                self.discard(
                    old.root.engine,
                    false,
                    Retention { source: Some(old.root.credit), result: None },
                )?;
            }
        }
        self.prepared = volumes
            .iter()
            .filter(|volume| {
                self.roots.get(&volume.id).is_some_and(|held| {
                    same_grant(volume, held) && held.authority.check(false).is_ok()
                })
            })
            .cloned()
            .collect();
        crate::runtime::syscall::validate_volumes(&self.prepared)?;
        if self.prepared == volumes {
            self.deferred = None;
        }
        Ok(())
    }
    fn admit_root(&self, volume: &AvailableVolume, credit: Credit) -> HostResult<Held> {
        let grant = *Id::generate().map_err(crate::runtime::store_bridge::error)?.as_bytes();
        let writable = volume.rights.contains(VolumeRights::WRITE);
        let authority = Arc::new(Authority::new(writable));
        let bundle = if volume.backing == playbit_api::abi::data::VOLUME_BUNDLE {
            let session = self
                .store
                .session(self.session.as_ref().ok_or(Error::Unsupported)?)
                .map_err(crate::runtime::store_bridge::error)?;
            let path = self
                .store
                .profile_path()
                .join(crate::paths::BUILDS)
                .join(&session.resolved_build_id);
            let package =
                crate::package::Package::open(&path).map_err(|_| Error::InvalidArgument)?;
            if package.id().map_err(|_| Error::Internal)? != session.resolved_build_id {
                return Err(Error::InvalidArgument);
            }
            Some(package)
        } else {
            None
        };
        let engine = self.owner.with(|executor| {
            if let Some(bundle) = bundle {
                return executor.admit_bundle_root(self.id, bundle, authority.clone());
            }
            executor.admit_volume_root(
                self.id,
                executor::VolumeRoot {
                    store: self.store.clone(),
                    access: self.access.clone().with_personal_incarnation(
                        Id::from_bytes(volume.id),
                        Id::from_bytes(volume.source_epoch),
                    ),
                    volume: Id::from_bytes(volume.id),
                    session: self.session.clone(),
                    transport: self.transport.clone(),
                    authority: authority.clone(),
                    placement: executor::PlacementSource::Managed(self.owner.placement.clone()),
                },
            )
        })?;
        let root = Root {
            replaces: None,
            backing: volume.backing,
            volume: volume.id,
            grant,
            engine,
            writable,
            credit,
        };
        Ok(Held { root, epoch: volume.source_epoch, authority, claimed: false })
    }
    pub fn retry_roots(&mut self) -> HostResult<Option<Vec<AvailableVolume>>> {
        if self.deferred.is_none()
            || !(self.capacity_released.load(Ordering::Acquire)
                || self.progress.load(Ordering::Acquire))
        {
            return Ok(None);
        }
        let previous = self.prepared.clone();
        self.advance_roots()?;
        Ok((previous != self.prepared).then(|| self.prepared.clone()))
    }
    pub fn roots(&mut self, volumes: &[AvailableVolume]) -> HostResult<Vec<Root>> {
        if self.failed {
            return Err(Error::Unavailable);
        }
        if self.prepared != volumes {
            return Err(Error::WouldBlock);
        }
        let roots = volumes
            .iter()
            .map(|volume| {
                self.roots.get(&volume.id).map(|held| held.root.clone()).ok_or(Error::Internal)
            })
            .collect::<HostResult<Vec<_>>>()?;
        for root in &roots {
            let held = self.roots.get_mut(&root.volume).expect("preflighted File root");
            held.claimed = true;
            held.root.replaces = None;
        }
        Ok(roots)
    }
    pub fn usage(&mut self) -> (usize, usize) {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let state = shared.lock().expect("local completion lock is not poisoned");
        self.collect();
        (
            state.entries.count(2)
                + self
                    .retiring
                    .iter()
                    .filter(|entry| matches!(entry.disposal, Disposal::Abandoned(_)))
                    .count(),
            state.file_entries().map(|entry| entry.bytes).sum::<usize>()
                + self.retiring.iter().map(|entry| entry.bytes).sum::<usize>(),
        )
    }
    pub fn begin(&mut self, generation: u64, call: &Call, retention: Retention) -> HostResult<u64> {
        let shared = self.completions.upgrade().ok_or(Error::Unavailable)?;
        let mut state = shared.lock().map_err(|_| Error::Internal)?;
        self.collect();
        if self.failed {
            return Err(Error::Unavailable);
        }
        if generation == 0 || self.generation.is_some_and(|current| current != generation) {
            return Err(Error::StaleHandle);
        }
        if state.entries.count(2) >= 64 {
            return Err(Error::LimitExceeded);
        }
        call.action.validate()?;
        let bytes = call.action.retained_bytes()?.checked_mul(2).ok_or(Error::LimitExceeded)?;
        let handle = state.entries.reserve(2, crate::capability::Rights::NONE)?;
        let ticket = match self.owner.with(|executor| executor.begin(self.id, call.clone())) {
            Ok(ticket) => ticket,
            Err(error) => {
                state.entries.cancel(handle).expect("rejected File retains reservation");
                return Err(error);
            }
        };
        let token = handle.raw();
        self.generation = Some(generation);
        state.generation = Some(generation);
        state
            .entries
            .insert(
                handle,
                super::Completion::File(Box::new(Entry {
                    bytes,
                    generation,
                    ticket,
                    result: None,
                    retention,
                    notified: false,
                    close: matches!(call.action, crate::runtime::host::files::Action::Close { .. }),
                    recovery_cleanup: None,
                })),
            )
            .expect("accepted File retains reservation");
        Ok(token)
    }
    pub fn peek(&mut self, token: u64) -> HostResult<Arc<Output>> {
        let shared = self.completions.upgrade().ok_or(Error::Unavailable)?;
        let mut state = shared.lock().map_err(|_| Error::Internal)?;
        let entry = state.file_entry_mut(token).ok_or(Error::StaleHandle)?;
        if entry.result.is_none() {
            let result = self.owner.with(|executor| executor.take(&entry.ticket));
            if matches!(result, Err(Error::WouldBlock)) {
                return Err(Error::WouldBlock);
            }
            entry.result = Some(result.map(Arc::new));
        }
        if entry.result.as_ref().is_some_and(Result::is_ok) && !entry.ticket.delivery_current() {
            if !entry.close {
                return Err(Error::Unavailable);
            }
            if let Some(Ok(output)) = &entry.result
                && let Output::Close { error, publication, recovery: Some(handle) } =
                    output.as_ref()
            {
                let cleanup = self.owner.with(|executor| executor.discard(self.id, *handle))?;
                entry.result = Some(Ok(Arc::new(Output::Close {
                    error: *error,
                    publication: *publication,
                    recovery: None,
                })));
                entry.recovery_cleanup = Some(cleanup);
            }
        }
        if let Some(cleanup) = &entry.recovery_cleanup {
            if !cleanup.ready() {
                entry.notified = false;
                return Err(Error::WouldBlock);
            }
            entry.recovery_cleanup = None;
        }
        entry.result.as_ref().expect("completed File operation").clone()
    }
    pub fn release(&mut self, token: u64, claimed: bool) {
        let Some(shared) = self.completions.upgrade() else { return };
        let mut state = shared.lock().expect("local completion lock is not poisoned");
        if state.file_entry_mut(token).is_none() {
            return;
        }
        let Some(super::Completion::File(entry)) = state.entries.take(&token) else {
            unreachable!("validated File completion")
        };
        drop(state);
        let entry = *entry;
        let resource = if claimed {
            None
        } else {
            match entry.result.as_ref() {
                Some(Ok(output)) => match output.as_ref() {
                    Output::File { handle } => Some((*handle, false)),
                    Output::List { handle } => Some((*handle, true)),
                    Output::Close { recovery: Some(handle), .. } => Some((*handle, false)),
                    _ => None,
                },
                _ => None,
            }
        };
        if let Some(cleanup) = entry.recovery_cleanup {
            self.retiring.push(Retiring {
                bytes: 0,
                disposal: Disposal::Cleanup(cleanup),
                _retention: entry.retention,
            });
        } else if let Some((handle, list)) = resource {
            if self.discard(handle, list, entry.retention.clone()).is_err() {
                self.failed = true;
                self.retiring.push(Retiring {
                    bytes: entry.bytes,
                    disposal: Disposal::Abandoned(entry.ticket.abandon()),
                    _retention: entry.retention,
                });
            }
        } else if entry.result.as_ref().is_none_or(Result::is_err) {
            self.retiring.push(Retiring {
                bytes: entry.bytes,
                disposal: Disposal::Abandoned(entry.ticket.abandon()),
                _retention: entry.retention,
            });
        }
        self.collect();
    }
    pub fn discard(&mut self, file: u64, list: bool, retention: Retention) -> HostResult<()> {
        self.collect();
        // Each cleanup consumes a distinct previously admitted resource.
        if self.retiring.len() >= 512 + 8 + 64 {
            return Err(Error::LimitExceeded);
        }
        let cleanup = self.owner.with(|executor| {
            if list { executor.close_list(self.id, file) } else { executor.discard(self.id, file) }
        })?;
        self.roots.retain(|_, held| held.root.engine != file);
        self.retiring.push(Retiring {
            bytes: 0,
            disposal: Disposal::Cleanup(cleanup),
            _retention: retention,
        });
        Ok(())
    }
    fn collect(&mut self) {
        let before = self.retiring.len();
        self.retiring.retain(|retiring| match &retiring.disposal {
            Disposal::Abandoned(work) => !work.ready(),
            Disposal::Cleanup(retirement) => !retirement.ready(),
        });
        self.cleanup_completed |= self.retiring.len() != before;
    }
    pub fn ready(&mut self) -> Vec<(u64, u64, bool)> {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let mut state = shared.lock().expect("local completion lock is not poisoned");
        self.collect();
        self.cleanup_completed = false;
        state
            .file_iter_mut()
            .filter_map(|(token, entry)| {
                if entry.notified || !entry.ticket.ready() {
                    return None;
                }
                entry.notified = true;
                Some((entry.generation, token, false))
            })
            .collect()
    }
    pub fn has_ready(&self) -> bool {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let state = shared.lock().expect("local completion lock is not poisoned");
        if self.deferred.is_some()
            && (self.capacity_released.load(Ordering::Acquire)
                || self.progress.load(Ordering::Acquire))
        {
            return true;
        }
        self.cleanup_completed
            || self.retiring.iter().any(|retiring| match &retiring.disposal {
                Disposal::Abandoned(work) => work.ready(),
                Disposal::Cleanup(retirement) => retirement.ready(),
            })
            || state.file_entries().any(|entry| !entry.notified && entry.ticket.ready())
    }
    pub fn defer(&mut self, token: u64) {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let mut state = shared.lock().expect("local completion lock is not poisoned");
        if let Some(entry) = state.file_entry_mut(token) {
            entry.notified = false;
        }
    }
    pub fn idle(&mut self) -> bool {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let state = shared.lock().expect("local completion lock is not poisoned");
        self.collect();
        state.entries.count(2) == 0 && self.retiring.is_empty()
    }
    pub fn waiting(&self) -> bool {
        let shared = self.completions.upgrade().expect("File adapter retains its completion owner");
        let state = shared.lock().expect("local completion lock is not poisoned");
        state.file_entries().any(|entry| !entry.ticket.ready()) || !self.retiring.is_empty()
    }
}
fn same_grant(volume: &AvailableVolume, held: &Held) -> bool {
    volume.source_epoch == held.epoch
        && volume.rights.contains(VolumeRights::READ)
        && volume.rights.contains(VolumeRights::WRITE) == held.root.writable
}
impl Drop for Adapter {
    fn drop(&mut self) {
        let mut retention = Vec::new();
        if let Some(shared) = self.completions.upgrade() {
            let mut state = shared.lock().expect("local completion lock is not poisoned");
            let ids = state
                .entries
                .iter()
                .filter_map(|(id, entry)| matches!(entry, super::Completion::File(_)).then_some(id))
                .collect::<Vec<_>>();
            for id in ids {
                let Some(super::Completion::File(entry)) = state.entries.take(&id) else {
                    unreachable!("owned File completion")
                };
                retention.push(entry.retention);
            }
        }
        retention
            .extend(std::mem::take(&mut self.retiring).into_iter().map(|entry| entry._retention));
        retention.extend(
            std::mem::take(&mut self.roots)
                .into_values()
                .map(|held| Retention { source: Some(held.root.credit), result: None }),
        );
        self.owner.retire(self.id, retention);
    }
}

pub(super) const OPERATIONS: &[crate::runtime::syscall::Opcode] = &[
    crate::runtime::syscall::Opcode::ChannelOpen,
    crate::runtime::syscall::Opcode::ChannelBegin,
    crate::runtime::syscall::Opcode::ChannelTake,
    // The same data operations plus the complete owned File surface.
    crate::runtime::syscall::Opcode::BloblogAppendBegin,
    crate::runtime::syscall::Opcode::BloblogAppendTake,
    crate::runtime::syscall::Opcode::BloblogReadBegin,
    crate::runtime::syscall::Opcode::BloblogReadTake,
    crate::runtime::syscall::Opcode::KvKeysBegin,
    crate::runtime::syscall::Opcode::KvKeysTake,
    crate::runtime::syscall::Opcode::KvReadBegin,
    crate::runtime::syscall::Opcode::KvReadTake,
    crate::runtime::syscall::Opcode::KvPutBegin,
    crate::runtime::syscall::Opcode::KvPutTake,
    crate::runtime::syscall::Opcode::KvDeleteBegin,
    crate::runtime::syscall::Opcode::KvDeleteTake,
    crate::runtime::syscall::Opcode::EntryResolveBegin,
    crate::runtime::syscall::Opcode::EntryResolveTake,
    crate::runtime::syscall::Opcode::SchemaReadBegin,
    crate::runtime::syscall::Opcode::SchemaReadTake,
    crate::runtime::syscall::Opcode::SchemaEstablishBegin,
    crate::runtime::syscall::Opcode::SchemaEstablishTake,
    crate::runtime::syscall::Opcode::ObjectCreateBegin,
    crate::runtime::syscall::Opcode::ObjectCreatePoll,
    crate::runtime::syscall::Opcode::ObjectReadBegin,
    crate::runtime::syscall::Opcode::ObjectReadTake,
    crate::runtime::syscall::Opcode::ObjectEditBegin,
    crate::runtime::syscall::Opcode::ObjectEditTake,
    crate::runtime::syscall::Opcode::ObjectQueryBegin,
    crate::runtime::syscall::Opcode::ObjectQueryTake,
    crate::runtime::syscall::Opcode::FileReadBegin,
    crate::runtime::syscall::Opcode::FileReadTake,
    crate::runtime::syscall::Opcode::FileWriteBegin,
    crate::runtime::syscall::Opcode::FileWriteTake,
    crate::runtime::syscall::Opcode::FileOpenBegin,
    crate::runtime::syscall::Opcode::FileOpenTake,
    crate::runtime::syscall::Opcode::FileStatusBegin,
    crate::runtime::syscall::Opcode::FileStatusTake,
    crate::runtime::syscall::Opcode::FileCloseBegin,
    crate::runtime::syscall::Opcode::FileCloseTake,
    crate::runtime::syscall::Opcode::FileListOpenBegin,
    crate::runtime::syscall::Opcode::FileListOpenTake,
    crate::runtime::syscall::Opcode::FileListNextBegin,
    crate::runtime::syscall::Opcode::FileListNextTake,
    crate::runtime::syscall::Opcode::FileListClose,
    crate::runtime::syscall::Opcode::OperationDispose,
];

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests retain real native File work across blocked workers and cleanup."
)]
mod tests {
    use super::*;
    use crate::{
        engine::{data_host::DataHost, in_process_host::InProcessHost},
        runtime::host::{DataCall, DataOperation, Host},
    };
    use playbit_api::abi::files::*;
    use std::{
        sync::{Condvar, Mutex, mpsc},
        time::Duration,
    };

    fn take(host: &mut InProcessHost, token: u64, wake: &mpsc::Receiver<()>) -> Arc<Output> {
        loop {
            match host.peek_file(token) {
                Ok(output) => {
                    host.release_file(token, true);
                    return output;
                }
                Err(Error::WouldBlock) => {
                    wake.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                Err(error) => panic!("File operation failed: {error:?}"),
            }
        }
    }
    #[test]
    fn blocked_files_keep_disposed_call_quota_and_do_not_block_root_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let services = Services::open(directory.path(), || {}).unwrap();
        let access = Access::owner(services.store.account().id);
        let root = services.store.create_volume("Root", false).unwrap().id;
        let other = services.store.create_volume("Other", false).unwrap().id;
        let volume = AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: *root.as_bytes(),
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        };
        let (send, wake) = mpsc::sync_channel(1);
        let (mut host, inbox) = InProcessHost::local(
            DataHost {
                store: services.store.clone(),
                access: access.clone(),
                changed: Arc::new(AtomicBool::new(false)),
            },
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        let volumes = host
            .enable_files(&services, access.clone(), None, None, std::slice::from_ref(&volume))
            .unwrap();
        let roots = host.file_roots(&volumes).unwrap();
        let root_handle = roots[0].engine;
        let mut writers = Vec::new();
        for name in [b"one", b"two"] {
            let token = host
                .begin_file(
                    1,
                    Arc::new(Call {
                        file: root_handle,
                        action: crate::runtime::host::files::Action::Open {
                            path: name.to_vec(),
                            mode: FILE_WRITE,
                        },
                    }),
                    Retention::default(),
                )
                .unwrap();
            let output = take(&mut host, token, &wake);
            let Output::File { handle } = output.as_ref() else { panic!("writer") };
            let writer = *handle;
            let write = host
                .begin_file(
                    1,
                    Arc::new(Call {
                        file: writer,
                        action: crate::runtime::host::files::Action::Write {
                            offset: 0,
                            bytes: name.to_vec(),
                        },
                    }),
                    Retention::default(),
                )
                .unwrap();
            assert!(matches!(take(&mut host, write, &wake).as_ref(), Output::Written));
            writers.push(writer);
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        struct ReleaseGate(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                if let Ok(mut open) = self.0.0.lock() {
                    *open = true;
                }
                self.0.1.notify_all();
            }
        }
        let _release = ReleaseGate(gate.clone());
        let (entered, entries) = mpsc::channel();
        let held = gate.clone();
        services
            .store
            .set_blob_publication_hook(Some(Arc::new(move || {
                entered.send(()).unwrap();
                let (lock, changed) = &*held;
                drop(changed.wait_while(lock.lock().unwrap(), |open| !*open).unwrap());
                Ok(())
            })))
            .unwrap();
        let mut closes = Vec::new();
        for file in writers {
            closes.push(
                host.begin_file(
                    1,
                    Arc::new(Call {
                        file,
                        action: crate::runtime::host::files::Action::Close { flags: 0 },
                    }),
                    Retention::default(),
                )
                .unwrap(),
            );
            // Isolate the worker scheduling boundary from separate optimistic
            // blob-cache transaction contention during concurrent preparation.
            if entries.recv_timeout(Duration::from_secs(5)).is_err() {
                panic!(
                    "workers did not reach publication: {:?}",
                    closes.iter().map(|token| host.peek_file(*token)).collect::<Vec<_>>()
                );
            }
        }
        for _ in 0..62 {
            let token = host
                .begin_file(
                    1,
                    Arc::new(Call {
                        file: root_handle,
                        action: crate::runtime::host::files::Action::Status,
                    }),
                    Retention::default(),
                )
                .unwrap();
            host.release_file(token, false);
        }
        assert_eq!(
            host.begin_data(
                1,
                Arc::new(DataCall {
                    volume: *root.as_bytes(),
                    operation: DataOperation::ResolveEntry { name: "one".into() }
                })
            ),
            Err(Error::LimitExceeded)
        );
        let mut added = volume.clone();
        added.id = *other.as_bytes();
        added.name = "Other".into();
        added.is_root = false;
        let refresh = inbox.clone();
        let opening_services = services.clone();
        let opening_access = access.clone();
        let (finished, refreshed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut second, _) = InProcessHost::local(
                DataHost {
                    store: opening_services.store.clone(),
                    access: opening_access.clone(),
                    changed: Arc::new(AtomicBool::new(false)),
                },
                Arc::new(|| {}),
            )
            .unwrap();
            let initial = second.enable_files(
                &opening_services,
                opening_access,
                None,
                None,
                std::slice::from_ref(&volume),
            );
            let result = refresh.prepare_files(&[volume, added]);
            finished.send((initial, result, second)).unwrap();
        });
        let result = refreshed.recv_timeout(Duration::from_secs(1));
        // Always release workers before asserting, so a regression fails instead
        // of trapping teardown behind the intentionally held publication hook.
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        worker.join().unwrap();
        let (initial, admitted, second) =
            result.expect("initial or refreshed roots waited for blocked File workers");
        assert_eq!(initial.unwrap().len(), 1);
        let admitted = admitted.unwrap();
        assert_eq!(admitted.len(), 2);
        host.file_roots(&admitted).unwrap();
        drop(second);
        for token in closes {
            assert!(matches!(
                take(&mut host, token, &wake).as_ref(),
                Output::Close { error: 0, publication: 1, .. }
            ));
        }
        assert!(inbox.retry_file_roots().unwrap().is_none());
        assert!(services.store.resolve_entry(&access, root, "one").is_ok());
        assert!(services.store.resolve_entry(&access, root, "two").is_ok());
        services.store.set_blob_publication_hook(None).unwrap();
    }

    #[test]
    fn destruction_is_ready_once_without_a_guest_token() {
        for inspect_idle_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let services = Services::open(directory.path(), || {}).unwrap();
            let volume = services.store.create_volume("Cleanup", false).unwrap();
            let (wake, receive) = mpsc::channel();
            let completions = Arc::new(std::sync::Mutex::new(super::super::State::default()));
            let mut adapter = Adapter::new(
                &services,
                Access::owner(services.store.account().id),
                None,
                None,
                Arc::new(move || {
                    let _ = wake.send(());
                }),
                Arc::downgrade(&completions),
            )
            .unwrap();
            let volumes = adapter
                .prepare(&[AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id: *volume.id.as_bytes(),
                    name: "Cleanup".into(),
                    tags: vec!["root".into()],
                    rights: VolumeRights::ALL,
                    is_root: true,
                    role: 0,
                }])
                .unwrap();
            let root = adapter.roots(&volumes).unwrap().pop().unwrap();
            adapter
                .discard(root.engine, false, Retention { source: Some(root.credit), result: None })
                .unwrap();
            while !adapter.has_ready() {
                receive.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if inspect_idle_first {
                assert!(adapter.idle());
                assert!(adapter.has_ready(), "collecting cleanup must preserve its notification");
            }
            assert!(adapter.ready().is_empty(), "destruction creates no guest event");
            assert!(adapter.idle());
            assert!(!adapter.has_ready(), "acknowledged destruction must not busy-loop");
        }
    }
}
