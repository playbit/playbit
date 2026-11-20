//! Selection and exact-instance actions preserve durable bindings and launch ownership.
use super::*;

impl Shell {
    pub(super) fn select_application_target(
        &mut self,
        target: Target,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(&target, Target::Scope { .. }) {
            self.applications.pending_scope = None;
        }
        match target {
            Target::Application(identity) => {
                if !self.prepare_browser_navigation(cx) {
                    return false;
                }
                self.applications.selected = Some(identity);
                self.applications.instance = None;
                self.applications.root_open = false;
                self.applications.selected_role = None;
                self.applications.log_open = false;
                self.sidebar_volumes = false;
                self.sidebar_session = None;
                self.selected = None;
                self.browser = None;
                self.new_session = None;
                self.personal = None;
                self.invitation = None;
                self.opening = None;
                self.refresh_logs(cx);
                self.applications.scope_refresh_needed = true;
                self.refresh_application_scopes(cx);
            }
            Target::Session(id) => {
                return self.select_sidebar_session(id, cx);
            }
            Target::Instance(id) => {
                if !self.prepare_browser_navigation(cx) {
                    return false;
                }
                self.applications.instance = Some(id);
                self.applications.root_open = false;
                self.applications.selected_role = None;
                self.applications.log_open = false;
                self.browser = None;
            }
            Target::Scope { role, volume, session } => {
                let slots = if session {
                    (self.applications.slots_session.as_deref() == self.selected_session_id())
                        .then_some(&self.applications.session_slots)
                } else {
                    (self.applications.slots_app.as_ref() == self.applications.selected.as_ref())
                        .then_some(&self.applications.app_slots)
                };
                let Some(slot) = slots
                    .and_then(|slots| {
                        slots.iter().find(|slot| slot.role == role && slot.volume == volume)
                    })
                    .cloned()
                else {
                    return false;
                };
                if slot.state == AutomaticVolumeState::Unavailable {
                    return false;
                }
                let ready = (slot.state == AutomaticVolumeState::Ready)
                    .then(|| self.catalog_volume(volume).cloned())
                    .flatten();
                let Some(volume) = ready else {
                    return self.open_application_scope(role, volume, session, cx);
                };
                if !self.select_volume(volume, cx) {
                    return false;
                }
                self.applications.pending_scope = None;
                self.sidebar_volumes = false;
                self.applications.root_open = true;
                self.applications.selected_role = Some(role);
                self.applications.scope_in_session = session;
                self.applications.log_open = false;
                self.applications.instance = None;
            }
            Target::Log => {
                if !self.prepare_browser_navigation(cx) {
                    return false;
                }
                self.browser = None;
                self.applications.root_open = false;
                self.applications.selected_role = None;
                self.applications.log_open = true;
                self.refresh_logs(cx);
            }
        }
        cx.notify();
        true
    }

    fn open_application_scope(
        &mut self,
        role: Role,
        volume: Id,
        in_session: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.applications.action_busy || !self.prepare_browser_navigation(cx) {
            return false;
        }
        let Some(app) = self.applications.selected.clone() else {
            return false;
        };
        let session = self.selected_session_id().map(str::to_owned);
        let request = if in_session {
            let Some(session) = session.clone() else {
                return false;
            };
            Request::OpenAutomaticVolume { session, role }
        } else {
            Request::OpenAppVolume { app: app.clone(), role }
        };
        self.applications.action_busy = true;
        self.applications.pending_scope = Some((role, volume, in_session));
        let profile = profile_path();
        let account = self.account.id;
        let sender = self.send.clone();
        let work = cx.background_executor().spawn(async move {
            let response = ipc::call_archive(&profile, &request)?;
            if let crate::engine::reply::ArchivedReply::Volume(opened) = response.archived()
                && opened.name.len() > 256
            {
                return Err("The opened volume metadata exceeds its display limits".into());
            }
            match response.into_owned()? {
                Reply::Volume(opened)
                    if opened.id == volume
                        && !opened.name.is_empty()
                        && opened.name.len() <= 256 =>
                {
                    Ok(opened)
                }
                Reply::Error { error } => Err(error.into()),
                _ => Err("The selected automatic volume could not be opened".into()),
            }
        });
        cx.spawn(async move |host, cx| {
            let result: crate::engine::Result<crate::store::Volume> = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) || host.account.id != account
                {
                    return;
                }
                host.applications.action_busy = false;
                let current = host.applications.pending_scope == Some((role, volume, in_session))
                    && host.applications.selected.as_ref() == Some(&app)
                    && host.selected_session_id() == session.as_deref();
                if current {
                    host.applications.pending_scope = None;
                }
                match result {
                    Ok(opened) if current => {
                        if host.select_volume(opened, cx) {
                            host.sidebar_volumes = false;
                            host.applications.root_open = true;
                            host.applications.selected_role = Some(role);
                            host.applications.scope_in_session = in_session;
                            host.applications.log_open = false;
                            host.applications.instance = None;
                            let slots = if in_session {
                                &mut host.applications.session_slots
                            } else {
                                &mut host.applications.app_slots
                            };
                            if let Some(slot) = slots
                                .iter_mut()
                                .find(|slot| slot.role == role && slot.volume == volume)
                            {
                                slot.state = AutomaticVolumeState::Ready;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => host.error = error.to_string(),
                }
                host.applications.scope_refresh_needed = true;
                host.refresh_application_scopes(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        true
    }

    pub(super) fn application_running_owners(&self) -> impl Iterator<Item = &LaunchOwner> {
        self.applications.owners.iter().filter(|owner| {
            Some(owner.instance.session.as_str()) == self.selected_session_id() && is_running(owner)
        })
    }

    pub(super) fn application_stop_target(&self) -> Option<Id> {
        if let Some(id) = self.applications.instance {
            return self
                .application_running_owners()
                .find(|owner| owner.instance.id == id)
                .map(|_| id);
        }
        let mut owners = self.application_running_owners();
        let owner = owners.next()?;
        owners.next().is_none().then_some(owner.instance.id)
    }

    pub(super) fn toggle_application(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.selected_session_id().map(str::to_owned) {
            if let Some(instance) = self.application_stop_target() {
                self.stop_application_instance(session, instance, cx);
            } else if self.application_running_owners().count() > 1 {
                self.show_visual_bell("Select the instance to stop", "app-browser-instance", cx);
            } else {
                self.open_retained_session(session, cx);
            }
            return;
        }
        let app = self
            .applications
            .apps
            .iter()
            .find(|app| Some(&app.identity) == self.applications.selected.as_ref())
            .cloned();
        if let Some(app) = app {
            if let Some(session) = app.sessions.first() {
                self.open_retained_session(session.clone(), cx);
            } else {
                self.start_imported_application(app, cx);
            }
        }
    }

    pub(super) fn start_imported_application(&mut self, app: Application, cx: &mut Context<Self>) {
        if self.applications.action_busy
            || self.applications.uncertain_app.as_ref() == Some(&app.identity)
        {
            return;
        }
        self.applications.action_busy = true;
        let profile = profile_path();
        let account = self.account.id;
        let sender = self.send.clone();
        let identity = app.identity.clone();
        let build = app.build.clone();
        let work = cx.background_executor().spawn(async move {
            let mut created = None;
            let mut uncertain = false;
            let result: crate::engine::Result<_> = (|| {
                let response = match ipc::call_archive(
                    &profile,
                    &Request::CreateBuildSession { build: build.clone(), root: None, user: None },
                ) {
                    Ok(response) => response,
                    Err(error) => {
                        uncertain = true;
                        return Err(error);
                    }
                };
                let response = match response.into_owned() {
                    Ok(response) => response,
                    Err(error) => {
                        uncertain = true;
                        return Err(error);
                    }
                };
                let session = match response {
                    Reply::Developer(crate::engine::reply::DeveloperReply::Session(session)) => {
                        session
                    }
                    Reply::Error { error } => return Err(error.into()),
                    _ => {
                        uncertain = true;
                        return Err("The session creation result needs inspection".into());
                    }
                };
                if session.app != identity || session.resolved_build_id != build {
                    return Err("The created session differs from the selected app".into());
                }
                created = Some(session.clone());
                let prepared = match ipc::call_archive(
                    &profile,
                    &Request::PrepareSession { session: session.id.clone() },
                )?
                .into_owned()?
                {
                    Reply::SessionPrepared(prepared)
                        if prepared.session.same_identity(&session) =>
                    {
                        prepared
                    }
                    Reply::Error { error } => return Err(error.into()),
                    _ => return Err("The created session could not be prepared".into()),
                };
                let instance = guest_app::launch_confirmed(
                    &profile,
                    &prepared.session,
                    false,
                    true,
                    Vec::new(),
                    true,
                )?;
                Ok((prepared, instance))
            })();
            (created, uncertain, result)
        });
        cx.spawn(async move |host, cx| {
            let (created, uncertain, result) = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) || host.account.id != account { return; }
                host.applications.action_busy = false;
                let select = host.applications.selected.as_ref() == Some(&app.identity) && host.selected_session_id().is_none();
                if let Some(session) = created {
                    if !host.sessions.iter().any(|row| row.id == session.id) && host.sessions.len() < MAX_SESSIONS {
                        host.sessions.push(SessionRow { reference: String::new(), id: session.id.clone(), resolved_build_id: session.resolved_build_id.clone(), app_name: Some(app.name.clone()), build_state: crate::engine::builds::BuildState::Ready });
                    }
                    if !host.applications.catalog.iter().any(|row| row.id == session.id) && host.applications.catalog.len() < MAX_SESSIONS { host.applications.catalog.push(session.clone()); }
                    if select { host.sidebar_session = Some(session.id); }
                    host.applications.rebuild(&host.sessions, &host.recent_sessions);
                    if select { host.select_application_session(cx); }
                }
                match result {
                    Ok((prepared, instance)) if select => {
                        host.selected = Some(prepared);
                        host.applications.instance = Some(instance.instance);
                    }
                    Ok(_) => {}
                    Err(error) => host.error = if uncertain {
                        host.applications.uncertain_app = Some(app.identity);
                        format!("The session may have been created. Check retained sessions before opening again. {error}")
                    } else { error.to_string() },
                }
                host.applications.refresh_needed = true;
                host.refresh_application_instances(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    pub(super) fn create_application_instance(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self
            .selected_session_id()
            .and_then(|id| self.applications.catalog.iter().find(|session| session.id == id))
            .cloned()
        else {
            return;
        };
        self.application_instance_action(cx, move |profile| {
            guest_app::launch_confirmed(profile, &session, true, true, Vec::new(), true)
                .map(|receipt| Some(receipt.instance))
        });
    }

    pub(super) fn show_application_instance(
        &mut self,
        session: String,
        instance: Id,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self
            .applications
            .owners
            .iter()
            .find(|owner| owner.instance.id == instance && owner.instance.session == session)
            .map(|owner| owner.instance.owner_generation)
        else {
            return;
        };
        self.application_instance_action(cx, move |profile| {
            session::show_instance(profile, session, instance, generation).map(|()| Some(instance))
        });
    }

    pub(super) fn stop_application_instance(
        &mut self,
        session: String,
        instance: Id,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self
            .applications
            .owners
            .iter()
            .find(|owner| owner.instance.id == instance && owner.instance.session == session)
            .map(|owner| owner.instance.owner_generation)
        else {
            return;
        };
        self.application_instance_action(cx, move |profile| {
            session::stop_instance(profile, session, instance, generation).map(|()| None)
        });
    }

    pub(super) fn application_instance_action(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&std::path::Path) -> crate::engine::Result<Option<Id>> + Send + 'static,
    ) {
        if self.applications.action_busy {
            return;
        }
        self.applications.action_busy = true;
        let profile = profile_path();
        let session = self.selected_session_id().map(str::to_owned);
        let sender = self.send.clone();
        let account = self.account.id;
        let work = cx.background_executor().spawn(async move { action(&profile) });
        cx.spawn(async move |host, cx| {
            let result = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) || host.account.id != account
                {
                    return;
                }
                host.applications.action_busy = false;
                match result {
                    Ok(instance) if host.selected_session_id() == session.as_deref() => {
                        host.applications.instance = instance;
                    }
                    Ok(_) => {}
                    Err(error) => host.error = error.to_string(),
                }
                host.applications.refresh_needed = true;
                host.refresh_application_instances(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
