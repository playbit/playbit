//! Engine ownership and two filesystem workers. Admission is memory-only;
//! descriptors, metadata, private storage cleanup and publication stay on workers.
use super::{
    authority::Authority,
    budget::{Budget, Credit, Lease},
    host::{Directory, PrivateObjects},
    resource::{self, Kind, Object, Placement, Services},
};
use crate::{
    runtime::host::{
        HostResult,
        files::{Action, Call, Output},
    },
    store::Id,
};
use playbit_api::abi::{files::*, syscall::Error};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

#[derive(Clone)]
pub enum PlacementSource {
    Ready(Arc<Placement>),
    Managed(Arc<super::private_storage::Managed>),
}
impl PlacementSource {
    pub(super) fn get(&self) -> std::io::Result<Arc<Placement>> {
        match self {
            Self::Ready(placement) => Ok(placement.clone()),
            Self::Managed(managed) => managed.get(),
        }
    }
}
pub struct Root {
    pub path: PathBuf,
    pub private: Arc<PrivateObjects>,
    pub authority: Arc<Authority>,
    pub placement: PlacementSource,
}
pub struct VolumeRoot {
    pub store: Arc<crate::store::LocalStore>,
    pub access: crate::store::Access,
    pub volume: Id,
    pub session: Option<String>,
    pub transport: crate::engine::blob::BlobTransport,
    pub authority: Arc<Authority>,
    pub placement: PlacementSource,
}
/// One package per process-owned generation, loaded once on a File worker.
#[cfg(all(feature = "process", not(target_os = "ios")))]
pub(crate) struct Bundle {
    path: PathBuf,
    build: String,
    package: Mutex<Option<crate::package::Package>>,
}
#[cfg(all(feature = "process", not(target_os = "ios")))]
impl Bundle {
    pub(crate) fn new(path: PathBuf, build: String) -> Self {
        Self { path, build, package: Mutex::new(None) }
    }
    fn get(&self) -> HostResult<crate::package::Package> {
        let mut retained = self.package.lock().map_err(|_| Error::Internal)?;
        if let Some(package) = &*retained {
            return Ok(package.clone());
        }
        let package =
            crate::package::Package::open(&self.path).map_err(|_| Error::InvalidArgument)?;
        if package.id().map_err(|_| Error::Internal)? != self.build {
            return Err(Error::InvalidArgument);
        }
        *retained = Some(package.clone());
        Ok(package)
    }
}
struct Cell {
    object: Mutex<Option<Object>>,
    #[cfg(test)]
    after_execute: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    authority: Arc<Authority>,
    list_closed: Arc<AtomicBool>,
    credit: Arc<Credit>,
    // Rust drops fields in declaration order; publish only after object/credit.
    retired: Retired,
}
struct Slot {
    owner: Id,
    alive: Arc<AtomicBool>,
    kind: Kind,
    usable: bool,
    sealed: bool,
    cell: Arc<Cell>,
}
struct Mailbox {
    fence: Mutex<Option<super::volume::Fence>>,
    finished: AtomicBool,
    disposed: AtomicBool,
    result: Mutex<Option<HostResult<Output>>>,
}
impl Mailbox {
    fn current(&self) -> bool {
        self.fence
            .lock()
            .expect("file delivery fence lock is not poisoned")
            .as_ref()
            .is_none_or(super::volume::Fence::current)
    }
}
/// A cleanup acknowledgment follows the last worker-owned reference. Frontend
/// quota is retained until this becomes ready, even though list close is local.
#[derive(Clone)]
pub struct Retirement(Arc<AtomicBool>);
struct Retired {
    signal: Retirement,
    notify: Arc<dyn Fn() + Send + Sync>,
}
impl Drop for Retired {
    fn drop(&mut self) {
        self.signal.0.store(true, Ordering::Release);
        (self.notify)();
    }
}
impl Retirement {
    pub fn ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
pub struct Ticket {
    id: u64,
    mailbox: Arc<Mailbox>,
    alive: Arc<AtomicBool>,
    authority: Arc<Authority>,
    list_closed: Arc<AtomicBool>,
    close: bool,
    retirement: Retirement,
    target_retirement: Option<Retirement>,
    notify: Arc<dyn Fn() + Send + Sync>,
}
/// Retains frontend quota until abandoned work and any unseen result retire.
pub struct Abandoned {
    mailbox: Arc<Mailbox>,
    source: Option<Retirement>,
    target: Option<Retirement>,
}
impl Abandoned {
    pub fn ready(&self) -> bool {
        self.mailbox.finished.load(Ordering::Acquire)
            && self.source.as_ref().is_none_or(Retirement::ready)
            && self.target.as_ref().is_none_or(Retirement::ready)
    }
}
impl Ticket {
    pub fn abandon(self) -> Abandoned {
        Abandoned {
            mailbox: self.mailbox.clone(),
            source: self.close.then(|| self.retirement.clone()),
            target: self.target_retirement.clone(),
        }
    }
    pub fn delivery_current(&self) -> bool {
        self.authority.check(false).is_ok() && self.mailbox.current()
    }
    pub fn ready(&self) -> bool {
        if self.close {
            if !self.mailbox.finished.load(Ordering::Acquire) || !self.retirement.ready() {
                return false;
            }
            let recovery = self
                .mailbox
                .result
                .lock()
                .expect("file completion lock is not poisoned")
                .as_ref()
                .is_some_and(|result| {
                    matches!(result, Ok(Output::Close { recovery: Some(_), .. }))
                });
            return (recovery && self.authority.check(false).is_ok() && self.mailbox.current())
                || self.target_retirement.as_ref().is_none_or(Retirement::ready);
        }
        self.mailbox.finished.load(Ordering::Acquire)
            || !self.alive.load(Ordering::Acquire)
            || self.list_closed.load(Ordering::Acquire)
            || (!self.close && self.authority.check(false).is_err())
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.mailbox.disposed.store(true, Ordering::Release);
        (self.notify)();
    }
}
enum Command {
    #[cfg(all(feature = "process", not(target_os = "ios")))]
    BundleRoot(Arc<Bundle>),
    Root(Root),
    VolumeRoot(VolumeRoot),
    Operation {
        source: Arc<Cell>,
        action: Action,
    },
}
struct Work {
    command: Command,
    target: Option<(u64, Arc<Cell>)>,
    services: Arc<Services>,
    mailbox: Arc<Mailbox>,
    notify: Arc<dyn Fn() + Send + Sync>,
    _credit: Arc<Credit>,
}
struct Publish {
    mailbox: Arc<Mailbox>,
    notify: Arc<dyn Fn() + Send + Sync>,
}
impl Drop for Publish {
    fn drop(&mut self) {
        self.mailbox.finished.store(true, Ordering::Release);
        (self.notify)();
    }
}
impl Work {
    fn run(self) {
        let _timing = crate::trace::Span::new("file.worker.execute");
        let Work { command, target, services, mailbox, notify, _credit } = self;
        let _publish = Publish { mailbox: mailbox.clone(), notify };
        let result = Self::execute(command, target, &services).map(|(output, fence)| {
            *mailbox.fence.lock().expect("file delivery fence lock is not poisoned") = fence;
            output
        });
        *mailbox.result.lock().expect("file completion lock is not poisoned") = Some(result);
    }
    fn execute(
        command: Command,
        target: Option<(u64, Arc<Cell>)>,
        services: &Services,
    ) -> HostResult<(Output, Option<super::volume::Fence>)> {
        let source = match &command {
            Command::Operation { source, .. } => Some(source.clone()),
            _ => None,
        };
        let volume_root = matches!(&command, Command::VolumeRoot(_));
        let (completion, checked_scope) = command.execute(services)?;
        let resource::Completion { mut output, created } = completion;
        #[cfg(test)]
        if let Some(source) = &source
            && let Some(hook) = source.after_execute.lock().expect("test hook lock").take()
        {
            hook();
        }
        // A logical root carries no backing data, matching native root
        // admission. Actual File operations retain their data delivery fences.
        let fence = if volume_root {
            None
        } else if let Some(fence) = checked_scope.filter(super::volume::Fence::current) {
            Some(fence)
        } else if let Some(created) = &created {
            match created.delivery_fence() {
                Ok(fence) => fence,
                Err(_) if matches!(output, Output::Close { .. }) => {
                    if let Output::Close { recovery, .. } = &mut output {
                        *recovery = None;
                    }
                    return Ok((output, None));
                }
                Err(error) => return Err(error),
            }
        } else if matches!(output, Output::Written | Output::Close { .. }) {
            None
        } else if let Some(source) = source {
            source
                .object
                .lock()
                .map_err(|_| Error::Internal)?
                .as_ref()
                .ok_or(Error::Closed)?
                .delivery_fence()?
        } else {
            None
        };
        if let Some(created) = created {
            let (handle, target) = target.as_ref().ok_or(Error::Internal)?;
            let mut slot = target.object.lock().map_err(|_| Error::Internal)?;
            assert!(slot.is_none(), "one operation initializes its reserved result slot once");
            *slot = Some(created);
            match &mut output {
                Output::File { handle: output } | Output::List { handle: output } => {
                    *output = *handle
                }
                Output::Close { recovery, .. } => *recovery = Some(*handle),
                _ => return Err(Error::Internal),
            }
        }
        Ok((output, fence))
    }
}
impl Command {
    fn execute(
        self,
        services: &Services,
    ) -> HostResult<(resource::Completion, Option<super::volume::Fence>)> {
        let completion = match self {
            #[cfg(all(feature = "process", not(target_os = "ios")))]
            Self::BundleRoot(bundle) => {
                let package = bundle.get()?;
                resource::Completion::created(
                    Output::File { handle: 0 },
                    Object::Bundle(crate::package::filesystem::Object::root(package)),
                )
            }
            Self::Root(root) => {
                let placement = root.placement.get().map_err(resource::error)?;
                let directory =
                    Directory::root(&root.path, root.private.clone(), root.authority.clone())
                        .map_err(resource::error)?;
                resource::Completion {
                    output: Output::File { handle: 0 },
                    created: Some(Object::Directory { directory, placement }),
                }
            }
            Self::VolumeRoot(root) => {
                let directory = super::volume::Directory {
                    store: root.store,
                    access: root.access,
                    volume: root.volume,
                    parent: None,
                    session: root.session,
                    transport: root.transport,
                    authority: root.authority,
                    placement: root.placement,
                };
                // Root admission retains only the authorized logical address.
                // Actual File operations check backing and current authority;
                // an unused unavailable scope must not prevent app startup.
                resource::Completion {
                    output: Output::File { handle: 0 },
                    created: Some(Object::VolumeDirectory(directory)),
                }
            }
            Self::Operation { source, action } => return Self::operation(source, action, services),
        };
        Ok((completion, None))
    }

    fn operation(
        source: Arc<Cell>,
        action: Action,
        services: &Services,
    ) -> HostResult<(resource::Completion, Option<super::volume::Fence>)> {
        if source.list_closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let mut object = source.object.lock().map_err(|_| Error::Internal)?;
        if let Action::Close { flags } = action {
            return Ok((object.take().ok_or(Error::Closed)?.close(flags)?, None));
        }
        let object = object.as_mut().ok_or(Error::Closed)?;
        // Successful volume opens retain the source's store, volume and access.
        // Capture before execution; reuse only while the scope stays current.
        let volume_open =
            matches!(object, Object::VolumeDirectory(_)) && matches!(action, Action::Open { .. });
        let checked_scope = if volume_open
            || matches!(action, Action::Read { .. } | Action::Status | Action::ListNext { .. })
        {
            object.capture_delivery_fence()?
        } else {
            None
        };
        let completion = object.execute(action, services, &source.authority)?;
        if volume_open {
            assert!(
                matches!(
                    completion.created,
                    Some(
                        Object::VolumeDirectory(_)
                            | Object::VolumeReader(_)
                            | Object::VolumeWriter(_)
                    )
                ),
                "volume open retains the source's access scope"
            );
        } else if checked_scope.is_some() {
            assert!(
                completion.created.is_none(),
                "source-only operations cannot create a resource"
            );
        }
        Ok((completion, checked_scope))
    }
}

enum Job {
    Execute(Box<Work>),
    Prepare {
        placement: Arc<super::private_storage::Managed>,
        authority: Arc<Authority>,
        _credit: Arc<Credit>,
    },
    // The last descriptor/staging reference is released on a worker even when
    // the guest or a pending operation disappears without collecting its result.
    Release(Arc<Cell>),
}
#[derive(Default)]
struct Progress {
    sequence: Mutex<u64>,
    changed: Condvar,
}
struct ProgressWake {
    progress: Arc<Progress>,
    notify: Arc<dyn Fn() + Send + Sync>,
}
impl Drop for ProgressWake {
    fn drop(&mut self) {
        let mut sequence =
            self.progress.sequence.lock().expect("worker progress lock is not poisoned");
        *sequence = sequence.checked_add(1).expect("worker completion sequence cannot wrap");
        self.progress.changed.notify_all();
        drop(sequence);
        (self.notify)();
    }
}
struct Pool {
    progress: Arc<Progress>,
    send: Option<mpsc::SyncSender<Job>>,
    threads: Vec<JoinHandle<()>>,
}
impl Pool {
    fn new(notify: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Job>(2);
        let receive = Arc::new(Mutex::new(receive));
        let mut pool = Self {
            progress: Arc::new(Progress::default()),
            send: Some(send),
            threads: Vec::with_capacity(2),
        };
        for index in 0..2 {
            let receive = receive.clone();
            let progress = pool.progress.clone();
            let notify = notify.clone();
            pool.threads.push(std::thread::Builder::new().name(format!("file-io-{index}")).spawn(
                move || loop {
                    let job = receive.lock().expect("file worker queue is not poisoned").recv();
                    let Ok(job) = job else { break };
                    let _wake = ProgressWake { progress: progress.clone(), notify: notify.clone() };
                    match job {
                        Job::Execute(work) => work.run(),
                        Job::Prepare { placement, authority, _credit } => {
                            if authority.check(true).is_ok()
                                && let Err(error) = placement.prepare_write()
                            {
                                eprintln!(
                                    "File write preparation failed; writer-open will retry: {error}"
                                );
                            }
                        }
                        Job::Release(cell) => drop(cell),
                    }
                },
            )?);
        }
        Ok(pool)
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.send.take();
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                eprintln!("Filesystem worker stopped after a panic");
            }
        }
    }
}
struct Accepted {
    owner: Id,
    source: Option<u64>,
    target: Option<u64>,
    close: bool,
    started: bool,
    mailbox: Arc<Mailbox>,
    work: Option<Box<Work>>,
    _credit: Arc<Credit>,
}
pub struct Executor {
    pool: Pool,
    budget: Arc<Budget>,
    services: Arc<Services>,
    notify: Arc<dyn Fn() + Send + Sync>,
    slots: crate::capability::Table<Box<Slot>>,
    owners: BTreeMap<Id, Arc<AtomicBool>>,
    leases: BTreeMap<Id, Arc<Lease>>,
    accepted: crate::capability::Table<Box<Accepted>>,
    accepted_order: VecDeque<u64>,
    release: VecDeque<Arc<Cell>>,
    retiring: BTreeMap<u64, (Id, Retirement)>,
}
impl Executor {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(notify);
        Ok(Self {
            pool: Pool::new(notify.clone())?,
            budget: Arc::new(Budget::default()),
            services: Arc::new(Services::default()),
            notify,
            slots: crate::capability::Table::new(crate::capability::Budget::backend()),
            owners: BTreeMap::new(),
            leases: BTreeMap::new(),
            accepted: crate::capability::Table::new(crate::capability::Budget::backend()),
            accepted_order: VecDeque::new(),
            release: VecDeque::new(),
            retiring: BTreeMap::new(),
        })
    }
    fn reserve_operation(
        &mut self,
        owner: Id,
        result_kind: Option<Kind>,
        authority: Arc<Authority>,
        reuse: Option<Arc<Credit>>,
    ) -> HostResult<(u64, Option<u64>)> {
        // A rejected completion cannot create a resource that needs a worker
        // wake to retire. Result admission failure cancels only empty metadata.
        let operation = self.accepted.reserve(1, crate::capability::Rights::NONE)?;
        let target =
            match result_kind.map(|kind| self.reserve(owner, kind, authority, reuse)).transpose() {
                Ok(target) => target,
                Err(error) => {
                    self.accepted.cancel(operation).expect("unpublished operation reservation");
                    return Err(error);
                }
            };
        Ok((operation.raw(), target))
    }
    fn reserve(
        &mut self,
        owner: Id,
        kind: Kind,
        authority: Arc<Authority>,
        reuse: Option<Arc<Credit>>,
    ) -> HostResult<u64> {
        let credit = match reuse {
            Some(credit) => credit,
            None => self.budget.resource(owner, kind)?,
        };
        let alive =
            self.owners.entry(owner).or_insert_with(|| Arc::new(AtomicBool::new(true))).clone();
        let cell = Arc::new(Cell {
            object: Mutex::new(None),
            #[cfg(test)]
            after_execute: Mutex::new(None),
            authority,
            list_closed: Arc::new(AtomicBool::new(false)),
            credit,
            retired: Retired {
                signal: Retirement(Arc::new(AtomicBool::new(false))),
                notify: self.notify.clone(),
            },
        });
        self.slots.insert_new(
            1,
            crate::capability::Rights::NONE,
            Box::new(Slot { owner, alive, kind, usable: false, sealed: false, cell }),
        )
    }
    fn slot(&self, owner: Id, id: u64) -> HostResult<&Slot> {
        let slot =
            self.slots.lookup(&id).filter(|slot| slot.owner == owner).ok_or(Error::StaleHandle)?;
        if slot.sealed {
            return Err(Error::Closed);
        }
        if !slot.usable || !slot.alive.load(Ordering::Acquire) {
            return Err(Error::StaleHandle);
        }
        Ok(slot)
    }
    pub fn reserve_context(&mut self, owner: Id) -> HostResult<()> {
        if !self.leases.contains_key(&owner) {
            self.leases.insert(owner, self.budget.lease(owner)?);
        }
        Ok(())
    }
    pub fn root(&mut self, owner: Id, root: Root) -> HostResult<Ticket> {
        root.authority.check(false).map_err(resource::error)?;
        // Trusted mount configuration is bounded before creating a Root.
        if root.path.as_os_str().len() > 4096 {
            return Err(Error::InvalidArgument);
        }
        self.reserve_context(owner)?;
        let credit = self.budget.operation(owner, 4352)?;
        let (operation, target) =
            self.reserve_operation(owner, Some(Kind::Directory), root.authority.clone(), None)?;
        Ok(self.enqueue(operation, None, target, Command::Root(root), credit))
    }
    pub fn volume_root(&mut self, owner: Id, root: VolumeRoot) -> HostResult<Ticket> {
        root.authority.check(false).map_err(resource::error)?;
        if root.session.as_ref().is_some_and(|session| session.len() > 4096) {
            return Err(Error::InvalidArgument);
        }
        self.reserve_context(owner)?;
        let credit = self.budget.operation(owner, 4352)?;
        let (operation, target) =
            self.reserve_operation(owner, Some(Kind::Directory), root.authority.clone(), None)?;
        self.prepare_volume_write(owner, &root);
        Ok(self.enqueue(operation, None, target, Command::VolumeRoot(root), credit))
    }
    /// Install an already-admitted volume grant using bounded memory only.
    /// The caller owns session/consent metadata admission. This capability
    /// discloses no contents: every operation checks current store authority on
    /// a File worker. Private write placement is prepared opportunistically on
    /// that pool; unavailable preparation never changes this read capability.
    pub(crate) fn admit_volume_root(&mut self, owner: Id, root: VolumeRoot) -> HostResult<u64> {
        root.authority.check(false).map_err(resource::error)?;
        if root.session.as_ref().is_some_and(|session| session.len() > 4096) {
            return Err(Error::InvalidArgument);
        }
        self.reserve_context(owner)?;
        let handle = self.reserve(owner, Kind::Directory, root.authority.clone(), None)?;
        self.prepare_volume_write(owner, &root);
        let slot = self.slots.lookup_mut(&handle).expect("reserved volume root");
        *slot.cell.object.lock().expect("new root object lock is not poisoned") =
            Some(Object::VolumeDirectory(super::volume::Directory {
                store: root.store,
                access: root.access,
                volume: root.volume,
                parent: None,
                session: root.session,
                transport: root.transport,
                authority: root.authority,
                placement: root.placement,
            }));
        slot.usable = true;
        Ok(handle)
    }
    #[cfg(all(feature = "process", not(target_os = "ios")))]
    pub(crate) fn bundle_root(
        &mut self,
        owner: Id,
        bundle: Arc<Bundle>,
        authority: Arc<Authority>,
    ) -> HostResult<Ticket> {
        authority.check(false).map_err(resource::error)?;
        self.reserve_context(owner)?;
        let credit = self.budget.operation(owner, 128 * 1024)?;
        let (operation, target) =
            self.reserve_operation(owner, Some(Kind::Directory), authority, None)?;
        Ok(self.enqueue(operation, None, target, Command::BundleRoot(bundle), credit))
    }
    pub(crate) fn admit_bundle_root(
        &mut self,
        owner: Id,
        package: crate::package::Package,
        authority: Arc<Authority>,
    ) -> HostResult<u64> {
        authority.check(false).map_err(resource::error)?;
        self.reserve_context(owner)?;
        let handle = self.reserve(owner, Kind::Directory, authority, None)?;
        let slot = self.slots.lookup_mut(&handle).expect("reserved bundle root");
        *slot.cell.object.lock().expect("new bundle object") =
            Some(Object::Bundle(crate::package::filesystem::Object::root(package)));
        slot.usable = true;
        Ok(handle)
    }
    fn prepare_volume_write(&self, owner: Id, root: &VolumeRoot) {
        let PlacementSource::Managed(placement) = &root.placement else { return };
        if root.authority.check(true).is_err()
            || placement.preparation_started.load(Ordering::Acquire)
        {
            return;
        }
        // Use existing operation/byte credits and the two-slot worker queue.
        // No backlog or retry timer: saturation simply leaves lazy write-open.
        let Ok(credit) = self.budget.operation(owner, std::mem::size_of::<Job>()) else { return };
        if placement
            .preparation_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let job = Job::Prepare {
            placement: placement.clone(),
            authority: root.authority.clone(),
            _credit: credit,
        };
        if self.pool.send.as_ref().expect("live worker pool").try_send(job).is_err() {
            placement.preparation_started.store(false, Ordering::Release);
        }
    }
    pub fn begin(&mut self, owner: Id, call: Call) -> HostResult<Ticket> {
        let slot = self.slot(owner, call.file)?;
        slot.kind.admits(&call.action)?;
        if slot.kind == Kind::List
            && self.accepted.values().any(|work| {
                work.source == Some(call.file) && !work.mailbox.disposed.load(Ordering::Acquire)
            })
        {
            return Err(Error::Busy);
        }
        let close = matches!(call.action, Action::Close { .. });
        if !close {
            slot.cell
                .authority
                .check(matches!(call.action, Action::Write { .. }))
                .map_err(resource::error)?;
        }
        let authority = slot.cell.authority.clone();
        let source = slot.cell.clone();
        let result_kind = match &call.action {
            Action::Open { mode, .. } => Some(match *mode {
                FILE_DIRECTORY => Kind::Directory,
                FILE_READ => Kind::Reader,
                _ => Kind::Writer,
            }),
            Action::ListOpen => Some(Kind::List),
            Action::Close { flags }
                if slot.kind == Kind::Writer && *flags != FILE_CLOSE_DISCARD =>
            {
                Some(Kind::Recovery)
            }
            _ => None,
        };
        let reuse = (close && result_kind.is_some()).then(|| source.credit.clone());
        let credit = self.budget.operation(owner, call.action.retained_bytes()?)?;
        let (operation, target) = self.reserve_operation(owner, result_kind, authority, reuse)?;
        if close {
            self.slots.lookup_mut(&call.file).expect("validated source remains owned").sealed =
                true;
        }
        Ok(self.enqueue(
            operation,
            Some(call.file),
            target,
            Command::Operation { source, action: call.action },
            credit,
        ))
    }
    fn enqueue(
        &mut self,
        id: u64,
        source: Option<u64>,
        target: Option<u64>,
        command: Command,
        credit: Arc<Credit>,
    ) -> Ticket {
        let slot = &self
            .slots
            .lookup(&source.or(target).expect("every file operation has a reserved resource"))
            .expect("owned source or target");
        let owner = slot.owner;
        let close = matches!(command, Command::Operation { action: Action::Close { .. }, .. });
        let mailbox = Arc::new(Mailbox {
            fence: Mutex::new(None),
            finished: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            result: Mutex::new(None),
        });
        let ticket = Ticket {
            id,
            mailbox: mailbox.clone(),
            alive: slot.alive.clone(),
            authority: slot.cell.authority.clone(),
            list_closed: slot.cell.list_closed.clone(),
            close,
            retirement: slot.cell.retired.signal.clone(),
            target_retirement: target.map(|id| {
                self.slots.lookup(&id).expect("reserved target").cell.retired.signal.clone()
            }),
            notify: self.notify.clone(),
        };
        let work = Work {
            command,
            target: target
                .map(|id| (id, self.slots.lookup(&id).expect("reserved target").cell.clone())),
            services: self.services.clone(),
            mailbox: mailbox.clone(),
            notify: self.notify.clone(),
            _credit: credit.clone(),
        };
        self.accepted
            .insert(
                crate::capability::Handle(id),
                Box::new(Accepted {
                    owner,
                    source,
                    target,
                    close,
                    started: false,
                    mailbox,
                    work: Some(Box::new(work)),
                    _credit: credit,
                }),
            )
            .expect("operation reservation remains owned");
        self.accepted_order.push_back(id);
        ticket
    }
    fn release(&mut self, id: u64) {
        if let Some(slot) = self.slots.take(&id) {
            self.retiring.insert(id, (slot.owner, slot.cell.retired.signal.clone()));
            self.release.push_back(slot.cell);
        }
    }
    /// Synchronous list close only changes memory. A pending Next is immediately
    /// ready with Closed, even while its worker is still retiring.
    pub fn close_list(&mut self, owner: Id, id: u64) -> HostResult<Retirement> {
        let slot = self.slot(owner, id)?;
        if slot.kind != Kind::List {
            return Err(Error::InvalidArgument);
        }
        slot.cell.list_closed.store(true, Ordering::Release);
        let retirement = slot.cell.retired.signal.clone();
        self.release(id);
        (self.notify)();
        Ok(retirement)
    }
    /// Disposal never replaces an accepted normal close. If publication is
    /// already running, wait for its resource retirement without replaying it.
    pub fn discard(&mut self, owner: Id, id: u64) -> HostResult<Retirement> {
        if let Some(slot) = self.slots.lookup(&id) {
            if slot.owner != owner {
                return Err(Error::StaleHandle);
            }
            if slot.sealed {
                return Ok(slot.cell.retired.signal.clone());
            }
            self.slot(owner, id)?;
            if slot.kind == Kind::List {
                return Err(Error::InvalidArgument);
            }
            let retirement = slot.cell.retired.signal.clone();
            // Accepted calls retain their source Cell, so releasing the table
            // slot cannot destroy it before those calls execute in order. The
            // last reference (including staging) is dropped on a File worker.
            self.release(id);
            (self.notify)();
            return Ok(retirement);
        }
        let (retiring_owner, retirement) = self.retiring.get(&id).ok_or(Error::StaleHandle)?;
        if *retiring_owner != owner {
            return Err(Error::StaleHandle);
        }
        Ok(retirement.clone())
    }
    pub fn retire(&mut self, owner: Id) {
        self.leases.remove(&owner);
        if let Some(alive) = self.owners.remove(&owner) {
            alive.store(false, Ordering::Release);
        }
        let slots = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.owner == owner)
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for id in slots {
            self.release(id);
        }
        for work in self.accepted.values().filter(|work| work.owner == owner) {
            work.mailbox.disposed.store(true, Ordering::Release);
        }
        (self.notify)();
    }
    pub fn take(&mut self, ticket: &Ticket) -> HostResult<Output> {
        self.collect();
        if !self.accepted.contains_key(&ticket.id)
            || ticket.mailbox.disposed.load(Ordering::Acquire)
        {
            return Err(Error::StaleHandle);
        }
        if !ticket.ready() {
            return Err(Error::WouldBlock);
        }
        let result = if !ticket.alive.load(Ordering::Acquire) {
            Err(Error::StaleHandle)
        } else if ticket.list_closed.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            let mut output = ticket.mailbox.result.lock().map_err(|_| Error::Internal)?;
            if ticket.close {
                // This is the delivery boundary. If recovery loses authority,
                // retain the known receipt until the unused resource retires.
                if ticket.authority.check(false).is_err() || !ticket.mailbox.current() {
                    let recovery = match output.as_mut() {
                        Some(Ok(Output::Close { recovery, .. })) => recovery.take(),
                        _ => None,
                    };
                    if let Some(id) = recovery {
                        drop(output);
                        self.release(id);
                        (self.notify)();
                        return Err(Error::WouldBlock);
                    }
                }
                output.take().unwrap_or(Err(Error::Internal))
            } else {
                match ticket.authority.delivery() {
                    Ok(_guard) if !ticket.mailbox.current() => Err(Error::Unavailable),
                    Ok(_guard) => output.take().unwrap_or(Err(Error::Internal)),
                    Err(error) => Err(resource::error(error)),
                }
            }
        };
        let delivered = match &result {
            Ok(Output::File { handle } | Output::List { handle }) => Some(*handle),
            Ok(Output::Close { recovery, .. }) => *recovery,
            _ => None,
        };
        if let Some(id) = delivered {
            self.slots.lookup_mut(&id).ok_or(Error::StaleHandle)?.usable = true;
        }
        if let Some(target) = self.accepted.lookup(&ticket.id).and_then(|work| work.target)
            && delivered != Some(target)
        {
            self.release(target);
        }
        // Keep credit and any worker-owned state until finished, even if a list
        // close or instance retirement made the ticket terminal before its I/O.
        ticket.mailbox.disposed.store(true, Ordering::Release);
        self.collect();
        result
    }
    fn collect(&mut self) {
        let completed = self
            .accepted
            .iter()
            .filter(|(_, work)| work.mailbox.finished.load(Ordering::Acquire))
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for id in completed {
            let work = self.accepted.lookup_mut(&id).expect("collected operation remains owned");
            if work.close
                && let Some(source) = work.source.take()
            {
                self.release(source);
            }
            let work = &self.accepted.lookup(&id).expect("owned operation");
            if work.close
                && let Some(target) = work.target
                && self.slots.lookup(&target).is_some_and(|slot| !slot.usable)
            {
                let mut result =
                    work.mailbox.result.lock().expect("file completion lock is not poisoned");
                let keep = work.mailbox.current()
                    && self
                        .slots
                        .lookup(&target)
                        .is_some_and(|slot| slot.cell.authority.check(false).is_ok())
                    && result.as_ref().is_some_and(|result| {
                        matches!(result, Ok(Output::Close { recovery: Some(_), .. }))
                    });
                if !keep {
                    if let Some(Ok(Output::Close { recovery, .. })) = result.as_mut() {
                        *recovery = None;
                    }
                    drop(result);
                    self.release(target);
                }
            }
            if self
                .accepted
                .lookup(&id)
                .expect("owned operation")
                .mailbox
                .disposed
                .load(Ordering::Acquire)
            {
                let work = self.accepted.take(&id).expect("completed work retains credit");
                if let Some(target) = work.target
                    && self.slots.lookup(&target).is_some_and(|slot| !slot.usable)
                {
                    self.release(target);
                }
            }
        }
        self.accepted_order.retain(|id| self.accepted.contains_key(id));
        self.owners.retain(|_, alive| Arc::strong_count(alive) > 1);
    }
    /// Called by readiness-driven engine maintenance, never a polling timer.
    pub fn maintain(&mut self) -> HostResult<()> {
        self.collect();
        self.retiring.retain(|_, (_, retirement)| !retirement.ready());
        assert!(
            self.retiring.len() <= 4096 + 64 + 1024,
            "retirement is bounded by resources and accepted work"
        );
        let mut blocked = BTreeSet::new();
        for id in &self.accepted_order {
            let work = self.accepted.lookup_mut(id).expect("scheduled operation remains owned");
            if work.mailbox.finished.load(Ordering::Acquire) {
                continue;
            }
            if let Some(source) = work.source
                && !blocked.insert(source)
            {
                continue;
            }
            if work.started {
                continue;
            }
            let input = work.work.take().expect("unstarted operation owns its input");
            match self
                .pool
                .send
                .as_ref()
                .expect("running executor owns sender")
                .try_send(Job::Execute(input))
            {
                Ok(()) => work.started = true,
                Err(mpsc::TrySendError::Full(Job::Execute(input))) => {
                    work.work = Some(input);
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(Job::Execute(input))) => {
                    work.work = Some(input);
                    return Err(Error::Unavailable);
                }
                _ => unreachable!("send failure returns the submitted job kind"),
            }
        }
        while let Some(cell) = self.release.pop_front() {
            match self
                .pool
                .send
                .as_ref()
                .expect("running executor owns sender")
                .try_send(Job::Release(cell))
            {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(Job::Release(cell))) => {
                    self.release.push_front(cell);
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(Job::Release(cell))) => {
                    self.release.push_front(cell);
                    return Err(Error::Unavailable);
                }
                _ => unreachable!("send failure returns the submitted job kind"),
            }
        }
        Ok(())
    }
    /// Reconstruction waits for accepted work, including abandoned writes and
    /// normal closes. Retained, finished results do not block a new generation.
    pub fn owner_retained(&self, owner: Id) -> bool {
        self.budget.retained(owner)
    }
    pub fn owner_pending(&self, owner: Id) -> bool {
        self.retiring
            .values()
            .any(|(retiring_owner, retirement)| *retiring_owner == owner && !retirement.ready())
            || self.accepted.values().any(|work| {
                work.owner == owner
                    && (!work.mailbox.finished.load(Ordering::Acquire)
                        || work.close && work.source.is_some())
            })
    }
    pub fn pending(&self) -> bool {
        !self.release.is_empty()
            || self.retiring.values().any(|(_, retirement)| !retirement.ready())
            || self.accepted.values().any(|work| {
                !work.mailbox.finished.load(Ordering::Acquire)
                    || work.close && work.source.is_some()
            })
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        // Explicit engine shutdown only. Keep the ordinary per-file scheduler
        // while draining: sending all queued calls directly to two workers could
        // invert mutex acquisition and publish before an earlier accepted write.
        for alive in self.owners.values() {
            alive.store(false, Ordering::Release);
        }
        for work in self.accepted.values() {
            work.mailbox.disposed.store(true, Ordering::Release);
        }
        let progress = self.pool.progress.clone();
        while self.accepted.values().any(|work| {
            !work.mailbox.finished.load(Ordering::Acquire) || work.close && work.source.is_some()
        }) {
            let before = *progress.sequence.lock().expect("worker progress lock is not poisoned");
            if self.maintain().is_err() {
                break;
            }
            if !self.accepted.values().any(|work| {
                !work.mailbox.finished.load(Ordering::Acquire)
                    || work.close && work.source.is_some()
            }) {
                break;
            }
            let sequence = progress.sequence.lock().expect("worker progress lock is not poisoned");
            drop(
                progress
                    .changed
                    .wait_while(sequence, |sequence| *sequence == before)
                    .expect("worker progress lock is not poisoned"),
            );
        }
        self.collect();
        for slot in self.slots.drain() {
            self.release.push_back(slot.cell);
        }
        for cell in self.release.drain(..) {
            let _ =
                self.pool.send.as_ref().expect("executor still owns pool").send(Job::Release(cell));
        }
    }
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
