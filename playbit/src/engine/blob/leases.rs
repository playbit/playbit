//! Bounded verified descriptors on the existing storage/bulk workers.
//! Cache entries grant no authority; each call checks the persistent index.
use super::*;
use crate::store::{BlobReadLease, VerifiedBlob};

pub use crate::store::BLOB_DESCRIPTOR_LIMIT as LEASE_LIMIT;

/// One captured revision. Metadata admission does not require resident bytes.
/// The first successful read retains its verified backing until this view ends.
pub struct ReadView {
    store: Arc<LocalStore>,
    access: Access,
    session: Option<String>,
    version: BlobVersion,
    file: Option<Arc<VerifiedBlob>>,
    storage: Option<BlobReadLease>,
}
impl Drop for ReadView {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("Could not enforce blob cache budget after File disposal: {error}");
        }
    }
}
impl ReadView {
    fn authorize(&self) -> Result<()> {
        self.storage
            .as_ref()
            .ok_or(Error::AccessDenied("read view is closed"))?
            .validate(&self.store, &self.access)?;
        Ok(())
    }

    /// Worker-only close: release the captured revision before reclaiming cache
    /// paths. Explicit close reports cleanup errors; disposal logs them.
    pub fn close(mut self) -> Result<()> {
        self.release()
    }

    fn release(&mut self) -> Result<()> {
        let Some(mut storage) = self.storage.take() else { return Ok(()) };
        self.file = None;
        if !storage.release() {
            return Ok(());
        }
        self.store.enforce_cache_budget()?;
        Ok(())
    }

    /// Captured metadata remains available without hydration, subject to current Read.
    pub fn version(&self) -> Result<&BlobVersion> {
        self.authorize()?;
        Ok(&self.version)
    }

    pub fn read(&mut self, transport: &BlobTransport, offset: u64, length: u32) -> Result<Vec<u8>> {
        if length as usize > crate::store::BLOB_CHUNK_BYTES
            || offset > MAX_FILE_BYTES
            || offset.checked_add(u64::from(length)).is_none_or(|end| end > MAX_FILE_BYTES)
        {
            return Err(Error::InvalidInput("invalid blob read range").into());
        }
        self.authorize()?;
        if length == 0 || offset >= self.version.len_bytes {
            return Ok(Vec::new());
        }
        if self.file.is_none() {
            self.file = Some(transport.verified_version(
                self.store.clone(),
                self.access.clone(),
                self.session.clone(),
                &self.version,
            )?);
        }
        let result =
            self.file.as_ref().expect("read has acquired its backing").read_range(offset, length);
        let bytes = match result {
            Err(Error::Corrupt(_)) => {
                self.store.invalidate_verified_blob(&self.version)?;
                self.file = None;
                let fresh = transport.prepare_file(
                    self.store.clone(),
                    self.access.clone(),
                    self.session.clone(),
                    &self.version,
                )?;
                let bytes = fresh.read_range(offset, length)?;
                self.file = Some(fresh);
                bytes
            }
            result => result?,
        };
        self.authorize()?;
        Ok(bytes)
    }
}
impl BlobTransport {
    /// Worker-only retrieval. A ready descriptor is shared; a miss verifies or
    /// hydrates under one of the existing two immutable-transfer credits.
    pub fn verified_file(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        address: EntryAddress,
    ) -> Result<(BlobVersion, Arc<VerifiedBlob>)> {
        let version = store.blob_version(&access, address, None)?;
        let file = self.verified_version(store, access, session, &version)?;
        Ok((version, file))
    }

    /// Capture namespace metadata on a storage worker without hydrating contents.
    /// The shared admission pool includes views whose bytes have not arrived yet.
    pub fn open_read(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        address: EntryAddress,
    ) -> Result<ReadView> {
        let version = store.blob_version(&access, address, None)?;
        self.open_read_version(store, access, session, version)
    }

    /// Select exactly the authorized metadata resolved by File open. A newer
    /// publication during lease acquisition cannot redirect this captured view.
    pub fn open_read_version(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        version: BlobVersion,
    ) -> Result<ReadView> {
        let storage = store.lease_blob_version(&access, &version)?;
        let file = store.cached_verified_blob(&version, &access)?;
        Ok(ReadView { store, access, session, version, file, storage: Some(storage) })
    }

    fn verified_version(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        version: &BlobVersion,
    ) -> Result<Arc<VerifiedBlob>> {
        let cached = store.cached_verified_blob(version, &access)?;
        let file = match cached {
            Some(file) => file,
            None => self.prepare_file(store.clone(), access.clone(), session, version)?,
        };
        authorize_version(&store, &access, version)?;
        Ok(file)
    }

    /// One worker-owned read. A corrupt cached range gets one fresh preparation
    /// of the same immutable version; neither effects nor uncertain exchanges replay.
    pub fn read_range(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        address: EntryAddress,
        offset: u64,
        limit: u32,
    ) -> Result<(BlobVersion, Vec<u8>)> {
        let (version, file) =
            self.verified_file(store.clone(), access.clone(), session.clone(), address)?;
        let result = match file.read_range(offset, limit) {
            Err(Error::Corrupt(_)) => {
                store.invalidate_verified_blob(&version)?;
                let fresh = self.prepare_file(store.clone(), access.clone(), session, &version)?;
                fresh.read_range(offset, limit)
            }
            result => result,
        };
        if result.is_err() {
            store.invalidate_verified_blob(&version)?;
        }
        let bytes = result?;
        authorize_version(&store, &access, &version)?;
        Ok((version, bytes))
    }

    fn prepare_file(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        version: &BlobVersion,
    ) -> Result<Arc<VerifiedBlob>> {
        authorize_version(&store, &access, version)?;
        let generation = store.blob_eviction_generation();
        let credit =
            self.transfers.bulk.clone().try_acquire_owned().map_err(|_| Error::LimitExceeded)?;
        // This caller already owns a storage worker. Verify resident bytes
        // there; only missing/corrupt bytes require the connected runtime.
        let file = Arc::new(match store.verify_blob_version(&access, version) {
            Ok(file) => file,
            Err(error)
                if !self.local
                    && matches!(error, Error::NotFound("blob content") | Error::Corrupt(_)) =>
            {
                let runtime =
                    self.handle.as_ref().ok_or("connected blob runtime is unavailable")?;
                #[cfg(feature = "connected-engine")]
                {
                    runtime.block_on(self.download_file(
                        store.clone(),
                        access.clone(),
                        session,
                        version.clone(),
                        Arc::new(credit),
                    ))?
                }
                #[cfg(not(feature = "connected-engine"))]
                {
                    let _ = (runtime, session, credit);
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        });
        authorize_version(&store, &access, version)?;
        store.cache_verified_blob(version.clone(), access, file.clone(), generation)?;
        Ok(file)
    }

    #[cfg(feature = "connected-engine")]
    async fn download_file(
        &self,
        store: Arc<LocalStore>,
        access: Access,
        session: Option<String>,
        version: BlobVersion,
        credit: TransferCredit,
    ) -> Result<VerifiedBlob> {
        let scope = shared_scope(&store, session.as_deref(), version.address.volume)?;
        let request = ServiceRequest::DownloadBlob {
            operation_id: Id::generate()?.to_string(),
            session_id: scope.map(str::to_owned),
            build_id: build_scope(&store, &access, scope)?,
            version: version.clone(),
        };
        let connection = self.current_connection()?;
        let mut received = None;
        let downloaded = tokio::time::timeout(TRANSFER_TIMEOUT, async {
            let mut stream = wire::begin(&connection, &request).await?;
            stream.finish()?;
            accepted(
                &store.profile_path().join(crate::paths::STAGING),
                &mut stream,
                &request,
                &version,
                credit.clone(),
                &mut received,
            )
            .await
        })
        .await
        .map_err(|_| match received {
            None => "blob download timed out before receiving the content header".to_owned(),
            Some(bytes) => {
                format!("blob download timed out after receiving {bytes} encrypted bytes")
            }
        })?;
        let mut encrypted = checked_session_result(&store, &request, downloaded)?;
        let verified = blocking(
            store.clone(),
            access.clone(),
            version.clone(),
            credit,
            move |store, access, version| {
                store.receive_blob_version(access, version, &mut encrypted, true)?;
                let verified = store.verify_blob_version(access, version)?;
                // Keep the verified descriptor for this read while reclaiming
                // recoverable paths, even for download-only clients. Eviction
                // advances the generation so it cannot install an idle lease.
                store.enforce_cache_budget()?;
                Ok(verified)
            },
        )
        .await?;
        authorize_version(&store, &access, &version)?;
        Ok(verified)
    }
}

#[cfg(any(feature = "connected-engine", test))]
async fn blocking<T: Send + 'static>(
    store: Arc<LocalStore>,
    access: Access,
    version: BlobVersion,
    credit: TransferCredit,
    operation: impl FnOnce(&LocalStore, &Access, &BlobVersion) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let _credit = credit;
        operation(&store, &access, &version)
    })
    .await?
}

fn authorize_version(
    store: &LocalStore,
    access: &Access,
    version: &BlobVersion,
) -> crate::store::Result<()> {
    store.validate_blob_version(access, version)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded descriptors and current authority.")]
mod tests;
