//! Accepted storage work has engine ownership through terminal commit/abort.
//! Socket tickets own delivery only. Bounded workers never execute guest code.
use super::{Context, Key, Outcome};
use crate::{
    engine::{Result, completion::Wake, host::LocalHost},
    runtime::host::{DataCall, DataKind, DataOperation, DataResult, HostResult},
    store::Id,
};
use playbit_api::abi::syscall::Error as HostError;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

const MAX_CALLS: usize = 1024;
const MAX_CONTEXT_CALLS: usize = 64;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STORAGE_SCRATCH: usize = 128 * 1024 * 1024;
const MAX_BULK_SCRATCH: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct Order {
    pub operation: u64,
    pub mutation: u64,
}

struct Mailbox {
    cancel: tokio::sync::Notify,
    ready: AtomicBool,
    abandoned: AtomicBool,
    result: Mutex<Option<Completion>>,
    wake: Arc<Wake>,
}
struct Completion {
    revision: Option<(Id, u64)>,
    result: HostResult<DataResult>,
}
pub(crate) struct Ticket {
    pub context: Context,
    volume: Id,
    pub operation: u64,
    pub mutation: u64,
    pub kind: u32,
    mailbox: Arc<Mailbox>,
}
impl Ticket {
    pub fn ready(&self) -> bool {
        self.mailbox.ready.load(Ordering::Acquire)
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.mailbox.abandoned.store(true, Ordering::Release);
        self.mailbox.cancel.notify_one();
        self.mailbox.wake.notify();
    }
}
pub(crate) struct Work {
    bloblog: Option<crate::engine::bloblog::CallBoundary>,
    call: DataCall,
    host: LocalHost,
    result_bytes: usize,
    binding: Binding,
    mailbox: Arc<Mailbox>,
    _publish: Publish,
}
// Accepted ownership carries its terminal wake through every exit, including
// panic or failure to hand work to the network worker. It never reruns a call.
struct Publish(Arc<Mailbox>);
impl Drop for Publish {
    fn drop(&mut self) {
        self.0.ready.store(true, Ordering::Release);
        self.0.wake.notify();
    }
}
pub(crate) enum Binding {
    SessionInstance(crate::engine::session_instance::SessionInstance),
    Tool(crate::engine::tool_context::Binding),
}
impl From<crate::engine::session_instance::SessionInstance> for Binding {
    fn from(instance: crate::engine::session_instance::SessionInstance) -> Self {
        Self::SessionInstance(instance)
    }
}
impl Binding {
    fn check(
        &self,
        host: &LocalHost,
        volume: Id,
        rights: crate::runtime::host::VolumeRights,
    ) -> HostResult<()> {
        match self {
            Self::SessionInstance(instance) => super::check_binding(instance, host, volume, rights),
            Self::Tool(binding) => {
                binding.check(&host.data.store)?;
                host.data
                    .store
                    .materialize_automatic_access(&host.data.access, volume)
                    .map_err(crate::runtime::store_bridge::error)
            }
        }
    }
    fn volumes(&self, store: &crate::store::LocalStore, volume: Id) -> Result<Vec<Id>> {
        match self {
            Self::SessionInstance(instance) => super::instance_delivery_volumes(store, instance),
            Self::Tool(binding) => Ok(binding.delivery_volumes(volume)),
        }
    }
}
impl Work {
    pub(crate) fn run(mut self) {
        let _span = crate::trace::Span::new("engine.service.execute");

        if self.call.operation.kind().bloblog() {
            self.run_bloblog();
            return;
        }

        if self.call.operation.network() {
            let store = self.host.data.store.clone();
            let session = self.host.session.clone();
            let transport = self.host.blobs.clone();
            let future = match &self.binding {
                Binding::SessionInstance(instance) => crate::identifier::decode(&instance.build)
                    .map_err(|error| error.into())
                    .and_then(|build| {
                        transport.channel_call(store, session, build, self.call.operation.clone())
                    }),
                Binding::Tool(_) => Err("tool contexts cannot own session channels".into()),
            };
            match future {
                Ok(future) => {
                    let _ = transport.spawn_channel(async move {
                        let cancelled=self.mailbox.cancel.notified();
                        let result=if self.mailbox.abandoned.load(Ordering::Acquire) {Err(HostError::Closed)} else {tokio::select! {biased;_=cancelled=>Err(HostError::Closed),result=future=>result.map_err(crate::engine::session_channels::host_error)}};
                        let publish=self._publish;
                        Self::complete(&self.mailbox,result,None);
                        drop(publish);
                    });
                }
                Err(_) => Self::complete(&self.mailbox, Err(HostError::Unavailable), None),
            }
            return;
        }
        let volume = Id::from_bytes(self.call.volume);
        let rights = self.call.operation.rights();
        let kind = self.call.operation.kind();
        let entry = self.call.operation.entry_id();
        let delivery = Delivery { volume, kind, entry, result_bytes: self.result_bytes };
        let mut revision = None;
        let result = self
            .binding
            .check(&self.host, volume, rights)
            .and_then(|_| {
                // Fence the whole read, including execution. Mutations retain
                // this marker so later retirement cannot obscure a known commit.
                revision =
                    Some(self.host.data.store.delivery_revision().ok_or(HostError::Unavailable)?);
                if self.call.operation.tool_only() {
                    match &self.binding {
                        Binding::Tool(binding) => binding.execute(self.call, &mut self.host.data),
                        Binding::SessionInstance(_) => Err(HostError::Unsupported),
                    }
                } else {
                    self.call.execute(&mut self.host)
                }
            })
            .and_then(|result| delivery.finish(&self.binding, &self.host, &mut revision, result));
        Self::complete(&self.mailbox, result, revision);
    }
    fn complete(
        mailbox: &Arc<Mailbox>,
        result: HostResult<DataResult>,
        revision: Option<(Id, u64)>,
    ) {
        if let Ok(mut output) = mailbox.result.lock() {
            *output = Some(Completion { revision, result });
        }
    }

    fn run_bloblog(self) {
        let volume = Id::from_bytes(self.call.volume);
        let mut revision = self.host.data.store.delivery_revision();
        let delivery = Delivery {
            volume,
            kind: self.call.operation.kind(),
            entry: self.call.operation.entry_id(),
            result_bytes: self.result_bytes,
        };
        let future =
            self.binding.check(&self.host, volume, self.call.operation.rights()).and_then(|_| {
                self.host.blobs.bloblog_call(
                    self.host.data.store.clone(),
                    self.host.data.access.clone(),
                    self.call,
                    self.bloblog.expect("bloblog admission captures its connection"),
                )
            });
        let transport = self.host.blobs.clone();
        let Ok(future) = future else {
            Self::complete(&self.mailbox, Err(future.err().expect("failed admission")), revision);
            return;
        };
        let _ = transport.spawn_channel(async move {
            let publish = self._publish;
            let result = if self.mailbox.abandoned.load(Ordering::Acquire) {
                Err(HostError::Closed)
            } else {
                tokio::select! {
                    biased;
                    _ = self.mailbox.cancel.notified() => Err(HostError::Uncertain),
                    result = future => result,
                }
            };
            if !delivery.kind.is_mutation() {
                revision = self.host.data.store.delivery_revision();
            }
            let result = result.and_then(|result| {
                delivery.finish(&self.binding, &self.host, &mut revision, result)
            });
            Self::complete(&self.mailbox, result, revision);
            drop(publish);
        });
    }
}
// Execution consumes its call. Retain only the scalar delivery scope, never a
// second payload, to validate the terminal result without repeating any I/O.
struct Delivery {
    volume: Id,
    kind: DataKind,
    entry: Option<[u8; 15]>,
    result_bytes: usize,
}
impl Delivery {
    fn finish(
        &self,
        binding: &Binding,
        host: &LocalHost,
        revision: &mut Option<(Id, u64)>,
        result: DataResult,
    ) -> HostResult<DataResult> {
        if self.kind.is_mutation() {
            if revision.is_none() || !result.is_receipt(self.kind) {
                return Err(HostError::Internal);
            }
        } else {
            let before = revision.ok_or(HostError::Unavailable)?;
            binding.check(host, self.volume, self.kind.rights())?;
            super::check_result(host, self.volume, self.entry, &result)?;
            let volumes = binding
                .volumes(&host.data.store, self.volume)
                .map_err(|_| HostError::AccessDenied)?;
            let (current, changed) = super::read_delivery_revision(
                &host.data.store,
                self.kind as u32,
                Some(before),
                &volumes,
            );
            if current.is_none() || changed.is_some() {
                // Never run the operation again to obtain a newer result.
                return Err(HostError::Unavailable);
            }
            *revision = Some(before);
        }
        if result.retained_bytes().is_none_or(|bytes| bytes > self.result_bytes) {
            Err(HostError::LimitExceeded)
        } else {
            Ok(result)
        }
    }
}
struct Pool {
    send: Option<mpsc::SyncSender<Work>>,
    threads: Vec<JoinHandle<()>>,
}
impl Pool {
    fn start(count: usize, name: &str) -> Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Work>(count);
        let receive = Arc::new(Mutex::new(receive));
        let mut pool = Self { send: Some(send), threads: Vec::new() };
        for index in 0..count {
            let receive = receive.clone();
            let thread =
                std::thread::Builder::new().name(format!("{name}-{index}")).spawn(move || {
                    loop {
                        let job = match receive.lock() {
                            Ok(queue) => queue.recv(),
                            Err(_) => return,
                        };
                        match job {
                            Ok(work) => work.run(),
                            Err(_) => return,
                        }
                    }
                })?;
            pool.threads.push(thread);
        }
        Ok(pool)
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.send.take();
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                crate::diagnostic!("Storage worker stopped after a panic");
            }
        }
    }
}
#[derive(Default)]
struct Sequence {
    frontier: u64,
    channels: super::ChannelSequences,
    ahead: BTreeSet<u64>,
    mutation: u64,
}
impl Sequence {
    fn admit(&mut self, operation: u64, mutation: u64, mutating: bool) -> HostResult<()> {
        if !valid_order(self.frontier, self.mutation, operation, mutation, mutating)
            || self.ahead.contains(&operation)
        {
            return Err(HostError::StaleHandle);
        }
        self.ahead.insert(operation);
        while let Some(next) = self.frontier.checked_add(1).filter(|next| self.ahead.contains(next))
        {
            self.ahead.remove(&next);
            self.frontier = next;
        }
        if mutating {
            self.mutation = mutation;
        }
        Ok(())
    }
}
fn valid_order(
    frontier: u64,
    previous: u64,
    operation: u64,
    mutation: u64,
    mutating: bool,
) -> bool {
    operation
        .checked_sub(frontier)
        .is_some_and(|distance| distance > 0 && distance <= MAX_CONTEXT_CALLS as u64)
        && if mutating { previous.checked_add(1) == Some(mutation) } else { mutation == 0 }
}
fn admits_credit(
    count: usize,
    bytes: usize,
    context_count: usize,
    context_bytes: usize,
    charge: usize,
) -> bool {
    count < MAX_CALLS
        && context_count < MAX_CONTEXT_CALLS
        && bytes.checked_add(charge).is_some_and(|sum| sum <= MAX_BYTES)
        && context_bytes.checked_add(charge).is_some_and(|sum| sum <= MAX_CONTEXT_BYTES)
}
struct Accepted {
    log: Option<(Id, [u8; 15])>,
    context: Key,
    mailbox: Arc<Mailbox>,
    bytes: usize,
    scratch: usize,
    bulk: bool,
    kind: DataKind,
    mutation: bool,
    started: bool,
    finished: bool,
    work: Option<Work>,
}
impl Accepted {
    fn uses_network_worker(&self) -> bool {
        self.kind.network() || self.kind.bloblog()
    }
}
pub(crate) struct Executor {
    storage: Option<Pool>,
    bulk: Option<Pool>,
    accepted: BTreeMap<u64, Accepted>,
    order: VecDeque<u64>,
    sequences: BTreeMap<Key, Sequence>,
    next: u64,
    bytes: usize,
    storage_scratch: usize,
    bulk_scratch: usize,
}
impl Default for Executor {
    fn default() -> Self {
        Self {
            storage: None,
            bulk: None,
            accepted: BTreeMap::new(),
            order: VecDeque::new(),
            sequences: BTreeMap::new(),
            next: 1,
            bytes: 0,
            storage_scratch: 0,
            bulk_scratch: 0,
        }
    }
}

impl Executor {
    pub fn context_pending(&self, context: impl Into<Context>) -> bool {
        let context = context.into();
        self.accepted.values().any(|work| {
            work.context == context.key() && !work.mailbox.ready.load(Ordering::Acquire)
        })
    }
    pub fn cancel_channels(&self, context: Context) {
        for work in self
            .accepted
            .values()
            .filter(|work| work.context == context.key() && work.kind.network())
        {
            // The caller still owns its ticket and must receive the terminal result.
            // Only dropping that ticket makes the completion reclaimable.
            work.mailbox.cancel.notify_one();
        }
    }
    pub fn record_request(
        &mut self,
        context: impl Into<Context>,
        operation: u64,
        mutation: u64,
        call: &DataCall,
        live: impl Fn(Key) -> bool,
    ) -> HostResult<()> {
        let context = context.into();
        if !self.sequences.contains_key(&context.key()) {
            // Only fresh contexts need bookkeeping collection. Accepted work
            // keeps its mailbox/order/credit independently of these counters.
            // A retired handle has already lost registry authority, so deleting
            // its counters cannot authorize replay through begin_service.
            self.sequences.retain(|handle, _| live(*handle));
            if self.sequences.len() >= MAX_CALLS {
                return Err(HostError::LimitExceeded);
            }
        }
        let sequence = self.sequences.entry(context.key()).or_default();
        if call.operation.network() {
            let mut channels = sequence.channels;
            if mutation != 0 || operation != channels.advance(call.operation.kind())? {
                return Err(HostError::StaleHandle);
            }
            sequence.channels = channels;
            return Ok(());
        }
        if operation >= super::CHANNEL_BIT {
            return Err(HostError::StaleHandle);
        }
        sequence.admit(operation, mutation, call.operation.is_mutation())
    }
    pub fn begin(
        &mut self,
        context: impl Into<Context>,
        order: Order,
        call: DataCall,
        host: LocalHost,
        binding: impl Into<Binding>,
        wake: Arc<Wake>,
    ) -> HostResult<Ticket> {
        let context = context.into();
        let binding = binding.into();
        let Order { operation, mutation } = order;
        call.operation.validate(Default::default())?;
        if call.operation.kind() == DataKind::AuthenticatedField {
            return Err(HostError::Unsupported);
        }
        let result_bytes = call.completion_bytes();
        let bytes = call
            .retained_bytes()
            .and_then(|input| input.checked_add(result_bytes))
            .ok_or(HostError::LimitExceeded)?;
        let held: Vec<_> =
            self.accepted.values().filter(|work| work.context == context.key()).collect();
        let context_bytes: usize = held.iter().map(|work| work.bytes).sum();
        if !admits_credit(self.accepted.len(), self.bytes, held.len(), context_bytes, bytes) {
            return Err(HostError::LimitExceeded);
        }
        let bloblog =
            call.operation.kind().bloblog().then(|| host.blobs.bloblog_boundary()).transpose()?;
        let id = self.next;
        self.next = id.checked_add(1).ok_or(HostError::LimitExceeded)?;
        let bulk = matches!(
            call.operation,
            DataOperation::FileRead { .. }
                | DataOperation::FileWrite { .. }
                | DataOperation::FileTransferStart { .. }
                | DataOperation::FileTransferChunk { .. }
                | DataOperation::FileTransferCommit { .. }
        );
        let scratch = if bulk {
            3 * crate::engine::blob::MAX_FILE_BYTES as usize
        } else {
            playbit_api::abi::syscall::MAX_QUERY_SCRATCH_BYTES
        };
        let mailbox = Arc::new(Mailbox {
            cancel: Default::default(),
            ready: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            result: Mutex::new(None),
            wake,
        });
        let ticket = Ticket {
            context,
            volume: Id::from_bytes(call.volume),
            operation,
            mutation,
            kind: call.operation.kind() as u32,
            mailbox: mailbox.clone(),
        };
        let mutating = call.operation.is_mutation();

        self.accepted.insert(
            id,
            Accepted {
                log: (call.operation.kind() == DataKind::BloblogAppend).then(|| {
                    (Id::from_bytes(call.volume), call.operation.entry_id().expect("log address"))
                }),
                context: context.key(),
                mailbox: mailbox.clone(),
                bytes,
                scratch,
                bulk,
                kind: call.operation.kind(),
                mutation: mutating,
                started: false,
                finished: false,
                work: Some(Work {
                    bloblog,
                    call,
                    host,
                    result_bytes,
                    binding,
                    _publish: Publish(mailbox.clone()),
                    mailbox,
                }),
            },
        );
        self.order.push_back(id);
        self.bytes += bytes;

        Ok(ticket)
    }
    /// Collect terminal work and schedule fair bounded starts. No socket wait,
    /// storage read, guest turn or mutation retry runs on this owner thread.
    pub fn maintain(&mut self) -> Result<bool> {
        let mut completed = false;
        for work in self.accepted.values_mut() {
            if work.started && !work.finished && work.mailbox.ready.load(Ordering::Acquire) {
                work.finished = true;
                if work.uses_network_worker() {
                    // The sole existing network worker owns no storage pool
                    // scratch. Retained inputs/results still consume credit.
                } else if work.bulk {
                    self.bulk_scratch -= work.scratch;
                } else {
                    self.storage_scratch -= work.scratch;
                }
                completed |= work.mutation;
            }
        }
        let discarded: Vec<_> = self
            .accepted
            .iter()
            .filter(|(_, work)| work.finished && work.mailbox.abandoned.load(Ordering::Acquire))
            .map(|(id, _)| *id)
            .collect();
        for id in discarded {
            self.remove(id);
        }
        for id in self.order.iter().copied().collect::<Vec<_>>() {
            let work = &self.accepted[&id];
            if work.started {
                continue;
            }
            if self.blocked(id) {
                continue;
            }
            let used = if work.bulk { self.bulk_scratch } else { self.storage_scratch };
            let limit = if work.bulk { MAX_BULK_SCRATCH } else { MAX_STORAGE_SCRATCH };
            if work.scratch > limit - used {
                continue;
            }
            let pool = if work.bulk { &mut self.bulk } else { &mut self.storage };
            if pool.is_none() {
                *pool = Some(Pool::start(
                    if work.bulk { 4 } else { 8 },
                    if work.bulk { "service-bulk" } else { "service-storage" },
                )?);
            }
            let work = self.accepted.get_mut(&id).expect("accepted queue owns its work");
            let input = work.work.take().expect("unstarted work retains its input");
            match pool
                .as_ref()
                .expect("worker pool was started")
                .send
                .as_ref()
                .expect("live pool owns admission")
                .try_send(input)
            {
                Ok(()) => {
                    work.started = true;
                    if !work.uses_network_worker() {
                        if work.bulk {
                            self.bulk_scratch += work.scratch;
                        } else {
                            self.storage_scratch += work.scratch;
                        }
                    }
                }
                Err(mpsc::TrySendError::Full(input)) => work.work = Some(input),
                Err(mpsc::TrySendError::Disconnected(input)) => {
                    work.work = Some(input);
                    return Err(
                        "Storage executor stopped; accepted calls must not be replayed".into()
                    );
                }
            }
        }
        Ok(completed)
    }
    fn blocked(&self, id: u64) -> bool {
        let work = &self.accepted[&id];
        (work.mutation || work.log.is_some())
            && self.order.iter().take_while(|earlier| **earlier != id).any(|earlier| {
                let earlier = &self.accepted[earlier];
                !earlier.finished
                    && match (work.log, earlier.log) {
                        (Some(log), Some(previous)) => log == previous,
                        (None, None) => earlier.context == work.context && earlier.mutation,
                        _ => false,
                    }
            })
    }
    fn remove(&mut self, id: u64) {
        let work = self.accepted.remove(&id).expect("completed work owns credit");
        self.bytes -= work.bytes;
        self.order.retain(|entry| *entry != id);
    }
    pub fn take(&mut self, ticket: Ticket) -> HostResult<Outcome> {
        if !ticket.ready() {
            return Err(HostError::WouldBlock);
        }
        let id = self
            .accepted
            .iter()
            .find(|(_, work)| Arc::ptr_eq(&work.mailbox, &ticket.mailbox))
            .map(|(id, _)| *id)
            .ok_or(HostError::StaleHandle)?;
        self.maintain().map_err(|_| HostError::Internal)?;
        let Completion { revision, result, .. } = ticket
            .mailbox
            .result
            .lock()
            .map_err(|_| HostError::Internal)?
            .take()
            .unwrap_or(Completion { revision: None, result: Err(HostError::Internal) });
        self.remove(id);
        let (status, result) = match result {
            Ok(result) => (0, Some(result)),
            Err(error) => (error as i32, None),
        };
        Ok(Outcome {
            context: ticket.context,
            volume: ticket.volume,
            operation: ticket.operation,
            mutation: ticket.mutation,
            kind: ticket.kind,
            revision,
            status,
            result,
        })
    }
    pub fn pending(&self) -> bool {
        self.accepted.values().any(|work| !work.finished && work.kind != DataKind::ChannelEvents)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert exact bounded sequence and admission results.")]
mod tests {
    use super::*;
    #[test]
    fn worker_kv_cuts_survive_content_but_recheck_read_and_current_authority() {
        // Execute each root-bound KV read, write later content, then withdraw
        // a real reader's ACL before the same worker delivery check.
        use crate::engine::{Engine, data_host::DataHost, service_call::ToolScope};
        use crate::store::{
            Access,
            object_contract::EntryAcl,
            volume_index::{EntryAddress, EntryKind},
        };
        use std::os::unix::net::UnixStream;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let owner = Access::owner(store.account().id);
        let root = crate::engine::tests::object_volume(&store, "KV delivery", false);
        let address = EntryAddress {
            volume: root.id,
            entry: store
                .create_database(&owner, root.id, "settings", EntryKind::KvDb {})
                .unwrap()
                .id,
        };
        store.kv_put(&owner, address, b"selected", b"initial").unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (_client, peer) = UnixStream::pair().unwrap();
        let context = engine.tool_contexts.open(&store, ToolScope::Volume(root.id), &peer).unwrap();
        let binding = engine.tool_contexts.authorize(context, &peer).unwrap();
        let mut host = LocalHost {
            data: DataHost {
                store: store.clone(),
                access: binding.access(),
                changed: Arc::default(),
            },
            blobs: engine.network.blobs.clone(),
            session: session.id,
        };
        let binding = Binding::Tool(binding);
        let reader = Id::from_bytes([88; 16]);
        store.grant_volume(&owner, root.id, reader, false).unwrap();
        for keys in [false, true] {
            store.set_entry_permissions(&owner, root.id, address.entry, EntryAcl::Inherit).unwrap();
            let call = DataCall {
                volume: *root.id.as_bytes(),
                operation: if keys {
                    DataOperation::KvKeys {
                        entry_id: *address.entry.as_bytes(),
                        after: None,
                        root: None,
                        limit: 64,
                    }
                } else {
                    DataOperation::KvRead {
                        entry_id: *address.entry.as_bytes(),
                        key: b"selected".to_vec(),
                        offset: 0,
                        limit: 64,
                        root: None,
                    }
                },
            };
            let delivery = Delivery {
                volume: root.id,
                kind: call.operation.kind(),
                entry: Some(*address.entry.as_bytes()),
                result_bytes: call.completion_bytes(),
            };
            let mut revision = store.delivery_revision();
            let result = call.execute(&mut host).unwrap();
            store
                .kv_put(&owner, address, b"other", if keys { b"after keys" } else { b"after read" })
                .unwrap();
            assert_eq!(
                delivery.finish(&binding, &host, &mut revision, result.clone()),
                Ok(result.clone())
            );
            let reader_host = LocalHost {
                data: DataHost {
                    store: store.clone(),
                    access: Access::owner(reader),
                    changed: Arc::default(),
                },
                blobs: engine.network.blobs.clone(),
                session: host.session.clone(),
            };
            assert_eq!(
                super::super::check_result(
                    &reader_host,
                    root.id,
                    Some(*address.entry.as_bytes()),
                    &result
                ),
                Ok(())
            );
            store
                .set_entry_permissions(
                    &owner,
                    root.id,
                    address.entry,
                    EntryAcl::Replace(Default::default()),
                )
                .unwrap();
            assert_eq!(
                super::super::check_result(
                    &reader_host,
                    root.id,
                    Some(*address.entry.as_bytes()),
                    &result
                ),
                Err(HostError::AccessDenied)
            );
            assert_eq!(
                delivery.finish(&binding, &host, &mut revision, result),
                Err(HostError::AccessDenied)
            );
        }
    }
    #[test]
    fn worker_delivery_preserves_a_real_commit_after_its_binding_is_denied() {
        // Execute the actual storage call, then deny its session before the
        // worker's terminal delivery check; no race timer or replay is involved.
        use crate::engine::{Engine, data_host::DataHost, service_call::ToolScope};
        use std::os::unix::net::UnixStream;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root =
            crate::engine::tests::object_volume(&engine.services.store, "Receipt fixture", false);
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (_client, peer) = UnixStream::pair().unwrap();
        let context = engine
            .tool_contexts
            .open(&engine.services.store, ToolScope::Session(session.id.clone()), &peer)
            .unwrap();
        let binding = engine.tool_contexts.authorize(context, &peer).unwrap();
        let mut host = LocalHost {
            data: DataHost {
                store: engine.services.store.clone(),
                access: binding.access(),
                changed: Arc::default(),
            },
            blobs: engine.network.blobs.clone(),
            session: session.id.clone(),
        };
        let binding = Binding::Tool(binding);
        binding.check(&host, root.id, crate::runtime::host::VolumeRights::SCHEMA).unwrap();
        let mut revision = host.data.store.delivery_revision();
        let call = DataCall {
            volume: *root.id.as_bytes(),
            operation: DataOperation::SchemaEstablish {
                entry_id: [5; 15],
                schema: Default::default(),
            },
        };
        let result_bytes = call.completion_bytes();
        let result = call.execute(&mut host).unwrap();
        host.data.store.record_session_denial(&session, "Removed after commit").unwrap();
        assert_eq!(
            binding.check(&host, root.id, crate::runtime::host::VolumeRights::SCHEMA),
            Err(HostError::AccessDenied)
        );
        let delivery = Delivery {
            volume: root.id,
            kind: DataKind::SchemaEstablish,
            entry: Some([5; 15]),
            result_bytes,
        };
        assert_eq!(
            delivery.finish(&binding, &host, &mut revision, result),
            Ok(DataResult::SchemaEstablished)
        );
        let read = Delivery { kind: DataKind::SchemaRead, ..delivery };
        assert_eq!(
            read.finish(&binding, &host, &mut revision, DataResult::Schema(Default::default())),
            Err(HostError::AccessDenied)
        );
        assert_eq!(
            host.data
                .store
                .schema(
                    &crate::store::Access::owner(host.data.store.account().id),
                    crate::engine::tests::object_address(root.id)
                )
                .unwrap(),
            Default::default()
        );
    }
    #[test]
    fn worker_read_fences_changes_between_execution_and_delivery() {
        use crate::engine::{Engine, data_host::DataHost, service_call::ToolScope};
        use std::os::unix::net::UnixStream;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root =
            crate::engine::tests::object_volume(&engine.services.store, "Receipt fixture", false);
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (_client, peer) = UnixStream::pair().unwrap();
        let context = engine
            .tool_contexts
            .open(&engine.services.store, ToolScope::Session(session.id.clone()), &peer)
            .unwrap();
        let binding = engine.tool_contexts.authorize(context, &peer).unwrap();
        let mut host = LocalHost {
            data: DataHost {
                store: engine.services.store.clone(),
                access: binding.access(),
                changed: Arc::default(),
            },
            blobs: engine.network.blobs.clone(),
            session: session.id.clone(),
        };
        let binding = Binding::Tool(binding);
        use crate::runtime::host::{Schema, TypeDeclaration};
        let read = || DataCall {
            volume: *root.id.as_bytes(),
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        let delivery = Delivery {
            volume: root.id,
            kind: DataKind::SchemaRead,
            entry: Some([5; 15]),
            result_bytes: read().completion_bytes(),
        };
        let mut revision = host.data.store.delivery_revision();
        let stale = read().execute(&mut host).unwrap();
        assert_eq!(stale, DataResult::Schema(Schema::default()));
        // A second worker commits after the read but before its terminal check.
        let schema = Schema {
            types: vec![TypeDeclaration {
                id: [1; 16],
                name: "Item".into(),
                deprecated: false,
                fields: vec![],
            }],
        };
        DataCall {
            volume: *root.id.as_bytes(),
            operation: DataOperation::SchemaEstablish { entry_id: [5; 15], schema: schema.clone() },
        }
        .execute(&mut host)
        .unwrap();
        assert_ne!(revision, host.data.store.delivery_revision());
        assert_eq!(
            delivery.finish(&binding, &host, &mut revision, stale),
            Err(HostError::Unavailable)
        );
        // An explicit fresh read succeeds; delivery never replays the stale one.
        revision = host.data.store.delivery_revision();
        let current = read().execute(&mut host).unwrap();
        assert_eq!(
            delivery.finish(&binding, &host, &mut revision, current),
            Ok(DataResult::Schema(schema))
        );
    }
    fn context(handle: Id) -> crate::engine::session_instance::Context {
        crate::engine::session_instance::Context {
            instance: Id::from_bytes([1; 16]),
            handle,
            incarnation: Id::from_bytes([2; 16]),
            owner_generation: 1,
        }
    }
    #[test]
    fn channel_sequences_reject_replay_and_scope_switching() {
        // Interleave receives, commands and storage, then retry valid IDs after rejected calls.
        let mut executor = Executor::default();
        let context = context(Id::from_bytes([1; 16]));
        let receiver = DataCall {
            volume: [0; 16],
            operation: DataOperation::ChannelEvents { instance: [1; 16] },
        };
        let read = DataCall {
            volume: [3; 16],
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        let command = DataCall {
            volume: [0; 16],
            operation: DataOperation::ChannelCommand {
                instance: [1; 16],
                name: "room".into(),
                action: 2,
                data: vec![],
            },
        };
        let first = super::super::CHANNEL_BIT | 1;
        let first_command = first | super::super::ChannelSequences::COMMAND_BIT;
        executor.record_request(context, first, 0, &receiver, |_| true).unwrap();
        executor.record_request(context, first_command, 0, &command, |_| true).unwrap();
        for operation in 1..=100 {
            executor.record_request(context, operation, 0, &read, |_| true).unwrap();
        }
        for (operation, mutation, call) in [
            (first, 0, &receiver),
            (first + 2, 0, &receiver),
            (first + 1, 1, &receiver),
            (first + 1, 0, &read),
            (101, 0, &receiver),
            (first_command, 0, &command),
            (first_command + 2, 0, &command),
            (first_command + 1, 1, &command),
            (first_command + 1, 0, &read),
            (first_command + 1, 0, &receiver),
            (first + 1, 0, &command),
            (101, 0, &command),
        ] {
            assert_eq!(
                executor.record_request(context, operation, mutation, call, |_| true),
                Err(HostError::StaleHandle)
            );
        }
        executor.record_request(context, first + 1, 0, &receiver, |_| true).unwrap();
        executor.record_request(context, first_command + 1, 0, &command, |_| true).unwrap();

        let channels =
            &mut executor.sequences.get_mut(&Context::from(context).key()).unwrap().channels;
        channels.receive = super::super::ChannelSequences::COMMAND_BIT - 1;
        assert_eq!(channels.advance(DataKind::ChannelEvents), Err(HostError::LimitExceeded));
    }
    #[test]
    fn retired_contexts_do_not_exhaust_sequence_admission() {
        let mut executor = Executor::default();
        let call = DataCall {
            volume: [3; 16],
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        for index in 1..=MAX_CALLS + 1 {
            let current = context(Id::from_bytes((index as u128).to_be_bytes()));
            executor
                .record_request(current, 1, 0, &call, |key| key == Context::from(current).key())
                .unwrap();
            assert_eq!(executor.sequences.len(), 1);
        }
    }
    #[test]
    fn collecting_retired_sequences_keeps_a_live_handles_replay_fence() {
        let mut executor = Executor::default();
        let call = DataCall {
            volume: [3; 16],
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        let first = context(Id::from_bytes([1; 16]));
        let next = context(Id::from_bytes([2; 16]));
        executor.record_request(first, 1, 0, &call, |_| true).unwrap();
        executor.record_request(next, 1, 0, &call, |_| true).unwrap();
        assert_eq!(executor.sequences.len(), 2);
        assert_eq!(
            executor.record_request(first, 1, 0, &call, |_| true),
            Err(HostError::StaleHandle)
        );
        executor.record_request(first, 2, 0, &call, |_| true).unwrap();
    }
    #[test]
    fn guest_and_tool_ids_have_independent_sequence_namespaces() {
        // Equal opaque bytes in different authority kinds must not share replay,
        // mutation-order or retained-work bookkeeping.
        let mut executor = Executor::default();
        let handle = Id::from_bytes([1; 16]);
        let guest = context(handle);
        let tool = Context::Tool { handle, incarnation: handle };
        let call = DataCall {
            volume: [3; 16],
            operation: DataOperation::SchemaRead { entry_id: [5; 15] },
        };
        executor.record_request(guest, 1, 0, &call, |_| true).unwrap();
        executor.record_request(tool, 1, 0, &call, |_| true).unwrap();
        assert_eq!(executor.sequences.len(), 2);
        assert_eq!(
            executor.record_request(guest, 1, 0, &call, |_| true),
            Err(HostError::StaleHandle)
        );
        assert_eq!(
            executor.record_request(tool, 1, 0, &call, |_| true),
            Err(HostError::StaleHandle)
        );
    }
    #[test]
    fn operation_window_accepts_independent_reordered_reads_but_never_duplicates() {
        let mut sequence = Sequence::default();
        sequence.admit(3, 0, false).unwrap();
        sequence.admit(2, 0, false).unwrap();
        assert_eq!(sequence.frontier, 0);
        assert_eq!(sequence.admit(3, 0, false), Err(HostError::StaleHandle));
        sequence.admit(1, 1, true).unwrap();
        assert_eq!(sequence.frontier, 3);
        assert!(sequence.ahead.is_empty());
        assert_eq!(sequence.admit(4, 3, true), Err(HostError::StaleHandle));
        sequence.admit(4, 2, true).unwrap();
        assert_eq!(sequence.admit(69, 0, false), Err(HostError::StaleHandle));
        assert_eq!(sequence.admit(4, 0, false), Err(HostError::StaleHandle));
        assert_eq!(sequence.admit(5, 1, false), Err(HostError::StaleHandle));
        sequence.admit(5, 0, false).unwrap();
    }
    #[test]
    fn credits_enforce_counts_both_byte_caps_and_overflow() {
        assert!(admits_credit(0, 0, 0, 0, MAX_CONTEXT_BYTES));
        assert!(!admits_credit(MAX_CALLS, 0, 0, 0, 0));
        assert!(!admits_credit(0, 0, MAX_CONTEXT_CALLS, 0, 0));
        assert!(!admits_credit(0, MAX_BYTES, 0, 0, 1));
        assert!(!admits_credit(0, 0, 0, MAX_CONTEXT_BYTES, 1));
        assert!(!admits_credit(0, usize::MAX, 0, 0, 1));
        assert!(!admits_credit(0, 0, 0, usize::MAX, 1));
    }
}
