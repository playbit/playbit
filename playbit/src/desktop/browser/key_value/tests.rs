#![allow(clippy::unwrap_used, reason = "Tests assert complete correlated browser exchanges.")]
use super::super::tests::{details, entry, volume};
use super::*;
use crate::desktop::{
    worker_data::{DataReply, QueuedData},
    worker_queue::{self, Message},
};
use crate::store::key_value::MAX_STORED_BYTES;

fn fill_draft_capacity(browser: &Browser, cx: &mut Context<Browser>) -> Id {
    use crate::store::browser_drafts::*;
    for summary in browser.local_draft_summaries(cx) {
        crate::desktop::drafts::remove(summary.id, cx);
    }
    for byte in 1..=MAX_DRAFTS as u8 {
        crate::desktop::drafts::upsert(
            BrowserDraft {
                id: Id::from_bytes([byte; 16]),
                revision: 1,
                volume: browser.volume.id,
                entry: browser.selected,
                ancestors: vec![],
                body: BrowserDraftBody::Kv(KvDraft {
                    creating: true,
                    original_key: None,
                    key_text: format!("filler-{byte}"),
                    value_text: "retained filler".into(),
                    key_encoding: DraftEncoding::Text,
                    value_encoding: DraftEncoding::Text,
                    base: None,
                    changed_during_edit: false,
                    uncertain: false,
                    read_unavailable: false,
                    complete_put: false,
                }),
            },
            cx,
        )
        .unwrap();
    }
    Id::from_bytes([1; 16])
}

#[gpui::test]
fn full_cache_keeps_retired_raw_input_hidden_until_capacity_and_read_return(
    cx: &mut gpui::TestAppContext,
) {
    // The 33rd raw draft must survive Read/backing/deletion loss without exposing canonical bytes.
    for loss in 0..3 {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let read = open_value(browser, &requests, cx);
            browser.receive_kv(
                reply(
                    read,
                    Ok(R::KvRead(Chunk {
                        root: Root { level: 0, digest: [3; 32] },
                        offset: 0,
                        total_bytes: 17,
                        bytes: b"private canonical".to_vec(),
                    })),
                ),
                cx,
            );
            let filler = fill_draft_capacity(browser, cx);
            browser.kv.value_encoding = Encoding::Hex;
            browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "aa f\nzz", cx));
            browser.kv.dirty = true;
            browser.kv.revision = 1;
            let mut unavailable = details(1, EntryAcl::Inherit);
            match loss {
                0 => unavailable.readable = false,
                1 => unavailable.backing_ready = false,
                _ => {
                    unavailable.entry.deleted = true;
                    unavailable.readable = false;
                    unavailable.retained_readable = false;
                }
            }
            browser.details = Some(Arc::new(unavailable));
            browser.update_kv_selection(cx);
            assert!(browser.retired_kv_draft_pending());
            assert!(!browser.kv_editor_open());
            assert!(browser.kv.chunk.is_none() && browser.kv.page.is_none());
            assert!(browser.kv.key.read(cx).text().is_empty());
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert!(!browser.retry_retired_kv_draft(cx));
            browser.update_kv_selection(cx);
            assert!(
                browser.retired_kv_draft_pending(),
                "repeated retirement keeps the one hidden slot"
            );
            assert_eq!(browser.local_draft_summaries(cx).len(), 32);
            crate::desktop::drafts::remove(filler, cx);
            browser.sync_kv_local_draft(cx);
            assert!(
                browser.retired_kv_draft_pending(),
                "cache notifications do not retry admission"
            );
            assert!(browser.retry_retired_kv_draft(cx));
            assert!(!browser.retired_kv_draft_pending());
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert_eq!(browser.local_draft_summaries(cx).len(), 32);
            browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
            let id = browser.kv_draft_for_key(b"value", cx).unwrap();
            assert!(browser.restore_kv_draft(id, cx));
            assert_eq!(browser.kv.value.read(cx).text(), "aa f\nzz");
            assert_eq!(browser.kv.value_encoding, Encoding::Hex);
            assert!(!browser.kv_can_save(cx));
        });
    }
}

#[gpui::test]
fn unreadable_retired_raw_input_can_be_discarded_without_evicting_other_drafts(
    cx: &mut gpui::TestAppContext,
) {
    // Capacity failure blocks retirement/navigation only until the user discards this hidden intent.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let read = open_value(browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        fill_draft_capacity(browser, cx);
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "unsaved", cx));
        browser.kv.dirty = true;
        let mut denied = details(1, EntryAcl::Inherit);
        denied.readable = false;
        browser.details = Some(Arc::new(denied));
        browser.update_kv_selection(cx);
        assert!(browser.retired_kv_draft_pending());
        assert!(!browser.prepare_entry_navigation(cx));
        assert!(browser.kv.value.read(cx).text().is_empty());
        browser.discard_retired_kv_draft(cx);
        assert!(!browser.has_kv_draft());
        assert!(!browser.kv_editor_open());
        assert!(browser.kv.chunk.is_none());
        assert_eq!(browser.local_draft_summaries(cx).len(), 32);
        assert!(browser.prepare_entry_navigation(cx));
    });
}

#[gpui::test]
fn raw_new_rows_survive_paging_and_retire_the_previous_read(cx: &mut gpui::TestAppContext) {
    use gpui::EntityInputHandler;
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser.kv.key_encoding = Encoding::Hex;
        browser.kv.value_encoding = Encoding::Hex;
        browser
    });
    let (key, value) = window
        .update(cx, |browser, _, _| (browser.kv.key.clone(), browser.kv.value.clone()))
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            key.update(cx, |editor, cx| editor.replace_text_in_range(Some(0..0), "0", window, cx));
            value.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..0), "aa f", window, cx)
            });
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |browser, _, cx| {
            let id = browser.kv.draft_id.unwrap();
            assert!(!browser.kv_can_save(cx));
            let stored = crate::desktop::drafts::get(id, cx).unwrap();
            let crate::store::browser_drafts::BrowserDraftBody::Kv(raw) = &stored.body else {
                panic!("KV draft")
            };
            assert_eq!(raw.key_text, "0");
            assert_eq!(raw.value_text, "aa f");
            assert!(raw.original_key.is_none());
            browser.select_kv_key(b"other".to_vec(), cx);
            let obsolete = data(&requests);
            browser.kv_page(Some(b"page".to_vec()), Some(Root { level: 0, digest: [1; 32] }), cx);
            assert!(browser.error.is_empty(), "{}", browser.error);
            let page = data(&requests);
            browser.receive_kv(
                reply(
                    obsolete,
                    Ok(R::KvRead(Chunk {
                        root: Root { level: 0, digest: [4; 32] },
                        offset: 0,
                        total_bytes: 6,
                        bytes: b"stale!".to_vec(),
                    })),
                ),
                cx,
            );
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert_eq!(browser.kv.pending.as_ref().unwrap().sequence, page.sequence);
            assert!(browser.restore_kv_draft(id, cx));
            assert_eq!(browser.kv.draft_id, Some(id));
            assert_eq!(browser.kv.key.read(cx).text(), "0");
            assert_eq!(browser.kv.value.read(cx).text(), "aa f");
            assert!(!browser.kv_can_save(cx));
            browser.new_kv_value(cx);
            browser.capture_kv_draft(cx).unwrap();
            assert_ne!(
                browser.kv.draft_id,
                Some(id),
                "empty new rows receive independent stable identities"
            );
            assert!(crate::desktop::drafts::get(id, cx).is_some());
        })
        .unwrap();
}

#[gpui::test]
fn cold_draft_pencils_transfer_list_reads_and_warm_restoration_retains_the_page(
    cx: &mut gpui::TestAppContext,
) {
    use crate::store::browser_drafts::{BrowserDraft, BrowserDraftBody, DraftEncoding, KvDraft};
    let page = Page {
        root: Some(Root { level: 0, digest: [9; 32] }),
        keys: [b"message".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|key| crate::store::key_value::Key {
                bytes: key.to_vec(),
                len_bytes: 3,
                preview: b"old".to_vec(),
            })
            .collect(),
        next: None,
    };
    let chunk = || Chunk {
        root: Root { level: 0, digest: [9; 32] },
        offset: 0,
        total_bytes: 9,
        bytes: b"canonical".to_vec(),
    };
    for creating in [false, true] {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let id = Id::from_bytes([if creating { 71 } else { 70 }; 16]);
            let key = if creating { "new key" } else { "message" };
            crate::desktop::drafts::upsert(
                BrowserDraft {
                    id,
                    revision: 1,
                    volume: volume(1).id,
                    entry: Some(entry(1).id),
                    ancestors: vec![],
                    body: BrowserDraftBody::Kv(KvDraft {
                        creating,
                        original_key: (!creating).then(|| b"message".to_vec()),
                        key_text: key.into(),
                        value_text: "aa f\nzz".into(),
                        key_encoding: DraftEncoding::Text,
                        value_encoding: DraftEncoding::Hex,
                        base: None,
                        changed_during_edit: false,
                        uncertain: false,
                        read_unavailable: false,
                        complete_put: false,
                    }),
                },
                cx,
            )
            .unwrap();
            assert!(browser.open_local_draft(id, cx));
            browser.receive(
                Reply::EntryDetails {
                    volume: volume(1).id,
                    details: details(1, EntryAcl::Inherit),
                },
                cx,
            );
            let obsolete = data(&requests);
            let current = data(&requests);
            assert!(matches!(obsolete.call.operation, O::KvKeys { .. }));
            assert_eq!(obsolete.call.operation, current.call.operation);
            assert_ne!(obsolete.owner, current.owner);
            assert_eq!(browser.kv.pending.as_ref().unwrap().sequence, current.sequence);
            assert!(!browser.kv.refresh_requested);
            assert_no_data(&requests);
            browser.receive_kv(reply(obsolete, Ok(R::KvKeys(page.clone()))), cx);
            assert!(browser.kv.page.is_none(), "the canceled owner cannot populate this view");
            assert_eq!(browser.kv.key.read(cx).text(), key);
            assert_eq!(browser.kv.value.read(cx).text(), "aa f\nzz");
            browser.receive_kv(reply(current, Ok(R::KvKeys(page.clone()))), cx);
            assert_eq!(browser.kv.page.as_deref(), Some(&page));
            if creating {
                assert_no_data(&requests);
            } else {
                let read = data(&requests);
                assert!(matches!(&read.call.operation, O::KvRead { key, .. } if key == b"message"));
                assert_eq!(read.owner, browser.kv.owner());
                assert_no_data(&requests);
                browser.receive_kv(reply(read, Ok(R::KvRead(chunk()))), cx);
            }
            let retained = browser.kv.page.clone().unwrap();
            assert!(browser.restore_kv_draft(id, cx));
            assert!(Arc::ptr_eq(browser.kv.page.as_ref().unwrap(), &retained));
            assert_eq!(browser.kv.key.read(cx).text(), key);
            assert_eq!(browser.kv.value.read(cx).text(), "aa f\nzz");
            if creating {
                assert_no_data(&requests);
            } else {
                let read = data(&requests);
                assert!(matches!(read.call.operation, O::KvRead { .. }));
                assert_no_data(&requests);
                browser.receive_kv(reply(read, Err("The value read failed".into())), cx);
                assert!(browser.kv.page.is_none() && browser.kv.pending.is_none());
                assert!(browser.restore_kv_draft(id, cx));
                let reload = data(&requests);
                assert!(matches!(reload.call.operation, O::KvKeys { after: None, root: None, .. }));
                assert_no_data(&requests);
                browser.receive_kv(reply(reload, Ok(R::KvKeys(page.clone()))), cx);
                let read = data(&requests);
                assert!(matches!(read.call.operation, O::KvRead { .. }));
                browser.receive_kv(reply(read, Ok(R::KvRead(chunk()))), cx);
            }
            assert_eq!(browser.kv.page.as_deref(), Some(&page));
            assert_eq!(browser.kv.key.read(cx).text(), key);
            assert_eq!(browser.kv.value.read(cx).text(), "aa f\nzz");
            assert_eq!(browser.kv.value_encoding, Encoding::Hex);
            assert!(browser.kv.dirty);
            assert_no_data(&requests);
        });
    }
}

#[gpui::test]
fn restored_kv_drafts_require_current_read_and_keep_current_write_limits(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser
            .kv
            .value
            .update(cx, |editor, cx| editor.acknowledge(None, "private unfinished value", cx));
        browser.kv.dirty = true;
        browser.capture_kv_draft(cx).unwrap();
        let id = browser.kv.draft_id.unwrap();
        let account = browser.local_draft_account();
        browser.kv = KeyValues::new(cx);
        browser.set_account(Id::from_bytes([99; 16]), None);
        assert!(!browser.local_draft_ready(cx));
        assert!(!browser.restore_kv_draft(id, cx));
        assert!(browser.kv.value.read(cx).text().is_empty());
        assert!(browser.kv_creating_drafts(cx).is_empty());
        browser.set_account(account, None);
        let mut denied = details(1, EntryAcl::Inherit);
        denied.readable = false;
        denied.writable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: denied }, cx);
        assert!(!browser.restore_kv_draft(id, cx));
        assert!(browser.kv.value.read(cx).text().is_empty());
        let mut readonly = details(1, EntryAcl::Inherit);
        readonly.writable = false;
        readonly.appendable = false;
        browser.receive(Reply::EntryDetails { volume: volume(1).id, details: readonly }, cx);
        assert!(browser.restore_kv_draft(id, cx));
        assert_eq!(browser.kv.value.read(cx).text(), "private unfinished value");
        assert!(!browser.kv_can_edit(cx));
        assert!(!browser.kv_can_save(cx));
        browser.discard_kv_draft(cx);
        assert!(crate::desktop::drafts::get(id, cx).is_none());
        assert!(browser.kv.value.read(cx).text().is_empty());
    });
}

#[gpui::test]
fn a_put_completed_offscreen_preserves_the_newer_raw_local_row(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "new-key", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "submitted", cx));
        browser.kv.dirty = true;
        browser.kv.revision = 1;
        browser.save_kv(cx);
        let put = data(&requests);
        let id = browser.kv.draft_id.unwrap();
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "newer\ntext", cx));
        browser.kv.revision = 2;
        browser.capture_kv_draft(cx).unwrap();
        browser.select_kv_key(b"other".to_vec(), cx);
        browser.receive_kv(reply(put, Ok(R::KvWritten)), cx);
        assert_eq!(browser.kv.selected.as_deref(), Some(b"other".as_slice()));
        let stored = crate::desktop::drafts::get(id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Kv(raw) = &stored.body else {
            panic!("KV draft")
        };
        assert_eq!(raw.value_text, "newer\ntext");
        assert_eq!(raw.original_key.as_deref(), Some(b"new-key".as_slice()));
        assert!(!raw.creating && !raw.uncertain);
        assert!(raw.complete_put);
        assert!(!crate::desktop::draft_publications::pending(id, cx));
    });
}

#[gpui::test]
fn navigating_during_composition_retains_raw_marked_text_without_publishing(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::EntityInputHandler;
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        browser
    });
    window
        .update(cx, |browser, window, cx| {
            browser.kv.value.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(
                    Some(0..3),
                    "日本\n語",
                    Some(4..4),
                    window,
                    cx,
                );
            });
            assert!(browser.kv_is_composing(cx));
            browser.new_kv_value(cx);
            assert!(browser.kv.creating);
            assert!(browser.kv.value.read(cx).text().is_empty());
            let id = browser.kv_draft_for_key(b"value", cx).unwrap();
            let stored = crate::desktop::drafts::get(id, cx).unwrap();
            let crate::store::browser_drafts::BrowserDraftBody::Kv(raw) = &stored.body else {
                panic!("KV draft")
            };
            assert_eq!(raw.value_text, "日本\n語");
            assert_no_data(&requests);
            assert!(browser.restore_kv_draft(id, cx));
            assert_eq!(browser.kv.value.read(cx).text(), "日本\n語");
            assert!(browser.kv.dirty);
            assert!(!browser.kv_is_composing(cx));
            assert!(matches!(data(&requests).call.operation, O::KvRead { .. }));
            assert_no_data(&requests);
        })
        .unwrap();
}

fn data(requests: &async_channel::Receiver<Message>) -> QueuedData {
    loop {
        match requests.try_recv().unwrap() {
            Message::Data(data) => return data,
            Message::Request(_) | Message::CopyComplete => {}
            Message::Changed(_) | Message::Fatal(_) | Message::Launch(_) => {
                panic!("unexpected observer")
            }
        }
    }
}
fn reply(queued: QueuedData, result: Result<R, String>) -> DataReply {
    DataReply {
        committed_root: None,
        owner: queued.owner,
        sequence: queued.sequence,
        volume: Id::from_bytes(queued.call.volume),
        entry: EntryId::from_bytes(queued.call.operation.entry_id().unwrap()).unwrap(),
        result: result.map_err(|message| crate::desktop::worker_data::DataFailure {
            status: Some(playbit_api::abi::syscall::Error::AccessDenied),
            message,
        }),
    }
}
fn open(browser: &mut Browser, cx: &mut Context<Browser>) {
    browser.select(entry(1).id, cx);
    let mut info = details(1, EntryAcl::Inherit);
    info.writable = true;
    browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: info }, cx);
}

fn open_value(
    browser: &mut Browser,
    requests: &async_channel::Receiver<Message>,
    cx: &mut Context<Browser>,
) -> QueuedData {
    open(browser, cx);
    browser.receive_kv(
        reply(data(requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
        cx,
    );
    browser.select_kv_key(b"value".to_vec(), cx);
    data(requests)
}

fn press_kv(window: gpui::WindowHandle<Browser>, key: &str, cx: &mut gpui::TestAppContext) {
    cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse(key).unwrap());
    cx.dispatch_key_up(window.into(), gpui::Keystroke::parse(key).unwrap());
    cx.run_until_parked();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
}

fn assert_kv_focus(
    window: gpui::WindowHandle<Browser>,
    expected: &str,
    cx: &mut gpui::TestAppContext,
) {
    use gpui::Focusable;
    let editor = window
        .update(cx, |browser, window, cx| {
            assert!(window.accessibility_error().is_none());
            match expected {
                "key" | "value" => {
                    let editor =
                        if expected == "key" { &browser.kv.key } else { &browser.kv.value };
                    assert!(editor.read(cx).focus_handle(cx).is_focused(window), "{expected}");
                    Some(editor.entity_id())
                }
                "key-encoding" | "value-encoding" => {
                    let group = if expected == "key-encoding" {
                        &browser.kv.key_representation
                    } else {
                        &browser.kv.value_representation
                    };
                    assert!(group.read(cx).focus_handle(cx).is_focused(window), "{expected}");
                    let id = gpui::accesskit::NodeId(group.entity_id().as_u64() * 2);
                    let (_, node) =
                        window.accessibility_nodes_for_test().find(|(n, _)| *n == id).unwrap();
                    assert_eq!(node.role(), gpui::accesskit::Role::RadioGroup);
                    assert!(!node.is_disabled());
                    assert!(
                        node.bounds()
                            .is_some_and(|bounds| bounds.width() > 0. && bounds.height() > 0.)
                    );
                    None
                }
                "kv-discard" | "kv-save" | "kv-reload" => {
                    assert!(browser.rendered_controls.contains(expected));
                    assert!(browser.controls[expected].read(cx).keyboard_focus_visible(window));
                    None
                }
                _ => panic!("unknown KV focus target"),
            }
        })
        .unwrap();
    if let Some(id) = editor {
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        // The test API retains static selectors; these bounded fixture names live for the test process.
        let surface = visual
            .debug_bounds(Box::leak(format!("text-editor-surface-{id}").into_boxed_str()))
            .unwrap();
        let ring = visual
            .debug_bounds(Box::leak(format!("text-editor-keyboard-focus-{id}").into_boxed_str()))
            .unwrap();
        assert_eq!(surface.left() - ring.left(), gpui::px(2.));
        assert_eq!(ring.right() - surface.right(), gpui::px(2.));
    }
}

#[gpui::test]
fn inspector_tab_cycle_orders_fields_groups_and_enabled_draft_actions(
    cx: &mut gpui::TestAppContext,
) {
    // Real key events must follow the editing journey rather than header/field paint order.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        window.activate_window();
        browser
    });
    cx.run_until_parked();
    for (key, expected) in [
        ("tab", ["value", "key-encoding", "value-encoding", "kv-discard", "key"]),
        ("shift-tab", ["kv-discard", "value-encoding", "key-encoding", "value", "key"]),
    ] {
        for expected in expected {
            press_kv(window, key, cx);
            assert_kv_focus(window, expected, cx);
        }
    }
    cx.simulate_input(window.into(), "new key");
    let revision = window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv_can_save(cx));
            browser.kv.revision
        })
        .unwrap();
    for (key, expected) in [
        ("tab", ["value", "key-encoding", "value-encoding", "kv-discard", "kv-save", "key"]),
        ("shift-tab", ["kv-save", "kv-discard", "value-encoding", "key-encoding", "value", "key"]),
    ] {
        for expected in expected {
            press_kv(window, key, cx);
            assert_kv_focus(window, expected, cx);
        }
    }
    window
        .update(cx, |browser, _, cx| {
            assert_eq!(browser.kv.key.read(cx).text(), "new key");
            assert_eq!(browser.kv.value.read(cx).text(), "");
            assert_eq!(browser.kv.revision, revision, "focus alone is not a draft edit");
            assert!(browser.kv.pending.is_none());
        })
        .unwrap();
    assert_no_data(&requests);
    window.update(cx, |browser, _, cx| browser.save_kv(cx)).unwrap();
    let put = data(&requests);
    assert!(matches!(&put.call.operation, O::KvPut { key, value, .. }
        if key == b"new key" && value.is_empty()));
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
    for key in ["tab", "shift-tab"] {
        for expected in ["value", "key"] {
            press_kv(window, key, cx);
            assert_kv_focus(window, expected, cx);
        }
    }
    assert_no_data(&requests);
}

#[gpui::test]
fn readonly_binary_fields_keep_visible_tab_focus_and_skip_unavailable_text_segments(
    cx: &mut gpui::TestAppContext,
) {
    // A readonly inspector still exposes two selectable fields and two single-stop mode groups.
    use gpui::Focusable;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.select_kv_key(vec![0xff], cx);
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 2,
                    bytes: vec![0x80, 0xfe],
                })),
            ),
            cx,
        );
        let mut details = (**browser.details.as_ref().unwrap()).clone();
        details.writable = false;
        browser.details = Some(Arc::new(details));
        browser.kv.key.read(cx).focus_handle(cx).focus(window);
        window.activate_window();
        browser
    });
    cx.run_until_parked();
    for (key, expected) in [
        ("tab", ["value", "key-encoding", "value-encoding", "key"]),
        ("shift-tab", ["value-encoding", "key-encoding", "value", "key"]),
    ] {
        for expected in expected {
            press_kv(window, key, cx);
            assert_kv_focus(window, expected, cx);
            if expected.ends_with("encoding") {
                press_kv(window, "left", cx);
                assert_kv_focus(window, expected, cx);
            }
        }
    }
    window
        .update(cx, |browser, window, cx| {
            assert_eq!(browser.kv.key_encoding, Encoding::Hex);
            assert_eq!(browser.kv.value_encoding, Encoding::Hex);
            assert_eq!(browser.kv.key.read(cx).text(), "ff");
            assert_eq!(browser.kv.value.read(cx).text(), "80fe");
            assert_eq!(browser.kv.revision, 0);
            let nodes =
                window.accessibility_nodes_for_test().map(|(_, node)| node).collect::<Vec<_>>();
            assert_eq!(
                nodes
                    .iter()
                    .filter(|node| node.role() == gpui::accesskit::Role::RadioGroup)
                    .count(),
                2
            );
            assert_eq!(
                nodes
                    .iter()
                    .filter(|node| node.role() == gpui::accesskit::Role::RadioButton
                        && node.label() == Some("Text")
                        && node.is_disabled())
                    .count(),
                2
            );
            let (key_id, key_text_id) =
                text_editor::accessibility_text_ids(browser.kv.key.entity_id());
            assert!(window.accessibility_nodes_for_test().any(|(id, node)| id == key_id
                && node.role() == gpui::accesskit::Role::TextInput
                && node.is_read_only()));
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(id, node)| id == key_text_id && node.value() == Some("ff"))
            );
            assert!(
                nodes.iter().any(|node| node.role() == gpui::accesskit::Role::MultilineTextInput
                    && node.value() == Some("80fe")
                    && node.is_read_only())
            );
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(text_editor::Copy), cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "ff"));
    cx.simulate_input(window.into(), "z");
    window.update(cx, |browser, _, cx| assert_eq!(browser.kv.key.read(cx).text(), "ff")).unwrap();
    assert_no_data(&requests);
    // Retired-read recovery remains keyboard reachable without admitting a write.
    window
        .update(cx, |browser, _, cx| {
            browser.kv.read_unavailable = true;
            cx.notify();
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
    for (key, expected) in [
        ("tab", ["value", "key-encoding", "value-encoding", "kv-reload", "key"]),
        ("shift-tab", ["kv-reload", "value-encoding", "key-encoding", "value", "key"]),
    ] {
        for expected in expected {
            press_kv(window, key, cx);
            assert_kv_focus(window, expected, cx);
        }
    }
    press_kv(window, "shift-tab", cx);
    assert_kv_focus(window, "kv-reload", cx);
    press_kv(window, "space", cx);
    let reload = data(&requests);
    assert!(matches!(reload.call.operation, O::KvKeys { after: None, root: None, .. }));
    window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv.read_unavailable);
            assert_eq!(browser.kv.key.read(cx).text(), "ff");
            assert_eq!(browser.kv.value.read(cx).text(), "80fe");
            assert_eq!(browser.kv.revision, 0);
        })
        .unwrap();
    assert_no_data(&requests);
}

#[gpui::test]
fn initial_keys_share_the_selection_delay_then_fill_the_table(cx: &mut gpui::TestAppContext) {
    // Metadata and the first key page share one delay; an unknown list has no empty column.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser
    });
    cx.run_until_parked();
    let pending = data(&requests);
    let loading_count = |cx: &mut gpui::TestAppContext| {
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                window
                    .accessibility_nodes_for_test()
                    .filter(|(_, node)| node.value() == Some("Loading..."))
                    .count()
            })
            .unwrap()
    };
    assert_eq!(loading_count(cx), 0);
    assert!(
        gpui::VisualTestContext::from_window(window.into(), cx).debug_bounds("kv-list").is_none()
    );
    cx.executor().advance_clock(std::time::Duration::from_millis(199));
    cx.run_until_parked();
    assert_eq!(loading_count(cx), 0);
    cx.executor().advance_clock(std::time::Duration::from_millis(1));
    cx.run_until_parked();
    window
        .update(cx, |browser, _, _| {
            assert!(browser.detail_loading.active, "entry read remains in flight");
            assert!(browser.detail_loading.visible, "the 200 ms notice fired");
            assert!(browser.detail_is_loading(), "keys remain unknown");
        })
        .unwrap();
    assert_eq!(loading_count(cx), 1);
    assert!(
        gpui::VisualTestContext::from_window(window.into(), cx).debug_bounds("kv-list").is_none()
    );
    window
        .update(cx, |browser, _, cx| {
            browser.receive_kv(
                reply(
                    pending,
                    Ok(R::KvKeys(Page {
                        root: Some(Root { level: 0, digest: [3; 32] }),
                        keys: vec![crate::store::key_value::Key {
                            bytes: b"a".to_vec(),
                            len_bytes: 4,
                            preview: vec![],
                        }],
                        next: None,
                    })),
                ),
                cx,
            );
            assert!(browser.detail_loading.timer.is_none());
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(loading_count(cx), 0);
    let list = gpui::VisualTestContext::from_window(window.into(), cx)
        .debug_bounds("kv-list")
        .expect("the ready table fills the lower content pane");
    assert!(list.size.width >= gpui::px(240.));
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    assert!(list.top() >= visual.debug_bounds("browser-upper").unwrap().bottom());
    assert!(visual.debug_bounds("table-key-header").is_some());
    assert!(visual.debug_bounds("table-value-header").is_some());
}

#[gpui::test]
fn key_list_scrolls_only_when_rows_overflow_at_each_zoom(cx: &mut gpui::TestAppContext) {
    cx.update(crate::ui::desktop::zoom::install);
    cx.set_auto_hide_scrollbars(false);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1800.), gpui::px(600.)));
    for scale in [10_000, 20_000] {
        visual.update(|_, cx| crate::ui::desktop::zoom::set_scale(scale, cx).unwrap());
        for count in [0, 3, 64, 0] {
            window
                .update(&mut visual, |browser, window, cx| {
                    crate::ui::desktop::zoom::apply(window, cx);
                    browser.kv.page = Some(Arc::new(Page {
                        root: None,
                        keys: (0..count)
                            .map(|index| crate::store::key_value::Key {
                                bytes: format!("key-{index:02}").into_bytes(),
                                len_bytes: 0,
                                preview: vec![],
                            })
                            .collect(),
                        next: None,
                    }));
                    cx.notify();
                })
                .unwrap();
            visual.run_until_parked();
            let handle = window
                .update(&mut visual, |browser, window, cx| {
                    assert!(browser.kv.creating, "the Entry inspector stays open");
                    let handle = browser.scrolling.records.read(cx).handle();
                    assert_key_list_scroll(&handle, window, count + 1, scale as f32 / 10_000.);
                    handle
                })
                .unwrap();
            if count == 64 {
                visual.simulate_event(gpui::ScrollWheelEvent {
                    position: handle.bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-40.))),
                    ..Default::default()
                });
                assert_eq!(handle.offset().y, px(-40.), "overflowing keys remain scrollable");
            } else {
                assert_eq!(handle.offset().y, px(0.), "shorter content clears the old offset");
            }
        }
    }
}

fn assert_key_list_scroll(handle: &gpui::ScrollHandle, window: &Window, count: usize, factor: f32) {
    let bounds = handle.bounds();
    assert!(bounds.size.height > px(0.));
    let maximum = (px((8. + count as f32 * 24.) * factor) - bounds.size.height).max(px(0.));
    assert_eq!(handle.max_offset().height, maximum);
    assert_eq!(handle.max_offset().width, px(0.));
    let has_scrollbar = window.accessibility_nodes_for_test().any(|(_, node)| {
        node.role() == gpui::accesskit::Role::ScrollBar
            && node.label() == Some("Vertical scroll")
            && node.bounds().is_some_and(|rail| {
                rail.x0 >= bounds.left().to_f64()
                    && rail.x1 <= bounds.right().to_f64()
                    && rail.y0 >= bounds.top().to_f64()
                    && rail.y1 <= bounds.bottom().to_f64()
            })
    });
    assert_eq!(has_scrollbar, maximum > px(0.), "only real overflow exposes a scrollbar");
}

#[gpui::test]
fn identical_metadata_refreshes_content_and_only_a_new_busy_state_notifies(
    cx: &mut gpui::TestAppContext,
) {
    // Equal metadata is not evidence that the separately replicated backing is unchanged.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        assert!(browser.kv.pending.is_none());
    });
    cx.run_until_parked();
    let (count, _subscription) = super::super::tests::notifications(&browser, cx);
    for expected in [1, 1] {
        browser.update(cx, |browser, cx| {
            let mut info = details(1, EntryAcl::Inherit);
            info.writable = true;
            browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: info }, cx);
            assert!(browser.kv.pending.is_some());
        });
        cx.run_until_parked();
        assert_eq!(count.get(), expected);
    }
    let queued = data(&requests);
    assert!(matches!(queued.call.operation, O::KvKeys { root: None, .. }));
    assert!(requests.try_recv().is_err());
    browser.update(cx, |browser, cx| {
        let mut denied = details(1, EntryAcl::Replace(Default::default()));
        denied.readable = false;
        browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: denied }, cx);
        browser.receive_kv(
            reply(queued, Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        assert!(browser.kv.page.is_none());
        assert!(browser.kv.pending.is_none());
    });
    cx.run_until_parked();
    assert_eq!(count.get(), 2);
}

#[test]
fn binary_hex_is_exact_and_bounded() {
    assert_eq!(parse_hex("00aAFF", 3, false).unwrap(), [0, 170, 255]);
    assert_eq!(hex(&[0, 170, 255]), "00aaff");
    for text in ["", "0", "00 ", "0x00", "é", "00000000"] {
        assert!(parse_hex(text, 3, false).is_err());
    }
    assert!(parse_hex("", 3, true).unwrap().is_empty());
    assert!(parse_hex(&"ff".repeat(4097), 4096, true).is_err());
}

#[test]
fn text_and_hex_representations_preserve_bytes_and_enforce_byte_limits() {
    // Single-line key text and compact hex preserve every key byte.
    let text = "Anne 🦀";
    assert_eq!(Encoding::for_bytes(text.as_bytes()), Encoding::Text);
    assert_eq!(Encoding::Text.display(text.as_bytes()).unwrap(), text);
    assert_eq!(Encoding::Text.parse("é", 2, false).unwrap(), "é".as_bytes());
    assert!(Encoding::Text.parse("é", 1, false).is_err());
    assert!(Encoding::Text.parse("", 2, false).is_err());
    assert_eq!(Encoding::Text.parse("", 2, true).unwrap(), Vec::<u8>::new());
    for bytes in [vec![0, 255], b"first\nsecond".to_vec(), (0..=255).collect()] {
        assert_eq!(Encoding::for_bytes(&bytes), Encoding::Hex);
        assert!(Encoding::Text.display(&bytes).is_err());
        let encoded = Encoding::Hex.display(&bytes).unwrap();
        assert_eq!(Encoding::Hex.parse(&encoded, bytes.len(), true).unwrap(), bytes);
    }
}

#[test]
fn binary_value_rows_and_whitespace_preserve_exact_bytes() {
    // Boundary-length rows and edited separators round-trip bytes without widening key syntax.
    let bytes: Vec<_> = (0..17).collect();
    for (length, expected) in [
        (15, "00010203 04050607 08090a0b 0c0d0e"),
        (16, "00010203 04050607 08090a0b 0c0d0e0f"),
        (17, "00010203 04050607 08090a0b 0c0d0e0f\n10"),
    ] {
        assert_eq!(hex_value(&bytes[..length]), expected);
        assert_eq!(parse_hex_value(expected, length).unwrap(), bytes[..length]);
        assert_eq!(Encoding::Hex.display_value(&bytes[..length]).unwrap(), expected);
    }
    assert_eq!(parse_hex_value(" \t00 01\n02\r03\x0c04 ", 5).unwrap(), [0, 1, 2, 3, 4]);
    assert_eq!(parse_hex_value("0 0 A\tF", 2).unwrap(), [0, 175]);
    assert!(parse_hex_value(" \t\r\n", 0).unwrap().is_empty());
    assert!(parse_hex_value("00 01", 1).is_err());
    for invalid in ["0", "00xz", "0x00", "00\u{00a0}ff", "00,ff"] {
        assert!(parse_hex_value(invalid, MAX_BYTES).is_err());
    }
    assert!(parse_hex_value(&" ".repeat(MAX_DISPLAY + 1), MAX_BYTES).is_err());
    assert_eq!(hex(&bytes), "000102030405060708090a0b0c0d0e0f10");
    assert!(parse_hex("00010203 04050607", 8, false).is_err());
}

#[test]
fn partial_hex_byte_identifies_only_a_supported_unfinished_byte() {
    // Underlines use original byte offsets; invalid input and either bound remain errors.
    for (text, range) in [("a", 0..1), (" \tA \n", 2..3), ("00 \n a \r", 5..6)] {
        assert_eq!(partial_hex_byte(text), Some(range));
    }
    for text in ["", " \t\r\n", "00", "a \nf", "g", "0x0", "é", "０", "0\u{a0}"] {
        assert_eq!(partial_hex_byte(text), None, "{text:?}");
    }
    let largest_partial = "f".repeat(MAX_BYTES * 2 - 1);
    assert_eq!(
        partial_hex_byte(&largest_partial),
        Some(largest_partial.len() - 1..largest_partial.len())
    );
    assert_eq!(partial_hex_byte(&format!("{largest_partial}ff")), None);
    let display_bound = format!("{}f", " ".repeat(MAX_DISPLAY - 1));
    assert_eq!(partial_hex_byte(&display_bound), Some(MAX_DISPLAY - 1..MAX_DISPLAY));
    assert_eq!(partial_hex_byte(&format!("{display_bound} ")), None);
}

#[gpui::test]
fn partial_hex_edit_keeps_the_viewport_and_size_until_a_complete_value_can_save(
    cx: &mut gpui::TestAppContext,
) {
    // Native edits keep unfinished bytes in place, then admit only complete supported pairs.
    use gpui::EntityInputHandler;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let bytes = vec![0xaa; 42];
    let initial = hex_value(&bytes);
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 42,
                    bytes: bytes.clone(),
                })),
            ),
            cx,
        );
        browser
    });
    cx.run_until_parked();
    let baseline = gpui::VisualTestContext::from_window(window.into(), cx)
        .debug_bounds("kv-value-field")
        .unwrap();
    let value = window.update(cx, |browser, _, _| browser.kv.value.clone()).unwrap();
    let replace = |text: &str, cx: &mut gpui::TestAppContext| {
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                value.update(cx, |editor, cx| {
                    let length = editor.text().encode_utf16().count();
                    editor.replace_text_in_range(Some(0..length), text, window, cx);
                });
            })
            .unwrap();
    };
    let partial = format!("{initial}f");
    replace(&partial, cx);
    window
        .update(cx, |browser, _, cx| {
            assert_eq!(browser.kv.value.read(cx).text(), partial);
            assert_eq!(partial.bytes().filter(u8::is_ascii_hexdigit).count(), 85);
            assert!(!browser.kv_can_save(cx));
            browser.save_kv(cx);
            assert!(browser.kv.pending.is_none());
            assert_no_data(&requests);
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
            assert!(window.accessibility_error().is_none());
            let (_, save) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .expect("the dirty value keeps its Save action");
            assert!(save.is_disabled());
            assert!(
                window.accessibility_nodes_for_test().any(|(_, node)| node.value() == Some("42 B"))
            );
        })
        .unwrap();
    assert_eq!(
        gpui::VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("kv-value-field")
            .unwrap(),
        baseline
    );
    // Other malformed values are still rejected rather than treated as an unfinished byte.
    for invalid in ["zz".to_owned(), "aa".repeat(MAX_BYTES + 1)] {
        replace(&invalid, cx);
        window
            .update(cx, |browser, _, cx| {
                assert_eq!(browser.kv.value.read(cx).text(), invalid);
                assert!(!browser.kv_can_save(cx));
                browser.save_kv(cx);
                assert!(browser.kv.pending.is_none());
                assert_no_data(&requests);
            })
            .unwrap();
    }
    let complete = format!("{partial}0");
    replace(&complete, cx);
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
            let (_, save) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .unwrap();
            assert!(!save.is_disabled());
            assert!(
                window.accessibility_nodes_for_test().any(|(_, node)| node.value() == Some("43 B"))
            );
        })
        .unwrap();
    window
        .update(cx, |browser, _, cx| {
            assert_eq!(browser.kv.value.read(cx).text(), complete);
            assert!(browser.kv_can_save(cx));
            browser.save_kv(cx);
            let put = data(&requests);
            let mut expected = bytes;
            expected.push(0xf0);
            assert!(matches!(&put.call.operation, O::KvPut { key, value, .. }
                if key == b"value" && value == &expected));
        })
        .unwrap();
}

#[gpui::test]
fn multiline_utf8_values_keep_newlines_through_modes_and_save(cx: &mut gpui::TestAppContext) {
    // Load and convert complete UTF-8 values, then inspect the exact bytes admitted by Save.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let first = open_value(browser, &requests, cx);
        let text = "Anne 🦀\nsecond line\n";
        browser.receive_kv(
            reply(
                first,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: text.len() as u64,
                    bytes: text.as_bytes().to_vec(),
                })),
            ),
            cx,
        );
        assert_eq!(Encoding::for_value(text.as_bytes(), false), Encoding::Text);
        assert_eq!(browser.kv.value_encoding, Encoding::Text);
        assert_eq!(browser.kv.value.read(cx).text(), text);
        assert!(browser.kv_text_available(DraftField::Value, cx));
        browser.set_kv_encoding(DraftField::Value, Encoding::Hex, cx);
        assert_eq!(browser.kv.value.read(cx).text(), hex_value(text.as_bytes()));
        browser.set_kv_encoding(DraftField::Value, Encoding::Text, cx);
        assert_eq!(browser.kv.value.read(cx).text(), text);
        let edited = "first\r\nsecond\n🦀";
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, edited, cx));
        browser.kv.dirty = true;
        browser.kv.revision = 1;
        browser.save_kv(cx);
        let saved = data(&requests);
        assert!(matches!(&saved.call.operation,
            O::KvPut { key, value, .. } if key == b"value" && value == edited.as_bytes()));
        browser.receive_kv(reply(saved, Ok(R::KvWritten)), cx);
        assert_eq!(browser.kv.value.read(cx).text(), edited);
        assert!(browser.kv.saved);
    });
}

#[gpui::test]
fn composing_a_value_blocks_representation_changes_and_canonical_mutations(
    cx: &mut gpui::TestAppContext,
) {
    // Native marked text has not delivered a browser draft yet; all routes must retain it.
    use gpui::{EntityInputHandler, Focusable};
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        browser
    });
    let value = window.update(cx, |browser, _, _| browser.kv.value.clone()).unwrap();
    let marked = gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            window.activate_window();
            value.read(cx).focus_handle(cx).focus(window);
            value.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(
                    Some(0..3),
                    "日本\n語",
                    Some(4..4),
                    window,
                    cx,
                );
                editor.marked_text_range(window, cx)
            })
        })
        .unwrap();
    assert_eq!(marked, Some(0..4));
    assert_no_data(&requests);
    let composing_revision = window.update(cx, |browser, _, _| browser.kv.revision).unwrap();
    for key in ["tab", "shift-tab"] {
        press_kv(window, key, cx);
        window
            .update(cx, |browser, window, cx| {
                assert!(value.read(cx).focus_handle(cx).is_focused(window));
                assert_eq!(browser.kv.revision, composing_revision);
                assert_eq!(value.read(cx).text(), "日本\n語");
            })
            .unwrap();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                value.update(cx, |editor, cx| {
                    assert_eq!(editor.marked_text_range(window, cx), marked)
                });
            })
            .unwrap();
        assert_no_data(&requests);
    }
    let (revision, discard) = window
        .update(cx, |browser, _, cx| {
            let revision = browser.kv.revision;
            assert!(!browser.kv.dirty, "composition is not a committed browser draft");
            assert!(browser.has_unsaved_changes(cx), "profile changes must retain marked text");
            assert!(browser.kv_is_composing(cx));
            browser.set_kv_encoding(DraftField::Value, Encoding::Hex, cx);
            assert_eq!(browser.kv.value_encoding, Encoding::Text);
            browser.delete_kv(b"value".to_vec(), cx);
            assert_eq!(browser.selected, Some(entry(1).id));
            assert_eq!(browser.kv.selected.as_deref(), Some(b"value".as_slice()));
            assert_eq!(browser.kv.value.entity_id(), value.entity_id());
            assert!(browser.kv.pending.is_none());
            // A preexisting dirty draft cannot be saved or discarded during IME either.
            browser.kv.dirty = true;
            browser.save_kv(cx);
            assert!(browser.kv.pending.is_none());
            let _ = browser.kv_actions(cx);
            (revision, browser.controls["kv-discard"].read(cx).popup_for_test().2)
        })
        .unwrap();
    cx.update(|cx| discard(0, cx));
    assert!(requests.try_recv().is_err(), "blocked actions cannot admit reads or mutations");
    window
        .update(cx, |browser, _, cx| {
            assert_eq!(browser.kv.revision, revision);
            assert!(browser.kv.dirty);
            assert_eq!(browser.kv.value.read(cx).text(), "日本\n語");
            assert!(browser.kv_is_composing(cx));
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                assert_eq!(editor.marked_text_range(window, cx), marked);
                editor.unmark_text(window, cx);
                assert!(!editor.is_composing());
            });
        })
        .unwrap();
    window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv.revision > revision);
            browser.set_kv_encoding(DraftField::Value, Encoding::Hex, cx);
            assert_eq!(browser.kv.value.read(cx).text(), hex_value("日本\n語".as_bytes()));
            assert_eq!(browser.kv.value_encoding, Encoding::Hex);
            assert!(browser.kv_can_save(cx));
            browser.save_kv(cx);
            let put = data(&requests);
            assert!(matches!(&put.call.operation, O::KvPut { key, value, .. }
                if key == b"value" && value == "日本\n語".as_bytes()));
        })
        .unwrap();
}

#[gpui::test]
fn captured_discard_cannot_retire_a_pending_save_or_its_newer_draft(cx: &mut gpui::TestAppContext) {
    // A callback captured while enabled revalidates admission before clearing local state.
    use gpui::EntityInputHandler;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        browser
    });
    let value = window.update(cx, |browser, _, _| browser.kv.value.clone()).unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..3), "first", window, cx)
            });
        })
        .unwrap();
    let (discard, save) = window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv_can_save(cx));
            let _ = browser.kv_actions(cx);
            (
                browser.controls["kv-discard"].read(cx).popup_for_test().2,
                browser.controls["kv-save"].read(cx).popup_for_test().2,
            )
        })
        .unwrap();
    cx.update(|cx| save(0, cx));
    let put = data(&requests);
    assert!(matches!(&put.call.operation, O::KvPut { value, .. } if value == b"first"));
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..5), "second", window, cx)
            });
        })
        .unwrap();
    let revision = window.update(cx, |browser, _, _| browser.kv.revision).unwrap();
    cx.update(|cx| discard(0, cx));
    let discard = window
        .update(cx, |browser, _, cx| {
            let pending = browser.kv.pending.as_ref().unwrap();
            assert_eq!(pending.sequence, put.sequence);
            assert!(matches!(&pending.call.operation, O::KvPut { value, .. } if value == b"first"));
            assert!(pending.revision < revision);
            assert_eq!(browser.kv.revision, revision);
            assert!(browser.kv.dirty);
            assert_eq!(browser.kv.value.entity_id(), value.entity_id());
            assert_eq!(browser.kv.value.read(cx).text(), "second");
            assert_no_data(&requests);
            browser.receive_kv(reply(put, Ok(R::KvWritten)), cx);
            assert!(browser.kv.pending.is_none());
            assert!(browser.kv.dirty);
            assert!(!browser.kv.saved);
            assert_eq!(browser.kv.value.read(cx).text(), "second");
            let _ = browser.kv_actions(cx);
            browser.controls["kv-discard"].read(cx).popup_for_test().2
        })
        .unwrap();
    cx.update(|cx| discard(0, cx));
    let read = data(&requests);
    assert!(matches!(&read.call.operation, O::KvRead { key, .. } if key == b"value"));
    window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv.dirty);
            browser.receive_kv(
                reply(
                    read,
                    Ok(R::KvRead(Chunk {
                        root: Root { level: 0, digest: [4; 32] },
                        offset: 0,
                        total_bytes: 5,
                        bytes: b"first".to_vec(),
                    })),
                ),
                cx,
            );
            assert_eq!(browser.kv.value.read(cx).text(), "first");
            browser.rendered_controls.clear();
            let _ = browser.kv_actions(cx);
            assert!(!browser.rendered_controls.contains("kv-save"));
            assert!(!browser.rendered_controls.contains("kv-discard"));
        })
        .unwrap();
}

#[gpui::test]
fn native_text_fields_keep_a_local_draft_until_explicit_utf8_save(cx: &mut gpui::TestAppContext) {
    // Exercise real text input and Save admission, including the user's previously blocked input.
    use gpui::Focusable;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let mut info = details(1, EntryAcl::Inherit);
        info.writable = true;
        browser.selected = Some(info.entry.id);
        browser.details = Some(Arc::new(info));
        browser.overview = false;
        browser.new_kv_value(cx);
        browser
    });
    cx.run_until_parked();
    while requests.try_recv().is_ok() {}
    window
        .update(cx, |browser, window, cx| {
            browser.kv.key.read(cx).focus_handle(cx).focus(window);
        })
        .unwrap();
    cx.simulate_input(window.into(), "a");
    window
        .update(cx, |browser, window, cx| {
            browser.kv.value.read(cx).focus_handle(cx).focus(window);
        })
        .unwrap();
    cx.simulate_input(window.into(), "anne");
    window
        .update(cx, |browser, _, cx| {
            assert_eq!(browser.kv.key_encoding, Encoding::Text);
            assert_eq!(browser.kv.value_encoding, Encoding::Text);
            assert!(browser.kv_can_save(cx));
            assert!(browser.prepare_navigation(cx));
            assert!(browser.kv.draft_id.is_some());
            assert!(
                browser.error.is_empty(),
                "ordinary draft guidance cannot trigger the modal alert path"
            );
            assert_eq!(browser.kv.key.read(cx).text(), "a");
            assert_eq!(browser.kv.value.read(cx).text(), "anne");
            browser.save_kv(cx);
            assert!(browser.hint.is_empty());
            let saved = data(&requests);
            assert!(matches!(&saved.call.operation, O::KvPut { key, value, .. }
            if key == b"a" && value == b"anne"));
            browser.receive_kv(reply(saved, Ok(R::KvWritten)), cx);
            assert!(browser.kv.saved);
            assert!(!browser.kv.dirty);
            let root = Root { level: 0, digest: [3; 32] };
            browser.receive_kv(
                reply(
                    data(&requests),
                    Ok(R::KvKeys(Page {
                        root: Some(root),
                        keys: vec![crate::store::key_value::Key {
                            bytes: b"a".to_vec(),
                            len_bytes: 5,
                            preview: vec![],
                        }],
                        next: None,
                    })),
                ),
                cx,
            );
            browser.receive_kv(
                reply(
                    data(&requests),
                    Ok(R::KvRead(Chunk {
                        root,
                        offset: 0,
                        total_bytes: 5,
                        bytes: b"annie".to_vec(),
                    })),
                ),
                cx,
            );
            assert_eq!(
                browser.kv.value.read(cx).text(),
                "annie",
                "the committed editor revision must accept a later authoritative value"
            );
        })
        .unwrap();
}

#[gpui::test]
fn binary_values_keep_hex_and_mode_changes_preserve_the_supplied_draft(
    cx: &mut gpui::TestAppContext,
) {
    // Read binary bytes, reject a lossy Text switch, and preserve identity across a key mode switch.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.select_kv_key(b"person".to_vec(), cx);
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: vec![0, 255, 10],
                })),
            ),
            cx,
        );
        assert_eq!(browser.kv.key_encoding, Encoding::Text);
        assert_eq!(browser.kv.value_encoding, Encoding::Hex);
        assert_eq!(browser.kv.value.read(cx).text(), "00ff0a");
        assert!(!browser.kv_text_available(DraftField::Value, cx));
        browser.set_kv_encoding(DraftField::Value, Encoding::Text, cx);
        assert_eq!(browser.kv.value_encoding, Encoding::Hex);
        assert_eq!(browser.kv.value.read(cx).text(), "00ff0a");
        assert!(!browser.hint.is_empty());
        assert!(browser.error.is_empty());
        browser.kv.dirty = true;
        let before = browser.kv_draft_bytes(cx).unwrap();
        browser.set_kv_encoding(DraftField::Key, Encoding::Hex, cx);
        assert_eq!(browser.kv_draft_bytes(cx).unwrap(), before);
        assert!(browser.kv.dirty);
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "616e6e65", cx));
        assert!(browser.kv_text_available(DraftField::Value, cx));
        browser.set_kv_encoding(DraftField::Value, Encoding::Text, cx);
        assert_eq!(browser.kv.value.read(cx).text(), "anne");
        assert_eq!(browser.kv_draft_bytes(cx).unwrap(), (b"person".to_vec(), b"anne".to_vec()));
        assert_eq!(browser.kv.selected.as_deref(), Some(b"person".as_slice()));
    });
}

#[gpui::test]
fn invalid_entry_names_keep_the_editor_and_report_inline(cx: &mut gpui::TestAppContext) {
    // Expected name validation preserves the draft without entering the native error alert path.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        while requests.try_recv().is_ok() {}
        browser.start_rename(entry(1).id, "Notes", cx);
        browser.commit_rename(entry(1).id, "", cx);
        assert!(browser.renaming.is_some());
        assert!(!browser.hint.is_empty());
        assert!(browser.error.is_empty());
        assert!(requests.try_recv().is_err(), "invalid input cannot submit a rename");
    });
}

#[gpui::test]
fn old_view_and_wrong_full_entry_cannot_replace_the_current_kv_page(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let old = data(&requests);
        browser.select(entry(2).id, cx);
        open(browser, cx); // Leave and return to retire the earlier view identity.
        let current = data(&requests);
        let page = || R::KvKeys(Page { root: None, keys: vec![], next: None });
        browser.receive_kv(reply(old, Ok(page())), cx);
        assert!(browser.kv.page.is_none());
        let mut current = reply(current, Ok(page()));
        current.entry = entry(2).id;
        browser.receive_kv(current, cx);
        assert!(browser.kv.page.is_none());
        assert!(browser.kv.pending.is_some());
    });
}

#[gpui::test]
fn failed_save_preserves_draft_and_receipt_cannot_clear_a_newer_edit(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser.kv.key_encoding = Encoding::Hex;
        browser.kv.value_encoding = Encoding::Hex;
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "00ff", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "aabb", cx));
        browser.kv.dirty = true;
        browser.kv.revision = 1;
        browser.save_kv(cx);
        let failed = data(&requests);
        assert_eq!(
            failed.call.operation,
            O::KvPut {
                entry_id: *entry(1).id.as_bytes(),
                key: vec![0, 255],
                value: vec![170, 187]
            }
        );
        browser.receive_kv(reply(failed, Err("Access denied".into())), cx);
        assert!(browser.kv.dirty);
        assert_eq!(browser.kv.value.read(cx).text(), "aabb");
        let failed_id = browser.kv.draft_id.unwrap();
        browser.new_kv_value(cx);
        assert!(browser.kv.creating);
        assert!(browser.kv.key.read(cx).text().is_empty());
        assert!(
            browser.restore_kv_draft(failed_id, cx),
            "the failed row remains separately reachable"
        );
        assert_eq!(browser.kv.key.read(cx).text(), "00ff");
        assert_eq!(browser.kv.value.read(cx).text(), "aabb");
        assert!(browser.prepare_navigation(cx));
        assert_eq!(browser.kv.value.read(cx).text(), "aabb");
        // A retained failed draft prevents automatic reads from taking admission
        // ahead of the next explicit supplied write.
        while let Ok(message) = requests.try_recv() {
            assert!(!matches!(message, Message::Data(_)));
        }
        browser.save_kv(cx);
        let saved = data(&requests);
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "ccdd", cx));
        browser.kv.revision = 2;
        browser.receive_kv(reply(saved, Ok(R::KvWritten)), cx);
        assert!(browser.kv.dirty);
        assert!(!browser.kv.saved);
        assert_eq!(browser.kv.value.read(cx).text(), "ccdd");
        assert!(!browser.kv.creating);
        assert!(browser.kv.chunk.is_none(), "a Put receipt must not fabricate a snapshot root");
        assert!(browser.kv_can_edit(cx), "the complete local value remains editable");
        assert!(browser.kv_can_save(cx), "the retained newer value can be saved");
        browser.save_kv(cx);
        assert!(matches!(data(&requests).call.operation, O::KvPut { key, value, .. }
            if key == [0, 255] && value == [204, 221]));
    });
}

#[gpui::test]
fn background_refresh_yields_to_a_dirty_draft_without_adding_an_exchange(
    cx: &mut gpui::TestAppContext,
) {
    // Repeated backing notices may coalesce, but cannot perpetually occupy the
    // sole exchange while a supplied draft is ready for an explicit Save.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let root = Root { level: 0, digest: [3; 32] };
        let page = || R::KvKeys(Page { root: Some(root), keys: vec![], next: None });
        browser.receive_kv(reply(data(&requests), Ok(page())), cx);
        browser.select_kv_key(vec![0, 1], cx);
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvRead(Chunk { root, offset: 0, total_bytes: 1, bytes: vec![170] })),
            ),
            cx,
        );
        let mut info = details(1, EntryAcl::Inherit);
        info.writable = true;
        browser
            .receive(Reply::EntryDetails { volume: browser.volume.id, details: info.clone() }, cx);
        let refreshing = data(&requests);
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "bb", cx));
        browser.kv.dirty = true;
        browser.kv.revision = 1;
        for _ in 0..64 {
            browser.receive(
                Reply::EntryDetails { volume: browser.volume.id, details: info.clone() },
                cx,
            );
        }
        browser.receive_kv(reply(refreshing, Ok(page())), cx);
        assert!(browser.kv.pending.is_none());
        assert!(browser.kv.refresh_requested);
        assert!(browser.kv_can_save(cx));
        assert_eq!(browser.kv.value.read(cx).text(), "bb");
        while let Ok(message) = requests.try_recv() {
            assert!(!matches!(message, Message::Data(_)));
        }
        browser.save_kv(cx);
        let saving = data(&requests);
        assert!(matches!(saving.call.operation,
            O::KvPut { ref key, ref value, .. } if key == &[0, 1] && value == &[187]));
        browser.receive_kv(reply(saving, Ok(R::KvWritten)), cx);
        assert!(browser.kv.saved);
        assert!(!browser.kv.dirty);
        assert!(matches!(data(&requests).call.operation, O::KvKeys { .. }));
    });
}

#[gpui::test]
fn root_only_changes_do_not_warn_that_the_selected_value_changed(cx: &mut gpui::TestAppContext) {
    // A sibling key changes the database root without changing this key. A
    // changed supplied range does warn, and neither reply replaces the draft.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let root = Root { level: 0, digest: [3; 32] };
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvKeys(Page { root: Some(root), keys: vec![], next: None })),
            ),
            cx,
        );
        browser.select_kv_key(vec![0, 1], cx);
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvRead(Chunk { root, offset: 0, total_bytes: 1, bytes: vec![170] })),
            ),
            cx,
        );
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "cc", cx));
        browser.kv.dirty = true;
        for (digest, bytes, changed) in [(4, vec![170], false), (5, vec![187], true)] {
            browser.read_kv();
            browser.receive_kv(
                reply(
                    data(&requests),
                    Ok(R::KvRead(Chunk {
                        root: Root { level: 0, digest: [digest; 32] },
                        offset: 0,
                        total_bytes: 1,
                        bytes,
                    })),
                ),
                cx,
            );
            assert_eq!(browser.kv.changed_during_edit, changed);
            assert_eq!(browser.kv.value.read(cx).text(), "cc");
            assert!(browser.kv.pending.is_none());
        }
        let mut denied = details(1, EntryAcl::Inherit);
        denied.readable = false;
        browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: denied }, cx);
        assert!(!browser.kv.dirty);
        assert!(browser.kv.chunk.is_none());
        assert!(browser.kv.value.read(cx).text().is_empty());
    });
}

#[gpui::test]
fn larger_value_preview_is_read_only_and_current_withdrawal_retires_its_content(
    cx: &mut gpui::TestAppContext,
) {
    // An oversized preview stays read-only and cannot outlive current read authorization.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let root = Root { level: 0, digest: [3; 32] };
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvKeys(Page { root: Some(root), keys: vec![], next: Some(vec![0, 1]) })),
            ),
            cx,
        );
        browser.select_kv_key(vec![0, 255], cx);
        let read = data(&requests);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root,
                    offset: 0,
                    total_bytes: (MAX_BYTES * 2) as u64,
                    bytes: vec![170; MAX_BYTES],
                })),
            ),
            cx,
        );
        browser.kv.dirty = true;
        assert!(!browser.kv_can_save(cx));
        assert!(!browser.kv_can_edit(cx));
        assert_eq!(browser.kv.value.read(cx).text(), hex_value(&vec![170; MAX_BYTES]));
        assert_no_data(&requests);
        browser.read_kv();
        let late = data(&requests);
        let mut denied = details(1, EntryAcl::Inherit);
        denied.readable = false;
        browser.receive(Reply::EntryDetails { volume: browser.volume.id, details: denied }, cx);
        assert!(browser.kv.chunk.is_none());
        assert!(browser.kv.page.is_none());
        assert!(!browser.kv.dirty);
        assert!(browser.kv.value.read(cx).text().is_empty());
        browser.receive_kv(
            reply(
                late,
                Ok(R::KvRead(Chunk {
                    root,
                    offset: 0,
                    total_bytes: (MAX_BYTES * 2) as u64,
                    bytes: vec![187; MAX_BYTES],
                })),
            ),
            cx,
        );
        assert!(browser.kv.chunk.is_none());
        assert!(browser.kv.value.read(cx).text().is_empty());
        assert!(!browser.kv_can_edit(cx));
        assert_no_data(&requests);
    });
}

#[gpui::test]
fn append_controls_create_new_keys_and_keep_failed_drafts(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let mut info = details(1, EntryAcl::Inherit);
        info.appendable = true;
        browser.details = Some(Arc::new(info));
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.content_header(true, cx);
        assert!(browser.kv_can_add(cx), "Append permits the heading’s direct new-entry action");
        browser.new_kv_value(cx);
        browser.kv.key_encoding = Encoding::Hex;
        browser.kv.value_encoding = Encoding::Hex;
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "00ff", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "aabb", cx));
        browser.kv.dirty = true;
        assert!(browser.kv_can_save(cx));
        browser.save_kv(cx);
        assert!(!browser.kv_can_edit(cx));
        browser.receive_kv(reply(data(&requests), Err("Access denied".into())), cx);
        assert!(browser.kv.creating);
        assert!(browser.kv.dirty);
        assert_eq!(browser.kv.value.read(cx).text(), "aabb");
        browser.save_kv(cx);
        browser.receive_kv(reply(data(&requests), Ok(R::KvWritten)), cx);
        assert!(!browser.kv.creating);
        assert!(browser.kv.complete_put);
        assert!(!browser.kv_can_edit(cx), "a complete Put receipt cannot grant Append overwrite");
        assert!(!browser.kv_can_save(cx));
        // An existing complete value still cannot be replaced with Append.
        browser.kv.pending = None;
        browser.kv.creating = false;
        browser.kv.chunk = Some(Arc::new(Chunk {
            root: Root { level: 0, digest: [3; 32] },
            offset: 0,
            total_bytes: 2,
            bytes: vec![170, 187],
        }));
        assert!(!browser.kv_can_edit(cx));
        assert!(!browser.kv_can_save(cx));
    });
}

#[gpui::test]
fn deleting_a_key_prevents_new_edits_until_its_selection_is_retired(cx: &mut gpui::TestAppContext) {
    // A completed Delete cannot leave an editable selection that recreates the removed key.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.kv.selected = Some(vec![0, 255]);
        browser.kv.chunk = Some(Arc::new(Chunk {
            root: Root { level: 0, digest: [3; 32] },
            offset: 0,
            total_bytes: 2,
            bytes: vec![170, 187],
        }));
        assert!(browser.kv_can_edit(cx));
        assert!(
            browser.submit_kv(O::KvDelete { entry_id: *entry(1).id.as_bytes(), key: vec![0, 255] })
        );
        assert!(!browser.kv_can_edit(cx));
        browser.receive_kv(reply(data(&requests), Ok(R::KvDeleted)), cx);
        assert!(browser.kv.selected.is_none());
        assert!(browser.kv.creating);
        assert!(!browser.kv.dirty);
    });
}

#[gpui::test]
fn opening_a_key_reads_one_current_preview_independently_of_the_list_root(
    cx: &mut gpui::TestAppContext,
) {
    // Opening a listed key admits one current bounded read instead of pinning its older list root.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let listed = Root { level: 0, digest: [3; 32] };
        let current = Root { level: 0, digest: [4; 32] };
        let key = vec![0, 255];
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvKeys(Page {
                    root: Some(listed),
                    keys: vec![crate::store::key_value::Key {
                        bytes: key.clone(),
                        len_bytes: (MAX_BYTES * 2) as u64,
                        preview: vec![],
                    }],
                    next: None,
                })),
            ),
            cx,
        );
        browser.select_kv_key(key, cx);
        let first = data(&requests);
        assert!(matches!(first.call.operation, O::KvRead { root: None, offset: 0, .. }));
        browser.receive_kv(
            reply(
                first,
                Ok(R::KvRead(Chunk {
                    root: current,
                    total_bytes: (MAX_BYTES * 2) as u64,
                    offset: 0,
                    bytes: vec![170; MAX_BYTES],
                })),
            ),
            cx,
        );
        assert_eq!(browser.kv.chunk.as_ref().unwrap().root, current);
        assert_eq!(browser.kv.chunk.as_ref().unwrap().bytes.len(), MAX_BYTES);
        assert_eq!(browser.kv.value.read(cx).text(), hex_value(&vec![170; MAX_BYTES]));
        assert!(browser.kv.pending.is_none());
        assert!(!browser.kv_can_edit(cx), "larger stored values exceed the supported write limit");
        assert!(!browser.kv_can_save(cx));
        assert_no_data(&requests);
    });
}

#[gpui::test]
fn maximum_writable_values_display_convert_and_save_complete_bytes(cx: &mut gpui::TestAppContext) {
    // Maximum values retain every byte through conversion, Save and the parent AX text area.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    for binary in [false, true] {
        let (send, requests, _) = worker_queue::channel();
        let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
        let (value_id, expected) = window
            .update(cx, |browser, _, cx| {
                let first = open_value(browser, &requests, cx);
                assert!(matches!(first.call.operation,
                O::KvRead { offset: 0, root: None, limit, .. } if limit as usize == MAX_BYTES));
                let bytes: Vec<u8> = if binary {
                    (0..MAX_BYTES).map(|index| (index % 256) as u8).collect()
                } else {
                    vec![b'a'; MAX_BYTES]
                };
                let root = Root { level: 0, digest: [3; 32] };
                browser.receive_kv(
                    reply(
                        first,
                        Ok(R::KvRead(Chunk {
                            root,
                            offset: 0,
                            total_bytes: bytes.len() as u64,
                            bytes: bytes.clone(),
                        })),
                    ),
                    cx,
                );
                let encoding = if binary { Encoding::Hex } else { Encoding::Text };
                assert_eq!(browser.kv.value_encoding, encoding);
                assert_eq!(
                    browser.kv.value.read(cx).text(),
                    encoding.display_value(&bytes).unwrap()
                );
                assert!(browser.kv_can_edit(cx));
                browser.set_kv_encoding(DraftField::Value, Encoding::Hex, cx);
                assert_eq!(browser.kv.value.read(cx).text(), hex_value(&bytes));
                browser.set_kv_encoding(DraftField::Value, Encoding::Text, cx);
                assert_eq!(browser.kv.value_encoding, encoding);
                assert_eq!(
                    browser.kv.value.read(cx).text(),
                    encoding.display_value(&bytes).unwrap()
                );

                let mut edited = bytes;
                *edited.last_mut().unwrap() ^= 1;
                let mut oversized = edited.clone();
                oversized.push(b'a');
                let text = encoding.display_value(&oversized).unwrap();
                assert!(encoding.parse_value(&text, MAX_BYTES).is_err());
                if !binary {
                    browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, &text, cx));
                }
                browser.kv.dirty = true;
                browser.kv.revision = 1;
                if !binary {
                    assert!(
                        !browser.kv_can_save(cx),
                        "the display bound cannot widen the write ABI"
                    );
                    browser.save_kv(cx);
                    assert_no_data(&requests);
                }
                let text = encoding.display_value(&edited).unwrap();
                browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, &text, cx));
                assert!(browser.kv_can_save(cx));
                browser.save_kv(cx);
                let saved = data(&requests);
                assert!(matches!(&saved.call.operation,
                O::KvPut { key, value, .. } if key == b"value" && value == &edited));
                browser.receive_kv(reply(saved, Ok(R::KvWritten)), cx);
                assert!(browser.kv.saved);
                assert!(!browser.kv.dirty);
                (browser.kv.value.entity_id(), browser.kv.value.read(cx).text().to_owned())
            })
            .unwrap();
        if binary {
            assert_eq!(expected.len(), MAX_DISPLAY);
        }
        cx.run_until_parked();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                assert!(window.accessibility_error().is_none());
                let parent_id = text_editor::accessibility_text_ids(value_id).0;
                let (_, node) = window
                    .accessibility_nodes_for_test()
                    .find(|(id, _)| *id == parent_id)
                    .expect("the value editor publishes its own accessible parent");
                assert_eq!(node.role(), gpui::accesskit::Role::MultilineTextInput);
                assert!(!node.is_read_only());
                assert_eq!(node.live(), None);
                assert_eq!(node.value(), Some(expected.as_str()));
            })
            .unwrap();
    }
}

#[gpui::test]
fn large_stored_values_display_one_bounded_read_only_preview(cx: &mut gpui::TestAppContext) {
    // Bounded previews expose their complete visible bytes on the read-only parent AX text area.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let mut split_utf8 = vec![b'a'; MAX_BYTES];
    *split_utf8.last_mut().unwrap() = 0xc3;
    for (total_bytes, bytes, encoding) in [
        (MAX_BYTES + 1, vec![b'a'; MAX_BYTES], Encoding::Text),
        (MAX_STORED_BYTES, vec![170; MAX_BYTES], Encoding::Hex),
        (MAX_STORED_BYTES, split_utf8, Encoding::Text),
    ] {
        let (send, requests, _) = worker_queue::channel();
        let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
        let (value_id, expected) = window
            .update(cx, |browser, _, cx| {
                let first = open_value(browser, &requests, cx);
                assert!(matches!(first.call.operation,
                O::KvRead { offset: 0, root: None, limit, .. } if limit as usize == MAX_BYTES));
                let root = Root { level: 0, digest: [3; 32] };
                browser.receive_kv(
                    reply(
                        first,
                        Ok(R::KvRead(Chunk {
                            root,
                            offset: 0,
                            total_bytes: total_bytes as u64,
                            bytes: bytes.clone(),
                        })),
                    ),
                    cx,
                );
                let preview = browser.kv.chunk.as_ref().unwrap();
                assert_eq!(preview.total_bytes, total_bytes as u64);
                assert_eq!(preview.bytes, bytes);
                assert_eq!(browser.kv.value_encoding, encoding);
                assert_eq!(
                    browser.kv.value.read(cx).text(),
                    encoding.display_preview(browser.kv.chunk.as_ref().unwrap()).unwrap()
                );
                assert!(browser.kv.value.read(cx).text().len() <= MAX_DISPLAY);
                let visible = browser.kv.value.read(cx).text().to_owned();
                browser.set_kv_encoding(DraftField::Value, Encoding::Hex, cx);
                assert_eq!(browser.kv.value.read(cx).text(), hex_value(&bytes));
                if encoding == Encoding::Text {
                    assert!(browser.kv_text_available(DraftField::Value, cx));
                    browser.set_kv_encoding(DraftField::Value, Encoding::Text, cx);
                    assert_eq!(browser.kv.value.read(cx).text(), visible);
                    assert_eq!(browser.kv.chunk.as_ref().unwrap().bytes, bytes);
                }
                assert!(browser.kv.pending.is_none());
                assert!(!browser.kv_can_edit(cx));
                assert!(!browser.kv_can_save(cx));
                assert_no_data(&requests);
                (browser.kv.value.entity_id(), browser.kv.value.read(cx).text().to_owned())
            })
            .unwrap();
        cx.run_until_parked();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                assert!(window.accessibility_error().is_none());
                let parent_id = text_editor::accessibility_text_ids(value_id).0;
                let (_, node) = window
                    .accessibility_nodes_for_test()
                    .find(|(id, _)| *id == parent_id)
                    .expect("the preview editor publishes its own accessible parent");
                assert_eq!(node.role(), gpui::accesskit::Role::MultilineTextInput);
                assert!(node.is_read_only());
                assert_eq!(node.live(), None);
                assert_eq!(node.value(), Some(expected.as_str()));
            })
            .unwrap();
    }
}

#[gpui::test]
fn repeated_write_only_metadata_keeps_supplied_drafts(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        let write_only = || {
            let mut info = details(1, EntryAcl::Inherit);
            info.readable = false;
            info.writable = true;
            info
        };
        browser
            .receive(Reply::EntryDetails { volume: browser.volume.id, details: write_only() }, cx);
        browser.new_kv_value(cx);
        browser.kv.key_encoding = Encoding::Hex;
        browser.kv.value_encoding = Encoding::Hex;
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "00ff", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "aabb", cx));
        browser.kv.dirty = true;
        let owner = browser.kv.owner();
        browser
            .receive(Reply::EntryDetails { volume: browser.volume.id, details: write_only() }, cx);
        assert_eq!(browser.kv.owner(), owner);
        assert!(browser.kv_can_save(cx));
        assert_eq!(browser.kv.value.read(cx).text(), "aabb");
    });
}

#[gpui::test]
fn a_new_value_focused_before_paint_reveals_the_lower_editor_without_panning_folders(
    cx: &mut gpui::TestAppContext,
) {
    // Deliver selection authority and start an entry before one paint; real editor focus wins.
    use gpui::Focusable;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        browser.selected = Some(entry(2).id);
        browser.fixture_namespace(
            [(1, "Attachments", None), (2, "2026", Some(1)), (3, "Notes", Some(2))]
                .map(|(id, name, parent)| crate::store::volume_index::EntrySummary {
                    id: entry(id).id,
                    parent: parent.map(|id| entry(id).id),
                    name: name.into(),
                    kind: if id == 3 {
                        crate::store::EntryType::KvDb
                    } else {
                        crate::store::EntryType::Directory
                    },
                    protocol: (id == 3).then_some(DatabaseProtocol::Kvdb),
                    deleted: false,
                    name_conflict: false,
                })
                .to_vec(),
        );
        window.activate_window();
        browser
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(500.), px(600.)));
    draw_kv_focus_frame(&mut visual);
    let (columns, folders, folder_offset) = window
        .update(&mut visual, |browser, _, cx| {
            browser.select(entry(3).id, cx);
            let mut current = details(3, EntryAcl::Inherit);
            current.entry.info.name = "Notes".into();
            current.entry.info.namespace.parent = Some(entry(2).id);
            current.writable = true;
            browser
                .receive(Reply::EntryDetails { volume: browser.volume.id, details: current }, cx);
            browser.new_kv_value(cx);
            assert!(browser.kv.focus_key && browser.kv.creating);
            let folders = browser.scrolling.columns.read(cx).handle();
            let offset = folders.offset();
            (browser.scrolling.content_columns.read(cx).handle(), folders, offset)
        })
        .unwrap();
    draw_kv_focus_frame(&mut visual);
    assert_full_kv_inspector(&mut visual, &columns);
    assert_eq!(folders.offset(), folder_offset, "lower editor focus preserves folder panning");
    let field = visual.debug_bounds("kv-key-field").unwrap();
    assert!(field.left() >= columns.bounds().left());
    assert!(field.right() <= columns.bounds().right());
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            assert!(browser.kv.creating && browser.kv.key.read(cx).text().is_empty());
        })
        .unwrap();
}

#[gpui::test]
fn focused_kv_fields_reveal_the_lower_editor_once_without_committing_drafts(
    cx: &mut gpui::TestAppContext,
) {
    // Use the real + action, wheel events and native IME to distinguish logical focus changes
    // from rerenders and window activation while checking the complete 320-dp lower editor.
    use gpui::{EntityInputHandler, Focusable};
    cx.update(text_editor::install_keybindings);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        window.activate_window();
        browser
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(500.), px(600.)));
    draw_kv_focus_frame(&mut visual);
    let (columns, folders, folder_offset, activate) = window
        .update(&mut visual, |browser, window, cx| {
            let columns = browser.scrolling.content_columns.read(cx).handle();
            let folders = browser.scrolling.columns.read(cx).handle();
            let folder_offset = folders.offset();
            assert!(columns.max_offset().width > px(0.));
            assert_eq!(columns.offset().x, px(0.), "the inspector starts partly offscreen");
            browser.controls["kv-add"].read(cx).focus(window);
            (
                columns,
                folders,
                folder_offset,
                browser.controls["kv-add"].read(cx).popup_for_test().2,
            )
        })
        .unwrap();
    visual.update(|_, cx| activate(0, cx));
    draw_kv_focus_frame(&mut visual);
    assert_full_kv_inspector(&mut visual, &columns);
    visual.simulate_input("draft key");
    draw_kv_focus_frame(&mut visual);
    let revision = window
        .update(&mut visual, |browser, window, cx| {
            assert!(browser.kv.creating);
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(browser.kv_draft_bytes(cx).unwrap(), (b"draft key".to_vec(), vec![]));
            browser.kv.revision
        })
        .unwrap();
    let manual_offset = scroll_kv_inspector_away(&mut visual, &columns);
    visual.deactivate_window();
    window
        .update(&mut visual, |browser, window, cx| {
            window.activate_window();
            cx.notify();
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(browser.kv.revision, revision);
            assert_eq!(browser.kv_draft_bytes(cx).unwrap(), (b"draft key".to_vec(), vec![]));
        })
        .unwrap();
    draw_kv_focus_frame(&mut visual);
    assert_eq!(columns.offset(), manual_offset, "reactivation does not repeat focus reveal");
    let value = window
        .update(&mut visual, |browser, window, cx| {
            browser.kv.value.read(cx).focus_handle(cx).focus(window);
            browser.kv.value.clone()
        })
        .unwrap();
    draw_kv_focus_frame(&mut visual);
    assert_full_kv_inspector(&mut visual, &columns);
    let marked = visual.update(|window, cx| {
        value.update(cx, |editor, cx| {
            editor.replace_and_mark_text_in_range(None, "日本\n語", Some(4..4), window, cx);
            editor.marked_text_range(window, cx)
        })
    });
    assert_eq!(marked, Some(0..4));
    draw_kv_focus_frame(&mut visual);
    let manual_offset = scroll_kv_inspector_away(&mut visual, &columns);
    visual.deactivate_window();
    visual.update(|window, _| window.activate_window());
    draw_kv_focus_frame(&mut visual);
    assert_eq!(columns.offset(), manual_offset);
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(browser.kv.value.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(browser.kv.revision, revision, "revealing and scrolling do not commit IME");
            assert_eq!(browser.kv.key.read(cx).text(), "draft key");
            assert_eq!(browser.kv.value.read(cx).text(), "日本\n語");
            assert!(browser.kv.pending.is_none());
        })
        .unwrap();
    visual.update(|window, cx| {
        value.update(cx, |editor, cx| {
            assert_eq!(editor.marked_text_range(window, cx), marked);
            editor.unmark_text(window, cx);
        });
    });
    let readonly_revision = window
        .update(&mut visual, |browser, window, cx| {
            let mut details = (**browser.details.as_ref().unwrap()).clone();
            details.writable = false;
            browser.details = Some(Arc::new(details));
            browser.kv.key.read(cx).focus_handle(cx).focus(window);
            cx.notify();
            browser.kv.revision
        })
        .unwrap();
    assert!(readonly_revision > revision, "only the explicit unmark commits the draft");
    draw_kv_focus_frame(&mut visual);
    scroll_kv_inspector_away(&mut visual, &columns);
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(!browser.kv_can_edit(cx));
            browser.kv.value.read(cx).focus_handle(cx).focus(window);
        })
        .unwrap();
    draw_kv_focus_frame(&mut visual);
    assert_full_kv_inspector(&mut visual, &columns);
    visual.simulate_keystrokes("cmd-a cmd-c");
    visual.run_until_parked();
    visual.update(|_, cx| {
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some("日本\n語".into()));
    });
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(browser.kv.value.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(browser.kv.revision, readonly_revision);
            assert_eq!(
                browser.kv_draft_bytes(cx).unwrap(),
                (b"draft key".to_vec(), "日本\n語".as_bytes().to_vec())
            );
        })
        .unwrap();
    assert_eq!(folders.offset(), folder_offset, "editor focus and IME do not pan the upper pane");
    assert_no_data(&requests);
}

fn draw_kv_focus_frame(visual: &mut gpui::VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });
    visual.run_until_parked();
}

fn assert_full_kv_inspector(visual: &mut gpui::VisualTestContext, columns: &gpui::ScrollHandle) {
    let inspector = visual.debug_bounds("kv-editor").unwrap();
    let upper = visual.debug_bounds("browser-upper").unwrap();
    assert!(inspector.right() - px(320.) >= columns.bounds().left() - px(1.));
    assert!(inspector.right() <= columns.bounds().right(), "the whole inspector must be visible");
    assert!(inspector.top() >= upper.bottom(), "the key editor stays in the lower pane");
}

fn scroll_kv_inspector_away(
    visual: &mut gpui::VisualTestContext,
    columns: &gpui::ScrollHandle,
) -> gpui::Point<gpui::Pixels> {
    let before = columns.offset();
    visual.simulate_event(gpui::ScrollWheelEvent {
        position: gpui::point(
            columns.bounds().right() - px(20.),
            columns.bounds().top() + px(100.),
        ),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(120.), px(0.))),
        ..Default::default()
    });
    let offset = columns.offset();
    assert!(offset.x > before.x, "a real wheel event moves the inspector out of view");
    draw_kv_focus_frame(visual);
    assert_eq!(columns.offset(), offset, "ordinary rerenders preserve manual scrolling");
    assert!(visual.debug_bounds("kv-editor").unwrap().right() > columns.bounds().right());
    offset
}

#[gpui::test]
fn heading_add_opens_a_focused_entry_inspector(cx: &mut gpui::TestAppContext) {
    // A pristine entry can be canceled; typing only its value keeps geometry and Save admission stable.
    use gpui::{EntityInputHandler, Focusable};
    cx.update(crate::ui::desktop::zoom::install);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser
    });
    cx.run_until_parked();
    let page = window.update(cx, |browser, _, _| browser.kv.page.clone().unwrap()).unwrap();
    let activate = window
        .update(cx, |browser, _, cx| browser.controls["kv-add"].read(cx).popup_for_test().2)
        .unwrap();
    cx.update(|cx| activate(0, cx));
    cx.run_until_parked();
    let discard = window
        .update(cx, |browser, window, cx| {
            assert!(browser.kv.creating);
            assert!(!browser.kv.dirty);
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            let (_, save) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .unwrap();
            assert!(save.is_disabled());
            let (_, discard) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Discard Changes"))
                .unwrap();
            assert!(!discard.is_disabled());
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.value() == Some("Entry"))
            );
            assert_eq!(
                window
                    .accessibility_nodes_for_test()
                    .filter(|(_, node)| node.role() == gpui::accesskit::Role::RadioGroup)
                    .count(),
                2
            );
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.role() == gpui::accesskit::Role::MultilineTextInput)
            );
            browser.controls["kv-discard"].read(cx).popup_for_test().2
        })
        .unwrap();
    cx.update(|cx| discard(0, cx));
    window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv_editor_open());
            assert!(!browser.kv.dirty);
            assert!(Arc::ptr_eq(browser.kv.page.as_ref().unwrap(), &page));
            if browser.kv.pending.is_some() {
                let refresh = data(&requests);
                assert!(matches!(refresh.call.operation, O::KvKeys { .. }));
                browser.receive_kv(reply(refresh, Ok(R::KvKeys((*page).clone()))), cx);
            }
            assert_no_data(&requests);
        })
        .unwrap();
    cx.update(|cx| activate(0, cx));
    cx.run_until_parked();
    let baseline = gpui::VisualTestContext::from_window(window.into(), cx)
        .debug_bounds("kv-value-field")
        .unwrap();
    let value = window.update(cx, |browser, _, _| browser.kv.value.clone()).unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..0), "a value without a key", window, cx);
            });
            let _ = window.draw(cx);
            let (_, save) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .unwrap();
            assert!(save.is_disabled());
        })
        .unwrap();
    assert_eq!(
        gpui::VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("kv-value-field")
            .unwrap(),
        baseline
    );
    let key = window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv.dirty);
            assert!(!browser.kv_can_save(cx));
            browser.save_kv(cx);
            assert_no_data(&requests);
            browser.kv.key.clone()
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            key.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..0), "name", window, cx);
            });
            let _ = window.draw(cx);
            let (_, save) = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .unwrap();
            assert!(!save.is_disabled());
        })
        .unwrap();
    window.update(cx, |browser, _, cx| assert!(browser.kv_can_save(cx))).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1800.), gpui::px(1100.)));
    for scale in [10_000, 20_000] {
        visual.update(|_, cx| crate::ui::desktop::zoom::set_scale(scale, cx).unwrap());
        window
            .update(&mut visual, |_, window, cx| {
                crate::ui::desktop::zoom::apply(window, cx);
                window.activate_window();
                cx.notify();
            })
            .unwrap();
        visual.run_until_parked();
        let viewport = visual.debug_bounds("kv-editor").unwrap();
        let lower = visual.debug_bounds("browser-lower").unwrap();
        let surface = visual.debug_bounds("kv-key-field").unwrap();
        let focus_stroke = gpui::px(2. * scale as f32 / 10_000.);
        assert!(surface.top() - focus_stroke >= lower.top());
        assert!(surface.left() - focus_stroke >= viewport.left());
        assert!(surface.right() + focus_stroke <= viewport.right());
    }
}

#[gpui::test]
fn store_selection_becomes_secondary_while_inspecting_a_key(cx: &mut gpui::TestAppContext) {
    // The selected key owns the active column; its store remains a selected ancestor.
    use gpui::Focusable;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let root = Root { level: 0, digest: [3; 32] };
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        browser.fixture_catalog(EntryPage {
            entries: vec![crate::store::volume_index::EntrySummary {
                id: entry(1).id,
                parent: None,
                name: "notes".into(),
                kind: crate::store::EntryType::KvDb,
                protocol: Some(DatabaseProtocol::Kvdb),
                deleted: false,
                name_conflict: false,
            }],
            next: None,
        });
        open(&mut browser, cx);
        browser.receive_kv(
            reply(
                data(&requests),
                Ok(R::KvKeys(Page {
                    root: Some(root),
                    keys: vec![crate::store::key_value::Key {
                        bytes: b"seed".to_vec(),
                        len_bytes: 1,
                        preview: vec![0x9a],
                    }],
                    next: None,
                })),
            ),
            cx,
        );
        browser
    });
    window.update(cx, |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    let assert_store = |browser: &Browser, window: &Window, cx: &App, primary: bool| {
        let control = browser.controls[&format!("entry-{}", entry(1).id)].read(cx);
        let mut surface = control.navigation_surface_for_test(window);
        let active = primary && window.is_window_active();
        let foreground = if active {
            rgba(0xffffffff)
        } else if primary {
            rgba(0x000000cc)
        } else {
            rgba(0x000000ff)
        };
        assert_eq!(
            surface.style().text.as_ref().and_then(|text| text.color),
            Some(foreground.into())
        );
        assert_eq!(
            surface.style().background,
            Some(rgba(if active { 0x000000b3 } else { 0x0000001a }).into())
        );
        assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
            node.label() == Some("notes") && node.is_selected() == Some(true)
        }));
    };
    window.update(cx, |browser, window, cx| assert_store(browser, window, cx, true)).unwrap();
    let position = window
        .update(cx, |_, window, _| {
            let node = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("seed"))
                .unwrap()
                .1;
            let bounds = node.bounds().unwrap();
            gpui::point(gpui::px((bounds.x0 + 20.) as f32), gpui::px((bounds.y0 + 12.) as f32))
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.deactivate_window();
    window
        .update(&mut visual, |browser, window, cx| {
            assert_store(browser, window, cx, true);
            window.activate_window();
        })
        .unwrap();
    visual.run_until_parked();
    visual.simulate_click(position, Default::default());
    window
        .update(&mut visual, |browser, window, cx| {
            assert_eq!(browser.kv.selected.as_deref(), Some(b"seed".as_slice()));
            assert_store(browser, window, cx, false);
            browser.receive_kv(
                reply(
                    data(&requests),
                    Ok(R::KvRead(Chunk { root, offset: 0, total_bytes: 1, bytes: vec![0x9a] })),
                ),
                cx,
            );
        })
        .unwrap();
    visual.run_until_parked();
    window
        .update(&mut visual, |browser, window, cx| {
            let key = &browser.controls["kv-key-73656564"];
            key.read(cx).focus(window);
            key.update(cx, |key, cx| {
                assert!(key.show_keyboard_focus_if_focused(window, cx));
                let mut surface = key.navigation_surface_for_test(window);
                assert_eq!(surface.style().background, Some(rgba(0x000000b3).into()));
                assert_eq!(
                    surface.style().text.as_ref().and_then(|text| text.color),
                    Some(gpui::white())
                );
            });
        })
        .unwrap();
    visual.deactivate_window();
    window
        .update(&mut visual, |browser, window, cx| {
            assert_store(browser, window, cx, false);
            let key = browser.controls["kv-key-73656564"].read(cx);
            assert!(key.focused(window));
            assert!(!key.keyboard_focus_visible(window));
            window.activate_window();
            browser.new_kv_value(cx);
            cx.notify();
        })
        .unwrap();
    visual.run_until_parked();
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(browser.kv.creating);
            assert_store(browser, window, cx, false);
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            let page = browser.kv.page.clone().unwrap();
            browser.select(entry(1).id, cx);
            assert!(Arc::ptr_eq(browser.kv.page.as_ref().unwrap(), &page));
            cx.notify();
        })
        .unwrap();
    visual.run_until_parked();
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(!browser.kv_editor_open());
            assert_store(browser, window, cx, true);
        })
        .unwrap();
}

#[test]
fn table_previews_distinguish_text_binary_and_truncated_utf8() {
    // Saved and draft captions remain single-line without altering UTF-8 or binary input.
    use crate::store::{browser_drafts::DraftEncoding, key_value::PREVIEW_BYTES};
    assert_eq!(layout::preview_label(b"auto", 4), ("auto".into(), false));
    assert_eq!(layout::preview_label(&[0x9a, 0xa0, 0xc7, 0xf3], 4), ("0x9aa0c7f3".into(), true));
    assert_eq!(layout::preview_label(&[b'a', 0xc3], 3), ("a…".into(), false));
    assert_eq!(layout::preview_label(&[], 0), (String::new(), false));
    assert_eq!(
        layout::draft_preview_label("café 雨", DraftEncoding::Text),
        ("café 雨".into(), false)
    );
    for separator in ["\n", "\r\n", "\t", "\u{85}", "\u{2028}", "\u{2029}"] {
        let text = format!("First line{separator}Second café 雨");
        let expected = (format!("0x{}", hex(text.as_bytes())), true);
        assert_eq!(layout::preview_label(text.as_bytes(), text.len() as u64), expected);
        assert_eq!(layout::draft_preview_label(&text, DraftEncoding::Text), expected);
        let raw = format!("aa{separator}f zz");
        assert_eq!(
            layout::draft_preview_label(&raw, DraftEncoding::Hex),
            (format!("aa{}f zz", " ".repeat(separator.chars().count())), true)
        );
    }
    let text = format!("{}🦀tail", "a".repeat(PREVIEW_BYTES - 1));
    assert_eq!(
        layout::draft_preview_label(&text, DraftEncoding::Text),
        (format!("{}…", "a".repeat(PREVIEW_BYTES - 1)), false)
    );
    assert_eq!(
        layout::draft_preview_label(
            &format!("\n{}tail", "a".repeat(PREVIEW_BYTES)),
            DraftEncoding::Text
        ),
        (format!("0x0a{}…", "61".repeat(PREVIEW_BYTES - 1)), true)
    );
    assert_eq!(
        layout::draft_preview_label(
            &format!("{}aa", "\n".repeat(PREVIEW_BYTES)),
            DraftEncoding::Hex
        ),
        (" ".repeat(PREVIEW_BYTES), true)
    );
}

#[gpui::test]
fn creating_value_preview_keeps_editor_bytes_and_size(cx: &mut gpui::TestAppContext) {
    // Render a multiline draft in both modes, then inspect the exact Save request.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .expect("load table font");
    let text = "First line\nSecond café 雨";
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        open(&mut browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "greeting", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, text, cx));
        browser.kv.dirty = true;
        browser.did_change_draft(cx);
        browser
    });
    for encoding in [Encoding::Text, Encoding::Hex, Encoding::Text] {
        window
            .update(cx, |browser, _, cx| browser.set_kv_encoding(DraftField::Value, encoding, cx))
            .expect("change draft display mode");
        cx.run_until_parked();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
            })
            .expect("draw the draft row");
        window
            .update(cx, |browser, window, cx| {
                let editor = encoding.display_value(text.as_bytes()).expect("valid UTF-8 value");
                assert_eq!(browser.kv.value.read(cx).text(), editor);
                let expected = if encoding == Encoding::Text {
                    format!("Value: 0x{}", hex(text.as_bytes()))
                } else {
                    format!("Value: {}", editor.replace('\n', " "))
                };
                let (_, row) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| {
                        node.label() == Some("greeting") && node.description().is_some()
                    })
                    .expect("rendered draft row");
                assert_eq!(row.description(), Some(expected.as_str()));
                assert_eq!(row.bounds().expect("draft row bounds").height(), 24.);
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.value() == Some("27 B"))
                );
                assert!(browser.kv_can_save(cx));
                assert_no_data(&requests);
            })
            .expect("render draft without changing its bytes");
    }
    window
        .update(cx, |browser, _, cx| {
            browser.save_kv(cx);
            let put = data(&requests);
            assert!(matches!(&put.call.operation, O::KvPut { key, value, .. }
                if key == b"greeting" && value == text.as_bytes()));
        })
        .expect("save exact multiline draft");
}

#[gpui::test]
fn returning_to_store_reissues_a_saved_values_in_flight_table_refresh(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        browser.new_kv_value(cx);
        browser.kv.key.update(cx, |editor, cx| editor.acknowledge(None, "new", cx));
        browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "value", cx));
        browser.kv.dirty = true;
        browser.save_kv(cx);
        browser.receive_kv(reply(data(&requests), Ok(R::KvWritten)), cx);
        let obsolete = data(&requests);
        assert!(matches!(obsolete.call.operation, O::KvKeys { .. }));
        browser.select(entry(1).id, cx);
        let current = data(&requests);
        assert_ne!(obsolete.owner, current.owner);
        assert!(matches!(current.call.operation, O::KvKeys { .. }));
        let refreshed = || {
            R::KvKeys(Page {
                root: Some(Root { level: 0, digest: [9; 32] }),
                keys: vec![crate::store::key_value::Key {
                    bytes: b"new".to_vec(),
                    len_bytes: 5,
                    preview: b"value".to_vec(),
                }],
                next: None,
            })
        };
        browser.receive_kv(reply(obsolete, Ok(refreshed())), cx);
        assert!(browser.kv.page.as_ref().unwrap().keys.is_empty());
        browser.receive_kv(reply(current, Ok(refreshed())), cx);
        assert!(!browser.kv_editor_open());
        let key = &browser.kv.page.as_ref().unwrap().keys[0];
        assert_eq!(key.bytes, b"new");
        assert_eq!(key.preview, b"value");
        assert!(browser.kv.pending.is_none());
    });
}

#[gpui::test]
fn permission_drafts_allow_key_navigation_and_keep_their_own_discard(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let toggle = browser.update(cx, |browser, cx| {
        open(browser, cx);
        browser.receive_kv(
            reply(data(&requests), Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        let details = browser.details.clone().unwrap();
        browser.permissions_panel(&details, true, cx);
        browser.controls["custom-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| toggle(0, cx));
    let discard = browser.update(cx, |browser, cx| {
        assert!(browser.dirty);
        browser.new_kv_value(cx);
        assert!(browser.kv_editor_open());
        let permissions = browser.permission_draft_id.unwrap();
        assert!(browser.local_draft_snapshot(permissions, cx).is_some());
        browser.select_kv_key(b"key".to_vec(), cx);
        assert_eq!(browser.kv.selected.as_deref(), Some(b"key".as_slice()));
        assert_eq!(browser.permission_draft_id, Some(permissions));
        let details = browser.details.clone().unwrap();
        browser.permissions_panel(&details, true, cx);
        assert!(browser.rendered_controls.contains("save-permissions"));
        assert!(browser.rendered_controls.contains("discard-permissions"));
        browser.controls["discard-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| discard(0, cx));
    browser.update(cx, |browser, _| {
        assert!(!browser.dirty);
        assert!(browser.kv_editor_open());
        assert_eq!(browser.kv.selected.as_deref(), Some(b"key".as_slice()));
    });
}

#[gpui::test]
fn external_entry_deletion_retains_a_dirty_value_until_explicit_discard(
    cx: &mut gpui::TestAppContext,
) {
    // Render deleted metadata after real input, retain the readonly draft, and click its visible Discard.
    use gpui::EntityInputHandler;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        browser
    });
    let (key, value) = window
        .update(cx, |browser, _, _| (browser.kv.key.clone(), browser.kv.value.clone()))
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..3), "unsaved\nvalue", window, cx)
            });
        })
        .unwrap();
    window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv.dirty);
            assert!(browser.kv_can_save(cx));
            let revision = browser.kv.revision;
            let mut deleted = browser.details.as_deref().unwrap().clone();
            deleted.entry.deleted = true;
            deleted.retained_readable = true;
            browser
                .receive(Reply::EntryDetails { volume: browser.volume.id, details: deleted }, cx);
            assert_eq!(browser.selected, Some(entry(1).id));
            assert_eq!(browser.kv.selected.as_deref(), Some(b"value".as_slice()));
            assert_eq!(browser.kv.key.entity_id(), key.entity_id());
            assert_eq!(browser.kv.value.entity_id(), value.entity_id());
            assert_eq!(browser.kv.key.read(cx).text(), "value");
            assert_eq!(browser.kv.value.read(cx).text(), "unsaved\nvalue");
            assert_eq!(browser.kv.revision, revision);
            assert!(browser.kv.dirty);
            let details = browser.details.as_ref().unwrap();
            assert!(details.entry.deleted && details.retained_readable);
            assert!(!details.readable && !details.writable && !details.manage_permissions);
            assert!(!browser.kv_can_save(cx));
            browser.save_kv(cx);
            assert!(browser.kv.pending.is_none());
            assert_no_data(&requests);
            assert!(!browser.overview);
            assert!(browser.kv_editor_open());
        })
        .unwrap();
    let discard_position = gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
            assert!(window.accessibility_error().is_none());
            let editor = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.role() == gpui::accesskit::Role::MultilineTextInput)
                .expect("the retained dirty value remains in the rendered inspector")
                .1;
            assert_eq!(editor.value(), Some("unsaved\nvalue"));
            assert!(editor.is_read_only());
            let bounds = editor.bounds().expect("the retained value has visible geometry");
            assert!(bounds.width() > 0. && bounds.height() > 0.);
            let save = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Save Changes"))
                .expect("the dirty inspector retains Save admission state")
                .1;
            assert!(save.is_disabled());
            let discard = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Discard Changes"))
                .expect("the retained draft has a reachable Discard action")
                .1;
            assert!(!discard.is_disabled());
            let bounds = discard.bounds().expect("the rendered Discard has pointer bounds");
            assert!(bounds.width() > 0. && bounds.height() > 0.);
            gpui::point(
                gpui::px(((bounds.x0 + bounds.x1) / 2.) as f32),
                gpui::px(((bounds.y0 + bounds.y1) / 2.) as f32),
            )
        })
        .unwrap();
    {
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(discard_position, Default::default());
        visual.run_until_parked();
    }
    window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv.dirty);
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert!(!browser.has_unsaved_changes(cx));
            assert!(browser.show_volume(cx));
            assert!(browser.selected.is_none());
            assert!(browser.details.is_none());
            assert_no_data(&requests);
        })
        .unwrap();
}

#[gpui::test]
fn deletion_during_native_composition_retains_the_visible_draft_after_readonly_unmark(
    cx: &mut gpui::TestAppContext,
) {
    // Render a pristine composing value readonly, then unmark and click Discard without losing its bytes.
    use gpui::EntityInputHandler;
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: 3,
                    bytes: b"old".to_vec(),
                })),
            ),
            cx,
        );
        browser
    });
    let value = window.update(cx, |browser, _, _| browser.kv.value.clone()).unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            value.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(
                    Some(0..3),
                    "日本\n語",
                    Some(4..4),
                    window,
                    cx,
                );
            });
        })
        .unwrap();
    let revision = window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv.dirty, "native composition has not delivered a draft");
            assert!(browser.kv_is_composing(cx));
            let previous = browser.kv.revision;
            let mut deleted = browser.details.as_deref().unwrap().clone();
            deleted.entry.deleted = true;
            deleted.retained_readable = true;
            browser
                .receive(Reply::EntryDetails { volume: browser.volume.id, details: deleted }, cx);
            assert!(browser.kv.dirty);
            assert!(browser.kv.revision > previous);
            assert_eq!(browser.kv.value.entity_id(), value.entity_id());
            assert!(browser.kv_is_composing(cx), "retention must not commit native composition");
            assert_no_data(&requests);
            browser.kv.revision
        })
        .unwrap();
    gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
            assert!(window.accessibility_error().is_none());
            let editor = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.role() == gpui::accesskit::Role::MultilineTextInput)
                .expect("the composing value remains visible after deletion")
                .1;
            assert_eq!(editor.value(), Some("日本\n語"));
            assert!(editor.is_read_only());
            value.update(cx, |editor, cx| {
                assert_eq!(editor.marked_text_range(window, cx), Some(0..4));
                editor.unmark_text(window, cx);
                assert!(!editor.is_composing());
            });
        })
        .unwrap();
    window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv.dirty);
            assert!(!browser.kv_is_composing(cx));
            assert_eq!(
                browser.kv.revision, revision,
                "readonly unmark cannot deliver another draft"
            );
            assert_eq!(browser.kv.value.read(cx).text(), "日本\n語");
            assert!(!browser.kv_can_save(cx));
            assert_no_data(&requests);
        })
        .unwrap();
    let position = gpui::AnyWindowHandle::from(window)
        .update(cx, |_, window, cx| {
            let _ = window.draw(cx);
            let discard = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some("Discard Changes"))
                .expect("unmark leaves an explicit Discard action")
                .1;
            assert!(!discard.is_disabled());
            let bounds = discard.bounds().expect("visible Discard pointer bounds");
            gpui::point(
                gpui::px(((bounds.x0 + bounds.x1) / 2.) as f32),
                gpui::px(((bounds.y0 + bounds.y1) / 2.) as f32),
            )
        })
        .unwrap();
    {
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(position, Default::default());
        visual.run_until_parked();
    }
    window
        .update(cx, |browser, _, cx| {
            assert!(!browser.kv.dirty);
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert!(browser.show_volume(cx));
            assert!(browser.selected.is_none());
            assert_no_data(&requests);
        })
        .unwrap();
}

fn assert_no_data(requests: &async_channel::Receiver<Message>) {
    while let Ok(message) = requests.try_recv() {
        assert!(!matches!(message, Message::Data(_)));
    }
}

fn unavailable(queued: QueuedData) -> DataReply {
    let mut failed = reply(queued, Err("The read was retired".into()));
    failed.result.as_mut().unwrap_err().status =
        Some(playbit_api::abi::syscall::Error::Unavailable);
    failed
}

#[gpui::test]
fn conflicting_value_read_offers_inline_reload_and_then_loads_current_bytes(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        let first = open_value(browser, &requests, cx);
        let mut failed = reply(first, Err("The store is busy".into()));
        failed.result.as_mut().unwrap_err().status =
            Some(playbit_api::abi::syscall::Error::Conflict);
        browser.receive_kv(failed, cx);
        assert!(browser.kv.chunk.is_none());
        assert!(browser.kv.value.read(cx).text().is_empty());
        assert!(browser.kv.pending.is_none());
        assert!(browser.kv.read_unavailable);
        assert!(browser.error.is_empty());
        browser.hint.clear();
        assert_eq!(browser.kv_status_hint(), Some(RETIRED_READ_HINT));
        let _ = browser.kv_actions(cx);
        assert!(browser.rendered_controls.contains("kv-reload"));
        assert_no_data(&requests);
        browser.reload_kv(cx);
        let keys = data(&requests);
        assert!(matches!(keys.call.operation, O::KvKeys { after: None, root: None, .. }));
        let root = Root { level: 0, digest: [4; 32] };
        browser.receive_kv(
            reply(keys, Ok(R::KvKeys(Page { root: Some(root), keys: vec![], next: None }))),
            cx,
        );
        let fresh = data(&requests);
        assert!(matches!(fresh.call.operation, O::KvRead { offset: 0, root: None, .. }));
        browser.receive_kv(
            reply(
                fresh,
                Ok(R::KvRead(Chunk { root, offset: 0, total_bytes: 3, bytes: b"new".to_vec() })),
            ),
            cx,
        );
        assert_eq!(browser.kv.value.read(cx).text(), "new");
        assert!(!browser.kv.read_unavailable);
        assert!(browser.kv_can_edit(cx));
        assert_no_data(&requests);
    });
}

#[gpui::test]
fn kv_safety_hints_survive_cleared_guidance_and_emit_a_visual_notice(
    cx: &mut gpui::TestAppContext,
) {
    // Persistent state owns safety guidance and priority; transient hints cannot erase it.
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        let read = open_value(&mut browser, &requests, cx);
        browser.receive_kv(
            reply(
                read,
                Ok(R::KvRead(Chunk {
                    root: Root { level: 0, digest: [3; 32] },
                    offset: 0,
                    total_bytes: (MAX_BYTES + 1) as u64,
                    bytes: vec![b'a'; MAX_BYTES],
                })),
            ),
            cx,
        );
        browser
    });
    let notices = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let captured = notices.clone();
    let browser = window.update(cx, |_, _, cx| cx.entity()).unwrap();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&browser, move |_, notice: &super::super::Notice, _| {
            captured.borrow_mut().push(notice.0.clone());
        })
    });
    for (uncertain, unavailable, changed, expected) in [
        (
            true,
            true,
            true,
            "The write outcome could not be confirmed. Inspect the value or discard the draft before another save.",
        ),
        (false, true, true, RETIRED_READ_HINT),
        (false, false, true, "This value changed. Review your draft before saving."),
        (false, false, false, "Read-only preview. This value is too large to edit here."),
    ] {
        window
            .update(cx, |browser, _, cx| {
                browser.kv.uncertain = uncertain;
                browser.kv.read_unavailable = unavailable;
                browser.kv.changed_during_edit = changed;
                browser.hint.clear();
                assert_eq!(browser.kv_status_hint(), Some(expected));
                cx.notify();
            })
            .unwrap();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                assert!(window.accessibility_error().is_none());

                if unavailable {
                    let (_, reload) = window
                        .accessibility_nodes_for_test()
                        .find(|(_, node)| node.label() == Some("Reload"))
                        .expect("the heading retains recovery for an unavailable read");
                    assert_eq!(reload.is_disabled(), uncertain);
                }
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(notices.borrow().last().map(String::as_str), Some(expected));
        assert_no_data(&requests);
    }
}

#[gpui::test]
fn malformed_value_reads_cannot_publish_out_of_bounds_bytes(cx: &mut gpui::TestAppContext) {
    // Invalid lengths and offsets never become visible content or authorize a following mutation.
    for (total_bytes, offset, length) in [
        (MAX_STORED_BYTES + 1, 0, MAX_BYTES),
        (MAX_BYTES * 2, 1, MAX_BYTES),
        (MAX_BYTES * 2, 0, MAX_BYTES - 1),
        (MAX_BYTES + 1, 0, MAX_BYTES + 1),
    ] {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            let first = open_value(browser, &requests, cx);
            browser.receive_kv(
                reply(
                    first,
                    Ok(R::KvRead(Chunk {
                        root: Root { level: 0, digest: [3; 32] },
                        offset: offset as u64,
                        total_bytes: total_bytes as u64,
                        bytes: vec![170; length],
                    })),
                ),
                cx,
            );
            assert!(browser.kv.chunk.is_none());
            assert!(browser.kv.value.read(cx).text().is_empty());
            assert!(browser.kv.pending.is_none());
            assert!(!browser.error.is_empty());
            assert!(!browser.kv_can_edit(cx));
            assert_no_data(&requests);
        });
    }
}

#[gpui::test]
fn retired_read_gets_one_fresh_admission_then_inline_reload(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        open(browser, cx);
        let initial = data(&requests);
        let sequence = initial.sequence;
        let call = initial.call.clone();
        browser.receive_kv(unavailable(initial), cx);
        let fresh = data(&requests);
        assert!(fresh.sequence > sequence);
        assert_eq!(fresh.call, call);
        assert!(browser.error.is_empty());
        assert!(browser.hint.is_empty());
        browser.receive_kv(unavailable(fresh), cx);
        assert!(browser.kv.pending.is_none());
        assert!(browser.kv.page.is_none());
        assert!(browser.error.is_empty());
        assert_eq!(browser.hint, RETIRED_READ_HINT);
        assert_no_data(&requests);
        // A supplied reload opens a new operation, not an unbounded retry loop.
        browser.reload_kv(cx);
        let reload = data(&requests);
        assert!(browser.hint.is_empty());
        browser.receive_kv(
            reply(reload, Ok(R::KvKeys(Page { root: None, keys: vec![], next: None }))),
            cx,
        );
        assert!(browser.kv.page.is_some());
        assert!(browser.kv.pending.is_none());
    });
}

#[gpui::test]
fn retired_reads_recover_a_dirty_draft_before_an_explicit_save(cx: &mut gpui::TestAppContext) {
    for terminal in [false, true] {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            open(browser, cx);
            let root = Root { level: 0, digest: [3; 32] };
            let page = || R::KvKeys(Page { root: Some(root), keys: vec![], next: None });
            let chunk =
                || R::KvRead(Chunk { root, offset: 0, total_bytes: 3, bytes: b"old".to_vec() });
            browser.receive_kv(reply(data(&requests), Ok(page())), cx);
            browser.select_kv_key(b"key".to_vec(), cx);
            browser.receive_kv(reply(data(&requests), Ok(chunk())), cx);
            browser.read_kv();
            let pending = data(&requests);
            browser.kv.value.update(cx, |editor, cx| editor.acknowledge(None, "my draft", cx));
            browser.kv.dirty = true;
            browser.kv.revision = 7;
            browser.receive_kv(unavailable(pending), cx);
            let fresh = data(&requests);
            if terminal {
                browser.receive_kv(unavailable(fresh), cx);
                assert!(browser.kv.read_unavailable);
                assert!(!browser.kv_can_edit(cx), "retired plaintext cannot authorize editing");
                assert_no_data(&requests);
                browser.reload_kv(cx);
                assert!(!browser.kv.read_unavailable);
                browser.receive_kv(reply(data(&requests), Ok(page())), cx);
                browser.receive_kv(reply(data(&requests), Ok(chunk())), cx);
            } else {
                browser.receive_kv(reply(fresh, Ok(chunk())), cx);
            }
            assert_eq!(browser.kv.value.read(cx).text(), "my draft");
            assert_eq!(browser.kv.revision, 7);
            assert!(browser.kv.dirty);
            assert!(browser.kv_can_edit(cx));
            assert!(browser.kv_can_save(cx));
            assert!(browser.error.is_empty());
            assert_no_data(&requests);
            browser.save_kv(cx);
            let saved = data(&requests);
            assert!(matches!(&saved.call.operation, O::KvPut { key, value, .. }
                if key == b"key" && value == b"my draft"));
            browser.receive_kv(reply(saved, Ok(R::KvWritten)), cx);
            assert!(!browser.kv.dirty);
        });
    }
}

#[gpui::test]
fn rejected_writes_and_uncertain_exchanges_keep_errors_without_replay(
    cx: &mut gpui::TestAppContext,
) {
    use playbit_api::abi::syscall::Error;
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        for (operation, status, uncertain) in [
            (
                O::KvPut { entry_id: *entry(1).id.as_bytes(), key: b"key".to_vec(), value: vec![] },
                Some(Error::Unavailable),
                false,
            ),
            (
                O::KvDelete { entry_id: *entry(1).id.as_bytes(), key: b"key".to_vec() },
                Some(Error::Unavailable),
                false,
            ),
            (
                O::KvPut { entry_id: *entry(1).id.as_bytes(), key: b"key".to_vec(), value: vec![] },
                None,
                true,
            ),
            (
                O::KvKeys { entry_id: *entry(1).id.as_bytes(), after: None, root: None, limit: 64 },
                Some(Error::StaleHandle),
                false,
            ),
        ] {
            open(browser, cx);
            let _initial = data(&requests);
            browser.kv.pending = None;
            assert!(browser.submit_kv(operation));
            let mut failure = reply(data(&requests), Err("The operation failed".into()));
            failure.result.as_mut().unwrap_err().status = status;
            browser.receive_kv(failure, cx);
            assert!(!browser.error.is_empty());
            assert_eq!(browser.kv.uncertain, uncertain);
            assert!(browser.kv.pending.is_none());
            assert_no_data(&requests);
        }
    });
}

mod rename;

#[gpui::test]
fn browser_panes_keep_metadata_above_content_and_editors_reachable_when_resized(
    cx: &mut gpui::TestAppContext,
) {
    // Render canonical and minimum heights around a live draft, preserving editor identity/focus.
    use gpui::Focusable;
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        super::super::tests::creation_authority(&mut browser);
        window.activate_window();
        browser
    });
    // Browser dimensions exclude the Shell's 240-point volume sidebar.
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(800.), px(640.)));
    visual.run_until_parked();
    let upper = visual.debug_bounds("browser-upper").unwrap();
    let lower = visual.debug_bounds("browser-lower").unwrap();
    let metadata = visual.debug_bounds("browser-metadata").unwrap();
    assert_eq!(upper.size.height, px(352.));
    assert_eq!(lower.size.height, px(288.));
    assert_eq!(lower.top(), upper.bottom());
    assert_eq!(lower.size.width, px(800.));
    assert_eq!(metadata.size.width, px(320.));
    assert_eq!(metadata.top(), upper.top());
    assert_eq!(metadata.bottom(), upper.bottom());
    assert_eq!(metadata.right(), upper.right());

    let editors = window
        .update(&mut visual, |browser, _, cx| {
            let mut current = details(1, EntryAcl::Inherit);
            current.writable = true;
            browser.selected = Some(current.entry.id);
            browser.details = Some(Arc::new(current));
            browser.overview = false;
            browser.kv.page = Some(Arc::new(crate::store::key_value::Page {
                root: None,
                keys: vec![],
                next: None,
            }));
            browser.new_kv_value(cx);
            (browser.kv.key.entity_id(), browser.kv.value.entity_id())
        })
        .unwrap();
    visual.run_until_parked();
    let upper = visual.debug_bounds("browser-upper").unwrap();
    let lower = visual.debug_bounds("browser-lower").unwrap();
    let metadata = visual.debug_bounds("browser-metadata").unwrap();
    let table = visual.debug_bounds("kv-list").unwrap();
    let editor = visual.debug_bounds("kv-editor").unwrap();
    assert_eq!(upper.size.height, px(286.));
    assert_eq!(lower.size.height, px(354.));
    assert_eq!(lower.top(), upper.bottom());
    assert_eq!(metadata.bottom(), upper.bottom());
    assert!(table.top() >= lower.top());
    assert!((table.size.width - px(480.)).abs() <= px(1.));
    assert!(editor.top() >= lower.top());
    assert!((editor.left() - table.right()).abs() <= px(1.));
    assert_eq!(editor.right(), lower.right());
    assert!(editor.bottom() <= lower.bottom());
    let key_field = visual.debug_bounds("kv-key-field").unwrap();
    let value_field = visual.debug_bounds("kv-value-field").unwrap();
    assert!((key_field.top() - lower.top() - px(40.)).abs() <= px(1.));
    assert!((value_field.top() - lower.top() - px(72.)).abs() <= px(1.));
    assert!((key_field.size.width - px(296.)).abs() <= px(1.));
    assert_eq!(value_field.size.width, key_field.size.width);
    assert!((value_field.size.height - px(270.)).abs() <= px(1.));
    for (selector, field) in
        [("kv-key-representation", key_field), ("kv-value-representation", value_field)]
    {
        let toggle = visual.debug_bounds(selector).unwrap();
        assert_eq!(toggle.size, gpui::size(px(24.), px(24.)));
        assert_eq!(toggle.right(), field.right());
        if selector == "kv-key-representation" {
            assert_eq!(toggle.top(), field.top());
        } else {
            assert_eq!(toggle.bottom(), field.bottom());
        }
    }

    visual.simulate_resize(gpui::size(px(800.), px(300.)));
    visual.run_until_parked();
    let upper = visual.debug_bounds("browser-upper").unwrap();
    let lower = visual.debug_bounds("browser-lower").unwrap();
    assert!(upper.size.height >= px(140.));
    assert!(lower.size.height >= px(160.));
    assert_eq!(lower.top(), upper.bottom());
    assert_eq!(lower.bottom(), px(300.));
    for key in ["kv-key-field", "kv-value-field"] {
        let field = visual.debug_bounds(key).unwrap();
        assert!(field.top() >= lower.top(), "{key} remains below the browser");
        assert!(field.bottom() <= lower.bottom(), "{key} remains inside the small window");
        assert!(field.size.height > px(0.), "{key} retains usable height");
    }
    window
        .update(&mut visual, |browser, window, cx| {
            assert_eq!((browser.kv.key.entity_id(), browser.kv.value.entity_id()), editors);
            assert!(browser.kv.key.read(cx).focus_handle(cx).is_focused(window));
            assert!(browser.kv.creating);
        })
        .unwrap();
}
