//! Bloblog transfer shares the account connection and volume subscription.
use super::completion::Wake;
use crate::engine::Result;
use crate::network::ap::{ApConnection, operation::ServiceRequest, service_client as wire};
use crate::store::{
    Access, EntryId, Id, LocalStore, Session,
    bloblog::{self, ReplicaPage},
};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub(crate) struct Received {
    pub data_changed: bool,
    pub receipt_changed: bool,
}

#[cfg(feature = "app-host")]
pub(crate) fn access(store: &LocalStore, volume: Id) -> Result<Access> {
    if store.volume(volume)?.owner == store.account().id {
        return Ok(Access::owner(store.account().id));
    }
    let session = store
        .sessions()?
        .into_iter()
        .find(|s| {
            (s.replica_volume() == volume) && store.session_denial(&s.id).is_ok_and(|d| d.is_none())
        })
        .ok_or("bloblog volume requires an authorized joined session")?;
    Ok(Access::session_version(store.account().id, session.id, session.resolved_build_id))
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) fn publish_request(
    volume: Id,
    log: EntryId,
    append_id: Id,
    session: Option<&Session>,
) -> Result<ServiceRequest> {
    Ok(ServiceRequest::PublishBloblog {
        append_id,
        log_id: log,
        operation_id: Id::generate()?.to_string(),
        volume_id: volume.to_string(),
        session_id: session.map(|s| s.id.clone()),
        build_id: session.map(|s| s.resolved_build_id.clone()),
    })
}

pub(crate) async fn catch_up_log(
    connection: &ApConnection,
    store: Arc<LocalStore>,
    volume: Id,
    log: EntryId,
    session: Option<&Session>,
    slots: Arc<Semaphore>,
    completion: Arc<Wake>,
) -> Result<bool> {
    let access = session.map_or_else(
        || Access::owner(store.account().id),
        |s| Access::session_version(store.account().id, &s.id, &s.resolved_build_id),
    );
    let permit = slots.clone().acquire_owned().await?;
    let initial_store = store.clone();
    let initial_access = access.clone();

    let mut cursor = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        remote_session(&initial_store, &initial_access, volume)?;
        initial_store.bloblog_checkpoint(&initial_access, volume, log)
    })
    .await??;
    let mut changed = false;
    loop {
        let request = ServiceRequest::GetBloblog {
            operation_id: Id::generate()?.to_string(),
            volume_id: volume.to_string(),
            session_id: session.map(|s| s.id.clone()),
            build_id: session.map(|s| s.resolved_build_id.clone()),
            log_id: log,
            cursor: cursor.as_ref().map(ToString::to_string),
        };
        let response = wire::request(connection, &request).await?;
        let receipt: ReplicaPage =
            serde_json::from_value(response.data.ok_or("missing bloblog page")?)?;
        receipt.cut()?;
        let page = receipt.page;
        bloblog::encode(&page.entries)?;
        if page.more && (page.entries.is_empty() || cursor.as_ref() == Some(&page.cursor)) {
            return Err("bloblog catch-up did not advance".into());
        }
        let more = page.more;
        cursor = Some(page.cursor.clone());
        let permit = slots.clone().acquire_owned().await?;
        let store = store.clone();
        let access = access.clone();
        let completion = completion.clone();

        changed |= tokio::task::spawn_blocking(move || {
            let _permit = permit;
            receive_page(&store, &access, volume, log, &page, &completion)
        })
        .await??;
        if !more {
            return Ok(changed);
        }
    }
}

/// The blocking owner must wake after commit even if its async waiter was canceled.
pub(crate) fn receive_entries(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    log: EntryId,
    entries: &[bloblog::Entry],
    completion: &Wake,
) -> crate::store::Result<bool> {
    let result = store.bloblog_receive(access, volume, log, entries);
    // An error can follow a committed backing update but failed readiness bookkeeping.
    if !matches!(result, Ok(false)) {
        completion.notify();
    }
    result
}

fn receive_page(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    log: EntryId,
    page: &bloblog::Page,
    completion: &Wake,
) -> crate::store::Result<bool> {
    let data_changed = receive_entries(store, access, volume, log, &page.entries, completion)?;
    if store.bloblog_set_checkpoint_changed(access, volume, log, &page.cursor)? {
        // The admitted blocking owner outlives a canceled async waiter. Wake
        // after commit, before any later network page or aggregate can wait.
        completion.notify();
    }
    Ok(data_changed)
}

/// Visit bounded index pages; each append log owns its checkpoint and backing.
/// Called only at initialization, reconnect or an explicit stream gap.
pub(crate) async fn catch_up(
    connection: &ApConnection,
    store: Arc<LocalStore>,
    volume: Id,
    session: Option<&Session>,
    slots: Arc<Semaphore>,
    completion: Arc<Wake>,
) -> Result<bool> {
    let access = session.map_or_else(
        || Access::owner(store.account().id),
        |s| Access::session_version(store.account().id, &s.id, &s.resolved_build_id),
    );
    let mut after = None;
    let mut changed = false;
    loop {
        let worker_store = store.clone();
        let worker_access = access.clone();
        let permit = slots.clone().acquire_owned().await?;
        let page = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut page = worker_store.entries(&worker_access, volume, after, 64)?;
            let mut permitted = Vec::with_capacity(page.entries.len());
            for entry in page.entries {
                if entry.deleted
                    || entry.protocol != Some(crate::store::volume_index::DatabaseProtocol::BlobLog)
                {
                    continue;
                }
                match worker_store.entry(&worker_access, volume, entry.id) {
                    Ok(_) => permitted.push(entry),
                    Err(crate::store::Error::AccessDenied(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            page.entries = permitted;
            Ok::<_, crate::store::Error>(page)
        })
        .await??;
        for entry in page.entries {
            if !entry.deleted
                && entry.protocol == Some(crate::store::volume_index::DatabaseProtocol::BlobLog)
            {
                changed |= catch_up_log(
                    connection,
                    store.clone(),
                    volume,
                    entry.id,
                    session,
                    slots.clone(),
                    completion.clone(),
                )
                .await?;
            }
        }
        let Some(next) = page.next else {
            return Ok(changed);
        };
        after = Some(next);
    }
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) struct CallBoundary {
    pub connection: Option<Arc<ApConnection>>,
    pub deadline: tokio::time::Instant,
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
type LogLocks = std::collections::BTreeMap<(Id, [u8; 15]), std::sync::Weak<tokio::sync::Mutex<()>>>;

#[derive(Default)]
pub(crate) struct Calls {
    pub(crate) publisher: std::sync::Mutex<std::sync::Weak<super::publisher::Publisher>>,
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    active: std::sync::atomic::AtomicUsize,
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    logs: std::sync::Mutex<LogLocks>,
    preparations: std::sync::Mutex<std::collections::VecDeque<Preparation>>,
    prepared: std::sync::Mutex<Vec<Prepared>>,
}

pub(crate) enum Limit {
    Calls = 8,
    Prepared = 64,
}

struct Prepared {
    connection: std::sync::Weak<ApConnection>,
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    volume: Id,
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    session: Option<(String, String)>,
}

pub(crate) struct Preparation {
    pub connection: Arc<ApConnection>,
    pub volume: Id,
    pub session: Option<Session>,
    reply: tokio::sync::oneshot::Sender<crate::runtime::host::HostResult<()>>,
}

impl Preparation {
    pub(crate) fn canceled(&self) -> bool {
        self.reply.is_closed()
    }

    pub(crate) fn finish(self, calls: &Calls, result: Result<()>) {
        use playbit_api::abi::syscall::Error;
        let result = result
            .map_err(|error| {
                error
                    .downcast_ref::<crate::store::Error>()
                    .map_or(Error::Unavailable, crate::runtime::store_bridge::error_ref)
            })
            .and_then(|()| {
                let mut prepared = calls.prepared.lock().map_err(|_| Error::Internal)?;
                prepared.retain(|item| {
                    item.connection.upgrade().is_some_and(|connection| !connection.is_closed())
                });
                if prepared.len() == Limit::Prepared as usize {
                    prepared.remove(0);
                }
                prepared.push(Prepared {
                    connection: Arc::downgrade(&self.connection),
                    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
                    volume: self.volume,
                    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
                    session: self.session.map(|session| (session.id, session.resolved_build_id)),
                });
                Ok(())
            });
        let _ = self.reply.send(result);
    }
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) struct Credit {
    calls: Arc<Calls>,
    order: Option<Arc<tokio::sync::Mutex<()>>>,
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) struct Lease {
    credit: Credit,
    _order: Option<tokio::sync::OwnedMutexGuard<()>>,
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
impl Drop for Credit {
    fn drop(&mut self) {
        self.calls.active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
impl Credit {
    pub(crate) async fn acquire(self) -> Arc<Lease> {
        let order = match &self.order {
            Some(order) => Some(order.clone().lock_owned().await),
            None => None,
        };
        Arc::new(Lease { credit: self, _order: order })
    }
}

impl Calls {
    pub(crate) fn needs_preparation(&self) -> bool {
        self.preparations.lock().is_ok_and(|requests| !requests.is_empty())
    }

    pub(crate) fn take_preparation(&self) -> Result<Option<Preparation>> {
        Ok(self.preparations.lock().map_err(|_| "bloblog preparation lock poisoned")?.pop_front())
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    async fn prepare(
        &self,
        connection: &Arc<ApConnection>,
        volume: Id,
        session: Option<&Session>,
        completion: &super::completion::Wake,
    ) -> crate::runtime::host::HostResult<()> {
        use playbit_api::abi::syscall::Error;
        {
            let prepared = self.prepared.lock().map_err(|_| Error::Internal)?;
            if prepared.iter().any(|item| {
                item.volume == volume
                    && item
                        .connection
                        .upgrade()
                        .is_some_and(|previous| Arc::ptr_eq(&previous, connection))
                    && item.session.as_ref().map(|(id, build)| (id.as_str(), build.as_str()))
                        == session.map(|session| {
                            (session.id.as_str(), session.resolved_build_id.as_str())
                        })
            }) {
                return Ok(());
            }
        }
        let (reply, receive) = tokio::sync::oneshot::channel();
        {
            let mut requests = self.preparations.lock().map_err(|_| Error::Internal)?;
            requests.retain(|request| !request.canceled());
            if requests.len() == Limit::Calls as usize {
                return Err(Error::LimitExceeded);
            }
            requests.push_back(Preparation {
                connection: connection.clone(),
                volume,
                session: session.cloned(),
                reply,
            });
        }
        completion.notify();
        tokio::time::timeout(std::time::Duration::from_secs(30), receive)
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(|_| Error::Unavailable)?
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    pub(crate) fn admit(
        self: &Arc<Self>,
        call: &crate::runtime::host::DataCall,
    ) -> crate::runtime::host::HostResult<Credit> {
        use crate::runtime::host::DataOperation;
        use playbit_api::abi::syscall::Error;
        let (DataOperation::BloblogAppend { log, .. } | DataOperation::BloblogRead { log, .. }) =
            &call.operation
        else {
            return Err(Error::Unsupported);
        };
        let mut logs = self.logs.lock().map_err(|_| Error::Internal)?;
        self.active
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count < Limit::Calls as usize).then_some(count + 1),
            )
            .map_err(|_| Error::LimitExceeded)?;
        logs.retain(|_, log| log.strong_count() != 0);
        let order = if matches!(call.operation, DataOperation::BloblogAppend { .. }) {
            let order = logs.entry((Id::from_bytes(call.volume), *log)).or_default();
            Some(order.upgrade().unwrap_or_else(|| {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                *order = Arc::downgrade(&lock);
                lock
            }))
        } else {
            None
        };
        Ok(Credit { calls: self.clone(), order })
    }
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) async fn execute(
    connection: Option<Arc<ApConnection>>,
    store: Arc<LocalStore>,
    access: Access,
    call: crate::runtime::host::DataCall,
    lease: Arc<Lease>,
    completion: Option<Arc<super::completion::Wake>>,
) -> crate::runtime::host::HostResult<crate::runtime::host::DataResult> {
    use crate::runtime::host::{DataOperation, DataResult};
    use playbit_api::abi::syscall::Error;
    let volume = Id::from_bytes(call.volume);
    match call.operation {
        DataOperation::BloblogAppend { log, append_id, blobs } => {
            let completion = completion.ok_or(Error::Unavailable)?;
            let log = EntryId::from_bytes(log).map_err(crate::runtime::store_bridge::error)?;
            let append_id = Id::from_bytes(append_id);
            let prepared = store.clone();
            let retained = lease.clone();
            let authority = access.clone();
            let (cursor, entries, session) = tokio::task::spawn_blocking(move || {
                let _lease = retained;
                let cursor = prepared.bloblog_prepare(&authority, volume, log, &blobs)?;
                let session = remote_session(&prepared, &authority, volume)?;
                let entries =
                    bloblog::append_entries(prepared.account().id, volume, log, append_id, &blobs)?;
                Ok::<_, crate::store::Error>((cursor, entries, session))
            })
            .await
            .map_err(|_| Error::Internal)?
            .map_err(crate::runtime::store_bridge::error)?;
            let connection = connection.ok_or(Error::Unavailable)?;
            lease.credit.calls.prepare(&connection, volume, session.as_ref(), &completion).await?;
            let publisher = lease
                .credit
                .calls
                .publisher
                .lock()
                .map_err(|_| Error::Internal)?
                .upgrade()
                .ok_or(Error::Unavailable)?;
            let mut volumes = vec![volume];
            if let Some(session) = &session
                && session.replica_volume() != volume
            {
                volumes.push(session.replica_volume());
            }
            tokio::time::timeout(std::time::Duration::from_secs(30), publisher.fence(&volumes))
                .await
                .map_err(|_| Error::Unavailable)?
                .map_err(|_| Error::Unavailable)?;
            if connection.is_closed() {
                return Err(Error::Unavailable);
            }
            let request = publish_request(volume, log, append_id, session.as_ref())
                .map_err(|_| Error::Internal)?;
            let bytes = bloblog::encode(&entries).map_err(crate::runtime::store_bridge::error)?;
            wire::publish_bloblog(&connection, &request, &bytes).await.map_err(append_error)?;
            // Confirmed remote success survives failure to populate this disposable cache.
            let _cache = tokio::task::spawn_blocking(move || {
                let _lease = lease;
                receive_entries(&store, &access, volume, log, &entries, &completion)
            });
            Ok(DataResult::BloblogAppended(cursor))
        }
        DataOperation::BloblogRead { log, cursor } => {
            let log = EntryId::from_bytes(log).map_err(crate::runtime::store_bridge::error)?;
            read(
                connection,
                store,
                access,
                crate::store::volume_index::EntryAddress { volume, entry: log },
                cursor,
                lease,
                completion,
            )
            .await
            .map(DataResult::BloblogPage)
        }
        _ => Err(Error::Unsupported),
    }
}

fn remote_session(
    store: &LocalStore,
    access: &Access,
    volume: Id,
) -> crate::store::Result<Option<Session>> {
    store.require_replication_supported(volume)?;
    if let Some(id) = access.session_id()
        && store.is_local_session(id)?
        && store.session(id)?.shared_volume.is_none()
    {
        return Err(crate::store::Error::Unsupported("this app session has no AP-enabled root"));
    }

    let mut session = access.session_id().map(|id| store.session(id)).transpose()?;
    if let Some(session) = &session
        && session.shared_volume.is_some()
        && session.root_volume == volume
    {
        // World remains separate from the invitation's Shared capability. The
        // guest's exact local authority was checked before selecting transport.
        if store.volume(volume)?.owner != store.account().id {
            return Err(crate::store::Error::AccessDenied("World transport requires its owner"));
        }
        return Ok(None);
    }
    if let Some(session) = &mut session
        && let Some(build) = access.build_id()
    {
        session.resolved_build_id = build.to_owned();
    }
    Ok(session)
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
fn append_error(
    error: Box<dyn std::error::Error + Send + Sync>,
) -> playbit_api::abi::syscall::Error {
    use playbit_api::abi::syscall::Error;
    if error.is::<wire::PublicationNotSent>() {
        return Error::Unavailable;
    }
    if error
        .downcast_ref::<crate::network::ap::service::ServiceFailure>()
        .is_some_and(|failure| failure.denial().is_some())
    {
        return Error::AccessDenied;
    }
    match error
        .downcast_ref::<crate::network::ap::service::ServiceFailure>()
        .map(|failure| failure.code())
    {
        Some("operation_conflict") => Error::Conflict,
        Some("operation_uncertain") | None => Error::Uncertain,
        Some(_) => Error::Unavailable,
    }
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
async fn read(
    connection: Option<Arc<ApConnection>>,
    store: Arc<LocalStore>,
    access: Access,
    address: crate::store::volume_index::EntryAddress,
    cursor: Option<bloblog::Cursor>,
    lease: Arc<Lease>,
    completion: Option<Arc<super::completion::Wake>>,
) -> crate::runtime::host::HostResult<bloblog::Page> {
    use playbit_api::abi::syscall::Error;
    let crate::store::volume_index::EntryAddress { volume, entry: log } = address;
    loop {
        let local = store.clone();
        let authority = access.clone();
        let after = cursor.clone();
        let retained = lease.clone();
        let (page, checkpoint, session) = tokio::task::spawn_blocking(move || {
            let _lease = retained;
            let page = local.bloblog_get(&authority, volume, log, after.as_ref())?;
            if !page.entries.is_empty() {
                return Ok((page, None, None));
            }
            let checkpoint = local.bloblog_checkpoint(&authority, volume, log)?;
            Ok::<_, crate::store::Error>((
                page,
                checkpoint,
                remote_session(&local, &authority, volume)?,
            ))
        })
        .await
        .map_err(|_| Error::Internal)?
        .map_err(crate::runtime::store_bridge::error)?;
        if !page.entries.is_empty() {
            return Ok(page);
        }
        let connection = connection.as_ref().ok_or(Error::Unavailable)?;
        lease
            .credit
            .calls
            .prepare(
                connection,
                volume,
                session.as_ref(),
                completion.as_deref().ok_or(Error::Unavailable)?,
            )
            .await?;
        let request = ServiceRequest::GetBloblog {
            operation_id: Id::generate().map_err(crate::runtime::store_bridge::error)?.to_string(),
            volume_id: volume.to_string(),
            log_id: log,
            session_id: session.as_ref().map(|s| s.id.clone()),
            build_id: session.as_ref().map(|s| s.resolved_build_id.clone()),
            cursor: checkpoint.as_ref().map(ToString::to_string),
        };
        let response = wire::request(connection, &request).await.map_err(|_| Error::Unavailable)?;
        let receipt: ReplicaPage = serde_json::from_value(response.data.ok_or(Error::Internal)?)
            .map_err(|_| Error::Internal)?;
        receipt.cut().map_err(crate::runtime::store_bridge::error)?;
        if receipt.page.more
            && (receipt.page.entries.is_empty()
                || checkpoint.as_ref() == Some(&receipt.page.cursor))
        {
            return Err(Error::Internal);
        }
        let more = receipt.page.more;
        let local = store.clone();
        let authority = access.clone();
        let after = cursor.clone();
        let retained = lease.clone();
        let completion = completion.as_ref().ok_or(Error::Unavailable)?.clone();
        let page = tokio::task::spawn_blocking(move || {
            let _lease = retained;
            receive_page(&local, &authority, volume, log, &receipt.page, &completion)?;
            local.bloblog_get(&authority, volume, log, after.as_ref())
        })
        .await
        .map_err(|_| Error::Internal)?
        .map_err(crate::runtime::store_bridge::error)?;
        if !page.entries.is_empty() || !more {
            return Ok(page);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn paged_data_commit_wakes_even_when_checkpoint_bookkeeping_fails() {
        // A wrong-log checkpoint fails after the entries commit. The actual page
        // apply must still notify, while a subsequent identical receipt stays idle.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Paged data", true).unwrap().id;
        let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
        let other = store.create_bloblog(&access, volume, "Other").unwrap().id;
        let entries = bloblog::append_entries(
            store.account().id,
            volume,
            log,
            Id::generate().unwrap(),
            &[b"committed before checkpoint failure".to_vec()],
        )
        .unwrap();
        let cursor = store.bloblog_get(&access, volume, other, None).unwrap().cursor;
        let page = bloblog::Page { entries, cursor, more: false };
        let completion = super::super::completion::Completion::unix().unwrap();
        let before = store.delivery_revision();
        assert!(receive_page(&store, &access, volume, log, &page, &completion.wake).is_err());
        assert!(completion.drain().unwrap());
        assert!(store.delivery_scope_revision(before, &[volume]).1.is_some());
        assert_eq!(store.bloblog_get(&access, volume, log, None).unwrap().entries, page.entries);
        let page = store.bloblog_get(&access, volume, log, None).unwrap();
        assert!(!receive_page(&store, &access, volume, log, &page, &completion.wake).unwrap());
        assert!(completion.drain().unwrap(), "the new checkpoint still wakes its consumer");
        assert!(!receive_page(&store, &access, volume, log, &page, &completion.wake).unwrap());
        assert!(!completion.drain().unwrap(), "identical data and receipt stay idle");
    }

    #[test]
    fn detached_append_cache_owner_wakes_after_its_data_commit() {
        // Gate the same receive boundary used by the detached append cache, drop
        // its async handle, then require a fresh wake after the blocking commit.
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Late append cache", true).unwrap().id;
        let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
        let entries = bloblog::append_entries(
            store.account().id,
            volume,
            log,
            Id::generate().unwrap(),
            &[b"committed append".to_vec()],
        )
        .unwrap();
        let completion = super::super::completion::Completion::unix().unwrap();
        let before = store.delivery_revision();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (started, start) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let (finished, finish) = std::sync::mpsc::sync_channel(1);
        let worker_store = store.clone();
        let wake = completion.wake.clone();
        let deadline = std::time::Duration::from_secs(5);
        let task = runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(deadline).unwrap();
            let result = receive_entries(&worker_store, &access, volume, log, &entries, &wake);
            finished.send(result).unwrap();
        });
        start.recv_timeout(deadline).unwrap();
        drop(task);
        completion.wake.notify();
        assert!(completion.drain().unwrap());
        release.send(()).unwrap();
        assert!(finish.recv_timeout(deadline).unwrap().unwrap());
        assert!(completion.drain().unwrap(), "late data owner must deliver a new wake");
        assert!(store.delivery_scope_revision(before, &[volume]).1.is_some());
    }

    #[cfg(all(target_os = "macos", feature = "process"))]
    #[test]
    fn cancelled_catch_up_waiter_cannot_lose_a_late_checkpoint_wake() {
        // Gate the actual blocking page owner before commit, drop its async
        // waiter, and consume the earlier terminal wake. The surviving apply
        // must then wake Size itself, independently of a network continuation.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Cancelled catch-up", true).unwrap().id;
        let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
        store.bloblog_put(&access, volume, log, &[vec![7; bloblog::INLINE_BYTES + 1]]).unwrap();
        let page = store.bloblog_get(&access, volume, log, None).unwrap();
        let (started, start) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let (finished, finish) = std::sync::mpsc::sync_channel(1);
        let worker_store = store.clone();
        let worker_access = access.clone();
        let worker_page = page.clone();
        let worker_completion = engine.completion.wake.clone();
        let deadline = std::time::Duration::from_secs(5);
        let task = engine.network.runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(deadline).unwrap();
            let result = receive_page(
                &worker_store,
                &worker_access,
                volume,
                log,
                &worker_page,
                &worker_completion,
            );
            finished.send(result).unwrap();
        });
        start.recv_timeout(deadline).unwrap();
        drop(task);
        engine.completion.wake.notify();
        engine.complete_background().unwrap();
        let before = engine.change_token();
        let receipt = store.delivery_provider_revision(None, None).0;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let bytes = store.payload_size(&access, volume, Some(log), &cancelled).unwrap();
        release.send(()).unwrap();
        assert!(!finish.recv_timeout(deadline).unwrap().unwrap());
        assert!(store.payload_size(&access, volume, Some(log), &cancelled).unwrap() > bytes);
        assert_ne!(store.delivery_provider_revision(None, None).0, receipt);
        engine.complete_background().unwrap();
        assert_ne!(engine.change_token(), before, "blocking receipt owner must wake after commit");
        assert!(engine.network.received.drain().is_empty());
        assert!(
            !receive_page(&store, &access, volume, log, &page, &engine.completion.wake).unwrap()
        );
        assert!(!engine.completion.drain().unwrap(), "identical page must not wake again");
    }

    #[test]
    fn catch_up_page_distinguishes_checkpoint_receipts_from_guest_data() {
        // A large committed echo reaches the paged hint path. Exercise
        // its actual blocking apply stage, preserving the separate body result.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Paged echo", true).unwrap().id;
        let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
        store.bloblog_put(&access, volume, log, &[vec![7; bloblog::INLINE_BYTES + 1]]).unwrap();
        let page = store.bloblog_get(&access, volume, log, None).unwrap();
        let delivery = store.delivery_revision();
        let publications = store.publication_generation();
        let receipt = store.delivery_provider_revision(None, None).0;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let before = store.payload_size(&access, volume, Some(log), &cancelled).unwrap();
        let completion = super::super::completion::Completion::unix().unwrap();
        assert!(!receive_page(&store, &access, volume, log, &page, &completion.wake).unwrap());
        assert!(completion.drain().unwrap());
        assert!(store.payload_size(&access, volume, Some(log), &cancelled).unwrap() > before);
        assert_eq!(store.delivery_revision(), delivery);
        assert_eq!(store.publication_generation(), publications);
        assert_ne!(store.delivery_provider_revision(None, None).0, receipt);
        assert_eq!(store.delivery_provider_revision(receipt, Some(&[volume])).1, None);
        assert_eq!(store.bloblog_get(&access, volume, log, None).unwrap().entries, page.entries);
        let receipt = store.delivery_provider_revision(None, None).0;
        assert!(!receive_page(&store, &access, volume, log, &page, &completion.wake).unwrap());
        assert!(!completion.drain().unwrap());
        assert_eq!(store.delivery_provider_revision(None, None).0, receipt);
    }
}
