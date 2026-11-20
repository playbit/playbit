use super::*;

pub(super) fn project(
    status: &super::super::reply::Status,
    value: &mut Value,
    fallback: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    let mut resolve = |id: &str| -> Result<SessionReference> {
        status.session_references.get(id).map_or_else(
            || fallback(id),
            |code| {
                Ok(SessionReference::from_link(
                    code,
                    crate::release_channel::ReleaseChannel::Public,
                )?)
            },
        )
    };
    value["sessions"] = status
        .sessions
        .iter()
        .map(|session| session_value(session, &resolve(&session.id)?))
        .collect::<Result<Vec<_>>>()?
        .into();
    value["recent_sessions"] = status
        .recent_sessions
        .iter()
        .map(|id| resolve(id).map(|reference| Value::from(reference.code())))
        .collect::<Result<Vec<_>>>()?
        .into();
    project_maps(status, value, &mut resolve)?;
    for (preview, row) in status
        .invitations
        .iter()
        .zip(value["invitations"].as_array_mut().ok_or("Invitations are not an array")?)
    {
        super::output::preview_value(preview, row, &mut resolve)?;
    }
    project_progress(status, value, &mut resolve)?;
    Ok(())
}

fn project_progress(
    status: &super::super::reply::Status,
    value: &mut Value,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    for (progress, row) in
        status.startup.iter().zip(value["startup"].as_array_mut().ok_or("Startup is not an array")?)
    {
        super::output::set_session(row, "session", &progress.session, resolve)?;
    }
    if let Some(session) = &status.transfer.session {
        super::output::set_session(&mut value["transfer"], "session", session, resolve)?;
    }
    if let Some(invitation) = &status.invitation_status {
        super::output::set_session(
            &mut value["invitation_status"],
            "session",
            &invitation.session,
            resolve,
        )?;
    }
    for (operation, row) in status
        .pending
        .iter()
        .zip(value["pending"].as_array_mut().ok_or("Pending operations are not an array")?)
    {
        if let crate::store::PendingKind::RegisterSession { session, .. } = &operation.operation {
            row["operation"]["session"] = session_value(session, &resolve(&session.id)?)?;
        }
    }
    Ok(())
}

fn project_maps(
    status: &super::super::reply::Status,
    value: &mut Value,
    resolve: &mut impl FnMut(&str) -> Result<SessionReference>,
) -> Result<()> {
    for (field, source) in [
        ("app_names", serde_json::to_value(&status.app_names)?),
        ("updates", serde_json::to_value(&status.updates)?),
    ] {
        let mut map = serde_json::Map::new();
        for (id, entry) in source.as_object().ok_or("Session map is not an object")? {
            map.insert(resolve(id)?.code(), entry.clone());
        }
        value[field] = map.into();
    }
    Ok(())
}
