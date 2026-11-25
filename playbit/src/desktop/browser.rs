//! Cut-fenced Finder columns and one selected entry, refreshed by committed changes.
use super::*;
use crate::store::object_contract::{Acl, EntryAcl, Grant, Permission, Principal, Schema, Subject};
use crate::store::volume_index::{DatabaseProtocol, EntryDetails, EntryKind, EntryPage};
use crate::store::{EntryId, Id, Volume};
use playbit_api::ui::{ButtonStyle, CheckState, ThemeColor as Role};
mod columns;
mod creation;
mod drafts;
mod entry_menu;
mod failure;
mod files;
mod history;
mod inspector;
mod key_value;
mod layout;
mod namespace;
mod navigation;
mod panes;
mod payload_size;
mod permissions;
mod preview;
mod records;
mod schema;
mod scrolling;
mod values;
mod volume_invitation;
pub(super) use failure::Failure;
pub(super) use navigation::ListNavigation;
pub(super) use records::settle_record_publication;
#[cfg(test)]
mod tests;

pub(super) const MIN_WIDTH: f32 = 560.;
const INSPECTOR_WIDTH: f32 = 320.;
const ENTRY_PAGE_ROWS: u32 = 32;
const DETAIL_LOADING_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Default)]
struct DetailLoading {
    generation: u64,
    active: bool,
    visible: bool,
    timer: Option<gpui::Task<()>>,
}

pub(super) struct Notice(pub String);
impl gpui::EventEmitter<Notice> for Browser {}

pub struct Browser {
    pub volume: Volume,
    pub(super) volume_invitation: Option<super::volume_invitations::Review>,
    pub(super) owner_name: Option<String>,
    invitation_link_focus: gpui::FocusHandle,
    volume_details: Option<crate::store::VolumeDetails>,
    permission_refresh: failure::PermissionRefresh,
    payload_size: payload_size::PayloadSize,
    volume_permissions_admission: Option<u64>,
    available_volumes: Vec<Volume>,
    shared_volume_owners: BTreeMap<Id, Id>,
    finder_volume: bool,
    signed_in_account: Option<Id>,
    signed_in_name: Option<String>,
    permission_names: permissions::Names,
    open_target: Option<values::Target>,
    send: RequestSender,
    catalog: namespace::Catalog,
    preview: preview::Preview,
    selected: Option<EntryId>,
    database_open: bool,
    creating_entry: Option<creation::PendingCreate>,
    creating_name: Option<String>,
    creating_parent: Option<EntryId>,
    renaming: Option<creation::Rename>,
    show_entry_details: bool,
    details: Option<Arc<EntryDetails>>,
    detail_loading: DetailLoading,
    permissions: EntryAcl,
    retained_permissions: Option<Acl>,
    permissions_pending: Option<(EntryAcl, Option<Acl>)>,
    permissions_admission: Option<u64>,
    dirty: bool,
    permissions_base: Option<(EntryAcl, Option<Acl>)>,
    permission_draft_id: Option<Id>,
    draft_capture_scheduled: bool,
    draft_open: Option<Id>,
    changed_during_edit: bool,
    name: Entity<text_editor::TextEditor>,
    account: Entity<text_editor::TextEditor>,
    controls: BTreeMap<String, Entity<controls::Control>>,
    text_ids: BTreeMap<String, Entity<()>>,
    identifier_fields: BTreeMap<&'static str, Entity<text_editor::TextEditor>>,
    rendered_controls: std::collections::BTreeSet<String>,
    rendered_text: std::collections::BTreeSet<String>,
    error: String,
    hint: String,
    announced_hint: String,
    draft_focus: Option<Id>,
    alerted_error: String,
    records: records::Records,
    retired_records: Option<records::Retired>,
    kv: key_value::KeyValues,
    schema_name: Entity<text_editor::TextEditor>,
    field_name: Entity<text_editor::TextEditor>,
    field_kind: crate::store::object_contract::ValueType,
    field_repeated: bool,
    overview: bool,
    schema_offset: usize,
    permission_offset: usize,
    adding_grant: bool,
    editing_grant: Option<Subject>,
    grant_service: bool,
    adding_type: bool,
    adding_field: bool,
    files: files::Files,
    history: history::History,
    scrolling: scrolling::Scrolling,
}

impl Browser {
    pub fn new(volume: Volume, send: RequestSender, cx: &mut Context<Self>) -> Self {
        #[cfg(test)]
        if !cx.has_global::<super::drafts::Drafts>() {
            super::drafts::configure_memory(volume.owner, cx);
        }
        let weak = cx.weak_entity();
        let changed: text_editor::CommitCallback =
            Arc::new(move |_, _, cx| weak.update(cx, |_, cx| cx.notify()).is_ok());
        let grant_owner = cx.weak_entity();
        let grant_changed: text_editor::CommitCallback = Arc::new(move |_, _, cx| {
            grant_owner
                .update(cx, |this, cx| {
                    if this.adding_grant {
                        this.did_change_draft(cx);
                    }
                    cx.notify();
                })
                .is_ok()
        });
        let mut browser = Self {
            volume_invitation: None,
            owner_name: None,
            invitation_link_focus: cx.focus_handle().tab_stop(true),
            finder_volume: volume.personal,
            available_volumes: vec![volume.clone()],
            shared_volume_owners: BTreeMap::new(),
            signed_in_account: None,
            signed_in_name: None,
            permission_names: Default::default(),
            open_target: None,
            volume,
            volume_details: None,
            payload_size: Default::default(),
            volume_permissions_admission: None,
            permission_refresh: Default::default(),
            send,
            catalog: namespace::Catalog::default(),
            preview: preview::Preview::new(cx),
            selected: None,
            database_open: false,
            creating_entry: None,
            creating_name: None,
            creating_parent: None,
            renaming: None,
            show_entry_details: false,
            details: None,
            detail_loading: Default::default(),
            permissions: EntryAcl::Inherit,
            retained_permissions: None,
            permissions_pending: None,
            permissions_admission: None,
            dirty: false,
            permissions_base: None,
            permission_draft_id: None,
            draft_capture_scheduled: false,
            draft_open: None,
            changed_during_edit: false,
            name: cx.new(|cx| {
                text_editor::TextEditor::new("", cx)
                    .literal()
                    .placeholder("Name or path")
                    .bound_edits(480)
                    .on_change(changed.clone())
            }),
            account: cx.new(|cx| {
                text_editor::TextEditor::new("", cx)
                    .literal()
                    .placeholder("Account ID")
                    .bound_edits(32)
                    .on_change(grant_changed)
            }),
            controls: BTreeMap::new(),
            text_ids: BTreeMap::new(),
            identifier_fields: BTreeMap::new(),
            rendered_controls: Default::default(),
            rendered_text: Default::default(),
            error: String::new(),
            hint: String::new(),
            announced_hint: String::new(),
            draft_focus: None,
            alerted_error: String::new(),
            records: Default::default(),
            retired_records: None,
            kv: key_value::KeyValues::new(cx),
            schema_name: cx.new(|cx| {
                text_editor::TextEditor::new("", cx)
                    .literal()
                    .placeholder("New type name")
                    .bound_edits(255)
                    .on_change(changed.clone())
            }),
            field_name: cx.new(|cx| {
                text_editor::TextEditor::new("", cx)
                    .literal()
                    .placeholder("New field name")
                    .bound_edits(255)
                    .on_change(changed)
            }),
            field_kind: crate::store::object_contract::ValueType::Text,
            field_repeated: false,
            overview: true,
            schema_offset: 0,
            permission_offset: 0,
            adding_grant: false,
            editing_grant: None,
            grant_service: false,
            adding_type: false,
            adding_field: false,
            files: Default::default(),
            history: Default::default(),
            scrolling: scrolling::Scrolling::new(cx),
        };
        browser.refresh();
        browser
    }

    pub(in crate::desktop) fn show_hint(&mut self, message: &str, cx: &mut Context<Self>) {
        self.hint = message.into();
        cx.notify();
    }

    pub(super) fn set_account(&mut self, account: Id, display_name: Option<&str>) -> bool {
        let display_name = display_name.map(str::trim).filter(|name| !name.is_empty());
        if self.signed_in_account == Some(account) && self.signed_in_name.as_deref() == display_name
        {
            return false;
        }
        if self.signed_in_account != Some(account) {
            self.permission_names.reset_account();
        }
        self.signed_in_account = Some(account);
        self.signed_in_name = display_name.map(str::to_owned);
        true
    }

    fn request_session(&self) -> Option<Box<[u8; 32]>> {
        self.shared_session().map(Box::new)
    }

    pub(super) fn shared_session(&self) -> Option<[u8; 32]> {
        self.send.1
    }

    pub fn refresh(&mut self) {
        self.invalidate_payload_size();
        self.refresh_catalog();
        self.submit(Request::VolumeDetails {
            session: self.request_session(),
            volume: self.volume.id,
        });
        if let Some(entry) = self.selected {
            self.submit(Request::EntryDetails {
                session: self.request_session(),
                volume: self.volume.id,
                entry,
            });
        }
        self.refresh_history();
        self.refresh_picker();
    }

    fn submit(&mut self, request: Request) -> bool {
        self.error = self.send.send(request).err().unwrap_or_default().into();
        self.error.is_empty()
    }

    fn select(&mut self, entry: EntryId, cx: &mut Context<Self>) {
        if self.entry_deletion_pending() || self.entry_is_hidden(entry) {
            return;
        }
        if self.selected == Some(entry) && self.kv_address().is_some() && self.kv_editor_open() {
            self.show_kv_store(cx);
            return;
        }
        if self.selected == Some(entry)
            && self.records.type_id.is_none()
            && !self
                .catalog_entry(entry)
                .is_some_and(|row| row.kind == crate::store::EntryType::Directory)
            && !self
                .details
                .as_ref()
                .is_some_and(|details| matches!(details.entry.info.kind, EntryKind::Directory {}))
        {
            return;
        }
        if !self.prepare_entry_navigation(cx) {
            return;
        }
        self.clear_blob_preview(cx);
        self.database_open = false;
        self.show_entry_details = false;
        self.hint.clear();
        self.open_target = None;
        self.name.update(cx, |editor, cx| editor.clear(cx));
        if self.selected != Some(entry) {
            self.catalog_navigation_changed();
        }
        self.selected = Some(entry);
        self.permission_refresh.entry = false;
        self.invalidate_payload_size();
        self.scrolling.reveal_entry = self
            .catalog_entry(entry)
            .is_none_or(|row| row.kind == crate::store::EntryType::Directory)
            .then(|| (entry, self.scrolling.columns.read(cx).handle().offset().x));
        self.select_catalog_entry(entry);
        self.details = None;
        self.dirty = false;
        self.permissions_base = None;
        self.permission_draft_id = None;
        self.draft_open = None;
        self.changed_during_edit = false;
        self.records = Default::default();
        self.kv = key_value::KeyValues::new(cx);
        self.reset_history();
        self.retained_permissions = None;
        self.permissions_pending = None;
        self.permissions_admission = None;
        self.overview = true;
        self.schema_offset = 0;
        self.permission_offset = 0;
        self.close_grant_editor(cx);
        self.adding_type = false;
        self.adding_field = false;
        self.submit(Request::EntryDetails {
            session: self.request_session(),
            volume: self.volume.id,
            entry,
        });
        self.start_detail_loading(cx);
        cx.notify();
    }

    fn detail_is_loading(&self) -> bool {
        if self.selected.is_none() || self.permission_refresh.entry || !self.error.is_empty() {
            return false;
        }
        if self.details.is_none() {
            return true;
        }
        if self.overview {
            return false;
        }
        self.kv_is_loading()
            || (self.database_address().is_some()
                && self.records.type_id.is_some()
                && (self.records.page.is_none()
                    || (self.records.selected.is_some() && self.records.object.is_none())))
    }

    fn start_detail_loading(&mut self, cx: &mut Context<Self>) {
        self.cancel_detail_loading();
        if !self.detail_is_loading() {
            return;
        }
        self.detail_loading.active = true;
        let generation = self.detail_loading.generation;
        let entry = self.selected;
        self.detail_loading.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DETAIL_LOADING_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.detail_loading.generation == generation && this.selected == entry {
                    this.detail_loading.timer = None;
                    if this.detail_is_loading() {
                        this.detail_loading.visible = true;
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn cancel_detail_loading(&mut self) {
        self.detail_loading.generation = self.detail_loading.generation.wrapping_add(1);
        self.detail_loading.active = false;
        self.detail_loading.visible = false;
        self.detail_loading.timer = None;
    }

    fn settle_detail_loading(&mut self) {
        if !self.detail_is_loading() && self.detail_loading.active {
            self.cancel_detail_loading();
        }
    }

    pub(in crate::desktop) fn is_showing_volume(&self) -> bool {
        self.selected.is_none()
    }

    pub(in crate::desktop) fn has_pending_entry_operation(&self) -> bool {
        self.creating_entry.is_some() || self.files.busy() || self.renaming.is_some()
    }

    pub(in crate::desktop) fn show_volume(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.prepare_navigation(cx) {
            return false;
        }
        self.clear_selected_view(cx);
        true
    }

    fn clear_selected_view(&mut self, cx: &mut Context<Self>) {
        self.invalidate_payload_size();
        self.catalog.columns.truncate(1);
        self.catalog.columns[0].selected = None;
        self.clear_entry_view(cx);
    }

    fn clear_entry_view(&mut self, cx: &mut Context<Self>) {
        if self.retired_kv_draft_pending() || self.retired_record_draft_pending() {
            self.overview = true;
            cx.notify();
            return;
        }
        self.scrolling.reveal_entry = None;
        self.catalog_navigation_changed();
        self.clear_blob_preview(cx);
        self.selected = None;
        self.permission_refresh.entry = false;
        self.dirty = false;
        self.permission_draft_id = None;
        self.permissions_base = None;
        if let Some(details) = &self.volume_details {
            self.permissions = EntryAcl::Replace(details.permissions.clone());
            self.retained_permissions = None;
            self.permissions_base =
                Some(drafts::canonical_permissions(&self.permissions, &self.retained_permissions));
        }
        self.cancel_detail_loading();
        self.details = None;
        self.database_open = false;
        self.open_target = None;
        self.show_entry_details = false;
        self.overview = true;
        self.records = Default::default();
        self.kv = key_value::KeyValues::new(cx);
        self.reset_history();
        self.retained_permissions = None;
        self.permissions_pending = None;
        self.permissions_admission = None;
        self.close_grant_editor(cx);
        self.hint.clear();
        self.restore_permission_draft(cx);
        cx.notify();
    }

    fn has_entry_draft(&self, cx: &App) -> bool {
        self.dirty
            || self.adding_grant
            || self.retired_record_draft_pending()
            || self.records.drafts.values().any(|draft| draft.editor.read(cx).is_composing())
            || self
                .records
                .values
                .as_ref()
                .is_some_and(|values| values.input.read(cx).is_composing())
            || self.permissions_pending.is_some()
            || self.renaming.is_some()
            || self.has_record_draft()
            || self.has_kv_draft()
            || self.kv_is_composing(cx)
    }

    #[cfg(test)]
    pub(super) fn has_unsaved_changes(&self, cx: &App) -> bool {
        self.has_entry_draft(cx) || self.creating_entry.is_some() || self.files.busy()
    }

    pub(super) fn open_database(&mut self, entry: EntryId, cx: &mut Context<Self>) {
        if !self.prepare_entry_navigation(cx) {
            return;
        }
        self.select(entry, cx);
        self.database_open = true;
    }

    pub fn receive(&mut self, reply: Reply, cx: &mut Context<Self>) {
        let picker_received = self.receive_picker(&reply, cx);
        self.receive_entry_action(&reply, cx);
        match reply {
            Reply::VolumeDetails { details } if details.volume.id == self.volume.id => {
                let refreshed = std::mem::take(&mut self.permission_refresh.volume);
                if self.volume_details.as_ref() == Some(&details) && !refreshed {
                    return;
                }
                if self.selected.is_none() {
                    if self.dirty {
                        self.changed_during_edit |= self
                            .permissions_base
                            .as_ref()
                            .is_some_and(|(active, _)| {
                                !matches!(active, EntryAcl::Replace(acl) if *acl == details.permissions)
                            });
                    } else {
                        self.permissions = EntryAcl::Replace(details.permissions.clone());
                        self.retained_permissions = None;
                        self.permissions_base = Some(drafts::canonical_permissions(
                            &self.permissions,
                            &self.retained_permissions,
                        ));
                    }
                }
                self.invalidate_payload_size();
                self.volume = details.volume.clone();
                self.volume_details = Some(details);
                if self.selected.is_none() {
                    self.restore_permission_draft(cx);
                }
            }
            Reply::Permissions { permissions_updated: true } if self.selected.is_none() => {
                self.volume_permissions_saved(cx);
            }
            Reply::VolumeEntry { volume, entry } if volume == self.volume.id => {
                let created =
                    self.creating_entry.as_ref().is_some_and(|pending| pending.matches(&entry))
                        && self.creating_name.as_deref() == Some(entry.info.name.as_str())
                        && entry.info.namespace.parent == self.creating_parent;
                if !created {
                    return;
                }
                self.creating_entry = None;
                self.creating_name = None;
                self.finish_created_entry(entry, cx);
            }
            Reply::DirectoryEntries { volume, target, cut_requested, after, listing } => {
                if !self.receive_directory(volume, target, cut_requested, after, listing, cx) {
                    return;
                }
            }
            Reply::EntryDetails { volume, details }
                if volume == self.volume.id && Some(details.entry.id) == self.selected =>
            {
                let refreshed = std::mem::take(&mut self.permission_refresh.entry);
                if !self.receive_details(details, cx) && !picker_received && !refreshed {
                    return;
                }
            }
            Reply::EntryPermissions { volume, entry } if volume == self.volume.id => {
                self.permissions_saved(entry, cx);
                self.reset_history();
                self.refresh();
            }
            Reply::EntryChanged { volume, entry } if volume == self.volume.id => {
                if self
                    .renaming
                    .as_ref()
                    .is_some_and(|rename| rename.entry == entry && rename.submitted.is_some())
                {
                    self.renaming = None;
                }
                self.clear_blob_preview(cx);
                self.reset_history();
                self.refresh();
            }
            Reply::Diagnostic(reply) => {
                if let crate::engine::reply::DiagnosticReply::PayloadSize { volume, entry, bytes } =
                    reply
                {
                    self.receive_payload_size(volume, entry, Ok(bytes), cx);
                    return;
                }
                if !self.receive_file(reply, cx) {
                    return;
                }
            }
            reply => {
                let Some(reply) = self.receive_history(reply) else {
                    cx.notify();
                    return;
                };
                if !self.receive_record(reply, cx) && !picker_received {
                    return;
                }
            }
        }
        self.settle_detail_loading();
        cx.notify();
    }

    fn project_deleted_details(
        &mut self,
        mut details: EntryDetails,
        cx: &mut Context<Self>,
    ) -> Option<EntryDetails> {
        details.entry.deleted |= self.entry_is_hidden(details.entry.id);
        if details.entry.deleted {
            let hidden = namespace::hidden(
                &self.catalog_summaries(),
                &std::collections::BTreeSet::from([details.entry.id]),
            );
            for row in self
                .catalog
                .columns
                .iter_mut()
                .flat_map(|column| column.page.iter_mut().flat_map(|page| &mut page.entries))
            {
                row.deleted |= hidden.contains(&row.id);
            }
            if !self.has_entry_draft(cx) {
                self.clear_selected_view(cx);
                return None;
            }
            details.readable = false;
            details.writable = false;
            details.appendable = false;
            details.manage_permissions = false;
            if details.retained_readable {
                // Retain an existing draft for explicit discard, with no mutation controls.
                self.retain_composing_kv_draft(cx);
                self.details = Some(Arc::new(details));
                self.show_hint("This entry was deleted. Your local draft is retained.", cx);
                return None;
            }
        }
        Some(details)
    }

    fn receive_details(&mut self, details: EntryDetails, cx: &mut Context<Self>) -> bool {
        if details.entry.deleted || !details.readable || self.entry_is_hidden(details.entry.id) {
            self.clear_blob_preview(cx);
        }
        let Some(details) = self.project_deleted_details(details, cx) else { return true };
        if self.database_open {
            self.database_open = false;
            if details.entry.deleted
                || !details.readable
                || !matches!(
                    details.entry.info.kind,
                    EntryKind::KvDb {} | EntryKind::ObjectDb { .. }
                )
            {
                self.error = "The reference does not identify an accessible database.".into();
                self.details = None;
                self.selected = None;
                return true;
            }
        }
        let changed = self.details.as_deref() != Some(&details);
        if self.details.is_none() && matches!(details.entry.info.kind, EntryKind::KvDb {}) {
            self.overview = false;
        }
        let opening = self.open_target.is_some();
        if self.dirty {
            self.changed_during_edit |= self.details.as_ref().is_some_and(|previous| {
                previous.permissions != details.permissions
                    || previous.retained_permissions != details.retained_permissions
            });
        } else {
            self.permissions = details.permissions.clone();
            self.retained_permissions = details.retained_permissions.clone();
            self.permissions_base =
                Some(drafts::canonical_permissions(&self.permissions, &self.retained_permissions));
            self.name.update(cx, |editor, cx| {
                if editor.text() != details.entry.info.name {
                    editor.acknowledge(None, &details.entry.info.name, cx);
                }
            });
        }
        if changed {
            self.invalidate_payload_size();
            self.details = Some(Arc::new(details));
        }
        if self.details.as_ref().is_some_and(|details| !details.retained_readable) {
            self.reset_history();
        }
        let history_changed = self.refresh_history();
        self.update_record_selection(cx);
        let kv_changed = self.update_kv_selection(cx);
        self.finish_open_target(cx);
        self.restore_permission_draft(cx);
        self.finish_local_draft_open(cx);
        if self.details.as_ref().is_some_and(|details| details.entry.deleted)
            && !self.has_entry_draft(cx)
        {
            self.clear_selected_view(cx);
            return true;
        }
        self.refresh_blob_preview(cx);
        changed || opening || history_changed || kv_changed
    }

    fn deleted_name_discard(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Entity<controls::Control>> {
        let rename = self.renaming.as_ref()?;
        let entry = rename.entry;
        if !self.entry_is_hidden(entry)
            && !self
                .details
                .as_ref()
                .is_some_and(|details| details.entry.id == entry && details.entry.deleted)
        {
            return None;
        }
        let pending = rename.submitted.is_some();
        Some(self.button(
            "discard-deleted-name",
            "Discard Name",
            pending,
            move |this, cx| {
                if this
                    .renaming
                    .as_ref()
                    .is_some_and(|rename| rename.entry == entry && rename.submitted.is_none())
                {
                    this.renaming = None;
                    if !this.has_entry_draft(cx) {
                        this.clear_selected_view(cx);
                    }
                }
            },
            cx,
        ))
    }

    fn text(
        &mut self,
        key: &str,
        value: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        host_text::element(value.into(), self.text_id(key, cx), false, false)
    }

    fn text_id(&mut self, key: &str, cx: &mut Context<Self>) -> gpui::accesskit::NodeId {
        self.rendered_text.insert(key.to_owned());
        let identity = self.text_ids.entry(key.to_owned()).or_insert_with(|| cx.new(|_| ()));
        text_editor::accessibility_text_ids(identity.entity_id()).0
    }

    fn control(
        &mut self,
        key: String,
        label: &str,
        kind: controls::Kind,
        disabled: bool,
        activate: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        self.rendered_controls.insert(key.clone());
        let theme = theme();
        let properties = controls::Properties { kind, label, disabled, theme: &theme };
        let weak = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                let _timing = crate::trace::Span::new("ui.browser.action");
                activate(this, cx);
                cx.notify();
            });
        });
        let control = self.controls.entry(key).or_insert_with(|| {
            cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
        });
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
        });
        control.clone()
    }

    fn button(
        &mut self,
        key: &str,
        label: &str,
        disabled: bool,
        activate: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        self.control(
            key.into(),
            label,
            controls::Kind::Button(ButtonStyle::Default),
            disabled,
            activate,
            cx,
        )
    }

    fn popup(
        &mut self,
        key: &str,
        label: &str,
        disabled: bool,
        items: &[crate::runtime::MenuEntry],
        activate: impl Fn(u32, &mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let control =
            self.control(key.into(), label, controls::Kind::Popup, disabled, |_, _| {}, cx);
        let weak = cx.weak_entity();
        control.update(cx, |control, cx| {
            control.set_popup_items(items, cx);
            control.set_activation(Rc::new(move |id, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let _timing = crate::trace::Span::new("ui.browser.action");
                    activate(id, this, cx);
                    cx.notify();
                });
            }));
        });
        control
    }
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _timing = crate::trace::Span::new("ui.browser.render");
        self.rendered_controls.clear();
        self.rendered_text.clear();
        self.settle_detail_loading();
        if self.detail_is_loading() && !self.detail_loading.active {
            self.start_detail_loading(cx);
        }
        self.request_payload_size();
        self.permission_names.rendered = false;
        let panes = self.panes(window, cx);
        self.permission_names.finish_render();
        self.focus_kv_editor(window, cx);
        self.focus_rename(window, cx);
        if self.error.is_empty() {
            self.alerted_error.clear();
        } else if self.error != self.alerted_error {
            self.alerted_error = self.error.clone();
            crate::ui::desktop::alert::show_in_window(
                crate::ui::desktop::alert::Kind::Recoverable,
                &self.error,
                window.window_handle(),
                cx,
            );
        }
        let notice = self
            .kv_status_hint()
            .or({
                if self.changed_during_edit {
                    Some("Permissions changed on another device. Review before saving.")
                } else if self.records.changed_during_edit {
                    Some("This record changed on another device. Review your draft before saving.")
                } else {
                    None
                }
            })
            .unwrap_or(&self.hint)
            .to_owned();
        if notice != self.announced_hint {
            self.announced_hint = notice.clone();
            if !notice.is_empty() {
                cx.emit(Notice(notice));
            }
        }
        if let Some(id) = self.draft_focus {
            let kind = self.local_draft_snapshot(id, cx).map(|draft| draft.summary().target);
            match kind {
                Some(crate::store::browser_drafts::BrowserDraftTarget::Permissions) => {
                    if self.adding_grant {
                        self.account.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
                        self.draft_focus = None;
                    } else if let Some(control) = self.controls.get("save-permissions").cloned() {
                        control.update(cx, |control, cx| control.focus_without_ring(window, cx));
                        self.draft_focus = None;
                    }
                }
                Some(crate::store::browser_drafts::BrowserDraftTarget::Record { .. }) => {
                    let editor =
                        self.records.values.as_ref().map(|values| values.input.clone()).or_else(
                            || {
                                self.records
                                    .drafts
                                    .values()
                                    .find(|draft| draft.dirty)
                                    .map(|draft| draft.editor.clone())
                            },
                        );
                    if let Some(editor) = editor {
                        editor.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
                        self.draft_focus = None;
                    }
                }
                _ => self.draft_focus = None,
            }
        }
        self.retire_controls(window, cx);
        panes
    }
}
