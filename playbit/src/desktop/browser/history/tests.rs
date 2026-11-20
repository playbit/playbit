#![allow(clippy::unwrap_used, reason = "Tests assert correlated, bounded history fixtures.")]
use super::super::tests::{details, entry, volume};
use super::*;
use crate::desktop::worker_queue::{self, Message, QueuedRequest};
use crate::store::volume_index::{EntryRevision, HistoryPage};

fn revision(index: usize) -> EntryRevision {
    let mut bytes = [0; 15];
    bytes[7..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
    EntryRevision {
        revision: EntryId::from_bytes(bytes).unwrap(),
        creation: [0; 32],
        origin_clock: kvdb::Clock { time: 1_000_000, count: (MAX_HISTORY_ROWS - index) as u32 }
            .as_u64(),
        origin: crate::store::volume_index::namespace::Origin {
            installation: Id::from_bytes([1; 16]),
            label: None,
        },
        observed: vec![],
        change: EntryChange::Schema { declaration: Schema::default() },
    }
}

fn queued(requests: &async_channel::Receiver<Message>) -> QueuedRequest {
    let Message::Request(queued) = requests.try_recv().unwrap() else {
        panic!("expected history request")
    };
    assert!(matches!(queued.request, Request::EntryRevisions { limit: PAGE_ROWS, .. }));
    queued
}

fn complete(browser: &mut Browser, request: &QueuedRequest, rows: Range<usize>, next: bool) {
    let Request::EntryRevisions { address, after, .. } = request.request else {
        panic!("history request")
    };
    assert!(browser.history_response_matches(request.credit.as_ref().map(|c| c.admission())));
    let revisions: Vec<_> = rows.map(revision).collect();
    let cursor = next.then(|| HistoryCursor::from(revisions.last().unwrap()));
    assert!(
        browser
            .receive_history(Reply::EntryRevisions {
                address,
                after,
                page: Box::new(HistoryPage { revisions, next: cursor }),
            })
            .is_none()
    );
}

#[gpui::test]
fn history_admissions_fence_navigation_reselection_and_failure(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send.clone(), cx));
    browser.update(cx, |browser, _| {
        while requests.try_recv().is_ok() {}
        browser.selected = Some(entry(1).id);
        browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
        assert!(browser.refresh_history());
        assert!(!browser.refresh_history(), "one page can be in flight");
    });
    let old = queued(&requests);
    browser.update(cx, |browser, _| {
        browser.reset_history();
        browser.selected = Some(entry(2).id);
        browser.details = Some(Arc::new(details(2, EntryAcl::Inherit)));
        assert!(!browser.refresh_history(), "the retired admission must drain first");
        browser.reset_history();
        browser.selected = Some(entry(1).id);
        browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
        complete(browser, &old, 0..2, false);
        assert!(browser.history.rows.is_empty(), "A→B→A cannot adopt the old A page");
    });
    let current = queued(&requests);
    browser.update(cx, |browser, cx| {
        assert!(!browser.history_response_matches(old.credit.as_ref().map(|c| c.admission())));
        complete(browser, &current, 0..64, true);
        assert_eq!(browser.history.rows.len(), 64);
        assert!(!browser.refresh_history());
        assert!(browser.more_history());
        assert!(!browser.more_history());
        let next = queued(&requests);
        let Request::EntryRevisions { address, after, .. } = next.request else {
            panic!("history request")
        };
        assert_eq!(after, Some(HistoryCursor::from(&revision(63))));
        browser.failed_history(address, after, "History is temporarily unavailable", cx);
        assert_eq!(browser.history.rows.len(), 64, "a later-page failure preserves readable rows");
        assert!(!browser.history.error.is_empty());
        assert!(browser.error.is_empty(), "history errors belong in the inspector");
        assert!(!browser.more_history(), "painting an error cannot retry in a loop");
        browser.reset_history();
        let mut denied = details(1, EntryAcl::Inherit);
        denied.retained_readable = false;
        browser.details = Some(Arc::new(denied));
        assert!(!browser.refresh_history());
        assert!(browser.history.rows.is_empty());
    });
    let replacement = cx.new(|cx| Browser::new(volume(1), send, cx));
    replacement.update(cx, |browser, _| {
        while requests.try_recv().is_ok() {}
        browser.selected = Some(entry(1).id);
        browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
        browser.refresh_history();
        assert!(!browser.history_response_matches(old.credit.as_ref().map(|c| c.admission())));
        assert!(!browser.history_response_matches(current.credit.as_ref().map(|c| c.admission())));
    });
}

struct Inspector(Entity<Browser>, f32);
impl Render for Inspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0.update(cx, |browser, cx| {
            browser.rendered_controls.clear();
            browser.rendered_text.clear();
            let details = browser.details.clone().unwrap();
            let inspector = div()
                .w(ui(360.))
                .h(ui(self.1))
                .overflow_hidden()
                .child(browser.entry_inspector(&details, window.is_window_active(), cx));
            browser.retire_controls(window, cx);
            inspector
        })
    }
}

#[gpui::test]
fn inspector_scroll_appends_one_page_and_reuses_only_visible_row_identities(
    cx: &mut gpui::TestAppContext,
) {
    // Exercise the actual inspector while paging, then check its window-corner rail
    // clearance at both UI zooms without adding rounding to the tracked content.
    cx.update(zoom::install);
    cx.set_auto_hide_scrollbars(false);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, _| {
        while requests.try_recv().is_ok() {}
        browser.selected = Some(entry(1).id);
        browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
        browser.refresh_history();
        complete(browser, &queued(&requests), 0..64, true);
    });
    let fixture = browser.clone();
    let window = cx.add_window(|_, _| Inspector(fixture, 240.));
    cx.run_until_parked();
    assert!(requests.try_recv().is_err(), "the first viewport must not prefetch all history");
    browser.read_with(cx, |browser, _| {
        assert!(browser.history.rendered.end < 64);
        assert!(browser.history.rendered.end > 0);
    });
    window
        .update(cx, |fixture, window, cx| {
            let handle = fixture.0.read(cx).scrolling.permissions.read(cx).handle();
            handle.set_offset(gpui::point(gpui::px(0.), -handle.max_offset().height));
            window.refresh();
        })
        .unwrap();
    cx.run_until_parked();
    let next = queued(&requests);
    browser.update(cx, |browser, cx| {
        assert!(browser.history.rendered.start > 0);
        assert!(browser.history.pending.is_some());
        assert!(requests.try_recv().is_err(), "one sentinel admits only one page");
        complete(browser, &next, 64..66, false);
        cx.notify();
    });
    cx.run_until_parked();
    browser.read_with(cx, |browser, _| {
        assert_eq!(browser.history.rows.len(), 66);
        assert_eq!(browser.history.rows[64].clock, revision(64).origin_clock);
        assert!(browser.history.next.is_none());
        let identities =
            browser.text_ids.keys().filter(|key| key.starts_with("history-row-")).count();
        assert!(identities <= 12, "offscreen activity must not allocate a row entity");
    });
    assert!(requests.try_recv().is_err());
    for scale in [10_000, 20_000] {
        cx.update(|cx| zoom::set_scale(scale, cx).unwrap());
        cx.run_until_parked();
        window
            .update(cx, |fixture, window, cx| {
                let handle = fixture.0.read(cx).scrolling.permissions.read(cx).handle();
                let bounds = handle.bounds();
                let rail = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Vertical scroll"))
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap();
                assert_eq!(handle.corner_radius(), px(0.));
                assert!(handle.max_offset().height > px(0.));
                assert_eq!(fixture.0.read(cx).scrolling.inspector_clearance(cx), 16.);
                let time = history_time(revision(0).origin_clock);
                let dates: Vec<_> = window
                    .accessibility_nodes_for_test()
                    .filter(|(_, node)| node.value() == Some(time.as_str()))
                    .filter_map(|(_, node)| node.bounds())
                    .collect();
                assert!(!dates.is_empty());
                for date in dates {
                    assert!(date.x1 <= f64::from(bounds.right() - px(24. * zoom::factor(cx))));
                }
                assert_eq!(rail.x0, f64::from(bounds.right() - px(16. * zoom::factor(cx))));
                assert_eq!(rail.x1, f64::from(bounds.right()));
                assert_eq!(rail.y0, f64::from(bounds.top()));
                assert_eq!(rail.y1, f64::from(bounds.bottom()));
            })
            .unwrap();
    }
    // Shrinking the real history and growing its viewport retires both the rail
    // and its content gutter through layout, without resetting the scroll handle.
    window
        .update(cx, |fixture, _, cx| {
            fixture.1 = 600.;
            fixture.0.update(cx, |browser, cx| {
                browser.history.rows.truncate(1);
                cx.notify();
            });
            cx.notify();
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |fixture, window, cx| {
            let browser = fixture.0.read(cx);
            let handle = browser.scrolling.permissions.read(cx).handle();
            assert_eq!(handle.max_offset().height, px(0.));
            assert_eq!(handle.offset().y, px(0.));
            assert_eq!(browser.scrolling.inspector_clearance(cx), 0.);
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .all(|(_, node)| node.label() != Some("Vertical scroll"))
            );
            let time = history_time(revision(0).origin_clock);
            let date = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.value() == Some(time.as_str()))
                .unwrap()
                .1
                .bounds()
                .unwrap();
            assert_eq!(date.x1, f64::from(handle.bounds().right() - px(12. * zoom::factor(cx))));
        })
        .unwrap();
}

#[gpui::test]
fn history_resource_failure_preserves_the_admitted_cache(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, _| {
        while requests.try_recv().is_ok() {}
        browser.selected = Some(entry(1).id);
        browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
        browser.history.rows =
            (0..MAX_HISTORY_ROWS).map(|i| Row::from_revision(revision(i))).collect();
        browser.history.loaded = true;
        browser.history.next = Some(HistoryCursor::from(&revision(MAX_HISTORY_ROWS - 1)));
        assert!(browser.more_history());
        complete(browser, &queued(&requests), MAX_HISTORY_ROWS..MAX_HISTORY_ROWS + 1, false);
        assert_eq!(browser.history.rows.len(), MAX_HISTORY_ROWS);
        assert!(!browser.history.error.is_empty());
        assert!(browser.error.is_empty());
        assert!(!browser.more_history());
    });
}

#[test]
fn recorded_activity_time_uses_hlc_physical_seconds_and_omits_the_absent_clock() {
    assert!(history_time(0).is_empty());
    let recorded = kvdb::Clock { time: 64_000_000, count: 1 };
    assert_eq!(
        history_time(recorded.as_u64()),
        history_time(kvdb::Clock { count: 99, ..recorded }.as_u64())
    );
    assert!(!history_time(recorded.as_u64()).is_empty());
}

#[test]
fn activity_labels_keep_creation_brief_and_quote_the_renamed_entry() {
    // Render retained creation and rename facts using the inspector's exact captions.
    let mut created = revision(0);
    let mut info = entry(1).info;
    info.kind = EntryKind::Blob {
        len_bytes: 1,
        digest: [0; 32],
        encrypted_key: [0; 32],
        published_at_micros: 0,
    };
    created.change = EntryChange::Created { info: Box::new(info) };
    let created = Row::from_revision(created);
    assert_eq!(created.label(), "Created");
    let mut renamed = revision(1);
    renamed.change = EntryChange::Name { name: "Preferences".into() };
    assert_eq!(Row::from_revision(renamed).label(), "Renamed to “Preferences”");
}
