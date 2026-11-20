//! Query one exposed volume and one database; current permissions admit targets.
use super::*;

pub(super) struct Picker {
    pub loading: bool,
    pub volume: Id,
    pub after: Option<EntryId>,
    pub entries: Vec<(EntryId, String)>,
    pub next: Option<EntryId>,
    pub entry: Option<EntryId>,
    pub readable: bool,
    pub objects: Vec<(Id, String)>,
    pub object_after: Option<Id>,
    pub object_next: Option<Id>,
}
impl Picker {
    fn new(volume: Id) -> Self {
        Self {
            loading: false,
            volume,
            after: None,
            entries: vec![],
            next: None,
            entry: None,
            readable: false,
            objects: vec![],
            object_after: None,
            object_next: None,
        }
    }
}

impl Browser {
    pub(in crate::desktop) fn set_volumes(&mut self, volumes: Vec<Volume>) {
        self.available_volumes = volumes;
        if let Some(picker) = self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
            && !self.available_volumes.iter().any(|volume| volume.id == picker.volume)
        {
            picker.entries.clear();
            picker.objects.clear();
            picker.readable = false;
            self.error = "The selected volume is no longer available.".into();
        }
    }

    pub(in crate::desktop::browser) fn start_picker(&mut self) {
        let Some(editor) = &mut self.records.values else { return };
        editor.picker = Some(Picker::new(self.volume.id));
        self.refresh_picker();
    }

    pub(in crate::desktop::browser) fn refresh_picker(&mut self) {
        let Some(editor) = &self.records.values else { return };
        let Some(picker) = &editor.picker else { return };
        let request = if let Some(entry) = picker.entry {
            Request::EntryDetails { volume: picker.volume, entry }
        } else {
            Request::Entries { volume: picker.volume, after: picker.after, limit: 64 }
        };
        let submitted = self.submit(request);
        if let Some(picker) = self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
        {
            picker.loading = submitted;
        }
    }

    pub(in crate::desktop::browser) fn receive_picker(
        &mut self,
        reply: &Reply,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(editor) = &mut self.records.values else { return false };
        let Some(picker) = &mut editor.picker else { return false };
        match reply {
            Reply::Entries { volume, after, page }
                if *volume == picker.volume && *after == picker.after =>
            {
                picker.entries_page(page, editor.kind);
            }
            Reply::EntryDetails { volume, details }
                if *volume == picker.volume && Some(details.entry.id) == picker.entry =>
            {
                match editor.accept_target(details) {
                    Ok(Some(request)) => {
                        let submitted = self.submit(request);
                        if let Some(picker) =
                            self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
                        {
                            picker.loading = submitted;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => self.error = error.into(),
                }
            }
            Reply::Objects(result)
                if Some(result.database.entry) == picker.entry
                    && result.database.volume == picker.volume
                    && editor.kind == ValueType::Reference(result.type_id)
                    && result.after == picker.object_after
                    && picker.readable =>
            {
                picker.objects = result
                    .page
                    .objects
                    .iter()
                    .map(|object| (object.object_id, super::super::records::record_label(object)))
                    .collect();
                picker.object_next = result.page.next;
                picker.loading = false;
            }
            _ => return false,
        }
        self.did_change_draft(cx);
        true
    }

    pub(in crate::desktop::browser) fn picker_failed(&mut self, request: &Request) -> bool {
        let Some(picker) = self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
        else {
            return false;
        };
        let matches = match request {
            Request::Entries { volume, after, .. } => {
                *volume == picker.volume && *after == picker.after
            }
            Request::EntryDetails { volume, entry } => {
                *volume == picker.volume && Some(*entry) == picker.entry
            }
            Request::Objects { database, after, .. } => {
                database.volume == picker.volume
                    && Some(database.entry) == picker.entry
                    && *after == picker.object_after
            }
            _ => false,
        };
        if matches {
            picker.loading = false;
            picker.entries.clear();
            picker.objects.clear();
            picker.readable = false;
        }
        matches
    }

    pub(in crate::desktop::browser) fn picker_volume(&mut self, volume: Id) {
        if !self.available_volumes.iter().any(|available| available.id == volume) {
            return;
        }
        let Some(editor) = &mut self.records.values else { return };
        editor.picker = Some(Picker::new(volume));
        self.refresh_picker();
    }

    pub(in crate::desktop::browser) fn picker_entry(&mut self, entry: EntryId) {
        let Some(picker) = self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
        else {
            return;
        };
        if !picker.entries.iter().any(|(id, _)| *id == entry) {
            return;
        }
        picker.entry = Some(entry);
        picker.readable = false;
        picker.objects.clear();
        picker.object_after = None;
        picker.object_next = None;
        self.refresh_picker();
    }

    pub(in crate::desktop::browser) fn picker_object(
        &mut self,
        object_id: Id,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.records.values else { return };
        let Some(picker) = &editor.picker else { return };
        let (Some(database_id), true) = (picker.entry, picker.readable) else { return };
        if !picker.objects.iter().any(|(id, _)| *id == object_id) {
            return;
        }
        let value = Value::Reference { volume_id: picker.volume, database_id, object_id };
        if let Err(error) = editor.replace(value) {
            self.error = error.into();
        }
        self.did_change_draft(cx);
    }

    pub(in crate::desktop::browser) fn picker_page(&mut self, objects: bool, first: bool) {
        let Some(picker) = self.records.values.as_mut().and_then(|editor| editor.picker.as_mut())
        else {
            return;
        };
        if objects {
            picker.object_after = if first { None } else { picker.object_next };
            picker.objects.clear();
            picker.readable = false;
        } else {
            picker.after = if first { None } else { picker.next };
            picker.entries.clear();
            picker.entry = None;
            picker.objects.clear();
        }
        self.refresh_picker();
    }
}

impl Picker {
    fn entries_page(&mut self, page: &EntryPage, kind: ValueType) {
        self.entries = page
            .entries
            .iter()
            .filter(|entry| {
                !entry.deleted
                    && matches!(
                        (entry.kind, kind),
                        (crate::store::EntryType::Blob, ValueType::File)
                            | (crate::store::EntryType::ObjectDb, ValueType::Reference(_))
                    )
            })
            .map(|entry| {
                (
                    entry.id,
                    format!(
                        "{}{}",
                        entry.name.chars().take(60).collect::<String>(),
                        if entry.name_conflict { " · Name conflict" } else { "" }
                    ),
                )
            })
            .collect();
        self.next = page.next;
        self.loading = false;
    }
}
impl Editor {
    fn accept_target(&mut self, details: &EntryDetails) -> Result<Option<Request>, &'static str> {
        let picker = self.picker.as_mut().ok_or("Select a target again.")?;
        picker.loading = false;
        if !details.readable || details.entry.deleted || !details.backing_ready {
            picker.readable = false;
            picker.objects.clear();
            return Err("The target is unavailable or you do not have permission to read it.");
        }
        picker.readable = false;
        picker.objects.clear();
        match (&details.entry.info.kind, self.kind) {
            (EntryKind::Blob { .. }, ValueType::File) => {
                let value = Value::File { volume_id: picker.volume, entry_id: details.entry.id };
                self.replace(value)?;
                Ok(None)
            }
            (EntryKind::ObjectDb { schema }, ValueType::Reference(type_id))
                if schema.types.contains_key(&type_id) =>
            {
                picker.readable = true;
                Ok(Some(Request::Objects {
                    database: EntryAddress { volume: picker.volume, entry: details.entry.id },
                    type_id,
                    after: picker.object_after,
                    limit: 64,
                }))
            }
            _ => Err("The selected entry has a different kind or schema."),
        }
    }
}
