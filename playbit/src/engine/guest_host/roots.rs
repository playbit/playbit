//! Prepared root batches share the existing bulk lane and quota. A notice
//! invalidates reuse immediately; only a fully prepared batch reaches the VM.
use super::{Failure, GuestHost, HostError, HostResult, LaneWork, Shared, files};
use crate::{
    engine::{file_service, session_instance::Context},
    runtime::host::{
        AvailableVolume, VolumeRights,
        files::{
            Output, Root,
            capacity::{Capacity, Kind, Retention},
        },
    },
    store::Id,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

pub(super) struct Notify {
    queued: AtomicBool,
    lane: mpsc::SyncSender<LaneWork>,
}
impl Notify {
    fn wake(&self) {
        if !self.queued.swap(true, Ordering::AcqRel)
            && self.lane.try_send(LaneWork::RootCapacity).is_err()
        {
            self.queued.store(false, Ordering::Release);
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retirement {
    Open,
    Pending,
    Complete,
}
pub(super) struct Held {
    root: Root,
    source_epoch: [u8; 16],
    claimed: bool,
    revoked: bool,
    retirement: Retirement,
}
struct Batch {
    volumes: Vec<AvailableVolume>,
    handles: Vec<u64>,
    revision: Option<(Id, u64)>,
}
struct Preparing {
    id: [u8; 16],
    writable: bool,
    epoch: [u8; 16],
    backing: u32,
    revoked: bool,
}
#[derive(Default)]
pub(super) struct State {
    pub enabled: bool,
    restore_host_mounts: bool,
    pub capacity: Arc<Capacity>,
    pub entries: BTreeMap<u64, Held>,
    pending: Option<Vec<AvailableVolume>>,
    deferred: Option<Vec<AvailableVolume>>,
    retry_after_adoption: bool,
    notify: Weak<Notify>,
    queued: bool,
    ready: Option<Batch>,
    adopting: Option<Batch>,
    initial: Option<mpsc::SyncSender<HostResult<()>>>,
    preparing: Option<Preparing>,
    uncertain: Vec<crate::runtime::host::files::capacity::Credit>,
}
impl State {
    pub fn blocked(&self) -> bool {
        self.enabled
            && (self.pending.is_some()
                || self.queued
                || self.preparing.is_some()
                || self.adopting.is_some()
                || self.ready.is_some())
    }
}

pub(super) fn request(
    state: &mut super::State,
    lane: &mpsc::SyncSender<LaneWork>,
    volumes: Vec<AvailableVolume>,
) -> HostResult<()> {
    crate::runtime::syscall::validate_volumes(&volumes)?;
    for held in state.roots.entries.values_mut() {
        let current = volumes.iter().find(|volume| volume.id == held.root.volume);
        held.revoked |= current.is_none_or(|volume| {
            volume.backing != held.root.backing
                || volume.source_epoch != held.source_epoch
                || !volume.rights.contains(VolumeRights::READ)
                || volume.rights.contains(VolumeRights::WRITE) != held.root.writable
        });
    }
    if let Some(preparing) = &mut state.roots.preparing {
        preparing.revoked |= !volumes.iter().any(|volume| {
            volume.id == preparing.id
                && volume.backing == preparing.backing
                && volume.source_epoch == preparing.epoch
                && volume.rights.contains(VolumeRights::READ)
                && volume.rights.contains(VolumeRights::WRITE) == preparing.writable
        });
    }
    if !state.roots.queued {
        lane.try_send(LaneWork::Roots).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => HostError::LimitExceeded,
            mpsc::TrySendError::Disconnected(_) => HostError::Unavailable,
        })?;
        state.roots.queued = true;
    }
    state.roots.pending = Some(volumes);
    state.roots.deferred = None;
    // A newer notice fences an older prepared batch before guest delivery.
    state.volumes = None;
    state.roots.ready = None;
    Ok(())
}

pub(super) fn prepare(
    host: &mut GuestHost,
    volumes: Vec<AvailableVolume>,
    restore_host_mounts: bool,
) -> HostResult<Vec<AvailableVolume>> {
    let (send, receive) = mpsc::sync_channel(1);
    {
        let mut state = host.shared.state.lock().expect("guest completion lock poisoned");
        if state.roots.enabled || state.failure.is_some() || state.disposed {
            return Err(HostError::Conflict);
        }
        let notify = Arc::new(Notify { queued: AtomicBool::new(false), lane: host.bulk.clone() });
        let weak = Arc::downgrade(&notify);
        state.roots.notify = weak.clone();
        state.roots.capacity.on_release(Arc::new(move || {
            if let Some(notify) = weak.upgrade() {
                notify.wake();
            }
        }))?;
        host.root_notify = Some(notify);
        state.roots.enabled = true;
        state.roots.restore_host_mounts = restore_host_mounts;
        state.roots.initial = Some(send);
        let volumes = state.volumes.take().unwrap_or(volumes);
        request(&mut state, &host.bulk, volumes)?;
    }
    receive.recv().map_err(|_| HostError::Unavailable)??;
    let state = host.shared.state.lock().expect("guest completion lock poisoned");
    // Initialization pins its batch before signaling. Later notices may prepare
    // a successor, but cannot replace the batch being adopted by VM creation.
    state.roots.adopting.as_ref().map(|batch| batch.volumes.clone()).ok_or(HostError::Unavailable)
}

pub(super) fn take_volumes(state: &mut super::State) -> Option<Vec<AvailableVolume>> {
    if state.roots.adopting.is_some() || state.roots.pending.is_some() || state.roots.queued {
        return None;
    }
    if !state.roots.ready.as_ref().is_some_and(|batch| deliverable(batch, state.revision)) {
        return None;
    }
    let batch = state.roots.ready.take()?;
    state.volumes = None;
    let volumes = batch.volumes.clone();
    state.roots.adopting = Some(batch);
    Some(volumes)
}

pub(super) fn adopt(
    state: &mut super::State,
    volumes: &[AvailableVolume],
) -> HostResult<Vec<Root>> {
    if !state.roots.enabled {
        return Ok(Vec::new());
    }
    let batch = state.roots.adopting.as_ref().ok_or(HostError::WouldBlock)?;
    if batch.volumes != volumes {
        return Err(HostError::InvalidArgument);
    }
    let roots = batch
        .handles
        .iter()
        .map(|handle| {
            let held = state.roots.entries.get(handle).ok_or(HostError::Internal)?;
            Ok(held.root.clone())
        })
        .collect::<HostResult<Vec<_>>>()?;
    let notify = state.roots.notify.upgrade();
    // Reserve the follow-up before transferring ownership. A failed wake leaves
    // the pinned batch unclaimed and available for ordinary host cleanup.
    if state.roots.pending.is_some()
        && !state.roots.queued
        && let Some(notify) = &notify
    {
        notify.lane.try_send(LaneWork::Roots).map_err(|_| HostError::Unavailable)?;
        state.roots.queued = true;
    }
    for root in &roots {
        let held = state.roots.entries.get_mut(&root.engine).expect("preflighted root");
        held.claimed = true;
        held.root.replaces = None;
    }
    state.roots.adopting = None;
    if std::mem::take(&mut state.roots.retry_after_adoption)
        && let Some(notify) = notify
    {
        notify.wake();
    }
    Ok(roots)
}

pub(super) fn execute(profile: &Path, context: Context, shared: &Arc<Shared>, sequence: &mut u64) {
    let volumes = {
        let mut state = shared.state.lock().expect("guest completion lock poisoned");
        state.roots.queued = false;
        // A pinned batch may be adopted concurrently by the VM owner. Keep its
        // helpers intact until ownership is settled; adopt reschedules this work.
        if state.roots.adopting.is_some() {
            return;
        }
        let Some(volumes) = state.roots.pending.take() else { return };
        state.roots.ready = None;
        state.volumes = None;
        volumes
    };
    let result = build(profile, context, shared, sequence, volumes);
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    match result {
        Ok(batch) if !state.disposed && state.failure.is_none() => {
            if state.roots.pending.is_none() {
                state.roots.ready = Some(batch);
                settle(&mut state);
            }
        }
        result => {
            let error = result.err().unwrap_or(HostError::Unavailable);
            // Preserve the admission error before settling the failed context.
            // record_failure settles all waiters with a generic transport error.
            let initial = state.roots.initial.take();
            super::record_failure(&mut state, Failure::Exchange);
            if let Some(initial) = initial {
                let _ = initial.send(Err(error));
            }
        }
    }
    // Unclaimed superseded roots have no VM owner. Retire them on this lane,
    // retaining their credits through the cleanup acknowledgment.
    let unused: Vec<_> = state
        .roots
        .entries
        .iter()
        .filter_map(|(&handle, held)| {
            let selected =
                state.roots.ready.as_ref().is_some_and(|batch| batch.handles.contains(&handle))
                    || state
                        .roots
                        .adopting
                        .as_ref()
                        .is_some_and(|batch| batch.handles.contains(&handle));
            (!held.claimed && !selected && held.retirement != Retirement::Pending).then_some(handle)
        })
        .collect();
    let retired: Vec<_> = unused
        .into_iter()
        .map(|handle| state.roots.entries.remove(&handle).expect("selected root"))
        .collect();
    drop(state);
    for held in retired {
        if held.retirement == Retirement::Open {
            files::retire_root(profile, context, shared, sequence, held.root);
        }
    }
    (shared.wake)();
}

fn build(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    volumes: Vec<AvailableVolume>,
) -> HostResult<Batch> {
    let mut batch = Building { handles: Vec::with_capacity(volumes.len()), ..Default::default() };
    for volume in &volumes {
        build_root(profile, context, shared, sequence, volume, &mut batch)?;
    }
    let Building { handles, fence, omitted, unavailable } = batch;
    let volumes = if omitted.is_empty() {
        volumes
    } else {
        let mut state = shared.state.lock().expect("guest completion lock poisoned");
        if state.roots.pending.is_none() {
            state.roots.deferred = Some(volumes.clone());
        }
        volumes.into_iter().filter(|volume| !omitted.contains(&volume.id)).collect()
    };
    let volumes = volumes.into_iter().filter(|volume| !unavailable.contains(&volume.id)).collect();
    Ok(Batch { volumes, handles, revision: fence })
}

#[derive(Default)]
struct Building {
    handles: Vec<u64>,
    fence: Option<(Id, u64)>,
    omitted: Vec<[u8; 16]>,
    unavailable: Vec<[u8; 16]>,
}
fn build_root(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    volume: &AvailableVolume,
    batch: &mut Building,
) -> HostResult<()> {
    let writable = volume.rights.contains(VolumeRights::WRITE);
    let (existing, capacity) = existing_root(shared, volume, writable)?;
    if let Some(handle) = existing {
        batch.handles.push(handle);
        return Ok(());
    }
    if !volume.rights.contains(VolumeRights::READ) {
        return Err(HostError::AccessDenied);
    }
    let Some((credit, replaces)) =
        reserve_root(profile, context, shared, sequence, &capacity, volume.is_root)?
    else {
        batch.omitted.push(volume.id);
        return Ok(());
    };
    let grant = *Id::generate().map_err(|_| HostError::Unavailable)?.as_bytes();
    shared.state.lock().expect("guest completion lock poisoned").roots.preparing =
        Some(Preparing {
            id: volume.id,
            writable,
            epoch: volume.source_epoch,
            backing: volume.backing,
            revoked: false,
        });
    let (result, revision, failed) = files::exchange(
        profile,
        context,
        sequence,
        if volume.backing == playbit_api::abi::data::VOLUME_BUNDLE {
            file_service::Request::BundleRoot
        } else if volume.backing == playbit_api::abi::data::VOLUME_HOST_FS {
            file_service::Request::HostRoot { mount: Id::from_bytes(volume.id) }
        } else {
            file_service::Request::VolumeRoot { volume: Id::from_bytes(volume.id) }
        },
    );
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    if failed
        || matches!((revision, state.revision), (Some((epoch, _)), Some((current, _))) if epoch != current)
    {
        super::record_failure(&mut state, Failure::Exchange);
    }
    let revoked = state.roots.preparing.take().expect("one root creation on the bulk lane").revoked;
    batch.observe_revision(&mut state, revision);
    let Some((handle, credit)) = received_root(&mut state, volume, batch, result, failed, credit)?
    else {
        return Ok(());
    };
    let root = Root {
        replaces,
        backing: volume.backing,
        volume: volume.id,
        grant,
        engine: handle,
        writable,
        credit,
    };
    register_root(&mut state, root, volume.source_epoch, revoked)?;
    batch.handles.push(handle);
    if state.failure.is_some() {
        return Err(HostError::Unavailable);
    }
    Ok(())
}

fn received_root(
    state: &mut super::State,
    volume: &AvailableVolume,
    batch: &mut Building,
    result: HostResult<Output>,
    failed: bool,
    credit: crate::runtime::host::files::capacity::Credit,
) -> HostResult<Option<(u64, crate::runtime::host::files::capacity::Credit)>> {
    let handle = match result {
        Ok(Output::File { handle }) => handle,
        Err(
            error @ (HostError::NotFound
            | HostError::AccessDenied
            | HostError::InvalidArgument
            | HostError::Unavailable),
        ) if !failed
            && state.failure.is_none()
            && state.roots.restore_host_mounts
            && volume.backing == playbit_api::abi::data::VOLUME_HOST_FS =>
        {
            // A saved path that cannot be reopened grants no capability.
            // Keep its configuration, never search or substitute a parent.
            eprintln!("Saved host folder {:?} is unavailable: {error:?}", volume.name);
            batch.unavailable.push(volume.id);
            return Ok(None);
        }
        result => {
            if failed {
                // A lost root reply may hide a live backend resource. Its
                // reservation cannot be reused before context retirement.
                state.roots.uncertain.push(credit);
            }
            return Err(result.err().unwrap_or(HostError::Internal));
        }
    };
    Ok(Some((handle, credit)))
}

fn existing_root(
    shared: &Arc<Shared>,
    volume: &AvailableVolume,
    writable: bool,
) -> HostResult<(Option<u64>, Arc<Capacity>)> {
    let result = {
        let state = shared.state.lock().expect("guest completion lock poisoned");
        if state.disposed || state.failure.is_some() {
            return Err(HostError::Unavailable);
        }
        (
            state
                .roots
                .entries
                .values()
                .find(|held| {
                    !held.revoked
                        && held.retirement == Retirement::Open
                        && held.root.volume == volume.id
                        && held.root.backing == volume.backing
                        && held.source_epoch == volume.source_epoch
                        && held.root.writable == writable
                })
                .map(|held| held.root.engine),
            state.roots.capacity.clone(),
        )
    };
    Ok(result)
}

fn reserve_root(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    capacity: &Arc<Capacity>,
    root: bool,
) -> HostResult<Option<(crate::runtime::host::files::capacity::Credit, Option<u64>)>> {
    let reserved = match capacity.one(Kind::File) {
        Ok(credit) => (credit, None),
        Err(HostError::LimitExceeded) => {
            // A revoked helper can be retired before its replacement is
            // admitted. Reuse its reservation only after the engine proves
            // retirement; old aliases retain separate credits and IDs.
            let previous = {
                let mut state = shared.state.lock().expect("guest completion lock poisoned");
                let Some(held) = state
                    .roots
                    .entries
                    .values_mut()
                    .find(|held| held.revoked && held.retirement == Retirement::Open)
                else {
                    if root {
                        return Err(HostError::LimitExceeded);
                    }
                    return Ok(None);
                };
                held.retirement = Retirement::Pending;
                held.root.clone()
            };
            files::cleanup_exchange(profile, context, sequence, previous.engine, false)?;
            let claimed = {
                let mut state = shared.state.lock().expect("guest completion lock poisoned");
                state.roots.entries.get_mut(&previous.engine).is_some_and(|held| {
                    held.retirement = Retirement::Complete;
                    held.claimed
                })
            };
            let replaces = if claimed { Some(previous.engine) } else { previous.replaces };
            (previous.credit, replaces)
        }
        Err(error) => return Err(error),
    };
    Ok(Some(reserved))
}

fn register_root(
    state: &mut super::State,
    root: Root,
    source_epoch: [u8; 16],
    revoked: bool,
) -> HostResult<()> {
    let handle = root.engine;
    if state.roots.entries.contains_key(&handle) {
        super::record_failure(state, Failure::Exchange);
        return Err(HostError::Internal);
    }
    state.roots.entries.insert(
        handle,
        Held { root, source_epoch, claimed: false, revoked, retirement: Retirement::Open },
    );
    Ok(())
}

impl Building {
    fn observe_revision(&mut self, state: &mut super::State, revision: Option<(Id, u64)>) {
        if let Some((epoch, after)) = revision {
            match self.fence {
                Some((before_epoch, before)) if before_epoch == epoch => {
                    self.fence = Some((epoch, before.max(after)))
                }
                None => self.fence = Some((epoch, after)),
                _ => super::record_failure(state, Failure::Exchange),
            }
        }
    }
}

/// Retry a capacity-limited addition only after an actual reservation retires.
/// One coalesced control slot shares the bulk FIFO; there is no freshness timer.
pub(super) fn retry(profile: &Path, context: Context, shared: &Arc<Shared>, sequence: &mut u64) {
    {
        let mut state = shared.state.lock().expect("guest completion lock poisoned");
        if let Some(notify) = state.roots.notify.upgrade() {
            notify.queued.store(false, Ordering::Release);
        }
        if state.disposed
            || state.failure.is_some()
            || state.roots.pending.is_some()
            || state.roots.queued
        {
            return;
        }
        if state.roots.adopting.is_some() {
            state.roots.retry_after_adoption |= state.roots.deferred.is_some();
            return;
        }
        let Some(volumes) = state.roots.deferred.take() else { return };
        state.roots.pending = Some(volumes);
    }
    execute(profile, context, shared, sequence);
}

fn deliverable(batch: &Batch, current: Option<(Id, u64)>) -> bool {
    batch.revision.is_none_or(|(epoch, after)| {
        current.is_some_and(|(current_epoch, applied)| epoch == current_epoch && after <= applied)
    })
}
/// Called after a bulk completion or an event fence advances. No I/O or waits.
pub(super) fn settle(state: &mut super::State) {
    if state.failure.is_some() || state.disposed {
        if let Some(initial) = state.roots.initial.take() {
            let _ = initial.send(Err(HostError::Unavailable));
        }
        return;
    }
    if state.roots.pending.is_some()
        || state.roots.queued
        || !state.roots.ready.as_ref().is_some_and(|batch| deliverable(batch, state.revision))
    {
        return;
    }
    if let Some(initial) = state.roots.initial.take() {
        state.roots.adopting = state.roots.ready.take();
        let _ = initial.send(Ok(()));
    } else {
        state.volumes = state.roots.ready.as_ref().map(|batch| batch.volumes.clone());
    }
}

/// The bulk lane exclusively owns a started root retirement. Releasing the VM
/// reference must not replay it; pending work retains its reservation here.
pub(super) fn discard_retiring(state: &mut super::State, file: u64) -> bool {
    let Some(held) = state.roots.entries.get_mut(&file) else { return false };
    if held.retirement == Retirement::Open {
        return false;
    }
    held.claimed = false;
    if held.retirement == Retirement::Complete {
        state.roots.entries.remove(&file);
    }
    true
}

pub(super) fn dispose(state: &mut super::State, lane: &mpsc::SyncSender<LaneWork>) {
    state.roots.pending = None;
    state.roots.deferred = None;
    state.roots.ready = None;
    state.roots.adopting = None;
    let roots = std::mem::take(&mut state.roots.entries);
    for (_, held) in roots {
        if !held.claimed && held.retirement == Retirement::Open {
            let _ = files::enqueue_cleanup(
                state,
                lane,
                held.root.engine,
                false,
                Retention { source: Some(held.root.credit), result: None },
            );
        }
    }
}

pub(super) const OPERATIONS: &[crate::runtime::syscall::Opcode] = &[
    crate::runtime::syscall::Opcode::ChannelOpen,
    crate::runtime::syscall::Opcode::ChannelBegin,
    crate::runtime::syscall::Opcode::ChannelTake,
    // The same data operations plus the complete owned File surface.
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
    crate::runtime::syscall::Opcode::BloblogAppendBegin,
    crate::runtime::syscall::Opcode::BloblogAppendTake,
    crate::runtime::syscall::Opcode::BloblogReadBegin,
    crate::runtime::syscall::Opcode::BloblogReadTake,
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert coherent root ownership and bounded cleanup.")]
mod tests {
    use super::*;
    fn volume(write: bool) -> AvailableVolume {
        AvailableVolume {
            role: 0,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [1; 16],
            name: "Root".into(),
            tags: vec![],
            rights: if write { VolumeRights::ALL } else { VolumeRights::READ },
            is_root: true,
        }
    }
    fn retain(state: &mut super::super::State, handle: u64, claimed: bool) {
        let root = Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: [1; 16],
            grant: [handle as u8; 16],
            engine: handle,
            writable: true,
            credit: state.roots.capacity.one(Kind::File).unwrap(),
        };
        state.roots.entries.insert(
            handle,
            Held {
                root,
                source_epoch: [0; 16],
                claimed,
                revoked: false,
                retirement: Retirement::Open,
            },
        );
    }
    #[test]
    fn coalesced_removal_and_rights_changes_never_reactivate_cached_roots() {
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        retain(&mut state, 1, true);
        let (send, receive) = mpsc::sync_channel(1);
        let mut renamed = volume(true);
        renamed.name = "Renamed".into();
        request(&mut state, &send, vec![renamed]).unwrap();
        assert!(!state.roots.entries[&1].revoked);
        let mut other = volume(true);
        other.id = [2; 16];
        request(&mut state, &send, vec![other]).unwrap();
        request(&mut state, &send, vec![volume(true)]).unwrap();
        assert!(state.roots.entries[&1].revoked);
        assert!(matches!(receive.try_recv(), Ok(LaneWork::Roots)));
        assert!(receive.try_recv().is_err(), "one coalesced refresh slot");
        retain(&mut state, 2, true);
        state.roots.preparing = Some(Preparing {
            id: [1; 16],
            writable: true,
            epoch: [0; 16],
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            revoked: false,
        });
        request(&mut state, &send, vec![volume(false)]).unwrap();
        request(&mut state, &send, vec![volume(true)]).unwrap();
        assert!(state.roots.entries[&2].revoked);
        assert!(state.roots.preparing.as_ref().unwrap().revoked);
        retain(&mut state, 3, true);
        let mut restored = volume(true);
        restored.source_epoch = [7; 16];
        request(&mut state, &send, vec![restored]).unwrap();
        assert!(state.roots.entries[&3].revoked, "coalesced regrant changes incarnation");
    }
    #[test]
    fn root_adoption_waits_for_fence_and_pins_a_coherent_batch() {
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        retain(&mut state, 1, false);
        let epoch = Id::from_bytes([3; 16]);
        state.revision = Some((epoch, 1));
        state.roots.ready = Some(Batch {
            volumes: vec![volume(true)],
            handles: vec![1],
            revision: Some((epoch, 2)),
        });
        settle(&mut state);
        assert!(take_volumes(&mut state).is_none());
        state.revision = Some((epoch, 2));
        settle(&mut state);
        let volumes = take_volumes(&mut state).unwrap();
        let (send, _receive) = mpsc::sync_channel(1);
        request(&mut state, &send, vec![volume(false)]).unwrap();
        let roots = adopt(&mut state, &volumes).unwrap();
        assert_eq!(roots[0].engine, 1);
        assert!(state.roots.entries[&1].claimed);
        assert!(state.roots.entries[&1].revoked);
        assert!(state.roots.blocked(), "new access waits for the successor");
    }
    #[test]
    fn a_pinned_batch_defers_backend_retirement_until_vm_adoption() {
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        retain(&mut state, 1, false);
        state.roots.adopting =
            Some(Batch { volumes: vec![volume(true)], handles: vec![1], revision: None });
        let (send, receive) = mpsc::sync_channel(2);
        let notify = Arc::new(Notify { queued: AtomicBool::new(false), lane: send.clone() });
        state.roots.notify = Arc::downgrade(&notify);
        request(&mut state, &send, vec![volume(false)]).unwrap();
        assert!(matches!(receive.try_recv(), Ok(LaneWork::Roots)));
        let shared =
            Arc::new(Shared { state: std::sync::Mutex::new(state), wake: Arc::new(|| {}) });
        let context = Context {
            instance: Id::from_bytes([1; 16]),
            handle: Id::from_bytes([2; 16]),
            incarnation: Id::from_bytes([3; 16]),
            owner_generation: 1,
        };
        let mut sequence = 0;
        execute(Path::new("/nonexistent-file-api-test-profile"), context, &shared, &mut sequence);
        let mut state = shared.state.lock().unwrap();
        assert_eq!(sequence, 0, "no cleanup or allocation while adoption is pinned");
        assert!(state.failure.is_none());
        assert!(state.roots.entries[&1].retirement == Retirement::Open);
        assert!(state.roots.pending.is_some());
        assert!(!state.roots.queued);
        let roots = adopt(&mut state, &[volume(true)]).unwrap();
        assert_eq!(roots[0].engine, 1);
        assert!(state.roots.entries[&1].claimed);
        assert!(state.roots.queued);
        assert!(matches!(receive.try_recv(), Ok(LaneWork::Roots)));
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn adoption_does_not_poll_deferred_capacity_and_failed_wakes_keep_ownership() {
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        retain(&mut state, 1, false);
        state.roots.adopting =
            Some(Batch { volumes: vec![volume(true)], handles: vec![1], revision: None });
        let (send, receive) = mpsc::sync_channel(1);
        let notify = Arc::new(Notify { queued: AtomicBool::new(false), lane: send.clone() });
        state.roots.notify = Arc::downgrade(&notify);
        state.roots.pending = Some(vec![volume(false)]);
        send.send(LaneWork::RootCapacity).unwrap();
        assert!(adopt(&mut state, &[volume(true)]).is_err());
        assert!(!state.roots.entries[&1].claimed);
        assert!(state.roots.adopting.is_some());
        assert!(matches!(receive.try_recv(), Ok(LaneWork::RootCapacity)));
        state.roots.pending = None;
        state.roots.deferred = Some(vec![volume(true)]);
        adopt(&mut state, &[volume(true)]).unwrap();
        assert!(receive.try_recv().is_err(), "adoption is not a capacity release");
    }

    #[test]
    fn redundant_work_preserves_ready_batches_and_real_release_waits_for_adoption() {
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        retain(&mut state, 1, false);
        state.roots.ready =
            Some(Batch { volumes: vec![volume(true)], handles: vec![1], revision: None });
        let (send, receive) = mpsc::sync_channel(1);
        let notify = Arc::new(Notify { queued: AtomicBool::new(false), lane: send });
        state.roots.notify = Arc::downgrade(&notify);
        let shared =
            Arc::new(Shared { state: std::sync::Mutex::new(state), wake: Arc::new(|| {}) });
        let context = Context {
            instance: Id::from_bytes([1; 16]),
            handle: Id::from_bytes([2; 16]),
            incarnation: Id::from_bytes([3; 16]),
            owner_generation: 1,
        };
        let mut sequence = 0;
        execute(Path::new("/nonexistent-file-api-test-profile"), context, &shared, &mut sequence);
        let mut state = shared.state.lock().unwrap();
        assert!(state.roots.ready.is_some());
        let volumes = take_volumes(&mut state).unwrap();
        state.roots.deferred = Some(vec![volume(false)]);
        drop(state);
        retry(Path::new("/nonexistent-file-api-test-profile"), context, &shared, &mut sequence);
        let mut state = shared.state.lock().unwrap();
        assert!(state.roots.retry_after_adoption);
        adopt(&mut state, &volumes).unwrap();
        assert!(matches!(receive.try_recv(), Ok(LaneWork::RootCapacity)));
        assert!(receive.try_recv().is_err());
        assert!(!state.roots.retry_after_adoption);
        assert_eq!(sequence, 0);
    }

    #[test]
    fn dropping_prepared_but_unclaimed_roots_queues_reserved_cleanup() {
        let mut state = super::super::State::default();
        retain(&mut state, 1, false);
        let capacity = state.roots.capacity.clone();
        let rest = capacity.reserve(Kind::File, 511).unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        dispose(&mut state, &send);
        assert!(capacity.one(Kind::File).is_err());
        assert!(matches!(receive.try_recv(), Ok(LaneWork::FileCleanup(_))));
        assert!(capacity.one(Kind::File).is_ok());
        drop(rest);
    }
    #[test]
    fn vm_disposal_during_root_retirement_keeps_credit_until_acknowledged() {
        let mut state = super::super::State::default();
        retain(&mut state, 1, true);
        let capacity = state.roots.capacity.clone();
        let full = capacity.reserve(Kind::File, 511).unwrap();
        let held = state.roots.entries.get_mut(&1).unwrap();
        held.retirement = Retirement::Pending;
        let source = held.root.credit.clone();
        let (send, receive) = mpsc::sync_channel(1);
        files::discard_locked(
            &mut state,
            &send,
            1,
            false,
            Retention { source: Some(source), result: None },
        )
        .unwrap();
        assert!(!state.roots.entries[&1].claimed);
        assert!(receive.try_recv().is_err());
        assert!(capacity.one(Kind::File).is_err(), "pending retirement still owns its credit");
        state.roots.entries.get_mut(&1).unwrap().retirement = Retirement::Complete;
        files::discard_locked(&mut state, &send, 1, false, Retention::default()).unwrap();
        assert!(!state.roots.entries.contains_key(&1));
        assert!(capacity.one(Kind::File).is_ok());
        assert!(receive.try_recv().is_err());
        drop(full);
    }
    #[test]
    fn full_quota_replacement_waits_for_retirement_and_reuses_only_its_credit() {
        use crate::engine::{ArchivedRequest, Request, ipc, reply::Reply};
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("control")).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(
            directory.path().join(crate::paths::ENGINE_SOCKET),
        )
        .unwrap();
        let context = Context {
            instance: Id::from_bytes([1; 16]),
            handle: Id::from_bytes([2; 16]),
            incarnation: Id::from_bytes([3; 16]),
            owner_generation: 1,
        };
        let server = std::thread::spawn(move || {
            for operation in 1..=3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let bytes = ipc::read(&mut stream).unwrap();
                let archived = ipc::codec::access::<ArchivedRequest>(&bytes).unwrap();
                let Request::FileCall { context: actual, operation: sequence, request } =
                    rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(archived).unwrap()
                else {
                    panic!("File exchange")
                };
                assert_eq!(actual, context);
                assert_eq!(sequence, operation);
                let output = if operation == 1 {
                    assert!(matches!(
                        *request,
                        file_service::Request::Operation(crate::runtime::host::files::Call {
                            file: 1,
                            action: crate::runtime::host::files::Action::Close { flags: 2 }
                        })
                    ));
                    Output::Close { error: 0, publication: 0, recovery: None }
                } else {
                    assert!(matches!(*request, file_service::Request::VolumeRoot { .. }));
                    Output::File { handle: operation }
                };
                let reply = Reply::FileCall(file_service::Outcome::result(
                    context,
                    sequence,
                    request.kind(),
                    Ok(output),
                    Some((context.incarnation, 1)),
                ));
                ipc::write(&mut stream, &ipc::codec::encode(&reply, 4096).unwrap()).unwrap();
            }
        });
        let mut state = super::super::State::default();
        state.roots.enabled = true;
        state.revision = Some((context.incarnation, 1));
        retain(&mut state, 1, true);
        state.roots.entries.get_mut(&1).unwrap().revoked = true;
        let original = state.roots.entries[&1].root.credit.clone();
        let capacity = state.roots.capacity.clone();
        let mut table = crate::runtime::file_handles::Table::with_capacity(1, capacity.clone());
        let mut caps = crate::runtime::capabilities::Capabilities::<()>::new();
        table
            .replace_roots(&caps, &[volume(true)], &[state.roots.entries[&1].root.clone()], true)
            .unwrap();
        let alias = table.enumerate(&mut caps, &[[1; 16]]).unwrap()[0];
        let old_resource = table.file(&caps, alias).unwrap().clone();
        let mut aliases = capacity.reserve(Kind::File, 510).unwrap();
        let shared =
            Arc::new(Shared { state: std::sync::Mutex::new(state), wake: Arc::new(|| {}) });
        let mut sequence = 0;
        let batch =
            build(directory.path(), context, &shared, &mut sequence, vec![volume(false)]).unwrap();
        assert_eq!(batch.handles, [2]);
        let state = shared.state.lock().unwrap();
        assert!(state.failure.is_none());
        assert!(state.roots.entries[&1].retirement == Retirement::Complete);
        assert!(state.roots.entries[&2].root.credit.same(&original));
        assert!(capacity.one(Kind::File).is_err());
        drop(state);
        let mut state = shared.state.lock().unwrap();
        state.roots.adopting = Some(batch);
        let admitted = adopt(&mut state, &[volume(false)]).unwrap();
        assert_eq!(admitted[0].replaces, Some(1));
        assert_eq!(state.roots.entries[&2].root.replaces, None);
        drop(state);
        let retired = table.replace_roots(&caps, &[volume(false)], &admitted, true).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(table.authorize_resource(&old_resource, false), Err(HostError::AccessDenied));
        let (cleanup_send, cleanup_receive) = mpsc::sync_channel(1);
        for resource in retired {
            files::discard_locked(
                &mut shared.state.lock().unwrap(),
                &cleanup_send,
                resource.engine.unwrap(),
                false,
                Retention { source: Some(resource.credit), result: None },
            )
            .unwrap();
        }
        assert!(cleanup_receive.try_recv().is_err(), "acknowledged retirement is never sent twice");
        assert!(!shared.state.lock().unwrap().roots.entries.contains_key(&1));
        let mut added = volume(true);
        added.id = [2; 16];
        added.is_root = false;
        let batch =
            build(directory.path(), context, &shared, &mut sequence, vec![volume(false), added])
                .unwrap();
        assert_eq!(batch.volumes.len(), 1, "capacity pressure leaves existing grants usable");
        assert_eq!(sequence, 2, "a deferred root performs no backend allocation");
        let (send, receive) = mpsc::sync_channel(1);
        let notify = Arc::new(Notify { queued: AtomicBool::new(false), lane: send });
        let weak = Arc::downgrade(&notify);
        shared.state.lock().unwrap().roots.notify = weak.clone();
        capacity
            .on_release(Arc::new(move || {
                if let Some(notify) = weak.upgrade() {
                    notify.wake();
                }
            }))
            .unwrap();
        drop(aliases.pop());
        drop(aliases.pop());
        assert!(matches!(receive.try_recv(), Ok(LaneWork::RootCapacity)));
        assert!(receive.try_recv().is_err(), "capacity release coalesces to one control slot");
        retry(directory.path(), context, &shared, &mut sequence);
        let state = shared.state.lock().unwrap();
        assert!(state.failure.is_none());
        assert!(state.roots.deferred.is_none());
        assert_eq!(state.roots.ready.as_ref().unwrap().handles, [2, 3]);
        assert_eq!(state.volumes.as_ref().unwrap().len(), 2);
        drop(state);
        drop(aliases);
        server.join().unwrap();
    }
}
