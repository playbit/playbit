//! Coalesced local snapshots keep storage and process work off the UI executor.
use super::*;

impl Shell {
    pub(super) fn refresh_application_scopes(&mut self, cx: &mut Context<Self>) {
        if self.applications.scope_refreshing
            || !self.applications.scope_refresh_needed
            || self.signing_out
        {
            return;
        }
        let Some(app) = self.applications.selected.clone() else {
            return;
        };
        let session = self.selected_session_id().map(str::to_owned);
        self.applications.scope_refresh_needed = false;
        self.applications.scope_refreshing = true;
        self.applications.scope_error = None;
        let profile = profile_path();
        let sender = self.send.clone();
        let account = self.account.id;
        let query_app = app.clone();
        let query_session = session.clone();
        let work = cx.background_executor().spawn(async move {
            let app = read_slots(&profile, Request::AppVolumes { app: query_app });
            let session = query_session
                .map(|session| read_slots(&profile, Request::AutomaticVolumes { session }));
            (app, session)
        });
        cx.spawn(async move |host, cx| {
            let (app_slots, session_slots) = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) || host.account.id != account
                {
                    return;
                }
                host.applications.scope_refreshing = false;
                if host.applications.selected.as_ref() == Some(&app)
                    && host.selected_session_id() == session.as_deref()
                {
                    host.applications.slots_app = Some(app);
                    host.applications.slots_session = session;
                    match app_slots {
                        Ok(slots) => host.applications.app_slots = slots,
                        Err(error) => {
                            host.applications.app_slots.clear();
                            host.applications.scope_error = Some(error.to_string());
                        }
                    }
                    match session_slots {
                        Some(Ok(slots)) => host.applications.session_slots = slots,
                        Some(Err(error)) => {
                            host.applications.session_slots.clear();
                            host.applications.scope_error = Some(error.to_string());
                        }
                        None => host.applications.session_slots.clear(),
                    }
                } else {
                    host.applications.scope_refresh_needed = true;
                }
                host.refresh_application_scopes(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::desktop) fn refresh_application_instances(&mut self, cx: &mut Context<Self>) {
        if self.applications.refreshing || !self.applications.refresh_needed || self.signing_out {
            return;
        }
        let Some(session) = self.selected_session_id().map(str::to_owned) else {
            self.applications.owners.clear();
            self.applications.owners_session = None;
            return;
        };
        if !self.applications.catalog.iter().any(|row| row.id == session) {
            return;
        }
        self.applications.refreshing = true;
        self.applications.refresh_needed = false;
        self.applications.instance_error = None;
        let profile = profile_path();
        let sender = self.send.clone();
        let account = self.account.id;
        let target = session.clone();
        let work = cx.background_executor().spawn(async move {
            let response =
                ipc::call_archive(&profile, &Request::InstanceList { session: target.clone() })?;
            let crate::engine::reply::ArchivedReply::InstanceOwners(owners) = response.archived()
            else {
                return Err("The local instance list is unavailable".into());
            };
            if owners.len() > crate::engine::session_instance::MAX_STARTUP_PROGRESS
                || owners.iter().any(|owner| {
                    owner.instance.session.len() > 44
                        || owner.instance.build.len() > 44
                        || owner.instance.extra_volumes.len()
                            > playbit_api::abi::syscall::MAX_VOLUMES
                        || owner.instance.host_mounts.len() > crate::engine::host_mount::MAX_MOUNTS
                        || owner.instance.host_mounts.iter().any(|mount| {
                            mount.path.len() > crate::engine::host_mount::MAX_PATH_BYTES
                                || mount.label.len() > crate::engine::host_mount::MAX_LABEL_BYTES
                        })
                })
            {
                return Err("The local instance list exceeds its display limits".into());
            }
            match response.into_owned()? {
                Reply::InstanceOwners(owners) => {
                    let mut ids = BTreeSet::new();
                    if owners.len() > crate::engine::session_instance::MAX_STARTUP_PROGRESS
                        || owners.iter().any(|owner| {
                            owner.instance.session != target
                                || owner.instance.account != account
                                || owner.instance.id == Id::from_bytes([0; 16])
                                || !ids.insert(owner.instance.id)
                                || !crate::identifier::valid(&owner.instance.build, 32)
                        })
                    {
                        return Err(
                            "The local instance list contains invalid owner bindings".into()
                        );
                    }
                    Ok(owners)
                }
                _ => Err("The local instance list is unavailable".into()),
            }
        });
        cx.spawn(async move |host, cx| {
            let result: crate::engine::Result<Vec<LaunchOwner>> = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) || host.account.id != account
                {
                    return;
                }
                host.applications.refreshing = false;
                if host.selected_session_id() == Some(session.as_str()) {
                    host.applications.owners_session = Some(session);
                    match result {
                        Ok(owners) => {
                            host.applications.owners = owners;
                            if host.applications.instance.is_some_and(|id| {
                                !host
                                    .applications
                                    .owners
                                    .iter()
                                    .any(|owner| owner.instance.id == id)
                            }) {
                                host.applications.instance = None;
                            }
                        }
                        Err(error) => {
                            host.applications.owners.clear();
                            host.applications.instance_error = Some(error.to_string());
                        }
                    }
                } else {
                    host.applications.refresh_needed = true;
                }
                host.refresh_application_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

fn read_slots(
    profile: &std::path::Path,
    request: Request,
) -> crate::engine::Result<Vec<AutomaticVolumeSlot>> {
    let expected: &[Role] = if matches!(&request, Request::AppVolumes { .. }) {
        &[Role::Root, Role::User, Role::Local]
    } else {
        &[Role::Root, Role::Session, Role::User, Role::UserSession, Role::Local, Role::LocalSession]
    };
    let response = ipc::call_archive(profile, &request)?;
    match response.archived() {
        crate::engine::reply::ArchivedReply::AutomaticVolumes { slots }
            if slots.len() == expected.len() => {}
        _ => {
            return Err(
                "The automatic volume slots are unavailable or exceed their role limit".into()
            );
        }
    }
    let Reply::AutomaticVolumes { slots } = response.into_owned()? else {
        return Err("The automatic volume snapshot is unavailable".into());
    };
    if slots.len() != expected.len()
        || slots.iter().any(|slot| {
            slot.overridden
                && (expected.len() == 3 || !matches!(slot.role, Role::Root | Role::User))
        })
        || !expected.iter().all(|role| {
            slots
                .iter()
                .filter(|slot| slot.role == *role && slot.volume != Id::from_bytes([0; 16]))
                .count()
                == 1
        })
    {
        return Err("The automatic volume snapshot contains invalid role bindings".into());
    }
    Ok(slots)
}
