//! Record recovery merges visible editors with the profile's offscreen fields.
use super::*;
use crate::desktop::{draft_publications, drafts as repository};
use crate::store::browser_drafts::{
    BrowserDraftBody, BrowserDraftTarget, RecordDraft, RecordFieldDraft, RecordValuesDraft,
};

#[derive(Clone)]
struct DraftTarget {
    account: Id,
    volume: Id,
    entry: EntryId,
    ancestors: Vec<EntryId>,
}

/// The existing bounded editor set is moved here only when local admission fails.
/// Canonical pages and records are dropped before this hold becomes hidden.
pub(in crate::desktop::browser) struct Retired {
    target: DraftTarget,
    records: Records,
}

impl Browser {
    fn local_record_draft(&self, cx: &App) -> Option<RecordDraft> {
        let id = self.records.draft_id.or_else(|| {
            self.find_local_draft(self.selected, |target| matches!(target,
                BrowserDraftTarget::Record { type_id, object }
                if Some(*type_id) == self.records.type_id && Some(*object) == self.records.selected
            ), cx)
        })?;
        let draft = self.local_draft_snapshot(id, cx)?;
        let BrowserDraftBody::Record(state) = &draft.body else { return None };
        if draft.volume != self.volume.id
            || draft.entry != self.selected
            || Some(state.type_id) != self.records.type_id
            || Some(state.object) != self.records.selected
        {
            return None;
        }
        Some(state.clone())
    }

    pub(in crate::desktop::browser) fn local_scalar_draft(
        &self,
        field: Id,
        cx: &App,
    ) -> Option<RecordFieldDraft> {
        self.local_record_draft(cx)?.fields.into_iter().find(|draft| draft.field == field)
    }

    pub(in crate::desktop::browser) fn local_values_draft(
        &self,
        field: Id,
        cx: &App,
    ) -> Option<RecordValuesDraft> {
        self.local_record_draft(cx)?.values.into_iter().find(|draft| draft.field == field)
    }

    pub(in crate::desktop::browser) fn restore_visible_values_draft(
        &mut self,
        field: Id,
        cx: &mut Context<Self>,
    ) {
        if self.records.values.is_some() {
            return;
        }
        if let Some(state) = self.local_record_draft(cx)
            && state.active_values == Some(field)
            && let Some(values) = state.values.iter().find(|values| values.field == field)
        {
            self.restore_values_state(values, cx);
        }
    }

    pub(in crate::desktop::browser) fn retired_record_draft_pending(&self) -> bool {
        self.retired_records.is_some() || self.records.recovery_pending
    }

    pub(in crate::desktop::browser) fn retired_record_draft_busy(&self, cx: &App) -> bool {
        self.retired_records.as_ref().is_some_and(|held| {
            held.target.account != self.local_draft_account()
                || held.records.draft_id.is_some_and(|id| draft_publications::pending(id, cx))
        }) || self.records.recovery_pending
            && self.records.draft_id.is_some_and(|id| draft_publications::pending(id, cx))
    }

    fn record_draft_target(&self) -> Option<DraftTarget> {
        let entry = self.selected?;
        Some(DraftTarget {
            account: self.local_draft_account(),
            volume: self.volume.id,
            entry,
            ancestors: self
                .catalog
                .columns
                .iter()
                .filter_map(|column| column.parent)
                .take_while(|id| *id != entry)
                .collect(),
        })
    }

    fn record_draft_id(
        &self,
        target: &DraftTarget,
        type_id: Id,
        object: Id,
        cx: &App,
    ) -> Option<Id> {
        self.records.draft_id.filter(|id| repository::get_for(target.account, *id, cx)
            .is_some_and(|draft| draft.volume == target.volume && draft.entry == Some(target.entry)
                && matches!(&draft.body, BrowserDraftBody::Record(state) if state.type_id == type_id && state.object == object)))
            .or_else(|| repository::snapshots_for(target.account, cx).into_iter().find(|draft| {
                draft.volume == target.volume && draft.entry == Some(target.entry)
                    && matches!(draft.target, BrowserDraftTarget::Record { type_id: found_type, object: found_object }
                        if found_type == type_id && found_object == object)
            }).map(|draft| draft.id))
    }

    /// Local recovery uses the captured private target, never current Read authority.
    fn capture_record_at(
        &mut self,
        target: &DraftTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (Some(type_id), Some(object)) = (self.records.type_id, self.records.selected) else {
            return Ok(());
        };
        let existing = self.record_draft_id(target, type_id, object, cx);
        let mut state = existing
            .and_then(|id| repository::get_for(target.account, id, cx))
            .and_then(|draft| match &draft.body {
                BrowserDraftBody::Record(state) => Some(state.clone()),
                _ => None,
            })
            .unwrap_or(RecordDraft {
                type_id,
                object,
                fields: vec![],
                values: vec![],
                active_values: None,
                changed_during_edit: self.records.changed_during_edit,
            });
        for (field, editor) in &self.records.drafts {
            if editor.dirty || editor.submitted.is_some() || editor.editor.read(cx).is_composing() {
                state.fields.retain(|draft| draft.field != *field);
                state.fields.push(RecordFieldDraft {
                    field: *field,
                    text: editor.editor.read(cx).text().to_owned(),
                    base_values: editor.base_values.clone(),
                });
            }
        }
        if let Some(values) = self.capture_values_state(cx)
            && (values.dirty || values.input_dirty)
        {
            state.values.retain(|draft| draft.field != values.field);
            state.active_values = Some(values.field);
            state.values.push(values);
        }
        state.fields.sort_unstable_by_key(|draft| draft.field);
        state.values.sort_unstable_by_key(|draft| draft.field);
        state.changed_during_edit = self.records.changed_during_edit;
        if state.fields.is_empty() && state.values.is_empty() {
            if repository::account(cx) == Some(target.account)
                && let Some(id) = existing
            {
                repository::remove(id, cx);
            }
            self.records.draft_id = None;
            self.records.recovery_pending = false;
            return Ok(());
        }
        self.records.recovery_pending = true;
        if !repository::ready_for(target.account, cx) {
            return Err(
                "Local changes for this account are not ready. Your record editor is retained."
                    .into(),
            );
        }
        let id = match existing {
            Some(id) => id,
            None => {
                let mut bytes = [0; 16];
                getrandom::fill(&mut bytes).map_err(|error| {
                    format!("Couldn’t create a local record draft identity: {error}")
                })?;
                Id::from_bytes(bytes)
            }
        };
        self.records.draft_id = Some(repository::upsert(
            crate::store::browser_drafts::BrowserDraft {
                id,
                revision: 1,
                volume: target.volume,
                entry: Some(target.entry),
                ancestors: target.ancestors.clone(),
                body: BrowserDraftBody::Record(state),
            },
            cx,
        )?);
        self.records.recovery_pending = false;
        Ok(())
    }

    fn persist_retired_record_draft(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if self
            .retired_records
            .as_ref()
            .is_some_and(|held| held.target.account != self.local_draft_account())
        {
            return Err("Return to the record’s account before keeping its local changes.".into());
        }
        let Some(held) = self.retired_records.take() else {
            return Ok(());
        };
        let Retired { target, records } = held;
        let visible = std::mem::replace(&mut self.records, records);
        let result = self.capture_record_at(&target, cx);
        let records = std::mem::replace(&mut self.records, visible);
        if result.is_err() {
            self.retired_records = Some(Retired { target, records });
        }
        result
    }

    pub(in crate::desktop::browser) fn retry_retired_record_draft(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Err(error) = self.capture_record_draft(cx) {
            self.local_draft_error(&error, cx);
            return false;
        }
        cx.notify();
        true
    }

    pub(in crate::desktop::browser) fn discard_retired_record_draft(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.retired_record_draft_busy(cx) {
            return;
        }
        if let Some(held) = self.retired_records.take()
            && repository::account(cx) == Some(held.target.account)
            && let (Some(type_id), Some(object)) = (held.records.type_id, held.records.selected)
        {
            let visible = std::mem::replace(&mut self.records, held.records);
            let id = self.record_draft_id(&held.target, type_id, object, cx);
            self.records = visible;
            if let Some(id) = id {
                repository::remove(id, cx);
            }
        }
        if self.records.recovery_pending {
            if let Some(target) = self.record_draft_target()
                && let (Some(type_id), Some(object)) = (self.records.type_id, self.records.selected)
                && let Some(id) = self.record_draft_id(&target, type_id, object, cx)
            {
                repository::remove(id, cx);
            }
            self.records.draft_id = None;
            self.records.drafts.clear();
            self.records.values = None;
            self.records.changed_during_edit = false;
            self.records.recovery_pending = false;
        }
        cx.notify();
    }

    pub(in crate::desktop::browser) fn retire_record_selection(&mut self, cx: &mut Context<Self>) {
        if self.retired_records.is_some() {
            self.records.page = None;
            self.records.object = None;
            return;
        }
        let Some(target) = self.record_draft_target() else {
            self.records = Records::default();
            return;
        };
        if let Err(error) = self.capture_record_at(&target, cx) {
            self.records.page = None;
            self.records.object = None;
            self.records.creating = false;
            self.records.drafts.retain(|_, draft| {
                draft.dirty |= draft.editor.read(cx).is_composing();
                draft.dirty || draft.submitted.is_some()
            });
            self.retire_record_values(cx);
            if !self.records.drafts.is_empty() || self.records.values.is_some() {
                self.retired_records =
                    Some(Retired { target, records: std::mem::take(&mut self.records) });
            } else {
                self.records = Records::default();
            }
            self.local_draft_error(&error, cx);
        } else {
            self.records = Records::default();
        }
    }

    pub(in crate::desktop::browser) fn restore_retired_record_selection(
        &mut self,
        _cx: &mut Context<Self>,
    ) {
        let Some(held) = self.retired_records.as_ref() else {
            return;
        };
        let Some(database) = self.database_address() else {
            return;
        };
        if held.target.account != self.local_draft_account()
            || held.target.volume != database.volume
            || held.target.entry != database.entry
        {
            return;
        }
        let Some(type_id) = held.records.type_id else {
            return;
        };
        let Some(details) = &self.details else {
            return;
        };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else {
            return;
        };
        let Some(declaration) = schema.types.get(&type_id) else { return };
        let first_field = held
            .records
            .values
            .as_ref()
            .map(|editor| editor.field)
            .or_else(|| held.records.drafts.keys().next().copied());
        let offset = first_field
            .and_then(|field| declaration.fields.keys().position(|id| *id == field))
            .map(|offset| offset / FIELD_PAGE * FIELD_PAGE);
        self.records = self.retired_records.take().expect("readable held record exists").records;
        if let Some(offset) = offset {
            self.records.field_offset = offset;
        }
        self.overview = false;
    }

    pub(in crate::desktop::browser) fn capture_record_draft(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.persist_retired_record_draft(cx)?;
        if let Some(target) = self.record_draft_target() {
            self.capture_record_at(&target, cx)?;
        }
        Ok(())
    }

    pub(in crate::desktop::browser) fn restore_record_draft(
        &mut self,
        id: Id,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(address) = self.database_address() else { return false };
        let Some(draft) = self.local_draft_snapshot(id, cx) else { return false };
        let BrowserDraftBody::Record(state) = &draft.body else { return false };
        if draft.volume != address.volume || draft.entry != Some(address.entry) {
            return false;
        }
        let Some(details) = &self.details else { return false };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else { return false };
        if !schema.types.contains_key(&state.type_id) {
            return false;
        }
        let changed = self.records.type_id != Some(state.type_id)
            || self.records.selected != Some(state.object);
        if changed {
            self.records = Records {
                type_id: Some(state.type_id),
                selected: Some(state.object),
                ..Default::default()
            };
        }
        self.records.draft_id = Some(id);
        self.records.drafts.retain(|_, editor| {
            editor.dirty || editor.submitted.is_some() || editor.editor.read(cx).is_composing()
        });
        let first_field =
            state.active_values.or_else(|| state.fields.first().map(|field| field.field));
        if let Some(offset) = first_field.and_then(|field| {
            schema.types[&state.type_id].fields.keys().position(|id| *id == field)
        }) {
            let offset = offset / FIELD_PAGE * FIELD_PAGE;
            self.records.field_offset = offset;
        }
        self.records.changed_during_edit = state.changed_during_edit;
        let raw_values = self.records.values.as_ref().is_some_and(|editor| {
            editor.dirty
                || editor.input_dirty
                || editor.submitted.is_some()
                || editor.input.read(cx).is_composing()
        });
        if !raw_values {
            self.records.values = None;
            if let Some(field) = state.active_values
                && let Some(values) = state.values.iter().find(|values| values.field == field)
            {
                self.restore_values_state(values, cx);
            }
        }
        self.overview = false;
        if changed {
            self.refresh_records();
            self.start_detail_loading(cx);
        }
        true
    }

    /// Repository acknowledgment changes flags/baselines without replacing live editors.
    pub(in crate::desktop::browser) fn sync_record_local_draft(&mut self, cx: &mut Context<Self>) {
        if self.retired_records.is_some()
            || self.records.recovery_pending
            || self.draft_capture_scheduled
            || self.records.draft_id.is_none()
        {
            return;
        }
        let state = self.local_record_draft(cx);
        for (field, editor) in &mut self.records.drafts {
            if let Some(saved) = state
                .as_ref()
                .and_then(|state| state.fields.iter().find(|draft| draft.field == *field))
            {
                editor.base_values = saved.base_values.clone();
            } else {
                editor.dirty = false;
                editor.submitted = None;
                editor.base_values = self
                    .records
                    .object
                    .as_ref()
                    .and_then(|object| object.fields.get(field))
                    .and_then(|cell| cell.values.clone());
                let text = scalar_text(editor.base_values.as_ref());
                editor
                    .editor
                    .update(cx, |input, cx| input.acknowledge(Some(editor.revision), &text, cx));
            }
        }
        let values = self.records.values.as_ref().and_then(|editor| {
            state
                .as_ref()
                .and_then(|state| state.values.iter().find(|draft| draft.field == editor.field))
        });
        self.sync_values_state(values);
        if state.is_none() {
            self.records.draft_id = None;
        }
    }

    pub(in crate::desktop::browser) fn record_publication_pending(&self, cx: &App) -> bool {
        !draft_publications::available(cx)
            || self.records.draft_id.is_some_and(|id| draft_publications::pending(id, cx))
    }

    pub(in crate::desktop::browser) fn publish_record_fields(
        &mut self,
        fields: BTreeMap<Id, Option<Vec<Value>>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(database), Some(object)) = (self.database_address(), self.records.selected)
        else {
            return false;
        };
        if !self.details.as_ref().is_some_and(|details| details.writable)
            || self.record_publication_pending(cx)
        {
            return false;
        }
        if let Err(error) = self.capture_record_draft(cx) {
            self.local_draft_error(&error, cx);
            return false;
        }
        let Some(id) = self.records.draft_id else { return false };
        let Some(snapshot) = self.local_draft_snapshot(id, cx) else { return false };
        match self.send.enqueue(Request::EditObject {
            session: self.request_session(),
            database,
            object,
            fields: fields.clone(),
        }) {
            Ok(admission) => {
                draft_publications::track_record(admission, snapshot, fields, cx);
                true
            }
            Err(error) => {
                self.error = error.into();
                false
            }
        }
    }

    pub(in crate::desktop::browser) fn discard_scalar_draft(
        &mut self,
        field: Id,
        cx: &mut Context<Self>,
    ) {
        if self.record_publication_pending(cx) {
            return;
        }
        let Some(mut state) = self.local_record_draft(cx) else { return };
        state.fields.retain(|draft| draft.field != field);
        self.replace_record_draft(state, cx);
    }

    pub(in crate::desktop::browser) fn discard_values_draft(
        &mut self,
        field: Id,
        cx: &mut Context<Self>,
    ) {
        if self.record_publication_pending(cx) {
            return;
        }
        let Some(mut state) = self.local_record_draft(cx) else { return };
        state.values.retain(|draft| draft.field != field);
        if state.active_values == Some(field) {
            state.active_values = None;
        }
        self.replace_record_draft(state, cx);
    }

    fn replace_record_draft(&mut self, state: RecordDraft, cx: &mut Context<Self>) {
        if state.fields.is_empty() && state.values.is_empty() {
            if let Some(id) = self
                .records
                .draft_id
                .take()
                .filter(|id| self.local_draft_snapshot(*id, cx).is_some())
            {
                repository::remove(id, cx);
            }
        } else if let Err(error) = self.store_local_draft(
            BrowserDraftBody::Record(state),
            self.selected,
            self.records.draft_id,
            cx,
        ) {
            self.error = error;
        }
    }
}

/// Settle only fields published by this receipt, preserving unrelated or newer edits.
pub(in crate::desktop) fn settle_record_publication(
    current: &mut RecordDraft,
    submitted: &RecordDraft,
    fields: &BTreeMap<Id, Option<Vec<Value>>>,
) -> bool {
    if current.type_id != submitted.type_id || current.object != submitted.object {
        return false;
    }
    current.fields.retain_mut(|draft| {
        let Some(values) = fields.get(&draft.field) else { return true };
        let unchanged = submitted
            .fields
            .iter()
            .find(|old| old.field == draft.field)
            .is_some_and(|old| old.text == draft.text);
        draft.base_values.clone_from(values);
        !unchanged
    });
    current.values.retain_mut(|draft| {
        let Some(values) = fields.get(&draft.field) else { return true };
        let unchanged =
            submitted.values.iter().find(|old| old.field == draft.field).is_some_and(|old| {
                old.values == draft.values
                    && old.input == draft.input
                    && old.input_dirty == draft.input_dirty
            });
        draft.base_values.clone_from(values);
        draft.dirty = draft.values != *values;
        !unchanged || draft.input_dirty || draft.dirty
    });
    if current
        .active_values
        .is_some_and(|field| !current.values.iter().any(|draft| draft.field == field))
    {
        current.active_values = None;
    }
    let clean = current.fields.is_empty() && current.values.is_empty();
    if clean {
        current.changed_during_edit = false;
    }
    clean
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Tests assert valid bounded record recovery fixtures.")]
    use super::*;
    use crate::desktop::browser::tests::{object_details, record, volume};
    use crate::desktop::worker_queue;

    fn state() -> RecordDraft {
        RecordDraft {
            type_id: Id::from_bytes([2; 16]),
            object: Id::from_bytes([4; 16]),
            fields: [3, 4]
                .map(|byte| RecordFieldDraft {
                    field: Id::from_bytes([byte; 16]),
                    text: format!("draft {byte}"),
                    base_values: Some(vec![Value::Text("base".into())]),
                })
                .to_vec(),
            values: [5, 6]
                .map(|byte| RecordValuesDraft {
                    field: Id::from_bytes([byte; 16]),
                    kind: ValueType::Bytes,
                    repeated: true,
                    base_values: None,
                    values: Some(vec![Value::Bytes(vec![byte])]),
                    input: String::new(),
                    editing: Some(0),
                    after: 0,
                    input_dirty: false,
                    dirty: true,
                })
                .to_vec(),
            active_values: Some(Id::from_bytes([5; 16])),
            changed_during_edit: false,
        }
    }

    #[test]
    fn partial_receipts_preserve_unpublished_fields_and_newer_scalar_or_raw_value_edits() {
        let submitted = state();
        let mut current = submitted.clone();
        current.fields[0].text = "newer scalar".into();
        current.values[0].input = "0".into();
        current.values[0].input_dirty = true;
        let fields = BTreeMap::from([
            (Id::from_bytes([3; 16]), Some(vec![Value::Text("acknowledged".into())])),
            (Id::from_bytes([5; 16]), Some(vec![Value::Bytes(vec![5])])),
        ]);
        assert!(!settle_record_publication(&mut current, &submitted, &fields));
        assert_eq!(current.fields[0].text, "newer scalar");
        assert_eq!(current.fields[0].base_values, fields[&Id::from_bytes([3; 16])]);
        assert_eq!(current.fields[1], submitted.fields[1]);
        assert_eq!(current.values[0].input, "0");
        assert!(current.values[0].input_dirty);
        assert_eq!(current.values[1], submitted.values[1]);

        let mut unchanged = submitted.clone();
        assert!(!settle_record_publication(&mut unchanged, &submitted, &fields));
        assert_eq!(unchanged.fields, vec![submitted.fields[1].clone()]);
        assert_eq!(unchanged.values, vec![submitted.values[1].clone()]);
        let last = unchanged.clone();
        let remaining = BTreeMap::from([
            (last.fields[0].field, Some(vec![Value::Text("last".into())])),
            (last.values[0].field, last.values[0].values.clone()),
        ]);
        assert!(settle_record_publication(&mut unchanged, &last, &remaining));
    }

    #[gpui::test]
    fn raw_scalar_and_multiple_repeated_drafts_survive_record_navigation_and_reopening(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, _requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send.clone(), cx));
        let (id, metadata) = browser.update(cx, |browser, cx| {
            let mut details = object_details();
            let EntryKind::ObjectDb { schema } = &mut details.entry.info.kind else { panic!() };
            let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
            for byte in [5, 6] {
                let mut field = declaration.fields[&Id::from_bytes([3; 16])].clone();
                field.value_type = ValueType::Bytes;
                field.repeated = true;
                declaration.fields.insert(Id::from_bytes([byte; 16]), field);
            }
            browser.selected = Some(details.entry.id);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details }, cx);
            browser.select_record(Id::from_bytes([4; 16]), cx);
            let object = record(4, "base", 1);
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database: browser.database_address().unwrap(),
                    object,
                }),
                cx,
            );
            let field = Id::from_bytes([3; 16]);
            let editor = browser.field_editor(field, "base", cx);
            editor.update(cx, |editor, cx| editor.acknowledge(None, "unfinished scalar", cx));
            browser.field_changed(field, "unfinished scalar", 1, cx);
            let mut repeated = state().values.remove(0);
            repeated.input = "0".into();
            repeated.input_dirty = true;
            browser.restore_values_state(&repeated, cx);
            browser.begin_values(Id::from_bytes([6; 16]), cx);
            repeated.field = Id::from_bytes([6; 16]);
            repeated.input = "f".into();
            browser.restore_values_state(&repeated, cx);
            browser.capture_record_draft(cx).unwrap();
            let id = browser.records.draft_id.unwrap();
            browser.select_record(Id::from_bytes([9; 16]), cx);
            let stored = repository::get(id, cx).unwrap();
            let BrowserDraftBody::Record(saved) = &stored.body else { panic!() };
            assert_eq!(saved.fields[0].text, "unfinished scalar");
            assert_eq!(saved.values.len(), 2);
            browser.record_page(Some(Id::from_bytes([9; 16])), cx);
            browser.select_type(Id::from_bytes([2; 16]), cx);
            assert!(browser.records.selected.is_none());
            assert!(browser.records.drafts.is_empty());
            (id, (**browser.details.as_ref().unwrap()).clone())
        });
        let reopened = cx.new(|cx| Browser::new(volume(1), send, cx));
        reopened.update(cx, |browser, cx| {
            browser.selected = Some(object_details().entry.id);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                cx,
            );
            assert!(browser.restore_record_draft(id, cx));
            assert_eq!(browser.records.selected, Some(Id::from_bytes([4; 16])));
            let scalar = browser.field_editor(Id::from_bytes([3; 16]), "base", cx);
            assert_eq!(scalar.read(cx).text(), "unfinished scalar");
            assert_eq!(browser.capture_values_state(cx).unwrap().input, "f");
            browser.begin_values(Id::from_bytes([5; 16]), cx);
            let repeated = browser.capture_values_state(cx).unwrap();
            assert_eq!(repeated.input, "0");
            assert_eq!(repeated.editing, Some(0));
            let mut readonly = metadata.clone();
            readonly.writable = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: readonly }, cx);
            assert!(browser.restore_record_draft(id, cx));
            assert_eq!(
                browser.field_editor(Id::from_bytes([3; 16]), "base", cx).read(cx).text(),
                "unfinished scalar"
            );
            browser.save_values(cx);
            assert!(!browser.record_publication_pending(cx));
            browser.signed_in_account = Some(Id::from_bytes([9; 16]));
            assert!(
                !browser.restore_record_draft(id, cx),
                "another account cannot restore this recovery record"
            );
            browser.signed_in_account = None;
            let mut denied = metadata;
            denied.readable = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
            assert!(!browser.restore_record_draft(id, cx));
            assert!(browser.records.drafts.is_empty());
            assert!(browser.records.values.is_none());
            assert!(repository::get(id, cx).is_some(), "local recovery is not authority");
        });
    }

    #[gpui::test]
    fn newly_loaded_record_drafts_survive_preview_and_clean_canonical_editors(
        cx: &mut gpui::TestAppContext,
    ) {
        for initially_ready in [false, true] {
            cx.update(|cx| {
                repository::configure_memory(Id::from_bytes([99; 16]), cx);
                if initially_ready {
                    repository::configure_memory(volume(1).owner, cx);
                }
            });
            let (send, _requests, _) = worker_queue::channel();
            let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
            let id = Id::from_bytes([80; 16]);
            let field = Id::from_bytes([3; 16]);
            let repeated = Id::from_bytes([5; 16]);
            let pencil = browser.update(cx, |browser, cx| {
                let mut metadata = object_details();
                let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else {
                    panic!()
                };
                let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
                let mut declaration_field = declaration.fields[&field].clone();
                declaration_field.value_type = ValueType::Bytes;
                declaration_field.repeated = true;
                declaration.fields.insert(repeated, declaration_field);
                browser.selected = Some(metadata.entry.id);
                browser
                    .receive(Reply::EntryDetails { volume: volume(1).id, details: metadata }, cx);
                browser.select_record(Id::from_bytes([4; 16]), cx);
                let database = browser.database_address().unwrap();
                browser.receive(
                    Reply::Object(crate::engine::reply::ObjectResult {
                        database,
                        object: record(4, "canonical", 1),
                    }),
                    cx,
                );
                browser.record_fields(cx);
                browser.begin_values(repeated, cx);
                assert_eq!(browser.local_draft_ready(cx), initially_ready);
                assert_eq!(browser.records.drafts.len(), usize::from(initially_ready));
                if !initially_ready {
                    assert!(browser.rendered_text.contains(&format!("field-preview-{field}")));
                    browser.save_field(field, Some(vec![Value::Text("stale action".into())]), cx);
                    assert!(browser.records.drafts.is_empty());
                }
                repository::configure_memory(volume(1).owner, cx);
                let mut raw = state();
                raw.fields.truncate(1);
                raw.fields[0].text = "loaded unfinished scalar".into();
                raw.values.truncate(1);
                raw.values[0].input = "0".into();
                raw.values[0].input_dirty = true;
                repository::upsert(
                    crate::store::browser_drafts::BrowserDraft {
                        id,
                        revision: 1,
                        volume: volume(1).id,
                        entry: Some(database.entry),
                        ancestors: vec![],
                        body: BrowserDraftBody::Record(raw.clone()),
                    },
                    cx,
                )
                .unwrap();
                browser.capture_record_draft(cx).unwrap();
                assert_eq!(repository::get(id, cx).unwrap().body, BrowserDraftBody::Record(raw));
                browser.draft_button(id, "record-readiness", cx).read(cx).popup_for_test().2
            });
            cx.update(|cx| pencil(0, cx));
            browser.update(cx, |browser, cx| {
                let input = browser.field_editor(field, "canonical", cx);
                assert_eq!(input.read(cx).text(), "loaded unfinished scalar");
                let values_input = browser.records.values.as_ref().unwrap().input.entity_id();
                assert_eq!(browser.capture_values_state(cx).unwrap().input, "0");
                assert!(browser.restore_record_draft(id, cx));
                assert_eq!(browser.records.drafts[&field].editor.entity_id(), input.entity_id());
                assert_eq!(
                    browser.records.values.as_ref().unwrap().input.entity_id(),
                    values_input
                );
                browser.select_record(Id::from_bytes([9; 16]), cx);
                browser.select_record(Id::from_bytes([4; 16]), cx);
                assert_eq!(
                    browser.field_editor(field, "canonical", cx).read(cx).text(),
                    "loaded unfinished scalar"
                );
                assert_eq!(browser.capture_values_state(cx).unwrap().input, "0");
                let cached = repository::get(id, cx).unwrap();
                let BrowserDraftBody::Record(raw) = &cached.body else { panic!() };
                assert_eq!(raw.fields[0].text, "loaded unfinished scalar");
                assert_eq!(raw.values[0].input, "0");
            });
        }
    }

    #[gpui::test]
    fn explicit_value_input_resets_survive_navigation_without_discarding_other_value_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        let field = Id::from_bytes([5; 16]);
        let other_field = Id::from_bytes([6; 16]);
        for change in [false, true] {
            for dirty_values in [false, true] {
                cx.update(|cx| {
                    repository::configure_memory(Id::from_bytes([99; 16]), cx);
                    repository::configure_memory(volume(1).owner, cx);
                });
                let (send, _requests, _) = worker_queue::channel();
                let browser = cx.new(|cx| Browser::new(volume(1), send.clone(), cx));
                let (reset, id, metadata, canonical) = browser.update(cx, |browser, cx| {
                    let mut metadata = object_details();
                    let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else {
                        panic!()
                    };
                    let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
                    let mut repeated = declaration.fields[&Id::from_bytes([3; 16])].clone();
                    repeated.value_type = ValueType::Bytes;
                    repeated.repeated = true;
                    declaration.fields.insert(field, repeated.clone());
                    declaration.fields.insert(other_field, repeated);
                    browser.selected = Some(metadata.entry.id);
                    browser.receive(
                        Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                        cx,
                    );
                    browser.select_record(Id::from_bytes([4; 16]), cx);
                    let mut canonical = record(4, "base", 1);
                    let mut cell = canonical.fields[&Id::from_bytes([3; 16])].clone();
                    cell.values = Some(vec![Value::Bytes(vec![1])]);
                    canonical.fields.insert(field, cell);
                    browser.receive(
                        Reply::Object(crate::engine::reply::ObjectResult {
                            database: browser.database_address().unwrap(),
                            object: canonical.clone(),
                        }),
                        cx,
                    );
                    let mut drafts = state().values;
                    browser.restore_values_state(&drafts.remove(1), cx);
                    browser.capture_record_draft(cx).unwrap();
                    let mut input = drafts.remove(0);
                    input.base_values = Some(vec![Value::Bytes(vec![1])]);
                    input.values = Some(vec![Value::Bytes(vec![if dirty_values { 2 } else { 1 }])]);
                    input.dirty = dirty_values;
                    input.input = "0".into();
                    input.input_dirty = true;
                    browser.restore_values_state(&input, cx);
                    browser.capture_record_draft(cx).unwrap();
                    let id = browser.records.draft_id.unwrap();
                    browser.record_fields(cx);
                    let control = if change {
                        format!("value-change-{field}-0")
                    } else {
                        "value-input-discard".into()
                    };
                    (
                        browser.controls[&control].read(cx).popup_for_test().2,
                        id,
                        metadata,
                        canonical,
                    )
                });
                cx.update(|cx| reset(0, cx));
                let expected_input = if dirty_values && change { "02" } else { "" };
                browser.update(cx, |browser, cx| {
                    browser.capture_record_draft(cx).unwrap();
                    let cached = repository::get(id, cx).unwrap();
                    let BrowserDraftBody::Record(saved) = &cached.body else { panic!() };
                    let input = saved.values.iter().find(|values| values.field == field);
                    if dirty_values {
                        let input = input.unwrap();
                        assert_eq!(input.values, Some(vec![Value::Bytes(vec![2])]));
                        assert_eq!(input.input, expected_input);
                        assert!(!input.input_dirty);
                    } else {
                        assert!(input.is_none(), "the explicit reset removes an input-only draft");
                    }
                    let other =
                        saved.values.iter().find(|values| values.field == other_field).unwrap();
                    assert_eq!(other.values, Some(vec![Value::Bytes(vec![6])]));
                    browser.select_record(Id::from_bytes([9; 16]), cx);
                    browser.select_record(Id::from_bytes([4; 16]), cx);
                    browser.receive(
                        Reply::Object(crate::engine::reply::ObjectResult {
                            database: browser.database_address().unwrap(),
                            object: canonical.clone(),
                        }),
                        cx,
                    );
                    browser.begin_values(field, cx);
                    assert_eq!(browser.capture_values_state(cx).unwrap().input, expected_input);
                });
                let reopened = cx.new(|cx| Browser::new(volume(1), send, cx));
                reopened.update(cx, |browser, cx| {
                    browser.selected = Some(metadata.entry.id);
                    browser.receive(
                        Reply::EntryDetails { volume: volume(1).id, details: metadata },
                        cx,
                    );
                    browser.select_record(Id::from_bytes([4; 16]), cx);
                    browser.receive(
                        Reply::Object(crate::engine::reply::ObjectResult {
                            database: browser.database_address().unwrap(),
                            object: canonical,
                        }),
                        cx,
                    );
                    browser.begin_values(field, cx);
                    assert_eq!(browser.capture_values_state(cx).unwrap().input, expected_input);
                    browser.begin_values(other_field, cx);
                    assert_eq!(
                        browser.records.values.as_ref().unwrap().values,
                        Some(vec![Value::Bytes(vec![6])])
                    );
                });
            }
        }
    }

    fn occupy_draft_slots(cx: &mut App) -> Vec<Id> {
        (100..132)
            .map(|byte| {
                let id = Id::from_bytes([byte; 16]);
                repository::upsert(
                    crate::store::browser_drafts::BrowserDraft {
                        id,
                        revision: 1,
                        volume: volume(2).id,
                        entry: Some(crate::desktop::browser::tests::entry(2).id),
                        ancestors: vec![],
                        body: BrowserDraftBody::Record(RecordDraft {
                            type_id: Id::from_bytes([2; 16]),
                            object: Id::from_bytes([byte; 16]),
                            fields: vec![RecordFieldDraft {
                                field: Id::from_bytes([3; 16]),
                                text: "another local draft".into(),
                                base_values: None,
                            }],
                            values: vec![],
                            active_values: None,
                            changed_during_edit: false,
                        }),
                    },
                    cx,
                )
                .unwrap();
                id
            })
            .collect()
    }

    #[gpui::test]
    fn a_full_repository_holds_retired_scalar_and_partial_values_until_explicit_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, _requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let mut metadata = object_details();
            let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else { panic!() };
            let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
            let mut repeated = declaration.fields[&Id::from_bytes([3; 16])].clone();
            repeated.value_type = ValueType::Bytes;
            repeated.repeated = true;
            declaration.fields.insert(Id::from_bytes([5; 16]), repeated);
            browser.selected = Some(metadata.entry.id);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                cx,
            );
            browser.select_record(Id::from_bytes([4; 16]), cx);
            let database = browser.database_address().unwrap();
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database,
                    object: record(4, "base", 1),
                }),
                cx,
            );
            browser.records.page = Some(Arc::new(crate::store::ObjectPage {
                objects: vec![record(4, "base", 1)],
                next: None,
            }));
            let slots = occupy_draft_slots(cx);
            let field = Id::from_bytes([3; 16]);
            let scalar = browser.field_editor(field, "base", cx);
            scalar.update(cx, |input, cx| input.acknowledge(None, "unpersisted scalar", cx));
            browser.field_changed(field, "unpersisted scalar", 1, cx);
            let mut values = state().values.remove(0);
            values.input = "0".into();
            values.input_dirty = true;
            browser.restore_values_state(&values, cx);
            let raw_values = browser.records.values.as_ref().unwrap().input.entity_id();
            assert!(browser.capture_record_draft(cx).is_err());
            metadata.readable = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: metadata }, cx);
            assert!(browser.retired_record_draft_pending());
            assert!(browser.records.page.is_none() && browser.records.object.is_none());
            assert!(browser.records.drafts.is_empty() && browser.records.values.is_none());
            let held = browser.retired_records.as_ref().unwrap();
            assert!(held.records.page.is_none() && held.records.object.is_none());
            assert_eq!(held.records.drafts[&field].editor.entity_id(), scalar.entity_id());
            assert_eq!(held.records.values.as_ref().unwrap().input.entity_id(), raw_values);
            browser.selected = None;
            browser.details = None;
            assert!(!browser.show_volume(cx), "failed hidden retention cannot discard raw edits");
            assert_eq!(repository::snapshots(cx).len(), 32);
            assert!(repository::remove(slots[0], cx));
            assert!(browser.retry_retired_record_draft(cx));
            assert!(!browser.retired_record_draft_pending());
            assert_eq!(repository::snapshots(cx).len(), 32, "recovery uses the freed slot");
            let saved = browser
                .local_draft_summaries(cx)
                .into_iter()
                .find(|draft| draft.volume == volume(1).id && draft.entry == Some(database.entry))
                .unwrap();
            let saved = browser.local_draft_snapshot(saved.id, cx).unwrap();
            let BrowserDraftBody::Record(state) = &saved.body else { panic!() };
            assert_eq!(state.fields[0].text, "unpersisted scalar");
            assert_eq!(state.values[0].input, "0");
            assert!(state.values[0].input_dirty);
            assert_eq!(state.values[0].editing, Some(0));
            assert!(
                !browser.restore_record_draft(saved.id, cx),
                "private recovery does not grant Read"
            );
        });
    }

    #[gpui::test]
    fn readable_restoration_follows_an_unadmitted_field_after_additive_schema_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        let (discard, id) = browser.update(cx, |browser, cx| {
            let mut metadata = object_details();
            let field = Id::from_bytes([18; 16]);
            let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else { panic!() };
            let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
            let template = declaration.fields[&Id::from_bytes([3; 16])].clone();
            for byte in 3..=19 {
                declaration.fields.insert(Id::from_bytes([byte; 16]), template.clone());
            }
            browser.selected = Some(metadata.entry.id);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                cx,
            );
            browser.select_record(Id::from_bytes([4; 16]), cx);
            let database = browser.database_address().unwrap();
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database,
                    object: record(4, "base", 1),
                }),
                cx,
            );
            let id = Id::from_bytes([80; 16]);
            repository::upsert(
                crate::store::browser_drafts::BrowserDraft {
                    id,
                    revision: 1,
                    volume: volume(1).id,
                    entry: Some(database.entry),
                    ancestors: vec![],
                    body: BrowserDraftBody::Record(RecordDraft {
                        type_id: Id::from_bytes([2; 16]),
                        object: Id::from_bytes([4; 16]),
                        fields: (3..=19)
                            .filter(|byte| *byte != 18)
                            .map(|byte| RecordFieldDraft {
                                field: Id::from_bytes([byte; 16]),
                                text: format!("stored {byte}"),
                                base_values: None,
                            })
                            .collect(),
                        values: vec![],
                        active_values: None,
                        changed_during_edit: false,
                    }),
                },
                cx,
            )
            .unwrap();
            browser.records.draft_id = Some(id);
            while requests.try_recv().is_ok() {}
            assert!(browser.publish_record_fields(
                BTreeMap::from([(
                    Id::from_bytes([3; 16]),
                    Some(vec![Value::Text("stored 3".into())])
                )]),
                cx
            ));
            let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
            assert_eq!(browser.records.field_offset, 0);
            let input = browser.field_editor(field, "", cx);
            input.update(cx, |input, cx| input.acknowledge(None, "seventeenth field", cx));
            browser.field_changed(field, "seventeenth field", 1, cx);
            assert!(browser.capture_record_draft(cx).is_err());
            let mut denied = metadata.clone();
            denied.readable = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
            assert!(browser.retired_record_draft_busy(cx));
            browser.discard_retired_record_draft(cx);
            assert!(
                browser.retired_record_draft_pending(),
                "an accepted write keeps its recovery intent"
            );
            let mut failed =
                crate::desktop::worker_response::WorkerResponse::new(Err("not saved".into()));
            failed.credit = queued.credit;
            crate::desktop::draft_publications::receive(&failed, cx);
            assert!(!browser.retired_record_draft_busy(cx));
            let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else { panic!() };
            schema
                .types
                .get_mut(&Id::from_bytes([2; 16]))
                .unwrap()
                .fields
                .insert(Id::from_bytes([2; 16]), template);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: metadata }, cx);
            assert!(browser.retired_records.is_none());
            assert!(
                browser.retired_record_draft_pending(),
                "whole-record recovery stays reachable"
            );
            assert!(browser.records.recovery_pending);
            assert_eq!(browser.records.field_offset, FIELD_PAGE);
            assert!(browser.records.object.is_none(), "held data is not a canonical reload");
            assert_eq!(browser.records.drafts[&field].editor.entity_id(), input.entity_id());
            browser.sync_record_local_draft(cx);
            assert!(browser.records.drafts[&field].dirty);
            assert_eq!(input.read(cx).text(), "seventeenth field");
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database,
                    object: record(4, "fresh", 2),
                }),
                cx,
            );
            browser.record_fields(cx);
            assert!(browser.rendered_controls.contains(&format!("field-discard-{field}")));
            (browser.controls[&format!("field-discard-{field}")].read(cx).popup_for_test().2, id)
        });
        cx.update(|cx| discard(0, cx));
        browser.update(cx, |browser, cx| {
            assert!(browser.show_volume(cx));
            assert!(!browser.retired_record_draft_pending());
            let saved = browser.local_draft_snapshot(id, cx).unwrap();
            let BrowserDraftBody::Record(state) = &saved.body else { panic!() };
            assert_eq!(state.fields.len(), 16);
            assert!(state.fields.iter().all(|field| field.text.starts_with("stored ")));
        });
    }

    #[gpui::test]
    fn whole_record_recovery_remains_reachable_when_schema_changes_split_a_full_draft_page(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, _requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        let (next, last_input) = browser.update(cx, |browser, cx| {
            let mut metadata = object_details();
            let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else { panic!() };
            let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
            let template = declaration.fields[&Id::from_bytes([3; 16])].clone();
            for byte in 3..19 {
                declaration.fields.insert(Id::from_bytes([byte; 16]), template.clone());
            }
            browser.selected = Some(metadata.entry.id);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                cx,
            );
            browser.select_record(Id::from_bytes([4; 16]), cx);
            let database = browser.database_address().unwrap();
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database,
                    object: record(4, "base", 1),
                }),
                cx,
            );
            occupy_draft_slots(cx);
            for byte in 3..19 {
                let field = Id::from_bytes([byte; 16]);
                let text = format!("unadmitted {byte}");
                let input = browser.field_editor(field, "", cx);
                input.update(cx, |input, cx| input.acknowledge(None, &text, cx));
                browser.field_changed(field, &text, 1, cx);
            }
            let last = Id::from_bytes([18; 16]);
            let last_input = browser.records.drafts[&last].editor.clone();
            assert!(browser.capture_record_draft(cx).is_err());
            let mut denied = metadata.clone();
            denied.readable = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
            assert!(browser.retired_records.is_some());
            let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else { panic!() };
            schema
                .types
                .get_mut(&Id::from_bytes([2; 16]))
                .unwrap()
                .fields
                .insert(Id::from_bytes([2; 16]), template.clone());
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                cx,
            );
            browser.receive(
                Reply::Object(crate::engine::reply::ObjectResult {
                    database,
                    object: record(4, "fresh", 2),
                }),
                cx,
            );
            assert!(browser.retired_records.is_none());
            assert!(browser.retired_record_draft_pending());
            assert_eq!(browser.records.field_offset, 0);
            browser.record_fields(cx);
            let editors: BTreeMap<_, _> = browser
                .records
                .drafts
                .iter()
                .map(|(id, draft)| (*id, draft.editor.entity_id()))
                .collect();
            assert_eq!(editors.len(), FIELD_PAGE);
            for suffix in 3..5 {
                let mut bytes = [2; 16];
                bytes[15] = suffix;
                let added = Id::from_bytes(bytes);
                let mut field = template.clone();
                field.value_type = ValueType::Boolean;
                let EntryKind::ObjectDb { schema } = &mut metadata.entry.info.kind else {
                    panic!()
                };
                schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap().fields.insert(added, field);
                browser.receive(
                    Reply::EntryDetails { volume: volume(1).id, details: metadata.clone() },
                    cx,
                );
                browser.record_fields(cx);
                assert_eq!(browser.records.drafts.len(), FIELD_PAGE);
                assert!(browser.rendered_text.contains(&format!("field-preview-{added}")));
                for (id, entity) in &editors {
                    assert_eq!(browser.records.drafts[id].editor.entity_id(), *entity);
                    assert!(browser.records.drafts[id].dirty);
                }
                browser.save_field(added, Some(vec![Value::Boolean(true)]), cx);
                assert_eq!(browser.records.drafts.len(), FIELD_PAGE);
                assert!(!browser.record_publication_pending(cx));
            }
            assert!(!browser.rendered_controls.contains(&format!("field-discard-{last}")));
            (browser.controls["fields-next"].read(cx).popup_for_test().2, last_input)
        });
        cx.update(|cx| next(0, cx));
        let retry = browser.update(cx, |browser, cx| {
            assert_eq!(
                browser.records.field_offset, 0,
                "the full repository blocks page retirement"
            );
            assert_eq!(browser.records.drafts.values().filter(|draft| draft.dirty).count(), 16);
            assert_eq!(
                browser.records.drafts[&Id::from_bytes([18; 16])].editor.entity_id(),
                last_input.entity_id()
            );
            browser.permissions_column(true, cx);
            browser.controls["retry-local-changes"].read(cx).popup_for_test().2
        });
        cx.update(|cx| retry(0, cx));
        let discard = browser.update(cx, |browser, cx| {
            assert!(browser.retired_record_draft_pending(), "a failed Retry retains every editor");
            assert_eq!(last_input.read(cx).text(), "unadmitted 18");
            assert!(!browser.retired_record_draft_busy(cx));
            browser.permissions_column(true, cx);
            browser.controls["discard-local-changes"].read(cx).popup_for_test().2
        });
        cx.update(|cx| discard(0, cx));
        browser.update(cx, |browser, cx| {
            assert!(!browser.retired_record_draft_pending());
            assert!(browser.records.drafts.is_empty() && browser.records.values.is_none());
            assert_eq!(browser.records.selected, Some(Id::from_bytes([4; 16])));
            assert!(browser.records.object.is_some(), "whole Discard retains fresh canonical data");
            assert_eq!(repository::snapshots(cx).len(), 32, "unrelated drafts remain intact");
            let clean = browser.field_editor(Id::from_bytes([3; 16]), "fresh", cx);
            assert_eq!(clean.read(cx).text(), "fresh");
            assert!(browser.show_volume(cx));
        });
    }
}
