#![allow(clippy::unwrap_used, reason = "Tests assert bounded directory reply fixtures.")]
use super::super::tests::{
    creation_authority, details, entry, retained_permissions, start_permission_draft, volume,
};
use super::*;

fn row(byte: u8, name: &str, parent: Option<u8>, directory: bool) -> EntrySummary {
    EntrySummary {
        id: entry(byte).id,
        name: name.into(),
        parent: parent.map(|id| entry(id).id),
        kind: if directory {
            crate::store::EntryType::Directory
        } else {
            crate::store::EntryType::Blob
        },
        protocol: None,
        deleted: false,
        name_conflict: false,
    }
}
fn ancestor(byte: u8, name: &str, parent: Option<u8>) -> DirectoryAncestor {
    DirectoryAncestor {
        id: entry(byte).id,
        name: name.into(),
        parent: parent.map(|id| entry(id).id),
    }
}
fn deliver(
    browser: &mut Browser,
    cut: u64,
    parent: Option<u8>,
    ancestors: Vec<DirectoryAncestor>,
    entries: Vec<EntrySummary>,
    next: Option<u8>,
    cx: &mut Context<Browser>,
) {
    let pending = browser.catalog.pending.clone().unwrap();
    assert!(browser.receive_directory(
        browser.volume.id,
        pending.target,
        pending.cut,
        pending.after,
        DirectoryListing::Page {
            cut,
            parent: parent.map(|id| entry(id).id),
            ancestors,
            entries,
            next: next.map(|id| entry(id).id),
        },
        cx
    ));
}

#[gpui::test]
fn folder_selection_opens_direct_child_columns_and_preserves_ancestor_selections(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        deliver(
            browser,
            1,
            None,
            vec![],
            vec![row(1, "foo", None, true), row(9, "Other", None, false)],
            None,
            cx,
        );
        browser.select(entry(1).id, cx);
        assert_eq!(browser.catalog.columns.len(), 2);
        assert_eq!(browser.catalog.columns[0].selected, Some(entry(1).id));
        assert_eq!(
            browser.catalog.pending.as_ref().unwrap().target,
            DirectoryTarget::Directory(entry(1).id)
        );
        deliver(
            browser,
            1,
            Some(1),
            vec![ancestor(1, "foo", None)],
            vec![row(2, "bar", Some(1), true)],
            None,
            cx,
        );
        browser.select(entry(2).id, cx);
        deliver(
            browser,
            1,
            Some(2),
            vec![ancestor(1, "foo", None), ancestor(2, "bar", Some(1))],
            vec![row(3, "Sample.bin", Some(2), false)],
            None,
            cx,
        );
        browser.select(entry(3).id, cx);
        assert_eq!(
            browser.catalog.columns.iter().map(|column| column.selected).collect::<Vec<_>>(),
            vec![Some(entry(1).id), Some(entry(2).id), Some(entry(3).id)]
        );
        assert_eq!(
            browser.catalog.columns.iter().map(|column| column.parent).collect::<Vec<_>>(),
            vec![None, Some(entry(1).id), Some(entry(2).id)]
        );
        browser.select(entry(9).id, cx);
        assert_eq!(browser.catalog.columns.len(), 1);
        assert_eq!(browser.selected, Some(entry(9).id));
    });
}

#[gpui::test]
fn known_entry_seeks_its_uca_page_and_canonical_ancestors_without_replacing_the_editor(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.fixture_namespace(vec![]);
        browser.selected = Some(entry(9).id);
        browser.start_rename(entry(9).id, "Sample.bin", cx);
        let editor = browser.renaming.as_ref().unwrap().editor.entity_id();
        browser.seek_entry(entry(9).id);
        let path = vec![ancestor(1, "foo", None), ancestor(2, "bar", Some(1))];
        deliver(
            browser,
            7,
            Some(2),
            path.clone(),
            vec![row(3, "Earlier.bin", Some(2), false)],
            Some(3),
            cx,
        );
        let pending = browser.catalog.pending.clone().unwrap();
        assert_eq!(pending.target, DirectoryTarget::Entry(entry(9).id));
        assert_eq!(pending.cut, Some(7));
        assert_eq!(pending.after, Some(entry(3).id));
        deliver(browser, 7, Some(2), path, vec![row(9, "Sample.bin", Some(2), false)], None, cx);
        assert_eq!(browser.catalog.columns.len(), 3);
        assert_eq!(browser.catalog.columns[2].selected, Some(entry(9).id));
        assert_eq!(browser.catalog.columns[2].after, Some(entry(3).id));
        assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor);
        assert_eq!(browser.selected, Some(entry(9).id));
        assert_eq!(browser.catalog.columns[1].parent, Some(entry(1).id));
        assert_eq!(browser.catalog.columns[2].parent, Some(entry(2).id));
    });
}

#[gpui::test]
fn changed_cut_restarts_without_publishing_an_empty_directory_and_stale_replies_are_ignored(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        deliver(browser, 1, None, vec![], vec![row(1, "foo", None, true)], Some(1), cx);
        browser.page_directory(None, false, cx);
        let old = browser.catalog.pending.clone().unwrap();
        assert!(browser.receive_directory(
            browser.volume.id,
            old.target,
            old.cut,
            old.after,
            DirectoryListing::Changed { cut: 2 },
            cx
        ));
        assert_eq!(browser.catalog.pending.as_ref().unwrap().cut, None);
        assert!(!browser.receive_directory(
            browser.volume.id,
            old.target,
            old.cut,
            old.after,
            DirectoryListing::Page {
                cut: 1,
                parent: None,
                ancestors: vec![],
                entries: vec![],
                next: None
            },
            cx
        ));
        deliver(browser, 2, None, vec![], vec![row(2, "New", None, false)], None, cx);
        assert_eq!(browser.catalog.columns[0].page.as_ref().unwrap().entries[0].name, "New");
        assert_eq!(browser.catalog.cut, Some(2));
    });
}

#[gpui::test]
fn entering_a_folder_retains_local_edits_and_opens_its_contents(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        deliver(
            browser,
            1,
            None,
            vec![],
            vec![row(1, "foo", None, true), row(2, "File", None, false)],
            None,
            cx,
        );
        browser.select(entry(2).id, cx);
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) }, cx);
        let active = start_permission_draft(browser);
        while requests.try_recv().is_ok() {}
        browser.select(entry(1).id, cx);
        assert_eq!(browser.selected, Some(entry(1).id));
        assert_eq!(browser.catalog.columns[0].selected, Some(entry(1).id));
        assert_eq!(browser.catalog.columns[1].parent, Some(entry(1).id));
        assert_eq!(retained_permissions(volume(1).id, Some(entry(2).id), cx).active, active);
        assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::DirectoryEntries { target: DirectoryTarget::Directory(id), .. }, .. }) if id == entry(1).id)));
    });
}

#[test]
fn pending_or_committed_parent_deletion_hides_admitted_descendants_and_can_be_reversed() {
    let entries = vec![
        row(1, "foo", None, true),
        row(2, "bar", Some(1), true),
        row(3, "file", Some(2), false),
        row(4, "Other", None, false),
    ];
    let excluded = BTreeSet::from([entry(1).id]);
    assert_eq!(
        hidden(&entries, &excluded),
        BTreeSet::from([entry(1).id, entry(2).id, entry(3).id])
    );
    assert!(hidden(&entries, &BTreeSet::new()).is_empty());
}

#[gpui::test]
fn explicit_paging_adopts_each_requested_page_and_clears_downstream_selection(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        deliver(browser, 1, None, vec![], vec![row(1, "A", None, false)], Some(1), cx);
        browser.select(entry(1).id, cx);
        browser.page_directory(None, false, cx);
        assert!(browser.selected.is_none());
        assert!(!browser.catalog.pending.as_ref().unwrap().seek);
        deliver(browser, 1, None, vec![], vec![row(2, "B", None, false)], Some(2), cx);
        assert_eq!(browser.catalog.columns[0].page.as_ref().unwrap().entries[0].id, entry(2).id);
        assert!(
            browser.catalog.pending.is_none(),
            "Next adopts page two without seeking page three"
        );
        browser.select(entry(2).id, cx);
        browser.page_directory(None, false, cx);
        deliver(browser, 1, None, vec![], vec![row(3, "C", None, false)], None, cx);
        assert_eq!(browser.catalog.columns[0].page.as_ref().unwrap().entries[0].id, entry(3).id);
        browser.select(entry(3).id, cx);
        browser.page_directory(None, true, cx);
        deliver(browser, 1, None, vec![], vec![row(1, "A", None, false)], Some(1), cx);
        assert_eq!(browser.catalog.columns[0].page.as_ref().unwrap().entries[0].id, entry(1).id);
        assert!(
            browser.catalog.pending.is_none(),
            "First adopts page one without restoring page three"
        );
    });
}

#[gpui::test]
fn old_discovery_cannot_reopen_ancestors_after_volume_or_sibling_navigation(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        for select_b in [false, true] {
            browser.selected = Some(entry(3).id);
            browser.fixture_namespace(vec![
                row(1, "foo", None, true),
                row(2, "bar", Some(1), true),
                row(3, "A", Some(2), false),
                row(9, "B", None, false),
            ]);
            browser.refresh_catalog();
            let old = browser.catalog.pending.clone().unwrap();
            if select_b {
                browser.select(entry(9).id, cx);
            } else {
                assert!(browser.show_volume(cx));
            }
            assert!(browser.receive_directory(
                browser.volume.id,
                old.target,
                old.cut,
                old.after,
                DirectoryListing::Page {
                    cut: 2,
                    parent: Some(entry(2).id),
                    ancestors: vec![ancestor(1, "foo", None), ancestor(2, "bar", Some(1))],
                    entries: vec![row(3, "A", Some(2), false)],
                    next: None
                },
                cx
            ));
            assert_eq!(
                browser.catalog.columns.len(),
                1,
                "old A discovery cannot restore A's folder columns"
            );
            assert_eq!(
                browser.catalog.pending.as_ref().unwrap().target,
                if select_b { DirectoryTarget::Entry(entry(9).id) } else { DirectoryTarget::Root }
            );
            assert_eq!(browser.selected, if select_b { Some(entry(9).id) } else { None });
        }
    });
}

#[gpui::test]
fn scoped_creation_and_failure_keep_the_original_directory_identity(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        let account = Id::from_bytes([2; 16]);
        browser.set_account(account, None);
        browser.volume_details.as_mut().unwrap().permissions = Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(account)),
                permissions: vec![Permission::Read, Permission::Append],
            }],
        };
        browser.selected = Some(entry(1).id);
        let mut deleted = row(5, "Untitled 2", Some(1), false);
        deleted.deleted = true;
        browser.fixture_namespace(vec![
            row(1, "before", None, true),
            row(2, "Other folder", None, true),
            row(3, "Untitled", Some(1), false),
            row(4, "Untitled 2", None, false),
            row(6, "Untitled 2", Some(2), false),
            row(7, "untitled 2", Some(1), false),
            deleted,
        ]);
        let mut directory = details(1, EntryAcl::Inherit);
        directory.entry.info.kind = EntryKind::Directory {};
        directory.retained_readable = false;
        browser.details = Some(Arc::new(directory.clone()));
        while requests.try_recv().is_ok() {}
        browser.create_entry_in(Some(entry(1).id), EntryKind::KvDb {}, cx);
        browser.import_selected(
            volume(1).id,
            Some(entry(1).id),
            "readme.txt",
            "/tmp/readme.txt".into(),
        );
        assert!(requests.is_empty(), "a known unreadable destination cannot create children");
        directory.retained_readable = true;
        browser.details = Some(Arc::new(directory));
        // Parent Write is unnecessary: creation uses the volume's Append grant.
        browser.volume_header(true, cx);
        assert!(
            browser.controls["create-entry-menu"].read(cx).popup_for_test().0.iter().all(
                |item| matches!(item, crate::runtime::MenuEntry::Action { disabled: false, .. })
            )
        );
        browser.create_entry_in(Some(entry(1).id), EntryKind::KvDb {}, cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::CreateDatabaseIn { parent, name, .. }, ..
        }) if parent == entry(1).id && name == "Untitled 2"));
        browser.catalog.columns[1].name = "after".into();
        let wrong = Request::CreateDatabaseIn {
            volume: volume(1).id,
            parent: entry(2).id,
            name: "Untitled 2".into(),
            kind: Box::new(EntryKind::KvDb {}),
        };
        browser.failed(Failure::for_request(&wrong).unwrap(), "Other folder failed", cx);
        assert!(browser.creating_entry.is_some());
        let mut committed = entry(9);
        committed.info.name = "Untitled 2".into();
        committed.info.namespace.parent = Some(entry(2).id);
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed.clone() }, cx);
        assert!(browser.creating_entry.is_some());
        committed.info.namespace.parent = Some(entry(1).id);
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed }, cx);
        assert_eq!(browser.selected, Some(entry(9).id));
        assert!(browser.creating_entry.is_none());
    });
}
