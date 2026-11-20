//! Finder columns retain one cut-fenced sibling page per effective directory.
use super::*;
use crate::store::volume_index::{
    DirectoryAncestor, DirectoryListing, DirectoryTarget, EntrySummary,
};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pending {
    pub target: DirectoryTarget,
    pub cut: Option<u64>,
    pub after: Option<EntryId>,
    pub seek: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Column {
    pub parent: Option<EntryId>,
    pub name: String,
    pub selected: Option<EntryId>,
    pub after: Option<EntryId>,
    pub page: Option<EntryPage>,
}
impl Column {
    fn new(parent: Option<EntryId>, name: String) -> Self {
        Self { parent, name, selected: None, after: None, page: None }
    }
    pub fn target(&self) -> DirectoryTarget {
        self.parent.map_or(DirectoryTarget::Root, DirectoryTarget::Directory)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Catalog {
    pub cut: Option<u64>,
    pub columns: Vec<Column>,
    pub pending: Option<Pending>,
    restart: bool,
}
impl Default for Catalog {
    fn default() -> Self {
        Self {
            cut: None,
            columns: vec![Column::new(None, String::new())],
            pending: None,
            restart: false,
        }
    }
}
impl Catalog {
    fn set_path(&mut self, ancestors: Vec<DirectoryAncestor>, selected: Option<EntryId>) {
        let mut old = std::mem::take(&mut self.columns);
        let mut columns = Vec::with_capacity(ancestors.len() + 1);
        for (parent, name) in std::iter::once((None, String::new()))
            .chain(ancestors.iter().map(|entry| (Some(entry.id), entry.name.clone())))
        {
            let column = old
                .iter()
                .position(|column| column.parent == parent)
                .map(|index| old.remove(index))
                .unwrap_or_else(|| Column::new(parent, name.clone()));
            columns.push(Column { name, ..column });
        }
        for index in 0..columns.len() {
            columns[index].selected =
                columns.get(index + 1).and_then(|column| column.parent).or(selected);
        }
        self.columns = columns;
    }
    fn entry(&self, id: EntryId) -> Option<&EntrySummary> {
        self.columns
            .iter()
            .flat_map(|column| column.page.iter().flat_map(|page| &page.entries))
            .find(|entry| entry.id == id)
    }
}

/// Pending deletions hide their admitted descendants while retaining source rows for failure.
pub(super) fn hidden(entries: &[EntrySummary], excluded: &BTreeSet<EntryId>) -> BTreeSet<EntryId> {
    let mut hidden = excluded.clone();
    hidden.extend(entries.iter().filter(|entry| entry.deleted).map(|entry| entry.id));
    for _ in 0..entries.len() {
        let mut changed = false;
        for entry in entries {
            if entry.parent.is_some_and(|parent| hidden.contains(&parent)) {
                changed |= hidden.insert(entry.id);
            }
        }
        if !changed {
            break;
        }
    }
    hidden
}

impl Browser {
    pub(super) fn catalog_entry(&self, id: EntryId) -> Option<&EntrySummary> {
        self.catalog.entry(id)
    }
    pub(super) fn catalog_summaries(&self) -> Vec<EntrySummary> {
        let mut entries: Vec<_> = self
            .catalog
            .columns
            .iter()
            .flat_map(|column| column.page.iter().flat_map(|page| &page.entries))
            .cloned()
            .collect();
        for index in 1..self.catalog.columns.len() {
            let column = &self.catalog.columns[index];
            if let Some(id) = column.parent
                && !entries.iter().any(|entry| entry.id == id)
            {
                entries.push(EntrySummary {
                    id,
                    parent: self.catalog.columns[index - 1].parent,
                    name: column.name.clone(),
                    kind: crate::store::EntryType::Directory,
                    protocol: None,
                    deleted: false,
                    name_conflict: false,
                });
            }
        }
        entries
    }
    fn request_directory(&mut self, pending: Pending) {
        if self.submit(Request::DirectoryEntries {
            volume: self.volume.id,
            target: pending.target,
            cut: pending.cut,
            after: pending.after,
            limit: ENTRY_PAGE_ROWS,
        }) {
            self.catalog.pending = Some(pending);
        }
    }
    pub(super) fn catalog_navigation_changed(&mut self) {
        self.catalog.restart |= self.catalog.pending.is_some();
    }

    pub(super) fn refresh_catalog(&mut self) {
        if self.catalog.pending.is_some() {
            self.catalog.restart = true;
            return;
        }
        self.begin_catalog_refresh();
    }
    fn begin_catalog_refresh(&mut self) {
        self.catalog.restart = false;
        self.catalog.cut = None;
        self.request_directory(Pending {
            target: self.selected.map_or(DirectoryTarget::Root, DirectoryTarget::Entry),
            cut: None,
            after: None,
            seek: true,
        });
    }
    pub(super) fn seek_entry(&mut self, entry: EntryId) {
        if self.catalog.pending.is_some() {
            self.catalog.restart = true;
        } else {
            self.request_directory(Pending {
                target: DirectoryTarget::Entry(entry),
                cut: None,
                after: None,
                seek: true,
            });
        }
    }
    pub(super) fn select_catalog_entry(&mut self, entry: EntryId) {
        let Some((index, summary)) =
            self.catalog.columns.iter().enumerate().find_map(|(index, column)| {
                column
                    .page
                    .as_ref()?
                    .entries
                    .iter()
                    .find(|summary| summary.id == entry)
                    .map(|summary| (index, summary.clone()))
            })
        else {
            self.seek_entry(entry);
            return;
        };
        self.catalog.columns.truncate(index + 1);
        self.catalog.columns[index].selected = Some(entry);
        if summary.kind == crate::store::EntryType::Directory {
            self.catalog.columns.push(Column::new(Some(entry), summary.name));
        }
        self.fill_directory_columns();
    }
    fn fill_directory_columns(&mut self) {
        if self.catalog.pending.is_some() {
            return;
        }
        let Some(column) = self.catalog.columns.iter().find(|column| column.page.is_none()) else {
            return;
        };
        self.request_directory(Pending {
            target: column.target(),
            cut: self.catalog.cut,
            after: column.after,
            seek: true,
        });
    }
    pub(super) fn page_directory(
        &mut self,
        parent: Option<EntryId>,
        first: bool,
        cx: &mut Context<Self>,
    ) {
        if self.catalog.pending.is_some() || !self.prepare_navigation(cx) {
            return;
        }
        let Some(index) = self.catalog.columns.iter().position(|column| column.parent == parent)
        else {
            return;
        };
        let after = if first {
            None
        } else {
            let Some(next) = self.catalog.columns[index].page.as_ref().and_then(|page| page.next)
            else {
                return;
            };
            Some(next)
        };
        self.retire_record_selection(cx);
        self.retire_kv_selection(cx);
        self.clear_entry_view(cx);
        self.catalog.columns.truncate(index + 1);
        self.catalog.columns[index].selected = None;
        if let Some(entry) = parent {
            self.selected = Some(entry);
            self.submit(Request::EntryDetails { volume: self.volume.id, entry });
            self.start_detail_loading(cx);
        }
        let column = &mut self.catalog.columns[index];
        column.after = after;
        column.page = None;
        let pending =
            Pending { target: column.target(), cut: self.catalog.cut, after, seek: false };
        self.request_directory(pending);
        cx.notify();
    }
    pub(super) fn receive_directory(
        &mut self,
        volume: Id,
        target: DirectoryTarget,
        cut_requested: Option<u64>,
        after: Option<EntryId>,
        listing: DirectoryListing,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pending) = self.catalog.pending.take() else { return false };
        if volume != self.volume.id
            || pending.target != target
            || pending.cut != cut_requested
            || pending.after != after
        {
            self.catalog.pending = Some(pending);
            return false;
        }
        if self.catalog.restart {
            self.begin_catalog_refresh();
            return true;
        }
        let DirectoryListing::Page { cut, parent, ancestors, entries, next } = listing else {
            self.begin_catalog_refresh();
            return true;
        };
        if entries.len() > ENTRY_PAGE_ROWS as usize
            || ancestors.len() > crate::store::volume_index::namespace::MAX_DEPTH
            || entries.iter().any(|entry| entry.parent != parent)
            || ancestors.last().map(|entry| entry.id) != parent
        {
            self.error = "The directory listing exceeds the browser's bounds.".into();
            return true;
        }
        if cut_requested.is_none() {
            self.catalog.cut = Some(cut);
            for column in &mut self.catalog.columns {
                column.page = None;
                column.after = None;
            }
            self.catalog.set_path(ancestors, self.selected);
        } else if self.catalog.cut != Some(cut) {
            self.begin_catalog_refresh();
            return true;
        }
        let Some(index) = self.catalog.columns.iter().position(|column| column.parent == parent)
        else {
            self.begin_catalog_refresh();
            return true;
        };
        // A known-ID open or retained ancestor selection may lie on a later UCA page.
        let selected = self.catalog.columns[index].selected;
        let seek_selected =
            pending.seek && selected.is_some_and(|id| !entries.iter().any(|entry| entry.id == id));
        if seek_selected && let Some(next) = next {
            self.catalog.columns[index].after = Some(next);
            self.request_directory(Pending {
                target,
                cut: Some(cut),
                after: Some(next),
                seek: true,
            });
            return true;
        }
        let retained = self
            .catalog
            .columns
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, column)| column.page.as_ref().map_or(0, |page| page.entries.len()))
            .sum::<usize>();
        if retained + entries.len() > crate::store::volume_index::namespace::MAX_ENTRIES {
            self.error = "The directory listing exceeds the browser's entry limit.".into();
            return true;
        }
        self.catalog.columns[index].after = after;
        self.catalog.columns[index].page = Some(EntryPage { entries, next });
        self.permission_refresh.catalog = false;
        if self.selected.is_some_and(|entry| self.entry_is_hidden(entry)) {
            if let Some(details) = self.details.as_deref().cloned() {
                self.receive_details(details, cx);
            } else if !self.has_entry_draft(cx) {
                self.retire_record_selection(cx);
                self.retire_kv_selection(cx);
                self.clear_selected_view(cx);
            }
            return true;
        }
        if let Some(entry) = self.selected.and_then(|id| self.catalog_entry(id)).cloned()
            && entry.kind == crate::store::EntryType::Directory
            && self.catalog.columns.last().is_some_and(|column| column.parent != Some(entry.id))
        {
            self.catalog.columns.push(Column::new(Some(entry.id), entry.name));
        }
        self.fill_directory_columns();
        true
    }
    pub(super) fn failed_directory(
        &mut self,
        volume: Id,
        target: DirectoryTarget,
        cut: Option<u64>,
        after: Option<EntryId>,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if volume != self.volume.id
            || !self.catalog.pending.as_ref().is_some_and(|pending| {
                pending.target == target && pending.cut == cut && pending.after == after
            })
        {
            return false;
        }
        self.catalog.pending = None;
        let refreshing = failure::permission_refresh_required(error);
        if refreshing || !self.catalog.restart {
            for column in &mut self.catalog.columns {
                column.page = None;
            }
            self.catalog.cut = None;
        }
        if self.catalog.restart {
            self.begin_catalog_refresh();
        } else if !refreshing {
            self.error = error.into();
        }
        cx.notify();
        true
    }
}

#[cfg(test)]
#[path = "namespace_tests.rs"]
mod tests;

#[cfg(test)]
impl Browser {
    pub(super) fn fixture_receive_root_page(
        &mut self,
        volume: Id,
        after: Option<EntryId>,
        page: EntryPage,
        cx: &mut Context<Self>,
    ) {
        self.catalog.pending =
            Some(Pending { target: DirectoryTarget::Root, cut: None, after, seek: false });
        self.catalog.restart = false;
        self.receive_directory(
            volume,
            DirectoryTarget::Root,
            None,
            after,
            DirectoryListing::Page {
                cut: 1,
                parent: None,
                ancestors: vec![],
                entries: page.entries,
                next: page.next,
            },
            cx,
        );
    }

    pub(super) fn fixture_catalog(&mut self, page: EntryPage) {
        self.fixture_namespace(page.entries);
        if let Some(root) = &mut self.catalog.columns[0].page {
            root.next = page.next;
        }
    }
    pub(super) fn fixture_namespace(&mut self, entries: Vec<EntrySummary>) {
        let mut ancestors = Vec::new();
        let mut parent = self
            .selected
            .and_then(|id| entries.iter().find(|entry| entry.id == id))
            .and_then(|entry| entry.parent);
        let mut visited = BTreeSet::new();
        while let Some(id) = parent {
            if !visited.insert(id)
                || ancestors.len() == crate::store::volume_index::namespace::MAX_DEPTH
            {
                break;
            }
            let Some(entry) = entries.iter().find(|entry| entry.id == id) else { break };
            ancestors.push(DirectoryAncestor {
                id,
                parent: entry.parent,
                name: entry.name.clone(),
            });
            parent = entry.parent;
        }
        ancestors.reverse();
        self.catalog = Catalog::default();
        self.catalog.cut = Some(1);
        self.catalog.set_path(ancestors, self.selected);
        if let Some(entry) =
            self.selected.and_then(|id| entries.iter().find(|entry| entry.id == id))
            && entry.kind == crate::store::EntryType::Directory
        {
            self.catalog.columns.push(Column::new(Some(entry.id), entry.name.clone()));
        }
        for column in &mut self.catalog.columns {
            let mut children: Vec<_> =
                entries.iter().filter(|entry| entry.parent == column.parent).cloned().collect();
            children.sort_unstable_by(|a, b| {
                crate::ui::collation::compare(&a.name, &b.name).then_with(|| a.id.cmp(&b.id))
            });
            column.page = Some(EntryPage { entries: children, next: None });
        }
    }
}
