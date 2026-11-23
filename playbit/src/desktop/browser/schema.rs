//! Additive schema editing sends only the new declaration and preserves stable IDs.
use super::*;
use crate::store::object_contract::{FieldDeclaration, TypeDeclaration, ValueType};
use crate::store::volume_index::EntryAddress;
use crate::ui::desktop::zoom::length as ui;

impl Browser {
    pub(super) fn schema_navigation(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(details) = self.details.clone() else { return div() };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else { return div() };
        let mut panel = div().flex().flex_col().gap(ui(6.)).pl(ui(24.)).pr(ui(8.)).pb(ui(8.));
        let overview = self.button(
            "entry-overview",
            "Details and permissions",
            false,
            |this, _| {
                this.overview = true;
                this.show_entry_details = true;
            },
            cx,
        );
        for (id, declaration) in schema.types.iter().skip(self.schema_offset).take(32) {
            let id = *id;
            let label = format!(
                "{}{}",
                declaration.name,
                if declaration.deprecated { " · Deprecated" } else { "" }
            );
            let control = self.button(
                &format!("schema-type-{id}"),
                &label,
                !details.readable || !details.backing_ready || details.entry.deleted,
                move |this, cx| this.select_type(id, cx),
                cx,
            );
            let accessory = self.find_local_draft(self.selected, |target| matches!(target,
                crate::store::browser_drafts::BrowserDraftTarget::Record { type_id, .. } if *type_id == id
            ), cx).map(|draft| self.draft_button(draft, &format!("type-{id}"), cx));
            control.update(cx, |control, cx| {
                control.set_navigation_accessory(accessory, cx);
                control.select_navigation(!self.overview && self.records.type_id == Some(id), cx);
                control.set_navigation_glyph("entry-type", cx);
            });
            panel = panel.child(div().h(ui(30.)).child(control));
        }
        if schema.types.len() > 32 {
            let offset = self.schema_offset;
            let previous = self.button(
                "types-previous",
                "Previous types",
                offset == 0,
                |this, _| this.schema_offset = this.schema_offset.saturating_sub(32),
                cx,
            );
            let next = self.button(
                "types-next",
                "Next types",
                offset + 32 >= schema.types.len(),
                |this, _| this.schema_offset += 32,
                cx,
            );
            panel = panel.child(div().flex().gap(ui(8.)).child(previous).child(next));
        }
        panel = panel.child(overview).child(self.type_form(details.writable, cx));
        if self.records.type_id.is_some() {
            panel = panel.child(self.field_form(cx));
        }
        panel
    }

    fn type_form(&mut self, writable: bool, cx: &mut Context<Self>) -> gpui::Div {
        if !self.adding_type {
            return div().child(self.button(
                "type-start",
                "New type…",
                !writable,
                |this, _| this.adding_type = true,
                cx,
            ));
        }
        let valid = !self.schema_name.read(cx).text().trim().is_empty();
        let add = self.button(
            "type-add",
            "Add type",
            !writable || !valid,
            |this, cx| {
                let Some(entry) = this.selected else { return };
                let id = match Id::generate() {
                    Ok(id) => id,
                    Err(error) => {
                        this.error = error.to_string();
                        return;
                    }
                };
                let declaration = TypeDeclaration {
                    name: this.schema_name.read(cx).text().into(),
                    deprecated: false,
                    fields: BTreeMap::new(),
                };
                this.submit(Request::Schema {
                    session: this.request_session(),
                    database: EntryAddress { volume: this.volume.id, entry },
                    schema: Schema { types: BTreeMap::from([(id, declaration)]) },
                });
            },
            cx,
        );
        div().flex().flex_col().gap(ui(6.)).child(self.schema_name.clone()).child(add)
    }

    pub(super) fn field_form(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let writable = self.details.as_ref().is_some_and(|details| details.writable);
        if !self.adding_field {
            return div().child(self.button(
                "field-start",
                "New field…",
                !writable,
                |this, _| this.adding_field = true,
                cx,
            ));
        }
        let kind = self.field_kind_control(writable, cx);
        let valid = !self.field_name.read(cx).text().trim().is_empty();
        let add = self.button(
            "field-add",
            "Add field",
            !writable || !valid,
            |this, cx| this.add_field(cx),
            cx,
        );
        let repeated = self.control(
            "field-repeated".into(),
            "Multiple values",
            controls::Kind::Checkbox(if self.field_repeated {
                CheckState::On
            } else {
                CheckState::Off
            }),
            !writable,
            |this, _| this.field_repeated = !this.field_repeated,
            cx,
        );
        div()
            .flex()
            .flex_col()
            .gap(ui(6.))
            .child(self.text("field-add-label", "New field", cx).text_size(ui(11.)))
            .child(self.field_name.clone())
            .child(repeated)
            .child(div().flex().gap(ui(8.)).child(kind).child(add))
    }

    fn field_kind_control(
        &mut self,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let reference = self.records.type_id.unwrap_or(self.volume.id);
        let kinds = [
            ("Text", ValueType::Text),
            ("Boolean", ValueType::Boolean),
            ("Integer", ValueType::Integer),
            ("Number", ValueType::Float),
            ("Bytes", ValueType::Bytes),
            ("File", ValueType::File),
            ("Record reference", ValueType::Reference(reference)),
        ];
        let label = kinds
            .iter()
            .find(|(_, kind)| *kind == self.field_kind)
            .map(|(label, _)| *label)
            .unwrap_or("Record reference");
        let items: Vec<_> = kinds
            .iter()
            .enumerate()
            .map(|(id, (label, kind))| crate::runtime::MenuEntry::Action {
                icon: None,
                id: id as u32,
                label: (*label).into(),
                checked: Some(*kind == self.field_kind),
                disabled: false,
                destructive: false,
            })
            .collect();
        self.popup(
            "field-kind",
            label,
            !writable,
            &items,
            move |id, this, _| {
                if let Some((_, kind)) = kinds.get(id as usize) {
                    this.field_kind = *kind;
                }
            },
            cx,
        )
    }

    fn add_field(&mut self, cx: &mut Context<Self>) {
        let (Some(database), Some(type_id), Some(details)) =
            (self.database_address(), self.records.type_id, self.details.as_ref())
        else {
            return;
        };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else { return };
        let Some(declaration) = schema.types.get(&type_id) else { return };
        let field_id = match Id::generate() {
            Ok(id) => id,
            Err(error) => {
                self.error = error.to_string();
                return;
            }
        };
        let kind = match self.field_kind {
            ValueType::Reference(_) => ValueType::Reference(type_id),
            kind => kind,
        };
        let declaration = TypeDeclaration {
            name: declaration.name.clone(),
            deprecated: declaration.deprecated,
            fields: BTreeMap::from([(
                field_id,
                FieldDeclaration {
                    name: self.field_name.read(cx).text().into(),
                    value_type: kind,
                    repeated: self.field_repeated,
                    deprecated: false,
                },
            )]),
        };
        self.submit(Request::Schema {
            session: self.request_session(),
            database,
            schema: Schema { types: BTreeMap::from([(type_id, declaration)]) },
        });
    }
}
