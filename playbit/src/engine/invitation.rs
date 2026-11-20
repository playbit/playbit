//! One selected invitation, delivered by the existing AP runtime and owner wake.
// W7 coverage decision: Watch owns the stream and bounded result mailbox;
// Engine retains the failed preview. Drop cancels observation, not durable pause.
// Restart/pause tests cover that owner; each transport/serialization failure
// variant is not separately injected. A canceled recipient receives no retry.
use super::completion::Wake;
use super::reply::{InvitationState, InvitationStatus, Reply};
use crate::engine::{Engine, Result};
use crate::network::ap::{ApConnection, operation::ServiceRequest, service_client};
use crate::store::{Id, Session};
use std::sync::Arc;

const WATCH_LIMIT: usize = 1;

pub(crate) struct Invitation {
    session: String,
    state: InvitationState,
    watch: Option<Watch>,
}
enum Event {
    State(InvitationState),
    Envelope { envelope: String, registered: Session },
}
pub(crate) struct Watch {
    connection: Arc<ApConnection>,
    receive: tokio::sync::mpsc::Receiver<std::result::Result<Event, String>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Watch {
    /// Readiness frames, plus the sealed envelope and authenticated session
    /// registration read together once the creator's record exists.
    pub(crate) fn start(
        runtime: &tokio::runtime::Handle,
        connection: Arc<ApConnection>,
        session: String,
        proof: Vec<u8>,
        envelope: bool,
        completion: Arc<Wake>,
    ) -> Result<Self> {
        let request = ServiceRequest::WatchInvitation {
            operation_id: Id::generate()?.to_string(),
            session_id: session.clone(),
            proof: proof.clone(),
        };
        let (send, receive) = tokio::sync::mpsc::channel(WATCH_LIMIT);
        let observed = connection.clone();
        let task = runtime.spawn(async move {
            let result = async {
                let mut stream = service_client::watch_invitation(&connection, &request).await?;
                let mut envelope = envelope;
                loop {
                    let state = service_client::receive_invitation(&mut stream).await?;
                    let registered = !matches!(state, crate::network::ap::operation::InvitationStatus::WaitingRegistration);
                    assert!(send.capacity() <= WATCH_LIMIT);
                    if send.send(Ok(Event::State(state.into()))).await.is_err() { return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(()); }
                    completion.notify();
                    if envelope && registered {
                        let read = ServiceRequest::ReadInvitation { operation_id: Id::generate()?.to_string(), session_id: session.clone(), proof: proof.clone() };
                        // A failed read is shown as a failure with its own retry:
                        // after a final Ready no further readiness change may
                        // arrive to retry it. A later change still retries it.
                        let message = match service_client::request(&connection, &read).await
                            .map_err(|error| error.to_string())
                            .and_then(|response| response.data.ok_or_else(|| "missing invitation envelope".into())) {
                            Ok(data) => match (data["envelope"].as_str(), serde_json::from_value::<Session>(data["session"].clone())) {
                                (Some(text), Ok(registered)) => { envelope = false; Event::Envelope { envelope: text.into(), registered } }
                                _ => Event::State(InvitationState::Failed { message: "The invitation could not be read: missing or invalid invitation registration".into() }),
                            },
                            Err(error) => Event::State(InvitationState::Failed { message: format!("The invitation could not be read: {error}") }),
                        };
                        assert!(send.capacity() <= WATCH_LIMIT);
                        if send.send(Ok(message)).await.is_err() { return Ok(()); }
                        completion.notify();
                    }
                }
            }.await;
            if let Err(error) = result {
                let message = error.to_string();
                let mut end = message.len().min(512);
                while !message.is_char_boundary(end) { end -= 1; }
                assert!(send.capacity() <= WATCH_LIMIT);
                if send.send(Err(message[..end].into())).await.is_ok() { completion.notify(); }
            }
        });
        Ok(Self { connection: observed, receive, task })
    }
}

impl Engine {
    pub(crate) fn watch_invitation(&mut self, session: String, resume: bool) -> Result<Reply> {
        let _timing = crate::trace::Span::new("invitation.observe_local");
        let paused = self.services.store.pending_invitation_paused(&session)?;
        if resume {
            self.services.store.pause_invitation(&session, false)?;
        }
        if !resume
            && self.invitation.as_ref().is_some_and(|current| {
                current.session == session
                    && current.watch.is_some()
                    && !matches!(current.state, InvitationState::Failed { .. })
            })
        {
            return Ok(Reply::InvitationStatus { invitation_status: self.invitation_status() });
        }
        let state = if paused && !resume {
            InvitationState::Paused
        } else if self.enrolled() {
            InvitationState::Checking
        } else {
            InvitationState::SignIn
        };
        let checking = state == InvitationState::Checking;
        self.invitation = Some(Invitation { session, state, watch: None });
        if checking && self.remote.as_ref().is_some_and(|remote| !remote.sync_connected()) {
            self.bootstrap_pending = true;
        }
        self.resume_invitation_watch()?;
        self.state_changed();
        Ok(Reply::InvitationStatus { invitation_status: self.invitation_status() })
    }

    pub(crate) fn invitation_status(&self) -> Option<InvitationStatus> {
        self.invitation.as_ref().map(|invitation| InvitationStatus {
            session: invitation.session.clone(),
            state: invitation.state.clone(),
        })
    }

    pub(crate) fn stop_invitation_watch(&mut self, session: &str) {
        if self.invitation.as_ref().is_some_and(|current| current.session == session) {
            self.invitation = None;
        }
    }

    pub(crate) fn resume_invitation_watch(&mut self) -> Result<()> {
        let Some(invitation) = &mut self.invitation else {
            return Ok(());
        };
        if invitation.state == InvitationState::Paused {
            return Ok(());
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        if !remote.sync_connected() {
            return Ok(());
        }
        if invitation
            .watch
            .as_ref()
            .is_some_and(|watch| remote.has_invitation_connection(&watch.connection))
        {
            return Ok(());
        }
        let proof = self
            .services
            .store
            .pending_invitation_proof(&invitation.session, self.services.store.account().id)?;
        let received =
            self.services.store.pending_invitation_preview(&invitation.session)?.received.is_some();
        invitation.watch =
            Some(remote.start_invitation_watch(invitation.session.clone(), proof, !received)?);
        invitation.state = InvitationState::Checking;
        self.state_changed();
        Ok(())
    }

    pub(crate) fn collect_invitation(&mut self) {
        let _timing = crate::trace::Span::new("invitation.apply_state");
        let Some(invitation) = &mut self.invitation else {
            return;
        };
        let Some(watch) = &mut invitation.watch else {
            return;
        };
        assert!(watch.receive.len() <= WATCH_LIMIT);
        let Ok(result) = watch.receive.try_recv() else {
            return;
        };
        let session = invitation.session.clone();
        let state = match result {
            Ok(Event::Envelope { envelope, registered }) => {
                crate::store::parse_envelope_text(&envelope)
                    .and_then(|envelope| {
                        self.services.store.check_invitation_registration(
                            &session,
                            &envelope,
                            &registered,
                        )?;
                        self.services.store.receive_invitation(&session, &envelope)
                    })
                    .err()
                    .map(|error| InvitationState::Failed { message: error.to_string() })
            }
            Ok(Event::State(state)) => Some(state),
            Err(message) => Some(InvitationState::Failed { message }),
        };
        if let (Some(state), Some(invitation)) = (state, &mut self.invitation) {
            invitation.state = state;
        }
        self.state_changed();
    }

    /// Read the creator's envelope now when the pending link has none yet.
    pub(crate) fn receive_pending_invitation(&mut self, session: &str) -> Result<()> {
        if self.services.store.pending_invitation_preview(session)?.received.is_some() {
            return Ok(());
        }
        let remote = self.remote.as_mut().ok_or("A network operation is in progress")?;
        let envelope = remote.read_invitation(&self.services.store, session)?;
        self.services.store.receive_invitation(session, &envelope)?;
        Ok(())
    }

    pub(crate) fn pause_invitation(&mut self, session: &str) -> Result<Reply> {
        let _timing = crate::trace::Span::new("invitation.pause_local");
        // A failed durable write is never acknowledged as paused.
        self.services.store.pause_invitation(session, true)?;
        self.cancel_invitation_transfer(session)?;
        if let Some(invitation) = &mut self.invitation
            && invitation.session == session
        {
            invitation.watch = None;
            invitation.state = InvitationState::Paused;
        }
        self.state_changed();
        Ok(Reply::InvitationPaused { paused: true, invitation_status: self.invitation_status() })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::engine::Request;

    #[test]
    fn pause_survives_restart_and_resume_only_observes() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Shared", true).unwrap();
        let version = crate::identifier::encode(&[0x11; 32]);
        let session = engine
            .services
            .store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: engine.services.store.account().id,
                    app_id: "test".into(),
                },
                crate::store::BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let url = engine
            .services
            .store
            .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        engine.dispatch_reply(Request::PreviewInvitation { invitation: url.clone() }).unwrap();
        assert_eq!(
            serde_json::to_value(
                engine
                    .dispatch_reply(Request::WatchInvitation { session: session.id.clone() })
                    .unwrap()
            )
            .unwrap()["invitation_status"]["state"]["kind"],
            "sign_in"
        );
        let before_pause = engine.change_token();
        assert_eq!(
            serde_json::to_value(
                engine
                    .dispatch_reply(Request::PauseInvitation { session: session.id.clone() })
                    .unwrap()
            )
            .unwrap()["paused"],
            true
        );
        assert_ne!(engine.change_token(), before_pause, "pause wakes the ordinary status refresh");
        drop(engine);
        let mut engine = Engine::open(directory.path()).unwrap();
        assert!(engine.services.store.pending_invitations().unwrap()[0].paused);
        assert_eq!(
            serde_json::to_value(
                engine
                    .dispatch_reply(Request::WatchInvitation { session: session.id.clone() })
                    .unwrap()
            )
            .unwrap()["invitation_status"]["state"]["kind"],
            "paused"
        );
        assert_eq!(
            serde_json::to_value(
                engine
                    .dispatch_reply(Request::ResumeInvitation { session: session.id.clone() })
                    .unwrap()
            )
            .unwrap()["invitation_status"]["state"]["kind"],
            "sign_in"
        );
        assert!(!engine.services.store.pending_invitations().unwrap()[0].paused);
        assert_eq!(
            engine
                .services
                .store
                .pending_invitation_preview(&session.id)
                .unwrap()
                .received
                .unwrap()
                .root_name,
            "Shared"
        );
        assert!(engine.background.is_none());
        assert!(engine.invitation.as_ref().unwrap().watch.is_none());
    }
}
