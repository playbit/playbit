use super::*;

pub(super) fn project(
    reply: &Reply,
    value: &mut Value,
    channel: ReleaseChannel,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    use super::super::reply::DeveloperReply;
    match reply {
        Reply::SessionReference { reference } => {
            *value = reference.code().into();
        }
        Reply::Status(status) => super::status::project(status, value, resolve)?,
        Reply::Session(session) | Reply::Developer(DeveloperReply::Session(session)) => {
            *value = session_value(session, &resolve(&session.id)?)?;
        }
        Reply::SessionPrepared(prepared) => {
            let reference = SessionReference::from_link(&prepared.invitation, channel)?;
            value["session"] = session_value(&prepared.session, &reference)?;
        }
        Reply::Developer(DeveloperReply::Updated { session, .. })
        | Reply::PersonalVolumes { session, .. } => {
            set_session(value, "session", session, resolve)?
        }
        Reply::ForgetSession { forgotten } => set_session(value, "forgotten", forgotten, resolve)?,
        Reply::Transfer { transfer } => {
            if let Some(session) = &transfer.session {
                set_session(&mut value["transfer"], "session", session, resolve)?;
            }
        }
        Reply::InvitationPreview(preview) => preview_value(preview, value, resolve)?,
        Reply::InvitationStatus { invitation_status }
        | Reply::InvitationPaused { invitation_status, .. } => {
            if let Some(status) = invitation_status {
                set_session(&mut value["invitation_status"], "session", &status.session, resolve)?;
            }
        }
        Reply::Instance(instance) => instance_value(instance, value, resolve)?,
        Reply::InstanceStatus { instance: Some(instance), .. } => {
            instance_value(instance, &mut value["instance"], resolve)?;
        }
        Reply::InstanceOwners(owners) => {
            let rows = value.as_array_mut().ok_or("Instance owners are not an array")?;
            for (owner, row) in owners.iter().zip(rows) {
                instance_value(&owner.instance, &mut row["instance"], resolve)?;
            }
        }
        Reply::InstanceAdmission(admission) => {
            instance_value(&admission.instance, &mut value["instance"], resolve)?;
            value["session"] = session_value(&admission.session, &resolve(&admission.session.id)?)?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn set_session(
    value: &mut Value,
    field: &str,
    session: &str,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    let reference = resolve(session)?;
    if reference.internal_id() != session {
        return Err("Session presentation belongs to another session".into());
    }
    value[field] = reference.code().into();
    Ok(())
}

pub(super) fn instance_value(
    instance: &super::super::session_instance::SessionInstance,
    value: &mut Value,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    set_session(value, "session", &instance.session, resolve)?;
    Ok(())
}

pub(super) fn preview_value(
    preview: &crate::store::InvitationPreview,
    value: &mut Value,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    let reference = resolve(&preview.session)?;
    if reference.internal_id() != preview.session {
        return Err("Invitation presentation belongs to another session".into());
    }
    value["session"] = reference.code().into();
    if let Some(received) = &preview.received {
        value["received"]["session"] = session_value(&received.session, &reference)?;
    }
    Ok(())
}
