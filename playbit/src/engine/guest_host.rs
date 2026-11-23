//! GUI-owned syscall host. Guest begin/take calls touch bounded memory only.
//! Separate I/O lanes keep bulk reads away from interactive reads. Mutations
//! share one ordered lane, including file writes. Exchanges are never replayed.
use super::{Request, ipc, reply::Reply, session_instance::Context};
use crate::runtime::{
    host::{DataCall, DataKind, DataOperation, DataResult, Host, HostResult},
    syscall::{EventKind, HostError, Opcode},
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};

mod channels;
mod files;
mod roots;

const MAX_CALLS: usize = 64;
const MAX_BYTES: usize = crate::runtime::host::MAX_RETAINED_DATA_BYTES;
const EXCHANGE_FAILED: &str = "The engine exchange failed. Accepted changes may have persisted. Reopen the app to read current data; this operation was not replayed.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    Exchange,
    Stopped(super::service_events::StopReason),
}
impl Failure {
    fn message(self) -> &'static str {
        match self {
            Self::Exchange => EXCHANGE_FAILED,
            Self::Stopped(reason) => reason.message(),
        }
    }
}
struct Entry {
    generation: u64,
    kind: DataKind,
    mutation: bool,
    revision: Option<(crate::store::Id, u64)>,
    bytes: usize,
    result_limit: usize,
    result: Option<HostResult<Arc<DataResult>>>,
    notified: bool,
    abandoned: bool,
}
#[derive(Default)]
struct State {
    generation: Option<u64>,
    operation: u64,
    channel_receive: u64,
    mutation: u64,
    frontier: u64,
    acknowledged: BTreeSet<u64>,
    bytes: usize,
    entries: crate::capability::Table<Completion>,
    files: files::State,
    roots: roots::State,
    failure: Option<Failure>,
    disposed: bool,
    event_sequence: u64,
    revision: Option<(crate::store::Id, u64)>,
    data_revision: u64,
    mount_revision: u64,
    changed: bool,

    volumes: Option<Vec<crate::runtime::host::AvailableVolume>>,
}
enum Completion {
    Data(Box<Entry>),
    File(Box<files::Entry>),
}
impl State {
    fn data_entry(&self, token: u64) -> Option<&Entry> {
        match self.entries.lookup(&token)? {
            Completion::Data(entry) => Some(entry),
            _ => None,
        }
    }
    fn data_entry_mut(&mut self, token: u64) -> Option<&mut Entry> {
        match self.entries.lookup_mut(&token)? {
            Completion::Data(entry) => Some(entry),
            _ => None,
        }
    }
    fn data_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().filter_map(|entry| match entry {
            Completion::Data(entry) => Some(entry.as_ref()),
            _ => None,
        })
    }
    fn data_iter_mut(&mut self) -> impl Iterator<Item = (u64, &mut Entry)> {
        self.entries.iter_mut().filter_map(|(id, entry)| match entry {
            Completion::Data(entry) => Some((id, entry.as_mut())),
            _ => None,
        })
    }
    fn file_entry(&self, token: u64) -> Option<&files::Entry> {
        match self.entries.lookup(&token)? {
            Completion::File(entry) => Some(entry),
            _ => None,
        }
    }
    fn file_entry_mut(&mut self, token: u64) -> Option<&mut files::Entry> {
        match self.entries.lookup_mut(&token)? {
            Completion::File(entry) => Some(entry),
            _ => None,
        }
    }
    fn file_entries(&self) -> impl Iterator<Item = (u64, &files::Entry)> {
        self.entries.iter().filter_map(|(id, entry)| match entry {
            Completion::File(entry) => Some((id, entry.as_ref())),
            _ => None,
        })
    }
}
struct Shared {
    state: Mutex<State>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
struct Work {
    token: u64,
    operation: u64,
    mutation: u64,
    call: Arc<DataCall>,
}
enum LaneWork {
    ChannelsClose,
    ChannelConsumed([u8; 16], [u8; 16], [u8; 16]),
    Data(Work),
    File { token: u64, call: Arc<crate::runtime::host::files::Call> },
    FileCleanup(files::Cleanup),
    Roots,
    RootCapacity,
}
struct Exchange {
    result: HostResult<DataResult>,
    revision: Option<(crate::store::Id, u64)>,
    failure: Option<Failure>,
}

/// Readiness belongs to the guest generation that admitted the operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ready {
    pub generation: u64,
    pub token: u64,
    pub kind: EventKind,
}

/// The local guest owner drains this inbox after a readiness wake. No polling
/// timer, socket ownership or storage access is exposed to the guest thread.
#[derive(Clone)]
pub struct Inbox(Arc<Shared>);
impl Inbox {
    pub fn has_ready(&self) -> bool {
        let state = self.0.state.lock().expect("guest completion lock poisoned");
        state.changed
            || state.volumes.is_some()
            || files::has_ready(&state)
            || state.data_entries().any(|entry| {
                available(entry, state.revision) && !entry.notified && !entry.abandoned
            })
    }
    pub fn take_changed(&self) -> bool {
        let mut state = self.0.state.lock().expect("guest completion lock poisoned");
        std::mem::take(&mut state.changed)
    }
    /// Adopt metadata before entering the guest or copying a retained result.
    pub fn take_volumes(&self) -> Option<Vec<crate::runtime::host::AvailableVolume>> {
        let mut state = self.0.state.lock().expect("guest completion lock poisoned");
        if state.roots.enabled { roots::take_volumes(&mut state) } else { state.volumes.take() }
    }
    pub fn defer_changed(&self) {
        let mut state = self.0.state.lock().expect("guest completion lock poisoned");
        state.changed = true;
    }
    pub fn ready(&self) -> Vec<Ready> {
        let mut state = self.0.state.lock().expect("guest completion lock poisoned");
        let revision = state.revision;
        let mut ready: Vec<_> = state
            .data_iter_mut()
            .filter_map(|(token, entry)| {
                if !available(entry, revision) || entry.notified || entry.abandoned {
                    return None;
                }
                entry.notified = true;
                Some(Ready {
                    generation: entry.generation,
                    token,
                    kind: if entry.kind == DataKind::Create {
                        EventKind::ObjectCreationReady
                    } else {
                        EventKind::DataReady
                    },
                })
            })
            .collect();
        files::ready(&mut state, &mut ready);
        ready
    }
    /// A full guest event queue leaves the exact notification eligible again.
    pub fn defer(&self, ready: Ready) {
        let mut state = self.0.state.lock().expect("guest completion lock poisoned");
        files::defer(&mut state, ready);
        if let Some(entry) = state.data_entry_mut(ready.token)
            && entry.generation == ready.generation
        {
            entry.notified = false;
        }
    }
    pub fn failure(&self) -> Option<&'static str> {
        self.0.state.lock().expect("guest completion lock poisoned").failure.map(Failure::message)
    }
    pub fn stop_reason(&self) -> Option<super::service_events::StopReason> {
        match self.0.state.lock().expect("guest completion lock poisoned").failure {
            Some(Failure::Stopped(reason)) => Some(reason),
            _ => None,
        }
    }
    /// Retained completions count until the guest consumes them. Quit therefore
    /// cannot mistake a ready parent for the end of its dependent guest steps.
    pub fn waiting(&self) -> bool {
        let state = self.0.state.lock().expect("guest completion lock poisoned");
        files::waiting(&state)
            || state.data_entries().any(|entry| {
                entry.kind != DataKind::ChannelEvents && !available(entry, state.revision)
            })
    }
    /// Work accepted by the old guest can still be queued before engine
    /// admission. Reconstruction must wait for these exchanges after VM drop.
    pub fn files_executing(&self) -> bool {
        let state = self.0.state.lock().expect("guest completion lock poisoned");
        files::executing(&state)
    }
    /// Terminal owner retirement waits for accepted exchanges, not retained
    /// results or the deliberately long-lived channel receive.
    pub fn work_executing(&self) -> bool {
        let state = self.0.state.lock().expect("guest completion lock poisoned");
        files::executing(&state)
            || state
                .data_entries()
                .any(|entry| entry.kind != DataKind::ChannelEvents && entry.result.is_none())
    }
    pub fn idle(&self) -> bool {
        let state = self.0.state.lock().expect("guest completion lock poisoned");
        state.data_entries().all(|entry| entry.kind == DataKind::ChannelEvents)
            && files::idle(&state)
    }
}

pub struct GuestHost {
    pub(super) renderer: super::GuestRenderer,
    pub(super) session_key_secret: Option<crate::store::Seed>,
    shared: Arc<Shared>,
    mutations: mpsc::SyncSender<LaneWork>,
    reads: mpsc::SyncSender<LaneWork>,
    bulk: mpsc::SyncSender<LaneWork>,
    channel_receives: mpsc::SyncSender<LaneWork>,
    channel_commands: mpsc::SyncSender<LaneWork>,
    _event_stop: EventStop,
    root_notify: Option<Arc<roots::Notify>>,
}
struct EventStop(std::os::unix::net::UnixStream);
impl Drop for EventStop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}
impl GuestHost {
    /// Start on the GUI's guest worker before entering guest code. Thread
    /// creation and subsequent socket I/O never run inside a guest syscall.
    pub fn start(
        profile: PathBuf,
        context: Context,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<(Self, Inbox)> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake });

        let mutations = lane("guest-mutations", &profile, context, &shared)?;
        let reads = lane("guest-reads", &profile, context, &shared)?;
        let bulk = lane("guest-bulk", &profile, context, &shared)?;
        let channel_receives = lane("guest-channel-receive", &profile, context, &shared)?;
        let channel_commands = lane("guest-channel-command", &profile, context, &shared)?;
        let event_stop = event_lane(&profile, context, &shared, bulk.clone())?;
        Ok((
            Self {
                renderer: super::GuestRenderer::Embedded,
                session_key_secret: None,
                shared: shared.clone(),
                mutations,
                reads,
                bulk,
                _event_stop: event_stop,
                root_notify: None,
                channel_receives,
                channel_commands,
            },
            Inbox(shared),
        ))
    }
}
impl GuestHost {
    /// Called by the resident owner before creating the VM. Root I/O belongs to
    /// the bulk lane; guest callbacks only borrow the resulting memory snapshot.
    pub fn prepare_files(
        &mut self,
        volumes: Vec<crate::runtime::host::AvailableVolume>,
        restore_host_mounts: bool,
    ) -> HostResult<Vec<crate::runtime::host::AvailableVolume>> {
        roots::prepare(self, volumes, restore_host_mounts)
    }
}
impl Host for GuestHost {
    fn session_key_secret(&self) -> HostResult<Option<crate::store::Seed>> {
        if self.shared.state.lock().expect("guest completion lock poisoned").failure.is_some() {
            return Err(HostError::Unavailable);
        }
        Ok(self.session_key_secret)
    }
    fn channel_consumed(
        &mut self,
        _recipient: [u8; 16],
        account: [u8; 16],
        sender: [u8; 16],
        message: [u8; 16],
    ) -> HostResult<()> {
        self.channel_commands
            .try_send(LaneWork::ChannelConsumed(account, sender, message))
            .map_err(|_| HostError::LimitExceeded)
    }
    fn retire_channels(&mut self, _instance: [u8; 16]) {
        let _ = self.channel_commands.try_send(LaneWork::ChannelsClose);
    }
    fn file_capacity(&self) -> Arc<crate::runtime::host::files::capacity::Capacity> {
        self.shared.state.lock().expect("guest completion lock poisoned").roots.capacity.clone()
    }
    fn file_roots(
        &mut self,
        volumes: &[crate::runtime::host::AvailableVolume],
    ) -> HostResult<Vec<crate::runtime::host::files::Root>> {
        roots::adopt(
            &mut self.shared.state.lock().expect("guest completion lock poisoned"),
            volumes,
        )
    }
    fn supported_features(&self) -> &[&str] {
        self.renderer.features()
    }
    fn audio_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.shared.wake.clone())
    }
    fn supports_audio(&self) -> bool {
        cfg!(all(feature = "native-audio", target_vendor = "apple"))
    }
    fn supported_operations(&self) -> &[Opcode] {
        if self.shared.state.lock().expect("guest completion lock poisoned").roots.enabled {
            return roots::OPERATIONS;
        }
        &[
            Opcode::ChannelOpen,
            Opcode::ChannelBegin,
            Opcode::ChannelTake,
            Opcode::BloblogAppendBegin,
            Opcode::BloblogAppendTake,
            Opcode::BloblogReadBegin,
            Opcode::BloblogReadTake,
            Opcode::KvKeysBegin,
            Opcode::KvKeysTake,
            Opcode::KvReadBegin,
            Opcode::KvReadTake,
            Opcode::KvPutBegin,
            Opcode::KvPutTake,
            Opcode::KvDeleteBegin,
            Opcode::KvDeleteTake,
            Opcode::EntryResolveBegin,
            Opcode::EntryResolveTake,
            Opcode::SchemaReadBegin,
            Opcode::SchemaReadTake,
            Opcode::SchemaEstablishBegin,
            Opcode::SchemaEstablishTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
        ]
    }
    fn begin_data(&mut self, generation: u64, call: Arc<DataCall>) -> HostResult<u64> {
        call.operation.validate(Default::default())?;
        let result_limit = call.completion_bytes();
        // The I/O lane temporarily owns the encoded request and a copied DTO.
        // Count those separately from the runtime's retained original input.
        let bytes = call
            .retained_bytes()
            .and_then(|input| input.checked_mul(2))
            .and_then(|input| input.checked_add(result_limit))
            .ok_or(HostError::LimitExceeded)?;
        let lane = if matches!(call.operation, DataOperation::ChannelEvents { .. }) {
            &self.channel_receives
        } else if call.operation.network() {
            &self.channel_commands
        } else if call.operation.is_mutation() {
            &self.mutations
        } else if matches!(call.operation, DataOperation::FileRead { .. }) {
            &self.bulk
        } else {
            &self.reads
        };
        let mut state = self.shared.state.lock().expect("guest completion lock poisoned");
        if state.failure.is_some() {
            return Err(HostError::Unavailable);
        }
        if generation == 0 || state.generation.is_some_and(|active| active != generation) {
            return Err(HostError::StaleHandle);
        }
        if !admits(state.entries.len(), state.bytes, bytes) {
            return Err(HostError::LimitExceeded);
        }
        let receiving = matches!(call.operation, DataOperation::ChannelEvents { .. });
        let operation = if receiving {
            state
                .channel_receive
                .checked_add(1)
                .filter(|next| *next < super::service_call::CHANNEL_RECEIVE_BIT)
                .ok_or(HostError::LimitExceeded)?
                | super::service_call::CHANNEL_RECEIVE_BIT
        } else {
            state.operation.checked_add(1).ok_or(HostError::LimitExceeded)?
        };
        if !receiving
            && (operation >= super::service_call::CHANNEL_RECEIVE_BIT
                || operation - state.frontier > MAX_CALLS as u64)
        {
            return Err(HostError::LimitExceeded);
        }
        let mutation = if call.operation.is_mutation() {
            state.mutation.checked_add(1).ok_or(HostError::LimitExceeded)?
        } else {
            0
        };
        let kind = call.operation.kind();
        let mutating = call.operation.is_mutation();
        // The shared lock keeps an immediate completion from preceding insertion.

        let handle = state.entries.reserve(1, crate::capability::Rights::NONE)?;
        let token = handle.raw();
        let sent = lane
            .try_send(LaneWork::Data(Work { token, operation, mutation, call }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => HostError::LimitExceeded,
                mpsc::TrySendError::Disconnected(_) => HostError::Unavailable,
            });
        if let Err(error) = sent {
            state.entries.cancel(handle).expect("unsent operation retains reservation");
            return Err(error);
        }
        state.generation = Some(generation);
        if receiving {
            state.channel_receive = operation & !super::service_call::CHANNEL_RECEIVE_BIT;
        } else {
            state.operation = operation;
        }
        if mutation != 0 {
            state.mutation = mutation;
        }
        state.bytes += bytes;
        state
            .entries
            .insert(
                handle,
                Completion::Data(Box::new(Entry {
                    generation,
                    kind,
                    mutation: mutating,
                    revision: None,
                    bytes,
                    result_limit,
                    result: None,
                    notified: false,
                    abandoned: false,
                })),
            )
            .expect("sent operation retains its reservation");

        Ok(token)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        let mut state = self.shared.state.lock().expect("guest completion lock poisoned");
        let entry = state.data_entry(token).ok_or(HostError::StaleHandle)?;
        if (state.volumes.is_some() || state.roots.blocked())
            && !entry.mutation
            && !entry.kind.network()
        {
            let entry = state.data_entry_mut(token).ok_or(HostError::StaleHandle)?;
            entry.notified = false;
            return Err(HostError::WouldBlock);
        }
        let entry = state.data_entry(token).ok_or(HostError::StaleHandle)?;
        if !available(entry, state.revision) {
            return Err(HostError::WouldBlock);
        }
        if !entry.mutation
            && !entry.kind.network()
            && entry.result.as_ref().is_some_and(Result::is_ok)
            && entry.revision.is_none_or(|(_, revision)| revision < state.data_revision)
        {
            return Err(HostError::Unavailable);
        }
        entry.result.as_ref().cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn begin_file(
        &mut self,
        generation: u64,
        call: Arc<crate::runtime::host::files::Call>,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<u64> {
        files::begin(self, generation, call, retention)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::files::Output>> {
        files::peek(self, token)
    }
    fn release_file(&mut self, token: u64, claimed: bool) {
        files::release(self, token, claimed);
    }
    fn discard_file(
        &mut self,
        file: u64,
        list: bool,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<()> {
        files::discard(self, file, list, retention)
    }
    fn release_data(&mut self, token: u64) {
        let mut state = self.shared.state.lock().expect("guest completion lock poisoned");
        if let Some(entry) = state.data_entry_mut(token) {
            if entry.result.is_none() {
                // Disposal releases delivery, not accepted execution or credit.
                entry.abandoned = true;
            } else {
                remove(&mut state, token);
            }
        }
    }
}
impl Drop for GuestHost {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("guest completion lock poisoned");
        state.disposed = true;
        roots::dispose(&mut state, &self.bulk);
        files::dispose(&mut state, &self.bulk);
        state.entries.retain(|_, entry| match entry {
            Completion::Data(entry) => {
                entry.abandoned = true;
                entry.result.is_none()
            }
            Completion::File(_) => true,
        });
        state.bytes =
            state.data_entries().map(|entry| entry.bytes).sum::<usize>() + files::bytes(&state);
        drop(state);
        // Sender destruction closes each lane. Its worker drains accepted work;
        // no join or cancellation runs on the thread disposing this host.
    }
}

fn available(entry: &Entry, revision: Option<(crate::store::Id, u64)>) -> bool {
    match &entry.result {
        None => false,
        Some(Err(_)) => true,
        Some(Ok(result)) if entry.mutation && result.is_receipt(entry.kind) => true,
        Some(Ok(_)) if matches!(entry.kind, DataKind::ChannelEvents | DataKind::ChannelCommand) => {
            true
        }
        Some(Ok(_)) => {
            matches!((entry.revision, revision), (Some((epoch, before)), Some((applied_epoch, applied))) if epoch == applied_epoch && before <= applied)
        }
    }
}

fn event_follows(previous: u64, sequence: u64, before: u64, after: u64, same_epoch: bool) -> bool {
    previous.checked_add(1) == Some(sequence) && same_epoch && before <= after
}

fn admits(count: usize, held: usize, bytes: usize) -> bool {
    count < MAX_CALLS && bytes <= MAX_BYTES && held <= MAX_BYTES - bytes
}
fn remove(state: &mut State, token: u64) {
    if state.data_entry(token).is_some() {
        let Some(Completion::Data(entry)) = state.entries.take(&token) else {
            unreachable!("validated data completion")
        };
        state.bytes -= entry.bytes;
    }
}
fn lane(
    name: &str,
    profile: &std::path::Path,
    context: Context,
    shared: &Arc<Shared>,
) -> std::io::Result<mpsc::SyncSender<LaneWork>> {
    let profile = profile.to_owned();
    let shared = shared.clone();
    let (send, receive) = mpsc::sync_channel::<LaneWork>(2 * MAX_CALLS + files::MAX_CLEANUPS + 2);
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut file_sequence = 0;
        while let Ok(work) = receive.recv() {
            match work {
                LaneWork::ChannelConsumed(account, sender, message) => {
                    channels::send_control(&profile, &shared, Request::InstanceChannelsAck {
                        context,
                        delivery: Box::new(super::ChannelDelivery {
                            account: crate::store::Id::from_bytes(account),
                            sender: crate::store::Id::from_bytes(sender),
                            message: crate::store::Id::from_bytes(message),
                        }),
                    });
                    continue;
                }
                LaneWork::ChannelsClose => {
                    channels::send_control(&profile, &shared, Request::InstanceChannelsClose { context });
                    continue;
                }
                LaneWork::RootCapacity => {
                    roots::retry(&profile, context, &shared, &mut file_sequence);
                    continue;
                }
                LaneWork::Roots => {
                    roots::execute(&profile, context, &shared, &mut file_sequence);
                    continue;
                }
                LaneWork::File { token, call } => {
                    files::execute(&profile, context, &shared, &mut file_sequence, token, call);
                    continue;
                }
                LaneWork::FileCleanup(cleanup) => {
                    files::cleanup(&profile, context, &shared, &mut file_sequence, cleanup);
                    continue;
                }
                LaneWork::Data(_) => {}
            }
            let LaneWork::Data(work) = work else { unreachable!() };

            let _span = crate::trace::Span::new("guest.data.exchange");

            let frozen = shared.state.lock().expect("guest completion lock poisoned").failure.is_some();
            let Exchange { mut result, revision, failure } = if frozen {
                Exchange { result: Err(HostError::Unavailable), revision: None, failure: None }
            } else {
                exchange(&profile, context, &work)
            };

            let mut state = shared.state.lock().expect("guest completion lock poisoned");
            if let Some(failure) = failure {
                record_failure(&mut state, failure);
            }
            if result.is_ok() && !work.call.operation.is_mutation() && matches!((revision, state.revision), (Some((epoch, _)), Some((applied_epoch, _))) if epoch != applied_epoch) {
                result = Err(HostError::Internal);
                record_failure(&mut state, Failure::Exchange);
            }
            if failure.is_none() && !frozen && !matches!(work.call.operation, DataOperation::ChannelEvents { .. }) {
                state.acknowledged.insert(work.operation);
                while let Some(next) = state.frontier.checked_add(1) {
                    if !state.acknowledged.remove(&next) {
                        break;
                    }
                    state.frontier = next;
                }
            }
            if let Some(entry) = state.data_entry_mut(work.token) {
                let result = result.and_then(|result| {
                    if result.retained_bytes().is_none_or(|bytes| bytes > entry.result_limit) {
                        Err(HostError::LimitExceeded)
                    } else {
                        Ok(Arc::new(result))
                    }
                });
                if entry.abandoned {
                    remove(&mut state, work.token);
                } else {

                    entry.revision = revision;
                    entry.result = Some(result);

                }
            }
            drop(state);
            (shared.wake)();
        }
    })?;
    Ok(send)
}
fn exchange(profile: &std::path::Path, context: Context, work: &Work) -> Exchange {
    let request = Request::Syscall {
        context: context.into(),
        operation: work.operation,
        mutation: work.mutation,
        call: Box::new(work.call.as_ref().clone()),
    };
    let response = ipc::call_archive(profile, &request).and_then(|response| response.into_owned());
    let Ok(Reply::Syscall(outcome)) = response else {
        return Exchange {
            result: Err(HostError::Unavailable),
            revision: None,
            failure: Some(Failure::Exchange),
        };
    };
    let revision = outcome.revision;
    let result = outcome.take(context, work.operation, work.mutation, &work.call);
    let failure = match result {
        Err(HostError::StaleHandle) => {
            Some(Failure::Stopped(super::service_events::StopReason::ContextUnavailable))
        }
        Err(HostError::Internal) => Some(Failure::Exchange),
        _ => None,
    };
    Exchange { result, revision, failure }
}

fn record_failure(state: &mut State, failure: Failure) {
    // Expected stops cannot conceal an uncertain exchange. A later ordered
    // notice can refine a known context refusal to its selected-build cause.
    if state.failure != Some(Failure::Exchange)
        && !(state.failure
            == Some(Failure::Stopped(super::service_events::StopReason::SelectedBuildChanged))
            && failure == Failure::Stopped(super::service_events::StopReason::ContextUnavailable))
    {
        state.failure = Some(failure);
    }
    roots::settle(state);
}

fn event_lane(
    profile: &std::path::Path,
    context: Context,
    shared: &Arc<Shared>,
    bulk: mpsc::SyncSender<LaneWork>,
) -> std::io::Result<EventStop> {
    use super::ipc::codec;
    // Startup runs on the resident owner. Connect first so events already have
    // their own socket when any data lane starts. Receive never has a timer.
    let mut stream =
        std::os::unix::net::UnixStream::connect(profile.join(crate::paths::ENGINE_SOCKET))?;
    ipc::verify_peer(&stream).map_err(|error| std::io::Error::other(error.to_string()))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let stop = EventStop(stream.try_clone()?);
    let shared = shared.clone();
    std::thread::Builder::new().name("guest-events".into()).spawn(move || {
        let request = Request::InstanceEvents { context };
        let mut run = || -> super::Result<()> {
            let bytes = codec::encode(&request, 4096)?;
            ipc::write(&mut stream, &bytes)?;
            loop {
                let bytes = codec::read(&mut stream, super::service_events::MAX_CHANNEL_BYTES)?;

                let response = ipc::Response::new(bytes)?;
                if !response.archived().matches_request(&request) {
                    return Err("Guest event context mismatch".into());
                }
                let Reply::InstanceEvents(notice) = response.into_owned()? else {
                    unreachable!("validated event reply");
                };
                let mut state = shared.state.lock().expect("guest completion lock poisoned");
                if state.disposed {
                    return Ok(());
                }
                let follows = notice.revision.is_some_and(|(epoch, after)| {
                    let (same_epoch, before) =
                        state.revision.map_or((true, 0), |(previous_epoch, before)| {
                            (epoch == previous_epoch, before)
                        });
                    event_follows(state.event_sequence, notice.sequence, before, after, same_epoch)
                });
                let mismatched_result = notice.revision.is_some_and(|(epoch, _)| {
                    files::mismatched_epoch(&state, epoch)
                        || state.data_entries().any(|entry| {
                            !entry.mutation
                                && entry.result.as_ref().is_some_and(Result::is_ok)
                                && entry
                                    .revision
                                    .is_some_and(|(result_epoch, _)| epoch != result_epoch)
                        })
                });
                let follows = follows
                    || (notice.stop.is_some()
                        && notice.revision.is_none()
                        && state.event_sequence.checked_add(1) == Some(notice.sequence));
                if !follows || mismatched_result {
                    return Err("Guest event ownership or continuity was lost".into());
                }
                if let Some(reason) = notice.stop {
                    if !notice.volumes.is_empty() {
                        return Err("Stopped guest notice retained mounts".into());
                    }
                    record_failure(&mut state, Failure::Stopped(reason));
                    drop(state);
                    (shared.wake)();
                    return Ok(());
                }
                crate::runtime::syscall::validate_volumes(&notice.volumes)
                    .map_err(|error| format!("Invalid guest mount snapshot: {error:?}"))?;
                if notice.mount_revision < state.mount_revision
                    || notice.data_revision < state.data_revision
                    || notice.revision.is_none_or(|(_, revision)| notice.data_revision > revision)
                {
                    return Err("Guest scoped data revision lost continuity".into());
                }
                let initial = state.event_sequence == 0;
                let changed = notice.data_revision > state.data_revision
                    || notice.mount_revision > state.mount_revision;
                state.event_sequence = notice.sequence;
                state.revision = notice.revision;
                state.data_revision = notice.data_revision;
                state.mount_revision = notice.mount_revision;
                if initial || changed {
                    if state.roots.enabled {
                        roots::request(&mut state, &bulk, notice.volumes)
                            .map_err(|error| format!("File root refresh failed: {error:?}"))?;
                    } else {
                        state.volumes = Some(notice.volumes);
                    }
                }
                // Engine history gaps advance the scoped fence explicitly.
                // Transport coalescing retains that fence in its successor.
                state.changed |= !initial && changed;

                roots::settle(&mut state);
                let wake = state.volumes.is_some()
                    || state.changed
                    || files::has_ready(&state)
                    || state.data_entries().any(|entry| available(entry, state.revision));
                drop(state);
                if wake {
                    (shared.wake)();
                }
            }
        };
        if let Err(error) = run() {
            let mut state = shared.state.lock().expect("guest completion lock poisoned");
            if !state.disposed {
                record_failure(&mut state, Failure::Exchange);
                crate::diagnostic!(
                    "Guest event channel failed for {}: {error}; operations were not replayed",
                    context.instance
                );
                drop(state);
                (shared.wake)();
            }
        }
    })?;
    Ok(stop)
}

#[cfg(test)]
#[path = "guest_host_tests.rs"]
mod tests;
