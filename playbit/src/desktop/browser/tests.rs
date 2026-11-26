#![allow(clippy::unwrap_used, reason = "Tests assert valid browser fixtures.")]
use super::*;
use crate::engine::reply::{ObjectPageResult, ObjectResult};
use crate::store::object_contract::{
    Acl, EditStamp, FieldCell, FieldDeclaration, ObjectFields, TypeDeclaration, Value, ValueType,
};
use crate::store::volume_index::{EntryInfo, EntrySummary, VolumeEntry};

pub(super) fn start_permission_draft(browser: &mut Browser) -> EntryAcl {
    let (active, retained) = browser
        .details
        .as_ref()
        .map_or((&browser.permissions, &browser.retained_permissions), |details| {
            (&details.permissions, &details.retained_permissions)
        });
    browser.permissions_base = Some(super::drafts::canonical_permissions(active, retained));
    let active = EntryAcl::Replace(Acl {
        grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Read] }],
    });
    browser.permissions = active.clone();
    browser.update_permission_dirty();
    assert!(browser.dirty);
    active
}

pub(super) fn retained_permissions(
    volume: Id,
    entry: Option<EntryId>,
    cx: &App,
) -> crate::store::browser_drafts::PermissionDraft {
    let id = crate::desktop::drafts::snapshots(cx)
        .into_iter()
        .find(|draft| {
            draft.volume == volume
                && draft.entry == entry
                && matches!(
                    draft.target,
                    crate::store::browser_drafts::BrowserDraftTarget::Permissions
                )
        })
        .expect("navigation retains the permission draft")
        .id;
    let saved = crate::desktop::drafts::get(id, cx).unwrap();
    let crate::store::browser_drafts::BrowserDraftBody::Permissions(state) = &saved.body else {
        panic!()
    };
    state.clone()
}

#[gpui::test]
fn append_log_view_preserves_pending_and_denied_status(cx: &mut gpui::TestAppContext) {
    // Render the real split view with pending and denied backing states; the
    // lower content must explain the state and retain the entry actions menu.
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
    cx.simulate_window_resize(window.into(), gpui::size(px(800.), px(640.)));
    for (ready, readable, message) in [
        (false, true, "Content has not arrived on this device yet."),
        (true, false, "You do not have permission to read this entry."),
    ] {
        window
            .update(cx, |browser, _, cx| {
                creation_authority(browser);
                let mut current = details(1, EntryAcl::Inherit);
                current.entry.info.protocol = Some(DatabaseProtocol::BlobLog);
                current.backing_ready = ready;
                current.readable = readable;
                browser.selected = Some(current.entry.id);
                browser.details = Some(Arc::new(current));
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |browser, window, _| {
                let status = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.value() == Some(message))
                    .expect("append-log availability is explained in the rendered content")
                    .1;
                assert!(status.bounds().unwrap().y0 >= 286.);
                assert!(browser.rendered_controls.contains("entry-menu"));
            })
            .unwrap();
    }
}

#[gpui::test]
fn same_volume_navigation_reveals_the_browser_after_finder_open(cx: &mut gpui::TestAppContext) {
    for database_link in [false, true] {
        let (host, _window, _requests) = crate::desktop::test_support::host(cx);
        let reference =
            crate::store::DatabaseReference { volume: volume(1).id, entry: entry(9).id };
        host.update(cx, |host, cx| {
            host.volumes = vec![volume(1)];
            host.select_volume(volume(1), cx);
            let browser = host.browser.as_ref().unwrap().clone();
            let active = browser.update(cx, |browser, cx| {
                browser.select(entry(1).id, cx);
                browser.receive(
                    Reply::EntryDetails {
                        volume: volume(1).id,
                        details: details(1, EntryAcl::Inherit),
                    },
                    cx,
                );
                start_permission_draft(browser)
            });
            host.open_url_result(Ok("file:///tmp/Missing.pbapp".into()), cx);
            assert!(host.new_session.is_some());
            if database_link {
                host.open_url_result(Ok(reference.to_string()), cx);
                assert!(host.pending_database.is_none());
                assert!(host.error.is_empty());
                assert_eq!(browser.read(cx).selected, Some(reference.entry));
            } else {
                assert!(host.select_volume(volume(1), cx));
                assert!(browser.read(cx).selected.is_none());
            }
            assert_eq!(retained_permissions(volume(1).id, Some(entry(1).id), cx).active, active);
            assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
            assert!(host.new_session.is_none(), "navigation must reveal the browser");
        });
    }
}

#[gpui::test]
fn renaming_another_volume_preserves_the_active_entry_and_its_permission_draft(
    cx: &mut gpui::TestAppContext,
) {
    // Sidebar naming does not navigate away from the active content or discard its draft.
    let (host, _window, requests) = crate::desktop::test_support::host(cx);
    host.update(cx, |host, cx| {
        host.volumes = vec![volume(1), volume(2)];
        assert!(host.select_volume(volume(1), cx));
        let browser = host.browser.as_ref().unwrap().clone();
        while requests.try_recv().is_ok() {}
        browser.update(cx, |browser, cx| {
            creation_authority(browser);
            browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        });
        assert!(matches!(
            requests.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::CreateDatabase { .. }, .. })
        ));
        assert!(!host.rename_volume(volume(2).id, cx));
        assert!(host.volume_rename.is_none());
        assert!(browser.read(cx).error.is_empty());
        assert!(browser.read(cx).hint.contains("Finish naming"));
        assert!(requests.try_recv().is_err(), "blocked naming cannot submit a volume label");
        let mut created = entry(1);
        created.info.name = "Untitled".into();
        let editor = browser.update(cx, |browser, cx| {
            browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: created }, cx);
            let editor = browser.renaming.as_ref().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.acknowledge(None, "Entry draft", cx));
            while requests.try_recv().is_ok() {}
            browser.commit_rename(entry(1).id, "Entry draft", cx);
            editor.entity_id()
        });
        assert!(matches!(
            requests.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::RenameEntryComponent { .. }, .. })
        ));
        assert!(!host.rename_volume(volume(2).id, cx));
        browser.update(cx, |browser, cx| {
            browser.failed_rename(entry(1).id, "Entry draft", "Name could not be saved", cx);
        });
        assert!(!host.rename_volume(volume(2).id, cx));
        assert!(host.volume_rename.is_none());
        browser.update(cx, |browser, cx| {
            let rename = browser.renaming.as_ref().unwrap();
            assert_eq!(rename.editor.entity_id(), editor);
            assert_eq!(rename.editor.read(cx).text(), "Entry draft");
            browser.commit_rename(entry(1).id, "Entry draft", cx);
            browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(1).id }, cx);
            browser.error.clear();
        });
        let draft = EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(Id::from_bytes([3; 16]))),
                permissions: vec![Permission::Read],
            }],
        });
        let previous = details(1, EntryAcl::Inherit);
        browser.update(cx, |browser, _| {
            browser.selected = Some(previous.entry.id);
            browser.details = Some(Arc::new(previous.clone()));
            browser.permissions_base = Some(super::drafts::canonical_permissions(
                &browser.permissions,
                &browser.retained_permissions,
            ));
            browser.permissions = draft.clone();
            browser.update_permission_dirty();
        });
        while requests.try_recv().is_ok() {}
        assert!(host.rename_volume(volume(2).id, cx));
        assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
        assert_eq!(browser.read(cx).volume.id, volume(1).id);
        assert_eq!(browser.read(cx).selected, Some(previous.entry.id));
        assert_eq!(browser.read(cx).details.as_deref(), Some(&previous));
        assert_eq!(browser.read(cx).permissions, draft);
        assert!(browser.read(cx).dirty);
        assert!(requests.try_recv().is_err(), "opening a volume name is a local action");
    });
}

#[gpui::test]
fn importing_a_file_retains_its_browser_until_the_stable_entry_can_be_named(
    cx: &mut gpui::TestAppContext,
) {
    // Navigation cannot strand a new-file completion in a retired browser, even with no draft.
    use crate::engine::reply::DiagnosticReply;
    use crate::store::volume_index::EntryAddress;
    let (host, _window, requests) = crate::desktop::test_support::host(cx);
    host.update(cx, |host, cx| {
        host.volumes = vec![volume(1), volume(2)];
        assert!(host.select_volume(volume(1), cx));
        let browser = host.browser.as_ref().unwrap().clone();
        while requests.try_recv().is_ok() {}
        browser.update(cx, |browser, _| {
            creation_authority(browser);
            browser.import_selected(volume(1).id, None, "sample.txt", "/tmp/sample.txt".into());
        });
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::FileCreate { volume: got, name, .. }, ..
        }) if got == volume(1).id && name == "sample.txt"));
        assert!(!host.rename_volume(volume(2).id, cx));
        assert!(host.volume_rename.is_none());
        assert!(!host.select_volume(volume(2), cx));
        assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
        assert!(browser.read(cx).files.pending.is_some());
        assert!(browser.read(cx).selected.is_none());
        assert!(browser.read(cx).error.is_empty(), "pending-operation guidance stays inline");
        assert!(requests.try_recv().is_err(), "blocked navigation must not create another browser");
        let mut file = entry(4);
        file.info.name = "sample.txt".into();
        file.info.protocol = None;
        file.info.kind = EntryKind::Blob {
            len_bytes: 4,
            digest: [4; 32],
            encrypted_key: [5; 32],
            published_at_micros: 6,
        };
        browser.update(cx, |browser, cx| {
            browser.receive(
                Reply::Diagnostic(DiagnosticReply::FileCreated {
                    address: EntryAddress { volume: volume(1).id, entry: file.id },
                    name: "sample.txt".into(),
                    entry: file,
                }),
                cx,
            );
            assert!(browser.files.pending.is_none());
            assert_eq!(browser.selected, Some(entry(4).id));
            assert_eq!(browser.renaming.as_ref().unwrap().entry, entry(4).id);
            assert_eq!(browser.entry_title(cx), "sample.txt");
        });
        browser.update(cx, |browser, cx| browser.commit_rename(entry(4).id, "sample.txt", cx));
        assert!(host.select_volume(volume(2), cx));
        assert_ne!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
    });
}

#[gpui::test]
fn database_document_intent_waits_for_account_and_preserves_native_drafts(
    cx: &mut gpui::TestAppContext,
) {
    let (host, _window, _requests) = crate::desktop::test_support::host(cx);
    let reference = crate::store::DatabaseReference { volume: volume(1).id, entry: entry(9).id };
    host.update(cx, |host, cx| {
        host.volumes = vec![volume(1)];
        host.enrolled = None;
        host.open_url_result(Ok(reference.to_string()), cx);
        assert_eq!(host.pending_database, Some(reference));
        assert!(host.browser.is_none());
        host.open_url_result(Ok("playbit:database:invalid".into()), cx);
        assert!(host.pending_database.is_none());
        host.enrolled = Some(true);
        host.error.clear();
        host.open_url_result(Ok(reference.to_string()), cx);
        assert!(host.pending_database.is_none());
        host.open_url_result(Ok("playbit:database:invalid".into()), cx);
        assert!(!host.error.is_empty());
        host.open_url_result(Ok(reference.to_string()), cx);
        assert!(host.error.is_empty());
        let browser = host.browser.as_ref().unwrap().clone();
        let active = browser.update(cx, |browser, cx| {
            assert_eq!(browser.volume.id, reference.volume);
            assert_eq!(browser.selected, Some(reference.entry));
            browser.receive(
                Reply::EntryDetails {
                    volume: volume(1).id,
                    details: details(9, EntryAcl::Inherit),
                },
                cx,
            );
            start_permission_draft(browser)
        });
        let other = crate::store::DatabaseReference { entry: entry(10).id, ..reference };
        host.open_url_result(Ok(other.to_string()), cx);
        assert_eq!(host.browser.as_ref().unwrap().read(cx).selected, Some(other.entry));
        assert!(host.error.is_empty());
        assert_eq!(
            retained_permissions(reference.volume, Some(reference.entry), cx).active,
            active
        );
    });
    cx.run_until_parked();
}

#[gpui::test]
fn release_channel_retirement_captures_local_edits_before_releasing_the_worker(
    cx: &mut gpui::TestAppContext,
) {
    // Exercise the switch's actual draft gate without relaunching the test process.
    let (host, window, _requests) = crate::desktop::test_support::host(cx);
    host.update(cx, |host, cx| {
        let browser = cx.new(|cx| Browser::new(volume(1), host.send.clone(), cx));
        host.browser = Some(browser.clone());
        let sender = host.send.clone();
        let active = browser.update(cx, |browser, cx| {
            browser.select(entry(1).id, cx);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
            browser.select_record(Id::from_bytes([4; 16]), cx);
            let database = browser.database_address().unwrap();
            browser.receive(Reply::Object(ObjectResult { database, object: record(4, "saved", 1) }), cx);
            let field = Id::from_bytes([3; 16]);
            let editor = browser.field_editor(field, "saved", cx);
            editor.update(cx, |editor, cx| editor.acknowledge(None, "unsaved value", cx));
            browser.field_changed(field, "unsaved value", 1, cx);
            start_permission_draft(browser)
        });
        let target = if host.channel == crate::release_channel::ReleaseChannel::Public {
            crate::release_channel::ReleaseChannel::Testing
        } else {
            crate::release_channel::ReleaseChannel::Public
        };
        assert!(!host.defer_draft_retirement(crate::desktop::draft_lifecycle::Retirement::Channel(target), cx));
        assert_eq!(retained_permissions(volume(1).id, Some(entry(1).id), cx).active, active);
        let saved = crate::desktop::drafts::snapshots(cx).into_iter().find(|draft|
            matches!(draft.target, crate::store::browser_drafts::BrowserDraftTarget::Record { object, .. } if object == Id::from_bytes([4; 16]))
        ).unwrap();
        let saved = crate::desktop::drafts::get(saved.id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Record(state) = &saved.body else { panic!() };
        assert_eq!(state.fields[0].text, "unsaved value");
        assert!(!host.channel_switching);
        assert!(host.send.same_worker(&sender));
        assert!(host.send.send(Request::RecoveryStatus).is_ok());
        assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
    });
    cx.run_until_parked();
    cx.update(|cx| assert!(cx.windows().contains(&window.into())));
}

#[gpui::test]
fn database_document_seeks_selection_and_rejects_non_database_replies(
    cx: &mut gpui::TestAppContext,
) {
    // Opening selects the target independently of the old page; duplicate replies cannot open a blob.
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let target = entry(9).id;
        browser.open_database(target, cx);
        assert_eq!(browser.selected, Some(target));
        assert!(browser.database_open);
        let details = details(9, EntryAcl::Inherit);
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: details.clone() }, cx);
        assert_eq!(browser.details.as_ref().unwrap().entry.id, target);
        browser.open_database(target, cx);
        let mut blob = details;
        blob.entry.info.kind = EntryKind::Blob {
            len_bytes: 0,
            digest: [0; 32],
            encrypted_key: [0; 32],
            published_at_micros: 0,
        };
        for _ in 0..2 {
            browser
                .receive(Reply::EntryDetails { volume: volume(1).id, details: blob.clone() }, cx);
        }
        assert!(browser.selected.is_none());
        assert!(browser.details.is_none());
        assert!(!browser.error.is_empty());
    });
}

pub(super) fn volume(byte: u8) -> Volume {
    Volume {
        id: Id::from_bytes([byte; 16]),
        name: "Archive".into(),
        owner: Id::from_bytes([1; 16]),
        personal: true,
        share_key_with_service: false,
    }
}

pub(super) fn creation_authority(browser: &mut Browser) {
    browser.set_account(browser.volume.owner, None);
    browser.volume_details = Some(crate::store::VolumeDetails {
        volume: browser.volume.clone(),
        permissions: Acl::default(),
        created_clock: 1,
        name_clock: 1,
    });
}

pub(super) fn entry(byte: u8) -> VolumeEntry {
    VolumeEntry {
        id: EntryId::from_bytes([byte; 15]).unwrap(),
        info: EntryInfo {
            namespace: Default::default(),
            origin: crate::store::volume_index::namespace::Origin {
                installation: crate::store::Id::from_bytes([42; 16]),
                label: None,
            },
            initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
            created_revision: EntryId::from_bytes([7; 15]).unwrap(),
            name: "notes".into(),
            kind: EntryKind::KvDb {},
            protocol: Some(DatabaseProtocol::Kvdb),
        },
        name_revision: EntryId::from_bytes([7; 15]).unwrap(),
        content_revision: EntryId::from_bytes([7; 15]).unwrap(),
        deleted: false,
        name_conflict: false,
    }
}

pub(super) fn summary(entry: &VolumeEntry) -> EntrySummary {
    EntrySummary {
        id: entry.id,
        parent: entry.info.namespace.parent,
        name: entry.info.name.clone(),
        kind: match entry.info.kind {
            EntryKind::Directory {} => crate::store::EntryType::Directory,
            EntryKind::KvDb {} => crate::store::EntryType::KvDb,
            EntryKind::ObjectDb { .. } => crate::store::EntryType::ObjectDb,
            EntryKind::Blob { .. } => crate::store::EntryType::Blob,
        },
        protocol: entry.info.protocol,
        deleted: entry.deleted,
        name_conflict: entry.name_conflict,
    }
}

#[gpui::test]
fn entry_loading_delay_restarts_for_selection_and_cancels_when_metadata_arrives(
    cx: &mut gpui::TestAppContext,
) {
    // The previous selection cannot show a loading hint or wake a completed selection later.
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| browser.select(entry(1).id, cx));
    cx.run_until_parked();
    cx.executor().advance_clock(std::time::Duration::from_millis(199));
    cx.run_until_parked();
    assert!(!browser.read_with(cx, |browser, _| browser.detail_loading.visible));
    browser.update(cx, |browser, cx| browser.select(entry(2).id, cx));
    cx.run_until_parked();
    cx.executor().advance_clock(std::time::Duration::from_millis(1));
    cx.run_until_parked();
    assert!(!browser.read_with(cx, |browser, _| browser.detail_loading.visible));
    browser.update(cx, |browser, cx| {
        let mut ready = details(2, EntryAcl::Inherit);
        ready.entry.info.kind = EntryKind::Directory {};
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: ready }, cx);
        assert!(browser.detail_loading.timer.is_none());
    });
    cx.run_until_parked();
    let (count, _subscription) = notifications(&browser, cx);
    cx.executor().advance_clock(DETAIL_LOADING_DELAY);
    cx.run_until_parked();
    assert_eq!(count.get(), 0, "a completed selection has no delayed wakeup");
    browser.update(cx, |browser, cx| browser.select(entry(3).id, cx));
    cx.run_until_parked();
    cx.executor().advance_clock(DETAIL_LOADING_DELAY);
    cx.run_until_parked();
    browser.update(cx, |browser, cx| {
        assert!(browser.detail_loading.visible);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        assert!(browser.detail_loading.visible, "an old reply cannot finish the current load");
        browser.failed(
            Failure::for_request(&Request::EntryDetails {
                session: None,
                volume: volume(1).id,
                entry: entry(3).id,
            })
            .unwrap(),
            "Entry could not be loaded",
            cx,
        );
        assert!(!browser.detail_loading.visible);
        assert!(browser.detail_loading.timer.is_none());
        let request =
            Request::EntryDetails { session: None, volume: volume(1).id, entry: entry(3).id };
        let waiting = "access denied: entry authority requires authenticated catch-up";
        browser.failed(Failure::for_request(&request).unwrap(), waiting, cx);
        assert!(browser.permission_refresh.entry);
        assert_eq!(browser.error, "Entry could not be loaded", "readiness preserves other errors");
        assert!(!browser.detail_is_loading(), "permission readiness schedules no loading timer");
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        assert!(browser.permission_refresh.entry, "an old entry cannot clear readiness");
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(3, EntryAcl::Inherit) },
            cx,
        );
        assert!(!browser.permission_refresh.entry);
        browser.failed(Failure::for_request(&request).unwrap(), waiting, cx);
        assert!(browser.show_volume(cx));
        assert!(!browser.permission_refresh.entry, "navigation retires the old entry wait");
        assert!(!browser.detail_loading.visible);
        assert!(browser.detail_loading.timer.is_none());
    });
}

#[gpui::test]
fn creating_an_entry_selects_its_committed_id_and_names_that_same_entry(
    cx: &mut gpui::TestAppContext,
) {
    use crate::store::volume_index::{DirectoryListing, DirectoryTarget};
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        let reader = Id::from_bytes([2; 16]);
        browser.set_account(reader, None);
        let acl = |permission| Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(reader)),
                permissions: if permission == Permission::Read {
                    vec![Permission::Read]
                } else {
                    vec![Permission::Read, permission]
                },
            }],
        };
        browser.volume_details.as_mut().unwrap().permissions = acl(Permission::Read);
        // An unsaved permission draft cannot enable a creation action.
        browser.permissions = EntryAcl::Replace(acl(Permission::Write));
        browser.volume_header(true, cx);
        let items = browser.controls["create-entry-menu"].read(cx).popup_for_test().0;
        assert!(
            items.iter().all(|item| matches!(
                item,
                crate::runtime::MenuEntry::Action { disabled: true, .. }
            ))
        );
        while requests.try_recv().is_ok() {}
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        browser.create_append_log_in(None, cx);
        browser.import_file_in(None, cx);
        browser.import_selected(volume(1).id, None, "readme.txt", "/tmp/readme.txt".into());
        assert!(requests.is_empty(), "read-only actions cannot submit mutations");
        assert!(!browser.files.dialog);
        assert!(browser.files.pending.is_none());
        assert!(browser.creating_entry.is_none());
        let mut admitted = browser.volume_details.take().unwrap();
        browser.set_account(browser.volume.owner, None);
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        assert!(requests.is_empty(), "even an owner waits for admitted volume metadata");
        browser.set_account(reader, None);
        admitted.permissions = acl(Permission::Write);
        browser.volume_details = Some(admitted);
        browser.permissions = EntryAcl::Inherit;
        browser.fixture_catalog(EntryPage { entries: vec![], next: None });
        while requests.try_recv().is_ok() {}
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::CreateDatabase { volume: got, name, .. }, ..
        }) if got == volume(1).id && name == "Untitled"));
        assert!(requests.try_recv().is_err());
        let mut committed = entry(9);
        committed.info.name = "Untitled".into();
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed.clone() }, cx);
        assert_eq!(browser.selected, Some(committed.id));
        assert_eq!(browser.renaming.as_ref().unwrap().entry, committed.id);
        let editor = browser.renaming.as_ref().unwrap().editor.entity_id();
        assert!(browser.creating_entry.is_none());
        assert!(browser.receive_directory(
            volume(1).id,
            DirectoryTarget::Entry(committed.id),
            None,
            None,
            DirectoryListing::Page {
                cut: 2,
                parent: None,
                ancestors: vec![],
                entries: vec![summary(&committed)],
                next: None
            },
            cx
        ));
        assert_eq!(browser.catalog_entry(committed.id).unwrap().name, "Untitled");
        assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor);
        while requests.try_recv().is_ok() {}
        browser.commit_rename(committed.id, "Notes", cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::RenameEntryComponent { entry: got, name, .. }, ..
        }) if got == committed.id && name == "Notes"));
        browser.receive(Reply::EntryChanged { volume: volume(2).id, entry: committed.id }, cx);
        assert!(browser.renaming.is_some());
        browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: committed.id }, cx);
        assert!(browser.renaming.is_none());

        let mut occupied = summary(&entry(5));
        occupied.name = "Untitled".into();
        let mut deleted = summary(&entry(6));
        deleted.name = "Untitled 2".into();
        deleted.deleted = true;
        browser.fixture_catalog(EntryPage {
            entries: vec![occupied, deleted],
            next: Some(entry(7).id),
        });
        while requests.try_recv().is_ok() {}
        let deleting =
            Request::RemoveEntry { session: None, volume: volume(1).id, entry: entry(5).id };
        browser.files.pending = files::Pending::for_request(&deleting);
        assert!(browser.entry_is_hidden(entry(5).id));
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        assert!(requests.try_recv().is_err(), "a pending deletion still owns the name");
        browser.failed(Failure::for_request(&deleting).unwrap(), "Deletion failed", cx);
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("create") };
        assert!(
            matches!(&queued.request, Request::CreateDatabase { name, .. } if name == "Untitled 2")
        );
        // An occupied name outside the retained page is a definitive rejected
        // attempt. Advance the same intent; never replay a late response.
        browser.failed(creation_collision(&queued.request), "entry name already exists", cx);
        assert!(browser.error.is_empty());
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::CreateDatabase { name, .. }, ..
        }) if name == "Untitled 3"));
        browser.failed(creation_collision(&queued.request), "entry name already exists", cx);
        assert!(requests.try_recv().is_err());
        let mut next = entry(10);
        next.info.name = "Untitled 2".into();
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: next.clone() }, cx);
        assert_eq!(browser.creating_name.as_deref(), Some("Untitled 3"));
        next.info.name = "Untitled 3".into();
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: next.clone() }, cx);
        assert_eq!(browser.selected, Some(next.id));
        assert_eq!(browser.renaming.as_ref().unwrap().entry, next.id);
        assert_eq!(browser.renaming.as_ref().unwrap().editor.read(cx).text(), "Untitled 3");
    });
}

fn creation_collision(request: &Request) -> Failure {
    let error = crate::engine::ipc::Response::new(
        crate::engine::ipc::codec::encode(
            &Reply::Error { error: "entry name already exists".into() },
            4096,
        )
        .unwrap(),
    )
    .err()
    .expect("complete engine rejection");
    Failure::for_name_collision(request, error.as_ref()).unwrap()
}

#[gpui::test]
fn append_log_creation_matches_its_protocol_and_releases_only_its_own_action(
    cx: &mut gpui::TestAppContext,
) {
    // One pending creation owns the completion, including the BlobLog database protocol.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        let mut occupied = summary(&entry(5));
        occupied.name = "Untitled".into();
        browser.fixture_catalog(EntryPage { entries: vec![occupied], next: None });
        while requests.try_recv().is_ok() {}
        browser.create_append_log_in(None, cx);
        browser.create_append_log_in(None, cx);
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        let request = Request::BloblogCreate {
            session: None,
            volume: volume(1).id,
            name: "Untitled 2".into(),
        };
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::BloblogCreate { volume: got, name, .. }, ..
        }) if got == volume(1).id && name == "Untitled 2"));
        assert!(requests.try_recv().is_err());
        let mut committed = entry(9);
        committed.info.name = "Untitled 2".into();
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed.clone() }, cx);
        assert!(browser.selected.is_none(), "an ordinary KV store is not the requested log");
        browser.failed(
            Failure::for_request(&Request::CreateDatabase {
                session: None,
                volume: volume(1).id,
                name: "Untitled 2".into(),
                kind: Box::new(EntryKind::KvDb {}),
            })
            .unwrap(),
            "Unrelated creation failed",
            cx,
        );
        assert!(browser.creating_entry.is_some());
        // The text alone does not authorize replay after an uncertain exchange.
        browser.failed(Failure::for_request(&request).unwrap(), "entry name already exists", cx);
        assert!(browser.creating_entry.is_none());
        assert!(requests.try_recv().is_err());
        browser.create_append_log_in(None, cx);
        assert!(matches!(
            requests.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::BloblogCreate { .. }, .. })
        ));
        committed.info.protocol = Some(DatabaseProtocol::BlobLog);
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed.clone() }, cx);
        assert_eq!(browser.selected, Some(committed.id));
        assert!(browser.creating_entry.is_none());
        let editor = browser.renaming.as_ref().unwrap().editor.entity_id();
        assert_eq!(browser.renaming.as_ref().unwrap().entry, committed.id);
        while requests.try_recv().is_ok() {}
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed }, cx);
        assert_eq!(browser.renaming.as_ref().unwrap().editor.entity_id(), editor);
        assert!(requests.try_recv().is_err(), "a late completion cannot repeat selection");
    });
}

#[gpui::test]
fn default_creation_retries_stop_at_the_bound_and_after_losing_authority(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        while requests.try_recv().is_ok() {}
        browser.create_append_log_in(None, cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("create log") };
        browser.failed(creation_collision(&queued.request), "entry name already exists", cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::BloblogCreate { name, .. }, ..
        }) if name == "Untitled 2"));
        assert!(browser.error.is_empty());

        let last = format!("Untitled {}", crate::store::volume_index::namespace::MAX_ENTRIES + 1);
        browser.creating_name = Some(last.clone());
        let request = Request::BloblogCreate { session: None, volume: volume(1).id, name: last };
        browser.failed(creation_collision(&request), "entry name already exists", cx);
        assert!(browser.creating_entry.is_none());
        assert!(requests.try_recv().is_err());
        assert!(browser.error.contains("available entry name"));

        browser.create_append_log_in(None, cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("create log") };
        browser.volume_details = None;
        browser.failed(creation_collision(&queued.request), "entry name already exists", cx);
        assert!(browser.creating_entry.is_none());
        assert!(requests.try_recv().is_err());
        assert_eq!(browser.error, "entry name already exists");
    });
}

#[test]
fn creation_collision_requires_an_exact_rejected_reply() {
    let request = Request::CreateDatabase {
        session: None,
        volume: volume(1).id,
        name: "Untitled".into(),
        kind: Box::new(EntryKind::KvDb {}),
    };
    for kind in [std::io::ErrorKind::Other, std::io::ErrorKind::TimedOut] {
        let uncertain = std::io::Error::new(kind, "entry name already exists");
        assert!(Failure::for_name_collision(&request, &uncertain).is_none());
    }
    for message in ["access denied", "volume name already exists", "storage failure"] {
        let error = crate::engine::ipc::Response::new(
            crate::engine::ipc::codec::encode(&Reply::Error { error: message.into() }, 4096)
                .unwrap(),
        )
        .err()
        .unwrap();
        assert!(Failure::for_name_collision(&request, error.as_ref()).is_none());
    }
    assert_eq!(creation::default_name_number("Untitled"), Some(1));
    assert_eq!(creation::default_name_number("Untitled 2"), Some(2));
    for name in ["Untitled 1", "Untitled 02", "Untitled +2", "Untitled 1026", "Elsewhere/Untitled"]
    {
        assert_eq!(creation::default_name_number(name), None);
    }
}

#[gpui::test]
fn inline_entry_text_updates_the_content_heading_without_committing_the_name(
    cx: &mut gpui::TestAppContext,
) {
    // Real editor input invalidates the owning browser while durable metadata stays committed.
    cx.update(text_editor::install_keybindings);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let mut previous = details(1, EntryAcl::Inherit);
        previous.entry.info.kind = EntryKind::Directory {};
        browser.selected = Some(previous.entry.id);
        browser.fixture_catalog(EntryPage { entries: vec![summary(&previous.entry)], next: None });
        browser.details = Some(Arc::new(previous));
        browser.start_rename(entry(1).id, "notes", cx);
        browser
    });
    cx.run_until_parked();
    let browser = window.root(cx).unwrap();
    let (count, _subscription) = notifications(&browser, cx);
    while requests.try_recv().is_ok() {}
    cx.simulate_input(window.into(), "Archive");
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.entry_title(cx), "Archive");
        assert_eq!(browser.details.as_ref().unwrap().entry.info.name, "notes");
        assert!(browser.renaming.as_ref().unwrap().submitted.is_none());
        assert!(requests.try_recv().is_err(), "typing a name does not write to the engine");
    });
    assert!(count.get() > 0, "the content heading must redraw on inline input");
    cx.simulate_keystrokes(window.into(), "enter");
    assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
        request: Request::RenameEntryComponent { entry: got, name, .. }, ..
    }) if got == entry(1).id && name == "Archive"));
}

#[gpui::test]
fn created_entry_completion_retains_prior_edits_and_focuses_the_new_item(
    cx: &mut gpui::TestAppContext,
) {
    // Creation selects its result after retaining edits made while it was pending.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        browser.select(entry(1).id, cx);
        let mut previous = details(1, EntryAcl::Inherit);
        previous.entry.info.kind = EntryKind::Directory {};
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: previous.clone() }, cx);
        browser.fixture_catalog(EntryPage { entries: vec![summary(&previous.entry)], next: None });
        let original_page = browser.catalog.columns[0].page.clone();
        while requests.try_recv().is_ok() {}
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::CreateDatabase { volume: got, name, .. }, ..
        }) if got == volume(1).id && name == "Untitled"));

        let draft = EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(Id::from_bytes([2; 16]))),
                permissions: vec![Permission::Read],
            }],
        });
        browser.permissions = draft.clone();
        browser.update_permission_dirty();
        let mut committed = entry(9);
        committed.info.name = "Untitled".into();
        browser.receive(Reply::VolumeEntry { volume: volume(1).id, entry: committed }, cx);

        assert_eq!(browser.selected, Some(entry(9).id));
        assert!(browser.details.is_none());
        assert_eq!(retained_permissions(volume(1).id, Some(previous.entry.id), cx).active, draft);
        assert_eq!(browser.catalog.columns[0].page, original_page);
        assert!(browser.creating_entry.is_none());
        let rename = browser.renaming.as_ref().unwrap();
        assert_eq!(rename.entry, entry(9).id);
        assert_eq!(rename.editor.read(cx).text(), "Untitled");
        let work: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert!(work.iter().any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::EntryDetails { entry: id, .. }, .. }) if *id == entry(9).id)));
        assert!(work.iter().all(|message| matches!(message, Message::Request(QueuedRequest {
            request: Request::DirectoryEntries { .. } | Request::EntryDetails { .. } | Request::VolumeDetails { .. }, ..
        }))), "a creation receipt cannot repeat its mutation");
    });
}

#[gpui::test]
fn failed_entry_creation_and_rename_release_only_the_matching_action(
    cx: &mut gpui::TestAppContext,
) {
    // A failed rename keeps its text for correction; unrelated failures cannot retire it.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        browser.create_entry_in(None, EntryKind::KvDb {}, cx);
        let create = Request::CreateDatabase {
            session: None,
            volume: volume(1).id,
            name: "Untitled".into(),
            kind: Box::new(EntryKind::KvDb {}),
        };
        browser.failed(Failure::for_request(&create).unwrap(), "Creation failed", cx);
        assert!(browser.creating_entry.is_none());
        browser.start_rename(entry(1).id, "Untitled", cx);
        browser.renaming.as_ref().unwrap().editor.update(cx, |editor, cx| {
            editor.acknowledge(None, "Notes", cx);
        });
        while requests.try_recv().is_ok() {}
        browser.commit_rename(entry(1).id, "Notes", cx);
        let rename = Request::RenameEntryComponent {
            session: None,
            volume: volume(1).id,
            entry: entry(1).id,
            name: "Notes".into(),
        };
        browser.failed(
            Failure::for_request(&Request::RenameEntryComponent {
                session: None,
                volume: volume(2).id,
                entry: entry(1).id,
                name: "Notes".into(),
            })
            .unwrap(),
            "Wrong volume",
            cx,
        );
        assert!(browser.renaming.as_ref().unwrap().submitted.is_some());
        browser.failed(Failure::for_request(&rename).unwrap(), "Rename failed", cx);
        let draft = browser.renaming.as_ref().unwrap();
        assert!(draft.submitted.is_none());
        assert_eq!(draft.editor.read(cx).text(), "Notes");
        assert!(browser.has_unsaved_changes(cx));
        while requests.try_recv().is_ok() {}
        browser.commit_rename(entry(1).id, "", cx);
        assert!(requests.try_recv().is_err());
        assert!(browser.renaming.is_some());
    });
}

#[gpui::test]
fn permission_presets_replace_the_complete_flag_set_and_wait_for_explicit_save(
    cx: &mut gpui::TestAppContext,
) {
    // A legacy combination remains Custom until a deliberate preset replaces every flag.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let activate = browser.update(cx, |browser, cx| {
        let permissions = EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(Id::from_bytes([2; 16]))),
                permissions: vec![Permission::Read, Permission::ManagePermissions],
            }],
        });
        browser.selected = Some(entry(1).id);
        let details = details(1, permissions.clone());
        browser.details = Some(Arc::new(details.clone()));
        browser.permissions = permissions;
        browser.permissions_panel(&details, true, cx);
        while requests.try_recv().is_ok() {}
        let (items, _, activate) =
            browser.controls["grant-permissions-0"].read(cx).popup_for_test();
        assert_eq!(items.len(), 4);
        assert!(items.iter().any(|item| matches!(item, crate::runtime::MenuEntry::Action {
            id: 0, checked: Some(true), disabled: true, label, ..
        } if label == "Custom: Read + Manage permissions")));
        assert!(!browser.dirty);
        activate
    });
    cx.update(|cx| activate(0, cx));
    browser.read_with(cx, |browser, _| assert!(!browser.dirty));
    cx.update(|cx| activate(2, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants[0].permissions, vec![Permission::Read, Permission::Write]);
        assert!(browser.dirty);
        assert!(requests.try_recv().is_err(), "presets require explicit Save");
    });
    cx.update(|cx| activate(3, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(
            acl.grants[0].permissions,
            vec![
                Permission::Read,
                Permission::Write,
                Permission::Append,
                Permission::ManagePermissions
            ]
        );
        assert!(acl.validate().is_ok());
        assert!(requests.try_recv().is_err(), "Full access remains a draft");
    });
}

#[gpui::test]
fn object_database_row_stays_selected_until_choosing_a_type(cx: &mut gpui::TestAppContext) {
    // Re-selecting the parent must leave it available for a subsequent rename click.
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        assert!(browser.overview);
        browser.select_type(Id::from_bytes([2; 16]), cx);
        assert!(!browser.overview);
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        assert!(browser.overview);
        assert_eq!(browser.selected, Some(entry(1).id));
    });
}

#[gpui::test]
fn external_deletion_keeps_record_recovery_when_returning_to_the_volume(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let id = browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        browser.select_record(Id::from_bytes([4; 16]), cx);
        let database = browser.database_address().unwrap();
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "saved", 1) }), cx);
        let field = Id::from_bytes([3; 16]);
        let editor = browser.field_editor(field, "saved", cx);
        editor.update(cx, |editor, cx| editor.acknowledge(None, "draft", cx));
        browser.field_changed(field, "draft", 1, cx);
        assert!(browser.retain_local_drafts(cx));
        browser.records.draft_id.unwrap()
    });
    cx.run_until_parked();
    browser.update(cx, |browser, cx| {
        let mut retired = (**browser.details.as_ref().unwrap()).clone();
        retired.entry.deleted = true;
        browser.fixture_receive_root_page(
            volume(1).id,
            None,
            EntryPage { entries: vec![summary(&retired.entry)], next: None },
            cx,
        );
        assert!(!browser.details.as_ref().unwrap().writable);
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: retired }, cx);
        while requests.try_recv().is_ok() {}
        assert!(browser.show_volume(cx));
        assert!(browser.selected.is_none());
        assert!(browser.records.object.is_none());
        let saved = browser.local_draft_snapshot(id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Record(state) = &saved.body else {
            panic!()
        };
        assert_eq!(saved.volume, volume(1).id);
        assert_eq!(saved.entry, Some(entry(1).id));
        assert_eq!(state.fields[0].text, "draft");
        assert!(
            !browser.restore_record_draft(id, cx),
            "local recovery grants no access to a deleted source"
        );
        assert!(requests.try_recv().is_err(), "returning to the volume does not read deleted data");
    });
}

#[gpui::test]
fn catalog_deletion_disables_a_draft_and_later_read_denial_still_retires_content(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        value_fixture(browser, cx, ValueType::Text, false, Some(vec![Value::Text("saved".into())]));
        let field = Id::from_bytes([3; 16]);
        let editor = browser.field_editor(field, "draft", cx);
        browser.field_changed(field, "draft", 1, cx);
        let mut row = summary(&browser.details.as_ref().unwrap().entry);
        row.deleted = true;
        browser.fixture_receive_root_page(
            volume(1).id,
            None,
            EntryPage { entries: vec![row], next: None },
            cx,
        );
        let current = browser.details.as_ref().unwrap();
        assert!(current.entry.deleted);
        assert!(!current.writable && !current.appendable && !current.manage_permissions);
        assert_eq!(browser.records.drafts[&field].editor.entity_id(), editor.entity_id());
        let mut denied = (**current).clone();
        denied.retained_readable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
        assert!(browser.records.drafts.is_empty());
        assert!(browser.records.object.is_none());
        assert!(browser.details.is_none());
        assert!(browser.selected.is_none());
    });
}

#[gpui::test]
fn a_hidden_name_draft_has_a_rendered_discard_action(cx: &mut gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let mut current = details(1, EntryAcl::Inherit);
        current.entry.info.kind = EntryKind::Directory {};
        current.writable = true;
        browser.selected = Some(current.entry.id);
        browser.fixture_catalog(EntryPage { entries: vec![summary(&current.entry)], next: None });
        browser.details = Some(Arc::new(current));
        browser.start_rename(entry(1).id, "notes", cx);
        browser
            .renaming
            .as_ref()
            .unwrap()
            .editor
            .update(cx, |editor, cx| editor.acknowledge(None, "My pending name", cx));
        browser
    });
    window.update(cx, |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    while requests.try_recv().is_ok() {}
    let original_editor = window
        .update(cx, |browser, _, cx| {
            let editor = browser.renaming.as_ref().unwrap().editor.entity_id();
            let mut retired = summary(&browser.details.as_ref().unwrap().entry);
            retired.deleted = true;
            browser.fixture_receive_root_page(
                volume(1).id,
                None,
                EntryPage { entries: vec![retired], next: None },
                cx,
            );
            browser.commit_rename(entry(1).id, "My pending name", cx);
            editor
        })
        .unwrap();
    cx.run_until_parked();
    let discard = window
        .update(cx, |browser, window, cx| {
            let rename = browser.renaming.as_ref().unwrap();
            assert_eq!(rename.editor.entity_id(), original_editor);
            assert_eq!(rename.editor.read(cx).text(), "My pending name");
            assert!(rename.submitted.is_none());
            assert!(!browser.show_volume(cx));
            let node = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Discard Name"))
                .expect("a hidden name draft retains a visible cancellation route")
                .1;
            assert!(!node.is_disabled());
            let bounds = node.bounds().unwrap();
            gpui::point(
                px(((bounds.x0 + bounds.x1) / 2.) as f32),
                px(((bounds.y0 + bounds.y1) / 2.) as f32),
            )
        })
        .unwrap();
    assert!(requests.try_recv().is_err(), "a deleted identity cannot receive another rename");
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_click(discard, Default::default());
    window
        .update(cx, |browser, _, cx| {
            assert!(browser.renaming.is_none());
            assert!(browser.selected.is_none());
            assert!(!browser.has_unsaved_changes(cx));
        })
        .unwrap();
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
fn entry_rename_reveals_its_identity_without_resetting_selected_content(
    cx: &mut gpui::TestAppContext,
) {
    use crate::store::volume_index::{DirectoryListing, DirectoryTarget};
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let target = entry(1).id;
    let activate = browser.update(cx, |browser, cx| {
        let mut selected = details(1, EntryAcl::Inherit);
        selected.entry.info.kind = EntryKind::Directory {};
        selected.writable = true;
        browser.selected = Some(target);
        browser.details = Some(Arc::new(selected));
        browser.fixture_catalog(EntryPage { entries: vec![summary(&entry(3))], next: None });
        browser.content_header(true, cx);
        while requests.try_recv().is_ok() {}
        browser.controls["entry-menu"].read(cx).popup_for_test().2
    });
    cx.update(|cx| activate(2, cx));
    browser.update(cx, |browser, cx| {
        assert_eq!(browser.selected, Some(target));
        assert_eq!(browser.details.as_ref().unwrap().entry.id, target);
        assert_eq!(browser.renaming.as_ref().unwrap().entry, target);
        let row = summary(&browser.details.as_ref().unwrap().entry);
        assert!(browser.receive_directory(
            volume(1).id,
            DirectoryTarget::Entry(target),
            None,
            None,
            DirectoryListing::Page {
                cut: 2,
                parent: None,
                ancestors: vec![],
                entries: vec![row],
                next: None
            },
            cx
        ));
        assert_eq!(browser.catalog_entry(target).unwrap().id, target);
        while requests.try_recv().is_ok() {}
        browser.commit_rename(target, "Archive", cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::RenameEntryComponent { entry, name, .. }, ..
        }) if entry == target && name == "Archive"));
    });
}

#[gpui::test]
fn permission_menu_keeps_its_entry_and_subject_identity_during_remote_refresh(
    cx: &mut gpui::TestAppContext,
) {
    // An open popup cannot retarget another subject when a refreshed ACL shifts its rows.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let original = Grant {
        subject: Subject::Principal(Principal::Human(Id::from_bytes([2; 16]))),
        permissions: vec![Permission::Read],
    };
    let replacement = Grant {
        subject: Subject::Principal(Principal::Human(Id::from_bytes([3; 16]))),
        permissions: vec![Permission::Read],
    };
    let activate = browser.update(cx, |browser, cx| {
        let acl = EntryAcl::Replace(Acl { grants: vec![original.clone()] });
        let mut selected = details(1, acl);
        selected.entry.info.kind = EntryKind::Directory {};
        browser.selected = Some(selected.entry.id);
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: selected }, cx);
        let details = browser.details.clone().unwrap();
        browser.permissions_panel(&details, true, cx);
        browser.controls["grant-permissions-0"].read(cx).popup_for_test().2
    });
    browser.update(cx, |browser, cx| {
        let mut refreshed =
            details(1, EntryAcl::Replace(Acl { grants: vec![replacement.clone()] }));
        refreshed.entry.info.kind = EntryKind::Directory {};
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: refreshed }, cx);
        while requests.try_recv().is_ok() {}
    });
    cx.update(|cx| activate(3, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants, vec![replacement]);
        assert!(!browser.dirty);
        assert!(requests.try_recv().is_err());
    });
    browser.update(cx, |browser, cx| {
        browser.select(entry(2).id, cx);
        let mut selected = details(2, EntryAcl::Replace(Acl { grants: vec![original.clone()] }));
        selected.entry.info.kind = EntryKind::Directory {};
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: selected }, cx);
        while requests.try_recv().is_ok() {}
    });
    cx.update(|cx| activate(3, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants, vec![original]);
        assert!(!browser.dirty);
        assert!(requests.try_recv().is_err());
    });
}

#[gpui::test]
fn public_permission_presets_reject_full_access_and_recheck_authority(
    cx: &mut gpui::TestAppContext,
) {
    // A disabled Full item and a stale menu callback both enforce the current authorization boundary.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let activate = browser.update(cx, |browser, cx| {
        let permissions = EntryAcl::Replace(Acl {
            grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Read] }],
        });
        browser.selected = Some(entry(1).id);
        let details = details(1, permissions.clone());
        browser.details = Some(Arc::new(details.clone()));
        browser.permissions = permissions;
        browser.permissions_panel(&details, true, cx);
        while requests.try_recv().is_ok() {}
        let (items, _, activate) =
            browser.controls["grant-permissions-0"].read(cx).popup_for_test();
        assert!(items.iter().any(|item| matches!(
            item,
            crate::runtime::MenuEntry::Action { id: 3, disabled: true, .. }
        )));
        activate
    });
    cx.update(|cx| activate(3, cx));
    browser.update(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants[0].permissions, vec![Permission::Read]);
        assert!(!browser.dirty);
        Arc::make_mut(browser.details.as_mut().unwrap()).manage_permissions = false;
    });
    cx.update(|cx| activate(2, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants[0].permissions, vec![Permission::Read]);
        assert!(!browser.dirty);
        assert!(requests.try_recv().is_err());
    });
}

#[gpui::test]
fn permission_header_adds_and_cancels_an_explicit_reader_draft(cx: &mut gpui::TestAppContext) {
    // The header opens a transient account field; Add changes the ACL draft and Cancel does not.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let start = browser.update(cx, |browser, cx| {
        browser.selected = Some(entry(1).id);
        browser.permissions = EntryAcl::Replace(Acl::default());
        browser.details = Some(Arc::new(details(1, browser.permissions.clone())));
        browser.permissions_column(true, cx);
        assert!(!browser.adding_grant);
        while requests.try_recv().is_ok() {}
        browser.controls["start-grant"].read(cx).popup_for_test().2
    });
    cx.update(|cx| start(0, cx));
    let (add, cancel) = browser.update(cx, |browser, cx| {
        assert!(browser.adding_grant);
        browser.account.update(cx, |editor, cx| {
            editor.acknowledge(None, &Id::from_bytes([2; 16]).to_string(), cx);
        });
        browser.permissions_column(true, cx);
        (
            browser.controls["add-grant"].read(cx).popup_for_test().2,
            browser.controls["cancel-grant"].read(cx).popup_for_test().2,
        )
    });
    cx.update(|cx| add(0, cx));
    browser.read_with(cx, |browser, cx| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(
            acl.grants,
            vec![Grant {
                subject: Subject::Principal(Principal::Human(Id::from_bytes([2; 16]))),
                permissions: vec![Permission::Read],
            }]
        );
        assert!(browser.dirty);
        assert!(!browser.adding_grant);
        assert!(browser.account.read(cx).text().is_empty());
        assert!(requests.try_recv().is_err(), "adding a reader still requires explicit Save");
    });
    cx.update(|cx| start(0, cx));
    browser.update(cx, |browser, cx| {
        browser.account.update(cx, |editor, cx| {
            editor.acknowledge(None, &Id::from_bytes([3; 16]).to_string(), cx);
        });
    });
    cx.update(|cx| cancel(0, cx));
    browser.read_with(cx, |browser, cx| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom ACL") };
        assert_eq!(acl.grants.len(), 1);
        assert!(!browser.adding_grant);
        assert!(browser.account.read(cx).text().is_empty());
    });
    cx.update(|cx| start(0, cx));
    let override_permissions = browser.update(cx, |browser, cx| {
        browser.account.update(cx, |editor, cx| editor.acknowledge(None, "Unsubmitted", cx));
        browser.permissions_column(true, cx);
        browser.controls["custom-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| override_permissions(0, cx));
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.permissions, EntryAcl::Inherit);
        assert!(!browser.adding_grant);
        assert!(browser.account.read(cx).text().is_empty());
    });
}

#[gpui::test]
fn volume_overview_preserves_catalog_navigation_and_retains_unsaved_entry_changes(
    cx: &mut gpui::TestAppContext,
) {
    // Returning to the overview is local navigation; drafts and pending names keep their owner.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let previous = Arc::new(details(1, EntryAcl::Inherit));
        browser.selected = Some(entry(1).id);
        browser.details = Some(previous.clone());
        browser.fixture_catalog(EntryPage { entries: vec![summary(&entry(1))], next: None });
        browser.catalog.columns[0].after = Some(entry(2).id);
        browser.adding_grant = true;
        let page = browser.catalog.columns[0].page.clone();
        while requests.try_recv().is_ok() {}
        assert!(browser.show_volume(cx));
        assert!(browser.selected.is_none());
        assert!(browser.details.is_none());
        assert!(browser.overview);
        assert!(!browser.adding_grant);
        assert_eq!(browser.catalog.columns[0].after, Some(entry(2).id));
        assert_eq!(browser.catalog.columns[0].page, page);
        assert!(requests.try_recv().is_err(), "the observed catalog does not need another fetch");

        browser.selected = Some(entry(1).id);
        browser.details = Some(previous.clone());
        let active = start_permission_draft(browser);
        assert!(browser.show_volume(cx));
        assert!(browser.selected.is_none());
        assert_eq!(retained_permissions(volume(1).id, Some(entry(1).id), cx).active, active);
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: (*previous).clone() },
            cx,
        );
        assert_eq!(browser.permissions, active);
        assert!(browser.dirty);
        browser.start_rename(entry(1).id, "notes", cx);
        browser.commit_rename(entry(1).id, "Renamed", cx);
        assert!(!browser.show_volume(cx));
        assert_eq!(browser.selected, Some(entry(1).id));
        assert_eq!(browser.renaming.as_ref().unwrap().submitted.as_deref(), Some("Renamed"));
    });
}

#[gpui::test]
fn permission_drafts_can_be_discarded_after_management_is_revoked(cx: &mut gpui::TestAppContext) {
    // Current authority disables Save while local retention and Discard remain available.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let discard = browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        let active = start_permission_draft(browser);
        browser.adding_grant = true;
        browser.account.update(cx, |editor, cx| editor.acknowledge(None, "Unsubmitted", cx));
        let mut current = details(1, EntryAcl::Inherit);
        current.manage_permissions = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current.clone() }, cx);
        assert!(browser.prepare_navigation(cx));
        assert_eq!(retained_permissions(volume(1).id, Some(entry(1).id), cx).active, active);
        browser.permissions_panel(&current, true, cx);
        while requests.try_recv().is_ok() {}
        browser.controls["discard-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| discard(0, cx));
    browser.update(cx, |browser, cx| {
        assert_eq!(browser.permissions, EntryAcl::Inherit);
        assert!(!browser.dirty);
        assert!(!browser.changed_during_edit);
        assert!(!browser.adding_grant);
        assert!(browser.account.read(cx).text().is_empty());
        assert!(browser.prepare_navigation(cx));
        assert!(requests.try_recv().is_err(), "discard is a local action");
    });
}

#[gpui::test]
fn leaving_a_volume_commits_the_inline_name_and_retains_its_owner_until_saved(
    cx: &mut gpui::TestAppContext,
) {
    // A sidebar transition must not drop the editor before its durable completion or failure.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.selected = Some(entry(1).id);
        browser.start_rename(entry(1).id, "Untitled", cx);
        browser.renaming.as_ref().unwrap().editor.update(cx, |editor, cx| {
            editor.acknowledge(None, "Notes", cx);
        });
        while requests.try_recv().is_ok() {}
        assert!(!browser.prepare_navigation(cx));
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::RenameEntryComponent { entry: id, name, .. }, ..
        }) if id == entry(1).id && name == "Notes"));
        assert!(!browser.prepare_navigation(cx));
        assert!(requests.try_recv().is_err(), "a pending name is not submitted twice");
        browser.select(entry(2).id, cx);
        assert_eq!(browser.selected, Some(entry(2).id));
        assert!(browser.renaming.is_some(), "the same browser owns the pending name");
        browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(1).id }, cx);
        assert!(browser.prepare_navigation(cx));
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
            cx,
        );
        let active = start_permission_draft(browser);
        assert!(browser.prepare_navigation(cx));
        assert_eq!(retained_permissions(volume(1).id, Some(entry(2).id), cx).active, active);
    });
}

pub(super) fn notifications<T: 'static>(
    entity: &Entity<T>,
    cx: &mut gpui::TestAppContext,
) -> (Rc<std::cell::Cell<usize>>, gpui::Subscription) {
    let count = Rc::new(std::cell::Cell::new(0));
    let observed = count.clone();
    let subscription =
        cx.update(|cx| cx.observe(entity, move |_, _| observed.set(observed.get() + 1)));
    (count, subscription)
}

#[gpui::test]
fn identical_details_keep_the_scene_and_authority_changes_still_notify(
    cx: &mut gpui::TestAppContext,
) {
    // All effective-authority and readiness fields participate in the metadata comparison.
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| browser.select(entry(1).id, cx));
    cx.run_until_parked();
    let (count, _subscription) = notifications(&browser, cx);
    let mut current = details(1, EntryAcl::Inherit);
    for expected in [1, 1] {
        browser.update(cx, |browser, cx| {
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: current.clone() },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(count.get(), expected);
    }
    for change in 0..5 {
        match change {
            0 => current.entry.info.name = "renamed".into(),
            1 => current.permissions = EntryAcl::Replace(Acl::default()),
            2 => current.readable = false,
            3 => current.backing_ready = false,
            4 => current.entry.deleted = true,
            _ => unreachable!(),
        }
        browser.update(cx, |browser, cx| {
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: current.clone() },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(count.get(), change + 2);
    }
    browser.read_with(cx, |browser, _| {
        assert!(browser.selected.is_none(), "deleted clean selections return to the volume");
        assert!(browser.details.is_none());
    });
}

pub(super) fn details(byte: u8, permissions: EntryAcl) -> EntryDetails {
    EntryDetails {
        file_availability: Default::default(),
        schema_pending: false,
        entry: entry(byte),
        permissions,
        retained_permissions: None,
        created_clock: 0,
        default_permissions: Default::default(),
        backing_ready: true,
        readable: true,
        retained_readable: true,
        recoverable: false,
        writable: false,
        appendable: false,
        manage_permissions: true,
    }
}

pub(super) fn object_details() -> EntryDetails {
    let mut details = details(1, EntryAcl::Inherit);
    details.writable = true;
    details.entry.info.kind = EntryKind::ObjectDb {
        schema: Schema {
            types: BTreeMap::from([(
                Id::from_bytes([2; 16]),
                TypeDeclaration {
                    name: "Note".into(),
                    deprecated: false,
                    fields: BTreeMap::from([(
                        Id::from_bytes([3; 16]),
                        FieldDeclaration {
                            name: "Title".into(),
                            value_type: ValueType::Text,
                            repeated: false,
                            deprecated: false,
                        },
                    )]),
                },
            )]),
        },
    };
    details
}

pub(super) fn record(byte: u8, title: &str, clock: u64) -> ObjectFields {
    ObjectFields {
        object_id: Id::from_bytes([byte; 16]),
        type_id: Id::from_bytes([2; 16]),
        fields: BTreeMap::from([(
            Id::from_bytes([3; 16]),
            FieldCell {
                stamp: EditStamp { clock, device_id: Id::from_bytes([1; 16]) },
                values: Some(vec![Value::Text(title.into())]),
            },
        )]),
    }
}

fn record_receipt(
    browser: &mut Browser,
    queued: QueuedRequest,
    result: Result<Reply, &str>,
    cx: &mut Context<Browser>,
) {
    let failure = Failure::for_request(&queued.request);
    let mut response = crate::desktop::worker_response::WorkerResponse::new(
        result.as_ref().map(|_| ()).map_err(|error| (*error).to_owned()),
    );
    response.credit = queued.credit;
    let error = result.as_ref().err().copied();
    response.browser_reply = result.ok();
    crate::desktop::draft_publications::receive(&response, cx);
    if let Some(reply) = response.browser_reply.take() {
        browser.receive(reply, cx);
    } else {
        browser.failed(failure.unwrap(), error.unwrap(), cx);
    }
}

#[gpui::test]
fn retained_history_checks_selection_cursor_and_current_read_before_display(
    cx: &mut gpui::TestAppContext,
) {
    use crate::store::volume_index::{EntryAddress, HistoryPage};
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        while requests.try_recv().is_ok() {}
        let key_loads = std::cell::RefCell::new(Vec::new());
        let key_load = |data: crate::desktop::worker_data::QueuedData| {
            assert_eq!(data.call.volume, *volume(1).id.as_bytes());
            let crate::runtime::host::DataOperation::KvKeys {
                entry_id,
                after: None,
                root: None,
                limit: 64,
            } = data.call.operation else {
                panic!("unexpected key load")
            };
            key_loads.borrow_mut().push(EntryId::from_bytes(entry_id).unwrap());
        };
        let history_request = || loop {
            match requests.try_recv().unwrap() {
                Message::Request(queued) if matches!(queued.request, Request::EntryRevisions { .. }) => break queued,
                Message::Request(queued) => assert!(matches!(queued.request, Request::EntryDetails { volume: got_volume, entry: got_entry, .. } if got_volume == volume(1).id && [entry(1).id, entry(2).id].contains(&got_entry))),
                Message::Data(data) => key_load(data),
                _ => panic!("unexpected history request companion"),
            }
        };
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        let address = EntryAddress { volume: volume(1).id, entry: entry(1).id };
        let first = history_request();
        let admission = first.credit.as_ref().map(|credit| credit.admission());
        assert!(browser.history_response_matches(admission));
        assert!(matches!(first.request, Request::EntryRevisions { address: got, after: None, limit: 64, .. } if got == address));
        browser.receive(
            Reply::EntryRevisions {
                address,
                after: Some(crate::store::volume_index::HistoryCursor { origin_clock: 1, revision: entry(9).id }),
                page: Box::new(HistoryPage { revisions: vec![], next: None }),
            },
            cx,
        );
        assert!(browser.history_response_matches(admission), "a wrong cursor cannot finish the page");
        browser.receive(
            Reply::EntryRevisions {
                address,
                after: None,
                page: Box::new(HistoryPage { revisions: vec![], next: None }),
            },
            cx,
        );
        assert!(!browser.history_response_matches(admission));
        assert!(!browser.refresh_history(), "the accepted empty page is already loaded");
        let mut denied = details(1, EntryAcl::Replace(Acl::default()));
        denied.retained_readable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
        assert!(!browser.refresh_history(), "revoked retained read cannot request history");
        browser.receive(
            Reply::EntryRevisions {
                address,
                after: None,
                page: Box::new(HistoryPage { revisions: vec![], next: None }),
            },
            cx,
        );
        assert!(!browser.refresh_history());
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        let restored = history_request();
        let restored_admission = restored.credit.as_ref().map(|credit| credit.admission());
        assert!(browser.history_response_matches(restored_admission), "restoring read must reload cleared history");
        assert_ne!(restored_admission, admission);
        browser.select(entry(2).id, cx);
        browser.receive(
            Reply::EntryRevisions {
                address,
                after: None,
                page: Box::new(HistoryPage { revisions: vec![], next: None }),
            },
            cx,
        );
        assert!(!browser.history_response_matches(restored_admission));
        assert!(!browser.refresh_history(), "the newly selected entry still needs its details");
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
            cx,
        );
        assert!(matches!(history_request().request, Request::EntryRevisions { address: got, after: None, .. } if got.entry == entry(2).id));
        while let Ok(message) = requests.try_recv() {
            let Message::Data(data) = message else { panic!("unexpected trailing history request") };
            key_load(data);
        }
        assert_eq!(key_loads.borrow().as_slice(), &[entry(1).id, entry(2).id], "only the two accepted selections load keys");
    });
}

#[gpui::test]
fn history_delivery_uses_the_current_browser_admission(cx: &mut gpui::TestAppContext) {
    use crate::desktop::WorkerResponse;
    use crate::store::volume_index::HistoryPage;
    for failed in [false, true] {
        let (host, _window, requests, responses) =
            crate::desktop::test_support::host_with_responses(cx);
        let open = |host: &mut crate::desktop::Shell, cx: &mut Context<crate::desktop::Shell>| {
            host.select_volume(volume(1), cx);
            let browser = host.browser.as_ref().unwrap().clone();
            browser.update(cx, |browser, cx| {
                browser.open_database(entry(1).id, cx);
                browser.receive(
                    Reply::EntryDetails {
                        volume: volume(1).id,
                        details: details(1, EntryAcl::Inherit),
                    },
                    cx,
                );
            });
            browser
        };
        let history_request = || loop {
            let queued = requests.try_recv().unwrap();
            if let Message::Request(queued) = queued
                && matches!(queued.request, Request::EntryRevisions { .. })
            {
                break queued;
            }
        };
        let old_browser = host.update(cx, open);
        let mut old = history_request();
        let current = host.update(cx, |host, cx| {
            host.browser = None;
            open(host, cx)
        });
        assert_ne!(old_browser, current);
        let mut pending = history_request();
        let admission = pending.credit.as_ref().map(|credit| credit.admission());
        assert_ne!(old.credit.as_ref().map(|credit| credit.admission()), admission);
        assert!(matches!(
            (&old.request, &pending.request),
            (Request::EntryRevisions { address: old, after: old_after, .. },
             Request::EntryRevisions { address: new, after: new_after, .. })
                if old == new && old_after == new_after
        ));
        let page = || {
            let Request::EntryRevisions { address, after, .. } = pending.request else {
                panic!("history request")
            };
            Reply::EntryRevisions {
                address,
                after,
                page: Box::new(HistoryPage { revisions: vec![], next: None }),
            }
        };
        let mut stale = if failed {
            let mut response = WorkerResponse::from_reply(Err("Retired history failure".into()));
            response.browser_failure = Failure::for_request(&old.request);
            response
        } else {
            WorkerResponse::from_reply(Ok(page()))
        };
        stale.credit = old.credit.take();
        responses.try_send(stale).unwrap();
        cx.run_until_parked();
        current.read_with(cx, |browser, _| {
            assert!(
                browser.history_response_matches(admission),
                "retired delivery cannot finish replacement work"
            );
            assert!(browser.error.is_empty());
        });
        host.read_with(cx, |host, _| assert!(host.error.is_empty()));

        let mut completed = WorkerResponse::from_reply(Ok(page()));
        completed.credit = pending.credit.take();
        responses.try_send(completed).unwrap();
        cx.run_until_parked();
        current.update(cx, |browser, _| {
            assert!(!browser.history_response_matches(admission));
            assert!(!browser.refresh_history(), "current delivery loaded the empty page");
            browser.retire_history();
            assert!(browser.refresh_history());
        });
        let mut retired = history_request();
        let retired_admission = retired.credit.as_ref().map(|credit| credit.admission());
        if failed {
            host.update(cx, |host, _| host.browser = None);
            let mut closed =
                WorkerResponse::from_reply(Err("Closed browser history failure".into()));
            closed.browser_failure = Failure::for_request(&retired.request);
            closed.credit = retired.credit.take();
            responses.try_send(closed).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.error.is_empty(), "closed history cannot become a host alert")
            });
        } else {
            let mut lost = WorkerResponse::from_reply(Err("Background process stopped".into()));
            lost.engine_lost = true;
            responses.try_send(lost).unwrap();
            cx.run_until_parked();
            current.read_with(cx, |browser, _| {
                assert!(
                    !browser.history_response_matches(retired_admission),
                    "engine loss retires history admission"
                );
            });
        }
    }
}

// Native file selection submits one background request. Replies and errors
// clear only that action, and a failed import retains the current selection.
#[gpui::test]
fn file_operations_follow_their_full_address_and_release_failed_actions(
    cx: &mut gpui::TestAppContext,
) {
    use crate::engine::reply::DiagnosticReply;
    use crate::store::volume_index::EntryAddress;
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        creation_authority(browser);
        while requests.try_recv().is_ok() {}
        browser.import_selected(
            volume(1).id,
            None,
            "Files/readme.txt",
            "/private/tmp/readme.txt".into(),
        );
        assert!(matches!(requests.try_recv().unwrap(),Message::Request(QueuedRequest {
            request:Request::FileCreate { name,volume:id,.. },..
        }) if name=="Files/readme.txt" && id==volume(1).id));
        let mut file = entry(4);
        file.info.name = "readme.txt".into();
        file.info.kind = EntryKind::Blob {
            len_bytes: 4,
            digest: [4; 32],
            encrypted_key: [5; 32],
            published_at_micros: 6,
        };
        let address = EntryAddress { volume: volume(1).id, entry: file.id };
        let wrong = EntryAddress { volume: volume(2).id, ..address };
        for (target, name) in [(wrong, "Files/readme.txt"), (address, "Other/readme.txt")] {
            browser.receive(
                Reply::Diagnostic(DiagnosticReply::FileCreated {
                    address: target,
                    name: name.into(),
                    entry: file.clone(),
                }),
                cx,
            );
            assert!(browser.selected.is_none());
            assert!(browser.files.pending.is_some());
        }
        browser.receive(
            Reply::Diagnostic(DiagnosticReply::FileCreated {
                address,
                name: "Files/readme.txt".into(),
                entry: file.clone(),
            }),
            cx,
        );
        assert_eq!(browser.selected, Some(address.entry));
        assert!(browser.files.pending.is_none());
        assert_eq!(browser.renaming.as_ref().unwrap().entry, address.entry);
        assert_eq!(browser.renaming.as_ref().unwrap().editor.read(cx).text(), "readme.txt");
        browser.commit_rename(address.entry, "readme.txt", cx);
        let mut file_details = details(4, EntryAcl::Inherit);
        file_details.entry = file.clone();
        browser.receive(Reply::EntryDetails { volume: address.volume, details: file_details }, cx);
        assert_eq!(browser.name.read(cx).text(), "readme.txt");
        browser.import_selected(
            volume(1).id,
            None,
            "replacement.txt",
            "/private/tmp/replace.txt".into(),
        );
        let request = Request::FileCreate { session: None,
            volume: volume(1).id,
            name: "replacement.txt".into(),
            path: "/private/tmp/replace.txt".into(),
        };
        browser.failed(
            Failure::for_request(&Request::FileCreate { session: None,
                volume: wrong.volume,
                name: "replacement.txt".into(),
                path: "/private/tmp/replace.txt".into(),
            })
            .unwrap(),
            "Wrong volume",
            cx,
        );
        assert!(browser.files.pending.is_some());
        browser.failed(Failure::for_request(&request).unwrap(), "Permission changed", cx);
        assert!(browser.files.pending.is_none());
        assert_eq!(browser.selected, Some(address.entry));
        assert_eq!(browser.error, "Permission changed");

        // A selected path can return after the account's creation grant was withdrawn.
        let account = Id::from_bytes([2; 16]);
        browser.set_account(account, None);
        browser.volume_details.as_mut().unwrap().permissions = Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(account)),
                permissions: vec![Permission::Read],
            }],
        };
        while requests.try_recv().is_ok() {}
        browser.import_selected(volume(1).id, None, "denied.txt", "/tmp/denied.txt".into());
        assert!(requests.is_empty());
        assert!(browser.files.pending.is_none());
        creation_authority(browser);
        browser.fixture_catalog(EntryPage { entries: vec![summary(&file)], next: None });
        while requests.try_recv().is_ok() {}
        browser.import_selected(volume(1).id, None, "next.txt", "/private/tmp/next.txt".into());
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::FileCreate { name, .. }, ..
        }) if name == "next.txt"));
        let draft = EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(Id::from_bytes([3; 16]))),
                permissions: vec![Permission::Read],
            }],
        });
        browser.permissions = draft.clone();
        browser.update_permission_dirty();
        let mut next = file;
        next.id = entry(9).id;
        next.info.name = "next.txt".into();
        // A completion cannot claim a different stable ID than its returned entry.
        browser.receive(
            Reply::Diagnostic(DiagnosticReply::FileCreated {
                address,
                name: "next.txt".into(),
                entry: next.clone(),
            }),
            cx,
        );
        assert!(browser.files.pending.is_some());
        let completion = || {
            Reply::Diagnostic(DiagnosticReply::FileCreated {
                address: EntryAddress { entry: next.id, ..address },
                name: "next.txt".into(),
                entry: next.clone(),
            })
        };
        browser.receive(completion(), cx);
        assert!(browser.files.pending.is_none());
        assert_eq!(browser.selected, Some(next.id));
        assert!(browser.details.is_none());
        assert_eq!(retained_permissions(address.volume, Some(address.entry), cx).active, draft);
        let rename = browser.renaming.as_ref().unwrap();
        assert_eq!(rename.entry, next.id);
        assert_eq!(rename.editor.read(cx).text(), "next.txt");
        let work: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert!(work.iter().any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::EntryDetails { entry: id, .. }, .. }) if *id == next.id)));
        assert!(work.iter().all(|message| matches!(message, Message::Request(QueuedRequest {
            request: Request::DirectoryEntries { .. } | Request::EntryDetails { .. } | Request::VolumeDetails { .. }, ..
        }))), "an import receipt cannot repeat its mutation");
        browser.receive(completion(), cx);
        assert!(requests.try_recv().is_err(), "a late import reply cannot repeat navigation");
    });
}

// Queue creation while an older read remains in flight. Only its creation reply
// selects the new record; the copied fields keep stable reference identities.
#[gpui::test]
fn delayed_reads_cannot_acknowledge_record_creation(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        let database = browser.database_address().unwrap();
        while requests.try_recv().is_ok() {}
        browser.create_record(
            BTreeMap::from([(Id::from_bytes([3; 16]), Some(vec![Value::Text("Copy".into())]))]),
            cx,
        );
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::CreateObject { fields, .. }, ..
        }) if fields[&Id::from_bytes([3;16])] == Some(vec![Value::Text("Copy".into())])));
        browser.receive(
            Reply::Object(ObjectResult { database, object: record(4, "Delayed read", 1) }),
            cx,
        );
        assert!(browser.records.creating);
        assert!(browser.records.selected.is_none());
        browser.receive(
            Reply::ObjectCreated(ObjectResult { database, object: record(5, "Copy", 2) }),
            cx,
        );
        assert!(!browser.records.creating);
        assert_eq!(browser.records.selected, Some(Id::from_bytes([5; 16])));
        assert_eq!(browser.records.object.as_ref().unwrap().object_id, Id::from_bytes([5; 16]));
    });
}

// Correlate content to the entire current selection, then prove revocation
// drops previously readable records and prevents another content request.
#[gpui::test]
fn record_pages_and_fields_follow_selection_and_current_read_permission(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        let database = browser.database_address().unwrap();
        let page = |database, type_id, after| {
            Reply::Objects(ObjectPageResult {
                database,
                type_id,
                after,
                page: crate::store::ObjectPage { objects: vec![record(4, "Saved", 1)], next: None },
            })
        };
        browser.receive(page(database, Id::from_bytes([5; 16]), None), cx);
        assert!(browser.records.page.is_none());
        browser.receive(page(database, Id::from_bytes([2; 16]), Some(Id::from_bytes([4; 16]))), cx);
        assert!(browser.records.page.is_none());
        browser.receive(page(database, Id::from_bytes([2; 16]), None), cx);
        assert_eq!(browser.records.page.as_ref().unwrap().objects.len(), 1);
        browser.select_record(Id::from_bytes([4; 16]), cx);
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(5, "Wrong", 1) }), cx);
        assert!(browser.records.object.is_none());
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "Saved", 1) }), cx);
        assert!(browser.records.object.is_some());
        while requests.try_recv().is_ok() {}
        let mut denied = object_details();
        denied.readable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
        assert!(browser.records.page.is_none());
        assert!(browser.records.object.is_none());
        assert!(browser.records.drafts.is_empty());
        assert!(requests.try_recv().is_err());
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "Delayed", 2) }), cx);
        assert!(browser.records.object.is_none());
    });
}

// Replication and a save acknowledgment retain text entered after submission.
// Saving is an explicit patch of one field, never a whole-object replacement.
#[gpui::test]
fn field_drafts_survive_replication_and_inflight_save(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        let database = browser.database_address().unwrap();
        browser.select_record(Id::from_bytes([4; 16]), cx);
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "Saved", 1) }), cx);
        let field = Id::from_bytes([3; 16]);
        let editor = browser.field_editor(field, "Saved", cx);
        editor.update(cx, |editor, cx| {
            editor.clear(cx);
            editor.acknowledge(None, "Draft", cx);
        });
        browser.field_changed(field, "Draft", 1, cx);
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "Remote", 2) }), cx);
        assert!(browser.records.drafts[&field].dirty);
        assert!(browser.records.changed_during_edit);
        while requests.try_recv().is_ok() {}
        browser.save_field(field, Some(vec![Value::Text("Draft".into())]), cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        assert!(matches!(&queued.request, Request::EditObject { fields, .. }
            if fields.len() == 1 && fields.contains_key(&field)));
        editor.update(cx, |editor, cx| editor.acknowledge(None, "Later draft", cx));
        browser.field_changed(field, "Later draft", 2, cx);
        record_receipt(browser, queued,
            Ok(Reply::Object(ObjectResult { database, object: record(4, "Draft", 3) })), cx);
        assert!(browser.records.drafts[&field].dirty);
        assert!(browser.records.drafts[&field].submitted.is_none());
        assert_eq!(browser.records.drafts[&field].revision, 2);
        browser.save_field(field, Some(vec![Value::Text("Later draft".into())]), cx);
        assert!(browser.records.drafts[&field].submitted.is_some());
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        assert!(matches!(&queued.request, Request::EditObject { database: got, object, fields, .. }
            if *got == database && *object == Id::from_bytes([4; 16])
                && *fields == BTreeMap::from([(field, Some(vec![Value::Text("Later draft".into())]))])));
        record_receipt(browser, queued, Err("Permission changed"), cx);
        assert!(browser.records.drafts[&field].submitted.is_none());
        assert!(browser.records.drafts[&field].dirty);
        assert_eq!(browser.error, "Permission changed");
    });
}

// Exercise actual platform text input and editor revisions, then deliver the
// remote and save snapshots while a newer local draft remains unsaved.
#[gpui::test]
fn platform_input_retains_the_newer_draft_after_a_save_reply(cx: &mut gpui::TestAppContext) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let field = Id::from_bytes([3; 16]);
    let (database, editor) = browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser
            .receive(Reply::EntryDetails { volume: volume(1).id, details: object_details() }, cx);
        let database = browser.database_address().unwrap();
        browser.select_record(Id::from_bytes([4; 16]), cx);
        browser
            .receive(Reply::Object(ObjectResult { database, object: record(4, "Saved", 1) }), cx);
        (database, browser.field_editor(field, "Saved", cx))
    });
    let window = cx.add_empty_window();
    window.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            gpui::EntityInputHandler::replace_text_in_range(
                editor,
                Some(0..5),
                "Draft",
                window,
                cx,
            );
        })
    });
    window.update(|_, cx| {
        browser.update(cx, |browser, cx| {
            browser.receive(
                Reply::Object(ObjectResult { database, object: record(4, "Remote", 2) }),
                cx,
            );
            assert_eq!(editor.read(cx).text(), "Draft");
            browser.save_field(field, Some(vec![Value::Text("Draft".into())]), cx);
        })
    });
    window.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            gpui::EntityInputHandler::replace_text_in_range(editor, None, " typed", window, cx);
        })
    });
    window.update(|_, cx| {
        browser.update(cx, |browser, cx| {
            browser.receive(
                Reply::Object(ObjectResult { database, object: record(4, "Draft", 3) }),
                cx,
            );
            assert_eq!(editor.read(cx).text(), "Draft typed");
            assert!(browser.records.drafts[&field].dirty);
            assert!(browser.records.drafts[&field].submitted.is_none());
        })
    });
}

#[gpui::test]
fn account_id_editor_keeps_room_for_input_in_the_permission_row(cx: &mut gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        browser.selected = Some(entry(1).id);
        browser.permissions = EntryAcl::Replace(Default::default());
        browser.adding_grant = true;
        browser.details = Some(Arc::new(details(1, browser.permissions.clone())));
        browser
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            let input = window
                .accessibility_nodes_for_test()
                .map(|(_, node)| node)
                .find(|node| {
                    node.role() == gpui::accesskit::Role::TextInput
                        && node.placeholder() == Some("Account ID")
                })
                .expect("account ID editor must remain accessible");
            assert!(input.bounds().unwrap().width() >= 160.);
            assert!(window.accessibility_error().is_none());
        })
        .unwrap();
}

#[gpui::test]
fn delayed_entry_details_cannot_replace_a_different_selection(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        browser.fixture_catalog(EntryPage {
            entries: vec![summary(&entry(1)), summary(&entry(2))],
            next: None,
        });
        while requests.try_recv().is_ok() {}
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
            cx,
        );
        assert!(browser.details.is_none());
        browser.receive(
            Reply::EntryDetails { volume: volume(2).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        assert!(browser.details.is_none());
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        assert_eq!(browser.details.as_ref().unwrap().entry.id, entry(1).id);
    });
    let mut history = 0;
    let mut keys = 0;
    while let Ok(message) = requests.try_recv() {
        match message {
            Message::Request(QueuedRequest { request: Request::EntryDetails { .. }, .. }) => {}
            Message::Request(QueuedRequest {
                request: Request::EntryRevisions { address, .. },
                ..
            }) => {
                assert_eq!(address.entry, entry(1).id);
                history += 1;
            }
            Message::Data(data) => {
                assert_eq!(data.call.volume, *volume(1).id.as_bytes());
                keys += 1;
            }
            _ => panic!("only accepted details can load current entry content"),
        }
    }
    assert_eq!((history, keys), (1, 1));
}

#[gpui::test]
fn replicated_permissions_and_rename_acknowledgments_preserve_unsaved_permissions(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    while requests.try_recv().is_ok() {}
    browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        let draft = EntryAcl::Replace(Default::default());
        browser.permissions = draft.clone();
        browser.update_permission_dirty();
        browser.receive(
            Reply::EntryDetails {
                volume: volume(1).id,
                details: details(
                    1,
                    EntryAcl::Replace(crate::store::object_contract::Acl {
                        grants: vec![Grant {
                            subject: Subject::Public,
                            permissions: vec![Permission::Read],
                        }],
                    }),
                ),
            },
            cx,
        );
        assert_eq!(browser.permissions, draft);
        assert!(browser.changed_during_edit);
        browser.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(1).id }, cx);
        assert_eq!(browser.permissions, draft);
        assert!(browser.dirty);
        browser.permissions_pending =
            Some((browser.permissions.clone(), browser.retained_permissions.clone()));
        browser.receive(Reply::EntryPermissions { volume: volume(1).id, entry: entry(1).id }, cx);
        assert!(!browser.dirty);
    });
}

pub(super) fn value_fixture(
    browser: &mut Browser,
    cx: &mut Context<Browser>,
    kind: ValueType,
    repeated: bool,
    values: Option<Vec<Value>>,
) {
    browser.select(entry(1).id, cx);
    let mut details = object_details();
    let EntryKind::ObjectDb { schema } = &mut details.entry.info.kind else { panic!() };
    let field = schema
        .types
        .get_mut(&Id::from_bytes([2; 16]))
        .unwrap()
        .fields
        .get_mut(&Id::from_bytes([3; 16]))
        .unwrap();
    field.value_type = kind;
    field.repeated = repeated;
    browser.receive(Reply::EntryDetails { volume: volume(1).id, details }, cx);
    let mut object = record(4, "", 1);
    object.fields.get_mut(&Id::from_bytes([3; 16])).unwrap().values = values;
    browser.select_record(object.object_id, cx);
    browser.receive(
        Reply::Object(ObjectResult { database: browser.database_address().unwrap(), object }),
        cx,
    );
    browser.begin_values(Id::from_bytes([3; 16]), cx);
}

#[gpui::test]
fn file_picker_uses_full_stable_address_and_preserves_draft_after_target_denial(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        value_fixture(browser, cx, ValueType::File, true, Some(vec![]));
        browser.set_volumes(vec![volume(1), volume(2)]);
        browser.start_picker();
        browser.picker_volume(volume(2).id);
        let mut blob = entry(5);
        blob.info.name = "renamed/files/report.txt".into();
        blob.info.kind = EntryKind::Blob {
            len_bytes: 0,
            digest: [0; 32],
            encrypted_key: [1; 32],
            published_at_micros: 7,
        };
        let mut retired = blob.clone();
        retired.id = entry(6).id;
        retired.deleted = true;
        browser.receive(
            Reply::Entries {
                volume: volume(2).id,
                after: None,
                page: crate::store::volume_index::EntryPage {
                    entries: vec![summary(&entry(7)), summary(&retired), summary(&blob)],
                    next: Some(entry(8).id),
                },
            },
            cx,
        );
        while requests.try_recv().is_ok() {}
        browser.picker_entry(blob.id);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::EntryDetails { volume: got, entry: got_entry, .. }, ..
        }) if got == volume(2).id && got_entry == blob.id));
        let mut details = details(5, EntryAcl::Inherit);
        details.entry = blob.clone();
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: details.clone() }, cx);
        assert_eq!(browser.records.values.as_ref().unwrap().values, Some(vec![]));
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details }, cx);
        let expected = Some(vec![Value::File { volume_id: volume(2).id, entry_id: blob.id }]);
        assert_eq!(browser.records.values.as_ref().unwrap().values, expected);
        browser.start_picker();
        browser.picker_volume(volume(2).id);
        let request =
            Request::Entries { session: None, volume: volume(2).id, after: None, limit: 64 };
        browser.failed(Failure::for_request(&request).unwrap(), "target read denied", cx);
        assert_eq!(browser.records.values.as_ref().unwrap().values, expected);
        assert!(browser.records.values.as_ref().unwrap().dirty);
        assert_eq!(browser.error, "target read denied");
    });
}

#[gpui::test]
fn repeated_values_acknowledge_only_submitted_snapshot_and_failed_save_retains_changes(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let field = Id::from_bytes([3;16]);
        value_fixture(browser, cx, ValueType::Text, true, Some(vec![Value::Text("first".into())]));
        browser.remove_value(0, cx);
        browser.records.values.as_ref().unwrap().input.update(cx, |input, cx| input.acknowledge(None, "second", cx));
        browser.input_value(cx);
        while requests.try_recv().is_ok() {}
        browser.save_values(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        assert!(matches!(&queued.request, Request::EditObject { fields, .. } if fields.get(&field) == Some(&Some(vec![Value::Text("second".into())]))));
        record_receipt(browser, queued, Err("write denied"), cx);
        let editor = browser.records.values.as_ref().unwrap();
        assert!(editor.dirty && editor.submitted.is_none());
        assert_eq!(editor.values, Some(vec![Value::Text("second".into())]));
        browser.save_values(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        browser.records.values.as_ref().unwrap().input.update(cx, |input, cx| input.acknowledge(None, "third", cx));
        browser.input_value(cx);
        let mut committed = record(4, "", 2);
        committed.fields.get_mut(&field).unwrap().values = Some(vec![Value::Text("second".into())]);
        let database = browser.database_address().unwrap();
        record_receipt(browser, queued, Ok(Reply::Object(ObjectResult {
            database, object: committed.clone(),
        })), cx);
        let editor = browser.records.values.as_ref().unwrap();
        assert!(editor.dirty && editor.submitted.is_none());
        assert_eq!(editor.values, Some(vec![Value::Text("second".into()), Value::Text("third".into())]));
        committed.fields.get_mut(&field).unwrap().values = editor.values.clone();
        let submitted = editor.values.clone();
        browser.records.values.as_mut().unwrap().submitted = Some(submitted);
        browser.acknowledge_values(&committed);
        assert!(!browser.records.values.as_ref().unwrap().dirty);
    });
}

#[gpui::test]
fn reference_navigation_retains_local_drafts_and_cross_volume_object_identity(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let ty = Id::from_bytes([2;16]);
        let value = Value::Reference { volume_id: volume(2).id, database_id: entry(9).id, object_id: Id::from_bytes([8;16]) };
        value_fixture(browser, cx, ValueType::Reference(ty), false, Some(vec![value.clone()]));
        browser.set_volumes(vec![volume(1), volume(2)]);
        browser.clear_values(cx);
        assert!(browser.retain_local_drafts(cx));
        let id = browser.records.draft_id.unwrap();
        let before = browser.local_draft_snapshot(id, cx).unwrap();
        browser.open_value(&value, ValueType::Reference(ty), cx);
        let saved = browser.local_draft_snapshot(id, cx).unwrap();
        assert_eq!(saved.volume, volume(1).id);
        assert_eq!(saved.entry, Some(entry(1).id));
        assert_eq!(saved.body, before.body);
        let crate::store::browser_drafts::BrowserDraftBody::Record(state) = &saved.body else { panic!() };
        assert_eq!(state.values[0].values, None);
        assert_eq!(state.values[0].base_values, Some(vec![value.clone()]));
        assert!(state.values[0].dirty);
        assert_eq!(browser.volume.id, volume(2).id);
        assert_eq!(browser.selected, Some(entry(9).id));
        let mut details = object_details(); details.entry.id = entry(9).id;
        details.entry.info.name = "another-name".into();
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details }, cx);
        assert_eq!(browser.records.selected, Some(Id::from_bytes([8;16])));
        assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message, Message::Request(QueuedRequest {
            request: Request::ReadObject { database, object, .. }, ..
        }) if database.volume == volume(2).id && database.entry == entry(9).id && object == Id::from_bytes([8;16]))));
    });
}

#[gpui::test]
fn reference_picker_rejects_wrong_page_type_and_withdrawn_target_read(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let ty = Id::from_bytes([2; 16]);
        value_fixture(browser, cx, ValueType::Reference(ty), false, None);
        browser.set_volumes(vec![volume(1), volume(2)]);
        browser.start_picker();
        browser.picker_volume(volume(2).id);
        let mut target = object_details();
        target.entry.id = entry(9).id;
        browser.receive(
            Reply::Entries {
                volume: volume(2).id,
                after: None,
                page: EntryPage {
                    entries: vec![summary(&entry(8)), summary(&target.entry)],
                    next: None,
                },
            },
            cx,
        );
        browser.picker_entry(entry(8).id); // A raw kv database is excluded.
        while requests.try_recv().is_ok() {}
        browser.picker_entry(entry(9).id);
        let mut wrong_type = target.clone();
        wrong_type.entry.info.kind = EntryKind::ObjectDb { schema: Schema::default() };
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: wrong_type }, cx);
        assert!(browser.records.values.as_ref().unwrap().values.is_none());
        assert!(!std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(
            message,
            Message::Request(QueuedRequest { request: Request::Objects { .. }, .. })
        )));
        browser.picker_entry(entry(9).id);
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: target.clone() }, cx);
        let database =
            crate::store::volume_index::EntryAddress { volume: volume(2).id, entry: entry(9).id };
        let page = |after, type_id| {
            Reply::Objects(ObjectPageResult {
                database,
                type_id,
                after,
                page: crate::store::ObjectPage {
                    objects: vec![record(8, "A readable target", 1)],
                    next: None,
                },
            })
        };
        browser.receive(page(Some(Id::from_bytes([7; 16])), ty), cx);
        browser.picker_object(Id::from_bytes([8; 16]), cx);
        assert!(browser.records.values.as_ref().unwrap().values.is_none());
        browser.receive(page(None, Id::from_bytes([99; 16])), cx);
        browser.picker_object(Id::from_bytes([8; 16]), cx);
        assert!(browser.records.values.as_ref().unwrap().values.is_none());
        browser.receive(page(None, ty), cx);
        let mut wrong_type = target.clone();
        wrong_type.entry.info.kind = EntryKind::ObjectDb { schema: Schema::default() };
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: wrong_type }, cx);
        browser.picker_object(Id::from_bytes([8; 16]), cx);
        assert!(browser.records.values.as_ref().unwrap().values.is_none());
        browser.picker_entry(entry(9).id);
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: target.clone() }, cx);
        browser.receive(page(None, ty), cx);
        target.readable = false;
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: target.clone() }, cx);
        browser.picker_object(Id::from_bytes([8; 16]), cx);
        assert!(browser.records.values.as_ref().unwrap().values.is_none());
        assert!(browser.error.contains("permission"));
        target.readable = true;
        browser.receive(Reply::EntryDetails { volume: volume(2).id, details: target }, cx);
        browser.receive(page(None, ty), cx);
        browser.picker_object(Id::from_bytes([8; 16]), cx);
        assert_eq!(
            browser.records.values.as_ref().unwrap().values,
            Some(vec![Value::Reference {
                volume_id: volume(2).id,
                database_id: entry(9).id,
                object_id: Id::from_bytes([8; 16])
            }])
        );
    });
}

#[gpui::test]
fn value_changes_preserve_long_text_and_clearing_file_field_only_edits_the_reference(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let text = "A long title 🌊 ".repeat(8);
        value_fixture(browser, cx, ValueType::Text, true, Some(vec![Value::Text(text.clone())]));
        browser.change_value(0, cx);
        assert_eq!(browser.records.values.as_ref().unwrap().input.read(cx).text(), text);
        browser.input_value(cx);
        assert_eq!(browser.records.values.as_ref().unwrap().values, Some(vec![Value::Text(text)]));
        value_fixture(browser, cx, ValueType::File, false,
            Some(vec![Value::File { volume_id: volume(2).id, entry_id: entry(9).id }]));
        browser.clear_values(cx);
        while requests.try_recv().is_ok() {}
        browser.save_values(cx);
        let Message::Request(QueuedRequest { request, .. }) = requests.try_recv().unwrap() else { panic!() };
        assert!(matches!(request, Request::EditObject { fields, .. } if fields.get(&Id::from_bytes([3;16])) == Some(&None)));
        assert!(requests.try_recv().is_err());
    });
}

#[gpui::test]
fn oversized_value_list_remains_in_original_record_without_a_truncated_draft(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let values = vec![Value::Boolean(true); 129];
        value_fixture(browser, cx, ValueType::Boolean, true, Some(values.clone()));
        assert!(browser.records.values.is_none());
        assert_eq!(
            browser
                .records
                .object
                .as_ref()
                .unwrap()
                .fields
                .get(&Id::from_bytes([3; 16]))
                .unwrap()
                .values,
            Some(values)
        );
        assert!(browser.error.contains("original values are retained"));
        value_fixture(browser, cx, ValueType::Boolean, true, Some(vec![Value::Boolean(true); 128]));
        assert_eq!(browser.records.values.as_ref().unwrap().values.as_ref().unwrap().len(), 128);
    });
}

#[gpui::test]
fn unapplied_native_value_input_survives_reference_navigation(cx: &mut gpui::TestAppContext) {
    let (send, _requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let (input, current) = browser.update(cx, |browser, cx| {
        value_fixture(browser, cx, ValueType::Text, true, None);
        (
            browser.records.values.as_ref().unwrap().input.clone(),
            (**browser.details.as_ref().unwrap()).clone(),
        )
    });
    let window = cx.add_empty_window();
    window.update(|window, cx| {
        input.update(cx, |input, cx| {
            gpui::EntityInputHandler::replace_text_in_range(
                input,
                None,
                "Unapplied draft",
                window,
                cx,
            )
        });
    });
    window.update(|_, cx| {
        browser.update(cx, |browser, cx| {
            assert!(browser.records.values.as_ref().unwrap().input_dirty);
            assert!(browser.has_record_draft());
            let value = Value::File { volume_id: volume(1).id, entry_id: entry(9).id };
            browser.open_value(&value, ValueType::File, cx);
            assert_eq!(browser.selected, Some(entry(9).id));
            assert_eq!(input.read(cx).text(), "Unapplied draft");
            let saved = browser
                .local_draft_summaries(cx)
                .into_iter()
                .find(|draft| {
                    draft.volume == volume(1).id
                        && draft.entry == Some(entry(1).id)
                        && matches!(
                            draft.target,
                            crate::store::browser_drafts::BrowserDraftTarget::Record { .. }
                        )
                })
                .unwrap();
            let saved = browser.local_draft_snapshot(saved.id, cx).unwrap();
            let crate::store::browser_drafts::BrowserDraftBody::Record(state) = &saved.body else {
                panic!()
            };
            assert_eq!(state.values[0].input, "Unapplied draft");
            assert!(state.values[0].input_dirty);
            browser.select(entry(1).id, cx);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
            browser.select_record(Id::from_bytes([4; 16]), cx);
            assert_eq!(
                browser.records.values.as_ref().unwrap().input.read(cx).text(),
                "Unapplied draft"
            );
            browser.input_value(cx);
            assert!(!browser.records.values.as_ref().unwrap().input_dirty);
            assert_eq!(
                browser.records.values.as_ref().unwrap().values,
                Some(vec![Value::Text("Unapplied draft".into())])
            );
        })
    });
}

#[test]
fn record_labels_distinguish_ids_created_in_the_same_second() {
    // Untitled rows retain the complete canonical ID, including zero bytes.
    let mut first = record(1, "", 1);
    let mut second = first.clone();
    first.object_id = Id::from_bytes([1; 16]);
    let mut bytes = *first.object_id.as_bytes();
    bytes[15] = 2;
    second.object_id = Id::from_bytes(bytes);
    assert_eq!(records::record_label(&first), first.object_id.to_string());
    assert_eq!(records::record_label(&second), second.object_id.to_string());
    assert_ne!(records::record_label(&first), records::record_label(&second));
    first.object_id = Id::from_bytes([0; 16]);
    assert_eq!(records::record_label(&first), "1".repeat(16));
}

#[gpui::test]
fn colliding_record_hints_and_titles_still_select_the_complete_object_id(
    cx: &mut gpui::TestAppContext,
) {
    // A shared title and canonical prefix must not change the selected identity.
    let (send, requests, _) = worker_queue::channel();
    let first = record(4, "Same title", 1);
    let mut second = first.clone();
    let mut id = *first.object_id.as_bytes();
    id[15] = 5;
    second.object_id = Id::from_bytes(id);
    let selected = second.object_id;
    assert_eq!(&first.object_id.to_string()[..6], &selected.to_string()[..6]);
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let details = object_details();
        browser.selected = Some(details.entry.id);
        browser.details = Some(Arc::new(details));
        browser.overview = false;
        browser.records.type_id = Some(first.type_id);
        browser.records.page =
            Some(Arc::new(crate::store::ObjectPage { objects: vec![first, second], next: None }));
        browser
    });
    cx.run_until_parked();
    let point = window
        .update(cx, |_, window, _| {
            let mut matches: Vec<_> = window
                .accessibility_nodes_for_test()
                .filter(|(_, node)| {
                    node.role() == gpui::accesskit::Role::Button
                        && node.label() == Some("Same title")
                })
                .map(|(_, node)| node.bounds().unwrap())
                .collect();
            matches.sort_by(|a, b| a.y0.total_cmp(&b.y0));
            assert_eq!(matches.len(), 2);
            gpui::point(px((matches[1].x0 + 16.) as f32), px((matches[1].y0 + 16.) as f32))
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_click(point, Default::default());
    window
        .update(&mut visual, |browser, _, _| assert_eq!(browser.records.selected, Some(selected)))
        .unwrap();
    assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
        Message::Request(QueuedRequest { request: Request::ReadObject { object, .. }, .. }) if object == selected
    )));
}

#[gpui::test]
fn entry_selection_keeps_rows_contiguous_and_retirement_preserves_focus_modality(
    cx: &mut gpui::TestAppContext,
) {
    // Selecting a KV entry must neither move its neighbors nor wait for pointer release.
    let (send, requests, _) = worker_queue::channel();
    let entries: Vec<_> = ["A first", "B middle", "C last"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let mut value = entry(index as u8 + 1);
            value.info.name = name.into();
            value
        })
        .collect();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        creation_authority(&mut browser);
        browser.fixture_catalog(EntryPage {
            entries: entries.iter().map(summary).collect(),
            next: None,
        });
        browser.selected = Some(entries[1].id);
        browser
    });
    window.update(cx, |_, window, _| window.activate_window()).unwrap();
    let rows = |window: &Window| {
        let mut rows: Vec<_> = window
            .accessibility_nodes_for_test()
            .filter(|(_, node)| {
                node.role() == gpui::accesskit::Role::Button
                    && matches!(node.label(), Some("A first" | "B middle" | "C last"))
            })
            .map(|(_, node)| (node.label().unwrap().to_owned(), node.bounds().unwrap()))
            .collect();
        rows.sort_by(|a, b| a.1.y0.total_cmp(&b.1.y0));
        assert_eq!(rows.len(), 3);
        for pair in rows.windows(2) {
            assert_eq!(pair[0].1.y1, pair[1].1.y0, "catalog rows must remain adjacent");
        }
        rows
    };
    for loaded in [false, true] {
        window
            .update(cx, |browser, _, cx| {
                browser.details = loaded.then(|| {
                    let mut info = details(2, EntryAcl::Inherit);
                    info.entry.info.name = "B middle".into();
                    Arc::new(info)
                });
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |_, window, _| rows(window)).unwrap();
    }
    let position = window
        .update(cx, |_, window, _| {
            let bounds = rows(window).into_iter().find(|row| row.0 == "C last").unwrap().1;
            gpui::point(px((bounds.x0 + 60.) as f32), px((bounds.y0 + 16.) as f32))
        })
        .unwrap();
    while requests.try_recv().is_ok() {}
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
    window
        .update(&mut visual, |browser, window, _| {
            assert_eq!(browser.selected, Some(entries[2].id));
            rows(window);
            let selected: Vec<_> = window
                .accessibility_nodes_for_test()
                .filter(|(_, node)| node.is_selected() == Some(true))
                .filter_map(|(_, node)| node.label())
                .collect();
            assert_eq!(selected, vec!["C last"]);
        })
        .unwrap();
    let message = match requests.try_recv().unwrap() {
        Message::CopyComplete => requests.try_recv().unwrap(),
        message => message,
    };
    assert!(matches!(message, Message::Request(QueuedRequest {
        request: Request::EntryDetails { entry: id, .. }, ..
    }) if id == entries[2].id));
    assert!(requests.try_recv().is_err());
    visual.simulate_mouse_up(
        gpui::point(px(300.), px(500.)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    window
        .update(&mut visual, |browser, _, _| assert_eq!(browser.selected, Some(entries[2].id)))
        .unwrap();
    assert!(requests.try_recv().is_err(), "release must not repeat navigation");

    for (index, tab_focused) in [(2, false), (0, true)] {
        if tab_focused {
            window
                .update(&mut visual, |browser, _, cx| {
                    let account = Id::from_bytes([2; 16]);
                    browser.set_account(account, None);
                    browser.volume_details.as_mut().unwrap().permissions = Acl {
                        grants: vec![Grant {
                            subject: Subject::Principal(Principal::Human(account)),
                            permissions: vec![Permission::Read],
                        }],
                    };
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
            window.update(&mut visual, |_, window, _| window.focus_next()).unwrap();
            cx.run_until_parked();
        }
        window
            .update(&mut visual, |browser, window, cx| {
                let control = &browser.controls[&format!("entry-{}", entries[index].id)];
                assert!(control.read(cx).focused(window));
                assert_eq!(control.read(cx).keyboard_focus_visible(window), tab_focused);
                browser.select(entries[index].id, cx);
                let mut current = browser.catalog.columns[0].page.clone().unwrap();
                current
                    .entries
                    .iter_mut()
                    .find(|row| row.id == entries[index].id)
                    .unwrap()
                    .deleted = true;
                browser.fixture_receive_root_page(volume(1).id, None, current, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(&mut visual, |browser, window, cx| {
                assert!(browser.selected.is_none());
                let key = if tab_focused {
                    format!("entry-{}", entries[1].id)
                } else {
                    "create-entry-menu".into()
                };
                let fallback = browser.controls[&key].read(cx);
                assert!(fallback.focused(window), "retirement retains a reachable focus target");
                assert_eq!(fallback.keyboard_focus_visible(window), tab_focused);
            })
            .unwrap();
    }
    for keyboard in [false, true] {
        window
            .update(&mut visual, |browser, _, cx| {
                browser
                    .fixture_catalog(EntryPage { entries: vec![summary(&entries[1])], next: None });
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(&mut visual, |browser, window, cx| {
                browser.controls[&format!("entry-{}", entries[1].id)].update(cx, |control, cx| {
                    if keyboard {
                        control.focus(window);
                    } else {
                        control.focus_without_ring(window, cx);
                    }
                });
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(&mut visual, |browser, window, cx| {
                assert_eq!(
                    browser.controls[&format!("entry-{}", entries[1].id)]
                        .read(cx)
                        .keyboard_focus_visible(window),
                    keyboard
                );
                browser.select(entries[1].id, cx);
                let mut deleted = summary(&entries[1]);
                deleted.deleted = true;
                browser.fixture_receive_root_page(
                    volume(1).id,
                    None,
                    EntryPage { entries: vec![deleted], next: None },
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(&mut visual, |browser, window, cx| {
                use gpui::{EntityInputHandler, Focusable};
                assert!(browser.selected.is_none());
                let identifier = &browser.identifier_fields["volume-id"];
                assert!(
                    identifier.read(cx).focus_handle(cx).is_focused(window),
                    "an empty read-only volume retains a selectable focus target"
                );
                if keyboard {
                    let selected = identifier.update(cx, |editor, cx| {
                        editor.selected_text_range(false, window, cx).unwrap()
                    });
                    assert_eq!(selected.range, 0..volume(1).id.to_string().len());
                }
            })
            .unwrap();
    }
}

#[gpui::test]
fn browser_scrollbars_share_virtual_list_state_and_keep_columns_independent(
    cx: &mut gpui::TestAppContext,
) {
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
    cx.simulate_window_resize(window.into(), gpui::size(px(1280.), px(600.)));
    let handles = window
        .update(cx, |browser, window, cx| {
            browser.fixture_catalog(EntryPage {
                entries: (1..=32).map(|byte| summary(&entry(byte))).collect(),
                next: None,
            });
            let mut details = object_details();
            let EntryKind::ObjectDb { schema } = &mut details.entry.info.kind else { panic!() };
            let declaration = schema.types.get_mut(&Id::from_bytes([2; 16])).unwrap();
            let mut object = record(4, "", 1);
            for byte in 3..=6 {
                let field = Id::from_bytes([byte; 16]);
                declaration.fields.insert(
                    field,
                    FieldDeclaration {
                        name: format!("Numbers {byte}"),
                        value_type: ValueType::Integer,
                        repeated: true,
                        deprecated: false,
                    },
                );
                object.fields.insert(
                    field,
                    FieldCell {
                        stamp: EditStamp { clock: 1, device_id: volume(1).owner },
                        values: Some((0..128).map(Value::Integer).collect()),
                    },
                );
            }
            browser.selected = Some(details.entry.id);
            browser.details = Some(Arc::new(details));
            browser.overview = false;
            browser.records.type_id = Some(object.type_id);
            browser.records.selected = Some(object.object_id);
            browser.records.object = Some(Arc::new(object));
            browser.records.page = Some(Arc::new(crate::store::ObjectPage {
                objects: (1..=64).map(|byte| record(byte, "A record", 1)).collect(),
                next: None,
            }));
            window.refresh();
            [
                browser.scrolling.entries.read(cx).handle(),
                browser.scrolling.records.read(cx).handle(),
                browser.scrolling.fields.read(cx).handle(),
            ]
        })
        .unwrap();
    cx.run_until_parked();
    for (index, handle) in handles.iter().enumerate() {
        assert!(
            handle.max_offset().height > px(0.),
            "viewport {index} must overflow: {:?}, maximum {:?}",
            handle.bounds(),
            handle.max_offset()
        );
    }
    assert!(
        handles[1].bounds().top() >= handles[0].bounds().bottom(),
        "records occupy the lower pane below the directory viewport"
    );
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (index, handle) in handles.iter().enumerate() {
        let position = handle.bounds().center();
        visual.simulate_event(gpui::ScrollWheelEvent {
            position,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-40.))),
            ..Default::default()
        });
        assert_eq!(handle.offset().y, px(-40.));
        for other in handles.iter().skip(index + 1) {
            assert_eq!(other.offset().y, px(0.));
        }
    }
    window
        .update(&mut visual, |browser, _, _| {
            assert_eq!(browser.scrolling.list.0.borrow().base_handle.offset().y, px(-40.));
        })
        .unwrap();

    // The minimum Shell width leaves 260 points beside its sidebar; retain a usable record editor.
    visual.simulate_resize(gpui::size(px(260.), px(640.)));
    visual.run_until_parked();
    let content = visual.debug_bounds("browser-content").unwrap();
    assert!(content.size.width >= px(480.));
    assert!(handles[2].bounds().size.width >= px(243.));
    let (lower, upper, upper_offset) = window
        .update(&mut visual, |browser, window, cx| {
            let lower = browser.scrolling.content_columns.read(cx).handle();
            let upper = browser.scrolling.columns.read(cx).handle();
            let upper_offset = upper.offset();
            assert!(lower.max_offset().width >= px(220.));
            window.activate_window();
            browser.controls["record-delete"].update(cx, |control, cx| {
                assert!(control.focus_without_ring(window, cx));
            });
            (lower, upper, upper_offset)
        })
        .unwrap();
    visual.run_until_parked();
    visual.update(|window, _| {
        let bounds = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| node.label() == Some("Delete record"))
            .unwrap()
            .1
            .bounds()
            .unwrap();
        assert!(px(bounds.x0 as f32) >= lower.bounds().left() - px(1.));
        assert!(
            px(bounds.x1 as f32) <= lower.bounds().right() + px(1.),
            "record action {bounds:?} must fit {:?} at {:?}",
            lower.bounds(),
            lower.offset()
        );
    });
    assert_eq!(upper.offset(), upper_offset, "record controls pan only the lower viewport");
}

#[gpui::test]
fn browser_views_scale_in_empty_editing_permission_and_database_states(
    cx: &mut gpui::TestAppContext,
) {
    // Sweep real browser columns while retaining record drafts and control identities.
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
    crate::desktop::test_support::check_zoom_geometry(window.into(), cx);
    for state in 0..12 {
        window
            .update(cx, |browser, window, cx| {
                browser
                    .fixture_catalog(EntryPage { entries: vec![summary(&entry(1))], next: None });
                let mut details = if (3..7).contains(&state) {
                    object_details()
                } else {
                    details(1, EntryAcl::Inherit)
                };
                if state == 7 {
                    details.entry.info.kind = EntryKind::Directory {};
                }
                if state == 8 || state == 9 {
                    details.entry.info.kind = EntryKind::Blob {
                        len_bytes: 123_456,
                        digest: [0; 32],
                        encrypted_key: [0; 32],
                        published_at_micros: 0,
                    };
                    details.entry.deleted = state == 9;
                }
                details.writable = state != 1;
                browser.selected = Some(details.entry.id);
                browser.details = Some(Arc::new(details));
                browser.overview = state == 2 || state >= 7;
                browser.adding_type = state == 4;
                browser.adding_field = state == 5;
                browser.error.clear();
                if (3..7).contains(&state) {
                    let object = record(4, "A long saved record title", 1);
                    browser.records.type_id = Some(object.type_id);
                    browser.records.selected = Some(object.object_id);
                    browser.records.object = Some(Arc::new(object));
                    browser.records.page = Some(Arc::new(crate::store::ObjectPage {
                        objects: vec![record(4, "Saved", 1)],
                        next: None,
                    }));
                }
                if state == 6 {
                    browser.error = "Couldn’t save the field".into();
                }
                if state == 10 {
                    browser.refresh_history();
                }
                if state == 11 {
                    browser.permissions_base = Some(super::drafts::canonical_permissions(
                        &browser.permissions,
                        &browser.retained_permissions,
                    ));
                    browser.permissions = EntryAcl::Replace(Acl {
                        grants: vec![Grant {
                            subject: Subject::Public,
                            permissions: vec![Permission::Read, Permission::Write],
                        }],
                    });
                    browser.update_permission_dirty();
                    browser.changed_during_edit = true;
                }
                window.refresh();
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        crate::desktop::test_support::check_zoom_geometry(window.into(), cx);
    }
}
