//! Bounded in-process data owners for native callbacks. Guest begin/take only
//! touch retained memory; storage runs on independent read, ordered mutation and bulk lanes.
use crate::runtime::{
    host::*,
    syscall::{HostError, Opcode},
};
#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};

mod files;

const MAX_CALLS: usize = 64;
const MAX_BYTES: usize = MAX_RETAINED_DATA_BYTES;
type Revision = Arc<
    dyn Fn(
            Option<(crate::store::Id, u64)>,
            crate::store::Id,
        ) -> (Option<(crate::store::Id, u64)>, Option<u64>)
        + Send
        + Sync,
>;
pub(crate) fn scoped_revision(
    store: Arc<crate::store::LocalStore>,
    binding: Option<[crate::store::Id; 6]>,
) -> Revision {
    Arc::new(move |previous, target| {
        let volumes = match binding {
            Some([a, b, c, d, e, f]) => [a, b, c, d, e, f, target],
            None => [target; 7],
        };
        store.delivery_scope_revision(previous, &volumes)
    })
}
struct Entry {
    generation: u64,
    kind: DataKind,
    volume: crate::store::Id,
    bytes: usize,
    result_limit: usize,
    revision: Option<(crate::store::Id, u64)>,
    result: Option<HostResult<Arc<DataResult>>>,
    notified: bool,
    abandoned: bool,
    channel_cancel: Arc<tokio::sync::Notify>,
}
#[derive(Default)]
struct State {
    generation: Option<u64>,
    bytes: usize,
    entries: crate::capability::Table<Completion>,
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
    fn file_entry_mut(&mut self, token: u64) -> Option<&mut files::Entry> {
        match self.entries.lookup_mut(&token)? {
            Completion::File(entry) => Some(entry),
            _ => None,
        }
    }
    fn file_entries(&self) -> impl Iterator<Item = &files::Entry> {
        self.entries.values().filter_map(|entry| match entry {
            Completion::File(entry) => Some(entry.as_ref()),
            _ => None,
        })
    }
    fn file_iter_mut(&mut self) -> impl Iterator<Item = (u64, &mut files::Entry)> {
        self.entries.iter_mut().filter_map(|(id, entry)| match entry {
            Completion::File(entry) => Some((id, entry.as_mut())),
            _ => None,
        })
    }
}
#[cfg(feature = "connected-engine")]
#[derive(Clone)]
struct ChannelBinding {
    store: Arc<crate::store::LocalStore>,
    session: String,
    build: [u8; 32],
    transport: super::blob::BlobTransport,
}
#[derive(Clone)]
struct DataBinding {
    store: Arc<crate::store::LocalStore>,
    access: crate::store::Access,
}
struct Shared {
    data: Mutex<Option<DataBinding>>,
    #[cfg(feature = "connected-engine")]
    channels: Mutex<Option<ChannelBinding>>,
    files: Mutex<Option<files::Adapter>>,
    state: Arc<Mutex<State>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    revision: Revision,
}
struct Work {
    #[cfg(feature = "connected-engine")]
    bloblog: Option<super::bloblog::CallBoundary>,
    token: u64,
    call: Arc<DataCall>,
}
#[derive(Clone)]
pub struct Inbox(Arc<Shared>);
impl Inbox {
    pub fn has_ready(&self) -> bool {
        if self
            .0
            .files
            .lock()
            .expect("native File adapter lock poisoned")
            .as_ref()
            .is_some_and(files::Adapter::has_ready)
        {
            return true;
        }
        self.0
            .state
            .lock()
            .expect("local completion lock poisoned")
            .data_entries()
            .any(|entry| entry.result.is_some() && !entry.notified && !entry.abandoned)
    }
    pub fn ready(&self) -> Vec<(u64, u64, bool)> {
        let mut ready = self
            .0
            .files
            .lock()
            .expect("native File adapter lock poisoned")
            .as_mut()
            .map_or_else(Vec::new, files::Adapter::ready);
        ready.extend(
            self.0
                .state
                .lock()
                .expect("local completion lock poisoned")
                .data_iter_mut()
                .filter_map(|(token, entry)| {
                    if entry.result.is_none() || entry.notified || entry.abandoned {
                        return None;
                    }
                    entry.notified = true;
                    Some((entry.generation, token, entry.kind == DataKind::Create))
                })
                .collect::<Vec<_>>(),
        );
        ready
    }
    pub fn defer(&self, token: u64) {
        if let Some(files) =
            self.0.files.lock().expect("native File adapter lock poisoned").as_mut()
        {
            files.defer(token);
        }
        if let Some(entry) =
            self.0.state.lock().expect("local completion lock poisoned").data_entry_mut(token)
        {
            entry.notified = false;
        }
    }
    pub fn waiting(&self) -> bool {
        if self
            .0
            .files
            .lock()
            .expect("native File adapter lock poisoned")
            .as_ref()
            .is_some_and(files::Adapter::waiting)
        {
            return true;
        }
        self.0
            .state
            .lock()
            .expect("local completion lock poisoned")
            .data_entries()
            .any(|entry| entry.kind != DataKind::ChannelEvents && entry.result.is_none())
    }
    pub fn prepare_files(&self, volumes: &[AvailableVolume]) -> HostResult<Vec<AvailableVolume>> {
        if let Some(files) = self.0.files.lock().map_err(|_| HostError::Internal)?.as_mut() {
            return files.request_roots(volumes);
        }
        Ok(volumes.to_vec())
    }
    pub fn retry_file_roots(&self) -> HostResult<Option<Vec<AvailableVolume>>> {
        self.0
            .files
            .lock()
            .map_err(|_| HostError::Internal)?
            .as_mut()
            .map_or(Ok(None), files::Adapter::retry_roots)
    }
    pub fn idle(&self) -> bool {
        if self
            .0
            .files
            .lock()
            .expect("native File adapter lock poisoned")
            .as_mut()
            .is_some_and(|files| !files.idle())
        {
            return false;
        }
        self.0
            .state
            .lock()
            .expect("local completion lock poisoned")
            .data_entries()
            .all(|entry| entry.kind == DataKind::ChannelEvents)
    }
}
pub struct InProcessHost {
    shared: Arc<Shared>,
    reads: Option<mpsc::SyncSender<Work>>,
    bulk: Option<mpsc::SyncSender<Work>>,
    mutations: Option<mpsc::SyncSender<Work>>,
    workers: Option<[std::thread::JoinHandle<()>; 3]>,
    limits: QueryLimits,
}
impl InProcessHost {
    /// Admit File roots and data authority from validated context metadata.
    /// First-use storage preparation runs later on the existing worker lanes.
    pub fn enable_files(
        &mut self,
        services: &super::Services,
        access: crate::store::Access,
        session: Option<String>,
        transport: Option<super::blob::BlobTransport>,
        volumes: &[AvailableVolume],
    ) -> HostResult<Vec<AvailableVolume>> {
        #[cfg(feature = "connected-engine")]
        if let (Some(session), Some(transport), Some(build)) =
            (session.clone(), transport.clone(), access.build_id())
        {
            let build = crate::identifier::decode(build).map_err(|_| HostError::InvalidArgument)?;
            *self.shared.channels.lock().map_err(|_| HostError::Internal)? =
                Some(ChannelBinding { store: services.store.clone(), session, build, transport });
        }
        let mut files = self.shared.files.lock().map_err(|_| HostError::Internal)?;
        if files.is_some() {
            return Err(HostError::Conflict);
        }
        let data = DataBinding { store: services.store.clone(), access: access.clone() };
        let mut adapter = files::Adapter::new(
            services,
            access,
            session,
            transport,
            self.shared.wake.clone(),
            Arc::downgrade(&self.shared.state),
        )?;
        let admitted = adapter.prepare(volumes)?;
        *files = Some(adapter);
        *self.shared.data.lock().expect("native data binding lock poisoned") = Some(data);
        Ok(admitted)
    }
    pub fn local(
        backend: super::data_host::DataHost,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<(Self, Inbox)> {
        let store = backend.store.clone();
        let data = DataBinding { store: store.clone(), access: backend.access.clone() };
        let (host, inbox) = Self::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            scoped_revision(store, None),
            wake,
        )?;
        *host.shared.data.lock().expect("native data binding lock poisoned") = Some(data);
        Ok((host, inbox))
    }
    /// Hosts are transferred exactly once before callbacks start. The three
    /// backend instances share authority/storage but own their own I/O turns.
    pub fn start(
        reads: Box<dyn Host + Send>,
        mutations: Box<dyn Host + Send>,
        bulk: Box<dyn Host + Send>,
        revision: Revision,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<(Self, Inbox)> {
        let limits = reads.query_limits();
        let shared = Arc::new(Shared {
            data: Mutex::new(None),
            files: Mutex::new(None),
            #[cfg(feature = "connected-engine")]
            channels: Mutex::new(None),
            state: Arc::new(Mutex::new(State::default())),
            wake,
            revision,
        });
        let (reads, read_worker) = lane("local-reads", reads, &shared)?;
        let (mutations, mutation_worker) = lane("local-mutations", mutations, &shared)?;
        let (bulk, bulk_worker) = lane("local-bulk", bulk, &shared)?;
        Ok((
            Self {
                reads: Some(reads),
                mutations: Some(mutations),
                bulk: Some(bulk),
                workers: Some([read_worker, mutation_worker, bulk_worker]),
                limits,
                shared: shared.clone(),
            },
            Inbox(shared),
        ))
    }
}
impl Host for InProcessHost {
    fn channel_consumed(
        &mut self,
        recipient: [u8; 16],
        account: [u8; 16],
        sender: [u8; 16],
        message: [u8; 16],
    ) -> HostResult<()> {
        #[cfg(feature = "connected-engine")]
        if let Some(binding) =
            self.shared.channels.lock().map_err(|_| HostError::Internal)?.as_ref()
        {
            binding.transport.channel_consumed(recipient, account, sender, message);
            return Ok(());
        }
        let _ = (recipient, account, sender, message);
        Err(HostError::Unsupported)
    }
    fn retire_channels(&mut self, instance: [u8; 16]) {
        #[cfg(feature = "connected-engine")]
        if let Some(binding) =
            self.shared.channels.lock().expect("channel binding poisoned").as_ref()
        {
            binding.transport.retire_channels(instance);
        }
        #[cfg(not(feature = "connected-engine"))]
        let _ = instance;
    }
    fn file_capacity(&self) -> Arc<crate::runtime::host::files::capacity::Capacity> {
        self.shared
            .files
            .lock()
            .expect("native File adapter lock poisoned")
            .as_ref()
            .map_or_else(|| Arc::new(Default::default()), |files| files.capacity.clone())
    }
    fn file_roots(
        &mut self,
        volumes: &[AvailableVolume],
    ) -> HostResult<Vec<crate::runtime::host::files::Root>> {
        self.shared
            .files
            .lock()
            .map_err(|_| HostError::Internal)?
            .as_mut()
            .map_or_else(|| Ok(Vec::new()), |files| files.roots(volumes))
    }
    fn begin_file(
        &mut self,
        generation: u64,
        call: Arc<crate::runtime::host::files::Call>,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<u64> {
        let mut files = self.shared.files.lock().map_err(|_| HostError::Internal)?;
        let files = files.as_mut().ok_or(HostError::Unsupported)?;
        let (calls, bytes) = files.usage();
        let state = self.shared.state.lock().map_err(|_| HostError::Internal)?;
        if generation == 0 || state.generation.is_some_and(|current| current != generation) {
            return Err(HostError::StaleHandle);
        }
        let charge =
            call.action.retained_bytes()?.checked_mul(2).ok_or(HostError::LimitExceeded)?;
        if !admits(state.entries.count(1) + calls, state.bytes + bytes, charge) {
            return Err(HostError::LimitExceeded);
        }
        drop(state);
        let token = files.begin(generation, &call, retention)?;
        Ok(token)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::files::Output>> {
        self.shared
            .files
            .lock()
            .map_err(|_| HostError::Internal)?
            .as_mut()
            .ok_or(HostError::Unsupported)?
            .peek(token)
    }
    fn release_file(&mut self, token: u64, claimed: bool) {
        if let Some(files) =
            self.shared.files.lock().expect("native File adapter lock poisoned").as_mut()
        {
            files.release(token, claimed);
        }
    }
    fn discard_file(
        &mut self,
        file: u64,
        list: bool,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<()> {
        self.shared
            .files
            .lock()
            .map_err(|_| HostError::Internal)?
            .as_mut()
            .ok_or(HostError::Unsupported)?
            .discard(file, list, retention)
    }
    fn query_limits(&self) -> QueryLimits {
        self.limits
    }
    fn supported_features(&self) -> &[&str] {
        super::guest_features()
    }
    fn audio_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.shared.wake.clone())
    }
    fn supports_audio(&self) -> bool {
        cfg!(all(feature = "native-audio", target_vendor = "apple"))
    }
    fn supported_operations(&self) -> &[Opcode] {
        if self.shared.files.lock().expect("native File adapter lock poisoned").is_some() {
            return files::OPERATIONS;
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
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
        ]
    }
    fn begin_data(&mut self, generation: u64, call: Arc<DataCall>) -> HostResult<u64> {
        call.operation.validate(self.limits)?;
        if call.operation.tool_only()
            || matches!(call.operation, DataOperation::AuthenticatedField { .. })
        {
            return Err(HostError::Unsupported);
        }
        let result_limit = call.completion_bytes();
        let bytes = call
            .retained_bytes()
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(result_limit))
            .ok_or(HostError::LimitExceeded)?;
        let (file_calls, file_bytes) = self
            .shared
            .files
            .lock()
            .map_err(|_| HostError::Internal)?
            .as_mut()
            .map_or((0, 0), files::Adapter::usage);
        #[cfg(feature = "connected-engine")]
        let bloblog = if call.operation.kind().bloblog() {
            self.shared
                .channels
                .lock()
                .map_err(|_| HostError::Internal)?
                .as_ref()
                .map(|binding| binding.transport.bloblog_boundary())
                .transpose()?
        } else {
            None
        };
        let mut state = self.shared.state.lock().expect("local completion lock poisoned");
        if generation == 0 || state.generation.is_some_and(|active| active != generation) {
            return Err(HostError::StaleHandle);
        }
        if !admits(state.entries.count(1) + file_calls, state.bytes + file_bytes, bytes) {
            return Err(HostError::LimitExceeded);
        }
        let handle = state.entries.reserve(1, crate::capability::Rights::NONE)?;
        let token = handle.raw();
        let kind = call.operation.kind();
        let volume = crate::store::Id::from_bytes(call.volume);
        let lane = if kind.is_mutation() {
            &self.mutations
        } else if matches!(call.operation, DataOperation::FileRead { .. }) {
            &self.bulk
        } else {
            &self.reads
        };

        let sent = lane.as_ref().ok_or(HostError::Unavailable).and_then(|lane| {
            lane.try_send(Work {
                token,
                call,
                #[cfg(feature = "connected-engine")]
                bloblog,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => HostError::LimitExceeded,
                mpsc::TrySendError::Disconnected(_) => HostError::Unavailable,
            })
        });
        if let Err(error) = sent {
            state.entries.cancel(handle).expect("unsent operation retains reservation");
            return Err(error);
        }
        // The same lock prevents publication before insertion.
        state.generation = Some(generation);
        state.bytes += bytes;
        state
            .entries
            .insert(
                handle,
                Completion::Data(Box::new(Entry {
                    generation,
                    kind,
                    volume,
                    bytes,
                    result_limit,
                    revision: None,
                    result: None,
                    notified: false,
                    abandoned: false,
                    channel_cancel: Arc::new(tokio::sync::Notify::new()),
                })),
            )
            .expect("sent operation retains reservation");

        Ok(token)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        let state = self.shared.state.lock().expect("local completion lock poisoned");
        let entry = state.data_entry(token).ok_or(HostError::StaleHandle)?;
        let result = entry.result.as_ref().ok_or(HostError::WouldBlock)?;
        if result.is_ok() && !entry.kind.is_mutation() && !entry.kind.network() {
            let (current, changed) = (self.shared.revision)(entry.revision, entry.volume);
            if current.is_none() || changed.is_some() {
                return Err(HostError::Unavailable);
            }
        }
        result.clone()
    }
    fn release_data(&mut self, token: u64) {
        let mut state = self.shared.state.lock().expect("local completion lock poisoned");
        if let Some(entry) = state.data_entry_mut(token) {
            if entry.result.is_none() {
                entry.abandoned = true;
                entry.channel_cancel.notify_one();
            } else {
                remove(&mut state, token);
            }
        }
    }
}
impl Drop for InProcessHost {
    fn drop(&mut self) {
        self.shared.files.lock().expect("native File adapter lock poisoned").take();
        let mut state = self.shared.state.lock().expect("local completion lock poisoned");
        state.entries.retain(|_, entry| match entry {
            Completion::Data(entry) => {
                entry.abandoned = true;
                entry.result.is_none()
            }
            Completion::File(_) => true,
        });
        state.bytes = state.data_entries().map(|entry| entry.bytes).sum();
        let pending = !state.entries.is_empty();
        drop(state);
        self.reads.take();
        self.bulk.take();
        self.mutations.take();
        // An abandoned accepted operation keeps executing independently. Normal
        // orderly teardown runs on the resident owner after its work drains;
        // join only idle lanes so completion also releases backend store handles.
        if !pending && let Some(workers) = self.workers.take() {
            for worker in workers {
                let _ = worker.join();
            }
        }
    }
}
fn admits(count: usize, held: usize, charge: usize) -> bool {
    count < MAX_CALLS && charge <= MAX_BYTES && held <= MAX_BYTES - charge
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
    mut host: Box<dyn Host + Send>,
    shared: &Arc<Shared>,
) -> std::io::Result<(mpsc::SyncSender<Work>, std::thread::JoinHandle<()>)> {
    let shared = shared.clone();
    let (send, receive) = mpsc::sync_channel::<Work>(MAX_CALLS);
    let worker = std::thread::Builder::new().name(name.into()).spawn(move || {
        while let Ok(work) = receive.recv() {

            let _span = crate::trace::Span::new("guest.data.execute");

            #[cfg(feature = "connected-engine")]
            if work.call.operation.kind().bloblog() {
                run_bloblog(&shared, work);
                continue;
            }
            #[cfg(feature = "connected-engine")]
            if work.call.operation.network() {
                let binding = shared.channels.lock().expect("channel binding poisoned").clone();
                let result = binding.as_ref().ok_or(HostError::Unsupported).and_then(|binding| {
                    binding
                        .transport
                        .channel_call(
                            binding.store.clone(),
                            binding.session.clone(),
                            binding.build,
                            work.call.operation.clone(),
                        )
                        .map_err(|_| HostError::Unavailable)
                });
                match (binding, result) {
                    (Some(binding), Ok(future)) => {
                        let shared = shared.clone();
                        // Await on the existing network runtime, never on a storage lane.
                        let result = binding.transport.spawn_channel(async move {
                            let cancel=shared.state.lock().expect("channel completion poisoned").data_entry(work.token).map(|entry|(entry.channel_cancel.clone(),entry.abandoned));
                            let result=match cancel {Some((cancel,false))=>tokio::select! {biased;_=cancel.notified()=>Err(HostError::Closed),result=future=>result.map_err(crate::engine::session_channels::host_error)},_=>Err(HostError::Closed)};
                            complete_channel(&shared, work, result);
                        });
                        if result.is_err() {
                            crate::diagnostic!("channel runtime stopped");
                        }
                    }
                    (_, Err(error)) => complete_channel(&shared, work, Err(error)),
                    _ => unreachable!("a channel future retains its transport"),
                }
                continue;
            }
            let (revision, result) = execute_data(host.as_mut(), &shared, &work.call);
            let mut state = shared.state.lock().expect("local completion lock poisoned");
            if let Some(entry) = state.data_entry_mut(work.token) {
                if entry.abandoned {
                    remove(&mut state, work.token);
                } else {

                    entry.revision = revision;
                    entry.result = Some(result.and_then(|result| {
                        if result.retained_bytes().is_none_or(|bytes| bytes > entry.result_limit) {
                            Err(HostError::LimitExceeded)
                        } else {
                            Ok(Arc::new(result))
                        }
                    }));

                }
            }
            drop(state);
            (shared.wake)();
        }
    })?;
    Ok((send, worker))
}

fn execute_data(
    host: &mut dyn Host,
    shared: &Shared,
    call: &DataCall,
) -> (Option<(crate::store::Id, u64)>, HostResult<DataResult>) {
    let volume = crate::store::Id::from_bytes(call.volume);
    let binding = shared.data.lock().expect("native data binding lock poisoned").clone();
    // First use runs on this storage lane, without holding a callback/admission
    // lock. Initialize before the read cut so its own seed cannot invalidate it.
    let prepared = binding.map_or(Ok(()), |binding| {
        binding
            .store
            .materialize_automatic_access(&binding.access, volume)
            .map_err(crate::runtime::store_bridge::error)
    });
    let before = (shared.revision)(None, volume).0;
    let mut result = prepared.and_then(|()| call.clone().execute(host));
    let (revision, changed) = (shared.revision)(before, volume);
    // Fence only the exact target/binding, plus policy/history gaps.
    if result.is_ok()
        && !call.operation.is_mutation()
        && (before.is_none() || revision.is_none() || changed.is_some())
    {
        result = Err(HostError::Unavailable);
    }
    (revision, result)
}

#[cfg(feature = "connected-engine")]
fn complete_channel(shared: &Arc<Shared>, work: Work, result: HostResult<DataResult>) {
    let mut state = shared.state.lock().expect("channel completion poisoned");
    if let Some(entry) = state.data_entry_mut(work.token) {
        if entry.abandoned {
            remove(&mut state, work.token);
        } else {
            if work.call.operation.kind().bloblog() {
                entry.revision = (shared.revision)(None, entry.volume).0;
            }
            entry.result = Some(result.and_then(|result| {
                if result.retained_bytes().is_some_and(|bytes| bytes <= entry.result_limit) {
                    Ok(Arc::new(result))
                } else {
                    Err(HostError::LimitExceeded)
                }
            }));
        }
    }
    drop(state);
    (shared.wake)();
}

#[cfg(feature = "connected-engine")]
fn run_bloblog(shared: &Arc<Shared>, mut work: Work) {
    let binding = shared.channels.lock().expect("host binding poisoned").clone();
    let Some(binding) = binding else {
        complete_channel(shared, work, Err(HostError::Unavailable));
        return;
    };
    let Some(boundary) = work.bloblog.take() else {
        complete_channel(shared, work, Err(HostError::Unavailable));
        return;
    };
    let access = crate::store::Access::session_version(
        binding.store.account().id,
        &binding.session,
        crate::identifier::encode(&binding.build),
    );
    let future = binding.transport.bloblog_call(
        binding.store.clone(),
        access,
        work.call.as_ref().clone(),
        boundary,
    );
    match future {
        Ok(future) => {
            let shared = shared.clone();
            let _ = binding.transport.spawn_channel(async move {
                let cancel = shared
                    .state
                    .lock()
                    .expect("log completion poisoned")
                    .data_entry(work.token)
                    .map(|entry| (entry.channel_cancel.clone(), entry.abandoned));
                let result = match cancel {
                    Some((cancel, false)) => tokio::select! {
                        biased;
                        _ = cancel.notified() => Err(HostError::Uncertain),
                        result = future => result,
                    },
                    _ => Err(HostError::Closed),
                };
                complete_channel(&shared, work, result);
            });
        }
        Err(error) => complete_channel(shared, work, Err(error)),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests control worker gates and assert exact completion ownership."
)]
mod tests {
    use super::*;
    use playbit_api::abi::syscall::Id;
    use std::{
        sync::{
            Condvar,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    type Gate = Arc<(Mutex<bool>, Condvar)>;
    #[test]
    fn actual_local_host_exposes_kv_and_executes_it_on_the_existing_lanes() {
        use crate::store::{Access, LocalStore, volume_index::EntryKind};
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let volume = store.create_volume("KV", false).unwrap().id;
        let access = Access::owner(store.account().id);
        let entry = store.create_database(&access, volume, "settings", EntryKind::KvDb {}).unwrap();
        let backend = crate::engine::data_host::DataHost {
            store: store.clone(),
            access,
            changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (send, wake) = mpsc::sync_channel(1);
        let (mut host, inbox) = InProcessHost::local(
            backend,
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        for opcode in [
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
        ] {
            assert!(host.supported_operations().contains(&opcode));
        }
        let id = *entry.id.as_bytes();
        for operation in [
            DataOperation::KvPut { entry_id: id, key: vec![0, 255], value: vec![0, 128] },
            DataOperation::KvRead {
                entry_id: id,
                key: vec![0, 255],
                offset: 0,
                limit: 2,
                root: None,
            },
            DataOperation::KvKeys { entry_id: id, after: None, root: None, limit: 64 },
            DataOperation::KvDelete { entry_id: id, key: vec![0, 255] },
        ] {
            let kind = operation.kind();
            let call = Arc::new(DataCall { volume: *volume.as_bytes(), operation });
            let token = host.begin_data(1, call.clone()).unwrap();
            wake.recv_timeout(Duration::from_secs(5)).unwrap();
            let result = host.peek_data(token).unwrap();
            call.validate_result(&result, Default::default()).unwrap();
            match (kind, result.as_ref()) {
                (DataKind::KvRead, DataResult::KvRead(chunk)) => assert_eq!(chunk.bytes, [0, 128]),
                (DataKind::KvKeys, DataResult::KvKeys(page)) => {
                    assert_eq!(page.keys[0].bytes, [0, 255])
                }
                (DataKind::KvPut, DataResult::KvWritten)
                | (DataKind::KvDelete, DataResult::KvDeleted) => {}
                _ => panic!("mismatched KV result"),
            }
            host.release_data(token);
        }
        assert!(inbox.idle());
    }
    #[cfg(all(
        target_os = "macos",
        feature = "desktop-ui",
        feature = "native-runtime",
        feature = "connected-engine"
    ))]
    #[test]
    fn native_first_by_id_query_initializes_only_its_scope_and_preserves_edits() {
        // Exercise both real native host setup and local DataHost setup before
        // any name lookup. Reattach each to verify its signed initializer runs
        // lazily once and retained edits and binding authority survive.
        use crate::{
            package::{AutomaticVolumeRole as Role, Package, VolumeInitializers},
            store::{
                Access, AutomaticVolumeState, EntryId, InitialDatabase, InitialDatabaseKind,
                InitialObject, RootInitialization,
                object_contract::{Schema as StoreSchema, TypeDeclaration},
            },
        };
        fn complete(
            host: &mut InProcessHost,
            wake: &mpsc::Receiver<()>,
            volume: crate::store::Id,
            operation: DataOperation,
        ) -> HostResult<Arc<DataResult>> {
            let token = host
                .begin_data(1, Arc::new(DataCall { volume: *volume.as_bytes(), operation }))
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let result = loop {
                match host.peek_data(token) {
                    Err(HostError::WouldBlock) => {
                        wake.recv_timeout(
                            deadline.saturating_duration_since(std::time::Instant::now()),
                        )
                        .unwrap();
                    }
                    result => break result,
                }
            };
            host.release_data(token);
            result
        }
        for (attached, role) in
            [(true, Role::Root), (false, Role::Root), (true, Role::Session), (false, Role::Session)]
        {
            let directory = tempfile::tempdir().unwrap();
            let services = super::super::Services::open(directory.path(), || {}).unwrap();
            let store = &services.store;
            let entry_id = [1; 15];
            let type_id = crate::store::Id::from_bytes([7; 16]);
            let seed = crate::store::Id::from_bytes([8; 16]);
            let initializer = RootInitialization {
                permissions: Default::default(),
                databases: vec![InitialDatabase {
                    id: EntryId::from_bytes(entry_id).unwrap(),
                    name: "tasks".into(),
                    kind: InitialDatabaseKind::ObjectDb {
                        schema: StoreSchema {
                            types: BTreeMap::from([(
                                type_id,
                                TypeDeclaration {
                                    name: "Task".into(),
                                    deprecated: false,
                                    fields: BTreeMap::new(),
                                },
                            )]),
                        },
                        objects: vec![InitialObject { id: seed, type_id, fields: BTreeMap::new() }],
                    },
                }],
                blobs: vec![],
            };
            let initializers = VolumeInitializers::single(role, initializer).unwrap();
            let package = crate::package::signature::testing::signed(
                store,
                Package::create(
                    "native-first-use".into(),
                    "Native first use".into(),
                    b"\0asm\x01\0\0\0".to_vec(),
                    BTreeMap::new(),
                    &initializers,
                )
                .unwrap(),
            )
            .unwrap();
            let session = services.create_local_session(&package, [0x37; 16]).unwrap();
            assert_eq!(session.shared_volume.is_some(), role == Role::Session);
            let volume = if role == Role::Session {
                session.shared_volume.expect("the declared Shared scope is bound")
            } else {
                session.root_volume
            };
            assert!(
                store
                    .automatic_volumes(&session.id)
                    .unwrap()
                    .iter()
                    .all(|slot| slot.state == AutomaticVolumeState::Lazy)
            );
            let (binding, context) =
                super::super::session_binding::Binding::prepare(&services, &session.id, &package)
                    .unwrap();
            let backend = super::super::data_host::DataHost {
                store: store.clone(),
                access: binding.access.clone(),
                changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            let query = DataOperation::Query {
                entry_id,
                type_id: *type_id.as_bytes(),
                after: None,
                limit: 32,
                projection: None,
            };
            for fresh in [true, false] {
                let (send, wake) = mpsc::sync_channel(1);
                let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                    let _ = send.try_send(());
                });
                let (mut host, inbox) = if attached {
                    let blobs = super::super::blob::BlobTransport::local();
                    let local = super::super::host::LocalHost {
                        data: backend.clone(),
                        blobs: blobs.clone(),
                        session: session.id.clone(),
                    };
                    let (mut host, inbox) = InProcessHost::start(
                        Box::new(local.clone()),
                        Box::new(local.clone()),
                        Box::new(local),
                        scoped_revision(store.clone(), Some(binding.delivery_volumes())),
                        notify,
                    )
                    .unwrap();
                    host.enable_files(
                        &services,
                        binding.access.clone(),
                        Some(session.id.clone()),
                        Some(blobs),
                        &context.volumes,
                    )
                    .unwrap();
                    (host, inbox)
                } else {
                    InProcessHost::local(backend.clone(), notify).unwrap()
                };
                let first = complete(&mut host, &wake, volume, query.clone()).expect(
                    "first by-ID query must initialize its lazy scope before the read snapshot",
                );
                let DataResult::Page(page) = first.as_ref() else { panic!("query page") };
                assert!(page.objects.contains(seed.as_bytes()));
                assert_eq!(page.objects.len(), if fresh { 1 } else { 2 });
                for slot in store.automatic_volumes(&session.id).unwrap() {
                    assert_eq!(
                        slot.state,
                        if slot.role == role {
                            AutomaticVolumeState::Ready
                        } else {
                            AutomaticVolumeState::Lazy
                        }
                    );
                }
                if fresh {
                    assert!(matches!(
                        complete(
                            &mut host,
                            &wake,
                            volume,
                            DataOperation::Create {
                                entry_id,
                                type_id: *type_id.as_bytes(),
                                fields: vec![]
                            }
                        )
                        .unwrap()
                        .as_ref(),
                        DataResult::Created(_)
                    ));
                } else {
                    store.record_session_denial(&session, "test revoked session").unwrap();
                    assert_eq!(
                        complete(&mut host, &wake, volume, query.clone()),
                        Err(HostError::AccessDenied)
                    );
                }
                assert!(inbox.idle());
            }
            let denial = store.session_denial_generation(&session).unwrap();
            assert!(store.clear_session_denial(&session, denial).unwrap());
            let mut stale = backend;
            stale.access = Access::session_version(store.account().id, &session.id, "wrong-build");
            let (send, wake) = mpsc::sync_channel(1);
            let (mut host, _) = InProcessHost::local(
                stale,
                Arc::new(move || {
                    let _ = send.try_send(());
                }),
            )
            .unwrap();
            assert_eq!(complete(&mut host, &wake, volume, query), Err(HostError::AccessDenied));
        }
    }
    #[derive(Clone)]
    struct Backend {
        gate: Gate,
        executed: Arc<AtomicUsize>,
    }
    impl Backend {
        fn run(&self) {
            let (lock, changed) = &*self.gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
            self.executed.fetch_add(1, Ordering::Release);
        }
    }
    impl Host for Backend {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
        fn schema(&mut self, _: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
            self.run();
            Ok(Schema { types: vec![] })
        }
        fn edit_fields(
            &mut self,
            _: crate::runtime::host::EntryAddress,
            _: Id,
            _: &[FieldChange],
        ) -> HostResult<()> {
            self.run();
            Ok(())
        }
    }
    struct Fixture {
        host: InProcessHost,
        inbox: Inbox,
        gate: Gate,
        executed: Arc<AtomicUsize>,
        revision: Arc<AtomicU64>,
        wake: mpsc::Receiver<()>,
    }
    fn fixture() -> Fixture {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let executed = Arc::new(AtomicUsize::new(0));
        let revision = Arc::new(AtomicU64::new(1));
        let observed = revision.clone();
        let backend = Backend { gate: gate.clone(), executed: executed.clone() };
        let (send, wake) = mpsc::sync_channel(1);
        let (host, inbox) = InProcessHost::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            Arc::new(move |previous, _| {
                let current =
                    (crate::store::Id::from_bytes([4; 16]), observed.load(Ordering::Acquire));
                (Some(current), (previous != Some(current)).then_some(current.1))
            }),
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        Fixture { host, inbox, gate, executed, revision, wake }
    }
    fn open(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    fn edit() -> Arc<DataCall> {
        Arc::new(DataCall {
            volume: [4; 16],
            operation: DataOperation::Edit {
                entry_id: [5; 15],
                object_id: [5; 16],
                fields: vec![FieldChange {
                    field_id: [6; 16],
                    values: Some(vec![Value::Boolean(true)]),
                }],
            },
        })
    }
    #[test]
    fn blocked_bulk_read_does_not_hold_interactive_storage_lane() {
        #[derive(Clone)]
        struct Isolated(Backend);
        impl Host for Isolated {
            fn supported_operations(&self) -> &[Opcode] {
                &[]
            }
            fn schema(&mut self, _: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
                Ok(Schema { types: vec![] })
            }
            fn read_file(
                &mut self,
                _: crate::runtime::host::EntryAddress,
                _: u64,
                _: u32,
            ) -> HostResult<crate::runtime::host::FileChunk> {
                self.0.run();
                Ok(crate::runtime::host::FileChunk { total_bytes: 4, bytes: b"file".to_vec() })
            }
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let backend =
            Isolated(Backend { gate: gate.clone(), executed: Arc::new(AtomicUsize::new(0)) });
        let (send, wake) = mpsc::sync_channel(1);
        let (mut host, inbox) = InProcessHost::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            Arc::new(|previous, _| {
                let current = (crate::store::Id::from_bytes([4; 16]), 1);
                (Some(current), (previous != Some(current)).then_some(1))
            }),
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        let bulk = host
            .begin_data(
                1,
                Arc::new(DataCall {
                    volume: [4; 16],
                    operation: DataOperation::FileRead { entry_id: [5; 15], offset: 0, limit: 4 },
                }),
            )
            .unwrap();
        let read = host
            .begin_data(
                1,
                Arc::new(DataCall {
                    volume: [4; 16],
                    operation: DataOperation::SchemaRead { entry_id: [5; 15] },
                }),
            )
            .unwrap();
        let ready = wake.recv_timeout(Duration::from_secs(5));
        let interactive = host.peek_data(read);
        let held_bulk = host.peek_data(bulk);
        open(&gate); // Always release the test I/O before assertions/teardown.
        assert!(ready.is_ok());
        assert!(matches!(interactive.as_deref(), Ok(DataResult::Schema(_))));
        assert_eq!(held_bulk, Err(HostError::WouldBlock));
        host.release_data(read);
        while !inbox.has_ready() {
            wake.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(matches!(host.peek_data(bulk).as_deref(), Ok(DataResult::File(_))));
        host.release_data(bulk);
        assert!(inbox.idle());
    }
    #[test]
    fn begin_and_take_do_no_storage_wait_and_results_keep_exact_generation() {
        let mut fixture = fixture();
        let token = fixture.host.begin_data(12, edit()).unwrap();
        assert_eq!(fixture.host.peek_data(token), Err(HostError::WouldBlock));
        assert_eq!(fixture.executed.load(Ordering::Acquire), 0);
        assert_eq!(fixture.host.begin_data(13, edit()), Err(HostError::StaleHandle));
        open(&fixture.gate);
        fixture.wake.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fixture.inbox.ready(), vec![(12, token, false)]);
        assert!(fixture.inbox.ready().is_empty());
        fixture.inbox.defer(token);
        assert_eq!(fixture.inbox.ready(), vec![(12, token, false)]);
        assert_eq!(*fixture.host.peek_data(token).unwrap(), DataResult::Edited);
        fixture.host.release_data(token);
        assert!(fixture.inbox.idle());
        assert_eq!(fixture.host.peek_data(token), Err(HostError::StaleHandle));
        assert!(fixture.host.begin_data(12, edit()).unwrap() > token);
    }
    #[test]
    fn disposed_delivery_still_executes_accepted_mutations_once() {
        let mut fixture = fixture();
        let token = fixture.host.begin_data(1, edit()).unwrap();
        fixture.host.release_data(token);
        assert!(!fixture.inbox.idle(), "execution retains credit after delivery is abandoned");
        drop(fixture.host); // This returns while storage is deliberately blocked.
        assert_eq!(fixture.executed.load(Ordering::Acquire), 0);
        open(&fixture.gate);
        fixture.wake.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fixture.executed.load(Ordering::Acquire), 1);
        assert!(fixture.inbox.idle());
        assert!(fixture.inbox.ready().is_empty());
    }
    #[test]
    fn held_reads_are_invalidated_and_known_mutation_acks_are_preserved() {
        let mut fixture = fixture();
        open(&fixture.gate);
        let read = fixture
            .host
            .begin_data(
                1,
                Arc::new(DataCall {
                    volume: [4; 16],
                    operation: DataOperation::SchemaRead { entry_id: [5; 15] },
                }),
            )
            .unwrap();
        fixture.wake.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(fixture.host.peek_data(read).is_ok());
        fixture.revision.store(2, Ordering::Release);
        assert_eq!(fixture.host.peek_data(read), Err(HostError::Unavailable));
        fixture.host.release_data(read);
        let edit = fixture.host.begin_data(1, edit()).unwrap();
        fixture.wake.recv_timeout(Duration::from_secs(5)).unwrap();
        fixture.revision.store(3, Ordering::Release);
        assert_eq!(*fixture.host.peek_data(edit).unwrap(), DataResult::Edited);
        assert_eq!(fixture.executed.load(Ordering::Acquire), 2);
        fixture.host.release_data(edit);
    }
    fn commit_schema(store: &crate::store::LocalStore, volume: crate::store::Id, value: u8) {
        store
            .establish_schema(
                &crate::store::Access::owner(store.account().id),
                crate::engine::tests::object_address(volume),
                &crate::store::object_contract::Schema {
                    types: BTreeMap::from([(
                        crate::store::Id::from_bytes([value; 16]),
                        crate::store::object_contract::TypeDeclaration {
                            name: "Scoped native declaration".into(),
                            deprecated: false,
                            fields: Default::default(),
                        },
                    )]),
                },
            )
            .unwrap();
    }
    #[test]
    fn native_delivery_retains_unrelated_commits_and_rejects_target_changes() {
        let profile = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::LocalStore::open(profile.path()).unwrap());
        let root = crate::engine::tests::object_volume(&store, "Read target", false).id;
        let other = crate::engine::tests::object_volume(&store, "Unrelated", false).id;
        let backend = super::super::data_host::DataHost {
            store: store.clone(),
            access: crate::store::Access::owner(store.account().id),
            changed: Arc::default(),
        };
        let (send, ready) = mpsc::sync_channel(1);
        let (mut host, _) = InProcessHost::local(
            backend,
            Arc::new(move || {
                let _ = send.try_send(());
            }),
        )
        .unwrap();
        let token = host
            .begin_data(
                1,
                Arc::new(DataCall {
                    volume: *root.as_bytes(),
                    operation: DataOperation::SchemaRead { entry_id: [5; 15] },
                }),
            )
            .unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let held = host.peek_data(token).unwrap();
        commit_schema(&store, other, 8);
        assert!(Arc::ptr_eq(&held, &host.peek_data(token).unwrap()));
        commit_schema(&store, root, 9);
        assert_eq!(host.peek_data(token), Err(HostError::Unavailable));
        host.release_data(token);
    }
    #[test]
    fn native_worker_fences_binding_changes_without_reexecuting_accepted_reads() {
        #[derive(Clone)]
        struct Blocked {
            backend: super::super::data_host::DataHost,
            gate: Gate,
            started: mpsc::SyncSender<()>,
            calls: Arc<AtomicUsize>,
        }
        impl Host for Blocked {
            fn supported_operations(&self) -> &[Opcode] {
                &[]
            }
            fn schema(&mut self, volume: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
                self.calls.fetch_add(1, Ordering::Release);
                self.started.send(()).unwrap();
                let mut released = self.gate.0.lock().unwrap();
                while !*released {
                    released = self.gate.1.wait(released).unwrap();
                }
                self.backend.schema(volume)
            }
        }
        let profile = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::LocalStore::open(profile.path()).unwrap());
        let target = crate::engine::tests::object_volume(&store, "Target", false).id;
        let root = crate::engine::tests::object_volume(&store, "Bound root", false).id;
        let other = crate::engine::tests::object_volume(&store, "Unrelated", false).id;
        let calls = Arc::new(AtomicUsize::new(0));
        for (index, changed) in [other, root, target].into_iter().enumerate() {
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let (entered, started) = mpsc::sync_channel(1);
            let backend = Blocked {
                backend: super::super::data_host::DataHost {
                    store: store.clone(),
                    access: crate::store::Access::owner(store.account().id),
                    changed: Arc::default(),
                },
                gate: gate.clone(),
                started: entered,
                calls: calls.clone(),
            };
            let (send, ready) = mpsc::sync_channel(1);
            let (mut host, _) = InProcessHost::start(
                Box::new(backend.clone()),
                Box::new(backend.clone()),
                Box::new(backend),
                scoped_revision(store.clone(), Some([root; 6])),
                Arc::new(move || {
                    let _ = send.try_send(());
                }),
            )
            .unwrap();
            let token = host
                .begin_data(
                    1,
                    Arc::new(DataCall {
                        volume: *target.as_bytes(),
                        operation: DataOperation::SchemaRead { entry_id: [5; 15] },
                    }),
                )
                .unwrap();
            let entered = started.recv_timeout(Duration::from_secs(5));
            if entered.is_ok() {
                commit_schema(&store, changed, 16 + index as u8);
            }
            open(&gate); // Release accepted I/O even if an observation failed.
            entered.unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            if changed == other {
                assert!(matches!(host.peek_data(token).as_deref(), Ok(DataResult::Schema(_))));
            } else {
                assert_eq!(host.peek_data(token), Err(HostError::Unavailable));
            }
            host.release_data(token);
            assert_eq!(calls.load(Ordering::Acquire), index + 1);
        }
    }
    #[test]
    fn saturation_keeps_credit_until_accepted_work_reaches_terminal_state() {
        let mut fixture = fixture();
        for _ in 0..MAX_CALLS {
            fixture.host.begin_data(1, edit()).unwrap();
        }
        assert_eq!(fixture.host.begin_data(1, edit()), Err(HostError::LimitExceeded));
        fixture.host.release_data(1);
        assert_eq!(fixture.host.begin_data(1, edit()), Err(HostError::LimitExceeded));
        drop(fixture.host);
        open(&fixture.gate);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !fixture.inbox.idle() {
            fixture
                .wake
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
        }
        assert_eq!(fixture.executed.load(Ordering::Acquire), MAX_CALLS);
    }
}
