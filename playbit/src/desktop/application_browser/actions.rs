//! Selection and exact-instance actions preserve durable bindings and launch ownership.
use super::*;

impl Shell {
    pub(in crate::desktop) fn dismiss_invitation(&mut self, cx: &mut Context<Self>) {
        let Some(mut preview) = self.invitation.take() else { return };
        self.join_dialog = None;
        self.invitation_resume = None;
        self.joining = false;
        if self.invitation_link.as_ref().is_none_or(|link| link.internal_id() == preview.session) {
            self.invitation_link = None;
            self.join_preview = None;
        }
        let saved =
            self.saved_invitations.iter_mut().find(|saved| saved.session == preview.session);
        let app = preview
            .received
            .as_ref()
            .or_else(|| saved.as_deref().and_then(|saved| saved.received.as_ref()))
            .map(|received| received.session.app.clone())
            .or_else(|| {
                self.applications
                    .catalog
                    .iter()
                    .find(|row| row.id == preview.session)
                    .map(|row| row.app.clone())
            });
        let Some(app) = app else { return };
        // The explicit preview can carry the envelope before the catalog notification arrives.
        if preview.received.is_some() {
            if let Some(saved) = saved {
                if saved.received.is_none() {
                    saved.received = preview.received;
                    self.rebuild_application_list();
                }
            } else if self.saved_invitations.len() < MAX_SESSIONS {
                preview.paused = true;
                self.saved_invitations.push(preview);
                self.rebuild_application_list();
            } else {
                self.error = "The invitation list exceeds its 256-session display limit.".into();
                return;
            }
        }
        // Returning to the previous app must preserve a newer link already
        // admitted while this invitation was still onscreen.
        let pending_link = self.invitation_link.take();
        if self.select_application_target(Target::Application(app), cx) {
            self.sidebar_volumes = false;
        }
        self.invitation_link = pending_link;
    }

    pub(super) fn select_application_target(
        &mut self,
        target: Target,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(&target, Target::Scope { .. }) {
            self.applications.pending_scope = None;
        }
        match target {
            Target::Volume(id) => {
                let Some(volume) = self.volumes.iter().find(|volume| volume.id == id).cloned()
                else {
                    return false;
                };
                return self.select_volume(volume, cx);
            }
            Target::Application(identity) => {
                if !self.prepare_browser_navigation(cx) {
                    return false;
                }
                self.applications.clear_selection();
                self.applications.selected = Some(identity);
                self.sidebar_session = None;
                self.selected = None;
                self.browser = None;
                self.new_session = None;
                self.personal = None;
                self.leave_invitation();
                self.opening = None;
                self.refresh_logs(cx);
                self.applications.scope_refresh_needed = true;
                self.refresh_application_scopes(cx);
            }
            Target::Session(id) => {
                return self.select_browser_session(id, self.sidebar_volumes, cx);
            }
            Target::Invitation(id) => {
                if !self.prepare_browser_navigation(cx)
                    || self.applications.catalog.iter().any(|session| session.id == id)
                {
                    return false;
                }
                let Some(mut preview) = self
                    .saved_invitations
                    .iter()
                    .find(|preview| {
                        preview.session == id && !preview.retained && preview.received.is_some()
                    })
                    .cloned()
                else {
                    return false;
                };
                let admission = match self.send.enqueue(Request::ResumeInvitation { session: id }) {
                    Ok(admission) => admission,
                    Err(error) => {
                        self.error = error.into();
                        cx.notify();
                        return false;
                    }
                };
                self.invitation_resume = Some(admission);
                self.invitation_link = None;
                preview.paused = true;
                self.open_join_entry(&preview.session, cx);
                self.invitation = Some(preview);
                self.join_status = InvitationState::Checking;
                self.joining = false;
                self.opening = None;
                self.selected = None;
                self.sidebar_session = None;
                self.browser = None;
                self.new_session = None;
                self.personal = None;
                self.error.clear();
                self.refresh_logs(cx);
            }
            Target::Instance(id) => {
                if !self.prepare_browser_navigation(cx) {
                    return false;
                }
                self.applications.instance = Some(id);
                self.refresh_logs(cx);
                self.applications.root_open = false;
                self.applications.selected_role = None;
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
                if !self.select_scope_volume(volume, role, session, cx) {
                    return false;
                }
                self.applications.pending_scope = None;
                self.applications.root_open = true;
                self.applications.selected_role = Some(role);
                self.applications.scope_in_session = session;
                self.applications.instance = None;
            }
        }
        cx.notify();
        true
    }

    fn select_scope_volume(
        &mut self,
        volume: crate::store::Volume,
        role: Role,
        in_session: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut binding = None;
        if in_session && role == Role::Session {
            let Some(selected) = self.selected_session_id().and_then(|id| {
                self.applications.sessions().iter().find(|session| session.id == id)
            }) else {
                return false;
            };
            if selected.shared_volume.is_some() {
                if selected.shared_volume != Some(volume.id) {
                    return false;
                }
                let Ok(id) = crate::identifier::decode(&selected.id) else { return false };
                binding = Some(id);
            }
        }
        self.select_volume_in(volume, binding, cx)
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
                        if host.select_scope_volume(opened, role, in_session, cx) {
                            host.applications.root_open = true;
                            host.applications.selected_role = Some(role);
                            host.applications.scope_in_session = in_session;
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

    pub(super) fn application_stop_target(&self) -> Option<Id> {
        let owner = self.applications.highlighted_instance()?;
        (Some(owner.instance.session.as_str()) == self.selected_session_id() && is_running(owner))
            .then_some(owner.instance.id)
    }

    pub(super) fn toggle_application(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.selected_session_id().map(str::to_owned) {
            if let Some(instance) = self.application_stop_target() {
                self.stop_application_instance(session, instance, cx);
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
        if let Some(app) = app.filter(|app| app.can_open) {
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
                    host.rebuild_application_list();
                    if select { host.select_application_session(cx); }
                }
                match result {
                    Ok((prepared, instance)) if select => {
                        host.selected = Some(prepared);
                        host.applications.instance = Some(instance.instance);
                        host.refresh_logs(cx);
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
                        host.refresh_logs(cx);
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
