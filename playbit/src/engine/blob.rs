//! Bounded local file jobs, with lazy encrypted transport for connected owners.
//! Synchronous runtime entry points never establish a connection themselves.
// Error ownership: read errors return to the guest file operation; upload
// errors return to synchronization. AP blob_transfer/runtime_blobs tests exercise
// interrupted and unauthorized transfers. RNG/OS failure at every propagation
// site is not separately injected. Never cache unverified bytes or manufacture
// durable acknowledgment.
use crate::engine::Result;
#[cfg(feature = "connected-engine")]
use crate::network::ap::{
    ApConnection, ApStream, operation::ServiceRequest, service_client as wire,
};
#[cfg(any(feature = "connected-engine", test))]
use crate::store::Id;
use crate::store::{
    Access, Error, LocalStore,
    volume_index::{BlobVersion, EntryAddress},
};
use std::sync::Arc;
#[cfg(feature = "connected-engine")]
use std::sync::Mutex;
#[cfg(feature = "connected-engine")]
use std::{
    fs,
    io::{Read, Seek, Write},
    time::Duration,
};

#[cfg(feature = "connected-engine")]
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest plaintext an independent blob can hold: one transfer envelope's payload.
pub const MAX_FILE_BYTES: u64 = crate::store::MAX_BLOB_PLAINTEXT_BYTES;

#[cfg(all(test, feature = "connected-engine"))]
mod denial_tests;
mod leases;
pub use leases::{LEASE_LIMIT, ReadView};
const TRANSFER_LIMIT: usize = 2;
#[cfg(feature = "connected-engine")]
const UPLOAD_LIMIT: usize = 1;
#[cfg(feature = "connected-engine")]
const _: () = assert!(UPLOAD_LIMIT < TRANSFER_LIMIT);
pub(crate) struct BlobTransfers {
    #[cfg(feature = "connected-engine")]
    pub(crate) logs: Arc<crate::engine::bloblog::Calls>,
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) channels: crate::engine::session_channels::Hub,
    #[cfg(feature = "connected-engine")]
    pub(crate) channel_connection: tokio::sync::Notify,
    bulk: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "connected-engine")]
    uploads: Arc<tokio::sync::Semaphore>,
}
impl Default for BlobTransfers {
    fn default() -> Self {
        Self {
            #[cfg(feature = "connected-engine")]
            logs: Default::default(),
            #[cfg(all(
                feature = "connected-engine",
                any(feature = "native-runtime", feature = "wasm-runtime")
            ))]
            channels: Default::default(),
            #[cfg(feature = "connected-engine")]
            channel_connection: Default::default(),
            bulk: Arc::new(tokio::sync::Semaphore::new(TRANSFER_LIMIT)),
            #[cfg(feature = "connected-engine")]
            uploads: Arc::new(tokio::sync::Semaphore::new(UPLOAD_LIMIT)),
        }
    }
}
#[cfg(any(feature = "connected-engine", test))]
type TransferCredit = Arc<tokio::sync::OwnedSemaphorePermit>;

#[derive(Clone)]
pub struct BlobTransport {
    handle: Option<tokio::runtime::Handle>,
    local: bool,
    #[cfg(feature = "connected-engine")]
    connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    completion: Option<Arc<super::completion::Wake>>,
    transfers: Arc<BlobTransfers>,
}
impl BlobTransport {
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn bloblog_boundary(
        &self,
    ) -> crate::runtime::host::HostResult<super::bloblog::CallBoundary> {
        Ok(super::bloblog::CallBoundary {
            connection: self
                .connection
                .lock()
                .map_err(|_| playbit_api::abi::syscall::Error::Internal)?
                .clone()
                .filter(|connection| !connection.is_closed()),
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
        })
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn bloblog_call(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        call: crate::runtime::host::DataCall,
        boundary: super::bloblog::CallBoundary,
    ) -> crate::runtime::host::HostResult<
        impl std::future::Future<
            Output = crate::runtime::host::HostResult<crate::runtime::host::DataResult>,
        > + Send
        + 'static,
    > {
        use playbit_api::abi::syscall::Error;
        self.handle.as_ref().ok_or(Error::Unavailable)?;
        let super::bloblog::CallBoundary { connection, deadline } = boundary;
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Unavailable);
        }
        let credit = self.transfers.logs.admit(&call)?;
        let completion = self.completion.clone();
        let timeout_error =
            if call.operation.is_mutation() { Error::Uncertain } else { Error::Unavailable };
        Ok(async move {
            let lease = tokio::time::timeout_at(deadline, credit.acquire())
                .await
                .map_err(|_| Error::Unavailable)?;
            tokio::time::timeout_at(
                deadline,
                crate::engine::bloblog::execute(connection, store, access, call, lease, completion),
            )
            .await
            .unwrap_or(Err(timeout_error))
        })
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn channel_call(
        &self,
        store: Arc<LocalStore>,
        session: String,
        build: [u8; 32],
        operation: crate::runtime::host::DataOperation,
    ) -> Result<
        impl std::future::Future<Output = Result<crate::runtime::host::DataResult>> + Send + 'static,
    > {
        if self.handle.is_none() {
            return Err("AP runtime unavailable".into());
        }
        let connection = self.connection.clone();
        let transfers = self.transfers.clone();
        let mut session = store.session(&session)?;
        session.resolved_build_id = crate::identifier::encode(&build);
        Ok(async move {
            if let crate::runtime::host::DataOperation::ChannelCommand {
                instance,
                name,
                action: 3 | 4,
                ..
            } = &operation
            {
                return transfers.channels.unsubscribe(*instance, name).await;
            }
            loop {
                let ready = transfers.channel_connection.notified();
                let current = connection.lock().map_err(|_| "AP connection poisoned")?.clone();
                if let Some(current) = current.filter(|connection| !connection.is_closed()) {
                    return transfers.channels.execute(current, store, session, operation).await;
                }
                if !matches!(operation, crate::runtime::host::DataOperation::ChannelEvents { .. }) {
                    return Err("channel disconnected".into());
                }
                ready.await;
            }
        })
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn channel_consumed(
        &self,
        recipient: [u8; 16],
        account: [u8; 16],
        sender: [u8; 16],
        message: [u8; 16],
    ) {
        self.transfers.channels.consumed(recipient, account, sender, message);
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn retire_channels(&self, instance: [u8; 16]) {
        let cleanup = self.transfers.channels.retire(instance);
        if let Some(handle) = &self.handle {
            handle.spawn(cleanup);
        }
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn spawn_channel(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        self.handle.as_ref().ok_or("AP runtime unavailable")?.spawn(work);
        Ok(())
    }
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_connected(&self) -> bool {
        self.connection
            .lock()
            .expect("connection mutex poisoned")
            .as_ref()
            .is_some_and(|connection| !connection.is_closed())
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
    pub(crate) fn local() -> Self {
        Self {
            handle: None,
            local: true,
            #[cfg(feature = "connected-engine")]
            connection: Arc::new(Mutex::new(None)),
            transfers: Arc::new(BlobTransfers::default()),
            #[cfg(all(
                feature = "connected-engine",
                any(feature = "native-runtime", feature = "wasm-runtime")
            ))]
            completion: None,
        }
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn new(
        handle: tokio::runtime::Handle,
        connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
        transfers: Arc<BlobTransfers>,
        #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))] completion: Option<
            Arc<super::completion::Wake>,
        >,
    ) -> Self {
        Self {
            handle: Some(handle),
            local: false,
            connection,
            transfers,
            #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
            completion,
        }
    }
    #[cfg(test)]
    pub(crate) fn new_local(handle: tokio::runtime::Handle, transfers: Arc<BlobTransfers>) -> Self {
        Self {
            handle: Some(handle),
            local: true,
            #[cfg(feature = "connected-engine")]
            connection: Arc::new(Mutex::new(None)),
            #[cfg(all(
                feature = "connected-engine",
                any(feature = "native-runtime", feature = "wasm-runtime")
            ))]
            completion: None,
            transfers,
        }
    }
}
#[cfg(feature = "connected-engine")]
impl BlobTransport {
    #[cfg(feature = "file-provider")]
    pub(crate) fn register_provider_push(
        &self,
        request: &ServiceRequest,
    ) -> Result<serde_json::Value> {
        let connection = self.provider_push_connection()?;
        let handle = self.handle.as_ref().ok_or("AP runtime unavailable")?;
        let response = handle.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), wire::request(&connection, request))
                .await
                .map_err(|_| "File Provider push registration timed out")?
        })?;
        Ok(response.data.unwrap_or(serde_json::Value::Null))
    }

    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_push_connection(&self) -> Result<Arc<ApConnection>> {
        self.connection
            .lock()
            .expect("connection mutex poisoned")
            .clone()
            .filter(|connection| !connection.is_closed())
            .ok_or_else(|| "AP disconnected; push registration will retry on reconnect".into())
    }

    /// Upload only versions captured before the index publication fence. A
    /// database acknowledgment never acknowledges immutable content durability.
    pub fn upload_pending(
        &self,
        store: &LocalStore,
        access: &Access,
        session: Option<&str>,
        volume: Id,
        versions: Vec<BlobVersion>,
    ) -> Result<()> {
        // Automatic uploads leave one bulk credit available for file reads.
        let credit = self
            .handle
            .as_ref()
            .ok_or("connected blob runtime is unavailable")?
            .block_on(self.upload_credit())?;
        self.upload_pending_admitted(store, access, session, volume, versions, credit)
    }

    pub(super) async fn upload_credit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        Ok(self.transfers.uploads.clone().acquire_owned().await?)
    }

    pub(super) fn upload_pending_admitted(
        &self,
        store: &LocalStore,
        access: &Access,
        session: Option<&str>,
        volume: Id,
        versions: Vec<BlobVersion>,
        _credit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let session = shared_scope(store, session, volume)?;
        for version in versions {
            if version.address.volume != volume {
                return Err("blob upload volume does not match its version".into());
            }
            let durable = match store.blob_upload_durable(access, &version) {
                Ok(durable) => durable,
                Err(crate::store::Error::AccessDenied(_) | crate::store::Error::NotFound(_)) => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if durable {
                store.acknowledge_blob_upload(access, &version)?;
                continue;
            }
            let credit = self.credit()?;
            let encrypted = store.encrypt_blob_version(access, &version)?;
            store.blob_upload_durable(access, &version)?;
            let length = encrypted.metadata()?.len();
            if request_length(&version)? != length {
                return Err("blob envelope length differs from index declaration".into());
            }
            let request = ServiceRequest::UploadBlob {
                operation_id: Id::generate()?.to_string(),
                session_id: session.map(str::to_owned),
                build_id: build_scope(store, access, session)?,
                version: version.clone(),
                content_length: length,
            };
            let result = self.upload_version(&request, &version, encrypted, credit);
            checked_session_result(store, &request, result)?;
            store.acknowledge_blob_upload(access, &version)?;
            store.enforce_cache_budget()?;
        }
        Ok(())
    }

    fn upload_version(
        &self,
        request: &ServiceRequest,
        version: &BlobVersion,
        encrypted: fs::File,
        credit: TransferCredit,
    ) -> Result<()> {
        let connection = self.current_connection()?;
        self.handle.as_ref().ok_or("connected blob runtime is unavailable")?.block_on(async {
            tokio::time::timeout(TRANSFER_TIMEOUT, async {
                let mut stream = wire::begin(&connection, request).await?;
                let begin = wire::response(&mut stream, request.operation_id()).await?;
                if let Some(stored) = begin.data {
                    return durable(&stored, version);
                }
                send_file(&mut stream, encrypted, credit.clone()).await?;
                stream.finish()?;
                acknowledged(&mut stream, request, version).await
            })
            .await
            .map_err(|_| "blob upload timed out; local content remains pending synchronization")?
        })
    }
    fn current_connection(&self) -> Result<Arc<ApConnection>> {
        self.connection
            .lock()
            .map_err(|_| "AP connection state is unavailable")?
            .clone()
            .ok_or_else(|| "file content is unavailable offline".into())
    }
    fn credit(&self) -> Result<TransferCredit> {
        Ok(Arc::new(
            self.handle
                .as_ref()
                .ok_or("connected blob runtime is unavailable")?
                .block_on(self.transfers.bulk.clone().acquire_owned())?,
        ))
    }
}
#[cfg(feature = "connected-engine")]
fn request_length(reference: &BlobVersion) -> Result<u64> {
    Ok(crate::store::blob_envelope_len(reference.len_bytes)?)
}
#[cfg(feature = "connected-engine")]
async fn accepted(
    scratch: &std::path::Path,
    stream: &mut ApStream,
    request: &ServiceRequest,
    reference: &BlobVersion,
    credit: TransferCredit,
    progress: &mut Option<u64>,
) -> Result<fs::File> {
    let expected = request_length(reference)?;
    // The metadata can precede its immutable body. This read is covered by
    // download_file's transfer deadline while AP waits for the actual upload.
    let header = crate::network::ap::service::response(
        stream.receive_with_body_limit(4096).await?,
        request.operation_id(),
    )?
    .data
    .ok_or("missing encrypted file header")?;
    if header["content_length"].as_u64() != Some(expected) {
        return Err("encrypted file header does not match the requested reference".into());
    }
    let mut file = crate::store::blob_transfer_file(scratch)?;
    let mut received = 0u64;
    *progress = Some(received);
    while received < expected {
        let chunk = wire::receive_data(stream).await?;
        if chunk.is_empty() || chunk.len() > 256 * 1024 || chunk.len() as u64 > expected - received
        {
            return Err("invalid encrypted file transfer length".into());
        }
        received += chunk.len() as u64;
        let held = credit.clone();
        file = tokio::task::spawn_blocking(move || -> Result<_> {
            let _credit = held;
            file.write_all(&chunk)?;
            Ok(file)
        })
        .await??;
        *progress = Some(received);
    }
    acknowledged(stream, request, reference).await?;
    file.rewind()?;
    Ok(file)
}

#[cfg(feature = "connected-engine")]
async fn send_file(
    stream: &mut ApStream,
    mut file: fs::File,
    credit: TransferCredit,
) -> Result<()> {
    loop {
        let held = credit.clone();
        let (next, bytes) = tokio::task::spawn_blocking(move || -> Result<_> {
            let _credit = held;
            let mut bytes = vec![0; 64 * 1024];
            let count = file.read(&mut bytes)?;
            bytes.truncate(count);
            Ok((file, bytes))
        })
        .await??;
        file = next;
        if bytes.is_empty() {
            break;
        }
        wire::send_data(stream, &bytes).await?;
    }
    Ok(())
}
#[cfg(feature = "connected-engine")]
async fn acknowledged(
    stream: &mut ApStream,
    request: &ServiceRequest,
    reference: &BlobVersion,
) -> Result<()> {
    let acknowledgment = wire::response(stream, request.operation_id())
        .await?
        .data
        .ok_or("missing durable file acknowledgment")?;
    durable(&acknowledgment, reference)
}
#[cfg(feature = "connected-engine")]
fn durable(acknowledgment: &serde_json::Value, reference: &BlobVersion) -> Result<()> {
    if acknowledgment["content_length"].as_u64() != Some(request_length(reference)?)
        || acknowledgment["encrypted_key"].as_str()
            != Some(crate::identifier::encode(&reference.encrypted_key).as_str())
    {
        return Err("durable blob acknowledgment does not match its index declaration".into());
    }
    Ok(())
}
#[cfg(feature = "connected-engine")]
fn build_scope(
    store: &LocalStore,
    access: &Access,
    session: Option<&str>,
) -> Result<Option<String>> {
    session
        .map(|id| {
            access
                .build_id()
                .map(str::to_owned)
                .map(Ok)
                .unwrap_or_else(|| store.session(id).map(|session| session.resolved_build_id))
        })
        .transpose()
        .map_err(Into::into)
}

#[cfg(feature = "connected-engine")]
fn checked_session_result<T>(
    store: &LocalStore,
    request: &ServiceRequest,
    result: Result<T>,
) -> Result<T> {
    if let Err(error) = &result {
        let binding = match request {
            ServiceRequest::UploadBlob {
                session_id: Some(session), build_id: Some(build), ..
            }
            | ServiceRequest::DownloadBlob {
                session_id: Some(session),
                build_id: Some(build),
                ..
            } => Some((session, build)),
            _ => None,
        };
        if let Some((id, build)) = binding
            && let Some(denial) = error
                .downcast_ref::<crate::network::ap::service::ServiceFailure>()
                .and_then(|failure| failure.denial())
        {
            let session = store.session(id)?;
            if session.resolved_build_id == *build
                && store.receive_ap_session_denial(denial, &session)?
            {
                return Err(Error::AccessDenied("session participant revoked").into());
            }
        }
        let message = error.to_string();
        if message.contains("access denied") || message.contains("session is not admitted") {
            return Err(Error::AccessDenied("remote file reference").into());
        }
    }
    result
}

// Selected personal volumes use owner-authorized AP transport. The local exact
// version Access is still checked before and after every fetch or upload.
#[cfg(feature = "connected-engine")]
fn shared_scope<'a>(
    store: &LocalStore,
    session: Option<&'a str>,
    volume: Id,
) -> Result<Option<&'a str>> {
    store.require_replication_supported(volume)?;
    if let Some(id) = session {
        if store.is_local_session(id)? && store.session(id)?.shared_volume.is_none() {
            return Err("automatic session synchronization is not supported yet".into());
        }
        let selected = store.session(id)?;
        if volume != selected.replica_volume() {
            if store.volume(volume)?.owner != store.account().id {
                return Err("personal volume owner differs from account".into());
            }
            return Ok(None);
        }
    }
    Ok(session)
}
