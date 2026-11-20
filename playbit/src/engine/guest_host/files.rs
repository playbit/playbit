//! File exchanges share the bulk lane. One FIFO preserves admission order,
//! including destructor cleanup, without serializing interactive data reads.
use super::{Failure, GuestHost, HostError, HostResult, LaneWork, Ready, Shared};
use crate::{
    engine::{file_service, ipc, reply::Reply, session_instance::Context},
    runtime::{
        host::files::{
            Action, Call, Output,
            capacity::{MAX_FILES, Retention},
        },
        syscall::EventKind,
    },
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, mpsc},
};

// Engine per-instance File and FileList ceilings. Cleanup has its own reserved
// channel slots: exhausting ordinary operation credit cannot prevent destruction.
pub(super) const MAX_CLEANUPS: usize = 512 + 8;
pub(super) struct Entry {
    generation: u64,
    retention: Retention,
    close: bool,
    pub(super) bytes: usize,
    revision: Option<(crate::store::Id, u64)>,
    result: Option<HostResult<Arc<Output>>>,
    abandoned: bool,
    notified: bool,
}
pub(super) struct Cleanup {
    file: u64,
    list: bool,
    _retention: Retention,
}
// Acknowledged discard removes the engine writer. Keep its failed logical
// handle and source reservation until ordered Close/dispose consumes it.
struct FailedWriter {
    error: HostError,
    _retention: Retention,
}
#[derive(Default)]
pub(super) struct State {
    cleanup: usize,
    cleanup_completed: bool,
    failed_cleanup: Vec<Cleanup>,
    failed_writers: BTreeMap<u64, FailedWriter>,
}
pub(super) fn bytes(state: &super::State) -> usize {
    state.file_entries().map(|(_, entry)| entry.bytes).sum()
}
pub(super) fn executing(state: &super::State) -> bool {
    state.files.cleanup > state.files.failed_cleanup.len()
        || state.file_entries().any(|(_, entry)| entry.result.is_none())
}
pub(super) fn idle(state: &super::State) -> bool {
    state.entries.count(2) == 0 && state.files.cleanup == 0
}
fn available(entry: &Entry, state: &super::State) -> bool {
    if state.failure.is_some() && !entry.close {
        return true;
    }
    match &entry.result {
        None => false,
        Some(Err(_)) => true,
        Some(Ok(output))
            if entry.close && (state.failure.is_some() || capability(output).is_none()) =>
        {
            true
        }
        Some(Ok(_)) => matches!((entry.revision, state.revision),
            (Some((epoch, before)), Some((applied_epoch, applied))) if epoch == applied_epoch && before <= applied),
    }
}
pub(super) fn has_ready(state: &super::State) -> bool {
    state.files.cleanup_completed
        || state
            .file_entries()
            .map(|(_, entry)| entry)
            .any(|entry| !entry.notified && !entry.abandoned && available(entry, state))
}
pub(super) fn waiting(state: &super::State) -> bool {
    state.files.cleanup != 0
        || state.file_entries().map(|(_, entry)| entry).any(|entry| !available(entry, state))
}
pub(super) fn ready(state: &mut super::State, output: &mut Vec<Ready>) {
    // Destruction has no guest token, but the owner must observe its completion
    // once to settle the last frame. Do not synthesize a guest callback.
    state.files.cleanup_completed = false;
    let tokens: Vec<_> = state
        .file_entries()
        .filter_map(|(token, entry)| {
            (!entry.notified && !entry.abandoned && available(entry, state)).then_some(token)
        })
        .collect();
    for token in tokens {
        let entry = state.file_entry_mut(token).expect("selected retained completion");
        entry.notified = true;
        output.push(Ready { generation: entry.generation, token, kind: EventKind::DataReady });
    }
}
pub(super) fn defer(state: &mut super::State, ready: Ready) {
    if let Some(entry) = state.file_entry_mut(ready.token)
        && entry.generation == ready.generation
    {
        entry.notified = false;
    }
}
pub(super) fn begin(
    host: &GuestHost,
    generation: u64,
    call: Arc<Call>,
    retention: Retention,
) -> HostResult<u64> {
    let bytes = call.action.retained_bytes()?.checked_mul(3).ok_or(HostError::LimitExceeded)?;
    let mut state = host.shared.state.lock().expect("guest completion lock poisoned");
    if state.failure.is_some() || state.disposed {
        return Err(HostError::Unavailable);
    }
    if generation == 0 || state.generation.is_some_and(|current| current != generation) {
        return Err(HostError::StaleHandle);
    }
    if !super::admits(state.entries.len(), state.bytes, bytes) {
        return Err(HostError::LimitExceeded);
    }
    let handle = state.entries.reserve(2, crate::capability::Rights::NONE)?;
    let token = handle.raw();
    let close = matches!(call.action, Action::Close { .. });
    let sent = host.bulk.try_send(LaneWork::File { token, call }).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => HostError::LimitExceeded,
        mpsc::TrySendError::Disconnected(_) => HostError::Unavailable,
    });
    if let Err(error) = sent {
        state.entries.cancel(handle).expect("unsent file operation retains reservation");
        return Err(error);
    }
    state.generation = Some(generation);
    state.bytes += bytes;
    state
        .entries
        .insert(
            handle,
            super::Completion::File(Box::new(Entry {
                generation,
                retention,
                close,
                bytes,
                revision: None,
                result: None,
                abandoned: false,
                notified: false,
            })),
        )
        .expect("sent file operation retains reservation");
    Ok(token)
}
pub(super) fn peek(host: &GuestHost, token: u64) -> HostResult<Arc<Output>> {
    let mut state = host.shared.state.lock().expect("guest completion lock poisoned");
    let entry = state.file_entry(token).ok_or(HostError::StaleHandle)?;
    if entry.abandoned {
        return Err(HostError::StaleHandle);
    }
    if state.failure.is_some() && !entry.close {
        return Err(HostError::AccessDenied);
    }
    if !available(entry, &state) {
        return Err(HostError::WouldBlock);
    }
    // Mount metadata must be adopted before copying data or transferring access.
    if (state.volumes.is_some() || state.roots.blocked())
        && state.failure.is_none()
        && (!entry.close
            || entry
                .result
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|output| capability(output).is_some()))
    {
        state.file_entry_mut(token).expect("retained File operation").notified = false;
        return Err(HostError::WouldBlock);
    }
    let result = entry.result.as_ref().cloned().unwrap_or(Err(HostError::WouldBlock))?;
    let stale = entry.revision.is_none_or(|(_, revision)| revision < state.data_revision);
    if stale && !entry.close {
        return Err(HostError::Unavailable);
    }
    if (state.failure.is_some() || stale)
        && let Output::Close { error, publication, recovery: Some(_) } = result.as_ref()
    {
        return Ok(Arc::new(Output::Close {
            error: *error,
            publication: *publication,
            recovery: None,
        }));
    }
    Ok(result)
}
fn capability(output: &Output) -> Option<(u64, bool)> {
    match output {
        Output::File { handle } | Output::Close { recovery: Some(handle), .. } => {
            Some((*handle, false))
        }
        Output::List { handle } => Some((*handle, true)),
        _ => None,
    }
}
// Retain one failed cleanup per engine capability until context retirement.
// Aliases never reach this lane. Repeated destructor attempts share the same
// capability credit; the first retained request already owns that reservation.
fn retain_failed_cleanup(state: &mut super::State, cleanup: Cleanup, counted: bool) {
    if state
        .files
        .failed_cleanup
        .iter()
        .any(|prior| prior.file == cleanup.file && prior.list == cleanup.list)
    {
        if counted {
            state.files.cleanup -= 1;
        }
    } else {
        assert!(
            state.files.failed_cleanup.len() < MAX_CLEANUPS + super::MAX_CALLS,
            "failed cleanup is bounded by admitted resources and pending close outcomes"
        );
        if !counted {
            state.files.cleanup += 1;
        }
        state.files.failed_cleanup.push(cleanup);
    }
    super::record_failure(state, Failure::Exchange);
}
pub(super) fn enqueue_cleanup(
    state: &mut super::State,
    lane: &mpsc::SyncSender<LaneWork>,
    file: u64,
    list: bool,
    retention: Retention,
) -> HostResult<()> {
    if file == 0 {
        return Err(HostError::StaleHandle);
    }
    let cleanup = Cleanup { file, list, _retention: retention };
    if state.files.cleanup >= MAX_CLEANUPS {
        retain_failed_cleanup(state, cleanup, false);
        return Err(HostError::LimitExceeded);
    }
    if state.files.failed_cleanup.iter().any(|prior| prior.file == file && prior.list == list) {
        return Err(HostError::Unavailable);
    }
    match lane.try_send(LaneWork::FileCleanup(cleanup)) {
        Ok(()) => {
            state.files.cleanup += 1;
            Ok(())
        }
        Err(error) => {
            let failure = match &error {
                mpsc::TrySendError::Full(_) => HostError::LimitExceeded,
                mpsc::TrySendError::Disconnected(_) => HostError::Unavailable,
            };
            let work = match error {
                mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work) => work,
            };
            let LaneWork::FileCleanup(cleanup) = work else { unreachable!("queued cleanup") };
            retain_failed_cleanup(state, cleanup, false);
            Err(failure)
        }
    }
}

pub(super) fn discard(
    host: &GuestHost,
    file: u64,
    list: bool,
    retention: Retention,
) -> HostResult<()> {
    let mut state = host.shared.state.lock().expect("guest completion lock poisoned");
    discard_locked(&mut state, &host.bulk, file, list, retention)
}
pub(super) fn discard_locked(
    state: &mut super::State,
    lane: &mpsc::SyncSender<LaneWork>,
    file: u64,
    list: bool,
    retention: Retention,
) -> HostResult<()> {
    if !list && super::roots::discard_retiring(state, file) {
        return Ok(());
    }
    let result = enqueue_cleanup(state, lane, file, list, retention);
    if result.is_ok() {
        state.roots.entries.remove(&file);
    }
    result
}
pub(super) fn release(host: &GuestHost, token: u64, claimed: bool) {
    let mut state = host.shared.state.lock().expect("guest completion lock poisoned");
    release_locked(&mut state, &host.bulk, token, claimed);
}
fn release_locked(
    state: &mut super::State,
    lane: &mpsc::SyncSender<LaneWork>,
    token: u64,
    claimed: bool,
) {
    let Some(entry) = state.file_entry_mut(token) else { return };
    if entry.result.is_none() {
        entry.abandoned = true;
        return;
    }
    let retention = entry.retention.clone();
    let cleanup = entry
        .result
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|output| capability(output));
    if (!claimed || state.failure.is_some())
        && let Some((file, list)) = cleanup
        && enqueue_cleanup(state, lane, file, list, retention).is_err()
    {
        // Preserve the retained capability/credit until owner retirement. Never
        // pretend an unacknowledged close was undone or replay a failed exchange.
        super::record_failure(state, Failure::Exchange);
        state.file_entry_mut(token).expect("retained cleanup").abandoned = true;
        return;
    }
    remove(state, token);
}
fn remove(state: &mut super::State, token: u64) {
    if state.file_entry(token).is_some() {
        let Some(super::Completion::File(entry)) = state.entries.take(&token) else {
            unreachable!("validated file completion")
        };
        state.bytes -= entry.bytes;
    }
}
pub(super) fn dispose(state: &mut super::State, lane: &mpsc::SyncSender<LaneWork>) {
    let tokens: Vec<_> = state.file_entries().map(|(id, _)| id).collect();
    for token in tokens {
        release_locked(state, lane, token, false);
    }
}
pub(super) fn exchange(
    profile: &Path,
    context: Context,
    sequence: &mut u64,
    request: file_service::Request,
) -> (HostResult<Output>, Option<(crate::store::Id, u64)>, bool) {
    let Some(operation) = sequence.checked_add(1) else {
        return (Err(HostError::LimitExceeded), None, false);
    };
    *sequence = operation;
    let wire = crate::engine::Request::FileCall { context, operation, request: Box::new(request) };
    let response = ipc::call_archive(profile, &wire).and_then(|response| response.into_owned());
    let Ok(Reply::FileCall(outcome)) = response else {
        return (Err(HostError::Unavailable), None, true);
    };
    let revision = outcome.revision;
    let crate::engine::Request::FileCall { request, .. } = &wire else { unreachable!() };
    let result = outcome.take(context, operation, request);
    let needs_fence =
        result.as_ref().is_ok_and(|output| !matches!(output, Output::Close { recovery: None, .. }));
    // Keep a validated close receipt even when its recovery capability lacks a
    // fence. Freezing delivery withholds and cleans up that capability.
    let failed = matches!(result, Err(HostError::Internal)) || needs_fence && revision.is_none();
    (result, revision, failed)
}
pub(super) fn execute(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    token: u64,
    call: Arc<Call>,
) {
    // Unlike new admission, already accepted File work survives ordinary
    // frontend teardown. The engine alone decides the authority cutoff.
    let ExchangeResult { result, revision, failed, close_cleanup, uncertain_close } =
        execute_result(profile, context, shared, sequence, token, &call);
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    if failed || close_cleanup.is_some() {
        super::record_failure(&mut state, Failure::Exchange);
    }
    if close_cleanup.is_some() || uncertain_close {
        let retention =
            state.file_entry(token).map(|entry| entry.retention.clone()).unwrap_or_default();
        retain_failed_cleanup(
            &mut state,
            Cleanup { file: call.file, list: false, _retention: retention },
            false,
        );
    }
    let mismatched = matches!((revision, state.revision), (Some((epoch, _)), Some((applied, _))) if epoch != applied);
    if mismatched {
        super::record_failure(&mut state, Failure::Exchange);
    }
    let abandoned = state.file_entry(token).is_none_or(|entry| entry.abandoned);
    if abandoned {
        // New result capabilities have never escaped this lane; dispose them
        // here before advancing. Earlier accepted work cannot address them.
        let cleanup = result.as_ref().ok().and_then(capability);
        drop(state);
        let failure = cleanup.and_then(|(file, list)| {
            cleanup_exchange(profile, context, sequence, file, list).err().map(|_| (file, list))
        });
        state = shared.state.lock().expect("guest completion lock poisoned");
        if let Some(capability) = failure {
            let retention =
                state.file_entry(token).map(|entry| entry.retention.clone()).unwrap_or_default();
            retain_failed_cleanup(
                &mut state,
                Cleanup { file: capability.0, list: capability.1, _retention: retention },
                false,
            );
            super::record_failure(&mut state, Failure::Exchange);
        }
        remove(&mut state, token);
        state.files.cleanup_completed = true;
    } else if let Some(entry) = state.file_entry_mut(token) {
        entry.result = Some(result.map(Arc::new));
        entry.revision = revision;
    }
    drop(state);
    (shared.wake)();
}
struct ExchangeResult {
    result: HostResult<Output>,
    revision: Option<(crate::store::Id, u64)>,
    failed: bool,
    close_cleanup: Option<HostError>,
    uncertain_close: bool,
}
fn execute_result(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    token: u64,
    call: &Call,
) -> ExchangeResult {
    if let Some(result) = failed_writer_result(shared, call) {
        return ExchangeResult {
            result,
            revision: None,
            failed: false,
            close_cleanup: None,
            uncertain_close: false,
        };
    }
    let frozen = shared.state.lock().expect("guest completion lock poisoned").failure
        == Some(Failure::Exchange);
    let (mut result, revision, failed) = if frozen {
        (Err(HostError::Unavailable), None, false)
    } else {
        exchange(profile, context, sequence, file_service::Request::Operation(call.clone()))
    };
    // A rejected close has already sealed the guest handle, but the engine
    // may still own its source. A lost reply may instead hide an accepted
    // publication. Discard waits for retirement and never replays that close.
    // A lost reply can hide a newly allocated recovery handle. Even a successful
    // source discard cannot identify that handle; keep its quota until context
    // retirement. The frozen generation cannot reuse that reservation.
    let uncertain_close = failed && result.is_err() && matches!(call.action, Action::Close { .. });
    let close_cleanup = if result.is_err() && matches!(call.action, Action::Close { .. }) {
        cleanup_exchange(profile, context, sequence, call.file, false).err()
    } else {
        None
    };
    if (failed || frozen)
        && result.is_err()
        && let Action::Close { flags } = call.action
    {
        // Losing the reply cannot establish whether a normal close published.
        // An exchange that was never submitted is definitely NotPublished.
        result = Ok(Output::Close {
            error: HostError::Unavailable as i32,
            publication: if frozen || flags == playbit_api::abi::files::FILE_CLOSE_DISCARD {
                crate::runtime::host::files::NOT_PUBLISHED
            } else {
                crate::runtime::host::files::PUBLICATION_UNKNOWN
            },
            recovery: None,
        });
    }
    let outcome = ExchangeResult { result, revision, failed, close_cleanup, uncertain_close };
    if matches!(call.action, Action::Write { .. }) && outcome.result.is_err() && !frozen {
        fail_writer(profile, context, shared, sequence, token, call, &outcome);
    }
    outcome
}
fn failed_writer_result(shared: &Shared, call: &Call) -> Option<HostResult<Output>> {
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    let failure = state.files.failed_writers.get(&call.file)?;
    let error = failure.error;
    if let Action::Close { flags } = call.action {
        state.files.failed_writers.remove(&call.file);
        return Some(Ok(Output::Close {
            error: if flags == playbit_api::abi::files::FILE_CLOSE_DISCARD {
                0
            } else {
                error as i32
            },
            publication: crate::runtime::host::files::NOT_PUBLISHED,
            recovery: None,
        }));
    }
    Some(Err(error))
}
fn fail_writer(
    profile: &Path,
    context: Context,
    shared: &Shared,
    sequence: &mut u64,
    token: u64,
    call: &Call,
    outcome: &ExchangeResult,
) {
    // Frontend admission already accepted this Write. Even an engine admission
    // rejection must fail the writer before the FIFO can process its Close.
    // Never replay the Write or attempt publication after an uncertain reply.
    let discarded = cleanup_exchange(profile, context, sequence, call.file, false);
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    let source = state.file_entry(token).and_then(|entry| entry.retention.source.clone());
    let retention = Retention { source, result: None };
    if discarded.is_err() {
        retain_failed_cleanup(
            &mut state,
            Cleanup { file: call.file, list: false, _retention: retention },
            false,
        );
        return;
    }
    assert!(state.files.failed_writers.len() < MAX_FILES, "failed writers retain File credit");
    let error = outcome.result.as_ref().err().copied().expect("failed accepted Write");
    let prior =
        state.files.failed_writers.insert(call.file, FailedWriter { error, _retention: retention });
    assert!(prior.is_none(), "failed writer operations are handled before engine exchange");
}
pub(super) fn cleanup_exchange(
    profile: &Path,
    context: Context,
    sequence: &mut u64,
    file: u64,
    list: bool,
) -> HostResult<()> {
    let request = if list {
        file_service::Request::ListClose { list: file }
    } else {
        file_service::Request::Operation(Call {
            file,
            action: Action::Close { flags: playbit_api::abi::files::FILE_CLOSE_DISCARD },
        })
    };
    let (result, _, _) = exchange(profile, context, sequence, request);
    match result {
        Ok(Output::Close { error: 0, .. }) | Err(HostError::Closed | HostError::StaleHandle) => {
            Ok(())
        }
        // An admission failure does not prove the object was released. Freeze
        // this generation and retain its cleanup accounting until retirement.
        _ => Err(HostError::Unavailable),
    }
}
pub(super) fn cleanup(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    cleanup: Cleanup,
) {
    let already_discarded = !cleanup.list
        && shared
            .state
            .lock()
            .expect("guest completion lock poisoned")
            .files
            .failed_writers
            .remove(&cleanup.file)
            .is_some();
    let result = if already_discarded {
        Ok(())
    } else {
        cleanup_exchange(profile, context, sequence, cleanup.file, cleanup.list)
    };
    let mut state = shared.state.lock().expect("guest completion lock poisoned");
    assert!(state.files.cleanup > 0, "queued cleanup retains its reservation");
    if result.is_ok() {
        state.files.cleanup -= 1;
        state.files.cleanup_completed = true;
    } else {
        retain_failed_cleanup(&mut state, cleanup, true);
    }
    drop(state);
    (shared.wake)();
}
pub(super) fn mismatched_epoch(state: &super::State, epoch: crate::store::Id) -> bool {
    state.file_entries().map(|(_, entry)| entry).any(|entry| {
        entry.result.as_ref().is_some_and(Result::is_ok)
            && entry.revision.is_some_and(|(result_epoch, _)| epoch != result_epoch)
    })
}

/// Superseded bootstrap roots never escaped to the VM. Keep their quota until
/// the same ordered transport acknowledges destruction.
pub(super) fn retire_root(
    profile: &Path,
    context: Context,
    shared: &Arc<Shared>,
    sequence: &mut u64,
    root: crate::runtime::host::files::Root,
) {
    if cleanup_exchange(profile, context, sequence, root.engine, false).is_err() {
        let mut state = shared.state.lock().expect("guest completion lock poisoned");
        retain_failed_cleanup(
            &mut state,
            Cleanup {
                file: root.engine,
                list: false,
                _retention: Retention { source: Some(root.credit), result: None },
            },
            false,
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests check cleanup quota retention on rejected submissions."
)]
mod cleanup_admission_tests {
    use super::*;
    use crate::runtime::host::files::capacity::{Capacity, Kind};

    #[test]
    fn rejected_cleanup_keeps_credit_and_freezes_without_replay() {
        for disconnected in [false, true] {
            let mut state = super::super::State::default();
            let capacity = Arc::new(Capacity::default());
            let source = capacity.one(Kind::File).unwrap();
            let full = capacity.reserve(Kind::File, 511).unwrap();
            let (send, receive) = mpsc::sync_channel(0);
            if disconnected {
                drop(receive);
            }
            assert!(
                enqueue_cleanup(
                    &mut state,
                    &send,
                    10,
                    false,
                    Retention { source: Some(source.clone()), result: None }
                )
                .is_err()
            );
            assert!(state.failure.is_some());
            assert!(!executing(&state));
            assert_eq!(state.files.failed_cleanup.len(), 1);
            assert!(
                enqueue_cleanup(
                    &mut state,
                    &send,
                    10,
                    false,
                    Retention { source: Some(source), result: None }
                )
                .is_err()
            );
            assert_eq!(state.files.failed_cleanup.len(), 1);
            assert!(matches!(capacity.one(Kind::File), Err(HostError::LimitExceeded)));
            drop(state);
            assert!(capacity.one(Kind::File).is_ok());
            drop(full);
        }
    }
}
