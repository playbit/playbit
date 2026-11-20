use super::*;

/// Apply only to human JSON requests before IPC admission. Native owners and
/// AP codecs never use this adapter, so a join key cannot become wire identity.
pub fn normalize_request(request: &mut Request, channel: ReleaseChannel) -> Result<()> {
    let session = match request {
        Request::InstanceList { session }
        | Request::InstanceConfigurations { session }
        | Request::InstanceReserve { session, .. }
        | Request::ForgetSession { session }
        | Request::StartJoin { session }
        | Request::PrepareSession { session }
        | Request::RetrySessionInfo { session }
        | Request::WatchInvitation { session }
        | Request::ResumeInvitation { session }
        | Request::PauseInvitation { session }
        | Request::UpdateSession { session, .. }
        | Request::AutomaticVolumes { session }
        | Request::OpenAutomaticVolume { session, .. }
        | Request::PersonalVolumes { session }
        | Request::SelectPersonalVolumes { session, .. }
        | Request::AddPersonalVolumes { session, .. }
        | Request::CheckUpdates { session }
        | Request::SessionReference { session }
        | Request::Invitation { session } => Some(session),
        Request::FileRead { session, .. } => session.as_mut(),
        Request::ToolContextOpen { scope: super::super::service_call::ToolScope::Session(id) } => {
            Some(id)
        }
        Request::OrderSessions { sessions } => {
            if sessions.len() > 256 {
                return Err("Session order exceeds its display limit".into());
            }
            for session in sessions {
                *session = session_argument(session, channel)?;
            }
            None
        }
        _ => None,
    };
    if let Some(session) = session {
        *session = session_argument(session, channel)?;
    }
    Ok(())
}
