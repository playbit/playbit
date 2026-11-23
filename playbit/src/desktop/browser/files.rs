//! Native dialogs select one path; bounded background diagnostics own its I/O.
use super::*;
use crate::engine::reply::DiagnosticReply;
use crate::store::volume_index::EntryAddress;
use std::path::PathBuf;

/// Compact binary units promote above 1000 before rounding to one decimal.
pub(super) fn format_byte_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let bytes = u128::from(bytes);
    let mut divisor = 1;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && bytes > 1000 * divisor {
        divisor *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let tenths = (bytes * 10 + divisor / 2) / divisor;
    if tenths % 10 == 0 {
        format!("{} {}", tenths / 10, UNITS[unit])
    } else {
        format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
    }
}

#[derive(Default)]
pub(super) struct Files {
    pub(super) dialog: bool,
    pub pending: Option<Pending>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Pending {
    Import { volume: Id, parent: Option<EntryId>, name: String },
    Duplicate(EntryAddress),
    Delete(EntryAddress),
}
impl Pending {
    pub(super) fn for_request(request: &Request) -> Option<Self> {
        Some(match request {
            Request::FileCreate { volume, name, .. } => {
                Self::Import { volume: *volume, parent: None, name: name.clone() }
            }
            Request::FileCreateIn { volume, parent, name, .. } => {
                Self::Import { volume: *volume, parent: Some(*parent), name: name.clone() }
            }
            Request::DuplicateEntry { address } => Self::Duplicate(*address),
            Request::RemoveEntry { volume, entry, .. } => {
                Self::Delete(EntryAddress { volume: *volume, entry: *entry })
            }
            _ => return None,
        })
    }
}
impl Files {
    pub(super) fn busy(&self) -> bool {
        self.dialog || self.pending.is_some()
    }
}

impl Browser {
    pub(super) fn import_file_in(&mut self, parent: Option<EntryId>, cx: &mut Context<Self>) {
        if !self.can_create_entry_in(parent)
            || self.files.busy()
            || self.creating_entry.is_some()
            || !self.prepare_navigation(cx)
        {
            return;
        }
        self.files.dialog = true;
        let volume = self.volume.id;
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import file".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            let _ = this.update(cx, |this, cx| {
                this.files.dialog = false;
                match result {
                    Ok(Ok(Some(mut paths))) if paths.len() == 1 => {
                        let path = paths.remove(0);
                        let name = {
                            let Some(component) = path.file_name().and_then(|name| name.to_str())
                            else {
                                this.error = "The selected file has no UTF-8 name.".into();
                                return;
                            };
                            component.to_owned()
                        };
                        this.import_selected(volume, parent, &name, path);
                    }
                    Ok(Ok(None)) => {}
                    _ => this.error = "Could not select a file.".into(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn import_selected(
        &mut self,
        volume: Id,
        parent: Option<EntryId>,
        name: &str,
        path: PathBuf,
    ) {
        if self.volume.id != volume || !self.can_create_entry_in(parent) {
            return;
        }
        if path.as_os_str().len() > 4096 {
            self.error = "The selected file path is too long.".into();
            return;
        }
        let request = {
            let name = if name.is_empty() {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    self.error = "Enter a name for this file.".into();
                    return;
                };
                name
            } else {
                name
            };
            if let Err(error) = crate::store::volume_index::validate_name(name) {
                self.error = error.to_string();
                return;
            }
            if let Some(parent) = parent {
                Request::FileCreateIn {
                    session: self.request_session(),
                    volume,
                    parent,
                    name: name.into(),
                    path,
                }
            } else {
                Request::FileCreate {
                    session: self.request_session(),
                    volume,
                    name: name.into(),
                    path,
                }
            }
        };
        self.start_file(request);
    }

    pub(super) fn start_file(&mut self, request: Request) {
        let pending = Pending::for_request(&request);
        if self.submit(request) {
            self.files.pending = pending;
        }
    }

    pub(super) fn entry_deletion_pending(&self) -> bool {
        matches!(self.files.pending, Some(Pending::Delete(_)))
    }

    pub(super) fn catalog_exclusions(&self) -> std::collections::BTreeSet<EntryId> {
        match self.files.pending {
            Some(Pending::Delete(address)) if address.volume == self.volume.id => {
                std::collections::BTreeSet::from([address.entry])
            }
            _ => Default::default(),
        }
    }

    pub(super) fn entry_is_hidden(&self, entry: EntryId) -> bool {
        let excluded = self.catalog_exclusions();
        excluded.contains(&entry)
            || namespace::hidden(&self.catalog_summaries(), &excluded).contains(&entry)
    }

    pub(super) fn receive_entry_action(&mut self, reply: &Reply, cx: &mut Context<Self>) {
        if let Reply::EntryChanged { volume, entry } = reply
            && self.files.pending
                == Some(Pending::Delete(EntryAddress { volume: *volume, entry: *entry }))
            && self.volume.id == *volume
        {
            // Entries and RemoveEntry share the worker's ordered local response lane.
            // Preserve this committed removal until the refreshed catalog arrives.
            let hidden = namespace::hidden(
                &self.catalog_summaries(),
                &std::collections::BTreeSet::from([*entry]),
            );
            for row in self
                .catalog
                .columns
                .iter_mut()
                .flat_map(|column| column.page.iter_mut().flat_map(|page| &mut page.entries))
            {
                row.deleted |= hidden.contains(&row.id);
            }
            self.files.pending = None;
            if self.selected == Some(*entry) {
                self.show_volume(cx);
            }
        }
    }

    pub(super) fn receive_file(&mut self, reply: DiagnosticReply, cx: &mut Context<Self>) -> bool {
        match reply {
            DiagnosticReply::EntryDuplicated { source, entry }
                if source.volume == self.volume.id
                    && self.files.pending == Some(Pending::Duplicate(source)) =>
            {
                self.files.pending = None;
                self.finish_created_entry(entry, cx);
            }
            DiagnosticReply::FileCreated { address, name, entry }
                if address.volume == self.volume.id
                    && address.entry == entry.id
                    && matches!(entry.info.kind, EntryKind::Blob { .. })
                    && matches!(&self.files.pending, Some(Pending::Import { volume, parent:None, name: expected })
                        if *volume == address.volume && expected == &name) =>
            {
                self.files.pending = None;
                self.finish_created_entry(entry, cx);
            }
            DiagnosticReply::FileCreatedIn { parent, address, name, entry }
                if address.volume == self.volume.id
                    && address.entry == entry.id
                    && matches!(entry.info.kind, EntryKind::Blob { .. })
                    && matches!(&self.files.pending, Some(Pending::Import {volume,parent:Some(expected_parent),name:expected_name}) if *volume==address.volume && *expected_parent==parent && expected_name==&name) =>
            {
                self.files.pending = None;
                self.finish_created_entry(entry, cx);
            }
            _ => return false,
        }
        true
    }

    pub(super) fn failed_file(&mut self, pending: Pending, error: &str, cx: &mut Context<Self>) {
        if self.files.pending.as_ref() == Some(&pending) {
            self.files.pending = None;
            self.error = error.into();
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_byte_size;

    #[test]
    fn byte_sizes_use_binary_units_and_promote_before_rounding() {
        // Examples and both sides of unit boundaries preserve the strict >1000 rule.
        for (bytes, expected) in [
            (0, "0 B"),
            (989, "989 B"),
            (1000, "1000 B"),
            (1001, "1 KiB"),
            (1024, "1 KiB"),
            (1280, "1.3 KiB"),
            (3000, "2.9 KiB"),
            (1000 * 1024, "1000 KiB"),
            (1000 * 1024 + 1, "1 MiB"),
            (1024 * 1024, "1 MiB"),
            (5_331_270, "5.1 MiB"),
            (1000 * 1024 * 1024, "1000 MiB"),
            (1000 * 1024 * 1024 + 1, "1 GiB"),
            (3 * 1024 * 1024 * 1024, "3 GiB"),
            (1024u64.pow(4), "1 TiB"),
            (1024u64.pow(5), "1 PiB"),
            (1000 * 1024u64.pow(5), "1000 PiB"),
            (1000 * 1024u64.pow(5) + 1, "1 EiB"),
            (u64::MAX, "16 EiB"),
        ] {
            assert_eq!(format_byte_size(bytes), expected, "{bytes}");
        }
    }
}
