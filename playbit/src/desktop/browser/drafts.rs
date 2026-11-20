//! Editor recovery belongs to the profile, not to the currently visible column.
use super::super::drafts as repository;
use super::*;
use crate::store::browser_drafts::{
    BrowserDraft, BrowserDraftBody, BrowserDraftSummary, BrowserDraftTarget, PermissionDraft,
};

pub(super) fn canonical_permissions(
    active: &EntryAcl,
    retained: &Option<Acl>,
) -> (EntryAcl, Option<Acl>) {
    (
        active.clone(),
        match active {
            EntryAcl::Replace(acl) => Some(acl.clone()),
            EntryAcl::Inherit => retained.clone(),
        },
    )
}

impl Browser {
    pub(super) fn local_draft_account(&self) -> Id {
        self.signed_in_account.unwrap_or(self.volume.owner)
    }
    pub(super) fn local_draft_ready(&self, cx: &App) -> bool {
        repository::ready_for(self.local_draft_account(), cx)
    }
    pub(super) fn local_draft_snapshot(&self, id: Id, cx: &App) -> Option<Arc<BrowserDraft>> {
        repository::get_for(self.local_draft_account(), id, cx)
    }
    pub(super) fn local_draft_summaries(&self, cx: &App) -> Vec<BrowserDraftSummary> {
        repository::snapshots_for(self.local_draft_account(), cx)
    }

    pub(in crate::desktop) fn sync_local_drafts(&mut self, cx: &mut Context<Self>) {
        if !self.local_draft_ready(cx) {
            return;
        }
        if let Some(id) = self.permission_draft_id
            && self.permissions_pending.is_none()
            && !self.draft_capture_scheduled
        {
            if let Some(draft) = self.local_draft_snapshot(id, cx) {
                if let BrowserDraftBody::Permissions(state) = &draft.body {
                    self.permissions_base =
                        Some((state.base_active.clone(), state.base_retained.clone()));
                    self.update_permission_dirty();
                }
            } else {
                self.permission_draft_id = None;
                self.permissions_base =
                    Some(canonical_permissions(&self.permissions, &self.retained_permissions));
                self.dirty = false;
                self.changed_during_edit = false;
            }
        }
        self.restore_permission_draft(cx);
        self.sync_kv_local_draft(cx);
        self.sync_record_local_draft(cx);
        self.finish_local_draft_open(cx);
        cx.notify();
    }
    pub(super) fn did_change_draft(&mut self, cx: &mut Context<Self>) {
        if self.draft_capture_scheduled {
            return;
        }
        self.draft_capture_scheduled = true;
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| {
                this.draft_capture_scheduled = false;
                this.retain_local_drafts(cx);
                cx.notify();
            });
        });
    }

    pub(super) fn store_local_draft(
        &mut self,
        body: BrowserDraftBody,
        entry: Option<EntryId>,
        existing: Option<Id>,
        cx: &mut Context<Self>,
    ) -> Result<Id, String> {
        if !self.local_draft_ready(cx) {
            return Err(
                "Local changes for this account are not ready. Your editor is unchanged.".into()
            );
        }
        let id = match existing {
            Some(id) => id,
            None => {
                let mut bytes = [0; 16];
                getrandom::fill(&mut bytes)
                    .map_err(|error| format!("Couldn’t create a local draft identity: {error}"))?;
                Id::from_bytes(bytes)
            }
        };
        let ancestors = self
            .catalog
            .columns
            .iter()
            .filter_map(|column| column.parent)
            .take_while(|id| Some(*id) != entry)
            .collect();
        repository::upsert(
            BrowserDraft { id, revision: 1, volume: self.volume.id, entry, ancestors, body },
            cx,
        )
    }

    pub(super) fn find_local_draft(
        &self,
        entry: Option<EntryId>,
        matches: impl Fn(&BrowserDraftTarget) -> bool,
        cx: &App,
    ) -> Option<Id> {
        self.local_draft_summaries(cx)
            .into_iter()
            .find(|draft| {
                draft.volume == self.volume.id && draft.entry == entry && matches(&draft.target)
            })
            .map(|draft| draft.id)
    }

    pub(super) fn local_draft_error(&mut self, error: &str, cx: &mut Context<Self>) {
        self.error = format!(
            "Couldn’t keep your local changes. Your edits are retained in this window. {error}"
        );
        self.alerted_error = self.error.clone();
        crate::ui::desktop::alert::show(
            crate::ui::desktop::alert::Kind::LocalDraft,
            &self.error,
            cx,
        );
        cx.notify();
    }

    pub(in crate::desktop) fn retain_local_drafts(&mut self, cx: &mut Context<Self>) -> bool {
        self.update_permission_dirty();
        let result = self
            .capture_permission_draft(cx)
            .and_then(|()| self.capture_kv_draft(cx))
            .and_then(|()| self.capture_record_draft(cx));
        if let Err(error) = result {
            self.local_draft_error(&error, cx);
            return false;
        }
        true
    }

    pub(super) fn update_permission_dirty(&mut self) {
        if let Some(base) = &self.permissions_base {
            self.dirty =
                canonical_permissions(&self.permissions, &self.retained_permissions) != *base;
        }
    }

    fn capture_permission_draft(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let publishing = self
            .permission_draft_id
            .is_some_and(|id| super::super::draft_publications::pending(id, cx));
        if !self.dirty && !self.adding_grant && self.permissions_pending.is_none() && !publishing {
            if let Some(id) = self.permission_draft_id.take() {
                repository::remove(id, cx);
            }
            return Ok(());
        }
        let Some((base_active, base_retained)) = self.permissions_base.clone() else {
            return Ok(());
        };
        let (active, retained) =
            canonical_permissions(&self.permissions, &self.retained_permissions);
        let existing = self.permission_draft_id.or_else(|| {
            self.find_local_draft(
                self.selected,
                |target| matches!(target, BrowserDraftTarget::Permissions),
                cx,
            )
        });
        self.permission_draft_id = Some(self.store_local_draft(
            BrowserDraftBody::Permissions(PermissionDraft {
                base_active,
                base_retained,
                active,
                retained,
                adding_grant: self.adding_grant,
                grant_input: self.account.read(cx).text().to_owned(),
                grant_target: self.editing_grant.clone(),
                grant_service: self.grant_service,
            }),
            self.selected,
            existing,
            cx,
        )?);
        Ok(())
    }

    pub(super) fn restore_permission_draft(&mut self, cx: &mut Context<Self>) {
        if self.permission_draft_id.is_some() {
            return;
        }
        let Some(id) = self.find_local_draft(
            self.selected,
            |target| matches!(target, BrowserDraftTarget::Permissions),
            cx,
        ) else {
            return;
        };
        let Some(draft) = self.local_draft_snapshot(id, cx) else {
            return;
        };
        let BrowserDraftBody::Permissions(state) = &draft.body else {
            return;
        };
        if !self.permission_edit_allowed(self.selected) {
            return;
        }
        let current = canonical_permissions(&self.permissions, &self.retained_permissions);
        self.permissions_base = Some((state.base_active.clone(), state.base_retained.clone()));
        self.permissions = state.active.clone();
        self.retained_permissions = state.retained.clone();
        self.permission_draft_id = Some(id);
        self.adding_grant = state.adding_grant;
        self.editing_grant = state.grant_target.clone();
        self.grant_service = state.grant_service;
        self.account.update(cx, |editor, cx| {
            editor.set_placeholder(if state.grant_service { "Service ID" } else { "Account ID" });
            editor.acknowledge(None, &state.grant_input, cx);
        });
        self.changed_during_edit =
            current != (state.base_active.clone(), state.base_retained.clone());
        self.update_permission_dirty();
    }

    pub(in crate::desktop) fn open_local_draft(&mut self, id: Id, cx: &mut Context<Self>) -> bool {
        let Some(draft) = self.local_draft_snapshot(id, cx) else {
            return false;
        };
        if draft.volume != self.volume.id || !self.retain_local_drafts(cx) {
            return false;
        }
        self.draft_open = Some(id);
        if let Some(entry) = draft.entry {
            if self.selected != Some(entry) {
                self.select(entry, cx);
            }
            self.draft_open = Some(id);
            self.seek_entry(entry);
        } else {
            self.show_volume(cx);
            self.draft_open = Some(id);
        }
        self.finish_local_draft_open(cx);
        cx.notify();
        true
    }

    pub(super) fn finish_local_draft_open(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.draft_open else {
            return;
        };
        let Some(draft) = self.local_draft_snapshot(id, cx) else {
            self.draft_open = None;
            return;
        };
        if self.selected != draft.entry || (draft.entry.is_some() && self.details.is_none()) {
            return;
        }
        match &draft.body {
            BrowserDraftBody::Permissions(_) => {
                self.restore_permission_draft(cx);
                self.show_entry_details = true;
                self.draft_focus = Some(id);
            }
            BrowserDraftBody::Kv(_) => {
                if !self.restore_kv_draft(id, cx) {
                    return;
                }
                self.kv.focus_key = true;
                self.overview = false;
            }
            BrowserDraftBody::Record(_) => {
                if !self.restore_record_draft(id, cx) {
                    return;
                }
                self.draft_focus = Some(id);
                self.overview = false;
            }
        }
        self.draft_open = None;
        // Focus is handed to a real editor after its column has been laid out.
        self.scrolling.reveal_entry = None;
    }

    pub(super) fn draft_for_entry(&self, entry: EntryId, cx: &App) -> Option<Id> {
        self.local_draft_summaries(cx)
            .into_iter()
            .find(|draft| {
                draft.volume == self.volume.id
                    && (draft.entry == Some(entry) || draft.ancestors.contains(&entry))
            })
            .map(|draft| draft.id)
    }

    pub(super) fn draft_button(
        &mut self,
        id: Id,
        occurrence: &str,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let button = self.button(
            &format!("draft-{occurrence}-{id}"),
            "Contains draft changes",
            false,
            move |this, cx| {
                this.open_local_draft(id, cx);
            },
            cx,
        );
        button.update(cx, |button, cx| button.set_navigation_accessory_icon("draft-changes", cx));
        button
    }
}
