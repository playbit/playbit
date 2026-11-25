//! Interactive visual fixture for the shared account window. Key restoration stays
//! pending; synthetic code/profile/reset replies exercise recovery navigation.
//! Uses no engine, profile, credentials or network. Enter synthetic values only.
use gpui::{Application, KeyBinding, Menu, MenuItem};
use playbit::ui::desktop::{controls, sign_in, text_editor, zoom};
use std::{borrow::Cow, cell::Cell, rc::Rc};

gpui::actions!(
    sign_in_fixture,
    [
        Quit,
        EnterCode,
        NewAccount,
        UserAgreement,
        Measure,
        ProvideKey,
        RejectKey,
        LongError,
        SetupError,
        ReplaceData,
        ActivatePreview,
        ZoomIn,
        ZoomOut,
        ActualSize
    ]
);

fn main() {
    let startup = std::time::Instant::now();
    Application::new().with_assets(controls::Assets).run(move |cx| {
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../assets/InterVariable.ttf"))])
            .expect("bundled font loads");
        text_editor::install_keybindings(cx);
        zoom::install(cx);
        cx.on_action(|_: &ZoomIn, cx| zoom::change(cx, 1));
        cx.on_action(|_: &ZoomOut, cx| zoom::change(cx, -1));
        cx.on_action(|_: &ActualSize, cx| zoom::actual_size(cx));
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![Menu {
            name: "Sign-in Preview".into(),
            items: vec![
                MenuItem::action("Bring Preview to Front", ActivatePreview),
                MenuItem::action("Enter Code", EnterCode),
                MenuItem::action("Create New Account", NewAccount),
                MenuItem::action("User Agreement", UserAgreement),
                MenuItem::action("Measure Current Screen", Measure),
                MenuItem::action("Replace Existing Data", ReplaceData),
                MenuItem::action("Provide account key", ProvideKey),
                MenuItem::action("Reject submitted key", RejectKey),
                MenuItem::action("Long Error", LongError),
                MenuItem::action("Setup Error", SetupError),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ActualSize),
                MenuItem::action("Quit", Quit),
            ],
        }]);
        let target = Rc::new(Cell::new(None::<gpui::WindowHandle<sign_in::SignIn>>));
        let reply_target = target.clone();
        let pending_key = Rc::new(Cell::new(None));
        let key_reply = pending_key.clone();
        let creating = Rc::new(Cell::new(false));
        let new_account = creating.clone();
        let accepted = Rc::new(Cell::new(false));
        let accepted_reply = accepted.clone();
        let (window, view) = sign_in::open(
            playbit::release_channel::ReleaseChannel::Public,
            false,
            Rc::new(move |work, cx| {
                let reply = match work.command {
                    sign_in::flow::Command::RestoreKey(_) => {
                        key_reply.set(Some(work.ticket));
                        None
                    }
                    sign_in::flow::Command::RequestCode(_) => Some(sign_in::flow::Reply::CodeSent),
                    sign_in::flow::Command::SignIn(_) => {
                        Some(sign_in::flow::Reply::SignedIn(playbit::engine::reply::SignInReply {
                            replacement_required: false,
                            signed_in: true,
                            recovery: Some(if new_account.get() {
                                playbit::engine::reply::RecoveryStatus::Setup {
                                    revision: 0,
                                    pending_save: false,
                                    secret: Some(playbit::store::account_key::format(&[7; 16])),
                                }
                            } else {
                                playbit::engine::reply::RecoveryStatus::Locked {
                                    revision: 1,
                                    pending_save: false,
                                }
                            }),
                            recovery_error: None,
                        }))
                    }
                    sign_in::flow::Command::ChangeProfile(change) => Some(match change {
                        sign_in::flow::ProfileChange::Replace(replace) => {
                            sign_in::flow::Reply::ReplacementResolved(replace)
                        }
                        sign_in::flow::ProfileChange::Reset => sign_in::flow::Reply::Reset,
                        sign_in::flow::ProfileChange::SignOut => sign_in::flow::Reply::SignedOut,
                    }),
                    sign_in::flow::Command::SaveKey => Some(sign_in::flow::Reply::Recovery(
                        playbit::engine::reply::RecoveryStatus::Ready {
                            revision: 1,
                            pending_save: false,
                            restored_sessions: None,
                            restored_volumes: None,
                        },
                    )),
                    sign_in::flow::Command::AcceptAgreement(_) => {
                        accepted_reply.set(true);
                        Some(sign_in::flow::Reply::AgreementAccepted)
                    }
                    sign_in::flow::Command::SaveName(_) => Some(sign_in::flow::Reply::NameSaved),
                    sign_in::flow::Command::Initialize => Some(sign_in::flow::Reply::Initialized),
                    sign_in::flow::Command::Profile => Some(sign_in::flow::Reply::Profile {
                        account: playbit::store::Id::from_bytes([1; 16]).to_string(),
                        email: "focus@example.test".into(),
                        profile: playbit::engine::reply::AccountProfile {
                            agreement_required: new_account.get() && !accepted_reply.get(),
                            replacement_required: false,
                            display_name: None,
                            unlocked: new_account.get(),
                            initialized: new_account.get() && accepted_reply.get(),
                        },
                    }),
                    _ => None,
                };
                if let Some(reply) = reply {
                    let target = reply_target.clone();
                    cx.defer(move |cx| {
                        target
                            .get()
                            .expect("fixture window exists")
                            .update(cx, |view, _, cx| {
                                view.receive(work.ticket, Ok(reply), cx);
                            })
                            .expect("fixture reset reply");
                    });
                }
                Ok(())
            }),
            Rc::new(|cx| cx.quit()),
            cx,
        )
        .expect("sign-in fixture opens");
        target.set(Some(window));
        println!("startup_to_first_scene_ms={}", startup.elapsed().as_secs_f64() * 1000.);
        let measured: gpui::AnyWindowHandle = window.into();
        let measured_view = view.clone();
        cx.on_action(move |_: &Measure, cx| {
            let scroll_error = !measured_view.read(cx).flow.error.is_empty();
            let error_y = if measured_view.read(cx).flow.error_screen() { 188. } else { 300. };
            cx.defer(move |cx| {
                let mut samples = Vec::with_capacity(1000);
                for index in 0..1000 {
                    let start = std::time::Instant::now();
                    measured
                        .update(cx, |_, window, cx| {
                            let input = if scroll_error {
                                let scale = f32::from(window.rem_size()) / 16.;
                                gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                    position: gpui::point(
                                        window.viewport_size().width / 2.,
                                        (window.viewport_size().height - gpui::px(360. * scale))
                                            / 2.
                                            + gpui::px(error_y * scale),
                                    ),
                                    delta: gpui::ScrollDelta::Pixels(gpui::point(
                                        gpui::px(0.),
                                        gpui::px(if index % 2 == 0 { -16. } else { 16. }),
                                    )),
                                    ..Default::default()
                                })
                            } else {
                                gpui::PlatformInput::KeyDown(gpui::KeyDownEvent {
                                    keystroke: gpui::Keystroke::parse(if index % 2 == 0 {
                                        "home"
                                    } else {
                                        "end"
                                    })
                                    .expect("fixture key"),
                                    is_held: false,
                                })
                            };
                            window.dispatch_event(input, cx);
                            let _ = window.draw(cx);
                        })
                        .expect("fixture draw");
                    samples.push(start.elapsed().as_secs_f64() * 1000.);
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "input_to_cpu_scene_ms n=1000 p50={} p99={} max={}",
                    samples[499], samples[989], samples[999]
                );
            });
        });

        let agreement = view.clone();
        let creating_agreement = creating.clone();
        let creating_setup = creating.clone();
        let accepted_setup = accepted.clone();
        cx.on_action(move |_: &UserAgreement, cx| {
            creating_agreement.set(true);
            accepted.set(false);
            agreement.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(true);
                view.flow.account = playbit::store::Id::from_bytes([1; 16]).to_string();
                view.flow.step = sign_in::flow::Step::Profile;
                view.profile(
                    &playbit::engine::reply::AccountProfile {
                        agreement_required: true,
                        replacement_required: false,
                        display_name: None,
                        unlocked: true,
                        initialized: false,
                    },
                    cx,
                );
            });
        });
        let new_account = view.clone();
        cx.on_action(move |_: &NewAccount, cx| {
            creating.set(true);
            new_account.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(false);
                view.flow.step = sign_in::flow::Step::Code;
                cx.notify();
            });
        });
        let replacement = view.clone();
        cx.on_action(move |_: &ReplaceData, cx| {
            replacement.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(false);
                view.flow.step = sign_in::flow::Step::Replace;
                cx.notify();
            });
        });
        let rejected = view.clone();
        let error = view.clone();
        let setup_error = view.clone();
        cx.on_action(move |_: &SetupError, cx| {
            creating_setup.set(true);
            accepted_setup.set(true);
            setup_error.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(true);
                view.flow.step = sign_in::flow::Step::Profile;
                view.flow.initialization_failed(
                    "Playbit couldn’t check the user agreement with the sync service. \
                     Your account key is saved. Retry to continue setup."
                        .into(),
                );
                cx.notify();
            });
        });
        cx.on_action(move |_: &LongError, cx| {
            error.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(false);
                view.flow.error = format!(
                    "Store error: malformed sync message\nProfile: /private/tmp/my-profile\n{}",
                    "A long diagnostic message that wraps over several lines. ".repeat(16)
                );
                cx.notify();
            });
        });
        cx.on_action(move |_: &RejectKey, cx| {
            if let Some(ticket) = pending_key.take() {
                rejected.update(cx, |view, cx| {
                    view.receive(ticket, Err("invalid account key".into()), cx)
                });
            }
        });
        let recovery = view.clone();
        cx.on_action(move |_: &ProvideKey, cx| {
            recovery.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(true);
                let work = view.flow.start().expect("fixture recovery lookup");
                view.receive(
                    work.ticket,
                    Ok(sign_in::flow::Reply::Recovery(
                        playbit::engine::reply::RecoveryStatus::Locked {
                            revision: 1,
                            pending_save: false,
                        },
                    )),
                    cx,
                );
                cx.notify();
            });
        });
        let activate = view.clone();
        cx.on_action(move |_: &ActivatePreview, cx| {
            activate.update(cx, |view, cx| view.activate(cx));
            cx.activate(true);
        });
        cx.on_action(move |_: &EnterCode, cx| {
            view.update(cx, |view, cx| {
                view.flow = sign_in::flow::Flow::new(false);
                view.flow.step = sign_in::flow::Step::Code;
                cx.notify();
            });
        });
        cx.activate(true);
    });
}
