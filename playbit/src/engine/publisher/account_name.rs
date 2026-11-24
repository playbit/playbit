//! Publish the readable profile independently of account and volume replication.
//! At most one request is in flight per connection. Retries reread the durable winner.
use super::*;
use crate::network::ap::service::ServiceFailure;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::time::Duration;
use tokio::{sync::Notify, task::JoinHandle};

pub(super) struct Publisher {
    ready: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Publisher {
    pub(super) fn start(
        store: Arc<LocalStore>,
        connection: Arc<ApConnection>,
        slots: Arc<Semaphore>,
    ) -> Self {
        let ready = Arc::new(Notify::new());
        let wake = ready.clone();
        let task = tokio::spawn(async move {
            let mut reported = None;
            loop {
                wake.notified().await;
                let mut delay = Duration::from_secs(1);
                loop {
                    match publish(&store, &connection, &slots, reported).await {
                        Ok(version) => {
                            reported = version;
                            break;
                        }
                        Err(error) => {
                            if error.downcast_ref::<ServiceFailure>().map(ServiceFailure::code)
                                == Some("unsupported_operation")
                            {
                                return;
                            }
                            if delay == Duration::from_secs(1) {
                                crate::diagnostic!(
                                    "account display name publication failed, retrying: {error}"
                                );
                            }
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
        });
        Self { ready, task }
    }
    pub(super) fn notify(&self) {
        self.ready.notify_one();
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn publish(
    store: &Arc<LocalStore>,
    connection: &ApConnection,
    slots: &Arc<Semaphore>,
    reported: Option<blake3::Hash>,
) -> Result<Option<blake3::Hash>> {
    let permit = slots.clone().acquire_owned().await?;
    let source = store.clone();
    let profile = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        source.account_display_name_publication(reported)
    })
    .await??;
    let Some(profile) = profile else { return Ok(reported) };
    store.check_account_authority()?;
    let request = ServiceRequest::PutAccountDisplayName {
        operation_id: Id::generate()?.to_string(),
        name: profile.name,
        record_key: crate::identifier::encode(&profile.record.key),
        clock: profile.record.clock.as_u64().to_string(),
        ciphertext: URL_SAFE_NO_PAD.encode(profile.record.ciphertext),
    };
    tokio::time::timeout(
        connection.response_timeout(),
        service_client::request(connection, &request),
    )
    .await??;
    Ok(Some(profile.version))
}
