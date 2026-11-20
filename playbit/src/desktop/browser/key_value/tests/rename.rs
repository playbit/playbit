use super::*;
use gpui::Focusable;

fn binary_value(
    browser: &mut Browser,
    requests: &async_channel::Receiver<Message>,
    cx: &mut Context<Browser>,
) {
    let read = open_value(browser, requests, cx);
    browser.receive_kv(
        reply(
            read,
            Ok(R::KvRead(Chunk {
                root: Root { level: 0, digest: [3; 32] },
                offset: 0,
                total_bytes: 4,
                bytes: vec![0, 255, 0, 128],
            })),
        ),
        cx,
    );
}

fn edit_name(window: gpui::WindowHandle<Browser>, text: &str, cx: &mut gpui::TestAppContext) {
    cx.update(text_editor::install_keybindings);
    window
        .update(cx, |browser, window, cx| {
            browser.kv.key.read(cx).focus_handle(cx).focus(window);
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "cmd-a");
    cx.simulate_input(window.into(), text);
}

#[gpui::test]
fn key_rename_preserves_binary_value_and_moves_selection_only_after_exact_receipt(
    cx: &mut gpui::TestAppContext,
) {
    // Goal: rename real inspector bytes. Method: native text input, bounded worker capture and committed-cut receipt.
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        binary_value(&mut browser, &requests, cx);
        window.activate_window();
        browser
    });
    cx.run_until_parked();
    edit_name(window, "renamed", cx);
    window
        .update(cx, |browser, _, cx| {
            assert!(browser.kv_can_save(cx));
            browser.save_kv(cx);
            assert_eq!(browser.kv.selected.as_deref(), Some(b"value".as_slice()));
            let queued = data(&requests);
            let source = queued.rename.as_ref().expect("dedicated management rename");
            assert_eq!(source.old_key, b"value");
            assert_eq!(source.expected, Root { level: 0, digest: [3; 32] });
            assert!(matches!(&queued.call.operation, O::KvPut { key, value, .. }
            if key == b"renamed" && value == &[0, 255, 0, 128]));
            let committed = Root { level: 1, digest: [4; 32] };
            let mut received = reply(queued, Ok(R::KvWritten));
            received.committed_root = Some(committed);
            browser.receive_kv(received, cx);
            assert_eq!(browser.kv.selected.as_deref(), Some(b"renamed".as_slice()));
            assert_eq!(browser.kv.chunk.as_ref().unwrap().root, committed);
            assert_eq!(browser.kv.chunk.as_ref().unwrap().bytes, [0, 255, 0, 128]);
            assert!(browser.kv.saved && !browser.kv.dirty);
        })
        .unwrap();
}

#[gpui::test]
fn rejected_rename_retains_original_key_base_and_raw_new_name_without_publishing_again(
    cx: &mut gpui::TestAppContext,
) {
    // Goal: preserve an unsuccessful rename. Method: reject a native edit with an explicit conflict receipt.
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        let mut browser = Browser::new(volume(1), send, cx);
        binary_value(&mut browser, &requests, cx);
        window.activate_window();
        browser
    });
    cx.run_until_parked();
    edit_name(window, "occupied", cx);
    window
        .update(cx, |browser, _, cx| {
            browser.save_kv(cx);
            let queued = data(&requests);
            assert!(queued.rename.is_some());
            let mut rejected = reply(queued, Err("destination KV key already exists".into()));
            rejected.result.as_mut().unwrap_err().status =
                Some(playbit_api::abi::syscall::Error::Conflict);
            browser.receive_kv(rejected, cx);
            assert_eq!(browser.kv.selected.as_deref(), Some(b"value".as_slice()));
            assert_eq!(browser.kv.key.read(cx).text(), "occupied");
            assert!(browser.kv.dirty && !browser.kv.uncertain && !browser.kv.saved);
            assert_eq!(browser.kv.chunk.as_ref().unwrap().root, Root { level: 0, digest: [3; 32] });
            let id = browser.kv.draft_id.expect("retained exact local draft");
            let draft = browser.local_draft_snapshot(id, cx).unwrap();
            let crate::store::browser_drafts::BrowserDraftBody::Kv(draft) = &draft.body else {
                panic!("KV draft")
            };
            assert_eq!(draft.original_key.as_deref(), Some(b"value".as_slice()));
            assert_eq!(draft.key_text, "occupied");
            assert!(
                std::iter::from_fn(|| requests.try_recv().ok())
                    .all(|message| !matches!(message, Message::Data(_))),
                "no automatic retry of a rejected mutation"
            );
        })
        .unwrap();
}
