//! Background operations for session sharing and app shutdown.
use crate::engine::{Request, ipc, reply::Reply};
use std::path::Path;

pub(super) fn stop(profile: &Path, session: String) -> crate::engine::Result<()> {
    let Reply::InstanceOwners(owners) =
        ipc::call_archive(profile, &Request::InstanceList { session })?.into_owned()?
    else {
        return Err("Invalid app owner response".into());
    };
    let mut owners = owners.into_iter().filter(|o| {
        o.process.is_some() && o.instance.state == crate::engine::session_instance::State::Owned
    });
    let owner = owners.next().ok_or("This session is no longer running")?;
    if owners.next().is_some() {
        return Err("Several instances are running; stop the app from its own window.".into());
    }
    stop_owner(profile, &owner)
}

fn stop_owner(
    profile: &Path,
    owner: &crate::engine::session_instance::LaunchOwner,
) -> crate::engine::Result<()> {
    match crate::ui::app_control::Exchange::connect(
        profile,
        owner,
        crate::ui::app_control::Operation::Quit,
    )?
    .receive()?
    {
        crate::ui::app_control::Reply::Accepted { .. } => Ok(()),
        crate::ui::app_control::Reply::Error { error } => Err(error.into()),
        _ => Err("The app did not acknowledge Stop".into()),
    }
}

pub(super) fn stop_instance(
    profile: &Path,
    session: String,
    instance: crate::store::Id,
    owner_generation: u64,
) -> crate::engine::Result<()> {
    let owner = instance_owner(profile, session, instance, owner_generation)?;
    stop_owner(profile, &owner)
}

pub(super) fn show_instance(
    profile: &Path,
    session: String,
    instance: crate::store::Id,
    owner_generation: u64,
) -> crate::engine::Result<()> {
    let owner = instance_owner(profile, session, instance, owner_generation)?;
    let Reply::InstanceStatus { incarnation, .. } =
        ipc::call_archive(profile, &Request::InstanceInspect { request: None })?.into_owned()?
    else {
        return Err("The local instance registry is unavailable".into());
    };
    super::guest_app::activate(profile, incarnation, &owner).map(|_| ())
}

fn instance_owner(
    profile: &Path,
    session: String,
    instance: crate::store::Id,
    owner_generation: u64,
) -> crate::engine::Result<crate::engine::session_instance::LaunchOwner> {
    let Reply::InstanceOwners(owners) =
        ipc::call_archive(profile, &Request::InstanceList { session })?.into_owned()?
    else {
        return Err("The local instance list is unavailable".into());
    };
    owners
        .into_iter()
        .find(|owner| {
            owner.instance.id == instance
                && owner.instance.owner_generation == owner_generation
                && owner.process.is_some()
        })
        .ok_or_else(|| {
            "This local instance changed or stopped. Select it again before controlling it.".into()
        })
}

pub(super) fn invitation(profile: &Path, session: String) -> crate::engine::Result<String> {
    match ipc::call_archive(profile, &Request::Invitation { session })?.into_owned()? {
        Reply::Invitation { invitation, .. } => Ok(invitation),
        Reply::Error { error } => Err(error.into()),
        _ => Err("Session invitation unavailable".into()),
    }
}
