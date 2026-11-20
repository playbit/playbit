//! One durable account outbox cursor on the existing publisher actor.
// W7 coverage decision: account jobs use the same actor completion owner and
// retained apply/feed credit as volume jobs. Cancellation and lost acknowledgment
// are exercised at those boundaries; every RNG/store/transport variant here is
// not separately injected. Do not synthesize a successful durable receipt.
use super::*;

pub(super) enum Command {
    Begin { reply: Reply<u64> },
    Finish { generation: u64, connection: Arc<ApConnection>, reply: Reply<()> },
    ReadyVolume { volume: Id, reply: Reply<()> },
    Fence { reply: Reply<()> },
    Fail { generation: u64, error: PublisherError, reply: Reply<()> },
}

#[derive(Clone, Debug)]
pub(crate) struct Notice {
    pub(crate) generation: u64,
    pub(crate) state: PublishState,
    pub(crate) error: Option<PublisherError>,
}

pub(super) struct Account {
    generation: u64,
    connection: Option<Arc<ApConnection>>,
    pub(super) active: Option<Active>,
    begin: Option<Reply<u64>>,
    fence: Option<Reply<()>>,
    state: PublishState,
    pending: bool,
    restart: bool,
    blocked: bool,
    cursor: Option<[u8; 32]>,
    error: Option<PublisherError>,
}
impl Account {
    fn new() -> Self {
        Self {
            generation: 0,
            connection: None,
            active: None,
            begin: None,
            fence: None,
            state: PublishState::Bootstrapping,
            pending: false,
            restart: false,
            blocked: false,
            cursor: None,
            error: None,
        }
    }
    fn notice(&self) -> Notice {
        Notice {
            generation: self.generation,
            state: if self.state == PublishState::Ready
                && (self.pending || self.active.is_some() || self.blocked)
            {
                PublishState::Pending
            } else {
                self.state
            },
            error: self.error.clone(),
        }
    }
    pub(super) fn fail(&mut self, state: PublishState, error: PublisherError) {
        #[cfg(feature = "test-support")]
        if state == PublishState::Failed {
            crate::diagnostic!(
                "account publication generation {} failed: {error}",
                self.generation
            );
        }
        self.state = state;
        self.connection = None;
        self.error = Some(error.clone());
        if let Some(cancel) = self.active.as_mut().and_then(|job| job.cancel.take()) {
            let _ = cancel.send(());
        }
        if let Some(reply) = self.begin.take() {
            let _ = reply.send(Err(error.clone()));
        }
        if let Some(reply) = self.fence.take() {
            let _ = reply.send(Err(error));
        }
    }
}

pub(super) enum Progress {
    End,
    Advanced { key: [u8; 32], blocked: bool },
}

impl Publisher {
    #[cfg(test)]
    pub(crate) fn test_account_state(&self, generation: u64, state: PublishState) {
        self.signals.notices.lock().expect("mutex is not poisoned").account =
            Some(Notice { generation, state, error: None });
    }
    pub(crate) async fn begin_account(&self) -> Result<u64> {
        let (reply, result) = self.reply()?;
        self.send(super::Command::Account(Command::Begin { reply }))?;
        result.await.map_err(|_| "account publisher closed during bootstrap")?.map_err(Into::into)
    }
    pub(crate) async fn finish_account(
        &self,
        generation: u64,
        connection: Arc<ApConnection>,
    ) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(super::Command::Account(Command::Finish { generation, connection, reply }))?;
        result.await.map_err(|_| "account publisher closed during activation")?.map_err(Into::into)
    }
    pub(crate) async fn fail_account(&self, generation: u64, error: &str) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(super::Command::Account(Command::Fail {
            generation,
            error: PublisherError::new(error),
            reply,
        }))?;
        result.await.map_err(|_| "account publisher closed during retirement")?.map_err(Into::into)
    }
    pub(crate) async fn account_volume_ready(&self, volume: Id) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(super::Command::Account(Command::ReadyVolume { volume, reply }))?;
        result
            .await
            .map_err(|_| "publisher closed during capability admission")?
            .map_err(Into::into)
    }
    pub(crate) async fn fence_account(&self) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(super::Command::Account(Command::Fence { reply }))?;
        result.await.map_err(|_| "publisher closed during account fence")?.map_err(Into::into)
    }
    pub(crate) fn account_state(&self) -> Option<Notice> {
        self.signals.notices.lock().expect("mutex is not poisoned").account.clone()
    }
    pub(crate) fn account_ready_volumes(&self) -> Vec<Id> {
        self.signals
            .notices
            .lock()
            .expect("mutex is not poisoned")
            .volumes
            .iter()
            .filter(|slot| slot.notice.account_ready)
            .map(|slot| slot.notice.volume)
            .collect()
    }
}

impl Actor {
    fn note_account(&self) {
        self.signals.notices.lock().expect("mutex is not poisoned").account =
            self.account.as_ref().map(Account::notice);
        self.signals.wake();
    }
    pub(super) fn account_wake(&mut self) {
        if let Some(account) = &mut self.account {
            if account.pending || account.active.is_some() {
                account.restart = true;
            } else {
                account.pending = true;
                account.blocked = false;
            }
        }
    }
    pub(super) fn account_command(&mut self, command: Command) {
        match command {
            Command::Begin { reply } => {
                if reply.is_closed() {
                    return;
                }
                if self.account.is_none() && self.volumes.len() == VOLUME_LIMIT {
                    let _ = reply.send(Err(PublisherError::new(
                        "account publication source capacity exceeded",
                    )));
                    return;
                }
                let Some(generation) = self.generation.checked_add(1) else {
                    let _ =
                        reply.send(Err(PublisherError::new("publication generation exhausted")));
                    return;
                };
                let account = self.account.get_or_insert_with(Account::new);
                if account.begin.is_some() {
                    let _ =
                        reply.send(Err(PublisherError::new("account bootstrap already preparing")));
                    return;
                }
                account.fail(
                    PublishState::Bootstrapping,
                    PublisherError::new("account bootstrap superseded publication"),
                );
                self.generation = generation;
                account.generation = generation;
                account.error = None;
                account.begin = Some(reply);
                account.cursor = None;
                account.pending = false;
                account.restart = false;
                account.blocked = false;
            }
            Command::Finish { generation, connection, reply } => {
                let result = match &mut self.account {
                    Some(account)
                        if account.generation == generation
                            && account.begin.is_none()
                            && account.active.is_none()
                            && account.state == PublishState::Bootstrapping =>
                    {
                        account.connection = Some(connection);
                        account.state = PublishState::Ready;
                        account.pending = true;
                        Ok(())
                    }
                    _ => Err(PublisherError::new("account bootstrap generation was replaced")),
                };
                let _ = reply.send(result);
            }
            Command::Fail { generation, error, reply } => {
                if let Some(account) = &mut self.account
                    && account.generation == generation
                {
                    account.fail(PublishState::Failed, error);
                }
                let _ = reply.send(Ok(()));
            }
            Command::ReadyVolume { volume, reply } => {
                match self.store.replication_supported(volume) {
                    Ok(false) => {
                        let _ = reply.send(Ok(()));
                        return;
                    }
                    Ok(true) => {}
                    Err(error) => {
                        let _ = reply.send(Err(PublisherError::new(error)));
                        return;
                    }
                }
                let result = self.ensure_volume(volume.into()).map(|state| {
                    let changed = !state.account_ready;
                    state.account_ready = true;
                    changed
                });
                match result {
                    Ok(changed) => {
                        self.signals.volume(self.volumes[&volume.into()].notice(volume));
                        if changed {
                            self.account_wake();
                        }
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(*error));
                    }
                }
            }
            Command::Fence { reply } => {
                match &mut self.account {
                    Some(account)
                        if account.state == PublishState::Ready && account.fence.is_none() =>
                    {
                        account.fence = Some(reply);
                        // Read the durable outbox even if its readiness byte has
                        // not reached this actor yet. No network freshness pull.
                        self.account_wake();
                    }
                    _ => {
                        let _ = reply.send(Err(PublisherError::new(
                            "account publication is unavailable or fenced",
                        )));
                    }
                }
            }
        }
        self.note_account();
    }
    pub(super) fn settle_account(&mut self) {
        let Some(account) = &mut self.account else {
            return;
        };
        if account.active.is_some() {
            return;
        }
        if let Some(reply) = account.begin.take()
            && reply.send(Ok(account.generation)).is_err()
        {
            account.fail(
                PublishState::Failed,
                PublisherError::new("account bootstrap caller canceled"),
            );
            self.note_account();
            return;
        }
        if account.state == PublishState::Ready
            && !account.pending
            && let Some(reply) = account.fence.take()
        {
            let result = if account.blocked {
                Err(PublisherError::new("account capability initialization is incomplete"))
            } else {
                Ok(())
            };
            let _ = reply.send(result);
        }
    }
    pub(super) fn start_account_job(&mut self) {
        if self.stopping || self.jobs.len() == JOB_LIMIT {
            return;
        }
        let Some(account) = &mut self.account else {
            return;
        };
        if account.state != PublishState::Ready || !account.pending || account.active.is_some() {
            return;
        }
        let Some(connection) = account.connection.clone() else {
            return;
        };
        let ready = self
            .volumes
            .iter()
            .filter(|(_, state)| state.account_ready)
            .map(|(id, _)| id.volume)
            .collect();
        let (cancel, cancelled) = oneshot::channel();
        let task = self.jobs.spawn(publish(
            self.store.clone(),
            self.apply_slots.clone(),
            connection,
            ready,
            account.cursor,
            account.generation,
            cancelled,
        ));
        account.active = Some(Active { id: task.id(), cancel: Some(cancel) });
        self.note_account();
    }
    pub(super) fn completed_account(
        &mut self,
        task: TaskId,
        generation: u64,
        result: std::result::Result<Progress, JobError>,
    ) {
        let Some(account) = &mut self.account else {
            return;
        };
        if account.active.as_ref().is_none_or(|job| job.id != task) {
            return;
        }
        account.active = None;
        if account.generation == generation && account.state == PublishState::Ready {
            match result {
                Ok(Progress::End) => {
                    account.cursor = None;
                    account.pending = std::mem::take(&mut account.restart);
                    if account.pending {
                        account.blocked = false;
                    }
                }
                Ok(Progress::Advanced { key, blocked }) => {
                    account.cursor = Some(key);
                    account.blocked |= blocked;
                }
                Err(error) => {
                    let (phase, error) = error.phase();
                    account.fail(
                        if matches!(phase, Some(Phase::Disconnected)) {
                            PublishState::Disconnected
                        } else {
                            PublishState::Failed
                        },
                        error,
                    );
                }
            }
        }
        self.note_account();
    }
    pub(super) fn fail_account(&mut self, state: PublishState, error: PublisherError) {
        if let Some(account) = &mut self.account {
            account.fail(state, error);
        }
        self.note_account();
    }
}

async fn publish(
    store: Arc<LocalStore>,
    slots: Arc<Semaphore>,
    connection: Arc<ApConnection>,
    ready: Vec<Id>,
    after: Option<[u8; 32]>,
    generation: u64,
    mut canceled: oneshot::Receiver<()>,
) -> JobResult {
    let result = async {
        let permit = cancellable(slots.clone().acquire_owned(), &mut canceled)
            .await?
            .map_err(PublisherError::new)?;
        let source = store.clone();
        let staged = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let Some(record) = source.pending_account_record(after)? else {
                return Ok(None);
            };
            let dependencies = source.account_record_dependencies(&record)?;
            if dependencies.into_iter().flatten().any(|volume| !ready.contains(&volume)) {
                return Ok(Some((record, None, true)));
            }
            let delta = source.stage_account_record(&record)?;
            if delta.is_none() {
                source.adopt_account_record(&record.key)?;
            }
            Ok::<_, crate::store::Error>(Some((record, delta, false)))
        })
        .await
        .map_err(PublisherError::new)?
        .map_err(PublisherError::new)?;
        let Some((record, delta, blocked)) = staged else {
            return Ok(Progress::End);
        };
        let key = record.key;
        if let Some(delta) = delta {
            store.check_account_authority().map_err(PublisherError::new)?;
            let request = ServiceRequest::PublishAccount {
                operation_id: Id::generate().map_err(PublisherError::new)?.to_string(),
            };
            cancellable(
                service_client::publish_replica(&connection, &request, &delta),
                &mut canceled,
            )
            .await?
            .map_err(|error| ap_failure(error.as_ref()))?;
            drop(delta);
            let permit = cancellable(slots.acquire_owned(), &mut canceled)
                .await?
                .map_err(PublisherError::new)?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                store.acknowledge_account_record(&record)
            })
            .await
            .map_err(PublisherError::new)?
            .map_err(PublisherError::new)?;
        }
        Ok(Progress::Advanced { key, blocked })
    }
    .await;
    JobResult::Account { generation, result }
}
