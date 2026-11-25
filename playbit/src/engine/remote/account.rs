//! Account bootstrap on the existing network owner and AP connection.
use super::*;

const JOURNAL_BYTES: usize = 128 * 1024 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotReceipt {
    root: crate::store::ReplicaRoot,
}

impl Remote {
    /// Account bootstrap is independent of bulk volume admission. Publication
    /// retains its existing dependency checks for volume-bearing records.
    pub(crate) fn initialize_account(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        self.connect(store)?;
        self.ensure_live(store)?;
        let prepared = self.begin_account_sync(store)?;
        self.finish_account_sync(store, prepared)
    }
    fn account_metadata_ready(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        if let Some(live) = &self.live {
            if !live.account_installed() {
                return Ok(());
            }
            self.runtime.block_on(live.account_receive_fence())?;
            self.account_ready.store(true, std::sync::atomic::Ordering::Release);
            self.completion.notify();
            // Install the recipient's saved decisions before showing the inbox.
            self.start_volume_invitations(store)?;
        }
        Ok(())
    }

    pub(super) fn begin_account_sync(
        &mut self,
        store: &Arc<LocalStore>,
    ) -> Result<Option<crate::engine::live::PreparedAccount>> {
        if store.recovery_state()?.is_none_or(|state| !state.acknowledged || state.revision == 0) {
            return Ok(None);
        }
        if self.live.as_ref().is_some_and(|live| live.account_installed()) {
            return Ok(None);
        }
        self.prepare_account_sync(store).map(Some)
    }

    fn prepare_account_sync(
        &mut self,
        store: &Arc<LocalStore>,
    ) -> Result<crate::engine::live::PreparedAccount> {
        self.account_ready.store(false, std::sync::atomic::Ordering::Release);
        store.seed_account_records()?;
        let connection = self.connection.as_ref().ok_or("AP disconnected")?.clone();
        let prepared = self.runtime.block_on(
            self.live.as_mut().ok_or("live account unavailable")?.begin_account(connection),
        )?;
        if let Err(error) = self.pull_account(store) {
            self.runtime.block_on(
                self.live
                    .as_mut()
                    .expect("account binding initialized live replication")
                    .fail_account(&error.to_string()),
            )?;
            return Err(error);
        }
        Ok(prepared)
    }

    pub(super) fn finish_account_sync(
        &mut self,
        store: &Arc<LocalStore>,
        prepared: Option<crate::engine::live::PreparedAccount>,
    ) -> Result<()> {
        let Some(prepared) = prepared else {
            return self.account_metadata_ready(store);
        };
        let result = (|| {
            let publisher = self.live.as_ref().ok_or("live account unavailable")?.publisher();
            let slots = self
                .live
                .as_ref()
                .expect("account binding initialized live replication")
                .apply_slots();
            let ready = publisher.account_ready_volumes();
            let mut cursor = None;
            while let Some(record) = store.pending_account_record(cursor)? {
                cursor = Some(record.key);
                if store
                    .account_record_dependencies(&record)?
                    .into_iter()
                    .flatten()
                    .all(|volume| ready.contains(&volume))
                {
                    let _permit = self.runtime.block_on(slots.acquire())?;
                    if store.stage_account_record(&record)?.is_none() {
                        store.adopt_account_record(&record.key)?;
                    }
                }
            }
            self.push_account(store)?;
            self.runtime.block_on(
                self.live
                    .as_mut()
                    .expect("account binding initialized live replication")
                    .finish_account(prepared),
            )
        })();
        if let Err(error) = &result {
            self.runtime.block_on(
                self.live
                    .as_mut()
                    .expect("account binding initialized live replication")
                    .fail_account(&error.to_string()),
            )?;
        }
        result?;
        self.account_metadata_ready(store)
    }

    pub(super) fn recover_account(
        &mut self,
        store: &Arc<LocalStore>,
        kind: crate::engine::live::RecoveryKind,
    ) -> Result<()> {
        if !self.live.as_ref().is_some_and(|live| live.account_recovery_is_current(kind)) {
            return Err("account recovery changed or failed before bootstrap".into());
        }
        // No unrelated volume pulls or admissions. Existing capability proofs
        // survive a transport reconnect and account-only gap recovery.
        let prepared = self.prepare_account_sync(store)?;
        self.finish_account_sync(store, Some(prepared))?;
        let live = self.live.as_ref().expect("account binding initialized live replication");
        if self.scope.read().provider() {
            self.runtime.block_on(live.account_receive_fence())?;
        } else {
            self.runtime.block_on(live.account_fence())?;
        }
        self.live
            .as_mut()
            .expect("account binding initialized live replication")
            .complete_account_recovery()
    }

    fn pull_account(&self, store: &LocalStore) -> Result<()> {
        let _timing = crate::trace::Interval::new("account.bootstrap_pull");
        let database = store.account_replica()?;
        let request = ServiceRequest::SyncAccount {
            operation_id: Id::generate()?.to_string(),
            direction: SyncDirection::Pull,
        };
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        // Transport retains a pinned read snapshot and bounded file journals,
        // never the account writer or profile gate.
        let (mut stream, prepared, output) = self.runtime.block_on(async {
            let mut stream = wire::begin(connection, &request).await?;
            wire::response(&mut stream, request.operation_id()).await?;
            let mut output = Vec::new();
            let mut receiver = kvdb::sync::PreparedSync::new(
                &database,
                crate::store::sync_staging(&store.profile_path().join(crate::paths::STAGING))?,
                crate::store::sync_journal(
                    &store.profile_path().join(crate::paths::STAGING),
                    JOURNAL_BYTES,
                )?,
                JOURNAL_BYTES,
                &mut output,
            )?;
            loop {
                wire::send_data(&mut stream, &output).await?;
                output.clear();
                let input = sync_payload(
                    wire::receive_transfer(&mut stream).await?,
                    request.operation_id(),
                )?;
                if receiver.process(&input, &mut output)? == SyncStatus::Done {
                    break;
                }
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((stream, receiver.finish()?, output))
        })?;
        {
            let slots = self.live.as_ref().ok_or("live account unavailable")?.apply_slots();
            let _permit = self.runtime.block_on(slots.acquire())?;
            store.merge_account_snapshot(prepared)?;
        }
        self.runtime.block_on(async {
            wire::send_data(&mut stream, &output).await?;
            stream.finish()?;
            let response = wire::response(&mut stream, request.operation_id()).await?;
            let receipt: SnapshotReceipt =
                serde_json::from_value(response.data.ok_or("missing account receipt")?)?;
            let _ = receipt.root;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
    }

    fn push_account(&self, store: &LocalStore) -> Result<()> {
        store.check_account_authority()?;
        let _timing = crate::trace::Interval::new("account.bootstrap_push");
        let database = store.account_replica()?;
        let request = ServiceRequest::SyncAccount {
            operation_id: Id::generate()?.to_string(),
            direction: SyncDirection::Push,
        };
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        self.runtime.block_on(async {
            store.check_account_authority()?;
            let mut stream = wire::begin(connection, &request).await?;
            wire::response(&mut stream, request.operation_id()).await?;
            let mut sender = SyncSender::new(&database)?;
            let mut output = vec![0; 64 * 1024];
            loop {
                let input = sync_payload(
                    wire::receive_transfer(&mut stream).await?,
                    request.operation_id(),
                )?;
                let status = sender.process(&input)?;
                let mut sent = false;
                loop {
                    let count = sender.write_output(&mut output)?;
                    if count == 0 {
                        break;
                    }
                    store.check_account_authority()?;
                    wire::send_data(&mut stream, &output[..count]).await?;
                    sent = true;
                }
                if status == SyncStatus::Done {
                    if sent {
                        sender.process(&sync_payload(
                            wire::receive_transfer(&mut stream).await?,
                            request.operation_id(),
                        )?)?;
                    }
                    break;
                }
            }
            stream.finish()?;
            let response = wire::response(&mut stream, request.operation_id()).await?;
            let receipt: SnapshotReceipt =
                serde_json::from_value(response.data.ok_or("missing account receipt")?)?;
            // Other devices may concurrently change AP state; the returned root
            // is not an acknowledgment of the source snapshot's exact root.
            let _ = receipt.root;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
    }
}
