//! A selected database retains one bounded page, one record and sixteen editors.
//! Current read permission gates every reply; drafts survive ordinary snapshots.
use super::*;
use crate::store::object_contract::{FieldDeclaration, ObjectFields, Value, ValueType};
use crate::store::volume_index::EntryAddress;
use crate::ui::desktop::typography::tabular_numerals;
use crate::ui::desktop::zoom::length as ui;
mod drafts;
pub(super) use drafts::Retired;
pub(in crate::desktop) use drafts::settle_record_publication;

const FIELD_PAGE: usize = 16;
const MAX_DRAFT_BYTES: usize = 8192;

#[derive(Default)]
pub(super) struct Records {
    pub draft_id: Option<Id>,
    pub recovery_pending: bool,
    pub type_id: Option<Id>,
    pub after: Option<Id>,
    pub page: Option<Arc<crate::store::ObjectPage>>,
    pub selected: Option<Id>,
    pub object: Option<Arc<ObjectFields>>,
    pub drafts: BTreeMap<Id, FieldDraft>,
    pub changed_during_edit: bool,
    pub creating: bool,
    pub field_offset: usize,
    pub values: Option<super::values::Editor>,
}

pub(super) struct FieldDraft {
    pub editor: Entity<text_editor::TextEditor>,
    pub dirty: bool,
    pub revision: u64,
    pub submitted: Option<(u64, Option<Vec<Value>>)>,
    pub base_values: Option<Vec<Value>>,
}

impl Browser {
    pub(super) fn database_address(&self) -> Option<EntryAddress> {
        let details = self.details.as_ref()?;
        if !details.readable
            || details.entry.deleted
            || !details.backing_ready
            || !matches!(details.entry.info.kind, EntryKind::ObjectDb { .. })
        {
            return None;
        }
        Some(EntryAddress { volume: self.volume.id, entry: details.entry.id })
    }

    pub(super) fn update_record_selection(&mut self, cx: &mut Context<Self>) {
        if self.database_address().is_none() {
            self.retire_record_selection(cx);
            return;
        }
        self.restore_retired_record_selection(cx);
        let Some(details) = &self.details else { return };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else { return };
        if !self.records.type_id.is_some_and(|id| schema.types.contains_key(&id)) {
            let first = schema.types.keys().next().copied();
            self.retire_record_selection(cx);
            if self.retired_record_draft_pending() {
                return;
            }
            self.records = Records { type_id: first, ..Default::default() };
        }
        self.refresh_records();
        cx.notify();
    }

    fn refresh_records(&mut self) {
        let Some(database) = self.database_address() else { return };
        if let Some(type_id) = self.records.type_id {
            self.submit(Request::Objects {
                session: self.request_session(),
                database,
                type_id,
                after: self.records.after,
                limit: 64,
            });
        }
        if let Some(object) = self.records.selected {
            self.submit(Request::ReadObject { session: self.request_session(), database, object });
        }
    }

    pub(super) fn select_type(&mut self, type_id: Id, cx: &mut Context<Self>) {
        if self.database_address().is_none() || !self.prepare_entry_navigation(cx) {
            return;
        }
        self.scrolling.list.0.borrow().base_handle.set_offset(gpui::point(px(0.), px(0.)));
        self.records = Records { type_id: Some(type_id), ..Default::default() };
        self.overview = false;
        self.refresh_records();
        self.start_detail_loading(cx);
        cx.notify();
    }

    pub(super) fn select_record(&mut self, object: Id, cx: &mut Context<Self>) {
        if self.database_address().is_none() || !self.prepare_entry_navigation(cx) {
            return;
        }
        self.scrolling.fields.read(cx).handle().set_offset(gpui::point(px(0.), px(0.)));
        self.records.selected = Some(object);
        self.records.draft_id = None;
        self.records.object = None;
        self.records.drafts.clear();
        self.records.values = None;
        self.records.changed_during_edit = false;
        self.records.field_offset = 0;
        if let Some(type_id) = self.records.type_id
            && let Some(id) = self.find_local_draft(
                self.selected,
                |target| {
                    matches!(target, crate::store::browser_drafts::BrowserDraftTarget::Record {
                    type_id: found, object: found_object
                } if *found == type_id && *found_object == object)
                },
                cx,
            )
        {
            self.restore_record_draft(id, cx);
        }
        if let Some(database) = self.database_address() {
            self.submit(Request::ReadObject { session: self.request_session(), database, object });
        }
        self.start_detail_loading(cx);
        cx.notify();
    }

    pub(super) fn receive_record(&mut self, reply: Reply, cx: &mut Context<Self>) -> bool {
        let Some(database) = self.database_address() else { return false };
        match reply {
            Reply::Objects(result)
                if result.database == database
                    && Some(result.type_id) == self.records.type_id
                    && result.after == self.records.after =>
            {
                self.records.page = Some(Arc::new(result.page));
            }
            Reply::ObjectCreated(result)
                if result.database == database
                    && Some(result.object.type_id) == self.records.type_id
                    && self.records.creating
                    && self.records.selected.is_none() =>
            {
                self.records.creating = false;
                self.records.selected = Some(result.object.object_id);
                self.records.object = Some(Arc::new(result.object));
                self.refresh_records();
            }
            Reply::Object(result)
                if result.database == database
                    && Some(result.object.type_id) == self.records.type_id
                    && self.records.selected == Some(result.object.object_id) =>
            {
                self.acknowledge_fields(&result.object, cx);
                self.acknowledge_values(&result.object);
                self.records.object = Some(Arc::new(result.object));
            }
            Reply::Object(result)
                if result.database == database
                    && self.records.selected == Some(result.object.object_id) =>
            {
                self.retire_record_selection(cx);
                if !self.retired_record_draft_pending() {
                    self.error = "The referenced record has a different type.".into();
                }
            }
            Reply::ObjectRemoved { database: got, object } if got == database => {
                if self.records.selected == Some(object) {
                    self.retire_record_selection(cx);
                }
                self.refresh_records();
            }
            Reply::Schema { database: got, ok: true } if got == database => {
                self.schema_name.update(cx, |editor, cx| editor.clear(cx));
                self.field_name.update(cx, |editor, cx| editor.clear(cx));
                self.adding_type = false;
                self.adding_field = false;
                self.refresh();
            }
            _ => return false,
        }
        true
    }

    fn acknowledge_fields(&mut self, object: &ObjectFields, cx: &mut Context<Self>) {
        for (field, draft) in &mut self.records.drafts {
            let cell = object.fields.get(field);
            let values = cell.and_then(|cell| cell.values.as_ref());
            if let Some((revision, submitted)) = &draft.submitted
                && values == submitted.as_ref()
            {
                let text = scalar_text(values);
                draft
                    .editor
                    .update(cx, |editor, cx| editor.acknowledge(Some(*revision), &text, cx));
                draft.dirty = draft.revision != *revision;
                draft.submitted = None;
                draft.base_values = values.cloned();
            } else if draft.dirty {
                self.records.changed_during_edit |= draft.base_values.as_ref() != values;
            } else {
                let text = scalar_text(values);
                draft.editor.update(cx, |editor, cx| editor.acknowledge(None, &text, cx));
                draft.base_values = values.cloned();
            }
        }
    }

    pub(super) fn field_editor(
        &mut self,
        field: Id,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Entity<text_editor::TextEditor> {
        if let Some(draft) = self.records.drafts.get(&field) {
            return draft.editor.clone();
        }
        assert!(self.records.drafts.len() < FIELD_PAGE, "record scalar editors stay bounded");
        let weak = cx.weak_entity();
        let database = self.database_address();
        let object = self.records.selected;
        let changed: text_editor::CommitCallback = Arc::new(move |text, revision, cx| {
            weak.update(cx, |this, cx| {
                if this.database_address() == database && this.records.selected == object {
                    this.field_changed(field, text, revision, cx);
                }
            })
            .is_ok()
        });
        let restored = self.local_scalar_draft(field, cx);
        let text = restored.as_ref().map_or(text, |draft| draft.text.as_str());
        let editor = cx.new(|cx| {
            text_editor::TextEditor::new(text, cx)
                .literal()
                .bound_edits(MAX_DRAFT_BYTES)
                .on_change(changed)
        });
        let base_values =
            restored.as_ref().map(|draft| draft.base_values.clone()).unwrap_or_else(|| {
                self.records
                    .object
                    .as_ref()
                    .and_then(|object| object.fields.get(&field))
                    .and_then(|cell| cell.values.clone())
            });
        self.records.drafts.insert(
            field,
            FieldDraft {
                editor: editor.clone(),
                dirty: restored.is_some(),
                revision: 0,
                submitted: None,
                base_values,
            },
        );
        editor
    }

    pub(super) fn field_changed(
        &mut self,
        field: Id,
        _: &str,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.records.drafts.get_mut(&field) {
            draft.dirty = true;
            draft.revision = revision;
        }
        self.did_change_draft(cx);
        cx.notify();
    }

    pub(super) fn save_field(
        &mut self,
        field: Id,
        values: Option<Vec<Value>>,
        cx: &mut Context<Self>,
    ) {
        if self.database_address().is_none() || self.records.selected.is_none() {
            return;
        }
        if !self.details.as_ref().is_some_and(|details| details.writable) {
            return;
        }
        if !self.local_draft_ready(cx) {
            self.local_draft_error("Local drafts have not finished loading.", cx);
            return;
        }
        let fields = BTreeMap::from([(field, values.clone())]);
        if !self.records.drafts.contains_key(&field) {
            if self.records.drafts.len() >= FIELD_PAGE {
                self.local_draft_error(
                    "Keep or discard existing field changes before editing another field.",
                    cx,
                );
                return;
            }
            let text = scalar_text(values.as_ref()).into_owned();
            self.field_editor(field, &text, cx);
        }
        if let Some(draft) = self.records.drafts.get_mut(&field) {
            draft.dirty = true;
        }
        if self.publish_record_fields(fields, cx)
            && let Some(draft) = self.records.drafts.get_mut(&field)
        {
            draft.submitted = Some((draft.revision, values));
        }
        cx.notify();
    }

    pub(super) fn records_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        if self.records.page.is_none() && self.records.object.is_none() {
            return self.loading_panel(cx);
        }
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.record_list(cx))
            .child(self.record_editor(cx))
    }

    fn record_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let unavailable = self.database_address().is_none();
        let mut list = div()
            .flex()
            .flex_col()
            .w(ui(220.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(super::super::theme::column_divider_color());
        let mut rows = div().id("records").size_full().px(ui(8.));
        let objects = self.records.page.clone();
        if let Some(page) = objects {
            let mut controls = Vec::with_capacity(page.objects.len());
            for object in &page.objects {
                let id = object.object_id;
                let label = record_label(object);
                let button = self.button(
                    &format!("record-{id}"),
                    &label,
                    unavailable,
                    move |this, cx| this.select_record(id, cx),
                    cx,
                );
                let accessory = self.find_local_draft(self.selected, |target| matches!(target,
                    crate::store::browser_drafts::BrowserDraftTarget::Record { object, .. } if *object == id
                ), cx).map(|draft| self.draft_button(draft, &format!("record-{id}"), cx));
                button.update(cx, |button, cx| {
                    button.select_navigation(self.records.selected == Some(id), cx);
                    button.set_navigation_detail(&id.to_string()[..6], cx);
                    button.set_navigation_accessory(accessory, cx);
                });
                controls.push(button);
            }
            if page.objects.is_empty() {
                rows = rows.child(self.text("no-records", "No records", cx));
            } else {
                rows = rows.child(
                    gpui::uniform_list("record-rows", controls.len(), move |range, _, _| {
                        range
                            .map(|index| div().h(ui(32.)).child(controls[index].clone()))
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(self.scrolling.list.clone())
                    .h_full(),
                );
            }
        } else {
            rows = rows.child(self.loading_label(cx));
        }
        list = list
            .child(scrolling::viewport(rows, &self.scrolling.records))
            .child(self.record_navigation(cx));
        list
    }

    fn record_navigation(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let next = self.records.page.as_ref().and_then(|page| page.next);
        let unavailable = self.database_address().is_none();
        let first = self.button(
            "records-first",
            "First",
            unavailable || self.records.after.is_none(),
            |this, cx| this.record_page(None, cx),
            cx,
        );
        let more = self.button(
            "records-next",
            "Next",
            unavailable || next.is_none(),
            move |this, cx| this.record_page(next, cx),
            cx,
        );
        let disabled = self.database_address().is_none()
            || !self.details.as_ref().is_some_and(|details| details.writable || details.appendable)
            || self.records.creating;
        let new = self.button(
            "record-new",
            "New record",
            disabled,
            |this, cx| this.create_record(BTreeMap::new(), cx),
            cx,
        );
        div()
            .flex()
            .flex_col()
            .gap(ui(8.))
            .p(ui(8.))
            .child(div().flex().gap(ui(8.)).child(first).child(more))
            .child(new)
    }

    pub(super) fn create_record(
        &mut self,
        fields: BTreeMap<Id, Option<Vec<Value>>>,
        cx: &mut Context<Self>,
    ) {
        if !self.retain_local_drafts(cx) {
            return;
        }
        let (Some(database), Some(type_id)) = (self.database_address(), self.records.type_id)
        else {
            return;
        };
        if !self.details.as_ref().is_some_and(|details| details.writable || details.appendable)
            || self.records.creating
        {
            return;
        }
        if self.submit(Request::CreateObject {
            session: self.request_session(),
            database,
            type_id,
            fields,
        }) {
            self.records.selected = None;
            self.records.draft_id = None;
            self.records.object = None;
            self.records.drafts.clear();
            self.records.values = None;
            self.records.creating = true;
        }
    }

    fn record_page(&mut self, after: Option<Id>, cx: &mut Context<Self>) {
        if self.database_address().is_none() || !self.prepare_entry_navigation(cx) {
            return;
        }
        self.scrolling.list.0.borrow().base_handle.set_offset(gpui::point(px(0.), px(0.)));
        self.records.after = after;
        self.records.page = None;
        self.records.selected = None;
        self.records.draft_id = None;
        self.records.object = None;
        self.records.drafts.clear();
        self.records.values = None;
        self.refresh_records();
        self.start_detail_loading(cx);
        cx.notify();
    }

    fn record_editor(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let content = self.record_fields(cx);
        scrolling::viewport(content, &self.scrolling.fields)
    }

    fn record_fields(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let mut panel = div()
            .id("record-fields")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scrolling.fields.read(cx).handle())
            .flex()
            .flex_col()
            .gap(ui(16.))
            .p(ui(16.));
        let Some(object) = self.records.object.clone() else {
            return panel.child(if self.records.selected.is_some() {
                self.loading_label(cx)
            } else {
                self.text("select-record", "Select a record", cx)
            });
        };
        panel = panel.child(self.record_actions(&object, cx));
        let Some(details) = self.details.clone() else { return panel };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else { return panel };
        let Some(declaration) = schema.types.get(&object.type_id) else { return panel };
        let field_offset = self.records.field_offset;
        let drafts_ready = self.local_draft_ready(cx);
        self.records.drafts.retain(|id, draft| {
            draft.dirty
                || draft.submitted.is_some()
                || draft.editor.read(cx).is_composing()
                || drafts_ready
                    && declaration
                        .fields
                        .keys()
                        .skip(field_offset)
                        .take(FIELD_PAGE)
                        .any(|visible| visible == id)
        });
        for (id, field) in
            declaration.fields.iter().skip(self.records.field_offset).take(FIELD_PAGE)
        {
            panel = panel.child(self.field_row(*id, field, &object, details.writable, cx));
        }
        if declaration.fields.len() > FIELD_PAGE {
            panel = panel.child(self.field_navigation(declaration.fields.len(), cx));
        }
        panel
    }

    fn record_actions(&mut self, object: &ObjectFields, cx: &mut Context<Self>) -> gpui::Div {
        let id = object.object_id;
        let disabled = !self.details.as_ref().is_some_and(|details| details.writable);
        let delete = self.button(
            "record-delete",
            "Delete record",
            disabled,
            move |this, _| {
                if let Some(database) = this.database_address() {
                    this.submit(Request::RemoveObject {
                        session: this.request_session(),
                        database,
                        object: id,
                    });
                }
            },
            cx,
        );
        let duplicable =
            self.details.as_ref().is_some_and(|details| details.writable || details.appendable)
                && !self.records.creating;
        let duplicate = self.button(
            "record-duplicate",
            "Duplicate",
            !duplicable,
            |this, cx| {
                if let Some(object) = &this.records.object {
                    let fields =
                        object.fields.iter().map(|(id, cell)| (*id, cell.values.clone())).collect();
                    this.create_record(fields, cx);
                }
            },
            cx,
        );
        div()
            .flex()
            .items_center()
            .gap(ui(8.))
            .child(
                tabular_numerals(self.text("record-id", format!("#{}", &id.to_string()[..8]), cx))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui(11.))
                    .text_color(rgb(0x777777)),
            )
            .child(duplicate)
            .child(delete)
    }

    fn field_navigation(&mut self, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        let offset = self.records.field_offset;
        let previous = self.button(
            "fields-previous",
            "Previous fields",
            offset == 0,
            |this, cx| {
                if !this.retain_local_drafts(cx) {
                    return;
                }
                this.records.field_offset = this.records.field_offset.saturating_sub(FIELD_PAGE);
                this.records.drafts.clear();
                this.records.values = None;
            },
            cx,
        );
        let next = self.button(
            "fields-next",
            "Next fields",
            offset + FIELD_PAGE >= count,
            |this, cx| {
                if !this.retain_local_drafts(cx) {
                    return;
                }
                this.records.field_offset += FIELD_PAGE;
                this.records.drafts.clear();
                this.records.values = None;
            },
            cx,
        );
        div().flex().gap(ui(8.)).child(previous).child(next)
    }

    fn field_row(
        &mut self,
        id: Id,
        field: &FieldDeclaration,
        object: &ObjectFields,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let drafts_ready = self.local_draft_ready(cx);
        let writable = writable && drafts_ready;
        let values = object.fields.get(&id).and_then(|cell| cell.values.as_ref());
        let mut row = div().flex().flex_col().gap(ui(6.)).child(
            self.text(
                &format!("field-label-{id}"),
                format!("{}{}", field.name, if field.deprecated { " · Deprecated" } else { "" }),
                cx,
            )
            .text_size(ui(11.))
            .text_color(rgb(0x777777)),
        );
        let text = scalar_text(values);
        if field.repeated
            || matches!(
                field.value_type,
                ValueType::File | ValueType::Reference(_) | ValueType::Bytes
            )
        {
            return row.child(self.value_field(id, field, values, writable, cx));
        }
        let scalar = !field.repeated
            && matches!(field.value_type, ValueType::Text | ValueType::Integer | ValueType::Float);
        let retained =
            self.records.drafts.contains_key(&id) || self.local_scalar_draft(id, cx).is_some();
        let editor_available = self.records.drafts.contains_key(&id)
            || drafts_ready && self.records.drafts.len() < FIELD_PAGE;
        if scalar
            && editor_available
            && (retained || text.len() <= MAX_DRAFT_BYTES && !text.contains(['\n', '\r']))
        {
            let editor = self.field_editor(id, &text, cx);
            editor.update(cx, |editor, cx| {
                editor.set_placeholder(&field.name);
                editor.set_theme(&theme());
                editor.set_read_only(!writable, &theme(), cx);
            });
            row = row.child(editor);
            row = row.child(self.field_save_controls(id, field.value_type, writable, cx));
        } else if !field.repeated && field.value_type == ValueType::Boolean && editor_available {
            let checked =
                matches!(values.and_then(|values| values.first()), Some(Value::Boolean(true)));
            row = row.child(self.control(
                format!("field-bool-{id}"),
                &field.name,
                controls::Kind::Checkbox(if checked { CheckState::On } else { CheckState::Off }),
                !writable || self.record_publication_pending(cx),
                move |this, cx| this.save_field(id, Some(vec![Value::Boolean(!checked)]), cx),
                cx,
            ));
        } else {
            row = row.child(self.text(&format!("field-preview-{id}"), value_preview(values), cx));
        }
        row
    }

    fn field_save_controls(
        &mut self,
        id: Id,
        kind: ValueType,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let dirty = self
            .records
            .drafts
            .get(&id)
            .is_some_and(|draft| draft.dirty && draft.submitted.is_none());
        let save = self.button(
            &format!("field-save-{id}"),
            "Save",
            !writable || !dirty || self.record_publication_pending(cx),
            move |this, cx| {
                let Some(draft) = this.records.drafts.get(&id) else { return };
                match parse_scalar(kind, draft.editor.read(cx).text()) {
                    Ok(value) => this.save_field(id, Some(vec![value]), cx),
                    Err(error) => this.error = error.into(),
                }
            },
            cx,
        );
        let clear = self.button(
            &format!("field-clear-{id}"),
            "Clear",
            !writable || self.record_publication_pending(cx),
            move |this, cx| this.save_field(id, None, cx),
            cx,
        );
        let discard = self.button(
            &format!("field-discard-{id}"),
            "Discard draft",
            !dirty || self.record_publication_pending(cx),
            move |this, cx| {
                if this.record_publication_pending(cx) {
                    return;
                }
                this.records.drafts.remove(&id);
                this.discard_scalar_draft(id, cx);
            },
            cx,
        );
        div().flex().gap(ui(8.)).child(save).child(clear).child(discard)
    }
}

fn scalar_text(values: Option<&Vec<Value>>) -> std::borrow::Cow<'_, str> {
    match values.and_then(|values| values.first()) {
        Some(Value::Text(value)) => std::borrow::Cow::Borrowed(value),
        Some(Value::Integer(value)) => std::borrow::Cow::Owned(value.to_string()),
        Some(Value::Float(value)) => std::borrow::Cow::Owned(value.to_string()),
        Some(Value::Boolean(value)) => {
            std::borrow::Cow::Borrowed(if *value { "true" } else { "false" })
        }
        _ => std::borrow::Cow::Borrowed(""),
    }
}

pub(super) fn parse_scalar(kind: ValueType, text: &str) -> Result<Value, &'static str> {
    match kind {
        ValueType::Text => Ok(Value::Text(text.into())),
        ValueType::Integer => text.parse().map(Value::Integer).map_err(|_| "Enter a whole number."),
        ValueType::Float => text
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::Float)
            .ok_or("Enter a finite number."),
        _ => Err("This field requires a different editor."),
    }
}

pub(super) fn record_label(object: &ObjectFields) -> String {
    let text = object
        .fields
        .values()
        .filter_map(|cell| cell.values.as_ref())
        .flat_map(|values| values.iter())
        .find_map(|value| match value {
            Value::Text(value) if !value.is_empty() => Some(value),
            _ => None,
        });
    text.map(|text| text.chars().take(60).collect()).unwrap_or_else(|| object.object_id.to_string())
}

fn value_preview(values: Option<&Vec<Value>>) -> String {
    let Some(values) = values else { return "Empty".into() };
    if values.len() != 1 {
        return format!("{} values", values.len());
    }
    match &values[0] {
        Value::Text(text) => {
            let mut chars = text.chars();
            let mut preview: String = chars.by_ref().take(256).collect();
            if chars.next().is_some() {
                preview.push('…');
            }
            preview
        }
        Value::Bytes(bytes) => format!("{} bytes", bytes.len()),
        Value::File { volume_id, entry_id } => format!("File {entry_id} · Volume {volume_id}"),
        Value::Reference { volume_id, database_id, object_id } => {
            format!("Record {object_id} · Database {database_id} · Volume {volume_id}")
        }
        Value::Boolean(value) => value.to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
    }
}
