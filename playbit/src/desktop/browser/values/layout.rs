//! Native value controls render at most sixteen rows and one bounded picker.
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Browser {
    pub(in crate::desktop::browser) fn value_field(
        &mut self,
        field: Id,
        declaration: &FieldDeclaration,
        values: Option<&Vec<Value>>,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let writable = writable && self.local_draft_ready(cx);
        self.restore_visible_values_draft(field, cx);
        if self.records.values.as_ref().is_some_and(|editor| editor.field == field) {
            return self.value_editor(writable, cx);
        }
        let mut panel = div().flex().flex_col().gap(ui(6.));
        let count = values.map_or(0, Vec::len);
        if count == 0 {
            panel = panel.child(self.text(&format!("value-empty-{field}"), "Empty", cx));
        }
        for (index, value) in values.into_iter().flatten().take(VALUE_PAGE).enumerate() {
            let mut row = div()
                .flex()
                .gap(ui(8.))
                .items_center()
                .child(self.text(&format!("value-{field}-{index}"), label(value), cx).flex_1());
            if matches!(value, Value::File { .. } | Value::Reference { .. }) {
                let value = value.clone();
                let kind = declaration.value_type;
                row = row.child(self.button(
                    &format!("value-open-{field}-{index}"),
                    "Open",
                    false,
                    move |this, cx| this.open_value(&value, kind, cx),
                    cx,
                ));
            }
            panel = panel.child(row);
        }
        if count > VALUE_PAGE {
            panel = panel.child(self.text(
                &format!("value-count-{field}"),
                format!("{} more values", count - VALUE_PAGE),
                cx,
            ));
        }
        let bounded = admit_values(values.map_or(&[], Vec::as_slice)).is_ok();
        if !bounded {
            panel = panel.child(self.text(
                &format!("values-limit-{field}"),
                "This value list exceeds the browser edit limit. Its original values are retained.",
                cx,
            ));
        }
        let retained = self.local_values_draft(field, cx).is_some();
        panel.child(self.button(
            &format!("values-edit-{field}"),
            "Edit values…",
            (!writable && !retained) || !bounded,
            move |this, cx| this.begin_values(field, cx),
            cx,
        ))
    }

    fn value_editor(&mut self, writable: bool, cx: &mut Context<Self>) -> gpui::Div {
        let Some(editor) = &self.records.values else { return div() };
        let field = editor.field;
        let after = editor.after;
        let values: Vec<_> =
            editor.values.iter().flatten().skip(after).take(VALUE_PAGE).cloned().collect();
        let count = editor.values.as_ref().map_or(0, Vec::len);
        let mut panel = div().flex().flex_col().gap(ui(8.));
        for (index, value) in values.iter().enumerate() {
            panel = panel.child(self.value_edit_row(field, after + index, value, !writable, cx));
        }
        if count > VALUE_PAGE {
            panel = panel.child(self.value_pages(after, count, cx));
        }
        panel = panel.child(self.value_input(writable, cx));
        if self.records.values.as_ref().is_some_and(|editor| editor.picker.is_some()) {
            panel = panel.child(self.picker_panel(cx));
        }
        panel.child(self.value_actions(writable, cx))
    }

    fn value_edit_row(
        &mut self,
        field: Id,
        index: usize,
        value: &Value,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let change = self.button(
            &format!("value-change-{field}-{index}"),
            "Change",
            disabled || self.record_publication_pending(cx),
            move |this, cx| this.change_value(index, cx),
            cx,
        );
        let remove = self.button(
            &format!("value-remove-{field}-{index}"),
            "Remove",
            disabled,
            move |this, cx| this.remove_value(index, cx),
            cx,
        );
        div()
            .flex()
            .items_center()
            .gap(ui(8.))
            .child(self.text(&format!("value-draft-{field}-{index}"), label(value), cx).flex_1())
            .child(change)
            .child(remove)
    }

    fn value_pages(&mut self, after: usize, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        let previous = self.button(
            "values-previous",
            "Previous values",
            after == 0,
            |this, cx| {
                if let Some(editor) = &mut this.records.values {
                    editor.after = editor.after.saturating_sub(VALUE_PAGE);
                }
                this.did_change_draft(cx);
            },
            cx,
        );
        let next = self.button(
            "values-next",
            "Next values",
            after + VALUE_PAGE >= count,
            |this, cx| {
                if let Some(editor) = &mut this.records.values {
                    editor.after += VALUE_PAGE;
                }
                this.did_change_draft(cx);
            },
            cx,
        );
        div().flex().gap(ui(8.)).child(previous).child(next)
    }

    fn value_input(&mut self, writable: bool, cx: &mut Context<Self>) -> gpui::Div {
        let Some(editor) = &self.records.values else { return div() };
        let kind = editor.kind;
        let editing = editor.editing.is_some();
        if matches!(kind, ValueType::File | ValueType::Reference(_)) {
            return div().child(self.button(
                "value-choose",
                if editing { "Choose replacement…" } else { "Choose target…" },
                !writable,
                |this, _| this.start_picker(),
                cx,
            ));
        }
        let input = editor.input.clone();
        input.update(cx, |input, cx| {
            input.set_theme(&theme());
            input.set_read_only(!writable, &theme(), cx);
            input.set_placeholder(if kind == ValueType::Bytes {
                "Hexadecimal bytes"
            } else {
                "New value"
            });
        });
        let content = if kind == ValueType::Boolean {
            let checked = input.read(cx).text() == "true";
            div().child(self.control(
                "value-boolean".into(),
                "Value",
                controls::Kind::Checkbox(if checked { CheckState::On } else { CheckState::Off }),
                !writable,
                move |this, cx| {
                    if let Some(editor) = &mut this.records.values {
                        editor.input_dirty = true;
                        editor.input.update(cx, |input, cx| {
                            input.acknowledge(None, if checked { "false" } else { "true" }, cx)
                        });
                    }
                    this.did_change_draft(cx);
                },
                cx,
            ))
        } else {
            div().flex_1().min_w_0().child(input)
        };
        let add = self.button(
            "value-add",
            if editing { "Apply value" } else { "Add value" },
            !writable,
            |this, cx| this.input_value(cx),
            cx,
        );
        content.child(add).child(self.button(
            "value-input-discard",
            "Discard input",
            self.record_publication_pending(cx),
            |this, cx| {
                if this.record_publication_pending(cx) {
                    return;
                }
                let clean_field = if let Some(editor) = &mut this.records.values {
                    editor.input_dirty = false;
                    editor.editing = None;
                    editor.input.update(cx, |input, cx| input.clear(cx));
                    (!editor.dirty && editor.submitted.is_none()).then_some(editor.field)
                } else {
                    None
                };
                if let Some(field) = clean_field {
                    this.discard_values_draft(field, cx);
                }
                this.did_change_draft(cx);
            },
            cx,
        ))
    }

    fn value_actions(&mut self, writable: bool, cx: &mut Context<Self>) -> gpui::Div {
        let Some(editor) = &self.records.values else { return div() };
        let dirty = editor.dirty && !editor.input_dirty;
        let pending = self.record_publication_pending(cx);
        let save = self.button(
            "values-save",
            "Save values",
            !writable || !dirty || pending,
            |this, cx| this.save_values(cx),
            cx,
        );
        let clear = self.button(
            "values-clear",
            "Clear field",
            !writable || pending,
            |this, cx| this.clear_values(cx),
            cx,
        );
        let discard = self.button(
            "values-discard",
            "Discard draft",
            pending,
            |this, cx| {
                if this.record_publication_pending(cx) {
                    return;
                }
                let field = this.records.values.as_ref().map(|editor| editor.field);
                this.records.values = None;
                if let Some(field) = field {
                    this.discard_values_draft(field, cx);
                }
            },
            cx,
        );
        div().flex().flex_wrap().gap(ui(8.)).child(save).child(clear).child(discard)
    }

    fn picker_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(picker) = self.records.values.as_ref().and_then(|editor| editor.picker.as_ref())
        else {
            return div();
        };
        let volume = picker.volume;
        let objects = picker.entry.is_some();
        let after = if objects { picker.object_after.is_some() } else { picker.after.is_some() };
        let more = if objects { picker.object_next.is_some() } else { picker.next.is_some() };
        let choice = self.picker_volume_popup(volume, cx);
        let rows = self.picker_choices(cx);
        let first = self.button(
            "picker-first",
            "First targets",
            !after,
            move |this, _| this.picker_page(objects, true),
            cx,
        );
        let next = self.button(
            "picker-next",
            "Next targets",
            !more,
            move |this, _| this.picker_page(objects, false),
            cx,
        );
        let cancel = self.button(
            "picker-cancel",
            "Cancel selection",
            false,
            |this, _| {
                if let Some(editor) = &mut this.records.values {
                    editor.picker = None;
                }
            },
            cx,
        );
        div()
            .flex()
            .flex_col()
            .gap(ui(6.))
            .child(choice)
            .child(rows)
            .child(div().flex().flex_wrap().gap(ui(8.)).child(first).child(next).child(cancel))
    }

    fn picker_volume_popup(
        &mut self,
        volume: Id,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let volumes: Vec<_> = self
            .available_volumes
            .iter()
            .map(|volume| (volume.id, volume.name.chars().take(60).collect::<String>()))
            .collect();
        let volume_items = menu(&volumes, Some(volume));
        let volume_label = volumes
            .iter()
            .find(|(id, _)| *id == volume)
            .map(|(_, name)| name.as_str())
            .unwrap_or("Choose volume");
        let volume_ids: Vec<_> = volumes.iter().map(|(id, _)| *id).collect();
        self.popup(
            "picker-volume",
            volume_label,
            volumes.is_empty(),
            &volume_items,
            move |index, this, _| {
                if let Some(id) = volume_ids.get(index as usize) {
                    this.picker_volume(*id);
                }
            },
            cx,
        )
    }

    fn picker_choices(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(picker) = self.records.values.as_ref().and_then(|editor| editor.picker.as_ref())
        else {
            return div();
        };
        if picker.entry.is_some() {
            let candidates = picker.objects.clone();
            let items = menu(&candidates, None);
            let ids: Vec<_> = candidates.iter().map(|(id, _)| *id).collect();
            let choice = self.popup(
                "picker-object",
                if picker.loading {
                    "Loading records…"
                } else if picker.objects.is_empty() {
                    "No readable matching records"
                } else {
                    "Choose record"
                },
                candidates.is_empty(),
                &items,
                move |index, this, cx| {
                    if let Some(id) = ids.get(index as usize) {
                        this.picker_object(*id, cx);
                    }
                },
                cx,
            );
            let back = self.button(
                "picker-back",
                "Choose another database",
                false,
                |this, _| {
                    if let Some(picker) =
                        this.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
                    {
                        picker.entry = None;
                        picker.readable = false;
                        picker.objects.clear();
                    }
                    this.refresh_picker();
                },
                cx,
            );
            return div().flex().gap(ui(6.)).child(choice).child(back);
        }
        let candidates = picker.entries.clone();
        let items = menu(&candidates, None);
        let ids: Vec<_> = candidates.iter().map(|(id, _)| *id).collect();
        div().child(self.popup(
            "picker-entry",
            if picker.loading {
                "Loading targets…"
            } else if candidates.is_empty() {
                "No matching targets on this page"
            } else {
                "Choose file or database"
            },
            candidates.is_empty(),
            &items,
            move |index, this, _| {
                if let Some(id) = ids.get(index as usize) {
                    this.picker_entry(*id);
                }
            },
            cx,
        ))
    }
}

fn menu<T: PartialEq>(rows: &[(T, String)], selected: Option<T>) -> Vec<crate::runtime::MenuEntry> {
    rows.iter()
        .enumerate()
        .map(|(index, (id, label))| crate::runtime::MenuEntry::Action {
            icon: None,
            id: index as u32,
            label: label.clone(),
            checked: Some(selected.as_ref() == Some(id)),
            disabled: false,
            destructive: false,
        })
        .collect()
}
