//! Receipt delivery owns at most one task. Durable bits survive disconnect/restart;
//! a failure leaves them pending until a later completion or reconnect notification.
use super::*;
use crate::network::ap::{Frame, frame_type};
use std::sync::atomic::Ordering;

#[derive(Debug, thiserror::Error)]
#[error(
    "Playbit couldn’t check the user agreement with the sync service.{key_status} Retry to continue."
)]
struct AgreementCheckFailed {
    key_status: &'static str,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl NetworkHandles {
    pub(crate) fn report_acceptance(&self, store: Arc<LocalStore>) {
        self.onboarding.fetch_and(!4, Ordering::AcqRel);
        self.report_onboarding(store);
    }
    pub(crate) fn report_onboarding(&self, store: Arc<LocalStore>) {
        // Completed installations never allocate another reporting task.
        if self.onboarding.load(Ordering::Acquire) & 4 != 0 {
            return;
        }
        if *self.onboarding_account.lock().expect("onboarding identity lock poisoned")
            != Some(store.account().id)
        {
            return;
        }
        if self.onboarding.fetch_or(3, Ordering::AcqRel) & 1 != 0 {
            return;
        }
        let handles = self.clone();
        self.runtime.spawn(async move {
            loop {
                handles.onboarding.fetch_and(!2, Ordering::AcqRel);
                let result = handles.deliver_onboarding(&store).await;
                let finished = if matches!(result, Ok(true)) { 4 } else { 0 };
                // A sign-in or new acceptance during delivery requested another pass.
                if handles
                    .onboarding
                    .compare_exchange(1, finished, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
        });
    }
    async fn deliver_onboarding(&self, store: &Arc<LocalStore>) -> Result<bool> {
        let connection = {
            let account =
                self.onboarding_account.lock().expect("onboarding identity lock poisoned");
            if *account != Some(store.account().id) {
                return Ok(false);
            }
            self.onboarding_connection.lock().expect("connection publication lock poisoned").clone()
        };
        let Some(connection) = connection else {
            return Ok(false);
        };
        if connection.is_closed() {
            return Ok(false);
        }
        let recovery = store.recovery_state()?.is_some_and(|r| r.acknowledged && r.revision > 0);
        let initialized = self.account_initialized();
        let complete = recovery
            && initialized
            && store.display_name()?.is_some()
            && !store.agreement_required()?;
        let observed = 1
            | if recovery { 2 } else { 0 }
            | if initialized { 4 } else { 0 }
            | if complete { 8 } else { 0 };
        let pending = store.onboarding_pending(observed)?;
        if store.agreement_pending()? {
            connection.agreement(true).await?;
            store.set_agreement_required(false)?;
        }
        for stage in 4..=7 {
            let bit = 1 << (stage - 4);
            if pending & bit == 0 {
                continue;
            }
            let frame = Frame::new(frame_type::ONBOARDING, 0, [], [], vec![stage])?;
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut stream = connection.open_stream().await?;
                stream.send(&frame).await?;
                stream.finish()?;
                let reply = stream.answer_with_body_limit(frame.body.len()).await?;
                if reply != frame {
                    return Err("invalid onboarding receipt acknowledgment".into());
                }
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await??;
            store.acknowledge_onboarding(bit)?;
        }
        Ok(observed == 15)
    }
}

impl Remote {
    /// Every new sync checks the service, including when this profile or transport
    /// previously synced. Only an existing local acceptance may create the receipt.
    pub(super) fn require_sync_agreement(&mut self, store: &LocalStore) -> Result<()> {
        self.authenticate(store)?;
        self.confirm_sync_agreement(store)
    }

    pub(super) fn confirm_sync_agreement(&mut self, store: &LocalStore) -> Result<()> {
        use crate::network::ap::registration::{TERMS_REVISION, TERMS_URL};
        if self.identity.account_id()? != *store.account().id.as_bytes() {
            return Err("AP account differs from local account".into());
        }
        let transport = self.authenticated_transport().ok_or("AP disconnected")?;
        let confirmed = match self.exchange_agreement(store, &transport, store.agreement_pending()?)
        {
            // Let the connection owner retain its recovery scopes and retry budget.
            Err(error)
                if canceled(error.as_ref())
                    || error
                        .downcast_ref::<ClientError>()
                        .is_some_and(ClientError::is_connection_retryable) =>
            {
                return Err(error);
            }
            result => result,
        };
        if matches!(confirmed, Ok(true)) {
            self.connection = Some(transport.clone());
            *self.sync_connection.lock().expect("connection lock is not poisoned") =
                Some(transport);
            self.blob_transfers.channel_connection.notify_waiters();
            return Ok(());
        }
        // A restored AP may have lost the receipt. Retire any data workers still
        // using a previously confirmed transport, including publishers holding an Arc.
        if self.connection.is_some() {
            self.disconnect();
        }
        confirmed.map_err(|source| AgreementCheckFailed {
            key_status: if store
                .recovery_state()
                .is_ok_and(|state| state.is_some_and(|key| key.acknowledged))
            {
                " Your account key is saved."
            } else {
                ""
            },
            source,
        })?;
        Err(format!(
            "Sync requires the current Playbit User Agreement. \
             Read {TERMS_URL} and run pb accept-terms --revision {TERMS_REVISION}."
        )
        .into())
    }

    /// Resolve returning users before presenting onboarding, on the sign-in worker.
    /// A failed lookup leaves the local state intact and data transfer still gated.
    pub(in crate::engine) fn sign_in_agreement(&self) -> Option<bool> {
        self.runtime.block_on(self.authenticated_transport()?.agreement(false)).ok()
    }

    fn exchange_agreement(
        &self,
        store: &LocalStore,
        connection: &ApConnection,
        accept: bool,
    ) -> Result<bool> {
        let previously_recorded = store.agreement_recorded()?;
        let accepted = Self::while_uncanceled(
            &self.runtime,
            self.canceled.as_ref(),
            connection.agreement(accept),
        )
        .inspect_err(|error| {
            crate::platform::failure::log(
                store.profile_path(),
                "check user agreement",
                &format!("{error:?}"),
            );
        })?;
        // A query that observed no server receipt can finish after a concurrent
        // acceptance was acknowledged. Only invalidate a receipt cached before this request.
        if accepted || previously_recorded {
            store.set_agreement_required(!accepted)?;
        }
        if !accepted {
            self.onboarding.fetch_and(!4, Ordering::AcqRel);
        }
        Ok(accepted)
    }
}
