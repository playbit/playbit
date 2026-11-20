//! Permission drafts replace volume defaults only after an explicit save.
use super::*;
use crate::ui::desktop::zoom::length as ui;

const FULL_ACCESS: [Permission; 4] =
    [Permission::Read, Permission::Write, Permission::Append, Permission::ManagePermissions];

fn preset_permissions(preset: u32) -> Option<Vec<Permission>> {
    match preset {
        1 => Some(vec![Permission::Read]),
        2 => Some(vec![Permission::Read, Permission::Write]),
        3 => Some(FULL_ACCESS.to_vec()),
        _ => None,
    }
}

fn permission_preset(permissions: &[Permission]) -> Option<u32> {
    match permissions.len() {
        1 if permissions.contains(&Permission::Read) => Some(1),
        2 if permissions.contains(&Permission::Read)
            && permissions.contains(&Permission::Write) =>
        {
            Some(2)
        }
        4 if FULL_ACCESS.iter().all(|flag| permissions.contains(flag)) => Some(3),
        _ => None,
    }
}

fn permission_summary(permissions: &[Permission]) -> String {
    [
        (Permission::Read, "Read"),
        (Permission::Write, "Write"),
        (Permission::Append, "Append"),
        (Permission::ManagePermissions, "Manage permissions"),
    ]
    .into_iter()
    .filter_map(|(flag, name)| permissions.contains(&flag).then_some(name))
    .collect::<Vec<_>>()
    .join(" + ")
}

fn permission_choice(
    id: u32,
    label: String,
    checked: Option<bool>,
    disabled: bool,
) -> crate::runtime::MenuEntry {
    crate::runtime::MenuEntry::Action {
        icon: None,
        id,
        label,
        checked,
        disabled,
        destructive: false,
    }
}

impl Browser {
    pub(super) fn permissions_panel(
        &mut self,
        details: &EntryDetails,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let custom = matches!(self.permissions, EntryAcl::Replace(_));
        let disabled = !details.manage_permissions || !self.local_draft_ready(cx);
        let toggle = self.permissions_toggle(custom, disabled, cx);
        toggle.update(cx, |toggle, cx| toggle.set_switch(cx));
        let mut panel = div().flex().flex_col().flex_none().child(
            self.inspector_heading(
                "custom-permissions-title",
                "Custom Permissions",
                window_active,
                cx,
            )
            .child(toggle),
        );
        if custom {
            panel = panel.child(self.custom_permission_rows(disabled, cx));
        }
        self.permission_actions(panel, disabled, cx)
    }

    fn permission_actions(
        &mut self,
        panel: gpui::Div,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.update_permission_dirty();
        if !self.dirty {
            return panel;
        }
        let save = self.button(
            "save-permissions",
            "Save permissions",
            disabled
                || !super::super::draft_publications::available(cx)
                || self.permissions_pending.is_some()
                || self
                    .permission_draft_id
                    .is_some_and(|id| super::super::draft_publications::pending(id, cx)),
            |this, cx| this.save_permissions(cx),
            cx,
        );
        let discard = self.button(
            "discard-permissions",
            "Discard changes",
            (self.selected.is_none() && self.volume_details.is_none())
                || self.permissions_pending.is_some()
                || self
                    .permission_draft_id
                    .is_some_and(|id| super::super::draft_publications::pending(id, cx)),
            |this, cx| this.discard_permissions(cx),
            cx,
        );
        panel.child(
            div()
                .flex()
                .flex_wrap()
                .gap(ui(8.))
                .pl(ui(12.))
                .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
                .pb(ui(8.))
                .child(save)
                .child(discard),
        )
    }

    fn custom_permission_rows(&mut self, disabled: bool, cx: &mut Context<Self>) -> gpui::Div {
        let EntryAcl::Replace(acl) = &self.permissions else {
            return div();
        };
        let owner = Subject::Principal(Principal::Human(self.volume.owner));
        let owner_grant = acl
            .grants
            .iter()
            .enumerate()
            .find(|(_, grant)| grant.subject == owner)
            .map(|(index, grant)| (index, grant.clone()));
        let count = acl.grants.iter().filter(|grant| grant.subject != owner).count();
        self.permission_offset = self.permission_offset.min(count.saturating_sub(1) / 16 * 16);
        let grants: Vec<_> = acl
            .grants
            .iter()
            .enumerate()
            .filter(|(_, grant)| grant.subject != owner)
            .skip(self.permission_offset)
            .take(16)
            .map(|(index, grant)| (index, grant.clone()))
            .collect();
        let mut rows = div()
            .flex()
            .flex_col()
            .gap(ui(8.))
            .pl(ui(8.))
            .pr(ui(8. + self.scrolling.inspector_clearance(cx)))
            .child(self.permission_owner_row(owner_grant.as_ref(), disabled, cx));
        for (index, grant) in grants {
            rows = rows.child(self.grant_row(index, &grant, disabled, cx));
        }
        if count > 16 {
            rows = rows.child(self.grant_navigation(count, cx));
        }
        if self.adding_grant {
            rows = rows.child(self.add_grant_row(disabled, cx));
        }
        let entry = self.selected.is_some();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .child(rows)
            .when(entry, |section| {
                section.child(
                    self.text(
                        "custom-permissions-note",
                        "This replaces volume-level permissions",
                        cx,
                    )
                    .mt(ui(16.))
                    .mx(ui(12.))
                    .h(ui(16.))
                    .line_height(ui(16.))
                    .text_size(ui(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgba(0x00000066)),
                )
            })
            .pb(ui(if entry { 24. } else { 16. }))
    }

    pub(super) fn volume_permissions_panel(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let disabled = !self.permission_edit_allowed(None) || !self.local_draft_ready(cx);
        let mut panel = div().flex().flex_col().flex_none().child(self.inspector_heading(
            "volume-permissions-title",
            "Permissions",
            window_active,
            cx,
        ));
        if self.volume_details.is_some() {
            panel = panel.child(self.custom_permission_rows(disabled, cx));
        } else if self.permission_refresh.volume {
            panel = panel.child(
                self.text("volume-permissions-refresh", "Refreshing permissions…", cx)
                    .mx(ui(12.))
                    .mb(ui(12.))
                    .text_color(gpui::black().alpha(0.5)),
            );
        }
        self.permission_actions(panel, disabled, cx)
    }

    fn permission_owner_row(
        &mut self,
        grant: Option<&(usize, Grant)>,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let access = self.permission_owner_access(grant, disabled, cx);
        let name = self.permission_human_label(self.volume.owner);
        let owner = self.volume.owner.to_string();
        let subject = div()
            .flex_1()
            .min_w_0()
            .h(ui(24.))
            .flex()
            .items_center()
            .gap(ui(5.))
            .px(ui(8.))
            .rounded(ui(6.))
            .border_1()
            .border_color(rgba(0x0000001a))
            .child(
                gpui::svg()
                    .path("permission-person")
                    .size(ui(16.))
                    .flex_none()
                    .text_color(gpui::black()),
            )
            .child(
                self.text("permission-owner", name, cx)
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui(11.))
                    .line_height(ui(24.))
                    .text_color(rgba(0x000000e6)),
            )
            .child(
                self.text("permission-owner-id", owner, cx)
                    .min_w_0()
                    .max_w(gpui::relative(0.4))
                    .truncate()
                    .text_size(ui(11.))
                    .line_height(ui(24.))
                    .text_color(rgba(0x00000080)),
            );
        div()
            .h(ui(24.))
            .flex()
            .items_center()
            .gap(ui(8.))
            .child(subject)
            .child(access)
            .child(self.permission_add_button(cx))
    }

    fn permission_owner_access(
        &mut self,
        grant: Option<&(usize, Grant)>,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        // Owners always manage permissions; classify their effective data access
        // without rewriting an older stored grant or ignoring Everyone's grants.
        if self.selected.is_none() {
            return self.permission_static_access("Owner", cx);
        }
        let full = matches!(&self.permissions, EntryAcl::Replace(acl) if
            [Permission::Read, Permission::Write, Permission::Append].into_iter().all(|flag|
                acl.permits(Some(Principal::Human(self.volume.owner)), flag)));
        if full {
            return self.permission_static_access("Full access", cx);
        }
        if let Some((index, grant)) = grant {
            div().child(self.grant_permission_popup(*index, grant, disabled, cx))
        } else {
            let mut flags = vec![Permission::ManagePermissions];
            if let EntryAcl::Replace(acl) = &self.permissions {
                for flag in FULL_ACCESS {
                    if flag != Permission::ManagePermissions
                        && acl.grants.iter().any(|grant| {
                            grant.subject == Subject::Public && grant.permissions.contains(&flag)
                        })
                    {
                        flags.push(flag);
                    }
                }
            }
            self.permission_static_access(&permission_summary(&flags), cx)
        }
    }

    fn permission_static_access(&mut self, label: &str, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .w(ui(107.))
            .h(ui(24.))
            .flex_none()
            .flex()
            .items_center()
            .pl(ui(12.))
            .pr(ui(8.))
            .rounded(ui(6.))
            .border_1()
            .border_color(rgba(0x0000001a))
            .child(
                self.text("permission-owner-access", label.to_owned(), cx)
                    .min_w_0()
                    .truncate()
                    .text_size(ui(11.))
                    .line_height(ui(24.))
                    .text_color(rgba(0x000000e6)),
            )
    }

    fn permission_human_label(&self, account: Id) -> String {
        if self.signed_in_account == Some(account) {
            self.signed_in_name
                .as_ref()
                .map_or_else(|| "You".into(), |name| format!("{name} (You)"))
        } else if account == self.volume.owner {
            "Owner".into()
        } else {
            "Account".into()
        }
    }

    pub(super) fn permission_edit_allowed(&self, entry: Option<EntryId>) -> bool {
        self.selected == entry
            && match entry {
                Some(entry) => self
                    .details
                    .as_ref()
                    .is_some_and(|details| details.entry.id == entry && details.manage_permissions),
                None => {
                    self.volume_details.is_some()
                        && self.signed_in_account == Some(self.volume.owner)
                }
            }
    }

    fn save_permissions(&mut self, cx: &mut Context<Self>) {
        self.update_permission_dirty();
        if !self.dirty
            || self.permissions_pending.is_some()
            || !super::super::draft_publications::available(cx)
            || !self.local_draft_ready(cx)
            || !self.permission_edit_allowed(self.selected)
            || self
                .permission_draft_id
                .is_some_and(|id| super::super::draft_publications::pending(id, cx))
            || !self.retain_local_drafts(cx)
        {
            return;
        }
        let Some(draft) = self.permission_draft_id.and_then(|id| self.local_draft_snapshot(id, cx))
        else {
            return;
        };
        let submitted =
            super::drafts::canonical_permissions(&self.permissions, &self.retained_permissions);
        let request = if let Some(entry) = self.selected {
            Request::EntryPermissions {
                volume: self.volume.id,
                entry,
                permissions: self.permissions.clone(),
                retained_permissions: matches!(self.permissions, EntryAcl::Inherit)
                    .then(|| self.retained_permissions.clone())
                    .flatten(),
            }
        } else {
            let EntryAcl::Replace(acl) = &self.permissions else {
                return;
            };
            Request::Permissions { volume: self.volume.id, acl: acl.clone() }
        };
        match self.send.enqueue(request) {
            Ok(admission) => {
                if self.selected.is_none() {
                    self.volume_permissions_admission = Some(admission);
                }
                self.permissions_pending = Some(submitted);
                self.permissions_admission = Some(admission);
                super::super::draft_publications::track_permissions(admission, draft, cx);
                self.error.clear();
            }
            Err(error) => self.error = error.into(),
        }
    }

    fn discard_permissions(&mut self, cx: &mut Context<Self>) {
        if (self.selected.is_none() && self.volume_details.is_none())
            || self.permissions_pending.is_some()
            || self
                .permission_draft_id
                .is_some_and(|id| super::super::draft_publications::pending(id, cx))
        {
            return;
        }
        if let Some(id) = self.permission_draft_id.take() {
            super::super::drafts::remove(id, cx);
        }
        if self.selected.is_none() {
            if let Some(details) = &self.volume_details {
                self.permissions = EntryAcl::Replace(details.permissions.clone());
                self.retained_permissions = None;
                self.permissions_base = Some(super::drafts::canonical_permissions(
                    &self.permissions,
                    &self.retained_permissions,
                ));
                self.dirty = false;
                self.changed_during_edit = false;
                self.hint.clear();
                self.permission_offset = 0;
                self.close_grant_editor(cx);
            }
            return;
        }
        if let Some(details) = &self.details {
            self.permissions = details.permissions.clone();
            self.retained_permissions = details.retained_permissions.clone();
            self.permissions_base = Some(super::drafts::canonical_permissions(
                &self.permissions,
                &self.retained_permissions,
            ));
            self.dirty = false;
            self.changed_during_edit = false;
            self.hint.clear();
            self.permission_offset = 0;
            self.close_grant_editor(cx);
        }
    }

    pub(in crate::desktop) fn volume_permissions_response_matches(
        &self,
        admission: Option<u64>,
    ) -> bool {
        self.volume_permissions_admission.is_some()
            && self.volume_permissions_admission == admission
    }

    pub(super) fn volume_permissions_saved(&mut self, cx: &mut Context<Self>) {
        let direct = super::super::draft_publications::acknowledge_permissions(
            self.volume_permissions_admission,
            self.volume.id,
            None,
            cx,
        );
        if self.selected.is_some() || self.volume_permissions_admission.take().is_none() {
            return;
        }
        let Some((EntryAcl::Replace(acl), _)) = self.permissions_pending.take().or(direct) else {
            return;
        };
        self.permissions_admission = None;
        if let Some(details) = &mut self.volume_details {
            details.permissions = acl.clone();
        }
        self.permissions_base =
            Some(super::drafts::canonical_permissions(&EntryAcl::Replace(acl.clone()), &None));
        if self.permissions == EntryAcl::Replace(acl) {
            self.dirty = false;
            self.changed_during_edit = false;
        }
        self.refresh();
        cx.notify();
    }

    pub(super) fn permissions_saved(&mut self, entry: EntryId, cx: &mut Context<Self>) {
        if self.selected != Some(entry) {
            return;
        }
        let direct = super::super::draft_publications::acknowledge_permissions(
            self.permissions_admission.take(),
            self.volume.id,
            Some(entry),
            cx,
        );
        let Some(submitted) = self.permissions_pending.take().or(direct) else {
            return;
        };
        self.permissions_base =
            Some(super::drafts::canonical_permissions(&submitted.0, &submitted.1));
        if let Some(details) = &mut self.details {
            let details = Arc::make_mut(details);
            details.permissions = submitted.0;
            details.retained_permissions = submitted.1;
        }
        self.update_permission_dirty();
        if !self.dirty {
            self.changed_during_edit = false;
            self.hint.clear();
        }
        self.did_change_draft(cx);
    }

    fn permissions_toggle(
        &mut self,
        custom: bool,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let entry = self.selected;
        self.control(
            "custom-permissions".into(),
            "Custom Permissions",
            controls::Kind::Checkbox(if custom { CheckState::On } else { CheckState::Off }),
            disabled,
            move |this, cx| {
                if !this.permission_edit_allowed(entry) {
                    return;
                }
                this.permissions = match &this.permissions {
                    EntryAcl::Inherit => EntryAcl::Replace(
                        this.retained_permissions
                            .clone()
                            .unwrap_or_else(|| this.initial_custom_permissions()),
                    ),
                    EntryAcl::Replace(acl) => {
                        this.retained_permissions = Some(acl.clone());
                        EntryAcl::Inherit
                    }
                };
                this.permission_offset = 0;
                this.close_grant_editor(cx);
                this.dirty = true;
                this.did_change_draft(cx);
            },
            cx,
        )
    }

    fn initial_custom_permissions(&self) -> Acl {
        let mut acl = self
            .details
            .as_ref()
            .map(|details| details.default_permissions.clone())
            .unwrap_or_default();
        let owner = Subject::Principal(Principal::Human(self.volume.owner));
        if let Some(grant) = acl.grants.iter_mut().find(|grant| grant.subject == owner) {
            grant.permissions = FULL_ACCESS.to_vec();
        } else {
            acl.grants.push(Grant { subject: owner, permissions: FULL_ACCESS.to_vec() });
        }
        acl
    }

    fn grant_row(
        &mut self,
        index: usize,
        grant: &Grant,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let subject = self.grant_subject_popup(index, grant, disabled, cx);
        let permissions = self.grant_permission_popup(index, grant, disabled, cx);
        let entry = self.selected;
        let target = grant.subject.clone();
        let remove = self.button(
            &format!("remove-grant-{index}"),
            "Remove permission",
            disabled,
            move |this, cx| {
                if !this.permission_edit_allowed(entry)
                    || target == Subject::Principal(Principal::Human(this.volume.owner))
                {
                    return;
                }
                if let EntryAcl::Replace(acl) = &mut this.permissions
                    && let Some(index) = acl.grants.iter().position(|grant| grant.subject == target)
                {
                    acl.grants.remove(index);
                    let owner = Subject::Principal(Principal::Human(this.volume.owner));
                    let count = acl.grants.iter().filter(|grant| grant.subject != owner).count();
                    this.permission_offset =
                        this.permission_offset.min(count.saturating_sub(1) / 16 * 16);
                    this.dirty = true;
                    this.did_change_draft(cx);
                }
            },
            cx,
        );
        remove.update(cx, |control, cx| control.set_inspector_icon("permission-remove", cx));
        div()
            .h(ui(24.))
            .flex()
            .items_center()
            .gap(ui(8.))
            .child(div().flex_1().min_w_0().child(subject))
            .child(permissions)
            .child(remove)
    }

    fn grant_permission_popup(
        &mut self,
        index: usize,
        grant: &Grant,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let current = permission_preset(&grant.permissions);
        let labels = ["Read only", "Read + Write", "Full access"];
        let mut items: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let id = index as u32 + 1;
                permission_choice(
                    id,
                    (*label).into(),
                    Some(current == Some(id)),
                    id == 3 && grant.subject == Subject::Public,
                )
            })
            .collect();
        if current.is_none() {
            items.push(permission_choice(
                0,
                format!("Custom: {}", permission_summary(&grant.permissions)),
                Some(true),
                true,
            ));
        }
        let entry = self.selected;
        let subject = grant.subject.clone();
        let popup = self.popup(
            &format!("grant-permissions-{index}"),
            current.map_or("Custom", |preset| labels[preset as usize - 1]),
            disabled,
            &items,
            move |preset, this, cx| {
                if !this.permission_edit_allowed(entry)
                    || (preset == 3 && subject == Subject::Public)
                {
                    return;
                }
                let Some(flags) = preset_permissions(preset) else {
                    return;
                };
                if let EntryAcl::Replace(acl) = &mut this.permissions
                    && let Some(grant) =
                        acl.grants.iter_mut().find(|grant| grant.subject == subject)
                    && grant.permissions != flags
                {
                    grant.permissions = flags;
                    this.dirty = true;
                    this.did_change_draft(cx);
                }
            },
            cx,
        );
        popup.update(cx, |control, cx| {
            control.set_popup_context(None, "", cx);
            control.set_button_size(107., 24., cx);
        });
        popup
    }

    fn grant_subject_popup(
        &mut self,
        index: usize,
        grant: &Grant,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let (label, detail, icon) = match grant.subject {
            Subject::Public => ("Everyone".into(), String::new(), "permission-person-popup"),
            Subject::Principal(Principal::Human(id)) => {
                (self.permission_human_label(id), id.to_string(), "permission-person-popup")
            }
            Subject::Principal(Principal::Service(id)) => {
                ("Service".into(), id.to_string(), "permission-service")
            }
        };
        let mut items = Vec::new();
        if grant.subject != Subject::Public {
            items.push(permission_choice(0, format!("{label} {detail}"), Some(true), true));
        }
        items.extend([
            permission_choice(1, "Change account…".into(), None, false),
            permission_choice(2, "Change service…".into(), None, false),
            permission_choice(
                3,
                "Everyone".into(),
                Some(grant.subject == Subject::Public),
                grant.permissions.contains(&Permission::ManagePermissions),
            ),
        ]);
        let entry = self.selected;
        let target = grant.subject.clone();
        let popup = self.popup(
            &format!("grant-subject-{index}"),
            &label,
            disabled,
            &items,
            move |choice, this, cx| {
                if !this.permission_edit_allowed(entry) || !this.has_permission_subject(&target) {
                    return;
                }
                match choice {
                    1 | 2 => this.begin_grant_editor(Some(target.clone()), choice == 2, cx),
                    3 if this.replace_grant_subject(Some(&target), Subject::Public, cx) => {
                        this.close_grant_editor(cx);
                    }
                    _ => {}
                }
            },
            cx,
        );
        popup.update(cx, |control, cx| {
            control.set_popup_context(Some(icon), &detail, cx);
        });
        popup
    }

    fn has_permission_subject(&self, target: &Subject) -> bool {
        matches!(&self.permissions, EntryAcl::Replace(acl) if acl.grants.iter().any(|grant| &grant.subject == target))
    }

    fn grant_navigation(&mut self, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        if count <= 16 {
            return div();
        }
        let offset = self.permission_offset;
        let previous = self.button(
            "grants-previous",
            "Previous permissions",
            offset == 0,
            |this, _| this.permission_offset = this.permission_offset.saturating_sub(16),
            cx,
        );
        let next = self.button(
            "grants-next",
            "Next permissions",
            offset + 16 >= count,
            |this, _| this.permission_offset += 16,
            cx,
        );
        div().flex().gap(ui(8.)).child(previous).child(next)
    }

    fn can_add_grant(&self) -> bool {
        self.permission_edit_allowed(self.selected)
            && matches!(&self.permissions, EntryAcl::Replace(acl) if acl.grants.len() < 64)
    }

    pub(super) fn permission_add_button(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let entry = self.selected;
        let add = self.button(
            "start-grant",
            "Add permission",
            !self.can_add_grant() || self.adding_grant || !self.local_draft_ready(cx),
            move |this, cx| {
                if this.permission_edit_allowed(entry) && this.can_add_grant() && !this.adding_grant
                {
                    this.begin_grant_editor(None, false, cx);
                }
            },
            cx,
        );
        add.update(cx, |control, cx| control.set_inspector_icon("permission-add", cx));
        add
    }

    fn begin_grant_editor(
        &mut self,
        target: Option<Subject>,
        service: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.local_draft_ready(cx) {
            return;
        }
        self.adding_grant = true;
        self.editing_grant = target.clone();
        self.grant_service = service;
        self.account.update(cx, |editor, cx| {
            editor.set_placeholder(if service { "Service ID" } else { "Account ID" });
            let id = match target {
                Some(Subject::Principal(Principal::Human(id) | Principal::Service(id))) => {
                    id.to_string()
                }
                _ => String::new(),
            };
            editor.acknowledge(None, &id, cx);
        });
        self.did_change_draft(cx);
    }

    pub(super) fn close_grant_editor(&mut self, cx: &mut Context<Self>) {
        let was_adding = self.adding_grant;
        self.adding_grant = false;
        self.editing_grant = None;
        self.grant_service = false;
        self.account.update(cx, |editor, cx| {
            editor.set_placeholder("Account ID");
            editor.clear(cx);
        });
        if was_adding {
            self.did_change_draft(cx);
        }
    }

    fn replace_grant_subject(
        &mut self,
        target: Option<&Subject>,
        subject: Subject,
        cx: &mut Context<Self>,
    ) -> bool {
        let EntryAcl::Replace(acl) = &mut self.permissions else {
            return false;
        };
        if target == Some(&subject) {
            return true;
        }
        if acl.grants.iter().any(|grant| grant.subject == subject) {
            self.hint = "That identity already has a permission row.".into();
            return false;
        }
        if let Some(target) = target {
            let Some(grant) = acl.grants.iter_mut().find(|grant| &grant.subject == target) else {
                return false;
            };
            if subject == Subject::Public
                && grant.permissions.contains(&Permission::ManagePermissions)
            {
                self.hint =
                    "Everyone cannot manage permissions. Choose a lower access level first.".into();
                return false;
            }
            grant.subject = subject;
        } else {
            if acl.grants.len() >= 64 {
                return false;
            }
            acl.grants.push(Grant { subject, permissions: vec![Permission::Read] });
            let owner = Subject::Principal(Principal::Human(self.volume.owner));
            let count = acl.grants.iter().filter(|grant| grant.subject != owner).count();
            self.permission_offset = count.saturating_sub(1) / 16 * 16;
        }
        self.dirty = true;
        self.did_change_draft(cx);
        true
    }

    fn grant_editor_kind(
        &mut self,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let entry = self.selected;
        let target = self.editing_grant.clone();
        let public_disabled = target.as_ref().is_some_and(|subject| matches!(&self.permissions, EntryAcl::Replace(acl) if acl.grants.iter().any(|grant| &grant.subject == subject && grant.permissions.contains(&Permission::ManagePermissions))));
        let items = [
            permission_choice(1, "Account".into(), Some(!self.grant_service), false),
            permission_choice(2, "Service".into(), Some(self.grant_service), false),
            permission_choice(3, "Everyone".into(), Some(false), public_disabled),
        ];
        let kind = self.popup(
            "grant-kind",
            if self.grant_service { "Service" } else { "Account" },
            disabled,
            &items,
            move |kind, this, cx| {
                if !this.permission_edit_allowed(entry)
                    || !this.adding_grant
                    || this.editing_grant != target
                {
                    return;
                }
                match kind {
                    1 | 2 => {
                        this.grant_service = kind == 2;
                        this.account.update(cx, |editor, _| {
                            editor.set_placeholder(if kind == 2 {
                                "Service ID"
                            } else {
                                "Account ID"
                            })
                        });
                        this.did_change_draft(cx);
                    }
                    3 if this.replace_grant_subject(target.as_ref(), Subject::Public, cx) => {
                        this.close_grant_editor(cx);
                    }
                    _ => {}
                }
            },
            cx,
        );
        kind.update(cx, |control, cx| control.set_popup_context(None, "", cx));
        kind
    }

    fn add_grant_row(&mut self, disabled: bool, cx: &mut Context<Self>) -> gpui::Div {
        let entry = self.selected;
        let target = self.editing_grant.clone();
        let cancel_target = target.clone();
        let valid = Id::parse(self.account.read(cx).text().trim()).is_ok();
        let allowed = target
            .as_ref()
            .map_or_else(|| self.can_add_grant(), |subject| self.has_permission_subject(subject));
        let add = self.button(
            "add-grant",
            if target.is_some() { "Apply identity" } else { "Add reader" },
            disabled || !valid || !allowed,
            move |this, cx| {
                if !this.permission_edit_allowed(entry)
                    || !this.adding_grant
                    || this.editing_grant != target
                {
                    return;
                }
                if let Ok(id) = Id::parse(this.account.read(cx).text().trim()) {
                    let subject = Subject::Principal(if this.grant_service {
                        Principal::Service(id)
                    } else {
                        Principal::Human(id)
                    });
                    if this.replace_grant_subject(target.as_ref(), subject, cx) {
                        this.close_grant_editor(cx);
                    }
                }
            },
            cx,
        );
        let cancel = self.button(
            "cancel-grant",
            "Cancel",
            false,
            move |this, cx| {
                if this.selected == entry && this.editing_grant == cancel_target {
                    this.close_grant_editor(cx);
                }
            },
            cx,
        );
        let kind = self.grant_editor_kind(disabled, cx);
        div()
            .flex()
            .flex_col()
            .gap(ui(8.))
            .child(kind)
            .child(
                div()
                    .flex()
                    .gap(ui(8.))
                    .child(div().flex_1().min_w(ui(160.)).child(self.account.clone()))
                    .child(add),
            )
            .child(cancel)
    }
}

#[cfg(test)]
mod tests;
