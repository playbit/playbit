//! Entry creation waits for the committed stable ID before offering an inline name.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use gpui::{Focusable, svg};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum PendingCreate {
    Database(EntryKind),
    AppendLog,
}

impl PendingCreate {
    pub(super) fn matches(&self, entry: &crate::store::volume_index::VolumeEntry) -> bool {
        match self {
            Self::Database(kind) => {
                *kind == entry.info.kind && entry.info.protocol != Some(DatabaseProtocol::BlobLog)
            }
            Self::AppendLog => {
                matches!(entry.info.kind, EntryKind::KvDb {})
                    && entry.info.protocol == Some(DatabaseProtocol::BlobLog)
            }
        }
    }
}

// One candidate beyond the complete namespace capacity. A collision rejection
// advances this number; a stale page cannot restart an unbounded create loop.
const DEFAULT_NAME_LIMIT: usize = crate::store::volume_index::namespace::MAX_ENTRIES + 1;

pub(super) fn default_name_number(name: &str) -> Option<usize> {
    if name == "Untitled" {
        return Some(1);
    }
    let number = name.strip_prefix("Untitled ")?.parse::<usize>().ok()?;
    ((2..=DEFAULT_NAME_LIMIT).contains(&number) && name == format!("Untitled {number}"))
        .then_some(number)
}

pub(super) struct Rename {
    pub entry: EntryId,
    pub editor: Entity<text_editor::TextEditor>,
    focus: bool,
    original: String,
    pub submitted: Option<String>,
}

impl Browser {
    pub(super) fn can_create_entry_in(&self, parent: Option<EntryId>) -> bool {
        let (Some(account), Some(volume)) = (self.signed_in_account, &self.volume_details) else {
            return false;
        };
        let participant = self.shared_session().is_some();
        let can_create = volume.permissions.checked().is_ok_and(|acl| {
            [Permission::Write, Permission::Append].into_iter().any(|permission| {
                acl.permits_in_session(Some(Principal::Human(account)), participant, permission)
            })
        });
        if account != volume.volume.owner && !can_create {
            return false;
        }
        if let Some(parent) = parent {
            if self.catalog_entry(parent).is_some_and(|entry| entry.deleted) {
                return false;
            }
            // Creation uses volume Write/Append and directory Read, not directory Write.
            // Unselected ancestor metadata is still checked by the store on submission.
            if let Some(details) =
                self.details.as_ref().filter(|details| details.entry.id == parent)
                && (details.entry.deleted
                    || !details.retained_readable
                    || !matches!(details.entry.info.kind, EntryKind::Directory {}))
            {
                return false;
            }
        }
        true
    }

    pub(in crate::desktop) fn prepare_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.prepare_entry_navigation(cx) {
            return false;
        }
        if self.files.busy() {
            self.show_hint("Wait for the file operation to finish before leaving this volume.", cx);
            return false;
        }
        if self.renaming.is_some() || self.creating_entry.is_some() {
            self.show_hint(
                "Wait for the entry name to finish saving before leaving this volume.",
                cx,
            );
            return false;
        }
        true
    }

    pub(super) fn prepare_entry_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.retain_local_drafts(cx) {
            return false;
        }
        let draft = self
            .renaming
            .as_ref()
            .filter(|rename| rename.submitted.is_none())
            .map(|rename| (rename.entry, rename.editor.read(cx).text().to_owned()));
        if let Some((entry, name)) = draft {
            self.commit_rename(entry, &name, cx);
            return self.renaming.as_ref().is_none_or(|rename| rename.submitted.is_some());
        }
        self.hint.clear();
        true
    }

    pub(super) fn create_entry_in(
        &mut self,
        parent: Option<EntryId>,
        kind: EntryKind,
        cx: &mut Context<Self>,
    ) {
        if !self.can_create_entry_in(parent)
            || self.creating_entry.is_some()
            || self.files.busy()
            || !self.prepare_navigation(cx)
        {
            return;
        }
        self.submit_creation(parent, PendingCreate::Database(kind), 1, cx);
    }

    pub(super) fn create_append_log_in(&mut self, parent: Option<EntryId>, cx: &mut Context<Self>) {
        if !self.can_create_entry_in(parent)
            || self.creating_entry.is_some()
            || self.files.busy()
            || !self.prepare_navigation(cx)
        {
            return;
        }
        self.submit_creation(parent, PendingCreate::AppendLog, 1, cx);
    }

    fn next_creation_name(&self, parent: Option<EntryId>, first: usize) -> Option<String> {
        let siblings = self
            .catalog
            .columns
            .iter()
            .find(|column| column.parent == parent)
            .and_then(|column| column.page.as_ref())
            .map_or(&[][..], |page| page.entries.as_slice());
        for number in first..=DEFAULT_NAME_LIMIT {
            let name =
                if number == 1 { "Untitled".to_owned() } else { format!("Untitled {number}") };
            // Pending deletion is still occupied. Only committed tombstones
            // release a name; unrelated directories do not occupy this one.
            let occupied = siblings.iter().any(|entry| !entry.deleted && entry.name == name)
                || self.details.as_ref().is_some_and(|details| {
                    !details.entry.deleted
                        && details.entry.info.namespace.parent == parent
                        && details.entry.info.name == name
                });
            if !occupied {
                return Some(name);
            }
        }
        None
    }

    pub(super) fn submit_creation(
        &mut self,
        parent: Option<EntryId>,
        pending: PendingCreate,
        first: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.next_creation_name(parent, first) else {
            self.error =
                "Couldn’t find an available entry name. Rename an existing entry, then try again."
                    .into();
            cx.notify();
            return;
        };
        let request = match (&pending, parent) {
            (PendingCreate::Database(kind), Some(parent)) => Request::CreateDatabaseIn {
                session: self.request_session(),
                volume: self.volume.id,
                parent,
                name: name.clone(),
                kind: Box::new(kind.clone()),
            },
            (PendingCreate::Database(kind), None) => Request::CreateDatabase {
                session: self.request_session(),
                volume: self.volume.id,
                name: name.clone(),
                kind: Box::new(kind.clone()),
            },
            (PendingCreate::AppendLog, Some(parent)) => Request::BloblogCreateIn {
                session: self.request_session(),
                volume: self.volume.id,
                parent,
                name: name.clone(),
            },
            (PendingCreate::AppendLog, None) => Request::BloblogCreate {
                session: self.request_session(),
                volume: self.volume.id,
                name: name.clone(),
            },
        };
        if self.submit(request) {
            self.creating_entry = Some(pending);
            self.creating_name = Some(name);
            self.creating_parent = parent;
        }
        cx.notify();
    }

    pub(super) fn finish_created_entry(
        &mut self,
        entry: crate::store::volume_index::VolumeEntry,
        cx: &mut Context<Self>,
    ) {
        if !self.prepare_entry_navigation(cx) || self.renaming.is_some() {
            self.refresh();
            if self.error.is_empty() {
                self.hint = "The entry was created. Finish naming the current entry, \
                    then select the new entry to give it a name."
                    .into();
            }
            cx.notify();
            return;
        }
        self.select(entry.id, cx);
        self.start_rename(entry.id, &entry.info.name, cx);
    }

    pub(super) fn entry_title(&self, cx: &App) -> String {
        if let Some(rename) =
            self.renaming.as_ref().filter(|rename| Some(rename.entry) == self.selected)
        {
            return rename.editor.read(cx).text().to_owned();
        }
        self.details.as_ref().map(|details| details.entry.info.name.clone()).unwrap_or_default()
    }

    pub(super) fn start_rename(&mut self, entry: EntryId, name: &str, cx: &mut Context<Self>) {
        if self.renaming.as_ref().is_some_and(|rename| rename.submitted.is_some()) {
            return;
        }
        let weak = cx.weak_entity();
        let changed_owner = weak.clone();
        let editor = cx.new(|cx| {
            let identity = cx.entity().entity_id();
            let commit: text_editor::CommitCallback = Arc::new(move |name, _, cx| {
                let weak = weak.clone();
                let name = name.to_owned();
                // Text delivery borrows the editor. Retire it after that borrow ends.
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if this
                            .renaming
                            .as_ref()
                            .is_some_and(|rename| rename.editor.entity_id() == identity)
                        {
                            this.commit_rename(entry, &name, cx);
                        }
                    });
                });
                true
            });
            let changed: text_editor::CommitCallback = Arc::new(move |_, _, cx| {
                changed_owner
                    .update(cx, |this, cx| {
                        if this
                            .renaming
                            .as_ref()
                            .is_some_and(|rename| rename.editor.entity_id() == identity)
                        {
                            cx.notify();
                        }
                    })
                    .is_ok()
            });
            text_editor::TextEditor::new(name, cx)
                .literal()
                .inline_name()
                .bound_edits(480)
                .on_change(changed)
                .on_commit(commit.clone())
                .on_blur(commit)
        });
        self.renaming =
            Some(Rename { entry, editor, focus: true, original: name.into(), submitted: None });
        cx.notify();
    }

    pub(super) fn commit_rename(&mut self, entry: EntryId, name: &str, cx: &mut Context<Self>) {
        if !self
            .renaming
            .as_ref()
            .is_some_and(|rename| rename.entry == entry && rename.submitted.is_none())
        {
            return;
        }
        if self.entry_is_hidden(entry)
            || self
                .details
                .as_ref()
                .is_some_and(|details| details.entry.id == entry && details.entry.deleted)
        {
            self.show_hint("This entry was deleted. Cancel its name edit to leave it.", cx);
            return;
        }
        if let Err(error) = crate::store::volume_index::validate_name(name) {
            self.show_hint(&error.to_string(), cx);
            return;
        }
        self.hint.clear();
        if self.renaming.as_ref().is_some_and(|rename| rename.original == name) {
            self.renaming = None;
        } else if self.submit(self.entry_rename_request(entry, name))
            && let Some(rename) = &mut self.renaming
        {
            rename.submitted = Some(name.into());
            rename.editor.update(cx, |editor, _| editor.set_enabled(false));
        }
        cx.notify();
    }

    fn entry_rename_request(&self, entry: EntryId, name: &str) -> Request {
        if name.contains('/') {
            Request::RenameEntry {
                session: self.request_session(),
                volume: self.volume.id,
                entry,
                name: name.into(),
            }
        } else {
            Request::RenameEntryComponent {
                session: self.request_session(),
                volume: self.volume.id,
                entry,
                name: name.into(),
            }
        }
    }

    pub(super) fn rename_from_field(&mut self, entry: EntryId, cx: &mut Context<Self>) {
        if self.selected != Some(entry) || self.files.busy() || self.creating_entry.is_some() {
            return;
        }
        let Some(original) = self
            .details
            .as_ref()
            .filter(|details| {
                details.entry.id == entry && !details.entry.deleted && details.writable
            })
            .map(|details| details.entry.info.name.clone())
        else {
            return;
        };
        let name = self.name.read(cx).text().to_owned();
        if let Some(rename) = self
            .renaming
            .as_ref()
            .filter(|rename| rename.entry == entry && rename.submitted.is_none())
        {
            rename.editor.update(cx, |editor, cx| editor.acknowledge(None, &name, cx));
            self.prepare_entry_navigation(cx);
            return;
        }
        if !self.prepare_navigation(cx) {
            return;
        }
        self.start_rename(entry, &original, cx);
        if let Some(rename) = &self.renaming {
            rename.editor.update(cx, |editor, cx| editor.acknowledge(None, &name, cx));
        }
        self.commit_rename(entry, &name, cx);
    }

    pub(super) fn failed_rename(
        &mut self,
        entry: EntryId,
        name: &str,
        error: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = &mut self.renaming else { return };
        if rename.entry != entry || rename.submitted.as_deref() != Some(name) {
            return;
        }
        rename.submitted = None;
        rename.focus = true;
        rename.editor.update(cx, |editor, _| editor.set_enabled(true));
        self.error = error.into();
        cx.notify();
    }

    pub(super) fn focus_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let available =
            self.renaming.as_ref().is_some_and(|rename| self.catalog_entry(rename.entry).is_some());
        let Some(rename) = &mut self.renaming else { return };
        if rename.focus && available {
            rename.focus = false;
            rename.editor.update(cx, |editor, cx| editor.select_all(cx));
            rename.editor.read(cx).focus_handle(cx).focus(window);
        }
    }

    pub(super) fn rename_row(
        &mut self,
        glyph: &'static str,
        editor: Entity<text_editor::TextEditor>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("entry-inline-name")
            .h(ui(32.))
            .flex()
            .items_center()
            .gap(ui(6.))
            .pl(ui(8.))
            .pr(ui(4.))
            .rounded(ui(4.))
            .bg(rgba(theme().color(Role::MenuSelection).0))
            .child(svg().path(glyph).size(ui(16.)).flex_none().text_color(gpui::white()))
            .child(div().flex_1().min_w_0().child(editor))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && this.renaming.as_ref().is_some_and(|rename| rename.submitted.is_none())
                {
                    this.renaming = None;
                    this.hint.clear();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Tests assert valid browser fixtures.")]
    use super::*;
    use crate::desktop::browser::tests::{details, entry, volume};
    use crate::store::{EntryType, volume_index::EntrySummary};

    #[gpui::test]
    fn nested_directory_component_rename_keeps_its_selection_and_children(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let mut directory = details(2, EntryAcl::Inherit);
            directory.entry.info.kind = EntryKind::Directory {};
            directory.entry.info.protocol = None;
            directory.entry.info.namespace.parent = Some(entry(1).id);
            directory.entry.info.name = "bar".into();
            directory.writable = true;
            browser.selected = Some(directory.entry.id);
            browser.details = Some(Arc::new(directory.clone()));
            browser.catalog.columns[0].after = Some(entry(1).id);
            // The parent is outside this page; naming must not reconstruct its path.
            browser.start_rename(directory.entry.id, "bar", cx);
            while requests.try_recv().is_ok() {}
            browser.commit_rename(directory.entry.id, "bar2", cx);
            assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
                request: Request::RenameEntryComponent { volume: got, entry, name, .. }, ..
            }) if got == volume(1).id && entry == directory.entry.id && name == "bar2"));
            let editor = browser.renaming.as_ref().unwrap().editor.entity_id();
            browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(3).id }, cx);
            assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor);
            browser.receive(
                Reply::EntryChanged { volume: volume(1).id, entry: directory.entry.id },
                cx,
            );
            assert!(browser.renaming.is_none());
            assert_eq!(browser.selected, Some(directory.entry.id));
            assert_eq!(browser.catalog.columns[0].after, Some(entry(1).id));
            directory.entry.info.name = "bar2".into();
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: directory }, cx);
            let rows = [
                (2, "bar2", Some(1), EntryType::Directory),
                (3, "Sample Copy.bin", Some(2), EntryType::Blob),
                (4, "Other.bin", Some(1), EntryType::Blob),
            ]
            .map(|(id, name, parent, kind)| EntrySummary {
                id: entry(id).id,
                parent: parent.map(|id| entry(id).id),
                name: name.into(),
                kind,
                protocol: None,
                deleted: false,
                name_conflict: false,
            });
            let mut entries = vec![EntrySummary {
                id: entry(1).id,
                parent: None,
                name: "foo".into(),
                kind: EntryType::Directory,
                protocol: None,
                deleted: false,
                name_conflict: false,
            }];
            entries.extend(rows);
            browser.fixture_namespace(entries);
            assert_eq!(browser.catalog.columns[1].parent, Some(entry(1).id));
            assert_eq!(browser.catalog.columns[2].parent, Some(entry(2).id));
            assert_eq!(
                browser.catalog.columns[2].page.as_ref().unwrap().entries[0].name,
                "Sample Copy.bin"
            );
            assert_eq!(
                browser.details.as_ref().unwrap().entry.info.namespace.parent,
                Some(entry(1).id)
            );
        });
    }

    #[gpui::test]
    fn nested_blob_component_rename_retains_its_parent_and_correlated_failure_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let mut blob = details(3, EntryAcl::Inherit);
            blob.entry.info.kind = EntryKind::Blob {
                len_bytes: 4,
                digest: [1; 32],
                encrypted_key: [2; 32],
                published_at_micros: 3,
            };
            blob.entry.info.protocol = None;
            blob.entry.info.namespace.parent = Some(entry(2).id);
            blob.entry.info.name = "Sample Copy.bin".into();
            blob.writable = true;
            browser.selected = Some(blob.entry.id);
            browser.details = Some(Arc::new(blob.clone()));
            browser.start_rename(blob.entry.id, "Sample Copy.bin", cx);
            let editor = browser.renaming.as_ref().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.acknowledge(None, "Sample Copy2.bin", cx));
            while requests.try_recv().is_ok() {}
            browser.commit_rename(blob.entry.id, "Sample Copy2.bin", cx);
            let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
            assert!(
                matches!(&queued.request, Request::RenameEntryComponent { volume: got, entry, name, .. }
                if *got == volume(1).id && *entry == blob.entry.id && name == "Sample Copy2.bin")
            );
            browser.failed(
                Failure::for_request(&Request::RenameEntryComponent { session: None,
                    volume: volume(1).id,
                    entry: blob.entry.id,
                    name: "Older name.bin".into(),
                })
                .unwrap(),
                "Stale failure",
                cx,
            );
            assert!(browser.renaming.as_ref().unwrap().submitted.is_some());
            browser.failed(Failure::for_request(&queued.request).unwrap(), "Rename failed", cx);
            assert!(browser.renaming.as_ref().unwrap().submitted.is_none());
            assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor.entity_id());
            assert_eq!(editor.read(cx).text(), "Sample Copy2.bin");
            assert_eq!(browser.details.as_deref(), Some(&blob));
            browser.commit_rename(blob.entry.id, "Sample Copy2.bin", cx);
            browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: blob.entry.id }, cx);
            assert_eq!(browser.selected, Some(blob.entry.id));
            assert_eq!(
                browser.details.as_ref().unwrap().entry.info.namespace.parent,
                Some(entry(2).id)
            );
            assert!(browser.renaming.is_none());
        });
    }

    #[gpui::test]
    fn nested_directory_rename_reuses_its_editor_after_failure(cx: &mut gpui::TestAppContext) {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let mut directory = details(2, EntryAcl::Inherit);
            directory.entry.info.kind = EntryKind::Directory {};
            directory.entry.info.namespace.parent = Some(entry(1).id);
            directory.entry.info.name = "bar".into();
            directory.writable = true;
            browser.selected = Some(directory.entry.id);
            browser.details = Some(Arc::new(directory));
            browser.start_rename(entry(2).id, "bar", cx);
            let editor = browser.renaming.as_ref().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.acknowledge(None, "bar2", cx));
            while requests.try_recv().is_ok() {}
            browser.commit_rename(entry(2).id, "bar2", cx);
            let Message::Request(first) = requests.try_recv().unwrap() else { panic!() };
            assert!(matches!(&first.request, Request::RenameEntryComponent { entry: id, name, .. }
                if *id == entry(2).id && name == "bar2"));
            browser.commit_rename(entry(2).id, "bar2", cx);
            assert!(requests.try_recv().is_err(), "the pending name is not admitted twice");
            browser.failed(Failure::for_request(&first.request).unwrap(), "Rename failed", cx);
            assert_eq!(browser.renaming.as_ref().unwrap().original, "bar");
            assert_eq!(editor.read(cx).text(), "bar2");
            editor.update(cx, |editor, cx| editor.acknowledge(None, "bar3", cx));
            browser.commit_rename(entry(2).id, "bar3", cx);
            let Message::Request(retry) = requests.try_recv().unwrap() else { panic!() };
            assert!(matches!(&retry.request, Request::RenameEntryComponent { entry: id, name, .. }
                if *id == entry(2).id && name == "bar3"));
            assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor.entity_id());
            browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(2).id }, cx);
            assert!(browser.renaming.is_none());
            assert_eq!(browser.selected, Some(entry(2).id));
            assert_eq!(
                browser.details.as_ref().unwrap().entry.info.namespace.parent,
                Some(entry(1).id)
            );
            browser.failed(Failure::for_request(&first.request).unwrap(), "Stale failure", cx);
            assert!(browser.error.is_empty());
        });
    }
}
