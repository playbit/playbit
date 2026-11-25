//! Apply worker replies and coordinate desktop startup and recovery.
use super::*;

struct RowsChanged {
    sessions: bool,
    profile: bool,
    account: bool,
    volumes: bool,
    saved: bool,
    invitation: bool,
}

impl Shell {
    fn watch_urls(
        urls: async_channel::Receiver<Result<String, &'static str>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |entity, cx| {
            while let Ok(url) = urls.recv().await {
                if entity
                    .update(cx, |this, cx| {
                        this.open_url_result(url, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
    pub(super) fn open_url_result(
        &mut self,
        url: Result<String, &'static str>,
        cx: &mut Context<Self>,
    ) {
        if self.signing_out {
            return;
        }
        self.url_generation = self.url_generation.wrapping_add(1);
        self.pending_database = None;
        match url {
            Ok(url) => {
                if self.join_dialog.is_some() {
                    self.cancel_join_dialog(cx);
                    if self.join_dialog.is_some() {
                        return;
                    }
                } else if self.new_session.is_some()
                    && self.invitation.is_some()
                    && !self.cancel_reviewed_invitation(cx)
                {
                    return;
                }
                self.join_preview = None;
                if url.starts_with("file:") {
                    if file_document::is_app(&url) {
                        self.show_app_document(url, cx);
                    } else {
                        self.open_database_document(url, cx);
                    }
                    return;
                }
                if url.starts_with("playbit:database:") {
                    match crate::store::DatabaseReference::parse(url.as_bytes()) {
                        Ok(reference) => self.pending_database = Some(reference),
                        Err(error) => self.error = error.to_string(),
                    }
                    self.finish_database_open(cx);
                    self.show_initial_feedback(cx);
                    cx.notify();
                    return;
                }
                self.new_session = None;
                self.cancel_invitation_restoration();
                // A link to a session this device already holds opens it at once;
                // the preview reply also reports that for links seen before status.
                let reference = crate::store::SessionReference::from_link(&url, self.channel);
                let result = match reference {
                    Ok(reference)
                        if self.sessions.iter().any(|s| s.id == reference.internal_id()) =>
                    {
                        self.open_retained(reference.internal_id().to_owned(), cx)
                    }
                    Ok(reference) => self.preview_invitation_link(reference),
                    Err(error) => {
                        self.error = error.to_string();
                        self.show_initial_feedback(cx);
                        cx.notify();
                        return;
                    }
                };
                if let Err(error) = result {
                    self.error = error.into();
                }
            }
            Err(error) => self.error = error.into(),
        }
        self.show_initial_feedback(cx);
        cx.notify();
    }
    pub(super) fn preview_invitation_link(
        &mut self,
        reference: crate::store::SessionReference,
    ) -> Result<(), &'static str> {
        let request =
            Request::PreviewDesktopInvitation { invitation: reference.link(self.channel) };
        self.join_preview = Some(if self.join_dialog.is_some() {
            self.send.join_dialog_request(request)?
        } else {
            self.send.enqueue(request)?
        });
        self.invitation_link = Some(reference);
        Ok(())
    }

    fn open_database_document(&mut self, url: String, cx: &mut Context<Self>) {
        let guard = match file_document::begin() {
            Ok(guard) => guard,
            Err(error) => {
                self.error = error;
                self.show_initial_feedback(cx);
                cx.notify();
                return;
            }
        };
        let generation = self.url_generation;
        let sender = self.send.clone();
        let read = cx.background_executor().spawn(async move { file_document::read(&url, guard) });
        cx.spawn(async move |entity, cx| {
            let result = read.await;
            let _ = entity.update(cx, |this, cx| {
                if this.signing_out
                    || this.url_generation != generation
                    || !this.send.same_worker(&sender)
                {
                    return;
                }
                match result {
                    Ok(reference) => this.open_url_result(Ok(reference.to_string()), cx),
                    Err(error) => {
                        this.error = error;
                        this.show_initial_feedback(cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
    fn finish_database_open(&mut self, cx: &mut Context<Self>) {
        let Some(reference) = self.pending_database else { return };
        if !self.account_ready() || self.sign_in.is_some() {
            return;
        }
        if !self.prepare_browser_navigation(cx) {
            return;
        }
        self.pending_database = None;
        let Some(volume) = self.catalog_volume(reference.volume).cloned() else {
            self.error = "The referenced volume is not available on this account or device.".into();
            return;
        };
        self.error.clear();
        self.select_volume(volume, cx);
        if let Some(browser) = &self.browser {
            browser.update(cx, |browser, cx| browser.open_database(reference.entry, cx));
        }
        self.initial_link = false;
        let host = cx.entity();
        cx.defer(move |cx| Self::show_sessions(&host, cx));
    }
    pub(super) fn watch_errors(
        sender: RequestSender,
        errors: async_channel::Receiver<&'static str>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |entity, cx| {
            while let Ok(error) = errors.recv().await {
                cx.background_executor()
                    .spawn(async move {
                        crate::platform::failure::log(
                            &crate::engine::profile_path(),
                            "management admission",
                            error,
                        );
                    })
                    .await;
                if entity
                    .update(cx, |this, cx| {
                        if this.signing_out
                            || !this.send.same_worker(&sender)
                            || error == worker_queue::RESTARTING
                        {
                            return;
                        }
                        this.error = error.into();
                        if !this
                            .new_session
                            .as_ref()
                            .is_some_and(|form| form.read(cx).reviewed_invitation().is_some())
                        {
                            this.joining = false;
                        }
                        this.show_initial_feedback(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
    pub(super) fn watch_responses(
        sender: RequestSender,
        receive: async_channel::Receiver<WorkerResponse>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |entity, cx| {
            while let Ok(response) = receive.recv().await {
                if entity
                    .update(cx, |this, cx| {
                        if this.send.same_worker(&sender) {
                            this.receive_response(response, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
    fn begin_response(&mut self, response: &mut WorkerResponse, cx: &mut Context<Self>) -> bool {
        if response.recovery_epoch.is_some_and(|epoch| !self.send.gate().accepts(epoch)) {
            // A delayed account reply must not overwrite refreshed recovery state
            // or complete a newer action after the observer replaces the engine.
            // Release only its matching sign-in ticket as interrupted work.
            if let Some(ticket) = response.sign_in.take()
                && let Some(view) = &self.sign_in
            {
                view.update(cx, |view, cx| {
                    view.receive(ticket, Err(worker_queue::INTERRUPTED.into()), cx);
                });
            }
            return false;
        }
        if let Some(version) = response.personal_session.take() {
            self.finish_personal_save(
                &version,
                response
                    .result
                    .as_ref()
                    .map(|_| response.personal_selected)
                    .map_err(String::as_str),
                cx,
            );
            cx.notify();
        }
        if response.recovery_action.is_some() && self.recovery_busy == response.recovery_action {
            self.recovery_busy = None;
        }
        self.finish_app_removal(response, cx);
        self.engine_response(response, cx);
        true
    }
    fn engine_response(&mut self, response: &WorkerResponse, cx: &mut Context<Self>) {
        if (response.engine_lost || response.critical)
            && let Some(browser) = &self.browser
        {
            browser.update(cx, |browser, cx| {
                browser.retire_history();
                browser.retire_payload_size();
                cx.notify();
            });
        }
        if response.critical {
            self.recovery_notice.take();
            self.alerted_error = response.result.as_ref().err().cloned().unwrap_or_default();
            let error =
                response.result.as_ref().err().map_or("Unknown startup failure", String::as_str);
            if self.enrolled.is_none()
                && matches!(error, ipc::PROFILE_OPEN_ERROR | ipc::CORRUPT_STORE_ERROR)
            {
                self.error = error.into();
                let host = cx.entity();
                cx.defer(move |cx| Self::show_startup_failure(&host, cx));
                return;
            }
            crate::ui::desktop::alert::show_with_origin(
                crate::ui::desktop::alert::Kind::Critical,
                response.result.as_ref().err().map_or("Unknown startup failure", String::as_str),
                response.origin.clone(),
                cx,
            );
        }
        if response.engine_recovered {
            drafts::retry_erasure_reload(cx);
            if let Some(link) = self.invitation_link.clone()
                && self.invitation.as_ref().is_some_and(|preview| {
                    preview.session == link.internal_id() && preview.received.is_none()
                })
                && let Err(error) = self.preview_invitation_link(link)
            {
                self.error = error.into();
            }
            self.engine_recovering = false;
            self.recovery_notice.take();
            cx.notify();
        }
        if response.engine_lost && !self.engine_recovering {
            self.ap_status.update(cx, |indicator, cx| indicator.set(None, cx));
            self.engine_recovering = true;
            if self.join_dialog.is_some() {
                self.join_preview = None;
                self.join_status =
                    InvitationState::Failed { message: worker_queue::INTERRUPTED.into() };
            } else if self.joining
                && self.invitation.is_some()
                && let Some(form) = &self.new_session
            {
                self.joining = false;
                self.join_preview = None;
                form.update(cx, |form, cx| form.join_failed(worker_queue::INTERRUPTED.into(), cx));
            }
            if self.recovery_busy.take().is_some()
                && let Some(editor) = &self.name_editor
            {
                editor.update(cx, |editor, _| editor.set_enabled(true));
            }
            let age =
                response.recovery_started.map_or(std::time::Duration::ZERO, |at| at.elapsed());
            let delay = crate::engine::restart::NOTICE_AFTER.saturating_sub(age);
            let startup_error =
                response.recovery_started.and(response.result.as_ref().err()).cloned();
            self.recovery_notice = Some(cx.spawn(async move |shell, cx| {
                cx.background_executor().timer(delay).await;
                let _ = shell.update(cx, |shell, cx| {
                    use crate::ui::desktop::alert::{self, Kind};
                    if shell.engine_recovering {
                        if let Some(error) = &startup_error
                            && shell.enrolled.is_none()
                            && cx.windows().is_empty()
                        {
                            alert::show(Kind::EngineStartup, error, cx);
                        } else {
                            alert::show(
                                Kind::RecoverySlow,
                                "Playbit is still trying to reconnect to its background process.",
                                cx,
                            );
                        }
                    }
                });
            }));
            cx.notify();
        }
    }
    fn browser_response(
        &mut self,
        response: &mut WorkerResponse,
        cx: &mut Context<Self>,
    ) -> (bool, bool) {
        draft_publications::receive(response, cx);
        let browser_reply = response.browser_reply.take();
        let data_reply = response.data_reply.take();
        let browser_failure = response.browser_failure.take();
        let current_session =
            self.browser.as_ref().and_then(|browser| browser.read(cx).shared_session());
        let response_session = response.credit.as_ref().and_then(|credit| credit.browser_session);
        if current_session != response_session {
            return (browser_reply.is_some() || data_reply.is_some(), browser_failure.is_some());
        }
        let size_matches = self.browser.as_ref().is_some_and(|browser| {
            browser.read(cx).payload_size_response_matches(
                response.credit.as_ref().map(|credit| credit.admission()),
            )
        });
        let history_matches = self.browser.as_ref().is_some_and(|browser| {
            browser
                .read(cx)
                .history_response_matches(response.credit.as_ref().map(|credit| credit.admission()))
        });
        let volume_permissions_matches = self.browser.as_ref().is_some_and(|browser| {
            browser.read(cx).volume_permissions_response_matches(
                response.credit.as_ref().map(|credit| credit.admission()),
            )
        });
        // Successful browser responses carry no host-state projection.
        // Their entity owns visible-state invalidation, including busy
        // and failed data exchanges inside an otherwise successful reply.
        let browser_response = browser_reply.is_some() || data_reply.is_some();
        // A retired history failure belongs to its old browser, never to a host alert.
        let browser_failed = browser_failure.as_ref().is_some_and(|failure| {
            self.browser.is_some() || failure.is_history() || failure.is_payload_size()
        });
        if let (Some(failure), Err(error), Some(browser)) =
            (browser_failure, &response.result, &self.browser)
            && (!failure.is_history() || history_matches)
            && (!failure.is_payload_size() || size_matches)
            && (!failure.is_volume_permissions() || volume_permissions_matches)
        {
            browser.update(cx, |browser, cx| browser.failed(failure, error, cx));
        }
        if let Some(reply) = browser_reply
            && let Some(browser) = &self.browser
            && (!matches!(&reply, Reply::EntryRevisions { .. }) || history_matches)
            && (!matches!(&reply, Reply::Diagnostic(reply)
                if matches!(reply, crate::engine::reply::DiagnosticReply::PayloadSize { .. }))
                || size_matches)
            && (!matches!(&reply, Reply::Permissions { .. }) || volume_permissions_matches)
        {
            browser.update(cx, |browser, cx| browser.receive(reply, cx));
        }
        if let Some(reply) = data_reply
            && let Some(browser) = &self.browser
        {
            browser.update(cx, |browser, cx| browser.receive_kv(reply, cx));
        }
        (browser_response, browser_failed)
    }
    fn apply_transfer_response(
        &mut self,
        transfer_status: Option<TransferStatus>,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(transfer) = transfer_status {
            if self.apply_reviewed_transfer(&transfer, cx) {
                self.last_transfer = transfer;
                return true;
            }
            if self.last_transfer == transfer
                && !matches!(
                    transfer.kind,
                    TransferKind::Complete | TransferKind::Failed | TransferKind::Paused
                )
            {
                return false;
            }
            match transfer.kind {
                TransferKind::Complete => {
                    if let Some(session) = transfer.session.as_deref()
                        && self.joining
                        && self.opening.as_deref().or_else(|| {
                            self.invitation.as_ref().map(|preview| preview.session.as_str())
                        }) == Some(session)
                    {
                        self.opening = Some(session.into());
                        if let Err(error) = self.prepare_session(session.into(), cx) {
                            self.error = error.into();
                            self.joining = false;
                        }
                    }
                }
                TransferKind::Failed | TransferKind::Paused => {
                    if let Some(session) = transfer.session.as_deref() {
                        let preparing = self.pending_windows.contains_key(session);
                        if !preparing
                            && self
                                .launches
                                .get(session)
                                .is_some_and(|progress| progress.instance.is_none())
                        {
                            self.launches.remove(session);
                        }
                        if !preparing && self.opening.as_deref() == Some(session) {
                            self.joining = false;
                            if transfer.kind == TransferKind::Paused {
                                self.opening = None;
                            }
                        } else if !preparing
                            && self.joining
                            && self
                                .invitation
                                .as_ref()
                                .is_some_and(|invitation| invitation.session == session)
                        {
                            if transfer.kind == TransferKind::Paused {
                                self.dismiss_invitation(cx);
                            } else {
                                self.joining = false;
                            }
                        }
                    }
                }
                _ => {}
            }
            self.last_transfer = transfer;
        }
        true
    }

    fn apply_account_response(
        &mut self,
        account_response: Option<AccountResponse>,
        cx: &mut Context<Self>,
    ) {
        if let Some(account) = account_response {
            match account {
                AccountResponse::Recovery(state) => {
                    if state != RecoveryStatus::Canceled {
                        self.apply_recovery(state, cx);
                    }
                    self.error.clear();
                }
                AccountResponse::AgreementAccepted => {
                    self.account_profile.agreement_required = false;
                }
                AccountResponse::DisplayName(name) => {
                    self.account_profile.display_name = Some(name);
                    self.name_editor = None;
                    self.account_name_open = false;
                    if let Some(window) = self.sessions_window {
                        cx.defer(move |cx| {
                            let _ = window.update(cx, |host, window, _| {
                                window.focus(&host.focus);
                                #[cfg(all(target_os = "macos", not(test)))]
                                crate::ui::desktop::appearance::set_content_modal(window, false);
                                window.refresh();
                            });
                        });
                    }
                    self.error.clear();
                }
                AccountResponse::SavedAccount(_)
                | AccountResponse::Prepared(_)
                | AccountResponse::SignedIn(_)
                | AccountResponse::CodeSent(_) => {}
            }
        }
    }
    fn apply_status_volumes(
        &mut self,
        mut status_volumes: Option<Vec<crate::store::Volume>>,
        scoped_volumes: Vec<crate::store::Volume>,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(volumes) = &mut status_volumes {
            super::sidebar_view::sort_volumes(volumes);
        }
        let volumes_changed = status_volumes.as_ref().is_some_and(|volumes| {
            volumes != &self.volumes || scoped_volumes != self.scoped_volumes
        });
        if let Some(volumes) = status_volumes {
            self.volumes = volumes;
            self.scoped_volumes = scoped_volumes;
            if self.volume_rename.as_ref().is_some_and(|rename| {
                !self.volumes.iter().any(|volume| volume.id == rename.volume())
            }) {
                self.volume_rename = None;
            }
            if self
                .browser
                .as_ref()
                .is_some_and(|browser| self.catalog_volume(browser.read(cx).volume.id).is_none())
            {
                self.browser = None;
            }
            if let Some(browser) = &self.browser {
                browser.update(cx, |browser, _| {
                    self.set_browser_volumes(browser);
                    if let Some(volume) = self.catalog_volume(browser.volume.id) {
                        browser.volume = volume.clone();
                    }
                    browser.refresh();
                });
            }
        }
        volumes_changed
    }
    fn apply_invitation_status(
        &mut self,
        invitation_status: Option<Option<InvitationStatus>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut invitation_changed = false;
        if let Some(state) = invitation_status {
            invitation_changed = self.invitation_status.as_ref().map(|s| (&s.session, &s.state))
                != state.as_ref().map(|s| (&s.session, &s.state));
            if let Some(state) = &state
                && self.invitation_resume.is_none()
                && self.join_preview.is_none()
                && self.invitation.as_ref().is_some_and(|invitation| {
                    invitation.session == state.session && !invitation.paused
                })
            {
                self.join_status = state.state.clone();
                if state.state == InvitationState::Paused {
                    if self.new_session.is_none() {
                        self.dismiss_invitation(cx);
                    } else if let Some(preview) = &mut self.invitation {
                        preview.paused = true;
                    }
                }
            }
            self.invitation_status = state;
        }
        invitation_changed
    }
    fn apply_status_rows(
        &mut self,
        response: &mut WorkerResponse,
        cx: &mut Context<Self>,
    ) -> RowsChanged {
        let session_rows = response.session_rows.take();
        let status_account_profile = response.status_account_profile.take();
        let status_account = response.status_account.take();
        let backup_changed =
            response.account_key_backup.is_some_and(|status| status != self.account_key_backup);
        if let Some(status) = response.account_key_backup.take() {
            self.account_key_backup = status;
        }
        let saved_invitations = response.saved_invitations.take();
        let sessions_changed =
            session_rows.as_ref().is_some_and(|sessions| sessions != &self.sessions);
        if let Some(sessions) = session_rows {
            if self.selected.as_ref().is_some_and(|prepared| {
                self.sessions.iter().any(|row| row.id == prepared.session.id)
                    && !sessions.iter().any(|row| row.id == prepared.session.id)
            }) {
                self.selected = None;
            }
            self.sessions = sessions;
            if self
                .sidebar_session
                .as_ref()
                .is_some_and(|id| !self.sessions.iter().any(|row| &row.id == id))
            {
                self.sidebar_session = None;
            }
            if self.selected_session_id().is_none() && self.log_session.is_some() {
                self.refresh_logs(cx);
            }
        }
        let profile_changed = status_account_profile.as_ref().is_some_and(|profile| {
            profile.replacement_required != self.account_profile.replacement_required
                || profile.display_name != self.account_profile.display_name
                || profile.initialized != self.account_profile.initialized
                || profile.unlocked != self.account_profile.unlocked
        });
        if let Some(profile) = status_account_profile {
            self.account_profile = profile;
        }
        let account_changed =
            status_account.as_ref().is_some_and(|account| account != &self.account);
        if let Some(account) = status_account {
            self.account = account;
        }
        if account_changed {
            self.applications = Default::default();
        }
        if self.account_profile.unlocked && self.account.id != crate::store::Id::from_bytes([0; 16])
        {
            #[cfg(not(test))]
            if let Err(error) = drafts::configure(self.account.id, profile_path(), cx) {
                self.error = error;
            }
            #[cfg(test)]
            drafts::configure_memory(self.account.id, cx);
        }
        let volumes_changed = self.apply_status_volumes(
            response.status_volumes.take(),
            std::mem::take(&mut response.status_scoped_volumes),
            cx,
        );
        let mut saved_invitations_changed =
            saved_invitations.as_ref().is_some_and(|saved| saved != &self.saved_invitations);
        if let Some(mut saved) = saved_invitations {
            if saved.len() > application_browser::MAX_SESSIONS {
                saved.truncate(application_browser::MAX_SESSIONS);
                self.error = "The invitation list can show up to 256 sessions.".into();
            }
            // Removing an app also retires its saved invitation review. Canceling
            // an old review must not put the row back into the sidebar.
            if let Some(current) = &self.invitation
                && self.saved_invitations.iter().any(|old| {
                    old.session == current.session && !old.retained && old.received.is_some()
                })
                && !saved.iter().any(|item| item.session == current.session)
                && !self.sessions.iter().any(|row| row.id == current.session)
            {
                if self.new_session.as_ref().is_some_and(|form| {
                    form.read(cx).reviewed_invitation() == Some(current.session.as_str())
                }) {
                    self.new_session = None;
                }
                self.joining = false;
                if self
                    .invitation_link
                    .as_ref()
                    .is_some_and(|link| link.internal_id() != current.session)
                {
                    self.invitation = None;
                    self.invitation_resume = None;
                } else {
                    self.dismiss_join_dialog(cx);
                }
            }
            if let Some(current) = &mut self.invitation
                && let Some(updated) =
                    saved.iter().find(|preview| preview.session == current.session)
                && updated.received.is_some()
                && current.received != updated.received
            {
                current.received = updated.received.clone();
                saved_invitations_changed = true;
            }
            self.saved_invitations = saved;
            if saved_invitations_changed
                && response.application_catalog.is_none()
                && response.imported_apps.is_none()
            {
                self.rebuild_application_list();
            }
        }
        if let Some(credit) = &response.credit
            && self.invitation_resume == Some(credit.admission())
        {
            self.invitation_resume = None;
            if response.result.is_err() {
                self.dismiss_invitation(cx);
            } else if let Some(invitation) = &mut self.invitation {
                invitation.paused = false;
            }
        }
        let invitation_changed =
            self.apply_invitation_status(response.invitation_status.take(), cx);
        RowsChanged {
            sessions: sessions_changed,
            profile: profile_changed || backup_changed,
            account: account_changed,
            volumes: volumes_changed,
            saved: saved_invitations_changed,
            invitation: invitation_changed,
        }
    }
    fn apply_invitation_preview(
        &mut self,
        invitation_preview: Option<crate::store::InvitationPreview>,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(mut preview) = invitation_preview {
            let Some(link) =
                self.invitation_link.as_ref().filter(|link| link.internal_id() == preview.session)
            else {
                return false;
            };
            let input = link.link(self.channel);
            self.join_preview = None;
            self.invitation_resume = None;
            self.restore_pending_invitation = false;
            if preview.retained {
                self.join_dialog = None;
                if let Err(error) = self.open_retained(preview.session, cx) {
                    self.error = error.into();
                }
                return true;
            }
            if preview.received.is_none()
                && let Some(cached) =
                    self.saved_invitations.iter().find(|p| p.session == preview.session)
            {
                preview.received.clone_from(&cached.received);
            }
            self.opening = None;
            self.joining = false;
            self.open_join_entry(&input, cx);
            self.join_status = InvitationState::Checking;
            let watch = Request::WatchInvitation { session: preview.session.clone() };
            self.invitation = Some(preview);
            self.lookup_join_dialog(watch);
            self.error.clear();
        }
        true
    }
    fn apply_prepared(
        &mut self,
        prepared: Option<crate::engine::reply::PreparedSession>,
        cx: &mut Context<Self>,
    ) {
        if let Some(prepared) = prepared {
            let id = prepared.session.id.clone();
            self.pending_windows.remove(&id);
            self.initial_link = false;
            self.recent_sessions.retain(|session| session != &id);
            self.recent_sessions.insert(0, id.clone());
            self.recent_sessions.truncate(256);
            self.selected = Some(prepared);
            self.refresh_logs(cx);
            if self.opening.as_deref() == Some(&id) {
                self.opening = None;
                self.joining = false;
                self.leave_invitation();
            }
            cx.notify();
        }
    }
    fn watch_startup_invitation(&mut self) {
        if let Some(invitation) = &self.invitation {
            if invitation.paused {
                return;
            }
            if let Some(state) = &self.invitation_status
                && state.session == invitation.session
            {
                self.join_status = state.state.clone();
            } else if !self.joining {
                self.send
                    .send(Request::WatchInvitation { session: invitation.session.clone() })
                    .ok();
            }
        }
    }
    fn apply_status_response(
        &mut self,
        status_enrolled: Option<bool>,
        status_recovery: Option<RecoveryStatus>,
        changed: RowsChanged,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.enrolled == status_enrolled
            && !changed.invitation
            && !changed.saved
            && !changed.volumes
            && !changed.account
            && !changed.profile
            && !changed.sessions
            && status_recovery.as_ref().is_none_or(|state| self.recovery.as_ref() == Some(state))
        {
            return false;
        }
        self.enrolled = status_enrolled;
        if let Some(state) = status_recovery {
            self.apply_recovery(state, cx);
        }
        self.restore_startup_opening(cx);
        if self.join_dialog.is_none() && self.new_session.is_none() {
            self.watch_startup_invitation();
        }
        self.refresh_session_icons(cx);
        self.first_account_update = true;
        maybe_send_crash_reports(cx, self);
        crate::trace::mark("ui.status_applied");
        true
    }
    fn finish_response(
        &mut self,
        response: WorkerResponse,
        changed: RowsChanged,
        browser_failed: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let WorkerResponse {
            result,
            session,
            launch_failed,
            engine_lost,
            status_enrolled,
            status_recovery,
            ..
        } = response;
        match result {
            Ok(()) if status_enrolled.is_some() => {
                if !self.apply_status_response(status_enrolled, status_recovery, changed, cx) {
                    return false;
                }
            }
            Ok(()) => {}
            Err(_) if engine_lost => {}
            Err(_) if browser_failed => {}
            Err(error) => {
                if let Some(id) = &session {
                    self.pending_windows.remove(id);
                }
                self.error = error;
                let reviewed_transfer = self
                    .new_session
                    .as_ref()
                    .is_some_and(|form| form.read(cx).reviewed_invitation().is_some());
                if !reviewed_transfer
                    && session.as_deref().is_none_or(|id| self.opening.as_deref() == Some(id))
                {
                    self.joining = false;
                    if launch_failed {
                        // Preparation already succeeded. Resume would repeat the same
                        // local failure or an unconfirmed dispatch, not resume a transfer.
                        self.opening = None;
                    }
                }
            }
        }
        true
    }
    fn receive_response(&mut self, mut response: WorkerResponse, cx: &mut Context<Self>) {
        if self.signing_out {
            return;
        }
        if response.result.is_err()
            && let (Some(session), Some(credit)) = (&response.session, &response.credit)
            && self.launches.get(session).is_some_and(|progress| {
                progress.instance.is_none() && progress.admission == Some(credit.admission())
            })
        {
            self.launches.remove(session);
        }
        // Navigation can replace an admission for the same session. Its old
        // completion releases only its own credit and cannot finish newer work.
        if let (Some(session), Some(credit)) = (&response.session, &response.credit)
            && self.pending_windows.get(session) != Some(&Some(credit.admission()))
        {
            return;
        }

        let initializing = self.enrolled.is_none();
        if !self.begin_response(&mut response, cx) {
            return;
        }
        if !self.receive_invitation_admission(&response, cx) {
            return;
        }
        self.finish_pending_volume(&response, cx);
        if response.transfer_status.is_some()
            && let (Some(session), Some(_)) = (&response.session, &response.credit)
        {
            // Preparation handed off to a transfer. Its completed admission
            // must not block preparation when that transfer finishes.
            self.pending_windows.remove(session);
            if let Some(progress) = self.launches.get_mut(session) {
                progress.admission = None;
            }
        }
        if let Some(recent) = response.recent_sessions.take()
            && self.recent_sessions != recent
        {
            self.recent_sessions = recent;
            cx.notify();
        }
        if let Some((id, _)) = &response.account_identity {
            self.account_id.clone_from(id);
        }
        self.apply_sign_in_response(&mut response, cx);
        if response.critical {
            return;
        }
        let _timing = crate::trace::Span::new("ui.apply_response");
        if let Some(status) = response.status_ap_connection.take() {
            self.ap_status.update(cx, |indicator, cx| indicator.set(Some(status), cx));
        }
        let (browser_response, browser_failed) = self.browser_response(&mut response, cx);
        if browser_response && response.result.is_ok() && !response.engine_lost {
            drop(_timing);
            crate::trace::mark("ui.response_applied");
            return;
        }
        if let Some(snapshot) = response.personal_snapshot.take() {
            self.open_personal(snapshot);
        }
        if !self.apply_transfer_response(response.transfer_status.take(), cx) {
            return;
        }
        self.apply_account_response(response.account_response.take(), cx);
        if let Some(scale) = response.status_ui_zoom.take() {
            crate::ui::desktop::zoom::configure_engine(
                crate::engine::profile_path(),
                None,
                scale,
                cx,
            );
        }
        if let Some(startup) = response.startup.take() {
            self.startup_changed(startup, cx);
        }
        if let Some(instance) = response.launched {
            if response.launch_failed {
                if let Err(error) = response.result {
                    self.launch_failed(instance, error, cx);
                }
                return;
            }
            if let Some(prepared) = &response.prepared {
                if let Some(progress) = self.launches.get_mut(&prepared.session.id) {
                    progress.instance = Some(instance);
                }
                self.reconcile_launches(cx);
            }
        }
        let viewing_app_volume = self.applications.root_open;
        let mut changed = self.apply_status_rows(&mut response, cx);
        if let Some(activity) = response.session_activity.take() {
            self.applications.set_activity(
                activity,
                response.application_catalog.as_deref().unwrap_or_default(),
                self.account.id,
                self.account_profile.display_name.as_deref(),
            );
        }
        if let Some(apps) = response.imported_apps.take() {
            changed.sessions |= apps != self.applications.imported_apps();
            self.apply_imported_app_catalog(apps);
        }
        if let Some(sessions) = response.application_catalog.take() {
            self.apply_application_catalog(sessions, cx);
        }
        if viewing_app_volume && !self.applications.root_open && self.prepare_browser_navigation(cx)
        {
            self.browser = None;
        }
        if let Some(id) = response.volume_label.take() {
            if let Some(volume) = response.volume_created.take()
                && let Some(row) = self.volumes.iter_mut().find(|row| row.id == id)
            {
                *row = volume;
                super::sidebar_view::sort_volumes(&mut self.volumes);
            }
            self.finish_volume_rename(id, response.result.is_ok(), cx);
        } else if let Some(volume) = response.volume_created.take() {
            let id = volume.id;
            if !self.volumes.iter().any(|row| row.id == id) && self.volumes.len() < 256 {
                self.volumes.push(volume.clone());
                super::sidebar_view::sort_volumes(&mut self.volumes);
            }
            if !self.sidebar_volumes
                || self.volume_rename.is_some()
                || !self.volumes.iter().any(|row| row.id == id)
                || !self.select_volume(volume, cx)
                || !self.rename_volume(id, cx)
            {
                self.show_visual_bell(
                    "Volume created. Choose Rename in Volumes to give it a name.",
                    "menu-check",
                    cx,
                );
            }
        }
        if let Some((form, row)) = response.session_created.take() {
            self.session_created(form, row, cx);
        }
        if !self.apply_invitation_preview(response.invitation_preview.take(), cx) {
            return;
        }
        self.review_ready_invitation(cx);
        self.apply_prepared(response.prepared.take(), cx);
        if !self.finish_response(response, changed, browser_failed, cx) {
            return;
        }
        self.ensure_sign_in(cx);
        self.finish_database_open(cx);
        if initializing
            && self.account_ready()
            && self.sign_in.is_none()
            && self
                .opening
                .as_ref()
                .is_some_and(|session| self.pending_windows.contains_key(session))
        {
            self.resume_sign_in_destination(cx);
        }
        cx.notify();
        self.show_initial_feedback(cx);
        drop(_timing);
        crate::trace::mark("ui.response_applied");
    }

    // This constructor is an explicit non-branching field initializer.
    #[rustfmt::skip]
    pub(super) fn with_editors(
        send: RequestSender,
        cx: &mut Context<Self>,
    ) -> Self {
        let session_scrollbars = cx.new(|_| crate::ui::desktop::scrollbar::Scrollbars::new(false));
        let volume_scrollbars = cx.new(|_| crate::ui::desktop::scrollbar::Scrollbars::new(false));
        let session_scroll = gpui::UniformListScrollHandle::new();
        let volume_scroll = gpui::UniformListScrollHandle::new();
        session_scroll.0.borrow_mut().base_handle = session_scrollbars.read(cx).handle();
        volume_scroll.0.borrow_mut().base_handle = volume_scrollbars.read(cx).handle();
        Self {
            send,
            focus: cx.focus_handle(),
            focus_window: true,
            enrolled: None,
            selected: None,
            sessions_window: None,
            crash_report_profile: None,
            crash_reports_pending: false,
            sending_crash_reports: cx.new(|_| false),
            initial_link: false,
            pending_database: None,
            url_generation: 0,
            pending_windows: Default::default(),
            launch_failures: Default::default(),
            launch_forms: Default::default(),
            launches: Default::default(),
            startup: Default::default(),
            error: String::new(),
            engine_recovering: false,
            recovery_notice: None,
            alerted_error: String::new(),
            startup_prompt: false,
            sign_out_prompt: false,
            sidebar_session: None,
            sidebar_volumes: false,
            account_id: String::new(),
            account_name_open: false,
            account_name_focus: false,
            visual_bell: None,
            update_notice: None,
            delete_volume_prompt: false,
            volume_rename: None,
            pending_volume: None,
            signing_out: false,
            sign_out_error: None,
            invitation: None,
            invitation_link: None,
            invitation_resume: None,
            saved_invitations: Vec::new(),
            volumes: Vec::new(),
            scoped_volumes: Vec::new(),
            sessions: Vec::new(),
            first_account_update: false,
            account: crate::store::Account { id: crate::client::Id([0u8; 16]), public_key: String::from("") },
            sign_in: None,
            sign_in_opening: false,
            channel_switching: false,
            show_environment_selector: crate::engine::profile::testing_profile_directory_exists(),
            channel: crate::release_channel::ReleaseChannel::Public,
            account_key_backup: Default::default(),
            account_profile: AccountProfile { agreement_required: false, replacement_required: false, display_name: None, unlocked: false, initialized: false },
            join_status: InvitationState::Checking,
            invitation_status: None,
            restore_pending_invitation: true,
            joining: false,
            opening: None,
            name_editor: None,
            recovery: None,
            recovery_busy: None,
            personal: None,
            new_session: None,
            app_preparation: new_session::Preparation::default(),
            personal_grants: BTreeMap::new(),
            personal_controls: BTreeMap::new(),
            personal_save: None,
            last_transfer: TransferStatus::default(),
            ap_status: cx.new(ap_status::Indicator::new),
            navigation_controls: BTreeMap::new(),
            browser: None,
            browser_notice: None,
            draft_retirement: None,
            _draft_observer: cx.observe_global::<drafts::Drafts>(|host,cx| {
                if let Some(browser)=&host.browser {
                    browser.update(cx,|browser,cx| {
                        browser.sync_local_drafts(cx);
                    });
                }
                host.resume_draft_retirement(cx);
                cx.notify();
            }),
            logs: cx.new(LogView::new),
            session_action_busy: false,
            recent_sessions: Vec::new(),
            applications: Default::default(),
            join_dialog: None,
            join_preview: None,
            session_dialog_id: None,
            welcome_scrollbars: cx.new(|_| crate::ui::desktop::scrollbar::Scrollbars::new(false)),
            making_project: false,
            log_watch: None,
            log_session: None,
            log_generation: 0,
            session_icons: Default::default(),
            session_scroll,
            volume_scroll,
            session_scrollbars,
            volume_scrollbars,
            host_controls: BTreeMap::new(),
            rendered_host_controls: Vec::with_capacity(64),
            host_text_ids: std::array::from_fn(|_| None),
            rendered_host_text: 0,
        }
    }
    pub(super) fn new(
        worker: (
            RequestSender,
            async_channel::Receiver<WorkerResponse>,
            async_channel::Receiver<&'static str>,
        ),
        urls: async_channel::Receiver<Result<String, &'static str>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (send, receive, errors) = worker;
        send.send(Request::DesktopLaunch).ok();
        Self::watch_urls(urls, cx);
        Self::watch_errors(send.clone(), errors, cx);
        Self::watch_responses(send.clone(), receive, cx);
        Self::with_editors(send, cx)
    }
}

#[cfg(test)]
mod status_bar_tests {
    use super::*;

    #[gpui::test]
    fn selection_retargets_details_and_ignores_late_launch_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, _window, _requests) = test_support::host(cx);
        let first = crate::identifier::encode(&[0x07; 32]);
        let second = crate::identifier::encode(&[0x09; 32]);
        let prepared = crate::engine::reply::PreparedSession {
            session: crate::store::Session {
                id: first.clone(),
                selector: crate::store::BuildSelector::Exact {
                    build_id: crate::identifier::encode(&[0x08; 32]),
                },
                resolved_build_id: crate::identifier::encode(&[0x08; 32]),
                root_volume: crate::store::Id::from_bytes([1; 16]),
                shared_volume: None,
                app: crate::package::AppIdentity {
                    publisher: crate::store::Id::from_bytes([3; 16]),
                    app_id: "test".into(),
                },

                creator: crate::store::Id::from_bytes([3; 16]),
                update_authority: crate::store::UpdateAuthority::Creator,
            },
            invitation: "first-session-link".into(),
            availability: "Connected".into(),
            update: None,
        };
        host.update(cx, |host, cx| {
            host.sessions = [&first, &second]
                .into_iter()
                .map(|id| SessionRow {
                    reference: String::new(),
                    id: id.clone(),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some(id.clone()),
                    resolved_build_id: String::new(),
                })
                .collect();
            host.apply_prepared(Some(prepared.clone()), cx);
            host.logs.update(cx, |logs, cx| {
                logs.set(
                    &first,
                    &[crate::ui::app_control::LogEntry::notice("first-session log".into())],
                    cx,
                )
            });
            host.select_sidebar_session(second.clone(), cx);
            host.apply_prepared(Some(prepared), cx);
            assert_eq!(host.selected_session_id(), Some(second.as_str()));
            assert!(host.selected_details().is_none());
            assert_eq!(host.logs.read(cx).session.as_deref(), Some(second.as_str()));
            assert!(
                !host.logs.read(cx).lines.iter().any(|line| line.message == "first-session log")
            );
        });
    }

    #[gpui::test]
    fn return_submits_name_once_and_restores_focus_after_save(cx: &mut gpui::TestAppContext) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_profile.display_name = Some("  Saved name  ".into());
            host.open_name_dialog(cx);
        });
        cx.run_until_parked();
        test_support::press(window, "enter", cx);
        let Message::Request(request) = requests.try_recv().expect("display-name submission")
        else {
            panic!("display-name request");
        };
        assert!(
            matches!(request.request, Request::SetDisplayName { name } if name == "Saved name")
        );
        host.read_with(cx, |host, _| {
            assert!(host.recovery_busy == Some(worker_recovery::Action::SetDisplayName));
            assert!(host.account_name_open);
        });
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
        host.update(cx, |host, cx| {
            let mut response = WorkerResponse::from_reply(Ok(Reply::DisplayName {
                display_name: "Saved name".into(),
            }));
            response.recovery_action = Some(worker_recovery::Action::SetDisplayName);
            host.receive_response(response, cx);
        });
        cx.run_until_parked();
        window
            .update(cx, |host, window, _| {
                assert!(!host.account_name_open);
                assert!(host.name_editor.is_none());
                assert!(host.recovery_busy.is_none());
                assert!(host.focus.is_focused(window));
            })
            .expect("name dialog window remains open");
    }

    #[gpui::test]
    fn automatic_replacement_status_opens_the_decision(cx: &mut gpui::TestAppContext) {
        // A replacement decision can arrive while optional form fill is pending;
        // it must not wait for that lookup or admit its stale fields.
        let (host, _, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.enrolled = Some(false);
            host.ensure_sign_in(cx);
        });
        cx.run_until_parked();
        host.update(cx, |host, cx| {
            let view = host.sign_in.clone().expect("sign-in window");
            assert_eq!(view.read(cx).flow.step, crate::ui::desktop::sign_in::flow::Step::Entry);
            let Message::Request(lookup) = requests.try_recv().expect("optional lookup") else {
                panic!("lookup request")
            };
            assert!(matches!(lookup.request, Request::SavedAccount));
            let mut profile = host.account_profile.clone();
            profile.replacement_required = true;
            let mut response = WorkerResponse::new(Ok(()));
            response.status_enrolled = Some(false);
            response.status_account_profile = Some(profile);
            host.receive_response(response, cx);
            assert_eq!(view.read(cx).flow.step, crate::ui::desktop::sign_in::flow::Step::Replace);
            assert!(!view.read(cx).flow.busy());
            view.update(cx, |view, cx| {
                view.receive(
                    lookup.sign_in.expect("lookup ticket"),
                    Ok(crate::ui::desktop::sign_in::flow::Reply::SavedAccount(None)),
                    cx,
                )
            });
            assert_eq!(view.read(cx).flow.step, crate::ui::desktop::sign_in::flow::Step::Replace);
        });
        assert!(requests.try_recv().is_err(), "the choice requires no network preflight");
    }

    #[gpui::test]
    fn retained_links_sign_in_and_transfer_keep_late_launch_errors(cx: &mut gpui::TestAppContext) {
        // Every continuation registers the same launch owner before preparation,
        // so a failure delivered after submission cannot disappear with navigation.
        for path in 0..3 {
            let (host, _, requests, responses) = test_support::host_with_responses(cx);
            let id = crate::identifier::encode(&[7; 32]);
            host.update(cx, |host, cx| match path {
                0 => host.open_retained(id.clone(), cx).expect("retained open"),
                1 => {
                    host.opening = Some(id.clone());
                    host.resume_sign_in_destination(cx);
                }
                _ => {
                    host.invitation = Some(crate::store::InvitationPreview {
                        session: id.clone(),
                        retained: false,
                        paused: false,
                        received: None,
                    });
                    host.joining = true;
                    host.apply_transfer_response(
                        Some(TransferStatus {
                            kind: TransferKind::Complete,
                            session: Some(id.clone()),
                            ..Default::default()
                        }),
                        cx,
                    );
                }
            });
            let Message::Request(queued) = requests.try_recv().expect("admitted preparation")
            else {
                panic!("prepare")
            };
            assert!(
                matches!(&queued.request, Request::PrepareSession { session } if *session == id)
            );
            let receipt = guest_app::LaunchReceipt {
                incarnation: crate::store::Id::from_bytes([2; 16]),
                instance: crate::store::Id::from_bytes([1; 16]),
                owner_generation: 1,
                presentation: 0,
            };
            host.read_with(cx, |host, _| {
                assert_eq!(
                    host.launches[&id].admission,
                    Some(queued.credit.as_ref().expect("preparation credit").admission())
                );
            });
            let prepared = crate::engine::reply::PreparedSession {
                session: serde_json::from_value(serde_json::json!({
                    "id":id, "selector":{"kind":"exact","build_id":crate::identifier::encode(&[8; 32])},
                    "resolved_build_id":crate::identifier::encode(&[8; 32]), "root_volume":crate::identifier::encode(&[1; 16]),
                    "app":{"publisher":crate::identifier::encode(&[3; 16]),"app_id":"test"}, "creator":crate::identifier::encode(&[3; 16]), "update_authority":"creator"
                })).expect("prepared session fixture"),
                invitation: String::new(), availability: "Local".into(), update: None,
            };
            let mut response = WorkerResponse::from_reply(Ok(Reply::SessionPrepared(prepared)));
            response.session = Some(id.clone());
            response.launched = Some(receipt);
            response.credit = queued.credit;
            responses.try_send(response).expect("preparation delivery");
            cx.run_until_parked();
            host.read_with(cx, |host, _| assert_eq!(host.launches[&id].instance, Some(receipt)));
            let mut failure = WorkerResponse::new(Err("Late native rejection".into()));
            failure.launched = Some(receipt);
            failure.launch_failed = true;
            responses.try_send(failure).expect("failure delivery");
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(!host.launches.contains_key(&id));
                assert_eq!(host.error, "Late native rejection");
            });
        }
    }

    #[gpui::test]
    fn native_launch_failure_does_not_offer_transfer_resume(cx: &mut gpui::TestAppContext) {
        // A prepared local session cannot progress by repeating PrepareSession.
        // Keep its concrete guidance, release admission, and remove Resume.
        let (host, window, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.opening = Some("saved-session".into());
            host.joining = true;
            host.pending_windows.insert("saved-session".into(), None);
            let mut response = WorkerResponse::new(Err("Move the generated bundle aside".into()));
            response.session = Some("saved-session".into());
            response.launch_failed = true;
            host.receive_response(response, cx);
            assert!(host.opening.is_none());
            assert!(!host.joining);
            assert!(!host.pending_windows.contains_key("saved-session"));
            assert_eq!(host.error, "Move the generated bundle aside");
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(!host.host_controls.contains_key("resume-opening"));
        });
        // Enter cannot replay the launch through a stale Resume control.
        super::super::test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn retired_engine_reply_interrupts_only_its_matching_sign_in_ticket(
        cx: &mut gpui::TestAppContext,
    ) {
        // Hold the real adapter's ticket across reconnect, then try it again
        // after admitting a newer operation in the same sign-in form.
        let (host, _, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: None,
                unlocked: false,
                initialized: false,
            };
            host.recovery = None;
            host.ensure_sign_in(cx);
        });
        cx.run_until_parked();
        let Message::Request(old) = requests.try_recv().expect("admitted recovery request") else {
            panic!("sign-in recovery request")
        };
        assert!(matches!(old.request, Request::RecoveryStatus));
        let ticket = old.sign_in.expect("sign-in operation ticket");
        host.update(cx, |host, cx| {
            let gate = host.send.gate();
            gate.pause();
            gate.resume();
            let stale_reply = || {
                let mut response = WorkerResponse::new(Ok(()));
                response.sign_in = Some(ticket);
                response.recovery_epoch = Some(old.epoch);
                response.account_response =
                    Some(AccountResponse::Recovery(RecoveryStatus::Ready {
                        revision: 1,
                        pending_save: false,
                        restored_sessions: None,
                        restored_volumes: None,
                    }));
                response
            };
            host.receive_response(stale_reply(), cx);
            assert!(host.recovery.is_none(), "stale state must not be adopted");
            assert!(host.error.is_empty());
            let view = host.sign_in.clone().expect("open sign-in form");
            assert!(!view.read(cx).flow.busy(), "matching form must allow retry");
            assert_eq!(view.read(cx).flow.error, worker_queue::INTERRUPTED);
            let current =
                view.update(cx, |view, _| view.flow.start().expect("retry recovery lookup").ticket);
            host.receive_response(stale_reply(), cx);
            assert!(view.read(cx).flow.busy(), "old ticket cannot finish newer work");
            // A pending retry retains the failure screen until its own reply arrives.
            assert!(view.read(cx).flow.error_screen());
            assert_eq!(view.read(cx).flow.error, worker_queue::INTERRUPTED);
            let mut response = WorkerResponse::new(Err("current account failure".into()));
            response.sign_in = Some(current);
            response.recovery_epoch = Some(gate.epoch());
            host.receive_response(response, cx);
            assert!(!view.read(cx).flow.busy());
            assert_eq!(view.read(cx).flow.error, "current account failure");
        });
        assert!(requests.is_empty(), "interrupted reply must not replay work");
    }

    #[gpui::test]
    fn recovery_failure_from_before_restart_cannot_replace_refreshed_state(
        cx: &mut gpui::TestAppContext,
    ) {
        // Retire a held account reply through the same gate as real admissions.
        let (host, _, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.send_action(Request::SaveRecovery, cx);
            assert!(host.recovery_busy == Some(worker_recovery::Action::Save));
        });
        let Message::Request(old) = requests.try_recv().expect("old recovery admission") else {
            panic!("recovery request");
        };
        host.update(cx, |host, cx| {
            let gate = host.send.gate();
            gate.pause();
            let mut lost = WorkerResponse::new(Err("engine stopped".into()));
            lost.engine_lost = true;
            host.receive_response(lost, cx);
            assert!(host.engine_recovering);
            assert!(host.recovery_busy.is_none());
            gate.resume();
            let mut recovered = WorkerResponse::new(Ok(()));
            recovered.engine_recovered = true;
            host.receive_response(recovered, cx);
            assert!(!host.engine_recovering);
            host.send_action(Request::SaveRecovery, cx);
            assert!(host.recovery_busy == Some(worker_recovery::Action::Save));

            let mut stale = WorkerResponse::new(Err(worker_queue::INTERRUPTED.into()));
            stale.recovery_epoch = Some(old.epoch);
            stale.recovery_action = Some(worker_recovery::Action::Save);
            host.receive_response(stale, cx);
            assert!(!host.engine_recovering);
            assert!(host.error.is_empty());
            assert!(host.recovery_busy == Some(worker_recovery::Action::Save));

            let mut current = WorkerResponse::new(Ok(()));
            current.recovery_epoch = Some(gate.epoch());
            current.recovery_action = Some(worker_recovery::Action::Save);
            host.receive_response(current, cx);
            assert!(host.recovery_busy.is_none());
        });
    }

    #[gpui::test]
    fn status_tracks_observer_owned_recovery(cx: &mut gpui::TestAppContext) {
        // Ordinary replies cannot finish recovery; the change observer owns it.
        let (host, _, _) = test_support::host(cx);
        host.update(cx, |host, cx| {
            let mut lost = WorkerResponse::new(Err("engine stopped".into()));
            lost.engine_lost = true;
            host.receive_response(lost, cx);
            assert!(host.engine_recovering);
            let mut unrelated = WorkerResponse::new(Ok(()));
            unrelated.status_enrolled = Some(true);
            host.receive_response(unrelated, cx);
            assert!(host.engine_recovering);
            let mut failed = WorkerResponse::new(Err("recovery request failed".into()));
            failed.recovery_action = Some(worker_recovery::Action::Prepare);
            host.receive_response(failed, cx);
            assert!(host.engine_recovering);
            let mut recovered = WorkerResponse::new(Ok(()));
            recovered.engine_recovered = true;
            host.receive_response(recovered, cx);
            assert!(!host.engine_recovering);
        });
    }

    #[gpui::test]
    fn slow_recovery_uses_a_dialog_and_recovery_cancels_the_pending_notice(
        cx: &mut gpui::TestAppContext,
    ) {
        // Exercise the real notice deadline, then cancel a later recovery before it fires.
        let (host, _, _) = test_support::host(cx);
        host.update(cx, |host, cx| {
            let mut lost = WorkerResponse::new(Err("engine stopped".into()));
            lost.engine_lost = true;
            host.receive_response(lost, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(crate::engine::restart::NOTICE_AFTER);
        cx.run_until_parked();
        let presented = cx.update(crate::ui::desktop::alert::take_presented);
        assert_eq!(presented.len(), 1);
        assert_eq!(presented[0].0, crate::ui::desktop::alert::Kind::RecoverySlow);
        host.update(cx, |host, cx| {
            let mut recovered = WorkerResponse::new(Ok(()));
            recovered.engine_recovered = true;
            host.receive_response(recovered, cx);
            let mut lost = WorkerResponse::new(Err("engine stopped".into()));
            lost.engine_lost = true;
            host.receive_response(lost, cx);
        });
        cx.run_until_parked();
        host.update(cx, |host, cx| {
            let mut recovered = WorkerResponse::new(Ok(()));
            recovered.engine_recovered = true;
            host.receive_response(recovered, cx);
        });
        cx.executor().advance_clock(crate::engine::restart::NOTICE_AFTER);
        cx.run_until_parked();
        assert!(cx.update(crate::ui::desktop::alert::take_presented).is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn transfer_handoff_allows_preparation_and_terminal_state_is_scoped(
        cx: &mut gpui::TestAppContext,
    ) {
        // A completed preparation admission cannot block the next preparation.
        // Old status and another session's work cannot retire the new admission.
        for kind in [TransferKind::Failed, TransferKind::Paused] {
            let (host, window, requests, responses) = test_support::host_with_responses(cx);
            let session = crate::identifier::encode(&[9; 32]);
            let other = crate::identifier::encode(&[10; 32]);
            host.update(cx, |host, cx| {
                host.open_retained(session.clone(), cx).unwrap();
                assert!(host.begin_launch(&other, cx));
            });
            let Message::Request(first) = requests.try_recv().unwrap() else { panic!("prepare") };
            let first_admission = first.credit.as_ref().unwrap().admission();
            let mut running = WorkerResponse::from_reply(Ok(Reply::Transfer {
                transfer: TransferStatus {
                    kind: TransferKind::Running,
                    session: Some(session.clone()),
                    ..Default::default()
                },
            }));
            running.session = Some(session.clone());
            running.credit = first.credit;
            responses.try_send(running).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(!host.pending_windows.contains_key(&session));
                assert!(host.launches.contains_key(&session));
            });
            responses
                .try_send(WorkerResponse::from_reply(Ok(Reply::Transfer {
                    transfer: TransferStatus {
                        kind: TransferKind::Complete,
                        session: Some(session.clone()),
                        ..Default::default()
                    },
                })))
                .unwrap();
            cx.run_until_parked();
            let Message::Request(next) = requests.try_recv().unwrap() else {
                panic!("prepare after transfer")
            };
            assert!(
                matches!(&next.request, Request::PrepareSession { session: id } if id == &session)
            );
            let admission = next.credit.as_ref().unwrap().admission();
            assert_ne!(admission, first_admission);
            let message = format!("App download failed: {}", "é".repeat(600));
            let terminal = TransferStatus {
                kind,
                session: Some(session.clone()),
                message: Some(message.clone()),
                ..Default::default()
            };
            responses
                .try_send(WorkerResponse::from_reply(Ok(Reply::Transfer {
                    transfer: terminal.clone(),
                })))
                .unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.joining);
                assert_eq!(host.pending_windows.get(&session), Some(&Some(admission)));
            });
            let mut response =
                WorkerResponse::from_reply(Ok(Reply::Transfer { transfer: terminal }));
            response.session = Some(session.clone());
            response.credit = next.credit;
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(!host.joining);
                assert!(!host.pending_windows.contains_key(&session));
                assert!(!host.launches.contains_key(&session));
                assert!(host.launches.contains_key(&other));
            });
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| {
                        let shown = message.chars().take(512).collect::<String>();
                        assert_eq!(
                            window
                                .accessibility_nodes_for_test()
                                .any(|(_, node)| node.value() == Some(shown.as_str())),
                            kind == TransferKind::Failed
                        );
                    })
                    .unwrap()
            });
            assert!(requests.is_empty());
        }
    }

    #[gpui::test]
    fn typed_transfer_completion_is_scoped_and_consent_retains_its_target(
        cx: &mut gpui::TestAppContext,
    ) {
        // Deliver the real reply type through the channel: unrelated and
        // duplicate completions must not start another guest.
        let (send, requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        cx.run_until_parked();
        drop(requests.try_recv().unwrap());
        host.update(cx, |host, _| {
            host.joining = true;
            host.opening = Some("intended".into());
            host.error = "Retained local error".into();
        });
        let deliver = |transfer, cx: &mut gpui::TestAppContext| {
            let response = WorkerResponse::from_reply(Ok(Reply::Transfer { transfer }));
            assert!(response.result.is_ok());
            responses.try_send(response).unwrap();
            cx.run_until_parked();
        };
        deliver(
            TransferStatus {
                kind: TransferKind::Complete,
                session: Some("other".into()),
                ..Default::default()
            },
            cx,
        );
        assert!(requests.try_recv().is_err());
        let complete = TransferStatus {
            kind: TransferKind::Complete,
            session: Some("intended".into()),
            ..Default::default()
        };
        deliver(complete.clone(), cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("prepare") };
        assert!(
            matches!(&queued.request, Request::PrepareSession { session } if session == "intended")
        );
        deliver(complete, cx);
        assert!(requests.try_recv().is_err());
        let mut response = WorkerResponse::from_reply(Ok(Reply::Transfer {
            transfer: TransferStatus {
                kind: TransferKind::Running,
                session: Some("intended".into()),
                ..Default::default()
            },
        }));
        response.session = Some("intended".into());
        response.credit = queued.credit;
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        deliver(
            TransferStatus {
                kind: TransferKind::Failed,
                session: Some("intended".into()),
                message: Some("Review access".into()),
                progress: None,
                update: Some(crate::engine::UpdateStatus {
                    kind: crate::engine::UpdateKind::Failed,
                    build_id: Some(crate::identifier::encode(&[0xab; 32])),
                    message: "Review access".into(),
                }),
            },
            cx,
        );
        host.read_with(cx, |host, _| {
            assert!(!host.joining);
            assert_eq!(host.opening.as_deref(), Some("intended"));
            assert_eq!(host.error, "Retained local error");
            assert!(!host.launches.contains_key("intended"));
            assert!(!host.pending_windows.contains_key("intended"));
        });
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn prepared_session_moves_metadata_and_finishes_windowless_launch(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        let id = crate::identifier::encode(&[0xab; 32]);
        let invitation = String::from("known session invitation");
        let allocation = invitation.as_ptr() as usize;
        let prepared = crate::engine::reply::PreparedSession {
            session: serde_json::from_value(serde_json::json!({
                "id":id, "selector":{"kind":"exact","build_id":crate::identifier::encode(&[0xcd; 32])},
                "resolved_build_id":crate::identifier::encode(&[0xcd; 32]), "root_volume":crate::identifier::encode(&[0x01; 16]),
                "app":{"publisher":crate::identifier::encode(&[0x03; 16]),"app_id":"test"}, "creator":crate::identifier::encode(&[0x03; 16]),
                "update_authority":"creator"
            }))
            .unwrap(),
            invitation,
            availability: "Local".into(),
            update: None,
        };
        host.update(cx, |host, cx| {
            host.initial_link = true;
            host.opening = Some(id.clone());
            host.joining = true;
            host.prepare_session(id.clone(), cx).unwrap();
        });
        let Message::Request(queued) = requests.try_recv().unwrap() else {
            panic!("expected the session admission");
        };
        let mut response = WorkerResponse::from_reply(Ok(Reply::SessionPrepared(prepared)));
        response.session = Some(id.clone());
        response.credit = queued.credit;
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            let selected = host.selected.as_ref().unwrap();
            assert_eq!(selected.session.id, id);
            assert_eq!(selected.invitation.as_ptr() as usize, allocation);
            assert_eq!(selected.availability, "Local");
            assert!(host.sessions_window.is_none());
            assert!(!host.initial_link && !host.joining && host.opening.is_none());
            assert!(!host.pending_windows.contains_key(&id));
        });
        assert!(requests.try_recv().is_err(), "metadata receipt admits no guest work");
    }
}

impl Shell {
    pub(super) fn apply_recovery(&mut self, state: RecoveryStatus, _cx: &mut Context<Self>) {
        self.recovery = Some(state);
    }

    pub(super) fn send_action(&mut self, request: Request, cx: &mut Context<Self>) {
        self.cancel_invitation_restoration();
        if matches!(&request, Request::RequestCode { email } if !account_form::email(email))
            || matches!(&request, Request::SignIn { code, .. } if !account_form::code(code))
            || matches!(&request, Request::SetDisplayName { name } if !account_form::display_name(name))
        {
            return;
        }
        let action = worker_recovery::Action::of(&request);
        if action.is_some() && self.recovery_busy.is_some() {
            return;
        }

        if self.send.send(request).is_ok() && action.is_some() {
            crate::trace::mark("ui.recovery_action");
            self.recovery_busy = action;
            if action == Some(worker_recovery::Action::SetDisplayName)
                && let Some(editor) = &self.name_editor
            {
                editor.update(cx, |editor, _| editor.set_enabled(false));
            }
            self.error.clear();
            cx.notify();
        }
    }
}
