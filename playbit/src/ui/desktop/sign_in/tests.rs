#![allow(clippy::unwrap_used, reason = "Tests assert fixture UI operations.")]
use super::*;
use gpui::EntityInputHandler;
use std::cell::{Cell, RefCell};

#[gpui::test]
fn channel_selection_precedes_authentication_and_admits_only_one_switch(
    cx: &mut gpui::TestAppContext,
) {
    // The shared view delegates profile work; a pending switch cannot submit
    // credentials or accept another environment choice.
    let selected = Rc::new(RefCell::new(Vec::new()));
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (_, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    let received = selected.clone();
    view.update(cx, |view, cx| {
        view.show_environment_selector(
            Some(Rc::new(move |channel, _| {
                received.borrow_mut().push(channel);
                Ok(())
            })),
            cx,
        );
        view.change_channel(ReleaseChannel::Testing, cx);
        view.change_channel(ReleaseChannel::Testing, cx);
        view.submit(cx);
        assert!(view.channel_busy);
    });
    assert_eq!(*selected.borrow(), [ReleaseChannel::Testing]);
    assert!(sent.borrow().is_empty());
    view.update(cx, |view, cx| {
        view.channel_failed("Close the other Playbit app and retry".into(), cx);
        assert_eq!(view.channel, ReleaseChannel::Public);
        assert!(!view.channel_busy);
        assert_eq!(view.flow.error, "Close the other Playbit app and retry");
        view.change_channel(ReleaseChannel::Testing, cx);
    });
    assert_eq!(*selected.borrow(), [ReleaseChannel::Testing, ReleaseChannel::Testing]);
    assert!(sent.borrow().is_empty());
}

#[gpui::test]
fn rejected_channel_selection_keeps_the_original_environment_and_error(
    cx: &mut gpui::TestAppContext,
) {
    // A host refusal never relabels the current profile or starts enrollment.
    let (_, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    view.update(cx, |view, cx| {
        view.show_environment_selector(
            Some(Rc::new(|_, _| Err("Another app is still using Public".into()))),
            cx,
        );
        view.change_channel(ReleaseChannel::Testing, cx);
        assert_eq!(view.channel, ReleaseChannel::Public);
        assert!(!view.channel_busy);
        assert_eq!(view.flow.error, "Another app is still using Public");
        view.flow.step = Step::Code;
        view.change_channel(ReleaseChannel::Testing, cx);
        assert!(!view.channel_busy);
    });
}

#[gpui::test]
fn channel_failures_keep_the_current_setup_form_and_can_retry_the_switch(
    cx: &mut gpui::TestAppContext,
) {
    // Both immediate refusal and background failure leave the current setup
    // choice editable; retrying a switch must not acknowledge the account key.
    for (step, deferred) in [Step::Name, Step::SaveKey]
        .into_iter()
        .flat_map(|step| [false, true].map(|deferred| (step, deferred)))
    {
        let attempts = Rc::new(Cell::new(0));
        let selected = attempts.clone();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let dispatch = sent.clone();
        let (window, view) = cx.update(|cx| {
            open(
                ReleaseChannel::Public,
                false,
                Rc::new(move |work, _| {
                    record_submission(&dispatch, work);
                    Ok(())
                }),
                Rc::new(|_| {}),
                cx,
            )
            .unwrap()
        });
        view.update(cx, |view, cx| {
            view.flow = Flow::new(true);
            view.flow.step = step;
            view.flow.saved_key = crate::store::account_key::format(&[3; 16]);
            view.saved.update(cx, |field, cx| field.acknowledge(None, &view.flow.saved_key, cx));
            view.show_environment_selector(
                Some(Rc::new(move |_, _| {
                    selected.set(selected.get() + 1);
                    if deferred { Ok(()) } else { Err("Another app is still using Public".into()) }
                })),
                cx,
            );
            view.change_channel(ReleaseChannel::Testing, cx);
            if deferred {
                view.channel_failed("Another app is still using Public".into(), cx);
            }
            assert_eq!(view.flow.step, step);
            assert!(!view.flow.error_screen());
            assert!(!view.channel_busy);
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                let nodes: Vec<_> = window.accessibility_nodes_for_test().map(|(_, n)| n).collect();
                assert!(nodes.iter().any(|n| {
                    n.role() == gpui::accesskit::Role::ComboBox
                        && n.label() == Some("Environment: Public")
                        && !n.is_disabled()
                }));
                let field = nodes
                    .iter()
                    .find(|n| {
                        n.placeholder()
                            == Some(if step == Step::Name {
                                "Display name"
                            } else {
                                "Saved account key"
                            })
                    })
                    .expect("the current setup field remains visible");
                assert_eq!(field.is_read_only(), step == Step::SaveKey);
                let primary = nodes
                    .iter()
                    .find(|n| {
                        n.label()
                            == Some(if step == Step::Name {
                                "Done"
                            } else {
                                "I have saved my account key"
                            })
                    })
                    .expect("the current setup action remains visible");
                assert_eq!(primary.is_disabled(), step == Step::Name);
            })
            .unwrap();
        assert!(sent.borrow().is_empty());
        view.update(cx, |view, cx| view.change_channel(ReleaseChannel::Testing, cx));
        assert_eq!(attempts.get(), 2);
        assert!(sent.borrow().is_empty(), "switching never submits the setup form");
        window.update(cx, |_, window, _| window.remove_window()).unwrap();
    }
}

#[gpui::test]
fn locked_public_account_can_select_testing_without_providing_its_key(
    cx: &mut gpui::TestAppContext,
) {
    // Recovery belongs to Public. Its saved account cannot force a support
    // user to unlock Public before choosing an independent Testing profile.
    let selected = Rc::new(Cell::new(None));
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (_, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    let chosen = selected.clone();
    view.update(cx, |view, cx| {
        view.show_environment_selector(
            Some(Rc::new(move |channel, _| {
                chosen.set(Some(channel));
                Ok(())
            })),
            cx,
        );
        view.flow = Flow::new(true);
        let recovery = view.flow.start().unwrap();
        view.change_channel(ReleaseChannel::Testing, cx);
        assert_eq!(selected.get(), None, "an admitted recovery action must finish first");
        view.receive(
            recovery.ticket,
            Ok(Reply::Recovery(crate::engine::reply::RecoveryStatus::Locked {
                revision: 1,
                pending_save: false,
            })),
            cx,
        );
        let profile = sent.borrow_mut().pop().expect("account metadata lookup");
        assert!(matches!(profile.command, flow::Command::Profile));
        view.receive(
            profile.ticket,
            Ok(Reply::Profile {
                account: crate::identifier::encode(&[1; 16]),
                email: "public@example.invalid".into(),
                profile: crate::engine::reply::AccountProfile {
                    agreement_required: false,
                    replacement_required: false,
                    display_name: None,
                    unlocked: false,
                    initialized: true,
                },
            }),
            cx,
        );
        assert_eq!(view.flow.step, Step::RestoreKey);
        view.change_channel(ReleaseChannel::Testing, cx);
        assert_eq!(selected.get(), Some(ReleaseChannel::Testing));
        assert!(view.channel_busy);
    });
    assert!(sent.borrow().is_empty(), "switching never submits the old account's key");
}

// Existing submission tests leave optional lookup unresolved; it must not block input.
fn record_submission(sent: &RefCell<Vec<Work>>, work: Work) {
    if !matches!(work.command, flow::Command::SavedAccount) {
        sent.borrow_mut().push(work);
    }
}

#[gpui::test]
fn grouped_paste_submits_once_and_error_waits_for_edit_or_return(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                view.email.update(cx, |field, cx| {
                    field.replace_text_in_range(None, "a@b.test", window, cx)
                });
            })
            .unwrap()
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| view.submit(cx));
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| view.receive(work.ticket, Ok(Reply::CodeSent), cx));
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                view.code.update(cx, |field, cx| {
                    field.replace_text_in_range(None, "001–002", window, cx)
                });
            })
            .unwrap()
    });
    cx.run_until_parked();
    assert_eq!(sent.borrow().len(), 1);
    let attempt = sent.borrow_mut().pop().unwrap();
    assert!(matches!(attempt.command, flow::Command::SignIn(ref value) if value == "001002"));
    view.update(cx, |view, cx| {
        view.receive(attempt.ticket, Err("That code is incorrect".into()), cx)
    });
    cx.run_until_parked();
    assert!(sent.borrow().is_empty(), "rendering an error must not resubmit a complete code");
    view.update(cx, |view, cx| {
        view.submit(cx);
        view.submit(cx);
    });
    assert_eq!(sent.borrow().len(), 1);
}

#[gpui::test]
fn saved_key_copy_does_not_acknowledge_and_completion_closes_once(cx: &mut gpui::TestAppContext) {
    let completed = Rc::new(Cell::new(0));
    let completion = completed.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(|_, _| Ok(())),
            Rc::new(move |_| {
                completion.set(completion.get() + 1);
            }),
            cx,
        )
        .unwrap()
    });
    let key = crate::store::account_key::format(&[3; 16]);
    view.update(cx, |view, cx| {
        view.flow.step = Step::SaveKey;
        view.flow.saved_key = key.clone();
        view.saved.update(cx, |field, cx| field.acknowledge(None, &key, cx));
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|cx| {
        super::super::text_editor::install_keybindings(cx);
        window.update(cx, |view, window, cx| window.focus(&view.saved.focus_handle(cx))).unwrap();
    });
    cx.simulate_keystrokes(window.into(), "cmd-a cmd-c");
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()), key);
    view.read_with(cx, |view, _| {
        assert_eq!(view.flow.step, Step::SaveKey);
        assert!(!view.flow.busy());
    });
    assert_eq!(completed.get(), 0);
    view.update(cx, |view, cx| {
        view.flow.step = Step::Complete;
        view.finish(cx);
        view.finish(cx);
    });
    cx.run_until_parked();
    assert_eq!(completed.get(), 1);
    assert!(!cx.update(|cx| cx.windows().contains(&window.into())));
}

#[gpui::test]
fn errors_are_accessible_and_masked_key_is_never_exposed_to_native_text(
    cx: &mut gpui::TestAppContext,
) {
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                view.key.update(cx, |field, cx| {
                    field.replace_text_in_range(None, "account-secret", window, cx);
                    let mut adjusted = None;
                    assert_eq!(
                        field.text_for_range(0..14, &mut adjusted, window, cx).as_deref(),
                        Some("**************")
                    );
                });
            })
            .unwrap()
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.flow.error = "AP unavailable. Retry.".into();
        cx.notify();
    });
    cx.run_until_parked();
    let nodes = cx.update(|cx| {
        window
            .update(cx, |_, window, cx| {
                let _ = cx;
                window
                    .accessibility_nodes_for_test()
                    .map(|(id, node)| (id, node.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap()
    });
    use gpui::accesskit::{Live, Role};
    assert!(nodes.iter().any(|(_, n)| n.role() == Role::PasswordInput));
    assert!(nodes.iter().all(|(_, n)| n.value() != Some("account-secret")));
    assert!(nodes.iter().any(
        |(_, n)| n.value() == Some("AP unavailable. Retry.") && n.live() == Some(Live::Polite)
    ));
}

#[gpui::test]
fn deferred_code_edit_cannot_acknowledge_the_next_screen(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, _) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                view.flow.step = Step::Code;
                view.code.update(cx, |field, cx| {
                    field.replace_text_in_range(None, "000012", window, cx)
                });
                view.flow.step = Step::SaveKey;
                view.flow.saved_key = crate::store::account_key::format(&[7; 16]);
            })
            .unwrap()
    });
    cx.run_until_parked();
    assert!(sent.borrow().is_empty(), "a queued edit belongs only to its original screen");
}

#[gpui::test]
fn only_top_32_points_drag_the_sign_in_window(cx: &mut gpui::TestAppContext) {
    let (window, _) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    for x in [16., 100., 200., 384.] {
        for y in [1., 16., 31.] {
            assert!(cx.window_can_drag_at(window.into(), gpui::point(px(x), px(y))));
        }
        for y in [33., 48., 140., 180., 300.] {
            assert!(!cx.window_can_drag_at(window.into(), gpui::point(px(x), px(y))));
        }
    }
}

#[gpui::test]
fn incomplete_email_disables_button_and_keyboard_submission(cx: &mut gpui::TestAppContext) {
    cx.update(super::super::text_editor::install_keybindings);
    let calls = Rc::new(Cell::new(0));
    let sent = calls.clone();
    let (window, _) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                if !matches!(work.command, flow::Command::SavedAccount) {
                    sent.set(sent.get() + 1);
                }
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                view.email
                    .update(cx, |field, cx| field.replace_text_in_range(None, "a@b", window, cx));
            })
            .unwrap()
    });
    cx.run_until_parked();
    cx.update(|cx| {
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, n)| n.role() == gpui::accesskit::Role::Button && n.is_disabled())
                );
            })
            .unwrap()
    });
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    assert_eq!(calls.get(), 0);
}

#[gpui::test]
fn key_sign_in_keeps_read_only_form_through_recovery_and_metadata(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    let key = crate::store::account_key::format(&[3; 16]);
    view.update(cx, |view, cx| {
        view.email.update(cx, |field, cx| field.acknowledge(None, "a@b.test", cx));
        view.key.update(cx, |field, cx| field.acknowledge(None, &key, cx));
        view.submit(cx);
    });
    let readonly_form = |cx: &mut gpui::TestAppContext| {
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                let nodes: Vec<_> =
                    window.accessibility_nodes_for_test().map(|(_, n)| n.clone()).collect();
                let fields: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        matches!(
                            n.role(),
                            gpui::accesskit::Role::TextInput | gpui::accesskit::Role::PasswordInput
                        )
                    })
                    .collect();
                assert_eq!(fields.len(), 2);
                assert!(fields.iter().all(|n| n.is_read_only() && !n.is_disabled()));
                for field in [&view.email, &view.key] {
                    field.update(cx, |field, cx| {
                        field.replace_text_in_range(None, "changed", window, cx)
                    });
                }
                assert_eq!(view.email.read(cx).text(), "a@b.test");
                assert_eq!(view.key.read(cx).text(), key);
            })
            .unwrap();
    };
    readonly_form(cx);
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| {
        view.receive(
            work.ticket,
            Ok(Reply::SignedIn(crate::engine::reply::SignInReply {
                replacement_required: false,
                signed_in: true,
                recovery: None,
                recovery_error: None,
            })),
            cx,
        )
    });
    assert_eq!(view.read_with(cx, |v, _| v.flow.step), Step::Recovery);
    readonly_form(cx);
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| {
        view.receive(
            work.ticket,
            Ok(Reply::Recovery(crate::engine::reply::RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            })),
            cx,
        )
    });
    readonly_form(cx);
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| {
        view.receive(
            work.ticket,
            Ok(Reply::Profile {
                account: "fixture".into(),
                email: "a@b.test".into(),
                profile: crate::engine::reply::AccountProfile {
                    agreement_required: false,
                    replacement_required: false,
                    unlocked: true,
                    initialized: false,
                    display_name: None,
                },
            }),
            cx,
        )
    });
    assert_eq!(view.read_with(cx, |v, _| v.flow.step), Step::Profile);
    readonly_form(cx);
}

#[gpui::test]
fn failed_setup_can_change_account_only_after_sign_out(cx: &mut gpui::TestAppContext) {
    // Click the real secondary control throughout setup. An unsuccessful
    // sign-out keeps credentials locked; success clears secrets and enables editing.
    for step in [Step::Recovery, Step::SaveKey, Step::RestoreKey, Step::Profile, Step::Name] {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let dispatch = sent.clone();
        let (window, view) = cx.update(|cx| {
            open(
                ReleaseChannel::Public,
                false,
                Rc::new(move |work, _| {
                    record_submission(&dispatch, work);
                    Ok(())
                }),
                Rc::new(|_| {}),
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        sent.borrow_mut().clear();
        view.update(cx, |view, cx| {
            view.flow = Flow::new(true);
            view.flow.step = step;
            view.flow.email = "first@example.test".into();
            let key = crate::store::account_key::format(&[3; 16]);
            view.flow.saved_key = key.clone();
            let failed = view.flow.submit("first@example.test", &key, "123456", "Robin").unwrap();
            view.receive(failed.ticket, Err("Account data unavailable".into()), cx);
            view.email.update(cx, |field, cx| field.acknowledge(None, "first@example.test", cx));
            view.key.update(cx, |field, cx| {
                field.acknowledge(None, &crate::store::account_key::format(&[3; 16]), cx)
            });
            view.code.update(cx, |field, cx| field.acknowledge(None, "123456", cx));
            cx.notify();
        });
        cx.run_until_parked();
        let click = |label: &str, cx: &mut gpui::TestAppContext| {
            let bounds = window
                .update(cx, |_, window, _| {
                    let bounds = window
                        .accessibility_nodes_for_test()
                        .find(|(_, node)| node.label() == Some(label) && !node.is_disabled())
                        .unwrap()
                        .1
                        .bounds()
                        .unwrap();
                    gpui::Bounds::from_corners(
                        gpui::point(px(bounds.x0 as f32), px(bounds.y0 as f32)),
                        gpui::point(px(bounds.x1 as f32), px(bounds.y1 as f32)),
                    )
                })
                .unwrap();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.simulate_click(bounds.center(), Default::default());
            visual.simulate_click(bounds.center(), Default::default());
        };
        click("Sign Out", cx);
        assert_eq!(sent.borrow().len(), 1);
        let first = sent.borrow_mut().pop().unwrap();
        assert!(matches!(
            first.command,
            flow::Command::ChangeProfile(flow::ProfileChange::SignOut)
        ));
        view.update(cx, |view, cx| {
            view.receive(
                first.ticket,
                Err(flow::ProfileChange::SignOut.failure_message(false).into()),
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.flow.step, Step::SignOut);
                assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
                    node.value() == Some(flow::ProfileChange::SignOut.failure_message(false))
                }));
                let fields: Vec<_> = window
                    .accessibility_nodes_for_test()
                    .filter(|(_, n)| {
                        matches!(
                            n.role(),
                            gpui::accesskit::Role::TextInput
                                | gpui::accesskit::Role::PasswordInput
                                | gpui::accesskit::Role::MultilineTextInput
                        )
                    })
                    .collect();
                assert_eq!(fields.len(), 1, "only the selectable error text is presented");
                assert!(fields[0].1.is_read_only());
                assert_eq!(
                    fields[0].1.value(),
                    Some(flow::ProfileChange::SignOut.failure_message(false))
                );
                assert!(!view.key.read(cx).text().is_empty());
            })
            .unwrap();
        click("Retry", cx);
        let retry = sent.borrow_mut().pop().unwrap();
        view.update(cx, |view, cx| {
            view.receive(first.ticket, Ok(Reply::SignedOut), cx);
            assert!(view.flow.enrolled);
            view.receive(retry.ticket, Ok(Reply::SignedOut), cx);
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.flow.step, Step::Entry);
                assert!(view.key.read(cx).text().is_empty());
                assert!(view.code.read(cx).text().is_empty());
                assert!(view.email.focus_handle(cx).is_focused(window));
                view.email.update(cx, |field, cx| {
                    field.select_all(cx);
                    field.replace_text_in_range(None, "second@example.test", window, cx);
                });
                view.key.update(cx, |field, cx| {
                    field.replace_text_in_range(
                        None,
                        &crate::store::account_key::format(&[4; 16]),
                        window,
                        cx,
                    );
                });
                view.receive(retry.ticket, Ok(Reply::SignedOut), cx);
                assert!(
                    !view.key.read(cx).text().is_empty(),
                    "stale sign-out cannot clear new credentials"
                );
                view.submit(cx);
            })
            .unwrap();
        assert!(
            matches!(sent.borrow()[0].command, flow::Command::SignInWithKey { ref email, .. } if email == "second@example.test")
        );
        window.update(cx, |_, window, _| window.remove_window()).unwrap();
    }
}

#[gpui::test]
fn saved_key_profile_failure_has_its_own_layout_and_retries_the_profile_read(
    cx: &mut gpui::TestAppContext,
) {
    // Exercise the reported journey and pointer targets at every supported zoom.
    // A second click while Retry is pending cannot replay authentication or work.
    cx.update(zoom::install);
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    let message = "Playbit couldn’t check the user agreement with the sync service. Your account key is saved. Retry to continue setup.";
    view.update(cx, |view, cx| {
        view.flow.step = Step::Code;
        view.code.update(cx, |field, cx| field.acknowledge(None, "001002", cx));
        view.submit(cx);
        let work = sent.borrow_mut().pop().unwrap();
        view.receive(
            work.ticket,
            Ok(Reply::SignedIn(crate::engine::reply::SignInReply {
                replacement_required: false,
                signed_in: true,
                recovery: Some(crate::engine::reply::RecoveryStatus::Setup {
                    revision: 0,
                    pending_save: false,
                    secret: Some(crate::store::account_key::format(&[3; 16])),
                }),
                recovery_error: None,
            })),
            cx,
        );
        view.submit(cx);
        let work = sent.borrow_mut().pop().unwrap();
        assert!(matches!(work.command, flow::Command::SaveKey));
        view.receive(
            work.ticket,
            Ok(Reply::Recovery(crate::engine::reply::RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            })),
            cx,
        );
        let work = sent.borrow_mut().pop().unwrap();
        assert!(matches!(work.command, flow::Command::Profile));
        view.receive(work.ticket, Err(message.into()), cx);
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for units in zoom::SAMPLES {
        cx.update(|cx| zoom::set_scale(units, cx).unwrap());
        cx.run_until_parked();
        let size = window.update(cx, |_, window, _| window.bounds().size).unwrap();
        visual.simulate_resize(size);
        cx.run_until_parked();
        let panel = visual.debug_bounds("sign-in-error").unwrap();
        let heading = visual.debug_bounds("sign-in-heading").unwrap();
        let actions = visual.debug_bounds("sign-in-error-actions").unwrap();
        assert_eq!(heading.left(), panel.left());
        assert_eq!(actions.left(), panel.left());
        assert_eq!(actions.size.width, panel.size.width);
        assert!(actions.top() > panel.bottom());
        let expected_height = window.update(cx, |_, window, _| zoom::pixels(window, 102.)).unwrap();
        assert_eq!(panel.size.height, expected_height);
        let retry = window
            .update(cx, |view, window, _| {
                let nodes: Vec<_> = window.accessibility_nodes_for_test().map(|(_, n)| n).collect();
                let fields: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        matches!(
                            n.role(),
                            gpui::accesskit::Role::TextInput
                                | gpui::accesskit::Role::PasswordInput
                                | gpui::accesskit::Role::MultilineTextInput
                        )
                    })
                    .collect();
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].value(), Some(message));
                assert!(fields[0].is_read_only());
                assert_eq!(fields[0].live(), Some(gpui::accesskit::Live::Polite));
                let buttons: Vec<_> =
                    nodes.iter().filter(|n| n.role() == gpui::accesskit::Role::Button).collect();
                assert_eq!(buttons.len(), 2);
                assert!(buttons.iter().any(|n| n.label() == Some("Sign Out")));
                assert_eq!(view.flow.step, Step::Profile);
                let bounds =
                    buttons.iter().find(|n| n.label() == Some("Retry")).unwrap().bounds().unwrap();
                gpui::Bounds::from_corners(
                    gpui::point(px(bounds.x0 as f32), px(bounds.y0 as f32)),
                    gpui::point(px(bounds.x1 as f32), px(bounds.y1 as f32)),
                )
            })
            .unwrap();
        visual.simulate_click(retry.center(), Default::default());
        visual.simulate_click(retry.center(), Default::default());
        assert_eq!(sent.borrow().len(), 1);
        let work = sent.borrow_mut().pop().unwrap();
        assert!(matches!(work.command, flow::Command::Profile));
        view.read_with(cx, |view, _| {
            assert!(view.flow.busy());
            assert!(view.flow.error_screen());
        });
        view.update(cx, |view, cx| view.receive(work.ticket, Err(message.into()), cx));
    }
}

#[gpui::test]
fn failed_key_sign_in_restores_editing_and_long_key_feedback(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    view.update(cx, |view, cx| {
        view.email.update(cx, |field, cx| field.acknowledge(None, "a@b.test", cx));
        view.key.update(cx, |field, cx| {
            field.acknowledge(None, &crate::store::account_key::format(&[3; 16]), cx)
        });
        view.submit(cx);
    });
    cx.run_until_parked();
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| {
        view.receive(work.ticket, Err("Email or account key is incorrect".into()), cx)
    });
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.value() == Some("Invalid account key"))
            );
            view.key.update(cx, |field, cx| {
                field.select_all(cx);
                field.replace_text_in_range(None, &"a".repeat(24), window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |view, window, _| {
            assert!(view.flow.field_error.is_none());
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.label() == Some("Account key too long") && n.is_disabled())
            );
        })
        .unwrap();
}

#[gpui::test]
fn tab_cycles_fields_and_enabled_button_in_both_directions(cx: &mut gpui::TestAppContext) {
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.role() == gpui::accesskit::Role::ComboBox })
            );
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "tab");
    window
        .update(cx, |view, window, cx| {
            assert!(view.key.read(cx).focus_handle(cx).is_focused(window))
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "tab");
    window.update(cx, |view, window, cx| assert!(view.privacy.read(cx).focused(window))).unwrap();
    cx.simulate_keystrokes(window.into(), "tab");
    window
        .update(cx, |view, window, cx| {
            assert!(view.email.read(cx).focus_handle(cx).is_focused(window))
        })
        .unwrap();
    view.update(cx, |view, cx| {
        view.email.update(cx, |field, cx| field.acknowledge(None, "preview@example.test", cx))
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "tab tab");
    window
        .update(cx, |view, window, cx| {
            assert!(view.primary.read(cx).focused(window));
            assert!(
                view.primary.read(cx).keyboard_focus_visible(window),
                "Tab must show the shared prominent-button focus treatment"
            );
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "shift-tab");
    window
        .update(cx, |view, window, cx| {
            assert!(!view.primary.read(cx).keyboard_focus_visible(window));
            assert!(view.key.read(cx).focus_handle(cx).is_focused(window))
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "shift-tab");
    window
        .update(cx, |view, window, cx| {
            assert!(view.email.read(cx).focus_handle(cx).is_focused(window));
            view.email.update(cx, |field, cx| {
                let selection = field.selected_text_range(false, window, cx).unwrap();
                assert_eq!(selection.range, 0.."preview@example.test".len());
                field.replace_text_in_range(None, "replacement@example.test", window, cx);
                assert_eq!(field.text(), "replacement@example.test");
            });
        })
        .unwrap();
    view.update(cx, |view, cx| view.show_environment_selector(Some(Rc::new(|_, _| Ok(()))), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "shift-tab");
    window
        .update(cx, |view, window, cx| {
            let selector = view.channel_control.as_ref().unwrap().read(cx);
            assert!(selector.focused(window));
            assert!(selector.keyboard_focus_visible(window));
        })
        .unwrap();
}

#[gpui::test]
fn code_progress_removes_back_and_extra_status_text(cx: &mut gpui::TestAppContext) {
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    view.update(cx, |view, cx| {
        view.flow.step = Step::Code;
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            assert!(window.accessibility_nodes_for_test().any(|(_, n)| n.label() == Some("Back")));
            view.code
                .update(cx, |field, cx| field.replace_text_in_range(None, "012-345", window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            assert!(view.flow.busy());
            assert_eq!(view.code.read(cx).text(), "012345");
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.role() == gpui::accesskit::Role::Button)
            );
            assert!(
                !window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.value() == Some("Signing in…"))
            );
        })
        .unwrap();
}

#[gpui::test]
fn recovery_back_resets_then_focuses_editable_retained_email(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    view.update(cx, |view, cx| {
        view.flow = flow::Flow::new(true);
        view.flow.step = Step::RestoreKey;
        view.flow.email = "retained@example.test".into();
        view.email.update(cx, |field, cx| field.acknowledge(None, "retained@example.test", cx));
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| view.back(cx));
    cx.run_until_parked();
    let work = sent.borrow_mut().pop().unwrap();
    assert!(matches!(work.command, flow::Command::ChangeProfile(flow::ProfileChange::Reset)));
    view.update(cx, |view, cx| view.receive(work.ticket, Ok(Reply::Reset), cx));
    cx.run_until_parked();
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.email.read(cx).text(), "retained@example.test");
                assert!(view.email.focus_handle(cx).is_focused(window));
                assert!(view.key.read(cx).text().is_empty());
                view.email.update(cx, |field, cx| {
                    field.select_all(cx);
                    field.replace_text_in_range(None, "different@example.test", window, cx);
                });
            })
            .unwrap()
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| view.submit(cx));
    let work = sent.borrow_mut().pop().unwrap();
    assert!(
        matches!(work.command, flow::Command::RequestCode(ref email) if email == "different@example.test")
    );
}

#[gpui::test]
fn restored_key_keeps_its_form_through_metadata_loading(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    let key = crate::store::account_key::format(&[3; 16]);
    view.update(cx, |view, cx| {
        view.flow.step = Step::Code;
        view.code.update(cx, |field, cx| field.acknowledge(None, "001002", cx));
        view.submit(cx);
        let work = sent.borrow_mut().pop().unwrap();
        view.receive(
            work.ticket,
            Ok(Reply::SignedIn(crate::engine::reply::SignInReply {
                replacement_required: false,
                signed_in: true,
                recovery: Some(crate::engine::reply::RecoveryStatus::Locked {
                    revision: 1,
                    pending_save: false,
                }),
                recovery_error: None,
            })),
            cx,
        );
        let work = sent.borrow_mut().pop().unwrap();
        view.receive(
            work.ticket,
            Ok(Reply::Profile {
                account: "fixture".into(),
                email: "a@b.test".into(),
                profile: crate::engine::reply::AccountProfile {
                    agreement_required: false,
                    replacement_required: false,
                    unlocked: false,
                    initialized: false,
                    display_name: None,
                },
            }),
            cx,
        );
        view.key.update(cx, |field, cx| field.acknowledge(None, &key, cx));
        view.submit(cx);
    });
    let assert_recovery_form = |cx: &mut gpui::TestAppContext| {
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.flow.presentation(), Step::RestoreKey);
                assert_eq!(
                    view.code.read(cx).text(),
                    "001002",
                    "old code must not choose the progress form"
                );
                assert_eq!(view.key.read(cx).text(), key);
                let nodes: Vec<_> =
                    window.accessibility_nodes_for_test().map(|(_, n)| n.clone()).collect();
                let fields: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        matches!(
                            n.role(),
                            gpui::accesskit::Role::TextInput | gpui::accesskit::Role::PasswordInput
                        )
                    })
                    .collect();
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].role(), gpui::accesskit::Role::PasswordInput);
                assert!(fields[0].is_read_only());
                assert!(
                    !nodes
                        .iter()
                        .any(|n| n.label() == Some("Use Account Key") || n.label() == Some("Help"))
                );
            })
            .unwrap();
    };
    assert_recovery_form(cx);
    view.update(cx, |view, cx| {
        let work = sent.borrow_mut().pop().unwrap();
        view.receive(
            work.ticket,
            Ok(Reply::Recovery(crate::engine::reply::RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            })),
            cx,
        );
    });
    assert_recovery_form(cx);
    view.update(cx, |view, cx| {
        let work = sent.borrow_mut().pop().unwrap();
        view.receive(
            work.ticket,
            Ok(Reply::Profile {
                account: "fixture".into(),
                email: "a@b.test".into(),
                profile: crate::engine::reply::AccountProfile {
                    agreement_required: false,
                    replacement_required: false,
                    unlocked: true,
                    initialized: false,
                    display_name: None,
                },
            }),
            cx,
        );
    });
    assert_recovery_form(cx);
}

#[gpui::test]
fn account_id_copy_supports_pointer_keyboard_and_accessibility(cx: &mut gpui::TestAppContext) {
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    let account = "0123456789abcdef0123456789abcdef";
    view.update(cx, |view, cx| {
        view.flow.step = Step::RestoreKey;
        view.flow.account = account.into();
        cx.notify();
    });
    cx.run_until_parked();
    let bounds = window
        .update(cx, |_, window, _| {
            let (_, node) = window
                .accessibility_nodes_for_test()
                .find(|(_, n)| n.label() == Some("Copy account ID"))
                .unwrap();
            let bounds = node.bounds().unwrap();
            gpui::Bounds::from_corners(
                gpui::point(px(bounds.x0 as f32), px(bounds.y0 as f32)),
                gpui::point(px(bounds.x1 as f32), px(bounds.y1 as f32)),
            )
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.simulate_mouse_down(bounds.center(), gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(
        gpui::point(px(1.), px(1.)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    assert!(cx.update(|cx| cx.read_from_clipboard()).is_none(), "outside release cancels copy");
    visual.simulate_click(bounds.center(), Default::default());
    assert_eq!(cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()), account);
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            let (_, node) = window
                .accessibility_nodes_for_test()
                .find(|(_, n)| n.label() == Some("Copy account ID"))
                .unwrap();
            assert_eq!(node.role(), gpui::accesskit::Role::Button);
            assert_eq!(node.description(), Some(format!("Account ID copied: {account}").as_str()));
            view.account_id.read(cx).focus(window);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "space");
    cx.dispatch_key_up(window.into(), gpui::Keystroke::parse("space").unwrap());
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()), account);
    // Each way of leaving the control clears the announcement, including
    // returning the pointer afterward (which must not revive old feedback).
    for departure in 0..3 {
        match departure {
            0 => visual.simulate_mouse_move(gpui::point(px(1.), px(1.)), None, Default::default()),
            1 => {
                cx.simulate_keystrokes(window.into(), "tab");
                window
                    .update(cx, |view, window, cx| {
                        assert!(
                            !view.account_id.read(cx).focused(window),
                            "Tab moves away from copy"
                        );
                    })
                    .unwrap();
            }
            _ => visual.deactivate_window(),
        }
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                let (_, node) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, n)| n.label() == Some("Copy account ID"))
                    .unwrap();
                assert_eq!(node.description(), Some(account), "departure {departure}");
                window.activate_window();
            })
            .unwrap();
        visual.simulate_mouse_move(bounds.center(), None, Default::default());
        window.update(cx, |view, window, cx| view.account_id.read(cx).focus(window)).unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "space");
        cx.dispatch_key_up(window.into(), gpui::Keystroke::parse("space").unwrap());
        cx.run_until_parked();
    }
    view.update(cx, |view, cx| {
        view.flow.account = "fedcba9876543210fedcba9876543210".into();
        cx.notify();
    });
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            let (_, node) = window
                .accessibility_nodes_for_test()
                .find(|(_, n)| n.label() == Some("Copy account ID"))
                .unwrap();
            assert_eq!(node.description(), Some("fedcba9876543210fedcba9876543210"));
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "enter");
    cx.dispatch_key_up(window.into(), gpui::Keystroke::parse("enter").unwrap());
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()),
        "fedcba9876543210fedcba9876543210"
    );
}

#[gpui::test]
fn rejected_recovery_key_requires_editing_but_transport_failure_can_retry(
    cx: &mut gpui::TestAppContext,
) {
    // The same valid-format key must not be resubmitted after rejection, even via Return.
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    view.update(cx, |view, cx| {
        view.flow.step = Step::RestoreKey;
        view.flow.enrolled = true;
        view.key.update(cx, |key, cx| {
            key.acknowledge(None, &crate::store::account_key::format(&[3; 16]), cx)
        });
        view.submit(cx);
    });
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| view.receive(work.ticket, Err("invalid account key".into()), cx));
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.label() == Some("Use Account Key") && n.is_disabled())
            );
            view.submit(cx);
        })
        .unwrap();
    assert!(sent.borrow().is_empty());
    window
        .update(cx, |view, window, cx| {
            view.key.update(cx, |key, cx| {
                key.select_all(cx);
                key.replace_text_in_range(
                    None,
                    &crate::store::account_key::format(&[4; 16]),
                    window,
                    cx,
                );
            });
        })
        .unwrap();
    cx.run_until_parked();
    view.update(cx, |view, cx| view.submit(cx));
    let work = sent.borrow_mut().pop().unwrap();
    view.update(cx, |view, cx| view.receive(work.ticket, Err("Service unavailable".into()), cx));
    cx.run_until_parked();
    window
        .update(cx, |view, window, cx| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, n)| n.label() == Some("Retry") && !n.is_disabled())
            );
            view.submit(cx);
        })
        .unwrap();
    assert_eq!(sent.borrow().len(), 1);
}

#[gpui::test]
fn replacement_buttons_dispatch_one_explicit_decision(cx: &mut gpui::TestAppContext) {
    // Pointer hit testing and the default safe focus survive reduced and enlarged zoom.
    for (replace, delta) in [(false, -2), (true, -2), (false, 0), (true, 0), (false, 1), (true, 1)]
    {
        cx.update(|cx| {
            zoom::install(cx);
            zoom::actual_size(cx);
            zoom::change(cx, delta);
        });
        let sent = Rc::new(RefCell::new(Vec::new()));
        let dispatch = sent.clone();
        let (window, view) = cx.update(|cx| {
            open(
                ReleaseChannel::Public,
                false,
                Rc::new(move |work, _| {
                    record_submission(&dispatch, work);
                    Ok(())
                }),
                Rc::new(|_| {}),
                cx,
            )
            .unwrap()
        });
        view.update(cx, |view, cx| {
            view.flow.step = Step::Replace;
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = window
            .update(cx, |view, window, cx| {
                assert!(
                    view.sign_out.read(cx).focused(window),
                    "initial focus preserves existing data"
                );
                for label in ["Sign Out", "Replace"] {
                    assert!(
                        window
                            .accessibility_nodes_for_test()
                            .any(|(_, n)| n.label() == Some(label))
                    );
                }
                let (_, node) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, n)| n.label() == Some(if replace { "Replace" } else { "Sign Out" }))
                    .unwrap();
                let b = node.bounds().unwrap();
                gpui::Bounds::from_corners(
                    gpui::point(px(b.x0 as f32), px(b.y0 as f32)),
                    gpui::point(px(b.x1 as f32), px(b.y1 as f32)),
                )
            })
            .unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(bounds.center(), Default::default());
        visual.simulate_click(bounds.center(), Default::default());
        assert_eq!(sent.borrow().len(), 1);
        assert!(
            matches!(sent.borrow()[0].command, flow::Command::ChangeProfile(flow::ProfileChange::Replace(choice)) if choice == replace)
        );
        window.update(cx, |_, window, _| window.remove_window()).unwrap();
    }
}

#[gpui::test]
fn zoom_sizes_new_sign_in_windows_and_all_form_steps(cx: &mut gpui::TestAppContext) {
    // Open at an existing zoom, then exercise live zoom on every form shape.
    cx.update(|cx| {
        zoom::install(cx);
        zoom::change(cx, 2);
    });
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    window
        .update(cx, |_, window, _| {
            assert_eq!(window.bounds().size, gpui::size(px(600.), px(540.)));
            assert_eq!(window.rem_size(), px(24.));
        })
        .unwrap();
    for step in [
        Step::Entry,
        Step::Reset,
        Step::SignOut,
        Step::Code,
        Step::Recovery,
        Step::RestoreKey,
        Step::SaveKey,
        Step::Profile,
        Step::Agreement,
        Step::Name,
        Step::Replace,
        Step::Complete,
    ] {
        view.update(cx, |view, cx| {
            view.flow.step = step;
            cx.notify();
        });
        for units in zoom::SAMPLES {
            let scale = units as f32 / 10_000.;
            cx.update(|cx| {
                zoom::actual_size(cx);
                zoom::set_scale(units, cx).expect("supported test scale");
            });
            cx.run_until_parked();
            // The fake platform's programmatic resize does not emit AppKit's
            // content-size callback. Deliver it before the next live change.
            let bounds = window.update(cx, |_, window, _| window.bounds()).unwrap();
            cx.simulate_window_resize(window.into(), bounds.size);
            cx.run_until_parked();
            window
                .update(cx, |_, window, _| {
                    assert_eq!(window.rem_size(), px(16. * scale));
                    assert!(window.accessibility_error().is_none());
                    assert_eq!(window.bounds().size, window_size(scale));
                })
                .unwrap();
        }
        cx.update(|cx| zoom::change(cx, 10));
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                assert_eq!(window.bounds().size, gpui::size(px(800.), px(720.)));
                assert_eq!(window.rem_size(), px(32.));
            })
            .unwrap();
        cx.update(zoom::actual_size);
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                assert_eq!(window.bounds().size, gpui::size(px(400.), px(360.)));
            })
            .unwrap();
    }
    cx.update(|cx| zoom::change(cx, -10));
    cx.run_until_parked();
    window
        .update(cx, |_, window, _| {
            assert_eq!(window.bounds().size, gpui::size(px(400.), px(360.)));
            assert_eq!(window.rem_size(), px(8.));
        })
        .unwrap();
}

#[gpui::test]
fn reduced_zoom_centers_the_heading_and_form_together(cx: &mut gpui::TestAppContext) {
    cx.update(zoom::install);
    // Check both the entry form and the separately sized replacement confirmation.
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (step, width) in [(Step::Entry, 208.), (Step::Replace, 226.)] {
        cx.update(zoom::actual_size);
        view.update(cx, |view, cx| {
            view.flow.step = step;
            cx.notify();
        });
        cx.run_until_parked();
        for delta in [-2, 1, 1, 1] {
            cx.update(|cx| zoom::change(cx, delta));
            cx.run_until_parked();
            // TestWindow::resize changes its bounds but does not send the OS resize callback.
            let size = window.update(cx, |_, window, _| window.bounds().size).unwrap();
            visual.simulate_resize(size);
            cx.run_until_parked();
            let canvas = visual.debug_bounds("sign-in-canvas").expect("canvas rendered");
            let heading = visual.debug_bounds("sign-in-heading").expect("heading rendered");
            let form = visual.debug_bounds("sign-in-form").expect("form rendered");
            let (size, width) = window
                .update(cx, |_, window, _| (window.bounds().size, zoom::pixels(window, width)))
                .unwrap();
            assert_eq!(canvas.center(), gpui::point(size.width / 2., size.height / 2.));
            assert_eq!(heading.center().x, canvas.center().x);
            // Snapping an odd-width form can shift its center by half a physical pixel.
            let tolerance =
                window.update(cx, |_, window, _| px(0.5 / window.scale_factor())).unwrap();
            assert!((form.center().x - canvas.center().x).abs() <= tolerance);
            assert!(heading.bottom() <= form.top());
            assert_eq!(form.size.width, width);
        }
    }
}

#[gpui::test]
fn saved_account_fills_untouched_fields_without_submitting(cx: &mut gpui::TestAppContext) {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (_, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                dispatch.borrow_mut().push(work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    cx.run_until_parked();
    let lookup = sent.borrow_mut().pop().unwrap();
    assert!(matches!(lookup.command, flow::Command::SavedAccount));
    let key = crate::store::account_key::format(&[9; 16]);
    view.update(cx, |view, cx| {
        view.receive(
            lookup.ticket,
            Ok(Reply::SavedAccount(Some(crate::engine::reply::SavedAccount {
                email: "saved@example.test".into(),
                account_key: key.clone(),
            }))),
            cx,
        );
        assert_eq!(view.email.read(cx).text(), "saved@example.test");
        assert_eq!(view.key.read(cx).text(), key);
        assert!(view.valid(cx));
        assert!(!view.flow.enrolled);
    });
    cx.run_until_parked();
    assert!(sent.borrow().is_empty());
}

#[gpui::test]
fn agreement_requires_scroll_end_and_save_exports_the_displayed_document(
    cx: &mut gpui::TestAppContext,
) {
    // Exercise the actual scroll/focus tree, then the native save-panel result.
    // Neither scrolling nor saving may submit an acceptance.
    let sent = Rc::new(RefCell::new(Vec::new()));
    let dispatch = sent.clone();
    let (window, view) = cx.update(|cx| {
        open(
            ReleaseChannel::Public,
            false,
            Rc::new(move |work, _| {
                record_submission(&dispatch, work);
                Ok(())
            }),
            Rc::new(|_| {}),
            cx,
        )
        .unwrap()
    });
    view.update(cx, |view, cx| {
        view.flow.account = crate::store::Id::from_bytes([1; 16]).to_string();
        view.flow.step = Step::Profile;
        view.profile(
            &crate::engine::reply::AccountProfile {
                agreement_required: true,
                replacement_required: false,
                display_name: None,
                unlocked: true,
                initialized: false,
            },
            cx,
        );
        assert_eq!(view.flow.step, Step::Agreement);
    });
    cx.run_until_parked();
    window
        .update(cx, |view, window, _| {
            assert_eq!(window.viewport_size(), gpui::size(px(400.), px(360.)));
            assert!(!view.flow.agreement_read);
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| node.label() == Some("Accept") && node.is_disabled())
            );
        })
        .unwrap();
    view.update(cx, |view, cx| view.submit(cx));
    assert!(sent.borrow().is_empty());
    cx.simulate_keystrokes(window.into(), "end");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.flow.agreement_read));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agreement.md");
    view.update(cx, |view, cx| view.save_agreement(cx));
    assert!(cx.did_prompt_for_new_path());
    cx.simulate_new_path_selection(|_| Some(path.clone()));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(path).unwrap(), agreement::DOCUMENT);
    assert!(sent.borrow().is_empty());
    view.update(cx, |view, cx| view.submit(cx));
    assert!(matches!(sent.borrow_mut().pop().unwrap().command, flow::Command::AcceptAgreement(_)));
}

#[gpui::test]
fn long_errors_stay_inside_the_window_scroll_and_allow_copy(cx: &mut gpui::TestAppContext) {
    // Exercise the bounded panel at each zoom, then select and copy its actual text.
    cx.update(|cx| {
        zoom::install(cx);
        super::super::text_editor::install_keybindings(cx);
    });
    let (window, view) = cx.update(|cx| {
        open(ReleaseChannel::Public, false, Rc::new(|_, _| Ok(())), Rc::new(|_| {}), cx).unwrap()
    });
    let message = format!(
        "Store error\nProfile: /private/tmp/my-profile\n{}",
        "A long diagnostic message that wraps over several lines. ".repeat(16)
    );
    view.update(cx, |view, cx| {
        view.flow.error = message.clone();
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (step, units) in [Step::Entry, Step::Code, Step::RestoreKey, Step::Replace, Step::Name]
        .into_iter()
        .flat_map(|step| zoom::SAMPLES.map(|units| (step, units)))
    {
        view.update(cx, |view, cx| {
            view.flow.step = step;
            cx.notify();
        });
        cx.update(|cx| zoom::set_scale(units, cx).unwrap());
        cx.run_until_parked();
        let size = window.update(cx, |_, window, _| window.bounds().size).unwrap();
        visual.simulate_resize(size);
        cx.run_until_parked();
        let panel = visual.debug_bounds("sign-in-error").expect("error panel rendered");
        let margin = window.update(cx, |_, window, _| zoom::pixels(window, 24.)).unwrap();
        assert!(panel.left() >= margin && panel.top() >= margin);
        assert!(panel.right() <= size.width - margin);
        assert!(
            panel.bottom() <= size.height - margin,
            "{step:?} {units}: panel {panel:?}, window {size:?}, margin {margin:?}"
        );
        assert!(panel.size.height > px(20.));
        let scroll = view.read_with(cx, |view, cx| view.error_scrollbars.read(cx).handle());
        assert!(scroll.max_offset().height > px(0.));
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: panel.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-40.))),
            ..Default::default()
        });
        assert!(scroll.offset().y < px(0.));
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: panel.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(40.))),
            ..Default::default()
        });
    }
    window.update(cx, |view, window, cx| window.focus(&view.error.focus_handle(cx))).unwrap();
    cx.simulate_keystrokes(window.into(), "cmd-a cmd-c backspace");
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()), message);
    view.read_with(cx, |view, cx| assert_eq!(view.error.read(cx).text(), message));
    let path = "/private/tmp/my-profile";
    cx.simulate_keystrokes(
        window.into(),
        &format!(
            "cmd-up {} {} cmd-c",
            "right ".repeat(message.find(path).unwrap()),
            "shift-right ".repeat(path.len())
        ),
    );
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()), path);
    view.update(cx, |view, cx| {
        view.flow.error = "Short error".into();
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        let scroll = view.error_scrollbars.read(cx).handle();
        assert_eq!(scroll.offset().y, px(0.));
        assert_eq!(scroll.max_offset().height, px(0.));
    });
}
