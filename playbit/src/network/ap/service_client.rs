//! Typed service requests and multiplexed data frames on an existing AP connection.
use crate::network::ap::{
    ApConnection, ApReceiveStream, ApSendStream, ApStream, Frame, frame_type, operation::*,
};

use super::service;
pub use super::service::Result;

#[derive(Debug)]
pub(crate) struct SharedServiceUpgradeRequired;
impl std::fmt::Display for SharedServiceUpgradeRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(service::SHARED_SERVICE_UPGRADE_REQUIRED)
    }
}
impl std::error::Error for SharedServiceUpgradeRequired {}

pub async fn begin(connection: &ApConnection, request: &ServiceRequest) -> Result<ApStream> {
    begin_with_features(connection, request, service::required_features(request)).await
}

pub(crate) async fn require_features(
    connection: &ApConnection,
    required_features: &[&str],
) -> Result<()> {
    service::validate_features(required_features)?;
    if required_features.is_empty() {
        return Ok(());
    }
    let description = connection.describe().await?;
    if let Some(feature) = required_features
        .iter()
        .copied()
        .find(|name| !description.features.iter().any(|supported| supported == name))
    {
        if service::SHARED_REGISTRATION_FEATURES.contains(&feature) {
            return Err(SharedServiceUpgradeRequired.into());
        }
        return Err("required AP feature is unavailable".into());
    }
    Ok(())
}

pub async fn begin_with_features(
    connection: &ApConnection,
    request: &ServiceRequest,
    required_features: &[&str],
) -> Result<ApStream> {
    require_features(connection, required_features).await?;
    let provider_operation = match request {
        ServiceRequest::RegisterFileProviderAccountPush { .. } => {
            Some("register_file_provider_account_push")
        }
        ServiceRequest::InvalidateFileProviderAccountPush { .. } => {
            Some("invalidate_file_provider_account_push")
        }
        _ => None,
    };
    if let Some(operation) = provider_operation {
        let description = connection.describe().await?;
        if !description.operations.iter().any(|supported| supported == operation) {
            return Err("account File Provider push is unavailable on this AP server".into());
        }
    }
    let _timing = crate::trace::Interval::new("ap.request_send");
    let frame = service::request_with_features(request, required_features)?;
    let mut stream = connection.open_stream().await?;
    stream.send(&frame).await?;
    Ok(stream)
}

pub async fn response(stream: &mut ApStream, operation_id: &str) -> Result<ServiceResponse> {
    let _timing = crate::trace::Interval::new("ap.response_wait");
    service::response(stream.answer().await?, operation_id)
}

/// Observe readiness on the existing AP connection; no periodic freshness reads.
pub async fn watch_invitation(
    connection: &ApConnection,
    request: &ServiceRequest,
) -> Result<ApStream> {
    if !matches!(request, ServiceRequest::WatchInvitation { .. }) {
        return Err("invitation subscription requires WatchInvitation".into());
    }
    tokio::time::timeout(connection.response_timeout(), async {
        let mut stream = begin(connection, request).await?;
        service::response(stream.receive_with_body_limit(4096).await?, request.operation_id())?;
        Ok(stream)
    })
    .await
    .map_err(|_| crate::network::ap::ClientError::ResponseTimeout)?
}

pub async fn receive_invitation(stream: &mut ApStream) -> Result<InvitationStatus> {
    let body = service::data(stream.receive_with_body_limit(4096).await?)?;
    Ok(serde_json::from_slice(&body)?)
}

pub async fn request(
    connection: &ApConnection,
    request: &ServiceRequest,
) -> Result<ServiceResponse> {
    if matches!(request, ServiceRequest::GetBloblog { .. }) {
        return tokio::time::timeout(connection.response_timeout(), async {
            let mut stream = begin(connection, request).await?;
            stream.finish()?;
            service::response(
                stream.receive_with_body_limit(384 * 1024).await?,
                request.operation_id(),
            )
        })
        .await
        .map_err(|_| crate::network::ap::ClientError::ResponseTimeout)?;
    }
    if matches!(request, ServiceRequest::GetAppDisplay { .. }) {
        return tokio::time::timeout(connection.response_timeout(), async {
            let mut stream = begin(connection, request).await?;
            stream.finish()?;
            service::response(
                stream.receive_with_body_limit(BUILD_DISPLAY_LIMIT).await?,
                request.operation_id(),
            )
        })
        .await
        .map_err(|_| crate::network::ap::ClientError::ResponseTimeout)?;
    }
    let mut stream = begin(connection, request).await?;
    stream.finish()?;
    response(&mut stream, request.operation_id()).await
}

pub async fn send_data(stream: &mut ApStream, bytes: &[u8]) -> Result<()> {
    let _timing = crate::trace::Interval::new("ap.transfer_send");
    for chunk in bytes.chunks(256 * 1024) {
        stream.send(&Frame::new(frame_type::DIRECT, 0, [], SERVICE_DESTINATION, chunk)?).await?;
    }
    Ok(())
}

/// Wait for an unsolicited frame. A listener may sit here indefinitely, so this
/// read is deliberately unbounded; a transfer that expects the next frame of an
/// answer it already started must use `receive_transfer`.
pub async fn receive_data(stream: &mut ApStream) -> Result<Vec<u8>> {
    service::data(stream.receive().await?)
}

/// Wait for the next frame of a transfer this caller asked for. The AP having
/// answered the request does not mean it will keep sending: without a deadline
/// between frames it can stall a transfer indefinitely while the connection
/// stays alive.
pub async fn receive_transfer(stream: &mut ApStream) -> Result<Vec<u8>> {
    let _timing = crate::trace::Interval::new("ap.transfer_receive");
    service::data(stream.answer().await?)
}

/// One bounded namespace evidence frame follows an AP Index pull's kvdb Bye.
/// The caller verifies its digest and scope against the final authenticated receipt.
pub async fn receive_namespace_evidence(stream: &mut ApStream) -> Result<Vec<u8>> {
    service::data(
        stream
            .answer_with_body_limit(
                crate::store::volume_index::namespace::receipt::MAX_EVIDENCE_BYTES,
            )
            .await?,
    )
}

/// Establish delivery before bootstrap; the stream remains writable for a
/// checkpoint request after snapshot merge. Owner scope still requires AP authority.
pub async fn subscribe_replica(
    connection: &ApConnection,
    request: &ServiceRequest,
) -> Result<(ApStream, crate::network::ap::replication::SubscriptionAck)> {
    if !matches!(
        request,
        ServiceRequest::SubscribeReplica { .. }
            | ServiceRequest::SubscribeAccount { .. }
            | ServiceRequest::WorldReplica { request: WorldReplicaRequest::Subscribe { .. }, .. }
    ) {
        return Err("request is not a replica subscription".into());
    }
    let mut stream = begin(connection, request).await?;
    let response = response(&mut stream, request.operation_id()).await?;
    let ack: crate::network::ap::replication::SubscriptionAck = serde_json::from_value(
        response.data.ok_or("missing replica subscription acknowledgment")?,
    )?;
    ack.validate()?;
    Ok((stream, ack))
}

/// Send one actual delta and await its correlated durable acknowledgment.
/// The caller must run this outside local commit/render critical paths.
pub async fn publish_replica(
    connection: &ApConnection,
    request: &ServiceRequest,
    delta: &[u8],
) -> Result<crate::network::ap::replication::PublicationAck> {
    if !matches!(
        request,
        ServiceRequest::PublishReplica { .. }
            | ServiceRequest::PublishAccount { .. }
            | ServiceRequest::WorldReplica { request: WorldReplicaRequest::Publish { .. }, .. }
    ) {
        return Err("request is not a replica publication".into());
    }
    crate::network::ap::replication::validate_delta(delta)?;
    if matches!(request, ServiceRequest::WorldReplica { .. }) {
        require_features(connection, service::required_features(request)).await?;
    }
    publish_payload(connection, request, delta).await
}

/// Stream admission failed before any publication request could be sent.
#[derive(Debug)]
pub struct PublicationNotSent;
impl std::fmt::Display for PublicationNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("publication connection is unavailable")
    }
}
impl std::error::Error for PublicationNotSent {}

async fn publish_payload(
    connection: &ApConnection,
    request: &ServiceRequest,
    delta: &[u8],
) -> Result<crate::network::ap::replication::PublicationAck> {
    let frame = service::request(request)?;
    // Covers stream admission and flow-control-blocked request/payload writes,
    // as well as the response. This is an operation deadline, never a poll.
    let bloblog = matches!(request, ServiceRequest::PublishBloblog { .. });
    let mut submitted = false;
    let result = tokio::time::timeout(connection.response_timeout(), async {
        let mut publication = {
            let _timing = crate::trace::Interval::new("ap.request_send");
            let mut publication = PublicationStream {
                stream: connection.open_stream().await.map_err(
                    |error| -> Box<dyn std::error::Error + Send + Sync> {
                        if bloblog { PublicationNotSent.into() } else { error.into() }
                    },
                )?,
                complete: false,
            };
            submitted = true;
            publication.stream.send(&frame).await?;
            publication
        };
        send_data(&mut publication.stream, delta).await?;
        publication.stream.finish()?;
        let response = {
            let _timing = crate::trace::Interval::new("ap.response_wait");
            // A receipt/error is bounded control data, not another delta. The
            // enclosing deadline covers this read; reject size from the header.
            let frame = publication.stream.receive_with_body_limit(4096).await?;
            service::response(frame, request.operation_id())?
        };
        let ack: crate::network::ap::replication::PublicationAck = serde_json::from_value(
            response.data.ok_or("missing durable publication acknowledgment")?,
        )?;
        ack.validate()?;
        publication.complete = true;
        Ok(ack)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) if bloblog && !submitted => Err(PublicationNotSent.into()),
        Err(_) => Err(crate::network::ap::ClientError::ResponseTimeout.into()),
    }
}

struct PublicationStream {
    stream: ApStream,
    complete: bool,
}
impl Drop for PublicationStream {
    fn drop(&mut self) {
        if !self.complete {
            self.stream.cancel();
        }
    }
}

/// CaughtUp arrives in ordinary stream order after earlier publications.
pub async fn replica_checkpoint(stream: &mut ApSendStream) -> Result<()> {
    stream
        .send(&Frame::new(
            frame_type::DIRECT,
            0,
            [],
            SERVICE_DESTINATION,
            crate::network::ap::replication::CHECKPOINT,
        )?)
        .await?;
    Ok(())
}

/// Receive one bounded event; the caller borrows it with ReplicaEvent::decode.
/// An idle subscription uses the existing unsolicited-read/liveness contract.
pub async fn receive_replica_event(stream: &mut ApReceiveStream) -> Result<Vec<u8>> {
    let bytes = service::data(
        stream.receive_with_body_limit(crate::network::ap::replication::MAX_EVENT_BYTES).await?,
    )?;
    crate::network::ap::replication::ReplicaEvent::decode(&bytes)?;
    Ok(bytes)
}

/// Opaque bloblog batch on the existing volume publication transport.
pub async fn publish_bloblog(
    connection: &ApConnection,
    request: &ServiceRequest,
    bytes: &[u8],
) -> Result<crate::network::ap::replication::PublicationAck> {
    if !matches!(request, ServiceRequest::PublishBloblog { .. })
        || !(4..=65536).contains(&bytes.len())
    {
        return Err("invalid bloblog publication".into());
    }
    publish_payload(connection, request, bytes).await
}
