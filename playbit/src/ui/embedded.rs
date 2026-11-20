//! UI-owned guest execution with explicit local controls and engine management.
//! The host serializes calls, receives the engine callback or watches its
//! `completion_socket`, and arms only
//! deadlines or a returned zero delay for already-ready admitted work.
use crate::engine::reply::Reply as EngineReply;
use crate::engine::{
    ChangeToken, Engine, Request as EngineRequest, Result, admission::request_ready,
};
use crate::store::Id;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Local controls never enter the engine IPC protocol. Open is an explicit new
/// owner; every other local control addresses the returned session instance.
#[derive(Clone, Debug)]
pub enum Request {
    Database(Vec<u8>),
    Management(EngineRequest),
    Open { session: String },
    Frame { instance: Id },
    Input { instance: Id, kind: u32, target: u32, data: Vec<u8> },
    Logs { instance: Id, after: Option<crate::engine::LogCursor> },
    Quit { instance: Id },
}
#[derive(Debug, serde::Serialize)]
pub struct Frame {
    pub instance: Id,
    pub generation: u64,
    pub session: crate::store::Session,
    pub view: Vec<u8>,
    pub logs: Vec<String>,
    pub invitation: Option<String>,
    pub availability: String,
    pub update: Option<crate::engine::UpdateStatus>,
}
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum Reply {
    Database(Vec<u8>),
    Management(EngineReply),
    Opened { instance: Id, generation: u64 },
    Frame(Frame),
    Logs(crate::engine::LogBatch),
    Quit { instance: Id, drain_failed: bool },
}
const MAX_PENDING: usize = 32;
enum Pending {
    Request(Request),
    Network(crate::engine::background::NetworkTicket),
    Closing { instance: Id },
}
struct Entry {
    id: u64,
    state: Pending,
}

/// Bounded management admission and UI-owned local guest execution.
/// The caller bounds aggregate request bytes before decoding and owns response
/// delivery. Dropping this owner never replays an admitted operation.
#[derive(Default)]
pub struct EmbeddedOwner {
    pending: VecDeque<Entry>,
    pub(crate) apps: crate::ui::local_guest::Apps,
    observing: bool,
}

pub struct Step {
    pub response: Option<(u64, Result<Reply>)>,
    pub changes: ChangeToken,
    pub wake_after: Option<Duration>,
    /// A background failure independent of any submitted request's response.
    pub error: Option<String>,
}

impl EmbeddedOwner {
    /// Sign-out retains the owner on failure. Wait only on admitted app work or
    /// its next runnable deadline, on the host's background runtime worker.
    #[cfg(feature = "ios")]
    pub(crate) fn drain_apps(&mut self, engine: &mut Engine, deadline: Instant) -> Result<()> {
        if !self.pending.is_empty() {
            return Err("Playbit is finishing another action. Try again.".into());
        }
        self.apps.deactivate_all();
        loop {
            // Inactive advancement collects writes without launching fresh network work.
            if let Some(error) = self.advance(engine, true, false, false).error {
                return Err(error.into());
            }
            if !self.apps.shutdown_pending()? {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(
                    "Apps are still saving. Your data is retained; try signing out again.".into()
                );
            }
            let ready =
                self.apps.wake_after(now).map_or(deadline, |delay| (now + delay).min(deadline));
            let socket = engine.completion_socket().ok_or("Sign-out needs runtime readiness")?;
            crate::engine::readiness::wait_readable(socket, ready)?;
        }
    }
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe local guest readiness and storage completion in real fixtures.
    /// Unlike Quit's finite-root barrier, this includes reader refresh work.
    #[cfg(any(feature = "test-support", feature = "desktop-qualification"))]
    pub fn work_pending(&self, instance: Id) -> Result<bool> {
        self.apps.work_pending(instance)
    }

    pub fn submit(&mut self, id: u64, request: Request) -> Result<()> {
        if self.pending.len() == MAX_PENDING {
            return Err("Embedded request queue is full; this request did not start".into());
        }
        if self.pending.iter().any(|entry| entry.id == id) {
            return Err(
                "Embedded request identifier is already pending; this request did not start".into(),
            );
        }
        if matches!(&request, Request::Input { data, .. } if data.len() > playbit_api::abi::syscall::MAX_EVENT_BYTES)
        {
            return Err("Guest input exceeds the event limit; this request did not start".into());
        }
        if matches!(&request, Request::Database(data) if data.len() > crate::engine::database_browser::MAX_REQUEST_BYTES)
        {
            return Err("Database browser request exceeds its limit".into());
        }
        if matches!(request, Request::Management(EngineRequest::WaitChanges { .. })) {
            return Err("Embedded hosts observe the returned change token".into());
        }
        assert!(self.pending.len() < MAX_PENDING, "embedded admission owns a pending slot");
        self.pending.push_back(Entry { id, state: Pending::Request(request) });
        Ok(())
    }

    /// Request enrolled catch-up once after actual application suspension.
    /// View appearance is not a reconnect boundary. Dispatch remains deferred
    /// until the host next advances while active and current guest turns drain.
    pub fn resume_network(&mut self, engine: &mut Engine) {
        if engine.enrolled() {
            engine.bootstrap_pending = true;
        }
    }

    /// `completion` reflects a host wake or readiness of Engine's descriptor. A busy
    /// main thread sets `delivery_ready=false`: runtime/replication still advance,
    /// while host responses keep their existing bounded worker-result credits.
    /// Inactive hosts collect prior work but dispatch no new request or guest.
    /// Before suspension the host cancels a transfer, so its completion cannot
    /// initialize a newly joined guest while the application is inactive.
    pub fn advance(
        &mut self,
        engine: &mut Engine,
        completion: bool,
        active: bool,
        delivery_ready: bool,
    ) -> Step {
        if !self.observing {
            let notices = self.apps.notices();
            engine.service_observer =
                Some(std::sync::Arc::new(move |batch| notices.received(batch)));
            self.observing = true;
        }
        let error = completion.then(|| engine.complete_background().err()).flatten().map(|error| {
            let mut message = error.to_string();
            let mut end = message.len().min(512);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message
        });
        if self.apps.advance(active) {
            engine.state_changed();
        }
        engine.embedded_sessions = self.apps.sessions();
        let (mut response, dispatched) =
            if delivery_ready { self.deliver(engine, active) } else { (None, false) };
        // A returned response keeps the host's sole delivery credit until its
        // explicit main-thread ACK. Background work continues in the meantime.
        if active
            && let Some(next) =
                self.start_background(engine, delivery_ready && response.is_none(), dispatched)
        {
            assert!(response.is_none(), "a host delivery owns one response");
            response = Some(next);
        }
        let delivery_ready = delivery_ready && response.is_none();
        let ready = delivery_ready
            && (self.completed(engine).is_some()
                || (active && self.dispatchable(engine).is_some()));
        let wake_after = if ready {
            Some(Duration::ZERO)
        } else if active && (!delivery_ready || !self.draining(engine)) {
            let now = Instant::now();
            self.apps.wake_after(now).into_iter().chain(engine.network_wake_after(now)).min()
        } else {
            None
        };
        Step { response, changes: engine.change_token(), wake_after, error }
    }

    fn deliver(
        &mut self,
        engine: &mut Engine,
        active: bool,
    ) -> (Option<(u64, Result<Reply>)>, bool) {
        let completed = self.completed(engine);
        let dispatchable = active.then(|| self.dispatchable(engine)).flatten();
        // An older completed ticket cannot starve behind fresh synchronous work.
        if let Some(index) = completed
            && dispatchable.is_none_or(|ready| index < ready)
        {
            return (Some(self.take_completed(engine, index)), false);
        }
        let dispatched = dispatchable.is_some();
        let mut response = dispatchable.and_then(|index| self.dispatch(engine, index));
        if response.is_none()
            && let Some(index) = self.completed(engine)
        {
            response = Some(self.take_completed(engine, index));
        }
        (response, dispatched)
    }
    fn start_background(
        &mut self,
        engine: &mut Engine,
        delivery_ready: bool,
        dispatched: bool,
    ) -> Option<(u64, Result<Reply>)> {
        engine.embedded_sessions = self.apps.sessions();
        if delivery_ready && self.draining(engine) {
            return None;
        }
        engine.start_network_work();
        // Handoff can clear admission's drain flag. Cached work can start on this
        // turn instead of waiting for a network ACK which may not change state.
        if delivery_ready && !dispatched {
            self.dispatchable(engine).and_then(|index| self.dispatch(engine, index))
        } else {
            None
        }
    }

    fn completed(&self, engine: &Engine) -> Option<usize> {
        self.pending.iter().position(|entry| match &entry.state {
            Pending::Network(ticket) => engine.network_request_complete(ticket),
            Pending::Closing { instance, .. } => {
                self.apps.drain_pending(*instance).map_or(true, |pending| !pending)
            }
            Pending::Request(_) => false,
        })
    }
    fn take_completed(&mut self, engine: &mut Engine, index: usize) -> (u64, Result<Reply>) {
        assert!(self.pending.len() <= MAX_PENDING, "embedded queue stays bounded at drain");
        let entry = self.pending.remove(index).expect("index was found in the pending queue");
        let result = match entry.state {
            Pending::Network(ticket) => engine.take_network_request(ticket).map(Reply::Management),
            Pending::Closing { instance } => {
                let drain = self.apps.drain_pending(instance);
                assert!(!matches!(drain, Ok(true)), "completed close has no runnable drain");
                // Failed guests cannot run again. Retire their UI owner while
                // close keeps any accepted backend work in the retired inbox.
                self.apps
                    .close(instance)
                    .map(|()| Reply::Quit { instance, drain_failed: drain.is_err() })
            }
            Pending::Request(_) => unreachable!(),
        };
        (entry.id, result)
    }
    fn dispatchable(&self, engine: &Engine) -> Option<usize> {
        for (index, entry) in self.pending.iter().enumerate() {
            let Pending::Request(request) = &entry.state else { continue };
            let Request::Management(request) = request else { return Some(index) };
            if request_ready(request, engine) {
                return Some(index);
            }
        }
        None
    }
    fn dispatch(&mut self, engine: &mut Engine, index: usize) -> Option<(u64, Result<Reply>)> {
        assert!(self.pending.len() <= MAX_PENDING, "embedded queue stays bounded at drain");
        let entry = self.pending.remove(index).expect("index was found in the pending queue");
        let Pending::Request(request) = entry.state else { unreachable!() };
        let Request::Management(request) = request else {
            return self.app_request(engine, entry.id, index, request);
        };
        let asynchronous = crate::engine::developer::is_request(&request)
            || crate::engine::enrollment::is_request(&request)
            || crate::engine::diagnostics::is_request(&request)
            || matches!(
                request,
                EngineRequest::Sync
                    | EngineRequest::RecoveryStatus
                    | EngineRequest::PrepareRecovery
                    | EngineRequest::SaveRecovery
                    | EngineRequest::RecoverCredentials { .. }
            );
        if asynchronous {
            match engine.begin_network_request(request) {
                Ok(ticket) => {
                    self.pending
                        .insert(index, Entry { id: entry.id, state: Pending::Network(ticket) });
                    None
                }
                Err(error) => Some((entry.id, Err(error))),
            }
        } else {
            Some((entry.id, engine.dispatch_reply(request).map(Reply::Management)))
        }
    }
    fn app_request(
        &mut self,
        engine: &mut Engine,
        id: u64,
        index: usize,
        request: Request,
    ) -> Option<(u64, Result<Reply>)> {
        if let Request::Quit { instance } = request {
            return match self.apps.deactivate(instance) {
                Ok(()) => {
                    self.pending.insert(index, Entry { id, state: Pending::Closing { instance } });
                    None
                }
                Err(error) => Some((id, Err(error))),
            };
        }
        let result = (|| match request {
            Request::Database(bytes) => {
                let (changed, result) =
                    crate::engine::database_browser::call(&engine.services.store, &bytes);
                if changed {
                    engine.state_changed();
                }
                result.map(Reply::Database).map_err(Into::into)
            }
            Request::Open { session } => {
                let instance = self.open_guest(engine, &session)?;
                Ok(Reply::Opened { instance, generation: self.apps.generation(instance)? })
            }
            Request::Frame { instance } => self.guest_reply(engine, instance),
            Request::Input { instance, kind, target, data } => {
                self.apps.input(instance, crate::engine::input_event(kind, target, data)?)?;
                if self.apps.advance(true) {
                    engine.state_changed();
                }
                self.guest_reply(engine, instance)
            }
            Request::Logs { instance, after } => self
                .apps
                .log_batch(instance, engine.change_token().generation, after)
                .and_then(|batch| batch.ok_or_else(|| "No new app logs are ready".into()))
                .map(Reply::Logs),
            Request::Management(_) | Request::Quit { .. } => {
                unreachable!("management and Quit have separate dispatch")
            }
        })();
        Some((id, result))
    }
    fn open_guest(&mut self, engine: &mut Engine, session: &str) -> Result<Id> {
        engine.services.store.retain_session(session)?;
        let wake = engine.completion.wake.clone();
        // Return the known owner before its first guest turn or data admission.
        self.apps.open(
            &engine.services,
            session,
            engine.network.blobs.clone(),
            std::sync::Arc::new(move || wake.notify()),
        )
    }
    fn guest_reply(&mut self, engine: &Engine, instance: Id) -> Result<Reply> {
        let session = self.apps.session(instance)?;
        let retained = engine.services.store.session(session)?;
        let invitation = if retained.shared_volume.is_some() {
            // Showing a link does not initialize data or claim AP readiness.
            Some(engine.services.store.session_reference(session)?.link(engine.channel()))
        } else if engine.services.store.is_local_session(session)? {
            None
        } else {
            Some(engine.invitation(session)?)
        };
        Ok(Reply::Frame(Frame {
            instance,
            generation: self.apps.generation(instance)?,
            session: retained,
            view: self.apps.frame_bytes(instance)?.to_vec(),
            logs: self.apps.logs(instance)?.to_vec(),
            invitation,
            availability: engine.availability_cache.clone(),
            update: engine.updates.get(session).cloned(),
        }))
    }
    fn draining(&self, engine: &Engine) -> bool {
        self.pending.iter().any(|entry| {
            matches!(&entry.state, Pending::Request(Request::Management(request))
            if request_ready(request, engine))
        })
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use polling::{Event, Events, Poller};

    struct Fixture {
        _directory: tempfile::TempDir,
        engine: Engine,
        owner: EmbeddedOwner,
        session: String,
        requests: std::collections::BTreeMap<u64, Request>,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = engine.services.store.create_volume("Local", false).unwrap();
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap().id;
            let mut owner = EmbeddedOwner::new();
            owner.open_guest(&mut engine, &session).unwrap();
            if owner.apps.advance(true) {
                engine.state_changed();
            }
            Self { _directory: directory, engine, owner, session, requests: Default::default() }
        }
        fn submit(&mut self, id: u64, request: EngineRequest) -> Result<()> {
            let request = Request::Management(request);
            self.owner.submit(id, request.clone())?;
            assert!(self.requests.insert(id, request).is_none());
            Ok(())
        }
        fn app(&mut self, id: u64, request: Request) -> Result<()> {
            self.owner.submit(id, request.clone())?;
            assert!(self.requests.insert(id, request).is_none());
            Ok(())
        }
        fn advance(&mut self, completion: bool, active: bool) -> Step {
            let step = self.owner.advance(&mut self.engine, completion, active, true);
            assert!(step.error.is_none(), "{:?}", step.error);
            if let Some((id, result)) = &step.response {
                let request =
                    self.requests.remove(id).expect("completion must have an admitted request");
                if let (Request::Management(request), Ok(Reply::Management(reply))) =
                    (&request, result)
                {
                    use crate::engine::{ipc::codec, reply::ArchivedReply};
                    let bytes = codec::encode(reply, codec::MAX_ARCHIVE).unwrap();
                    let archived = codec::access::<ArchivedReply>(&bytes).unwrap();
                    assert!(
                        archived.matches_request(request),
                        "owner completed {request:?} with {reply:?}"
                    );
                    let unrelated = if matches!(request, EngineRequest::LocalProtocol) {
                        EngineRequest::Status
                    } else {
                        EngineRequest::LocalProtocol
                    };
                    assert!(!archived.matches_request(&unrelated));
                }
            }
            step
        }
        #[allow(
            unsafe_code,
            reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
        )]
        fn wait(&mut self, timeout: Option<Duration>, active: bool) -> Step {
            let socket = self
                .engine
                .completion_socket()
                .expect("Unix engine completion")
                .try_clone()
                .unwrap();
            let poller = Poller::new().unwrap();
            // The socket stays alive until explicitly unregistered below.
            unsafe {
                poller.add(&socket, Event::readable(1)).unwrap();
            }
            let mut events = Events::new();
            poller.wait(&mut events, Some(timeout.unwrap_or(Duration::from_secs(5)))).unwrap();
            poller.delete(&socket).unwrap();
            assert!(
                !events.is_empty() || timeout.is_some(),
                "missing real completion notification"
            );
            self.advance(!events.is_empty(), active)
        }
        fn instance(&self) -> Id {
            self.owner.apps.id(&self.session).unwrap()
        }
        fn input(&self, target: u32) -> Request {
            Request::Input { instance: self.instance(), kind: 2, target, data: vec![] }
        }
    }

    #[test]
    fn diagnostic_sync_collects_failure_only_after_actual_completion_readiness() {
        let mut f = Fixture::new();
        let (send, remote) = crate::engine::background::tests::transfer(&mut f.engine);
        let ticket = crate::engine::background::tests::diagnostic_ticket(&mut f.engine);
        f.owner.pending.push_back(Entry { id: 1, state: Pending::Network(ticket) });
        f.requests.insert(1, Request::Management(EngineRequest::Sync));
        send.send((remote, Err("deliberate transfer failure".into()), Default::default())).unwrap();
        crate::engine::background::tests::notify(&f.engine);
        assert!(
            f.advance(false, true).response.is_none(),
            "ordinary requests do not drain completion"
        );
        let step = f.wait(None, false);
        assert!(
            step.response
                .unwrap()
                .1
                .unwrap_err()
                .to_string()
                .contains("deliberate transfer failure")
        );
        assert!(f.engine.network_request_ready());
        assert!(f.advance(false, true).response.is_none());
    }
    #[test]
    fn sync_request_uses_existing_async_ticket_and_returns_one_terminal_result() {
        let mut f = Fixture::new();
        f.engine.remote.as_mut().unwrap().set_test_transport(
            crate::network::ap::ClientConfig::platform("127.0.0.1:9".parse().unwrap(), "localhost"),
        );
        f.submit(1, EngineRequest::Sync).unwrap();
        let mut step = f.advance(false, true);
        assert!(matches!(
            f.owner.pending.front().map(|entry| &entry.state),
            Some(Pending::Network(_))
        ));
        assert!(step.response.is_none());
        while step.response.is_none() {
            step = f.wait(step.wake_after, true);
        }
        assert!(step.response.unwrap().1.is_err());
        assert!(f.engine.network_request_ready());
        assert!(f.advance(false, true).response.is_none());
    }
    #[test]
    fn canceled_transfer_completion_preserves_a_resume_requested_while_it_was_in_flight() {
        for complete_while_active in [false, true] {
            let mut f = Fixture::new();
            f.engine.remote.as_mut().unwrap().set_test_transport(
                crate::network::ap::ClientConfig::platform(
                    "127.0.0.1:9".parse().unwrap(),
                    "localhost",
                ),
            );
            let (send, remote) = crate::engine::background::tests::transfer(&mut f.engine);
            f.engine.dispatch_reply(EngineRequest::CancelTransfer).unwrap();
            // Represents the enrolled resume latch before the canceled worker
            // has returned ownership. The fixture uses no real account.
            f.engine.bootstrap_pending = true;
            send.send((
                remote,
                Ok(crate::engine::background::NetworkResult::Synchronized),
                Default::default(),
            ))
            .unwrap();
            crate::engine::background::tests::notify(&f.engine);
            f.wait(None, complete_while_active);
            if !complete_while_active {
                assert!(f.engine.bootstrap_pending);
                assert!(f.engine.background.is_none());
                assert_eq!(
                    f.engine.transfer_result.kind,
                    crate::engine::reply::TransferKind::Paused
                );
                f.advance(false, true);
            }
            assert!(!f.engine.bootstrap_pending, "one resumed operation consumes the latch");
            assert!(
                f.engine.background.is_some(),
                "resumed synchronization starts after Remote returns"
            );
            while f.engine.background.is_some() {
                f.wait(None, false);
            }
            assert_eq!(f.owner.apps.logs(f.owner.apps.id(&f.session).unwrap()).unwrap().len(), 1);
            assert!(f.advance(false, true).wake_after.is_none());
            assert!(
                f.engine.background.is_none(),
                "a terminal offline failure must not start another attempt"
            );
        }
    }
    #[test]
    fn bounded_queue_dispatches_one_request_per_step_with_correlated_responses() {
        let mut f = Fixture::new();
        let token = f.engine.change_token();
        for id in 1..=MAX_PENDING as u64 {
            f.app(id, Request::Frame { instance: f.instance() }).unwrap();
        }
        assert!(f.app(33, f.input(1)).unwrap_err().to_string().contains("full"));
        assert_eq!(f.owner.pending.len(), MAX_PENDING);
        let retained_bytes = std::mem::size_of::<EmbeddedOwner>()
            + f.owner.pending.capacity() * std::mem::size_of::<Entry>()
            + std::mem::size_of::<Step>();
        // Request entries and the sole response stay within the owner allowance.
        assert!(
            retained_bytes <= 4096,
            "{retained_bytes} bytes of bookkeeping: owner {}, entry {} × {}, step {}; engine request {}",
            std::mem::size_of::<EmbeddedOwner>(),
            std::mem::size_of::<Entry>(),
            f.owner.pending.capacity(),
            std::mem::size_of::<Step>(),
            std::mem::size_of::<EngineRequest>()
        );
        let blocked = f.owner.advance(&mut f.engine, false, true, false);
        assert!(blocked.response.is_none() && blocked.wake_after.is_none());
        assert_eq!(f.owner.pending.len(), MAX_PENDING);
        for id in 1..=MAX_PENDING as u64 {
            let step = f.advance(false, true);
            let (response_id, response) = step.response.unwrap();
            assert_eq!(response_id, id);
            assert!(response.is_ok());
            assert_eq!(step.changes, token);
            assert!(
                step.wake_after.is_none(),
                "returning a response consumes main delivery credit"
            );
            assert_eq!(f.owner.pending.len(), MAX_PENDING - id as usize);
        }
        assert_eq!(f.owner.apps.logs(f.owner.apps.id(&f.session).unwrap()).unwrap().len(), 1);
    }
    #[test]
    fn local_start_input_and_stop_leave_engine_execution_empty() {
        let mut f = Fixture::new();
        f.app(1, f.input(1)).unwrap();
        let (id, result) = f.advance(false, true).response.unwrap();
        assert_eq!(id, 1);
        let Reply::Frame(frame) = result.unwrap() else { panic!("expected local scene") };
        assert_eq!(frame.logs, ["turn", "turn"]);
        f.app(2, Request::Quit { instance: f.instance() }).unwrap();
        let (id, result) = f.advance(false, true).response.unwrap();
        assert_eq!(id, 2);
        assert!(result.is_ok());
        assert!(f.owner.apps.id(&f.session).is_none());
    }
    #[test]
    fn explicit_open_returns_a_new_owner_before_execution_and_quit_preserves_its_sibling() {
        let mut f = Fixture::new();
        let first = f.instance();
        f.app(1, Request::Open { session: f.session.clone() }).unwrap();
        let Reply::Opened { instance: second, generation } =
            f.advance(false, true).response.unwrap().1.unwrap()
        else {
            panic!("Open returns a known session-instance receipt")
        };
        assert_ne!(first, second);
        assert!(generation > 0);
        assert!(f.owner.apps.logs(second).unwrap().is_empty(), "Open precedes Started");
        f.app(2, Request::Frame { instance: second }).unwrap();
        let Reply::Frame(frame) = f.advance(false, true).response.unwrap().1.unwrap() else {
            panic!("Frame addresses its exact local owner")
        };
        assert_eq!(frame.instance, second);
        assert_eq!(frame.generation, generation);
        assert_eq!(frame.session.id, f.session);
        assert_eq!(frame.invitation, Some(f.engine.invitation(&f.session).unwrap()));
        assert_eq!(frame.logs, ["turn"]);
        f.app(3, Request::Input { instance: second, kind: 2, target: 1, data: vec![] }).unwrap();
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        assert_eq!(f.owner.apps.logs(first).unwrap(), ["turn"]);
        assert_eq!(f.owner.apps.logs(second).unwrap(), ["turn", "turn"]);
        f.app(4, Request::Quit { instance: second }).unwrap();
        assert!(matches!(f.advance(false, true).response.unwrap().1.unwrap(),
            Reply::Quit { instance, drain_failed: false } if instance == second));
        f.app(5, Request::Frame { instance: second }).unwrap();
        assert!(f.advance(false, true).response.unwrap().1.is_err(), "stale owner cannot resume");
        f.app(6, Request::Input { instance: first, kind: 2, target: 1, data: vec![] }).unwrap();
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        assert_eq!(f.owner.apps.logs(first).unwrap(), ["turn", "turn"]);
        assert!(f.advance(false, true).response.is_none(), "reads and errors cannot replay a turn");
    }
    #[test]
    fn quit_retires_failed_residents_and_restores_open_capacity() {
        let mut f = Fixture::new();
        let mut instances = vec![f.instance()];
        for _ in 1..4 {
            instances.push(f.owner.open_guest(&mut f.engine, &f.session).unwrap());
        }
        let old = f.engine.services.store.session(&f.session).unwrap();
        let mut package = crate::engine::tests::button_package();
        package = package.renamed("Replacement").unwrap();
        let target = f.engine.create_session(package, old.root_volume).unwrap();
        let mut selected = old.clone();
        selected.selector = target.selector;
        selected.resolved_build_id = target.resolved_build_id;
        f.engine.services.store.adopt_session(&old, &selected).unwrap();
        f.owner.apps.advance(true);
        #[cfg(feature = "ios")]
        assert!(
            f.owner.drain_apps(&mut f.engine, Instant::now() + Duration::from_secs(1)).is_err(),
            "sign-out must report a failed app drain and retain its owner"
        );
        for (index, instance) in instances.into_iter().enumerate() {
            f.app(index as u64 + 1, Request::Quit { instance }).unwrap();
            assert!(matches!(f.advance(false, true).response.unwrap().1.unwrap(),
                Reply::Quit { instance: closed, drain_failed: true } if closed == instance));
        }
        assert!(f.owner.open_guest(&mut f.engine, &f.session).is_ok());
    }

    #[cfg(feature = "ios")]
    #[test]
    fn sign_out_drains_apps_and_refuses_undelivered_requests() {
        // The shutdown barrier owns existing app work, never a queued UI operation.
        let mut f = Fixture::new();
        f.app(1, f.input(1)).unwrap();
        assert!(
            f.owner.drain_apps(&mut f.engine, Instant::now() + Duration::from_secs(1)).is_err()
        );
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        f.owner.drain_apps(&mut f.engine, Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(!f.owner.apps.shutdown_pending().unwrap());
        assert_eq!(f.owner.apps.logs(f.instance()).unwrap(), ["turn", "turn"]);
    }

    #[test]
    fn oversized_input_is_rejected_before_retaining_a_queue_slot() {
        let mut f = Fixture::new();
        assert!(
            f.app(
                1,
                Request::Input {
                    instance: f.instance(),
                    kind: 3,
                    target: 1,
                    data: vec![0; playbit_api::abi::syscall::MAX_EVENT_BYTES + 1],
                }
            )
            .unwrap_err()
            .to_string()
            .contains("did not start")
        );
        assert!(f.owner.pending.is_empty());
        assert_eq!(f.owner.apps.logs(f.instance()).unwrap(), ["turn"]);
        let maximum = playbit_api::abi::text::TextInput::encode(
            &"x".repeat(playbit_api::abi::text::MAX_TEXT_BYTES),
            &[playbit_api::abi::text::TextEdit { start: 0, end: 1, inserted_len: 1 };
                playbit_api::abi::text::MAX_TEXT_EDITS],
        )
        .unwrap();
        assert_eq!(maximum.len(), playbit_api::abi::syscall::MAX_EVENT_BYTES);
        f.app(2, Request::Input { instance: f.instance(), kind: 14, target: 1, data: maximum })
            .expect("complete metadata allowance reaches the embedded queue");
        assert_eq!(f.owner.pending.len(), 1);
    }
    #[test]
    fn inactive_and_delivery_blocked_hosts_keep_new_input_queued() {
        let mut f = Fixture::new();
        f.app(1, f.input(1)).unwrap();
        assert!(f.advance(false, false).response.is_none());
        assert_eq!(f.owner.pending.len(), 1);
        let blocked = f.owner.advance(&mut f.engine, false, true, false);
        assert!(blocked.response.is_none() && blocked.wake_after.is_none());
        assert_eq!(f.owner.pending.len(), 1);
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        assert!(f.advance(false, true).response.is_none());
    }
    #[test]
    fn duplicate_ids_invalid_controls_and_independent_snapshots_do_not_replay() {
        let mut f = Fixture::new();
        assert!(f.submit(1, EngineRequest::WaitChanges { after: None }).is_err());
        f.app(1, Request::Frame { instance: f.instance() }).unwrap();
        assert!(f.app(1, f.input(1)).is_err());
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        f.app(1, f.input(999)).unwrap();
        assert!(f.advance(false, true).response.unwrap().1.is_err());
        f.app(2, f.input(1)).unwrap();
        assert!(f.advance(false, true).response.unwrap().1.is_ok());
        let id = f.owner.apps.id(&f.session).unwrap();
        assert_eq!(f.owner.apps.logs(id).unwrap(), ["turn", "turn"]);
        for _ in 0..3 {
            assert!(f.advance(false, true).response.is_none());
        }
    }
    #[test]
    fn blocked_network_and_cancel_do_not_block_cached_local_input() {
        let mut f = Fixture::new();
        let (send, remote) = crate::engine::background::tests::transfer(&mut f.engine);
        f.submit(10, EngineRequest::RecoveryStatus).unwrap();
        f.app(20, f.input(1)).unwrap();
        f.submit(30, EngineRequest::CancelTransfer).unwrap();
        let (id, result) = f.advance(false, true).response.unwrap();
        assert_eq!(id, 20);
        assert!(result.is_ok());
        let (id, result) = f.advance(false, true).response.unwrap();
        assert_eq!(id, 30);
        assert!(result.is_ok());
        assert!(f.engine.background.is_some());
        send.send((
            remote,
            Ok(crate::engine::background::NetworkResult::Synchronized),
            Default::default(),
        ))
        .unwrap();
        crate::engine::background::tests::notify(&f.engine);
        let mut step = f.wait(None, true);
        while step.response.is_none() {
            step = f.wait(step.wake_after, true);
        }
        assert_eq!(step.response.unwrap().0, 10);
        assert!(f.owner.pending.is_empty());
    }
}
