//! Live, encrypted named channels. Storage authority is deliberately absent here.
use crate::{
    engine::Result,
    network::ap::{
        ApConnection, ApStream,
        channel::{self, Control},
        frame_type,
    },
    store::{Id, LocalStore, Session},
};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
};
const SESSION_MESSAGE_LIMIT: usize = channel::MAX_MESSAGE_BYTES;
pub(crate) const EVENT_LIMIT: usize = 64;
#[derive(Debug)]
pub(crate) struct PublicationUncertain;
#[derive(Debug)]
struct ConsumerOverflow;
impl std::fmt::Display for ConsumerOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel consumer overflow; live messages were dropped")
    }
}
impl std::error::Error for ConsumerOverflow {}
impl std::fmt::Display for PublicationUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel publication outcome is uncertain; it was not retried")
    }
}
impl std::error::Error for PublicationUncertain {}
pub(crate) fn host_error(
    error: Box<dyn std::error::Error + Send + Sync>,
) -> playbit_api::abi::syscall::Error {
    if error.is::<PublicationUncertain>() {
        playbit_api::abi::syscall::Error::Uncertain
    } else if error.is::<ConsumerOverflow>() {
        playbit_api::abi::syscall::Error::LimitExceeded
    } else {
        playbit_api::abi::syscall::Error::Unavailable
    }
}

pub use crate::network::ap::channel::Event;
struct Queued {
    event: Event,
    _session: OwnedSemaphorePermit,
    _account: OwnedSemaphorePermit,
}
struct Receiver {
    task: JoinHandle<()>,
    stopped: Arc<AtomicBool>,
}
impl Receiver {
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn abort(&self) {
        self.task.abort();
    }
}
pub struct SessionChannels {
    connection: Arc<ApConnection>,
    store: Arc<LocalStore>,
    session: Session,
    instance: Option<([u8; 16], [u8; 32])>,
    app_updates_only: bool,
    receive: Arc<tokio::sync::Mutex<mpsc::Receiver<Result<Queued>>>>,
    send: mpsc::Sender<Result<Queued>>,
    control: Arc<tokio::sync::Mutex<watch::Receiver<Option<Event>>>>,
    control_send: watch::Sender<Option<Event>>,
    failure: Arc<tokio::sync::Mutex<watch::Receiver<Option<playbit_api::abi::syscall::Error>>>>,
    failure_send: watch::Sender<Option<playbit_api::abi::syscall::Error>>,
    tasks: BTreeMap<String, Receiver>,
    own: Receiver,
    updates: Arc<AtomicU8>,
    wake: Arc<dyn Fn() + Send + Sync>,
    session_bytes: Arc<Semaphore>,
    account_bytes: Arc<Semaphore>,
}
impl SessionChannels {
    pub(crate) fn needs_public_app_observer(store: &LocalStore, session: &Session) -> Result<bool> {
        Ok((store.is_local_session(&session.id)? && session.shared_volume.is_none())
            || (session.shared_volume.is_some()
                && !store.shared_session_initialized(&session.id)?))
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn app_updates_only(&self) -> bool {
        self.app_updates_only
    }
    pub async fn listen(
        connection: Arc<ApConnection>,
        store: Arc<LocalStore>,
        session: Session,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::join(
            connection,
            store,
            session,
            None,
            Arc::new(Semaphore::new(channel::ACCOUNT_QUEUE_BYTES)),
            Arc::new(wake),
        )
        .await
    }
    pub async fn join(
        connection: Arc<ApConnection>,
        store: Arc<LocalStore>,
        session: Session,
        instance: Option<([u8; 16], [u8; 32])>,
        account_bytes: Arc<Semaphore>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        Self::join_budget(
            connection,
            store,
            session,
            instance,
            Arc::new(Semaphore::new(channel::SESSION_QUEUE_BYTES)),
            account_bytes,
            wake,
        )
        .await
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Independent queue scopes are owned for the receiver lifetime."
    )]
    async fn join_budget(
        connection: Arc<ApConnection>,
        store: Arc<LocalStore>,
        session: Session,
        instance: Option<([u8; 16], [u8; 32])>,
        session_bytes: Arc<Semaphore>,
        account_bytes: Arc<Semaphore>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let app_updates_only = Self::needs_public_app_observer(&store, &session)?;
        let (stream, destination, display_names) =
            control_subscription(&connection, &store, &session, instance, app_updates_only).await?;
        let (send, receive) = mpsc::channel(EVENT_LIMIT);
        let (control_send, control) = watch::channel(None);
        let (failure_send, failure) = watch::channel(None);
        let updates = Arc::new(AtomicU8::new(0));
        let task = receiver(
            stream,
            destination,
            None,
            send.clone(),
            control_send.clone(),
            failure_send.clone(),
            updates.clone(),
            wake.clone(),
            session_bytes.clone(),
            account_bytes.clone(),
            instance.is_none(),
            #[cfg(feature = "app-host")]
            instance.map(|_| (store.clone(), session.id.clone())),
            display_names,
        );
        Ok(Self {
            connection,
            store,
            session,
            instance,
            app_updates_only,
            receive: Arc::new(tokio::sync::Mutex::new(receive)),
            send,
            control: Arc::new(tokio::sync::Mutex::new(control)),
            control_send,
            failure: Arc::new(tokio::sync::Mutex::new(failure)),
            failure_send,
            tasks: BTreeMap::new(),
            own: task,
            updates,
            wake,
            session_bytes,
            account_bytes,
        })
    }
    pub fn has_update(&self) -> bool {
        self.updates.load(Ordering::Acquire) != 0
    }
    pub fn take_update(&self) -> u8 {
        self.updates.swap(0, Ordering::AcqRel)
    }
    pub async fn subscribe(&mut self, name: &str) -> Result<()> {
        if self.app_updates_only {
            return Err(
                "named channels for new app sessions require encrypted synchronization".into()
            );
        }
        let destination =
            channel::destination(&crate::identifier::decode(&self.session.id)?, name)?;
        if self.tasks.get(name).is_some_and(|receiver| !receiver.is_stopped()) {
            return Ok(());
        }
        self.unsubscribe(name);
        if self.tasks.len() >= channel::MAX_CHANNELS {
            return Err("channel subscription capacity exhausted".into());
        }
        let proof = self.store.session_proof(&self.session.id, self.store.account().id)?;
        let mut stream = self.connection.open_stream().await?;
        stream
            .send(&channel::first_frame(
                frame_type::SUBSCRIBE,
                destination.clone(),
                &proof,
                self.instance,
                &[],
            )?)
            .await?;
        let ack = tokio::time::timeout(
            self.connection.response_timeout(),
            stream.receive_with_body_limit(0),
        )
        .await??;
        channel::acknowledgement(&ack, frame_type::SUBSCRIBE, &destination)?;
        let task = receiver(
            stream,
            destination,
            Some((name.into(), self.store.session_message_key(&self.session.id)?)),
            self.send.clone(),
            self.control_send.clone(),
            self.failure_send.clone(),
            self.updates.clone(),
            self.wake.clone(),
            self.session_bytes.clone(),
            self.account_bytes.clone(),
            false,
            #[cfg(feature = "app-host")]
            None,
            None,
        );
        self.tasks.insert(name.into(), task);
        Ok(())
    }
    pub fn unsubscribe(&mut self, name: &str) {
        if let Some(task) = self.tasks.remove(name) {
            task.abort();
        }
    }
    pub async fn publish(&self, name: &str, data: &[u8]) -> Result<String> {
        self.publication(name.into(), data.to_vec())?.await
    }
    pub(crate) fn publication(
        &self,
        name: String,
        data: Vec<u8>,
    ) -> Result<impl std::future::Future<Output = Result<String>> + Send + 'static> {
        let connection = self.connection.clone();
        let instance = self.instance.ok_or("an observer cannot publish")?;
        let session = self.session.id.clone();
        let account = self.store.account().id;
        let proof = self.store.session_proof(&session, account)?;
        let key = self.store.session_message_key(&session)?;
        Ok(async move {
            if connection.is_closed() {
                return Err("channel disconnected".into());
            }
            if data.len() > channel::MAX_MESSAGE_BYTES {
                return Err("channel message exceeds 32 KiB".into());
            }
            let (instance, build) = instance;
            let destination = channel::destination(&crate::identifier::decode(&session)?, &name)?;
            let message = Id::generate()?;
            let mut source = [0; channel::SOURCE_BYTES];
            source[..16].copy_from_slice(account.as_bytes());
            source[16..32].copy_from_slice(&connection.channel_device()?);
            source[32..48].copy_from_slice(&instance);
            source[48..].copy_from_slice(&build);
            let aad = channel::associated_data(&destination, &source, message.as_bytes())?;
            let cipher = seal(key, &aad, &data)?;
            let payload = [message.as_bytes().as_slice(), &cipher].concat();
            let frame = channel::first_frame(
                frame_type::PUBLISH,
                destination.clone(),
                &proof,
                Some((instance, build)),
                &payload,
            )?;
            let mut stream = connection.open_stream().await?;
            let result = async {
                stream.send(&frame).await?;
                let ack = tokio::time::timeout(
                    connection.response_timeout(),
                    stream.receive_with_body_limit(0),
                )
                .await??;
                channel::acknowledgement(&ack, frame_type::PUBLISH, &destination)?;
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            }
            .await;
            result.map_err(|_| PublicationUncertain)?;
            // Acknowledgement remains authoritative if half-close is interrupted.
            let _ = stream.finish();
            Ok(message.to_string())
        })
    }
    pub async fn next(&mut self) -> Result<Event> {
        let mut receive = self.receive.lock().await;
        let mut control = self.control.lock().await;
        let mut failure = self.failure.lock().await;
        tokio::select! {
            biased;
            changed=failure.changed()=>{changed?;let error=*failure.borrow_and_update();Err(if error==Some(playbit_api::abi::syscall::Error::LimitExceeded){Box::new(ConsumerOverflow) as Box<dyn std::error::Error+Send+Sync>}else{"channel disconnected".into()})}
            changed=control.changed()=>{changed?;control.borrow_and_update().clone().ok_or_else(||"missing membership".into())}
            message=receive.recv()=>Ok(message.ok_or("session channel disconnected")??.event),
        }
    }
    pub fn drain(&mut self) -> Result<Vec<Event>> {
        if self.updates.load(Ordering::Acquire) & 2 != 0 {
            return Err(
                "session channel disconnected or overflowed; resubscription required".into()
            );
        }
        let mut result = Vec::new();
        for _ in 0..EVENT_LIMIT {
            match self
                .receive
                .try_lock()
                .map_err(|_| "channel receiver is already waiting")?
                .try_recv()
            {
                Ok(event) => result.push(event?.event),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(_) => return Err("session channel disconnected".into()),
            }
        }
        Ok(result)
    }
}
impl Drop for SessionChannels {
    fn drop(&mut self) {
        self.own.abort();
        for (_, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}
struct DisplayNames {
    names: watch::Receiver<String>,
    discover: Option<Arc<ApConnection>>,
}
impl DisplayNames {
    fn field(&mut self) -> Result<[u8; channel::DISPLAY_NAME_BYTES]> {
        let name = self.names.borrow_and_update();
        channel::display_name(name.as_bytes())?;
        let mut field = [0; channel::DISPLAY_NAME_BYTES];
        field[..name.len()].copy_from_slice(name.as_bytes());
        Ok(field)
    }

    async fn announce(
        mut self,
        mut writer: crate::network::ap::ApSendStream,
        destination: &[u8],
    ) -> Result<()> {
        if let Some(connection) = self.discover.take() {
            if !connection.discover_session_names().await {
                return std::future::pending().await;
            }
            let opt_in = crate::network::ap::Frame::new(
                frame_type::SUBSCRIBE,
                channel::SESSION,
                [],
                destination,
                self.field()?,
            )?;
            writer.send(&opt_in).await?;
        }
        // The watch coalesces renames during discovery and bounded stream writes.
        loop {
            self.names.changed().await?;
            let frame = {
                let name = self.names.borrow_and_update();
                let name = channel::display_name(name.as_bytes())?;
                crate::network::ap::Frame::new(
                    frame_type::SUBSCRIBE,
                    channel::SESSION,
                    [],
                    destination,
                    name.as_bytes(),
                )?
            };
            writer.send(&frame).await?;
        }
    }
}

/// Reuse the control stream for public app changes without joining a local
/// session to AP. Named/instance channels still require the session proof.
async fn control_subscription(
    connection: &Arc<ApConnection>,
    store: &LocalStore,
    session: &Session,
    instance: Option<([u8; 16], [u8; 32])>,
    app_updates_only: bool,
) -> Result<(ApStream, Vec<u8>, Option<DisplayNames>)> {
    let mut frame = if app_updates_only {
        if instance.is_some() {
            return Err("sharing app sessions requires encrypted synchronization, which is not supported yet".into());
        }
        let crate::store::BuildSelector::App { app } = &session.selector else {
            return Err("exact local sessions have no remote selection subscription".into());
        };
        if app != &session.app {
            return Err("app update subscription differs from retained identity".into());
        }
        let destination = crate::network::ap::operation::app_update_channel(app)?;
        crate::network::ap::Frame::new(
            frame_type::SUBSCRIBE,
            channel::SESSION,
            [],
            destination,
            [],
        )?
    } else {
        let destination = crate::identifier::decode::<32>(&session.id)?;
        let proof = store.session_proof(&session.id, store.account().id)?;
        let app_updates = match &session.selector {
            crate::store::BuildSelector::App { app } => {
                crate::network::ap::operation::app_update_channel(app)?.to_vec()
            }
            _ => vec![],
        };
        channel::first_frame(
            frame_type::SUBSCRIBE,
            destination.to_vec(),
            &proof,
            instance,
            &app_updates,
        )?
    };
    let display_names = if instance.is_some() {
        match connection.cached_session_names() {
            Some(false) => None,
            known => {
                let mut names = DisplayNames {
                    names: store.subscribe_display_name(),
                    discover: known.is_none().then(|| connection.clone()),
                };
                if known == Some(true) {
                    frame.body.extend(names.field()?);
                }
                Some(names)
            }
        }
    } else {
        None
    };
    let destination = frame.destination.clone();
    let mut stream = connection.open_stream().await?;
    stream.send(&frame).await?;
    let ack =
        tokio::time::timeout(connection.response_timeout(), stream.receive_with_body_limit(0))
            .await??;
    channel::acknowledgement(&ack, frame_type::SUBSCRIBE, &destination)?;
    Ok((stream, destination, display_names))
}

#[allow(
    clippy::too_many_arguments,
    reason = "A receiver owns its bounded queue credits and immutable authentication context."
)]
fn receiver(
    stream: ApStream,
    destination: Vec<u8>,
    named_channel: Option<(String, [u8; 32])>,
    send: mpsc::Sender<Result<Queued>>,
    control: watch::Sender<Option<Event>>,
    failure: watch::Sender<Option<playbit_api::abi::syscall::Error>>,
    updates: Arc<AtomicU8>,
    wake: Arc<dyn Fn() + Send + Sync>,
    session_bytes: Arc<Semaphore>,
    account_bytes: Arc<Semaphore>,
    observer: bool,
    #[cfg(feature = "app-host")] roster: Option<(Arc<LocalStore>, String)>,
    display_names: Option<DisplayNames>,
) -> Receiver {
    let stopped = Arc::new(AtomicBool::new(false));
    let terminal = stopped.clone();
    let task = tokio::spawn(async move {
        let (writer, mut reader) = stream.split();
        let read = async {
            let limit = if named_channel.is_none() {
                channel::MAX_CONTROL_BYTES
            } else {
                channel::MAX_MESSAGE_BYTES + 44
            };
            loop {
                let frame = reader.receive_with_body_limit(limit).await?;
                let event = match channel::incoming(
                    &destination,
                    frame.frame_type,
                    &frame.destination,
                    &frame.source,
                    &frame.body,
                )? {
                    channel::Incoming::Overflow => {
                        return Err(
                            Box::new(ConsumerOverflow) as Box<dyn std::error::Error + Send + Sync>
                        );
                    }
                    channel::Incoming::Message { sender, message, sealed } => {
                        let source =
                            frame.source.as_slice().try_into().expect("validated source width");
                        let aad = channel::associated_data(&destination, source, &message)?;
                        let (name, key) =
                            named_channel.as_ref().expect("named message destination");
                        let data = open(*key, &aad, sealed.to_vec())?;
                        Event::Message {
                            channel: name.clone(),
                            sender,
                            message_id: crate::identifier::encode(&message),
                            data,
                        }
                    }
                    channel::Incoming::Control(Control::Presence { participants }) => {
                        #[cfg(feature = "app-host")]
                        if let Some((store, session)) = &roster {
                            store.observe_session_participants(session, &participants);
                        }
                        Event::Presence { participants }
                    }
                    channel::Incoming::Control(Control::UpdateAvailable) => {
                        updates.fetch_or(1, Ordering::Release);
                        wake();
                        continue;
                    }
                };
                if observer {
                    wake();
                    continue;
                }
                if named_channel.is_none() {
                    control.send_replace(Some(event));
                    wake();
                    continue;
                }
                let bytes = frame.body.len() + frame.source.len() + frame.destination.len() + 128;
                let queued = Queued {
                    event,
                    _session: session_bytes
                        .clone()
                        .try_acquire_many_owned(bytes.try_into()?)
                        .map_err(|_| ConsumerOverflow)?,
                    _account: account_bytes
                        .clone()
                        .try_acquire_many_owned(bytes.try_into()?)
                        .map_err(|_| ConsumerOverflow)?,
                };
                send.try_send(Ok(queued)).map_err(|_| ConsumerOverflow)?;
                wake();
            }
            #[allow(unreachable_code)]
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        };
        let write = async {
            match display_names {
                Some(names) => names.announce(writer, &destination).await,
                None => std::future::pending().await,
            }
        };
        if let Err(error) = tokio::try_join!(read, write) {
            report_receiver_failure(&terminal, &updates, &failure, error);
            wake();
        }
    });
    Receiver { task, stopped }
}
fn report_receiver_failure(
    stopped: &AtomicBool,
    updates: &AtomicU8,
    failure: &watch::Sender<Option<playbit_api::abi::syscall::Error>>,
    error: Box<dyn std::error::Error + Send + Sync>,
) {
    // Recovery may run on another worker as soon as the watch notification is
    // published. It must see termination even while this task is still exiting.
    stopped.store(true, Ordering::Release);
    updates.fetch_or(2, Ordering::Release);
    failure.send_replace(Some(host_error(error)));
}
fn seal(key: [u8; 32], aad: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let key = LessSafeKey::new(
        UnboundKey::new(&aead::AES_256_GCM, &key).map_err(|_| "invalid message key")?,
    );
    let mut nonce = [0; 12];
    SystemRandom::new().fill(&mut nonce).map_err(|_| "message nonce generation failed")?;
    let mut ciphertext = data.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut ciphertext,
    )
    .map_err(|_| "message encryption failed")?;
    Ok([nonce.as_slice(), &ciphertext].concat())
}
fn open(key: [u8; 32], aad: &[u8], ciphertext: Vec<u8>) -> Result<Vec<u8>> {
    if ciphertext.len() < 28 || ciphertext.len() > SESSION_MESSAGE_LIMIT + 28 {
        return Err("invalid message size".into());
    }
    let key = LessSafeKey::new(
        UnboundKey::new(&aead::AES_256_GCM, &key).map_err(|_| "invalid message key")?,
    );
    let nonce = ciphertext[..12].try_into()?;
    let mut data = ciphertext[12..].to_vec();
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut data)
        .map_err(|_| "session message authentication failed")?;
    Ok(plain.to_vec())
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
type DeliveryIdentity = ([u8; 16], [u8; 16], [u8; 16]);
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
type DeliveryCredits = (OwnedSemaphorePermit, OwnedSemaphorePermit);
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
#[derive(Default)]
struct InstanceEndpoint {
    retired: std::sync::atomic::AtomicBool,
    channels: tokio::sync::Mutex<Option<SessionChannels>>,
    desired: tokio::sync::Mutex<std::collections::BTreeSet<String>>,
    delivered: std::sync::Mutex<BTreeMap<DeliveryIdentity, DeliveryCredits>>,
}
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
impl InstanceEndpoint {
    fn check(&self) -> Result<()> {
        if self.retired.load(Ordering::Acquire) {
            Err("channel generation retired".into())
        } else {
            Ok(())
        }
    }
}
/// Shared by native, process and mobile hosts on the account AP connection.
/// Waiting for a message never holds the endpoint's command lock.
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
#[derive(Default)]
pub(crate) struct Hub {
    entries: std::sync::Mutex<BTreeMap<[u8; 16], Arc<InstanceEndpoint>>>,
    bytes: std::sync::OnceLock<Arc<Semaphore>>,
    scopes: tokio::sync::Mutex<BTreeMap<String, std::sync::Weak<Semaphore>>>,
}
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
impl Hub {
    /// Cleanup is local, including while AP is disconnected. Keep the own channel alive.
    pub async fn unsubscribe(
        &self,
        instance: [u8; 16],
        name: &str,
    ) -> Result<crate::runtime::host::DataResult> {
        let endpoint =
            self.entries.lock().expect("channel registry poisoned").get(&instance).cloned();
        if let Some(endpoint) = endpoint {
            endpoint.check()?;
            let mut channels = endpoint.channels.lock().await;
            endpoint.check()?;
            if let Some(channels) = channels.as_mut() {
                channels.unsubscribe(name);
            }
            endpoint.desired.lock().await.remove(name);
        }
        Ok(crate::runtime::host::DataResult::ChannelCompleted)
    }
    pub async fn execute(
        &self,
        connection: Arc<ApConnection>,
        store: Arc<LocalStore>,
        session: Session,
        operation: crate::runtime::host::DataOperation,
    ) -> Result<crate::runtime::host::DataResult> {
        use crate::runtime::host::{DataOperation as O, DataResult as R};
        let instance = match &operation {
            O::ChannelEvents { instance } | O::ChannelCommand { instance, .. } => *instance,
            _ => return Err("not a channel request".into()),
        };
        let endpoint = {
            let mut entries = self.entries.lock().expect("channel registry poisoned");
            if !entries.contains_key(&instance) && entries.len() >= 64 {
                return Err("channel instance capacity exceeded".into());
            }
            entries.entry(instance).or_insert_with(|| Arc::new(InstanceEndpoint::default())).clone()
        };
        {
            endpoint.check()?;
            let mut binding = endpoint.channels.lock().await;
            if binding.as_ref().is_none_or(|e| {
                !Arc::ptr_eq(&e.connection, &connection)
                    || e.own.is_stopped()
                    || e.session.resolved_build_id != session.resolved_build_id
            }) {
                let names = endpoint.desired.lock().await.clone();
                binding.take();
                let session_bytes = {
                    let mut scopes = self.scopes.lock().await;
                    scopes.retain(|_, scope| scope.strong_count() > 0);
                    if let Some(scope) = scopes.get(&session.id).and_then(std::sync::Weak::upgrade)
                    {
                        scope
                    } else {
                        let scope = Arc::new(Semaphore::new(channel::SESSION_QUEUE_BYTES));
                        scopes.insert(session.id.clone(), Arc::downgrade(&scope));
                        scope
                    }
                };
                let build = crate::identifier::decode(&session.resolved_build_id)?;
                let mut replacement = SessionChannels::join_budget(
                    connection,
                    store,
                    session,
                    Some((instance, build)),
                    session_bytes,
                    self.bytes
                        .get_or_init(|| Arc::new(Semaphore::new(channel::ACCOUNT_QUEUE_BYTES)))
                        .clone(),
                    Arc::new(|| {}),
                )
                .await?;
                for name in names {
                    replacement.subscribe(&name).await?;
                }
                endpoint.check()?;
                *binding = Some(replacement);
            } else if let Some(channels) = binding.as_mut() {
                // A named stream's overflow cannot retire the own-channel
                // membership or healthy named subscriptions.
                let names = endpoint.desired.lock().await.clone();
                let mut refreshed = false;
                for name in names {
                    if channels.tasks.get(&name).is_none_or(Receiver::is_stopped) {
                        channels.unsubscribe(&name);
                        channels.subscribe(&name).await?;
                        refreshed = true;
                    }
                }
                if refreshed {
                    channels.control_send.send_modify(|_| {});
                }
                channels.updates.fetch_and(!6, Ordering::AcqRel);
            }
        }
        match operation {
            O::ChannelEvents { .. } => {
                let (receive, control, failure, updates) = {
                    let entry = endpoint.channels.lock().await;
                    let entry = entry.as_ref().ok_or("channel instance retired")?;
                    (
                        entry.receive.clone(),
                        entry.control.clone(),
                        entry.failure.clone(),
                        entry.updates.clone(),
                    )
                };
                loop {
                    endpoint.check()?;
                    let mut messages = receive.lock().await;
                    let mut controls = control.lock().await;
                    let mut failures = failure.lock().await;
                    let queued = tokio::select! {
                        biased;
                        result=failures.changed()=>{
                            result?;
                            let status=*failures.borrow_and_update();
                            updates.fetch_or(4,Ordering::Release);
                            return Err(if status==Some(playbit_api::abi::syscall::Error::LimitExceeded){Box::new(ConsumerOverflow) as Box<dyn std::error::Error+Send+Sync>}else{"channel disconnected".into()});
                        }
                        result=controls.changed()=>{result?;let event=controls.borrow_and_update().clone().ok_or("missing membership")?;return Ok(R::ChannelEvents(vec![event]));}
                        queued=messages.recv()=>queued.ok_or("channel disconnected")??,
                    };
                    drop(messages);
                    drop(controls);
                    drop(failures);
                    endpoint.check()?;
                    let Queued { event, _session, _account } = queued;
                    if let Event::Message { sender, message_id, .. } = &event {
                        let key = (
                            crate::identifier::decode(&sender.account_id)?,
                            crate::identifier::decode(&sender.instance_id)?,
                            crate::identifier::decode(message_id)?,
                        );
                        let mut held =
                            endpoint.delivered.lock().expect("channel delivery credits poisoned");
                        endpoint.check()?;
                        if held.contains_key(&key) {
                            continue;
                        }
                        if held.len() >= 256 {
                            return Err("channel delivery capacity exceeded".into());
                        }
                        held.insert(key, (_session, _account));
                    }
                    return Ok(R::ChannelEvents(vec![event]));
                }
            }
            O::ChannelCommand { name, action, data, .. } => {
                if action == 1 {
                    endpoint.check()?;
                    let future = endpoint
                        .channels
                        .lock()
                        .await
                        .as_ref()
                        .ok_or("channel instance retired")?
                        .publication(name, data)?;
                    future.await?;
                    return Ok(R::ChannelCompleted);
                }
                endpoint.check()?;
                let mut guard = endpoint.channels.lock().await;
                let channels = guard.as_mut().ok_or("channel instance retired")?;
                match action {
                    2 => {
                        channels.subscribe(&name).await?;
                        endpoint.check()?;
                        endpoint.desired.lock().await.insert(name.clone());
                    }
                    _ => return Err("invalid channel action".into()),
                };
                Ok(R::ChannelCompleted)
            }
            _ => Err("not a channel request".into()),
        }
    }
    /// Invalidate synchronously; cleanup holds the old endpoint Arc so it
    /// cannot remove or publish into a replacement generation's endpoint.
    pub fn retire(
        &self,
        instance: [u8; 16],
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let entry = self.entries.lock().expect("channel registry poisoned").remove(&instance);
        if let Some(entry) = &entry {
            entry.retired.store(true, Ordering::Release);
            entry.delivered.lock().expect("channel delivery credits poisoned").clear();
        }
        async move {
            if let Some(entry) = entry {
                entry.channels.lock().await.take();
            }
        }
    }
    pub fn consumed(
        &self,
        recipient: [u8; 16],
        account: [u8; 16],
        sender: [u8; 16],
        message: [u8; 16],
    ) {
        if let Some(endpoint) =
            self.entries.lock().expect("channel registry poisoned").get(&recipient)
        {
            endpoint
                .delivered
                .lock()
                .expect("channel delivery credits poisoned")
                .remove(&(account, sender, message));
        }
    }
}

#[cfg(test)]
mod crypto_tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receiver_failure_is_terminal_before_the_task_finishes() {
        let stopped = Arc::new(AtomicBool::new(false));
        let terminal = stopped.clone();
        let updates = Arc::new(AtomicU8::new(0));
        let reported = updates.clone();
        let (failure, mut notification) = watch::channel(None);
        let (release, held) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            report_receiver_failure(&terminal, &reported, &failure, "stream closed".into());
            let _ = held.await;
        });
        let receiver = Receiver { task, stopped };
        tokio::time::timeout(std::time::Duration::from_secs(1), notification.changed())
            .await
            .expect("failure wakes recovery")
            .expect("failure sender remains alive");
        assert!(receiver.is_stopped(), "both own and named recovery see the terminal state");
        assert!(!receiver.task.is_finished(), "the failing worker is deliberately preempted");
        assert_eq!(updates.load(Ordering::Acquire), 2);
        assert_eq!(*notification.borrow(), Some(playbit_api::abi::syscall::Error::Unavailable));
        release.send(()).expect("release the failing task");
        receiver.task.await.expect("receiver exits");
    }
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    #[tokio::test]
    async fn retirement_releases_old_delivery_credits_before_replacement_cleanup() {
        let hub = Hub::default();
        let old = Arc::new(InstanceEndpoint::default());
        let messages = Arc::new(Semaphore::new(1));
        let bytes = Arc::new(Semaphore::new(128));
        old.delivered.lock().expect("delivery credits").insert(
            ([2; 16], [3; 16], [4; 16]),
            (
                messages.clone().try_acquire_owned().expect("message credit"),
                bytes.clone().try_acquire_many_owned(128).expect("byte credits"),
            ),
        );
        hub.entries.lock().expect("channel registry").insert([1; 16], old.clone());
        assert_eq!(messages.available_permits(), 0);
        assert_eq!(bytes.available_permits(), 0);
        let cleanup = hub.retire([1; 16]);
        assert_eq!(messages.available_permits(), 1);
        assert_eq!(bytes.available_permits(), 128);
        assert!(old.check().is_err());
        let replacement = Arc::new(InstanceEndpoint::default());
        hub.entries.lock().expect("channel registry").insert([1; 16], replacement.clone());
        cleanup.await;
        assert!(replacement.check().is_ok());
        assert!(Arc::ptr_eq(
            hub.entries
                .lock()
                .expect("channel registry")
                .get(&[1; 16])
                .expect("replacement endpoint"),
            &replacement
        ));
    }
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    #[tokio::test]
    async fn disconnected_unsubscribe_and_close_remove_reconnect_intent() {
        use crate::{
            engine::blob::{BlobTransfers, BlobTransport},
            runtime::host::{DataOperation, DataResult},
            store::BuildSelector,
        };
        let directory = tempfile::tempdir().expect("profile");
        let store = Arc::new(LocalStore::open(directory.path()).expect("store"));
        let root = store.create_volume("Root", false).expect("root");
        let build = crate::identifier::encode(&[7; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .expect("session");
        let transfers = Arc::new(BlobTransfers::default());
        let endpoint = Arc::new(InstanceEndpoint::default());
        endpoint.desired.lock().await.extend(["unsubscribe".into(), "close".into()]);
        transfers.channels.entries.lock().expect("registry").insert([1; 16], endpoint.clone());
        let transport =
            BlobTransport::new_local(tokio::runtime::Handle::current(), transfers.clone());
        for (action, name) in [(3, "unsubscribe"), (4, "close")] {
            let result = transport
                .channel_call(
                    store.clone(),
                    session.id.clone(),
                    [7; 32],
                    DataOperation::ChannelCommand {
                        instance: [1; 16],
                        name: name.into(),
                        action,
                        data: vec![],
                    },
                )
                .expect("accepted")
                .await
                .expect("local cleanup");
            assert!(matches!(result, DataResult::ChannelCompleted));
        }
        assert!(endpoint.desired.lock().await.is_empty());
        assert!(endpoint.check().is_ok(), "named cleanup preserves own-channel membership");
        assert!(transfers.channels.entries.lock().expect("registry").contains_key(&[1; 16]));
    }
    #[test]
    fn encryption_authenticates_the_exact_channel_and_every_sender_field() {
        let destination = channel::destination(&[1; 32], "é/\0").expect("channel");
        let source: [u8; 80] =
            [[2; 16].as_slice(), &[3; 16], &[4; 16], &[5; 32]].concat().try_into().expect("source");
        let aad = channel::associated_data(&destination, &source, &[6; 16]).expect("context");
        let encrypted = seal([7; 32], &aad, b"private message").expect("seal");
        assert_eq!(open([7; 32], &aad, encrypted.clone()).expect("open"), b"private message");
        assert!(open([8; 32], &aad, encrypted.clone()).is_err());
        for position in 0..aad.len() {
            let mut changed = aad.clone();
            changed[position] ^= 1;
            assert!(open([7; 32], &changed, encrypted.clone()).is_err());
        }
        for position in 0..encrypted.len() {
            let mut changed = encrypted.clone();
            changed[position] ^= 1;
            assert!(open([7; 32], &aad, changed).is_err());
        }
        assert!(open([7; 32], &aad, vec![0; 27]).is_err());
        assert!(open([7; 32], &aad, vec![0; channel::MAX_MESSAGE_BYTES + 29]).is_err());
    }
}
