#![allow(clippy::unwrap_used, reason = "Tests assert valid bounded navigation fixtures.")]
use super::super::tests::{details, entry, retained_permissions, start_permission_draft, volume};
use super::*;
use crate::desktop::{
    test_support,
    worker_data::{DataReply, QueuedData},
};
use crate::runtime::host::{DataOperation, DataResult};
use crate::store::{
    key_value::{Chunk, Key, Page, Root},
    object_contract::{ObjectFields, TypeDeclaration},
    volume_index::{DirectoryAncestor, DirectoryListing, DirectoryTarget, EntrySummary},
};

fn fixture(
    cx: &mut gpui::TestAppContext,
) -> (Entity<Shell>, gpui::WindowHandle<Shell>, Entity<Browser>, async_channel::Receiver<Message>) {
    let (host, window, requests) = test_support::host(cx);
    let browser = host.update(cx, |host, cx| {
        host.volumes = vec![volume(1), volume(2)];
        host.select_volume(volume(1), cx);
        let browser = host.browser.as_ref().unwrap().clone();
        browser.update(cx, |browser, cx| {
            browser.fixture_catalog(EntryPage {
                entries: [(1, "Zebra"), (2, "Archive UI"), (3, "Archive")]
                    .map(|(id, name)| EntrySummary {
                        id: entry(id).id,
                        parent: None,
                        name: name.into(),
                        kind: crate::store::EntryType::KvDb,
                        protocol: Some(DatabaseProtocol::Kvdb),
                        deleted: false,
                        name_conflict: false,
                    })
                    .to_vec(),
                next: None,
            });
            cx.notify();
        });
        browser
    });
    window.update(cx, |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    while requests.try_recv().is_ok() {}
    (host, window, browser, requests)
}

fn focused(
    window: gpui::WindowHandle<Shell>,
    control: Entity<controls::Control>,
    cx: &mut gpui::TestAppContext,
) {
    window
        .update(cx, |_, window, cx| {
            assert!(control.read(cx).focused(window));
            assert!(
                !control.read(cx).keyboard_focus_visible(window),
                "arrows cannot reveal the ring"
            );
        })
        .unwrap();
}

fn data(requests: &async_channel::Receiver<Message>) -> QueuedData {
    loop {
        match requests.try_recv().unwrap() {
            Message::Data(data) => return data,
            Message::Request(_) => {}
            _ => panic!("unexpected non-request navigation work"),
        }
    }
}

fn reply(queued: QueuedData, result: DataResult) -> DataReply {
    DataReply {
        committed_root: None,
        owner: queued.owner,
        sequence: queued.sequence,
        volume: Id::from_bytes(queued.call.volume),
        entry: EntryId::from_bytes(queued.call.operation.entry_id().unwrap()).unwrap(),
        result: Ok(result),
    }
}

fn open_keys(
    browser: &Entity<Browser>,
    requests: &async_channel::Receiver<Message>,
    cx: &mut gpui::TestAppContext,
) {
    browser.update(cx, |browser, cx| {
        browser.select(entry(3).id, cx);
        let mut info = details(3, EntryAcl::Inherit);
        info.writable = true;
        browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: info }, cx);
        browser.receive_kv(
            reply(
                data(requests),
                DataResult::KvKeys(Page {
                    root: Some(Root { level: 0, digest: [3; 32] }),
                    keys: [b"a", b"b", b"c"]
                        .map(|key| Key { bytes: key.to_vec(), len_bytes: 1, preview: vec![] })
                        .to_vec(),
                    next: None,
                }),
            ),
            cx,
        );
    });
    cx.run_until_parked();
    while requests.try_recv().is_ok() {}
}

#[gpui::test]
fn entry_arrows_follow_display_order_and_retain_local_edits(cx: &mut gpui::TestAppContext) {
    let (_host, window, browser, requests) = fixture(cx);
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_click(gpui::point(px(300.), px(60.)), Default::default());
    for (keys, id) in [("down", 2), ("down", 1), ("up", 2)] {
        visual.simulate_keystrokes(keys);
        browser.read_with(cx, |browser, _| assert_eq!(browser.selected, Some(entry(id).id)));
        let control = browser.read_with(cx, |browser, _| {
            browser.controls[&format!("entry-{}", entry(id).id)].clone()
        });
        focused(window, control, cx);
    }
    let active = browser.update(cx, |browser, cx| {
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
            cx,
        );
        start_permission_draft(browser)
    });
    while requests.try_recv().is_ok() {}
    visual.simulate_keystrokes("down");
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.selected, Some(entry(1).id));
        assert_eq!(retained_permissions(volume(1).id, Some(entry(2).id), cx).active, active);
    });
    let control = browser
        .read_with(cx, |browser, _| browser.controls[&format!("entry-{}", entry(1).id)].clone());
    focused(window, control, cx);
    assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
        Message::Request(QueuedRequest { request: Request::EntryDetails { entry: id, .. }, .. }) if id == entry(1).id)));
    while requests.try_recv().is_ok() {}
    visual.simulate_keystrokes("down down");
    assert!(requests.try_recv().is_err(), "the end of a list is a local no-op");
}

#[gpui::test]
fn right_from_a_volume_selects_the_first_live_entry_and_left_activates_the_volume(
    cx: &mut gpui::TestAppContext,
) {
    let (host, window, browser, requests) = fixture(cx);
    browser.update(cx, |browser, cx| {
        browser.fixture_namespace(
            [(9, "A deleted folder", true), (1, "Attachments", false), (2, "Documents", false)]
                .map(|(id, name, deleted)| EntrySummary {
                    id: entry(id).id,
                    parent: None,
                    name: name.into(),
                    kind: crate::store::EntryType::Directory,
                    protocol: None,
                    deleted,
                    name_conflict: false,
                })
                .to_vec(),
        );
        cx.notify();
    });
    cx.run_until_parked();
    let volume_row = host.read_with(cx, |host, _| {
        host.navigation_controls[&format!("volume-{}", volume(1).id)].clone()
    });
    window
        .update(cx, |_, window, cx| {
            volume_row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    while requests.try_recv().is_ok() {}
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_keystrokes("right");
    let selected = browser.read_with(cx, |browser, _| {
        assert_eq!(browser.selected, Some(entry(1).id));
        assert_eq!(browser.catalog.columns[0].selected, Some(entry(1).id));
        assert_eq!(browser.catalog.columns[1].parent, Some(entry(1).id));
        browser.controls[&format!("directory-{}", entry(1).id)].clone()
    });
    focused(window, selected, cx);
    let work: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    assert_eq!(work.len(), 2);
    assert!(work.iter().any(|message| matches!(message, Message::Request(QueuedRequest {
        request: Request::EntryDetails { entry: id, .. }, ..
    }) if *id == entry(1).id)));
    assert!(work.iter().any(|message| matches!(message, Message::Request(QueuedRequest {
        request: Request::DirectoryEntries { target: DirectoryTarget::Directory(id), .. }, ..
    }) if *id == entry(1).id)));
    visual.simulate_keystrokes("left");
    focused(window, volume_row, cx);
    browser.read_with(cx, |browser, _| {
        assert!(browser.is_showing_volume());
        assert_eq!(browser.catalog.columns.len(), 1);
        assert!(browser.catalog.columns[0].selected.is_none());
    });
    assert!(requests.try_recv().is_err(), "showing the loaded volume is local navigation");
}

#[gpui::test]
fn horizontal_arrows_activate_adjacent_lists_and_leave_fields_alone(cx: &mut gpui::TestAppContext) {
    let (_host, window, browser, requests) = fixture(cx);
    open_keys(&browser, &requests, cx);
    window
        .update(cx, |_, window, cx| {
            let row = browser.read(cx).controls[&format!("entry-{}", entry(3).id)].clone();
            row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_keystrokes("right");
    let control = browser.read_with(cx, |browser, _| browser.controls["kv-key-61"].clone());
    focused(window, control, cx);
    let first = data(&requests);
    assert!(matches!(&first.call.operation, DataOperation::KvRead { key, .. } if key == b"a"));
    browser.update(cx, |browser, cx| {
        browser.receive_kv(
            reply(
                first,
                DataResult::KvRead(Chunk {
                    root: Root { level: 0, digest: [4; 32] },
                    offset: 0,
                    total_bytes: 1,
                    bytes: b"a".to_vec(),
                }),
            ),
            cx,
        );
    });
    cx.run_until_parked();
    visual.simulate_keystrokes("down");
    let pending = data(&requests);
    assert!(matches!(&pending.call.operation, DataOperation::KvRead { key, .. } if key == b"b"));
    cx.run_until_parked();
    let selected = browser.read_with(cx, |browser, _| browser.controls["kv-key-62"].clone());
    window
        .update(cx, |_, window, cx| {
            assert!(selected.read(cx).focused(window));
            assert_eq!(
                selected.read(cx).navigation_selection(),
                Some(true),
                "the chosen row is selected while its value loads"
            );
        })
        .unwrap();
    browser.update(cx, |browser, cx| {
        browser.receive_kv(
            reply(
                pending,
                DataResult::KvRead(Chunk {
                    root: Root { level: 0, digest: [4; 32] },
                    offset: 0,
                    total_bytes: 1,
                    bytes: b"b".to_vec(),
                }),
            ),
            cx,
        )
    });
    cx.run_until_parked();
    focused(window, selected, cx);
    assert!(
        visual.debug_bounds("table-focus-ring").is_none(),
        "finishing the value read cannot reveal an arrow-focused row's ring"
    );
    visual.simulate_keystrokes("left");
    browser.read_with(cx, |browser, _| {
        assert!(!browser.kv_editor_open(), "Left activates the store's inspector");
        assert_eq!(browser.kv_navigation_keys().len(), 3);
    });
    visual.simulate_keystrokes("right");
    let control = browser.read_with(cx, |browser, _| browser.controls["kv-key-61"].clone());
    focused(window, control, cx);
    let reopened = data(&requests);
    assert!(matches!(&reopened.call.operation, DataOperation::KvRead { key, .. } if key == b"a"));
    assert!(requests.try_recv().is_err(), "Right reads only the newly selected first value");
    browser.update(cx, |browser, cx| browser.new_kv_value(cx));
    cx.run_until_parked();
    let field = window.update(cx, |_, window, cx| window.focused(cx).unwrap()).unwrap();
    visual.simulate_keystrokes("left right up down");
    window
        .update(cx, |_, window, _| {
            assert!(field.is_focused(window), "text-field arrows stay in the editor")
        })
        .unwrap();
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
fn pointer_selected_store_arrows_activate_the_volume_and_first_key_but_leave_rename_input_alone(
    cx: &mut gpui::TestAppContext,
) {
    fn click_row(visual: &mut gpui::VisualTestContext, label: &str) {
        let position = visual.update(|window, _| {
            let bounds = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| {
                    node.role() == gpui::accesskit::Role::Button && node.label() == Some(label)
                })
                .unwrap()
                .1
                .bounds()
                .unwrap();
            gpui::point(px((bounds.x0 + 32.) as f32), px(((bounds.y0 + bounds.y1) / 2.) as f32))
        });
        visual.simulate_click(position, Default::default());
    }
    cx.update(text_editor::install_keybindings);
    let (host, window, browser, requests) = fixture(cx);
    browser.update(cx, |browser, cx| {
        browser.fixture_catalog(EntryPage {
            entries: [(3, "Preferences"), (1, "Zebra")]
                .map(|(id, name)| EntrySummary {
                    id: entry(id).id,
                    parent: None,
                    name: name.into(),
                    kind: crate::store::EntryType::KvDb,
                    protocol: Some(DatabaseProtocol::Kvdb),
                    deleted: false,
                    name_conflict: false,
                })
                .to_vec(),
            next: None,
        });
        cx.notify();
    });
    cx.run_until_parked();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1500.), px(750.)));
    visual.run_until_parked();
    click_row(&mut visual, "Zebra");
    browser.update(&mut visual, |browser, cx| {
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, EntryAcl::Inherit) },
            cx,
        );
        browser.receive_kv(
            reply(
                data(&requests),
                DataResult::KvKeys(Page { root: None, keys: vec![], next: None }),
            ),
            cx,
        );
    });
    visual.run_until_parked();
    click_row(&mut visual, "Preferences");
    let page = Page {
        root: Some(Root { level: 0, digest: [3; 32] }),
        keys: [b"color_scheme".as_slice(), b"seed".as_slice()]
            .into_iter()
            .map(|key| Key { bytes: key.to_vec(), len_bytes: 0, preview: vec![] })
            .collect(),
        next: None,
    };
    let active = browser.update(&mut visual, |browser, cx| {
        let mut current = details(3, EntryAcl::Inherit);
        current.entry.info.name = "Preferences".into();
        current.writable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
        browser.receive_kv(reply(data(&requests), DataResult::KvKeys(page.clone())), cx);
        start_permission_draft(browser)
    });
    visual.run_until_parked();
    let store = browser.read_with(&visual, |browser, _| {
        assert_eq!(browser.selected, Some(entry(3).id));
        assert!(!browser.kv_editor_open());
        browser.controls[&format!("entry-{}", entry(3).id)].clone()
    });
    focused(window, store.clone(), &mut visual);
    let identifier = visual.update(|window, _| {
        let expected = entry(3).id.to_string();
        let bounds = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| {
                node.role() == gpui::accesskit::Role::TextInput
                    && node.value() == Some(expected.as_str())
            })
            .unwrap()
            .1
            .bounds()
            .unwrap();
        gpui::point(
            px(((bounds.x0 + bounds.x1) / 2.) as f32),
            px(((bounds.y0 + bounds.y1) / 2.) as f32),
        )
    });
    visual.simulate_click(identifier, Default::default());
    visual.update(|window, cx| assert!(!store.read(cx).focused(window)));
    click_row(&mut visual, "Preferences");
    browser.update(&mut visual, |browser, _cx| {
        assert_eq!(browser.selected, Some(entry(3).id), "the pointer hit the store row");
        let current = browser.details.as_ref().expect("same selection retains its details");
        assert_eq!(current.entry.info.name, "Preferences");
        assert!(!current.writable);
        assert_eq!(browser.kv_navigation_keys().len(), page.keys.len());
    });
    visual.run_until_parked();
    focused(window, store.clone(), &mut visual);
    while requests.try_recv().is_ok() {}
    visual.simulate_keystrokes("right");
    let first = browser.read_with(&visual, |browser, _| {
        let first = &browser.kv_navigation_keys()[0].0;
        browser.controls[first].clone()
    });
    focused(window, first, &mut visual);
    let stale = data(&requests);
    assert!(
        matches!(&stale.call.operation, DataOperation::KvRead { key, .. } if key == b"color_scheme")
    );
    visual.simulate_keystrokes("left left");
    let volume_row = host.read_with(&visual, |host, _| {
        host.navigation_controls[&format!("volume-{}", volume(1).id)].clone()
    });
    focused(window, volume_row, &mut visual);
    browser.update(&mut visual, |browser, cx| {
        assert!(browser.is_showing_volume());
        assert_eq!(retained_permissions(volume(1).id, Some(entry(3).id), cx).active, active);
        browser.receive_kv(
            reply(
                stale,
                DataResult::KvRead(Chunk {
                    root: Root { level: 0, digest: [4; 32] },
                    offset: 0,
                    total_bytes: 5,
                    bytes: b"stale".to_vec(),
                }),
            ),
            cx,
        );
        assert!(browser.is_showing_volume() && !browser.kv_editor_open());
    });
    assert!(requests.try_recv().is_err(), "activating the volume does not reload its catalog");
    visual.simulate_keystrokes("right");
    browser.update(&mut visual, |browser, cx| {
        let mut current = details(3, EntryAcl::Inherit);
        current.entry.info.name = "Preferences".into();
        current.writable = true;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
        browser.receive_kv(reply(data(&requests), DataResult::KvKeys(page)), cx);
    });
    visual.run_until_parked();
    focused(window, store, &mut visual);
    click_row(&mut visual, "Preferences");
    visual.executor().advance_clock(std::time::Duration::from_secs(1));
    visual.run_until_parked();
    let rename_focus = visual.update(|window, cx| {
        use gpui::Focusable;
        let browser = browser.read(cx);
        let rename = browser.renaming.as_ref().expect("a second name click starts inline rename");
        let focus = rename.editor.read(cx).focus_handle(cx);
        assert!(focus.is_focused(window));
        focus
    });
    while requests.try_recv().is_ok() {}
    visual.simulate_keystrokes("left right");
    visual.update(|window, cx| {
        assert!(rename_focus.is_focused(window));
        assert_eq!(browser.read(cx).selected, Some(entry(3).id));
    });
    assert!(requests.try_recv().is_err(), "rename arrows stay in the text editor");
}

#[gpui::test]
fn volume_arrows_retain_local_edits_and_focus_the_new_volume(cx: &mut gpui::TestAppContext) {
    let (host, window, browser, requests) = fixture(cx);
    let active = browser.update(cx, |browser, cx| {
        browser.select(entry(3).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(3, EntryAcl::Inherit) },
            cx,
        );
        start_permission_draft(browser)
    });
    while requests.try_recv().is_ok() {}
    let control = host.read_with(cx, |host, _| {
        host.navigation_controls[&format!("volume-{}", volume(1).id)].clone()
    });
    window
        .update(cx, |_, window, cx| {
            control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_keystrokes("down");
    let control = host.read_with(cx, |host, cx| {
        let next = host.browser.as_ref().unwrap().read(cx);
        assert_eq!(next.volume.id, volume(2).id);
        assert!(next.selected.is_none());
        assert_eq!(retained_permissions(volume(1).id, Some(entry(3).id), cx).active, active);
        host.navigation_controls[&format!("volume-{}", volume(2).id)].clone()
    });
    focused(window, control, cx);
    assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
        Message::Request(QueuedRequest { request: Request::DirectoryEntries { volume: id, .. }, .. }) if id == volume(2).id)));
}

#[gpui::test]
fn left_reveals_the_selected_virtual_volume_including_a_pending_row(cx: &mut gpui::TestAppContext) {
    let (host, window, old_browser, requests) = fixture(cx);
    let page = old_browser.read_with(cx, |browser, _| browser.catalog.columns[0].page.clone());
    let browser = host.update(cx, |host, cx| {
        host.volumes = (1..=128)
            .map(|byte| {
                let mut volume = volume(byte);
                volume.name = format!("Volume{:03}", byte - 1);
                volume
            })
            .collect();
        host.select_volume(host.volumes[64].clone(), cx);
        let browser = host.browser.as_ref().unwrap().clone();
        browser.update(cx, |browser, cx| {
            browser.catalog.columns[0].page = page;
            cx.notify();
        });
        browser
    });
    cx.run_until_parked();
    let top = window
        .update(cx, |_, window, _| {
            window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Volume000"))
                .unwrap()
                .1
                .bounds()
                .unwrap()
                .y0
        })
        .unwrap();
    host.update(cx, |host, cx| {
        host.start_volume_creation(
            Request::CreateVolume { name: "Untitled".into(), share_key_with_service: true },
            cx,
        );
        assert_eq!(host.pending_volume.as_ref().unwrap().position(&host.volumes), 0);
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.label() == Some("Volume064"))
            );
            let row = browser.read(cx).controls[&format!("entry-{}", entry(3).id)].clone();
            row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    while requests.try_recv().is_ok() {}
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_keystrokes("left");
    window
        .update(cx, |host, window, cx| {
            assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser.entity_id());
            assert_eq!(browser.read(cx).volume.id, volume(65).id);
            let row = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Volume064"))
                .expect("the selected volume is mounted before focus transfers")
                .1;
            assert!((row.bounds().unwrap().y0 - top).abs() <= 1.);
            let control = host.navigation_controls[&format!("volume-{}", volume(65).id)].read(cx);
            assert!(control.focused(window));
            assert!(!control.keyboard_focus_visible(window));
        })
        .unwrap();
    assert!(requests.try_recv().is_err(), "column focus cannot reload a volume");
}

#[gpui::test]
fn schema_type_selection_hands_focus_to_records_and_retains_record_drafts(
    cx: &mut gpui::TestAppContext,
) {
    let (_host, window, browser, requests) = fixture(cx);
    let types = [Id::from_bytes([4; 16]), Id::from_bytes([5; 16])];
    let objects = [Id::from_bytes([6; 16]), Id::from_bytes([7; 16])];
    browser.update(cx, |browser, cx| {
        let mut info = details(3, EntryAcl::Inherit);
        info.entry.info.kind = EntryKind::ObjectDb {
            schema: Schema {
                types: types
                    .map(|id| {
                        (
                            id,
                            TypeDeclaration {
                                name: id.to_string(),
                                deprecated: false,
                                fields: BTreeMap::from([(
                                    Id::from_bytes([9; 16]),
                                    crate::store::object_contract::FieldDeclaration {
                                        name: "Title".into(),
                                        value_type: crate::store::object_contract::ValueType::Text,
                                        repeated: false,
                                        deprecated: false,
                                    },
                                )]),
                            },
                        )
                    })
                    .into(),
            },
        };
        browser.catalog.columns[0]
            .page
            .as_mut()
            .unwrap()
            .entries
            .iter_mut()
            .find(|e| e.id == info.entry.id)
            .unwrap()
            .kind = crate::store::EntryType::ObjectDb;
        browser.selected = Some(info.entry.id);
        browser.catalog.columns[0].selected = browser.selected;
        browser.details = Some(Arc::new(info));
        browser.overview = true;
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            let row = browser.read(cx).controls[&format!("schema-type-{}", types[0])].clone();
            row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_keystrokes("down");
    browser.read_with(cx, |browser, _| assert_eq!(browser.records.type_id, Some(types[1])));
    let entry_row = browser
        .read_with(cx, |browser, _| browser.controls[&format!("entry-{}", entry(3).id)].clone());
    focused(window, entry_row, cx);
    browser.update(cx, |browser, cx| {
        browser.records.page = Some(Arc::new(crate::store::ObjectPage {
            objects: objects
                .map(|object_id| ObjectFields {
                    object_id,
                    type_id: types[1],
                    fields: BTreeMap::new(),
                })
                .to_vec(),
            next: None,
        }));
        cx.notify();
    });
    cx.run_until_parked();
    while requests.try_recv().is_ok() {}
    visual.simulate_keystrokes("right down");
    browser.read_with(cx, |browser, _| assert_eq!(browser.records.selected, Some(objects[1])));
    let control = browser
        .read_with(cx, |browser, _| browser.controls[&format!("record-{}", objects[1])].clone());
    focused(window, control, cx);
    while requests.try_recv().is_ok() {}
    browser.update(cx, |browser, cx| {
        let field = Id::from_bytes([9; 16]);
        browser.field_editor(field, "draft", cx);
        browser.field_changed(field, "draft", 1, cx);
    });
    visual.simulate_keystrokes("up");
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.records.selected, Some(objects[0]));
        let draft = crate::desktop::drafts::snapshots(cx).into_iter().find(|draft|
            matches!(draft.target, crate::store::browser_drafts::BrowserDraftTarget::Record { object, .. } if object == objects[1])
        ).unwrap();
        let saved = crate::desktop::drafts::get(draft.id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Record(state) = &saved.body else { panic!() };
        assert_eq!(state.fields[0].text, "draft");
    });
    let control = browser
        .read_with(cx, |browser, _| browser.controls[&format!("record-{}", objects[0])].clone());
    focused(window, control, cx);
    assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
        Message::Request(QueuedRequest { request: Request::ReadObject { object, .. }, .. }) if object == objects[0])));

    browser.update(cx, |browser, cx| {
        browser.overview = true;
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            let row = browser.read(cx).controls[&format!("schema-type-{}", types[0])].clone();
            row.update(cx, |row, cx| {
                row.focus(window);
                row.show_keyboard_focus_if_focused(window, cx);
            });
            browser.update(cx, |browser, cx| browser.select_type(types[0], cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            let row = &browser.read(cx).controls[&format!("entry-{}", entry(3).id)];
            assert!(row.read(cx).focused(window));
            assert!(row.read(cx).keyboard_focus_visible(window));
        })
        .unwrap();
}

fn long_record_page(browser: &Entity<Browser>, cx: &mut gpui::TestAppContext) -> Vec<Id> {
    let type_id = Id::from_bytes([4; 16]);
    let ids: Vec<_> = (1..=64).map(|byte| Id::from_bytes([byte; 16])).collect();
    browser.update(cx, |browser, cx| {
        let mut info = details(3, EntryAcl::Inherit);
        info.entry.info.kind = EntryKind::ObjectDb {
            schema: Schema {
                types: BTreeMap::from([(
                    type_id,
                    TypeDeclaration {
                        name: "Record".into(),
                        deprecated: false,
                        fields: BTreeMap::new(),
                    },
                )]),
            },
        };
        browser.catalog.columns[0]
            .page
            .as_mut()
            .unwrap()
            .entries
            .iter_mut()
            .find(|summary| summary.id == info.entry.id)
            .unwrap()
            .kind = crate::store::EntryType::ObjectDb;
        browser.selected = Some(info.entry.id);
        browser.catalog.columns[0].selected = browser.selected;
        browser.details = Some(Arc::new(info));
        browser.overview = false;
        browser.records.type_id = Some(type_id);
        let objects: Vec<_> = ids
            .iter()
            .map(|id| ObjectFields { object_id: *id, type_id, fields: BTreeMap::new() })
            .collect();
        browser.records.selected = Some(ids[0]);
        browser.records.object = Some(Arc::new(objects[0].clone()));
        browser.records.page = Some(Arc::new(crate::store::ObjectPage { objects, next: None }));
        cx.notify();
    });
    cx.run_until_parked();
    ids
}

fn visible_record(window: gpui::WindowHandle<Shell>, id: Id, cx: &mut gpui::TestAppContext) {
    window
        .update(cx, |_, window, _| {
            let label = id.to_string();
            let row = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some(label.as_str()))
                .expect("focused record is mounted")
                .1;
            let bounds = row.bounds().unwrap();
            assert!(bounds.y0 >= 44. && bounds.y1 <= f64::from(window.viewport_size().height));
        })
        .unwrap();
}

#[gpui::test]
fn record_page_navigation_supersedes_a_pending_launch_even_after_returning(
    cx: &mut gpui::TestAppContext,
) {
    let (_host, window, browser, _requests) = fixture(cx);
    let ids = long_record_page(&browser, cx);
    let cursor = ids[63];
    let start = browser.update(cx, |browser, cx| {
        browser.records.selected = None;
        browser.records.object = None;
        let page = browser.records.page.as_mut().expect("record list");
        Arc::make_mut(page).next = Some(cursor);
        cx.notify();
        browser.interaction_revision
    });
    cx.run_until_parked();
    let press = |key: &str, cx: &mut gpui::TestAppContext| {
        window
            .update(cx, |_, window, cx| browser.read(cx).controls[key].read(cx).focus(window))
            .expect("browser window");
        test_support::press(window, "enter", cx);
    };
    press("records-first", cx);
    browser.read_with(cx, |browser, _| assert_eq!(browser.interaction_revision, start));
    press("records-next", cx);
    let next = browser.update(cx, |browser, cx| {
        assert_eq!(browser.records.after, Some(cursor));
        assert_eq!(browser.records.selected, None);
        assert_ne!(browser.interaction_revision, start);
        let next = browser.interaction_revision;
        browser.receive(
            Reply::Objects(crate::engine::reply::ObjectPageResult {
                database: browser.database_address().expect("record database"),
                type_id: browser.records.type_id.expect("record type"),
                after: Some(cursor),
                page: crate::store::ObjectPage { objects: vec![], next: None },
            }),
            cx,
        );
        next
    });
    cx.run_until_parked();
    browser.read_with(cx, |browser, _| assert_eq!(browser.interaction_revision, next));
    press("records-first", cx);
    browser.read_with(cx, |browser, _| {
        assert_eq!(browser.records.after, None);
        assert_ne!(browser.interaction_revision, start);
    });
}

#[gpui::test]
fn arrows_reveal_virtualized_records_and_horizontal_return_restores_the_selected_row(
    cx: &mut gpui::TestAppContext,
) {
    let (_host, window, browser, requests) = fixture(cx);
    let ids = long_record_page(&browser, cx);
    window
        .update(cx, |_, window, cx| {
            assert!(f32::from(window.viewport_size().height) < 64. * 32.);
            let last = ids[63].to_string();
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.label() == Some(last.as_str()))
            );
            let row = browser.read(cx).controls[&format!("record-{}", ids[0])].clone();
            row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    while requests.try_recv().is_ok() {}
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for id in ids.iter().skip(1) {
        visual.simulate_keystrokes("down");
        browser.read_with(cx, |browser, _| assert_eq!(browser.records.selected, Some(*id)));
        while requests.try_recv().is_ok() {}
    }
    visible_record(window, ids[63], cx);
    visual.simulate_keystrokes("left");
    browser.update(cx, |browser, cx| {
        browser.scrolling.list.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            let last = ids[63].to_string();
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.label() == Some(last.as_str()))
            );
        })
        .unwrap();
    visual.simulate_keystrokes("right");
    let row = browser
        .read_with(cx, |browser, _| browser.controls[&format!("record-{}", ids[63])].clone());
    focused(window, row, cx);
    visible_record(window, ids[63], cx);
    assert!(requests.try_recv().is_err(), "column focus cannot reread the selected record");
    for id in ids.iter().rev().skip(1) {
        visual.simulate_keystrokes("up");
        browser.read_with(cx, |browser, _| assert_eq!(browser.records.selected, Some(*id)));
        while requests.try_recv().is_ok() {}
    }
    visible_record(window, ids[0], cx);
}

#[gpui::test]
fn right_preserves_the_folder_path_and_left_selects_the_parent_with_its_inspector(
    cx: &mut gpui::TestAppContext,
) {
    let (_host, window, browser, requests) = fixture(cx);
    browser.update(cx, |browser, cx| {
        let mut rows = Vec::new();
        for (id, name, parent, directory) in [
            (1, "Attachments", None, true),
            (2, "2026", Some(1), true),
            (3, "Settings", Some(2), false),
            (9, "Other", None, false),
        ] {
            rows.push(EntrySummary {
                id: entry(id).id,
                parent: parent.map(|id| entry(id).id),
                name: name.into(),
                kind: if directory {
                    crate::store::EntryType::Directory
                } else {
                    crate::store::EntryType::KvDb
                },
                protocol: if directory { None } else { Some(DatabaseProtocol::Kvdb) },
                deleted: false,
                name_conflict: false,
            });
        }
        browser.selected = Some(entry(3).id);
        browser.fixture_namespace(rows);
        let mut current = details(3, EntryAcl::Inherit);
        current.entry.info.namespace.parent = Some(entry(2).id);
        browser.details = Some(Arc::new(current));
        browser.overview = true;
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            let row = browser.read(cx).controls[&format!("directory-{}", entry(1).id)].clone();
            row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
        })
        .unwrap();
    while requests.try_recv().is_ok() {}
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (id, directory) in [(2, true), (3, false)] {
        visual.simulate_keystrokes("right");
        let row = browser.read_with(cx, |browser, _| {
            browser.controls
                [&format!("{}-{}", if directory { "directory" } else { "entry" }, entry(id).id)]
                .clone()
        });
        focused(window, row, cx);
        browser.read_with(cx, |browser, _| {
            assert_eq!(browser.selected, Some(entry(3).id));
            assert_eq!(
                browser.catalog.columns.iter().map(|column| column.selected).collect::<Vec<_>>(),
                vec![Some(entry(1).id), Some(entry(2).id), Some(entry(3).id)]
            );
        });
    }
    assert!(requests.try_recv().is_err(), "Right keeps the selected path without rereading it");
    let active = browser.update(cx, |browser, _| start_permission_draft(browser));
    for id in [2, 1] {
        visual.simulate_keystrokes("left");
        let row = browser.read_with(cx, |browser, cx| {
            assert_eq!(browser.selected, Some(entry(id).id));
            assert_eq!(browser.catalog.columns.len(), usize::from(id) + 1);
            assert_eq!(browser.catalog.columns.last().unwrap().parent, Some(entry(id).id));
            assert!(browser.catalog.columns.last().unwrap().selected.is_none());
            assert!(browser.details.is_none(), "parent authority is read afresh");
            assert_eq!(retained_permissions(volume(1).id, Some(entry(3).id), cx).active, active);
            browser.controls[&format!("directory-{}", entry(id).id)].clone()
        });
        focused(window, row, cx);
        let work: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(work.len(), 2);
        assert!(work.iter().any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::EntryDetails { entry: selected, .. }, .. })
                if *selected == entry(id).id)));
        assert!(work.iter().any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::DirectoryEntries {
                target: DirectoryTarget::Directory(parent), cut: Some(1), after: None, limit: 32, ..
            }, .. }) if *parent == entry(id).id)));
        browser.update(cx, |browser, cx| {
            let name = if id == 1 { "Attachments" } else { "2026" };
            let child = if id == 1 {
                EntrySummary {
                    id: entry(2).id,
                    parent: Some(entry(1).id),
                    name: "2026".into(),
                    kind: crate::store::EntryType::Directory,
                    protocol: None,
                    deleted: false,
                    name_conflict: false,
                }
            } else {
                EntrySummary {
                    id: entry(3).id,
                    parent: Some(entry(2).id),
                    name: "Settings".into(),
                    kind: crate::store::EntryType::KvDb,
                    protocol: Some(DatabaseProtocol::Kvdb),
                    deleted: false,
                    name_conflict: false,
                }
            };
            browser.receive(
                Reply::DirectoryEntries {
                    volume: volume(1).id,
                    target: DirectoryTarget::Directory(entry(id).id),
                    cut_requested: Some(1),
                    after: None,
                    listing: DirectoryListing::Page {
                        cut: 1,
                        parent: Some(entry(id).id),
                        ancestors: (1..=id)
                            .map(|parent| DirectoryAncestor {
                                id: entry(parent).id,
                                parent: (parent > 1).then(|| entry(parent - 1).id),
                                name: if parent == 1 { "Attachments" } else { "2026" }.into(),
                            })
                            .collect(),
                        entries: vec![child],
                        next: None,
                    },
                },
                cx,
            );
            let mut current = details(id, EntryAcl::Inherit);
            current.entry.info.name = name.into();
            current.entry.info.kind = EntryKind::Directory {};
            current.entry.info.protocol = None;
            current.entry.info.namespace.parent = (id > 1).then(|| entry(id - 1).id);
            current.writable = false;
            current.manage_permissions = false;
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
        });
        cx.run_until_parked();
        browser.read_with(cx, |browser, cx| {
            assert_eq!(browser.details.as_ref().unwrap().entry.id, entry(id).id);
            assert_eq!(
                browser.identifier_fields["inspector-id"].read(cx).text(),
                entry(id).id.to_string()
            );
            assert!(browser.catalog.columns.last().unwrap().page.is_some());
        });
        while requests.try_recv().is_ok() {}
    }
    visual.simulate_keystrokes("down");
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.selected, Some(entry(9).id));
        assert_eq!(browser.catalog.columns.len(), 1);
        assert_eq!(retained_permissions(volume(1).id, Some(entry(3).id), cx).active, active);
    });
}

#[gpui::test]
fn selecting_nested_entries_reveals_folders_and_keeps_leaf_content_below(
    cx: &mut gpui::TestAppContext,
) {
    fn draw(visual: &mut gpui::VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        visual.run_until_parked();
    }
    fn bounds(browser: &Browser, parent: EntryId, cx: &App) -> gpui::Bounds<gpui::Pixels> {
        let (mut bounds, previous) =
            browser.scrolling.directories[&Some(parent)].bounds.get().unwrap();
        bounds.origin.x += browser.scrolling.columns.read(cx).handle().offset().x - previous;
        bounds
    }
    let rows = |directory| {
        [
            (1, "Attachments", None),
            (2, "2026", Some(1)),
            (3, if directory { "baz" } else { "Notes" }, Some(2)),
        ]
        .map(|(id, name, parent)| EntrySummary {
            id: entry(id).id,
            parent: parent.map(|id| entry(id).id),
            name: name.into(),
            kind: if id != 3 || directory {
                crate::store::EntryType::Directory
            } else {
                crate::store::EntryType::KvDb
            },
            protocol: (id == 3 && !directory).then_some(DatabaseProtocol::Kvdb),
            deleted: false,
            name_conflict: false,
        })
        .into_iter()
        .chain(std::iter::once(EntrySummary {
            id: entry(9).id,
            parent: Some(entry(2).id),
            name: "Other".into(),
            kind: crate::store::EntryType::KvDb,
            protocol: Some(DatabaseProtocol::Kvdb),
            deleted: false,
            name_conflict: false,
        }))
        .collect()
    };
    // The 240-dp volume rail is outside the browser viewport. These widths cover a
    // partial child, a fitting pair with a clipped child, and an already-visible pair.
    for (directory, width) in [true, false]
        .into_iter()
        .flat_map(|directory| [700., 1024., 2200.].map(|width| (directory, width)))
    {
        let (_host, window, browser, requests) = fixture(cx);
        browser.update(cx, |browser, cx| {
            browser.selected = Some(entry(2).id);
            browser.fixture_namespace(rows(directory));
            let mut current = details(2, EntryAcl::Inherit);
            current.entry.info.name = "2026".into();
            current.entry.info.kind = EntryKind::Directory {};
            current.entry.info.protocol = None;
            current.entry.info.namespace.parent = Some(entry(1).id);
            browser.details = Some(Arc::new(current));
            cx.notify();
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(gpui::size(px(width), px(600.)));
        draw(&mut visual);
        window
            .update(&mut visual, |_, window, cx| {
                browser.update(cx, |browser, cx| {
                    assert!(browser.focus_navigation_row(
                        &Row { target: Target::Entry(entry(3).id), control: None, selected: false },
                        gpui::ScrollStrategy::Top,
                        window,
                        cx,
                    ));
                });
            })
            .unwrap();
        draw(&mut visual);
        let (columns, before, position) = window
            .update(&mut visual, |_, window, cx| {
                let browser = browser.read(cx);
                let columns = browser.scrolling.columns.read(cx).handle();
                let prefix = if directory { "directory" } else { "entry" };
                let row = browser.controls[&format!("{prefix}-{}", entry(3).id)].read(cx);
                assert!(row.focused(window));
                assert!(!row.keyboard_focus_visible(window));
                assert_eq!(browser.selected, Some(entry(2).id));
                let node = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some(if directory { "baz" } else { "Notes" }))
                    .unwrap()
                    .1;
                let bounds = node.bounds().unwrap();
                (
                    columns.clone(),
                    columns.offset(),
                    gpui::point(
                        px(((bounds.x0 + bounds.x1) / 2.) as f32),
                        px(((bounds.y0 + bounds.y1) / 2.) as f32),
                    ),
                )
            })
            .unwrap();
        while requests.try_recv().is_ok() {}
        visual.simulate_click(position, Default::default());
        draw(&mut visual);
        browser.read_with(&visual, |browser, _| {
            assert_eq!(browser.selected, Some(entry(3).id));
            if directory {
                assert!(browser.catalog.columns.last().unwrap().page.is_none());
            } else {
                assert_eq!(browser.catalog.columns.len(), 3);
                assert!(browser.details.is_none());
            }
        });
        let selected = browser.read_with(&visual, |browser, cx| bounds(browser, entry(2).id, cx));
        let viewport = columns.bounds();
        let tolerance = px(1.);
        assert!(selected.left() >= viewport.left() - tolerance);
        assert!(selected.right() <= viewport.right() + tolerance);
        if directory {
            let contents =
                browser.read_with(&visual, |browser, cx| bounds(browser, entry(3).id, cx));
            if width == 700. {
                assert!(contents.right() - selected.left() > viewport.size.width);
                assert!((selected.left() - viewport.left()).abs() <= tolerance);
                assert!(contents.left() < viewport.right());
                assert!(contents.right() > viewport.right());
            } else {
                assert!(contents.right() - selected.left() <= viewport.size.width + tolerance);
                assert!(contents.right() <= viewport.right() + tolerance);
                if width == 1024. {
                    assert!(columns.offset().x < before.x, "reveal the newly clipped child");
                } else {
                    assert_eq!(columns.offset(), before, "an already-visible pair does not scroll");
                }
            }
        } else {
            assert_eq!(columns.offset(), before, "leaf content does not pan the upper columns");
            let upper = visual.debug_bounds("browser-upper").unwrap();
            let lower = visual.debug_bounds("browser-lower").unwrap();
            let content = visual.debug_bounds("browser-content").unwrap();
            assert!(lower.top() >= upper.bottom() - tolerance);
            assert!(content.top() >= lower.top() - tolerance);
            assert!(content.bottom() <= lower.bottom() + tolerance);
            assert!(content.size.height > px(0.));
        }
        let work: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(work.len(), if directory { 2 } else { 1 });
        if directory {
            assert!(work.iter().any(|message| matches!(message, Message::Request(QueuedRequest {
            request: Request::DirectoryEntries {
                target: DirectoryTarget::Directory(id), cut: Some(1), after: None, limit: 32, ..
            }, ..
            }) if *id == entry(3).id)));
        }
        assert!(work.iter().any(|message| matches!(message, Message::Request(QueuedRequest {
            request: Request::EntryDetails { entry: id, .. }, ..
        }) if *id == entry(3).id)));

        let lower = visual.debug_bounds("browser-lower").unwrap();
        if columns.max_offset().width > px(0.) {
            let revealed = columns.offset();
            let delta = if revealed.x < px(0.) { px(80.) } else { px(-80.) };
            visual.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::point(viewport.right() - px(20.), viewport.top() + px(100.)),
                delta: gpui::ScrollDelta::Pixels(gpui::point(delta, px(0.))),
                ..Default::default()
            });
            draw(&mut visual);
            assert_ne!(
                columns.offset(),
                revealed,
                "a horizontal wheel pans manually: directory={directory}, width={width}"
            );
            assert_eq!(visual.debug_bounds("browser-lower").unwrap(), lower);
        }
        let manual = columns.offset();
        browser.update(&mut visual, |browser, cx| {
            if directory {
                browser.receive(
                    Reply::DirectoryEntries {
                        volume: volume(1).id,
                        target: DirectoryTarget::Directory(entry(3).id),
                        cut_requested: Some(1),
                        after: None,
                        listing: DirectoryListing::Page {
                            cut: 1,
                            parent: Some(entry(3).id),
                            ancestors: [
                                (1, "Attachments", None),
                                (2, "2026", Some(1)),
                                (3, "baz", Some(2)),
                            ]
                            .map(|(id, name, parent)| DirectoryAncestor {
                                id: entry(id).id,
                                parent: parent.map(|id| entry(id).id),
                                name: name.into(),
                            })
                            .to_vec(),
                            entries: vec![EntrySummary {
                                id: entry(4).id,
                                parent: Some(entry(3).id),
                                name: "Settings".into(),
                                kind: crate::store::EntryType::KvDb,
                                protocol: Some(DatabaseProtocol::Kvdb),
                                deleted: false,
                                name_conflict: false,
                            }],
                            next: None,
                        },
                    },
                    cx,
                );
            }
            let mut current = details(3, EntryAcl::Inherit);
            current.entry.info.name = if directory { "baz" } else { "Notes" }.into();
            if directory {
                current.entry.info.kind = EntryKind::Directory {};
                current.entry.info.protocol = None;
            }
            current.entry.info.namespace.parent = Some(entry(2).id);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
            if !directory {
                let keys = data(&requests);
                assert!(matches!(keys.call.operation, DataOperation::KvKeys { .. }));
                browser.receive_kv(
                    reply(
                        keys,
                        DataResult::KvKeys(Page {
                            root: None,
                            keys: [b"color_scheme".as_slice(), b"seed".as_slice()]
                                .into_iter()
                                .map(|key| Key {
                                    bytes: key.to_vec(),
                                    len_bytes: 0,
                                    preview: vec![],
                                })
                                .collect(),
                            next: None,
                        }),
                    ),
                    cx,
                );
            }
        });
        draw(&mut visual);
        visual.deactivate_window();
        visual.update(|window, _| window.activate_window());
        draw(&mut visual);
        assert_eq!(columns.offset(), manual, "late pages and reactivation do not repeat reveal");
        while requests.try_recv().is_ok() {}
        window
            .update(&mut visual, |_, window, cx| {
                let row = browser.read(cx).controls[&format!("directory-{}", entry(2).id)].clone();
                row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
            })
            .unwrap();
        visual.simulate_keystrokes("right");
        draw(&mut visual);
        browser.read_with(&visual, |browser, _| {
            assert_eq!(browser.selected, Some(entry(3).id));
            assert_eq!(browser.catalog.columns.len(), if directory { 4 } else { 3 });
        });
        assert!(
            requests.try_recv().is_err(),
            "Right on the retained path does not request content"
        );
        let focused = browser.read_with(&visual, |browser, _| {
            let prefix = if directory { "directory" } else { "entry" };
            browser.controls[&format!("{prefix}-{}", entry(3).id)].clone()
        });
        window
            .update(&mut visual, |_, window, cx| {
                assert!(focused.read(cx).focused(window));
                assert!(!focused.read(cx).keyboard_focus_visible(window));
            })
            .unwrap();
        let active = browser.update(&mut visual, |browser, _| start_permission_draft(browser));
        visual.simulate_keystrokes("down");
        draw(&mut visual);
        browser.read_with(&visual, |browser, cx| {
            assert_eq!(browser.selected, Some(entry(9).id));
            assert_eq!(browser.catalog.columns.len(), 3);
            assert_eq!(retained_permissions(volume(1).id, Some(entry(3).id), cx).active, active);
        });
        assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|message| matches!(message,
            Message::Request(QueuedRequest { request: Request::EntryDetails { entry: id, .. }, .. }) if id == entry(9).id)));
        window.update(&mut visual, |_, window, _| window.remove_window()).unwrap();
    }
}

#[gpui::test]
fn delayed_entry_discovery_preserves_panning_and_reveals_only_folder_pairs(
    cx: &mut gpui::TestAppContext,
) {
    // Open a known ID outside the retained page, then resolve its path after a real wheel event.
    fn draw(visual: &mut gpui::VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        visual.run_until_parked();
    }
    fn row(id: u8, name: &str, parent: Option<u8>, directory: bool) -> EntrySummary {
        EntrySummary {
            id: entry(id).id,
            parent: parent.map(|id| entry(id).id),
            name: name.into(),
            kind: if directory {
                crate::store::EntryType::Directory
            } else {
                crate::store::EntryType::KvDb
            },
            protocol: (!directory).then_some(DatabaseProtocol::Kvdb),
            deleted: false,
            name_conflict: false,
        }
    }
    fn deliver_path_page(browser: &mut Browser, directory: bool, cx: &mut Context<Browser>) {
        let pending = browser.catalog.pending.clone().unwrap();
        let ancestors: Vec<_> =
            [(1, "Attachments", None), (2, "2026", Some(1)), (3, "Pictures", Some(2))]
                .map(|(id, name, parent)| DirectoryAncestor {
                    id: entry(id).id,
                    parent: parent.map(|id| entry(id).id),
                    name: name.into(),
                })
                .into();
        let (parent, count, entries) = match pending.target {
            DirectoryTarget::Entry(id) if id == entry(3).id => {
                (Some(entry(2).id), 2, vec![row(3, "Pictures", Some(2), directory)])
            }
            DirectoryTarget::Root => (None, 0, vec![row(1, "Attachments", None, true)]),
            DirectoryTarget::Directory(id) if id == entry(1).id => {
                (Some(id), 1, vec![row(2, "2026", Some(1), true)])
            }
            DirectoryTarget::Directory(id) if directory && id == entry(3).id => {
                (Some(id), 3, vec![row(4, "Notes", Some(3), false)])
            }
            _ => panic!("unexpected path page"),
        };
        browser.receive(
            Reply::DirectoryEntries {
                volume: browser.volume.id,
                target: pending.target,
                cut_requested: pending.cut,
                after: pending.after,
                listing: DirectoryListing::Page {
                    cut: 1,
                    parent,
                    ancestors: ancestors[..count].to_vec(),
                    entries,
                    next: None,
                },
            },
            cx,
        );
    }
    for (directory, manual) in [false, true]
        .into_iter()
        .flat_map(|directory| [false, true].map(|manual| (directory, manual)))
    {
        let (_host, window, browser, _requests) = fixture(cx);
        browser.update(cx, |browser, cx| {
            browser.selected = Some(entry(2).id);
            browser.fixture_namespace(vec![
                row(1, "Attachments", None, true),
                row(2, "2026", Some(1), true),
                row(9, "Other", Some(2), false),
            ]);
            let mut current = details(2, EntryAcl::Inherit);
            current.entry.info.kind = EntryKind::Directory {};
            current.entry.info.protocol = None;
            current.entry.info.namespace.parent = Some(entry(1).id);
            browser.details = Some(Arc::new(current));
            cx.notify();
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(gpui::size(px(1024.), px(600.)));
        draw(&mut visual);
        browser.update(&mut visual, |browser, cx| browser.select(entry(3).id, cx));
        draw(&mut visual);
        let columns = browser.read_with(&visual, |browser, cx| {
            assert!(browser.catalog_entry(entry(3).id).is_none());
            assert_eq!(
                browser.catalog.pending.as_ref().unwrap().target,
                DirectoryTarget::Entry(entry(3).id)
            );
            browser.scrolling.columns.read(cx).handle()
        });
        let waiting = columns.offset();
        if manual {
            visual.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::point(
                    columns.bounds().right() - px(20.),
                    columns.bounds().top() + px(100.),
                ),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(-80.), px(0.))),
                ..Default::default()
            });
            draw(&mut visual);
            assert!(columns.offset().x < waiting.x, "the user pans while the path is pending");
        }
        let before_reply = columns.offset();
        browser.update(&mut visual, |browser, cx| deliver_path_page(browser, directory, cx));
        draw(&mut visual);
        if manual || !directory {
            assert_eq!(
                columns.offset(),
                before_reply,
                "late leaf discovery and manual panning do not move the upper viewport"
            );
        } else {
            assert!(
                columns.offset().x < before_reply.x,
                "late discovery still reveals an untouched pair"
            );
            browser.read_with(&visual, |browser, _| {
                let (list, _) =
                    browser.scrolling.directories[&Some(entry(2).id)].bounds.get().unwrap();
                let (content, _) =
                    browser.scrolling.directories[&Some(entry(3).id)].bounds.get().unwrap();
                assert!(list.left() >= columns.bounds().left() - px(1.));
                assert!(content.right() <= columns.bounds().right() + px(1.));
            });
        }
        let resolved = columns.offset();
        for _ in 0..if directory { 3 } else { 2 } {
            browser.update(&mut visual, |browser, cx| deliver_path_page(browser, directory, cx));
            draw(&mut visual);
        }
        browser.read_with(&visual, |browser, _| assert!(browser.catalog.pending.is_none()));
        assert_eq!(columns.offset(), resolved, "remaining ancestor reads cannot repeat reveal");
        window.update(&mut visual, |_, window, _| window.remove_window()).unwrap();
    }
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn selecting_a_sibling_folder_during_elastic_scroll_keeps_its_list_visible(
    cx: &mut gpui::TestAppContext,
) {
    fn draw(visual: &mut gpui::VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        visual.run_until_parked();
    }
    let (_host, window, browser, _requests) = fixture(cx);
    browser.update(cx, |browser, cx| {
        browser.selected = Some(entry(1).id);
        browser.fixture_namespace(
            [(1, "foo", None), (2, "bar", None), (3, "child", Some(1))]
                .map(|(id, name, parent)| EntrySummary {
                    id: entry(id).id,
                    parent: parent.map(|id| entry(id).id),
                    name: name.into(),
                    kind: crate::store::EntryType::Directory,
                    protocol: None,
                    deleted: false,
                    name_conflict: false,
                })
                .to_vec(),
        );
        let mut current = details(1, EntryAcl::Inherit);
        current.entry.info.name = "foo".into();
        current.entry.info.kind = EntryKind::Directory {};
        current.entry.info.protocol = None;
        browser.details = Some(Arc::new(current));
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(700.), px(600.)));
    draw(&mut visual);
    let columns = browser.read_with(&visual, |browser, cx| {
        assert_eq!(browser.catalog.columns.len(), 2);
        browser.scrolling.columns.read(cx).handle()
    });
    let viewport = columns.bounds();
    let range = columns.max_offset();
    assert_eq!(columns.offset().x, px(0.));
    visual.simulate_event(gpui::ScrollWheelEvent {
        position: gpui::point(viewport.left() + px(100.), viewport.top() + px(100.)),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(100.), px(0.))),
        touch_phase: gpui::TouchPhase::Started,
        is_gesture: true,
        ..Default::default()
    });
    draw(&mut visual);
    assert_eq!(columns.offset().x, px(0.));
    assert!(columns.visual_offset().x > px(0.));
    let position = window
        .update(&mut visual, |_, window, _| {
            let bounds = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("bar"))
                .unwrap()
                .1
                .bounds()
                .unwrap();
            gpui::point(
                px(((bounds.x0 + bounds.x1) / 2.) as f32),
                px(((bounds.y0 + bounds.y1) / 2.) as f32),
            )
        })
        .unwrap();
    // Keep the gesture open; changing siblings preserves the column count and range.
    visual.simulate_click(position, Default::default());
    draw(&mut visual);
    browser.read_with(&visual, |browser, _| {
        assert_eq!(browser.selected, Some(entry(2).id));
        assert_eq!(browser.catalog.columns.len(), 2);
        assert_eq!(browser.catalog.columns[1].parent, Some(entry(2).id));
        let (mut logical, _) = browser.scrolling.directories[&None].bounds.get().unwrap();
        logical.origin.x -= columns.elastic_offset().x;
        assert!(logical.left() >= viewport.left() - px(1.));
        assert!(logical.right() <= viewport.right() + px(1.));
    });
    assert_eq!(columns.max_offset(), range);
    assert_eq!(columns.offset().x, px(0.), "elastic paint coordinates cannot scroll the list");
}
