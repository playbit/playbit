//! Opening a link resolves its full address and never rewrites a dangling value.
use super::*;

pub(in crate::desktop::browser) struct Target {
    address: EntryAddress,
    object: Option<(Id, Id)>,
}

impl Browser {
    pub(in crate::desktop::browser) fn has_record_draft(&self) -> bool {
        self.records.drafts.values().any(|draft| draft.dirty || draft.submitted.is_some())
            || self.records.values.as_ref().is_some_and(|editor| {
                editor.dirty || editor.input_dirty || editor.submitted.is_some()
            })
    }

    pub(in crate::desktop::browser) fn open_value(
        &mut self,
        value: &Value,
        kind: ValueType,
        cx: &mut Context<Self>,
    ) {
        if !self.prepare_navigation(cx) {
            return;
        }
        let target = match (value, kind) {
            (Value::File { volume_id, entry_id }, ValueType::File) => Target {
                address: EntryAddress { volume: *volume_id, entry: *entry_id },
                object: None,
            },
            (
                Value::Reference { volume_id, database_id, object_id },
                ValueType::Reference(type_id),
            ) => Target {
                address: EntryAddress { volume: *volume_id, entry: *database_id },
                object: Some((*object_id, type_id)),
            },
            _ => return,
        };
        let Some(volume) = self
            .available_volumes
            .iter()
            .find(|volume| volume.id == target.address.volume)
            .cloned()
        else {
            self.error = "The referenced volume is not available on this device.".into();
            return;
        };
        self.volume = volume;
        self.volume_details = None;
        self.catalog = super::super::namespace::Catalog::default();
        // The source was captured under its own volume. Retire its editors before
        // selection captures again under the destination's identity.
        self.clear_entry_view(cx);
        self.select(target.address.entry, cx);
        self.open_target = Some(target);
        self.refresh();
    }

    pub(in crate::desktop::browser) fn finish_open_target(&mut self, cx: &mut Context<Self>) {
        let Some(target) = &self.open_target else { return };
        let Some(details) = &self.details else { return };
        if target.address.volume != self.volume.id || Some(target.address.entry) != self.selected {
            return;
        }
        if details.entry.deleted || !details.readable || !details.backing_ready {
            self.open_target = None;
            return;
        }
        let Some((object, type_id)) = target.object else {
            if !matches!(details.entry.info.kind, EntryKind::Blob { .. }) {
                self.error = "The referenced entry is not a file.".into();
            }
            self.open_target = None;
            return;
        };
        let EntryKind::ObjectDb { schema } = &details.entry.info.kind else {
            self.error = "The referenced entry is not an object database.".into();
            self.open_target = None;
            return;
        };
        if !schema.types.contains_key(&type_id) {
            self.error = "The referenced record type is not present in this database.".into();
            self.open_target = None;
            return;
        }
        self.open_target = None;
        self.select_type(type_id, cx);
        self.select_record(object, cx);
    }
}
