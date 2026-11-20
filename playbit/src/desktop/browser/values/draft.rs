//! Source writes retain the exact draft through remote changes and failures.
use super::*;
use crate::store::browser_drafts::RecordValuesDraft;

impl Browser {
    pub(in crate::desktop::browser) fn retire_record_values(&mut self, cx: &App) {
        if let Some(editor) = &mut self.records.values {
            editor.input_dirty |= editor.input.read(cx).is_composing();
            editor.picker = None;
            if !editor.dirty && !editor.input_dirty && editor.submitted.is_none() {
                self.records.values = None;
            }
        }
    }

    pub(in crate::desktop::browser) fn begin_values(&mut self, field: Id, cx: &mut Context<Self>) {
        if self.database_address().is_none() || !self.local_draft_ready(cx) {
            return;
        }
        if self.records.values.as_ref().is_some_and(|editor| editor.field == field) {
            return;
        }
        if !self.retain_local_drafts(cx) {
            return;
        }
        if let Some(restored) = self.local_values_draft(field, cx) {
            self.restore_values_state(&restored, cx);
            return;
        }
        let Some(object) = &self.records.object else { return };
        let Some(declaration) = self.field_declaration(field) else { return };
        let values = object.fields.get(&field).and_then(|cell| cell.values.as_ref());
        if let Err(error) = admit_values(values.map_or(&[], Vec::as_slice)) {
            self.error = error.into();
            return;
        }
        let state = RecordValuesDraft {
            field,
            kind: declaration.value_type,
            repeated: declaration.repeated,
            base_values: values.cloned(),
            values: values.cloned(),
            input: String::new(),
            editing: None,
            after: 0,
            input_dirty: false,
            dirty: false,
        };
        self.restore_values_state(&state, cx);
    }

    pub(in crate::desktop::browser) fn restore_values_state(
        &mut self,
        state: &RecordValuesDraft,
        cx: &mut Context<Self>,
    ) {
        let field = state.field;
        let weak = cx.weak_entity();
        let database = self.database_address();
        let object = self.records.selected;
        let changed: text_editor::CommitCallback = Arc::new(move |_, _, cx| {
            weak.update(cx, |this, cx| {
                if this.database_address() == database
                    && this.records.selected == object
                    && let Some(editor) = &mut this.records.values
                    && editor.field == field
                {
                    editor.input_dirty = true;
                }
                this.did_change_draft(cx);
                cx.notify();
            })
            .is_ok()
        });
        self.records.values = Some(Editor {
            field,
            kind: state.kind,
            repeated: state.repeated,
            values: state.values.clone(),
            base_values: state.base_values.clone(),
            dirty: state.dirty,
            input_dirty: state.input_dirty,
            submitted: None,
            input: cx.new(|cx| {
                text_editor::TextEditor::new(state.input.as_str(), cx)
                    .literal()
                    .placeholder("New value")
                    .bound_edits(VALUE_BYTES)
                    .on_change(changed)
            }),
            editing: state.editing.map(|index| index as usize),
            after: state.after as usize,
            picker: None,
        });
    }

    pub(in crate::desktop::browser) fn capture_values_state(
        &self,
        cx: &App,
    ) -> Option<RecordValuesDraft> {
        let editor = self.records.values.as_ref()?;
        Some(RecordValuesDraft {
            field: editor.field,
            kind: editor.kind,
            repeated: editor.repeated,
            base_values: editor.base_values.clone(),
            values: editor.values.clone(),
            input: editor.input.read(cx).text().to_owned(),
            editing: editor.editing.map(|index| index as u32),
            after: editor.after as u32,
            input_dirty: editor.input_dirty || editor.input.read(cx).is_composing(),
            dirty: editor.dirty || editor.submitted.is_some(),
        })
    }

    pub(in crate::desktop::browser) fn sync_values_state(
        &mut self,
        state: Option<&RecordValuesDraft>,
    ) {
        let Some(editor) = &mut self.records.values else { return };
        if let Some(state) = state {
            editor.base_values = state.base_values.clone();
            editor.dirty = state.dirty;
            editor.input_dirty = state.input_dirty;
        } else {
            editor.base_values = editor.values.clone();
            editor.dirty = false;
            editor.input_dirty = false;
        }
        editor.submitted = None;
    }

    fn field_declaration(&self, field: Id) -> Option<&FieldDeclaration> {
        let EntryKind::ObjectDb { schema } = &self.details.as_ref()?.entry.info.kind else {
            return None;
        };
        schema.types.get(&self.records.type_id?)?.fields.get(&field)
    }

    pub(in crate::desktop::browser) fn acknowledge_values(&mut self, object: &ObjectFields) {
        let Some(editor) = &mut self.records.values else { return };
        let values = object.fields.get(&editor.field).and_then(|cell| cell.values.as_ref());
        if let Some(submitted) = &editor.submitted
            && values == submitted.as_ref()
        {
            editor.dirty = editor.values != *submitted;
            editor.base_values = values.cloned();
            editor.submitted = None;
            return;
        }
        if editor.dirty || editor.input_dirty {
            self.records.changed_during_edit |= editor.base_values.as_ref() != values;
        } else {
            match admit_values(values.map_or(&[], Vec::as_slice)) {
                Ok(()) => {
                    editor.values = values.cloned();
                    editor.base_values = values.cloned();
                }
                Err(error) => {
                    self.records.values = None;
                    self.error = error.into();
                }
            }
        }
    }

    pub(in crate::desktop::browser) fn save_values(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.records.values else { return };
        if !editor.dirty
            || editor.input_dirty
            || editor.submitted.is_some()
            || !self.details.as_ref().is_some_and(|details| details.writable)
        {
            return;
        }
        let field = editor.field;
        let values = editor.values.clone();
        if self.publish_record_fields(BTreeMap::from([(field, values.clone())]), cx)
            && let Some(editor) = &mut self.records.values
        {
            editor.submitted = Some(values);
        }
        cx.notify();
    }

    pub(in crate::desktop::browser) fn input_value(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.records.values else { return };
        let text = editor.input.read(cx).text();
        let value = match editor.kind {
            ValueType::Bytes => parse_bytes(text).map(Value::Bytes),
            ValueType::Boolean => Ok(Value::Boolean(text == "true")),
            kind => super::super::records::parse_scalar(kind, text),
        };
        match value.and_then(|value| editor.replace(value)) {
            Ok(()) => {
                editor.input_dirty = false;
                editor.input.update(cx, |input, cx| input.clear(cx));
            }
            Err(error) => self.error = error.into(),
        }
        self.did_change_draft(cx);
    }

    pub(in crate::desktop::browser) fn clear_values(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.records.values {
            editor.values = None;
            editor.dirty = true;
            editor.input_dirty = false;
            editor.editing = None;
            editor.picker = None;
            editor.input.update(cx, |input, cx| input.clear(cx));
        }
        self.did_change_draft(cx);
    }

    pub(in crate::desktop::browser) fn remove_value(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.records.values else { return };
        let Some(values) = &mut editor.values else { return };
        if index >= values.len() {
            return;
        }
        values.remove(index);
        editor.dirty = true;
        editor.editing = None;
        editor.picker = None;
        editor.after = editor.after.min(values.len().saturating_sub(1) / VALUE_PAGE * VALUE_PAGE);
        self.did_change_draft(cx);
    }

    pub(in crate::desktop::browser) fn change_value(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.record_publication_pending(cx) {
            return;
        }
        let Some(editor) = &mut self.records.values else { return };
        let Some(value) = editor.values.as_ref().and_then(|values| values.get(index)) else {
            return;
        };
        editor.editing = Some(index);
        editor.input_dirty = false;
        let text = match value {
            Value::Text(text) => text.clone(),
            Value::Bytes(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            value => label(value),
        };
        editor.input.update(cx, |input, cx| input.acknowledge(None, &text, cx));
        let choose_target = matches!(editor.kind, ValueType::File | ValueType::Reference(_));
        let clean_field = (!editor.dirty && editor.submitted.is_none()).then_some(editor.field);
        if let Some(field) = clean_field {
            self.discard_values_draft(field, cx);
        }
        if choose_target {
            self.start_picker();
        }
        self.did_change_draft(cx);
    }
}

fn parse_bytes(text: &str) -> Result<Vec<u8>, &'static str> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Enter bytes as pairs of hexadecimal digits.");
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| (byte as char).to_digit(16).ok_or("Enter hexadecimal bytes.");
            Ok((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_input_checks_pairs_and_preserves_exact_bytes() {
        assert_eq!(parse_bytes("00fF12"), Ok(vec![0, 255, 18]));
        assert!(parse_bytes("0").is_err());
        assert!(parse_bytes("π").is_err());
        assert!(parse_bytes("xx").is_err());
    }
}
