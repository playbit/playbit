//! Human session references at local tool boundaries. Internal IPC and remote
//! protocols retain the public identity; only explicit output exports the code.
use super::{Request, Result, reply::Reply};
use crate::{release_channel::ReleaseChannel, store::SessionReference};
use serde_json::Value;
mod output;
mod requests;
mod status;
pub use requests::normalize_request;

pub fn session_argument(value: &str, channel: ReleaseChannel) -> Result<String> {
    Ok(SessionReference::from_link(value, channel)?.internal_id().to_owned())
}

pub fn reference_from_reply(reply: Reply, channel: ReleaseChannel) -> Result<SessionReference> {
    if let Reply::SessionReference { reference } = reply {
        reference.validate()?;
        return Ok(reference);
    }
    let Reply::Invitation { invitation, .. } = reply else {
        return Err("Unexpected session reference reply".into());
    };
    Ok(SessionReference::from_link(&invitation, channel)?)
}

pub fn reply_value(
    reply: &Reply,
    channel: ReleaseChannel,
    mut resolve: impl FnMut(&str) -> Result<SessionReference>,
) -> Result<Value> {
    let mut value = serde_json::to_value(reply)?;
    output::project(reply, &mut value, channel, &mut resolve)?;
    Ok(value)
}

pub fn session_value(
    session: &crate::store::Session,
    reference: &SessionReference,
) -> Result<Value> {
    reference.validate()?;
    if reference.internal_id() != session.id {
        return Err("Session presentation belongs to another session".into());
    }
    let mut value = serde_json::to_value(session)?;
    let fields = value.as_object_mut().ok_or("Session metadata is not an object")?;
    fields.insert("id".into(), reference.code().into());
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_invitation_status_projects_its_code_without_admitting_the_session() {
        let directory = tempfile::tempdir().expect("profile directory");
        let mut engine = crate::engine::Engine::open(directory.path()).expect("engine");
        let channel = engine.channel();
        let reference = SessionReference::new([0x4a; 16]).expect("reference");
        engine
            .dispatch_reply(Request::PreviewInvitation { invitation: reference.link(channel) })
            .expect("retain pending invitation");
        for paused in [false, true] {
            engine
                .services
                .store
                .pause_invitation(reference.internal_id(), paused)
                .expect("set pause");
            let status = engine.dispatch_reply(Request::Status).expect("status snapshot");
            let value = reply_value(&status, channel, |id| {
                reference_from_reply(
                    engine.dispatch_reply(Request::SessionReference { session: id.into() })?,
                    channel,
                )
            })
            .expect("present pending invitation");
            assert_eq!(value["invitations"][0]["session"], reference.code());
            assert_eq!(value["invitations"][0]["paused"], paused);
            assert_eq!(value["invitations"][0]["retained"], false);
            assert!(value["invitations"][0]["received"].is_null());
        }
        let store = &engine.services.store;
        assert!(store.sessions().expect("sessions").is_empty());
        assert!(store.volumes().expect("volumes").is_empty());
        assert!(store.session_seed(reference.internal_id()).is_err());
        assert!(store.session_proof(reference.internal_id(), store.account().id).is_err());
        let unknown = SessionReference::new([0x4b; 16]).expect("unknown reference");
        assert!(store.session_reference(unknown.internal_id()).is_err());
    }
}
