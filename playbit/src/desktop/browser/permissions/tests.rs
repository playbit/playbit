#![allow(
    clippy::unwrap_used,
    reason = "Tests exercise permission draft and acknowledgment lifetimes."
)]
use super::*;
use crate::desktop::browser::tests::{details, entry, volume};

fn configured(byte: u8) -> Acl {
    Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(Id::from_bytes([byte; 16]))),
            permissions: vec![Permission::Read],
        }],
    }
}

fn set_current(browser: &mut Browser, current: EntryDetails) {
    browser.selected = Some(current.entry.id);
    browser.permissions = current.permissions.clone();
    browser.retained_permissions = current.retained_permissions.clone();
    browser.permissions_base = Some(super::super::drafts::canonical_permissions(
        &current.permissions,
        &current.retained_permissions,
    ));
    browser.details = Some(Arc::new(current));
}

fn receipt(
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
fn failed_permission_save_releases_only_pending_state_and_preserves_edits(
    cx: &mut gpui::TestAppContext,
) {
    // A failed save must allow retry without replacing the grants edited after submission.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let save = browser.update(cx, |browser, cx| {
        let details = details(1, EntryAcl::Inherit);
        set_current(browser, details.clone());
        browser.permissions = EntryAcl::Replace(configured(2));
        browser.update_permission_dirty();
        browser.permissions_panel(&details, true, cx);
        while requests.try_recv().is_ok() {}
        browser.controls["save-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| save(0, cx));
    let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("permission save") };
    browser.update(cx, |browser, cx| {
        assert!(browser.permissions_pending.is_some());
        browser.permissions = EntryAcl::Replace(configured(3));
        browser.update_permission_dirty();
        assert!(browser.retain_local_drafts(cx));
        let other = Request::EntryPermissions {
            volume: volume(2).id,
            entry: entry(1).id,
            permissions: EntryAcl::Inherit,
            retained_permissions: None,
        };
        browser.failed(Failure::for_request(&other).unwrap(), "Other volume", cx);
        assert!(browser.permissions_pending.is_some());
        receipt(browser, queued, Err("Not saved"), cx);
        assert!(browser.permissions_pending.is_none());
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(3)));
        assert!(browser.dirty);
        assert_eq!(browser.error, "Not saved");
    });
    cx.update(|cx| save(0, cx));
    assert!(
        matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::EntryPermissions { permissions: EntryAcl::Replace(acl), .. }, .. }) if acl == configured(3))
    );
}

#[gpui::test]
fn pending_save_blocks_discard_and_acknowledges_only_the_submitted_draft(
    cx: &mut gpui::TestAppContext,
) {
    // Editing remains possible while saving; acknowledgment advances the saved baseline but preserves the newer draft.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let (save, discard) = browser.update(cx, |browser, cx| {
        let details = details(1, EntryAcl::Inherit);
        set_current(browser, details.clone());
        browser.permissions = EntryAcl::Inherit;
        browser.retained_permissions = Some(configured(2));
        browser.update_permission_dirty();
        browser.permissions_panel(&details, true, cx);
        while requests.try_recv().is_ok() {}
        (
            browser.controls["save-permissions"].read(cx).popup_for_test().2,
            browser.controls["discard-permissions"].read(cx).popup_for_test().2,
        )
    });
    cx.update(|cx| save(0, cx));
    let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
    assert!(
        matches!(&queued.request, Request::EntryPermissions { permissions: EntryAcl::Inherit, retained_permissions: Some(acl), .. } if *acl == configured(2))
    );
    cx.update(|cx| save(0, cx));
    assert!(requests.try_recv().is_err());
    browser.update(cx, |browser, _| browser.retained_permissions = Some(configured(3)));
    cx.update(|cx| discard(0, cx));
    browser.update(cx, |browser, cx| {
        assert!(browser.permissions_pending.is_some());
        assert_eq!(browser.retained_permissions, Some(configured(3)));
        assert!(browser.dirty);
        assert!(browser.prepare_navigation(cx));
        receipt(
            browser,
            queued,
            Ok(Reply::EntryPermissions { volume: volume(1).id, entry: entry(1).id }),
            cx,
        );
        assert!(browser.permissions_pending.is_none());
        assert!(browser.dirty);
        assert_eq!(browser.retained_permissions, Some(configured(3)));
        assert_eq!(browser.details.as_ref().unwrap().retained_permissions, Some(configured(2)));
    });
    cx.update(|cx| discard(0, cx));
    browser.update(cx, |browser, cx| {
        assert!(!browser.dirty);
        assert_eq!(browser.permissions, EntryAcl::Inherit);
        assert_eq!(browser.retained_permissions, Some(configured(2)));
        assert!(browser.prepare_navigation(cx));
    });
}

#[gpui::test]
fn matching_permission_ack_clears_dirty_but_unsolicited_ack_does_not(
    cx: &mut gpui::TestAppContext,
) {
    // Acknowledgments finish their pending draft, never an unsubmitted or subsequently edited configuration.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        set_current(browser, details(1, EntryAcl::Inherit));
        browser.permissions = EntryAcl::Replace(configured(2));
        browser.retained_permissions = Some(configured(1));
        browser.update_permission_dirty();
        browser.permissions_saved(entry(1).id, cx);
        assert!(browser.dirty);
        while requests.try_recv().is_ok() {}
        browser.save_permissions(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        assert!(
            crate::desktop::draft_publications::acknowledge_permissions(
                queued.credit.as_ref().map(|credit| credit.admission() + 1),
                volume(1).id,
                Some(entry(1).id),
                cx,
            )
            .is_none()
        );
        assert!(browser.dirty);
        receipt(
            browser,
            queued,
            Ok(Reply::EntryPermissions { volume: volume(1).id, entry: entry(1).id }),
            cx,
        );
        assert!(!browser.dirty);
        assert!(browser.permissions_pending.is_none());
        assert_eq!(browser.details.as_ref().unwrap().permissions, EntryAcl::Replace(configured(2)));
        assert_eq!(browser.details.as_ref().unwrap().retained_permissions, Some(configured(2)));
    });
}

#[gpui::test]
fn enabling_custom_permissions_preserves_inherited_owner_access_but_not_normalize_retained(
    cx: &mut gpui::TestAppContext,
) {
    // First enable materializes implicit owner Full; a remembered custom configuration is restored verbatim.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let toggle = browser.update(cx, |browser, cx| {
        let mut current = details(1, EntryAcl::Inherit);
        current.default_permissions = configured(2);
        set_current(browser, current.clone());
        browser.permissions_panel(&current, true, cx);
        while requests.try_recv().is_ok() {}
        browser.controls["custom-permissions"].read(cx).popup_for_test().2
    });
    cx.update(|cx| toggle(0, cx));
    browser.update(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom configuration") };
        assert_eq!(acl.grants[0], configured(2).grants[0]);
        assert_eq!(acl.grants[1].subject, Subject::Principal(Principal::Human(volume(1).owner)));
        assert_eq!(acl.grants[1].permissions, FULL_ACCESS);
        assert!(requests.try_recv().is_err());
    });
    cx.update(|cx| toggle(0, cx));
    browser.update(cx, |browser, _| browser.retained_permissions = Some(configured(1)));
    cx.update(|cx| toggle(0, cx));
    browser.read_with(cx, |browser, _| {
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(1)))
    });
}

#[gpui::test]
fn owner_row_uses_the_single_configured_grant_and_allows_explicit_upgrade(
    cx: &mut gpui::TestAppContext,
) {
    // A nonpreset owner grant stays editable without a duplicate row or a rendering-time authority change.
    let (send, requests, _) = worker_queue::channel();
    let window = cx.add_window(|_, cx| Browser::new(volume(1), send, cx));
    let browser = window.root(cx).unwrap();
    let upgrade = browser.update(cx, |browser, cx| {
        browser.set_account(volume(1).owner, Some("Alice"));
        assert_eq!(browser.permission_human_label(volume(1).owner), "Alice (You)");
        let mut acl = configured(2);
        acl.grants.push(Grant {
            subject: Subject::Principal(Principal::Human(volume(1).owner)),
            permissions: vec![Permission::Read, Permission::ManagePermissions],
        });
        let current = details(1, EntryAcl::Replace(acl.clone()));
        set_current(browser, current.clone());
        browser.rendered_controls.clear();
        browser.permissions_panel(&current, true, cx);
        assert_eq!(browser.permissions, EntryAcl::Replace(acl));
        assert_eq!(
            browser
                .rendered_controls
                .iter()
                .filter(|key| key.starts_with("grant-subject-"))
                .count(),
            1
        );
        let (items, _, activate) =
            browser.controls["grant-permissions-1"].read(cx).popup_for_test();
        assert!(items.iter().any(|item| matches!(
            item,
            crate::runtime::MenuEntry::Action { id: 0, checked: Some(true), disabled: true, .. }
        )));
        while requests.try_recv().is_ok() {}
        activate
    });
    cx.update(|cx| upgrade(3, cx));
    browser.update(cx, |browser, cx| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom configuration") };
        assert_eq!(acl.grants.len(), 2);
        assert_eq!(acl.grants[1].permissions, FULL_ACCESS);
        let current = browser.details.clone().unwrap();
        browser.rendered_controls.clear();
        browser.permissions_panel(&current, true, cx);
        assert_eq!(
            browser
                .rendered_controls
                .iter()
                .filter(|key| key.starts_with("grant-permissions-"))
                .count(),
            1
        );
        assert!(browser.dirty);
        assert!(requests.try_recv().is_err(), "upgrading still requires explicit Save");
    });

    // Many grants require a vertical gutter without pushing the compact row actions offscreen.
    browser.update(cx, |browser, cx| {
        let EntryAcl::Replace(acl) = &mut browser.permissions else { panic!() };
        for byte in 3..=14 {
            acl.grants.extend(configured(byte).grants);
        }
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(800.), px(640.)));
    visual.run_until_parked();
    let metadata = visual.debug_bounds("browser-metadata").unwrap();
    assert_eq!(metadata.size.width, px(320.));
    browser.read_with(&visual, |browser, cx| {
        assert!(browser.scrolling.permissions.read(cx).handle().max_offset().height > px(0.));
        assert_eq!(browser.scrolling.inspector_clearance(cx), 16.);
    });
    visual.update(|window, _| {
        let mut checked = 0;
        for (_, node) in window.accessibility_nodes_for_test() {
            if matches!(
                node.label(),
                Some("Add permission" | "Account" | "Read only" | "Remove permission")
            ) || matches!(node.value(), Some("Alice (You)" | "Full access"))
            {
                let bounds = node.bounds().unwrap();
                assert!(px(bounds.x0 as f32) >= metadata.left());
                assert!(px(bounds.x1 as f32) <= metadata.right() - px(16.));
                checked += 1;
            }
        }
        assert!(checked >= 6, "owner, access, subject and action bounds were checked");
    });
}

#[gpui::test]
fn owner_full_access_includes_implicit_management_and_public_data_grants(
    cx: &mut gpui::TestAppContext,
) {
    // Effective owner Full is displayed without changing stored flags; another RWA account remains Custom.
    let (send, _, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    browser.update(cx, |browser, cx| {
        for public_append in [false, true] {
            let data = vec![Permission::Read, Permission::Write, Permission::Append];
            let mut acl = Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(volume(1).owner)),
                        permissions: if public_append { data[..2].to_vec() } else { data.clone() },
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(Id::from_bytes([7; 16]))),
                        permissions: data,
                    },
                ],
            };
            if public_append {
                acl.grants.push(Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Append],
                });
            }
            let current = details(1, EntryAcl::Replace(acl.clone()));
            set_current(browser, current.clone());
            browser.rendered_controls.clear();
            browser.rendered_text.clear();
            browser.permissions_panel(&current, true, cx);
            assert!(browser.rendered_text.contains("permission-owner-access"));
            assert!(!browser.rendered_controls.iter().any(|key| key == "grant-permissions-0"));
            let (items, _, _) = browser.controls["grant-permissions-1"].read(cx).popup_for_test();
            assert!(items.iter().any(|item| matches!(
                item,
                crate::runtime::MenuEntry::Action { id: 0, checked: Some(true), .. }
            )));
            assert_eq!(browser.permissions, EntryAcl::Replace(acl));
            assert!(!browser.dirty);
        }
    });
}

#[gpui::test]
fn subject_popup_changes_identity_without_changing_flags_and_rejects_duplicates(
    cx: &mut gpui::TestAppContext,
) {
    // Typed service changes preserve custom flags; duplicate identities and Public Manage never enter the draft.
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let subject = browser.update(cx, |browser, cx| {
        let mut acl = configured(2);
        acl.grants[0].permissions.push(Permission::ManagePermissions);
        acl.grants.push(Grant {
            subject: Subject::Principal(Principal::Service(Id::from_bytes([3; 16]))),
            permissions: vec![Permission::Read],
        });
        let current = details(1, EntryAcl::Replace(acl));
        set_current(browser, current.clone());
        browser.permissions_panel(&current, true, cx);
        let (items, _, activate) = browser.controls["grant-subject-0"].read(cx).popup_for_test();
        assert!(items.iter().any(|item| matches!(
            item,
            crate::runtime::MenuEntry::Action { id: 0, checked: Some(true), .. }
        )));
        assert!(items.iter().any(|item| matches!(
            item,
            crate::runtime::MenuEntry::Action { id: 3, disabled: true, .. }
        )));
        while requests.try_recv().is_ok() {}
        activate
    });
    cx.update(|cx| subject(3, cx));
    browser.read_with(cx, |browser, _| assert!(!browser.dirty));
    cx.update(|cx| subject(2, cx));
    let apply = browser.update(cx, |browser, cx| {
        assert!(browser.grant_service && browser.adding_grant);
        browser.account.update(cx, |editor, cx| {
            editor.acknowledge(None, &Id::from_bytes([3; 16]).to_string(), cx)
        });
        let current = browser.details.clone().unwrap();
        browser.permissions_panel(&current, true, cx);
        browser.controls["add-grant"].read(cx).popup_for_test().2
    });
    cx.update(|cx| apply(0, cx));
    browser.update(cx, |browser, cx| {
        assert!(browser.adding_grant);
        assert!(!browser.dirty);
        assert!(browser.hint.contains("already"));
        browser.account.update(cx, |editor, cx| {
            editor.acknowledge(None, &Id::from_bytes([4; 16]).to_string(), cx)
        });
    });
    cx.update(|cx| apply(0, cx));
    browser.read_with(cx, |browser, _| {
        let EntryAcl::Replace(acl) = &browser.permissions else { panic!("custom configuration") };
        assert_eq!(
            acl.grants[0].subject,
            Subject::Principal(Principal::Service(Id::from_bytes([4; 16])))
        );
        assert_eq!(
            acl.grants[0].permissions,
            vec![Permission::Read, Permission::ManagePermissions]
        );
        assert!(acl.validate().is_ok());
        assert!(browser.dirty);
        assert!(!browser.adding_grant);
        assert!(requests.try_recv().is_err());
    });
}

#[gpui::test]
fn volume_acl_drafts_use_shared_rows_and_correlated_save_baselines(cx: &mut gpui::TestAppContext) {
    // Fenced reads retire data with inline readiness; the same error from a save still alerts.
    let (host, window, requests, responses) = crate::desktop::test_support::host_with_responses(cx);
    let browser = host.update(cx, |host, cx| {
        host.select_volume(volume(1), cx);
        host.browser.as_ref().unwrap().clone()
    });
    browser.update(cx, |browser, cx| {
        let volume = volume(1);
        browser.set_account(volume.owner, Some("Alice"));
        browser.receive(Reply::VolumeDetails { details: crate::store::VolumeDetails {
            volume, permissions: configured(2), created_clock: 1, name_clock: 1,
        }}, cx);
        browser.permissions = EntryAcl::Replace(configured(3));
        browser.update_permission_dirty();
        browser.volume_permissions_panel(true,cx);
        assert!(browser.controls.contains_key("start-grant"));
        while requests.try_recv().is_ok() {}
        browser.save_permissions(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("volume ACL save") };
        assert!(matches!(&queued.request, Request::Permissions { volume:got,acl } if *got==browser.volume.id && acl==&configured(3)));
        let admission = queued.credit.as_ref().map(|credit| credit.admission());
        assert!(browser.volume_permissions_response_matches(admission));
        assert!(!browser.volume_permissions_response_matches(admission.map(|value| value+1)));
        assert!(browser.show_volume(cx), "pending save remains attached to its local draft while navigating");
        browser.permissions = EntryAcl::Replace(configured(4));
        browser.update_permission_dirty();
        assert!(browser.retain_local_drafts(cx));
        browser.volume_permissions_saved(cx);
        assert!(browser.dirty, "a newer draft survives its prior acknowledgment");
        assert_eq!(browser.volume_details.as_ref().unwrap().permissions,configured(3));
        browser.discard_permissions(cx);
        assert!(!browser.dirty);
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(3)));
        assert!(!browser.volume_permissions_response_matches(admission));
        browser.permissions = EntryAcl::Replace(configured(4));
        browser.update_permission_dirty();
        assert!(browser.retain_local_drafts(cx));
    });
    let error = "access denied: current authority requires authenticated catch-up";
    let row = crate::store::volume_index::EntrySummary {
        id: entry(1).id,
        parent: None,
        name: "Retained note".into(),
        kind: crate::store::EntryType::KvDb,
        protocol: None,
        deleted: false,
        name_conflict: false,
    };
    browser.update(cx, |browser, cx| {
        browser.failed(
            Failure::for_request(&Request::VolumeDetails { volume: volume(2).id }).unwrap(),
            error,
            cx,
        );
        assert!(!browser.permission_refresh.volume, "another volume cannot start readiness");
        browser.fixture_receive_root_page(
            volume(1).id,
            None,
            EntryPage { entries: vec![row.clone()], next: None },
            cx,
        );
        assert_eq!(
            browser.catalog.columns[0].page.as_ref().unwrap().entries.as_slice(),
            std::slice::from_ref(&row)
        );
        while requests.try_recv().is_ok() {}
        browser.refresh_catalog();
        browser.refresh_catalog();
        for (cut, waiting) in [(Some(99), false), (None, true)] {
            let request = Request::DirectoryEntries {
                volume: volume(1).id,
                target: crate::store::volume_index::DirectoryTarget::Root,
                cut,
                after: None,
                limit: ENTRY_PAGE_ROWS,
            };
            browser.failed(Failure::for_request(&request).unwrap(), error, cx);
            assert_eq!(browser.permission_refresh.catalog, waiting, "only a matching cut waits");
        }
        assert!(browser.catalog.columns[0].page.is_none(), "fenced catalog is retired");
        assert!(browser.catalog.pending.is_some(), "the coalesced refresh remains pending");
        assert_eq!(
            std::iter::from_fn(|| requests.try_recv().ok())
                .filter(|message| matches!(
                    message,
                    Message::Request(QueuedRequest {
                        request: Request::DirectoryEntries { .. },
                        ..
                    })
                ))
                .count(),
            2,
            "one original query and its already requested coalesced refresh"
        );
    });
    let mut failure = crate::desktop::WorkerResponse::from_reply(Err(error.into()));
    failure.browser_failure =
        Failure::for_request(&Request::VolumeDetails { volume: volume(1).id });
    responses.try_send(failure).unwrap();
    cx.run_until_parked();
    browser.update(cx, |browser, cx| {
        assert!(browser.volume_details.is_none(), "fenced metadata is hidden");
        assert!(!browser.permission_edit_allowed(None));
        assert!(browser.error.is_empty());
        assert!(browser.permission_refresh.volume);
        let draft = browser.permission_draft_id.unwrap();
        browser.discard_permissions(cx);
        assert_eq!(browser.permission_draft_id, Some(draft));
        assert!(browser.local_draft_snapshot(draft, cx).is_some());
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(4)));
        assert!(browser.dirty, "a read failure does not discard edits");
    });
    host.read_with(cx, |host, _| assert!(host.error.is_empty(), "no duplicate host alert"));
    assert!(cx.update(crate::ui::desktop::alert::take_presented).is_empty());
    window
        .update(cx, |_, window, _| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.value() == Some("Refreshing permissions…") }),
                "the read wait is visible without a modal"
            );
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.label() == Some("Retained note") }),
                "the coalesced refresh cannot expose the retired row"
            );
        })
        .unwrap();
    browser.update(cx, |browser, cx| {
        browser.receive(
            Reply::VolumeDetails {
                details: crate::store::VolumeDetails {
                    volume: volume(2),
                    permissions: configured(5),
                    created_clock: 1,
                    name_clock: 1,
                },
            },
            cx,
        );
        assert!(browser.permission_refresh.volume, "another snapshot cannot clear readiness");
        browser.receive(
            Reply::VolumeDetails {
                details: crate::store::VolumeDetails {
                    volume: volume(1),
                    permissions: configured(5),
                    created_clock: 1,
                    name_clock: 1,
                },
            },
            cx,
        );
        assert!(!browser.permission_refresh.volume);
        assert!(browser.permission_refresh.catalog, "metadata does not clear the catalog wait");
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.value() == Some("Refreshing permissions…") }),
                "the catalog stays visibly fenced after metadata recovers"
            );
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.label() == Some("Retained note") })
            );
        })
        .unwrap();
    browser.update(cx, |browser, cx| {
        browser.receive(
            Reply::DirectoryEntries {
                volume: volume(1).id,
                target: crate::store::volume_index::DirectoryTarget::Root,
                cut_requested: None,
                after: None,
                listing: crate::store::volume_index::DirectoryListing::Page {
                    cut: 2,
                    parent: None,
                    ancestors: vec![],
                    entries: vec![row.clone()],
                    next: None,
                },
            },
            cx,
        );
        assert!(!browser.permission_refresh.catalog);
        assert_eq!(
            browser.catalog.columns[0].page.as_ref().unwrap().entries.as_slice(),
            std::slice::from_ref(&row)
        );
        assert!(browser.permission_edit_allowed(None));
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(4)));
        assert!(browser.dirty);
        assert!(
            browser.changed_during_edit,
            "refreshed permissions are compared with the saved baseline"
        );
        browser.discard_permissions(cx);
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(5)));
        assert!(!browser.dirty);
        assert!(browser.permission_draft_id.is_none());
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.value() == Some("Refreshing permissions…") })
            );
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.label() == Some("Retained note") })
            );
        })
        .unwrap();

    // A real save may finish while read readiness is active; its error must remain modal.
    let save = browser.update(cx, |browser, cx| {
        browser.permissions = EntryAcl::Replace(configured(6));
        browser.update_permission_dirty();
        while requests.try_recv().is_ok() {}
        browser.save_permissions(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("save") };
        assert!(matches!(&queued.request, Request::Permissions { .. }));
        browser.failed(
            Failure::for_request(&Request::VolumeDetails { volume: volume(1).id }).unwrap(),
            error,
            cx,
        );
        assert!(browser.permission_refresh.volume);
        queued
    });
    let mut failure = crate::desktop::WorkerResponse::from_reply(Err(error.into()));
    failure.browser_failure = Failure::for_request(&save.request);
    failure.credit = save.credit;
    responses.try_send(failure).unwrap();
    cx.run_until_parked();
    browser.read_with(cx, |browser, _| {
        assert!(browser.permission_refresh.volume);
        assert_eq!(browser.error, error);
        assert!(browser.permissions_pending.is_none());
        assert!(browser.dirty);
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(6)));
    });
    assert_eq!(
        cx.update(crate::ui::desktop::alert::take_presented),
        vec![(crate::ui::desktop::alert::Kind::Recoverable, error.into())]
    );
}

#[gpui::test]
fn returning_to_the_canonical_acl_clears_dirty_and_its_local_draft(cx: &mut gpui::TestAppContext) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
    let change = browser.update(cx, |browser, cx| {
        let current = details(1, EntryAcl::Replace(configured(2)));
        set_current(browser, current.clone());
        browser.permissions_panel(&current, true, cx);
        while requests.try_recv().is_ok() {}
        browser.controls["grant-permissions-0"].read(cx).popup_for_test().2
    });
    cx.update(|cx| change(2, cx));
    cx.run_until_parked();
    let id = browser.read_with(cx, |browser, cx| {
        assert!(browser.dirty);
        let id = browser.permission_draft_id.unwrap();
        let saved = browser.local_draft_snapshot(id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Permissions(state) = &saved.body else {
            panic!()
        };
        assert_eq!(state.base_active, EntryAcl::Replace(configured(2)));
        let EntryAcl::Replace(acl) = &state.active else { panic!() };
        assert_eq!(acl.grants[0].permissions, vec![Permission::Read, Permission::Write]);
        id
    });
    cx.update(|cx| change(1, cx));
    cx.run_until_parked();
    browser.read_with(cx, |browser, cx| {
        assert_eq!(browser.permissions, EntryAcl::Replace(configured(2)));
        assert!(!browser.dirty);
        assert!(browser.local_draft_snapshot(id, cx).is_none());
        assert!(requests.try_recv().is_err(), "an ACL round trip does not publish");
    });
}

#[gpui::test]
fn unfinished_grant_identity_survives_entry_and_volume_navigation_and_a_fresh_browser(
    cx: &mut gpui::TestAppContext,
) {
    let (host, _window, _requests) = crate::desktop::test_support::host(cx);
    let target = Subject::Principal(Principal::Human(Id::from_bytes([2; 16])));
    let current = details(1, EntryAcl::Replace(configured(2)));
    let original = host.update(cx, |host, cx| {
        host.volumes = vec![volume(1), volume(2)];
        assert!(host.select_volume(volume(1), cx));
        let browser = host.browser.as_ref().unwrap().clone();
        browser.update(cx, |browser, cx| {
            browser.select(entry(1).id, cx);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: current.clone() },
                cx,
            );
            browser.begin_grant_editor(Some(target.clone()), true, cx);
            browser
                .account
                .update(cx, |editor, cx| editor.acknowledge(None, "partial service ID", cx));
            browser.select(entry(2).id, cx);
            assert_eq!(browser.selected, Some(entry(2).id));
            browser.receive(
                Reply::EntryDetails {
                    volume: volume(1).id,
                    details: details(2, EntryAcl::Inherit),
                },
                cx,
            );
            browser.select(entry(1).id, cx);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: current.clone() },
                cx,
            );
            assert!(browser.adding_grant && browser.grant_service);
            assert_eq!(browser.editing_grant, Some(target.clone()));
            assert_eq!(browser.account.read(cx).text(), "partial service ID");
            assert!(!browser.dirty, "unfinished identity input is separate from the ACL");
        });
        assert!(host.select_volume(volume(2), cx));
        assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, volume(2).id);
        browser.entity_id()
    });
    cx.run_until_parked();
    host.update(cx, |host, cx| {
        assert!(host.select_volume(volume(1), cx));
        let browser = host.browser.as_ref().unwrap().clone();
        assert_ne!(browser.entity_id(), original);
        browser.update(cx, |browser, cx| {
            browser.select(entry(1).id, cx);
            browser.receive(Reply::EntryDetails { volume: volume(1).id, details: current }, cx);
            assert!(browser.adding_grant && browser.grant_service);
            assert_eq!(browser.editing_grant, Some(target.clone()));
            assert_eq!(browser.account.read(cx).text(), "partial service ID");
            let id = browser.permission_draft_id.unwrap();
            let saved = browser.local_draft_snapshot(id, cx).unwrap();
            let crate::store::browser_drafts::BrowserDraftBody::Permissions(state) = &saved.body
            else {
                panic!()
            };
            assert!(state.adding_grant && state.grant_service);
            assert_eq!(state.grant_target, Some(target));
            assert_eq!(state.grant_input, "partial service ID");
            assert_eq!(state.active, state.base_active);
        });
    });
}

#[gpui::test]
fn offscreen_permission_receipt_rebases_reverted_acl_and_preserves_unfinished_identity(
    cx: &mut gpui::TestAppContext,
) {
    let (send, requests, _) = worker_queue::channel();
    let browser = cx.new(|cx| Browser::new(volume(1), send.clone(), cx));
    let baseline = EntryAcl::Replace(configured(2));
    let submitted = EntryAcl::Replace(configured(3));
    let target = Subject::Principal(Principal::Human(Id::from_bytes([2; 16])));
    let (queued, id) = browser.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, baseline.clone()) },
            cx,
        );
        browser.permissions = submitted.clone();
        browser.update_permission_dirty();
        while requests.try_recv().is_ok() {}
        browser.save_permissions(cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        let id = browser.permission_draft_id.unwrap();
        browser.select(entry(2).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
            cx,
        );
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, baseline.clone()) },
            cx,
        );
        assert_eq!(browser.permissions, submitted);
        browser.permissions = baseline.clone();
        browser.update_permission_dirty();
        assert!(!browser.dirty);
        browser.begin_grant_editor(Some(target.clone()), false, cx);
        browser
            .account
            .update(cx, |editor, cx| editor.acknowledge(None, "unfinished principal", cx));
        assert!(browser.retain_local_drafts(cx));
        assert!(crate::desktop::draft_publications::pending(id, cx));
        (queued, id)
    });
    cx.run_until_parked();
    browser.update(cx, |browser, cx| {
        receipt(
            browser,
            queued,
            Ok(Reply::EntryPermissions { volume: volume(1).id, entry: entry(1).id }),
            cx,
        );
        browser.sync_local_drafts(cx);
        assert!(browser.dirty, "the submitted ACL is now the canonical baseline");
        assert_eq!(browser.permissions, baseline);
        assert_eq!(browser.account.read(cx).text(), "unfinished principal");
        assert_eq!(browser.editing_grant, Some(target.clone()));
        assert!(!crate::desktop::draft_publications::pending(id, cx));
        let saved = browser.local_draft_snapshot(id, cx).unwrap();
        let crate::store::browser_drafts::BrowserDraftBody::Permissions(state) = &saved.body else {
            panic!()
        };
        assert_eq!(state.base_active, submitted);
        assert_eq!(state.active, baseline);
        assert!(state.adding_grant);
        assert_eq!(state.grant_input, "unfinished principal");
    });
    let reopened = cx.new(|cx| Browser::new(volume(1), send, cx));
    reopened.update(cx, |browser, cx| {
        browser.select(entry(1).id, cx);
        browser.receive(
            Reply::EntryDetails { volume: volume(1).id, details: details(1, submitted) },
            cx,
        );
        assert_eq!(browser.permissions, baseline);
        assert!(browser.dirty && browser.adding_grant);
        assert_eq!(browser.account.read(cx).text(), "unfinished principal");
        while requests.try_recv().is_ok() {}
        browser.save_permissions(cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::EntryPermissions { permissions, .. }, ..
        }) if permissions == baseline));
    });
}
