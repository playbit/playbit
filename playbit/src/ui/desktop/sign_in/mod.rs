//! Shared macOS account window. The two hosts supply bounded background adapters;
//! no authentication, storage, or network operation runs in an input callback.
mod agreement;
pub mod flow;
mod progress;
pub mod validation;
use super::{controls, scrollbar::Scrollbars, text_editor::TextEditor, visual_bell, zoom};
use crate::release_channel::ReleaseChannel;
use flow::{Flow, Reply, Step, Ticket, Work};
use gpui::{App, Context, Entity, Focusable, Window, WindowHandle, div, prelude::*, px};
use playbit_api::ui::{ButtonStyle, Color, Theme, ThemeColor};
use std::{
    rc::Rc,
    sync::{Arc, OnceLock},
};

pub type Dispatch = Rc<dyn Fn(Work, &mut App) -> Result<(), String>>;
pub type Complete = Rc<dyn Fn(&mut App)>;
pub type SelectChannel =
    Rc<dyn Fn(crate::release_channel::ReleaseChannel, &mut App) -> Result<(), String>>;
const FEEDBACK_TEXT: usize = 4;
pub struct SignIn {
    pub flow: Flow,
    email: Entity<TextEditor>,
    key: Entity<TextEditor>,
    code: Entity<TextEditor>,
    name: Entity<TextEditor>,
    saved: Entity<TextEditor>,
    error: Entity<TextEditor>,
    error_scrollbars: Entity<Scrollbars>,
    progress: Entity<progress::Progress>,
    primary: Entity<controls::Control>,
    sign_out: Entity<controls::Control>,
    back: Entity<controls::Control>,
    help: Entity<controls::Control>,
    privacy: Entity<controls::Control>,
    channel_control: Entity<controls::Control>,
    channel: crate::release_channel::ReleaseChannel,
    channel_busy: bool,
    select_channel: Option<SelectChannel>,
    agreement_save: Entity<controls::Control>,
    agreement_decline: Entity<controls::Control>,
    agreement: agreement::Agreement,
    account_id: Entity<controls::Control>,
    dispatch: Dispatch,
    complete: Complete,
    window: Option<WindowHandle<Self>>,
    focus_step: Option<Step>,
    focus_error: bool,
    finished: bool,
    feedback: Option<(String, gpui::Task<()>)>,
    text_ids: [Entity<()>; 8],
}
fn theme() -> Theme {
    let mut theme = Theme::light();
    theme.set_color(ThemeColor::FocusRing, Color(0x000000b3));
    theme.set_color(ThemeColor::PanelBackground, Color(0x00000000));
    theme
}
fn artwork() -> &'static [Arc<gpui::RenderImage>; 4] {
    static IMAGES: OnceLock<[Arc<gpui::RenderImage>; 4]> = OnceLock::new();
    IMAGES.get_or_init(|| {
        [
            include_bytes!("../../../../assets/sign-in-bg.png").as_slice(),
            include_bytes!("../../../../assets/sign-in-bg-caution.png").as_slice(),
            include_bytes!("../../../../assets/sign-in-symbol-green.png").as_slice(),
            include_bytes!("../../../../assets/sign-in-symbol-yellow.png").as_slice(),
        ]
        .map(|bytes| {
            let mut pixels = image::load_from_memory(bytes)
                .expect("bundled sign-in texture decodes")
                .into_rgba8();
            for pixel in pixels.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            Arc::new(gpui::RenderImage::new([image::Frame::new(pixels)]))
        })
    })
}
fn window_size(scale: f32) -> gpui::Size<gpui::Pixels> {
    gpui::size(px(400. * scale.max(1.)), px(360. * scale.max(1.)))
}
pub fn open(
    channel: ReleaseChannel,
    enrolled: bool,
    dispatch: Dispatch,
    complete: Complete,
    cx: &mut App,
) -> gpui::Result<(WindowHandle<SignIn>, Entity<SignIn>)> {
    let entity = cx.new(|cx| SignIn::new(channel, enrolled, dispatch, complete, cx));
    let view = entity.clone();
    let handle = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
                None,
                window_size(zoom::factor(cx)),
                cx,
            ))),
            titlebar: None,
            is_resizable: false,
            is_minimizable: false,
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| {
            super::appearance::set_light_panel(window);
            zoom::apply(window, cx);
            window.on_window_should_close(cx, |_, _| false);
            view
        },
    )?;
    entity.update(cx, |this, cx| {
        this.window = Some(handle);
        let work = this.flow.start();
        if work.is_some() {
            this.send(work, cx);
        } else {
            // Let the host apply an already-known replacement decision first.
            let view = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = view.update(cx, |view, cx| view.send(None, cx));
            });
        }
    });
    Ok((handle, entity))
}
impl SignIn {
    pub fn activate(&self, cx: &mut Context<Self>) {
        if let Some(window) = self.window {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            });
        }
    }
    fn new(
        channel: ReleaseChannel,
        enrolled: bool,
        dispatch: Dispatch,
        complete: Complete,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = |placeholder: &'static str,
                      limit: usize,
                      secret: bool,
                      code: bool,
                      step: Step,
                      cx: &mut Context<Self>| {
            let weak = weak.clone();
            let commit = weak.clone();
            cx.new(|cx| {
                let mut editor = TextEditor::new("", cx)
                    .placeholder(placeholder)
                    .bound_edits(limit)
                    .on_change(Arc::new(move |_, _, cx| {
                        let weak = weak.clone();
                        cx.defer(move |cx| {
                            let _ = weak.update(cx, |this: &mut SignIn, cx| {
                                if this.flow.step != step
                                    && !(secret && this.flow.step == Step::RestoreKey)
                                {
                                    return;
                                }
                                this.flow.edited();
                                if code && validation::code(this.code.read(cx).text()) {
                                    this.submit(cx);
                                }
                                cx.notify();
                            });
                        });
                        true
                    }))
                    .on_commit(Arc::new(move |_, _, cx| {
                        let weak = commit.clone();
                        cx.defer(move |cx| {
                            let _ = weak.update(cx, |this: &mut SignIn, cx| {
                                if this.flow.step == step
                                    || (secret && this.flow.step == Step::RestoreKey)
                                {
                                    this.submit(cx);
                                }
                            });
                        });
                        true
                    }));
                editor.set_theme(&theme());
                if step != Step::Name {
                    editor = editor.literal();
                }
                if secret {
                    editor = editor.account_key();
                }
                if code {
                    editor = editor.email_code();
                }
                editor
            })
        };
        let email = editor("E-mail address", 254, false, false, Step::Entry, cx);
        let key = editor("Account key", 128, true, false, Step::Entry, cx);
        let code = editor("Six-digit email code", 6, false, true, Step::Code, cx);
        let name = editor("Display name", 256, false, false, Step::Name, cx);
        let saved = cx.new(|cx| {
            TextEditor::new("", cx).literal().placeholder("Saved account key").read_only()
        });
        let error_scrollbars = cx.new(|_| Scrollbars::new(false));
        let scroll = error_scrollbars.read(cx).handle();
        let error = cx.new(|cx| TextEditor::new("", cx).read_only_message(scroll));
        for field in [&email, &key, &code, &name] {
            cx.observe(field, |_, _, cx| cx.notify()).detach();
        }
        let control = |id, label, style, cx: &mut Context<Self>| {
            let weak = weak.clone();
            cx.new(|cx| {
                let control = controls::Control::new(
                    id,
                    controls::Properties {
                        kind: controls::Kind::Button(style),
                        label,
                        disabled: false,
                        theme: &theme(),
                    },
                    Rc::new(move |id, cx| {
                        let _ = weak.update(cx, |this: &mut SignIn, cx| match id {
                            1 => this.submit(cx),
                            2 => this.back(cx),
                            3 => cx.open_url(&this.flow.help_url(this.channel)),
                            6 => this.save_agreement(cx),
                            7 => cx.quit(),
                            9 => cx.open_url(agreement::PRIVACY_URL),
                            5 => {
                                let work = if this.flow.step == Step::Replace {
                                    this.flow.decline_replacement()
                                } else {
                                    this.flow.change_account()
                                };
                                this.send(work, cx);
                            }
                            _ => unreachable!("shared sign-in control identity"),
                        });
                    }),
                    cx,
                );
                if id == 2 { control.circle_icon("sign-in-back") } else { control }
            })
        };
        let primary = control(1, "Sign in", ButtonStyle::Primary, cx);
        let sign_out = control(5, "Sign Out", ButtonStyle::Default, cx);
        let back = control(2, "Back", ButtonStyle::Default, cx);
        let help = control(3, "Help", ButtonStyle::Default, cx);
        let privacy = control(9, "Privacy policy", ButtonStyle::Default, cx);
        privacy.update(cx, |button, cx| button.set_button_size(96., 24., cx));
        let channel_control = {
            let weak = weak.clone();
            cx.new(|cx| {
                controls::Control::new(
                    8,
                    controls::Properties {
                        kind: controls::Kind::Popup,
                        label: "Public",
                        disabled: true,
                        theme: &theme(),
                    },
                    Rc::new(move |id, cx| {
                        let _ = weak.update(cx, |this: &mut SignIn, cx| {
                            let channel = match id {
                                1 => crate::release_channel::ReleaseChannel::Public,
                                2 => crate::release_channel::ReleaseChannel::Testing,
                                _ => return,
                            };
                            this.change_channel(channel, cx);
                        });
                    }),
                    cx,
                )
                .accessibility_label(format!("Environment: {}", channel.label()))
            })
        };
        let agreement_save = control(6, "Save as…", ButtonStyle::Default, cx);
        let agreement_decline = control(7, "Decline & Quit", ButtonStyle::Default, cx);
        let account_id = cx.new(|cx| {
            controls::Control::new(
                4,
                controls::Properties {
                    kind: controls::Kind::Button(ButtonStyle::Default),
                    label: "Account ID",
                    disabled: false,
                    theme: &theme(),
                },
                Rc::new(|_, _| {}),
                cx,
            )
            .accessibility_label("Copy account ID".into())
        });
        Self {
            flow: Flow::new(enrolled),
            email,
            key,
            code,
            name,
            saved,
            error,
            error_scrollbars,
            progress: cx.new(|_| progress::Progress::default()),
            primary,
            sign_out,
            back,
            help,
            privacy,
            channel_control,
            channel,
            channel_busy: false,
            select_channel: None,
            agreement_save,
            agreement_decline,
            agreement: agreement::Agreement::new(cx),
            account_id,
            dispatch,
            complete,
            window: None,
            focus_step: None,
            focus_error: false,
            finished: false,
            feedback: None,
            text_ids: std::array::from_fn(|_| cx.new(|_| ())),
        }
    }
    fn text(&self, index: usize, value: impl Into<gpui::SharedString>) -> gpui::Div {
        let text = value.into();
        let id = super::text_editor::accessibility_text_ids(self.text_ids[index].entity_id()).0;
        div().relative().child(text.clone()).child(
            gpui::canvas(
                move |bounds, window, _| {
                    let mut node = super::text_accessibility_node(&text, index == 0, bounds);
                    if index == 1 || index == FEEDBACK_TEXT {
                        node.set_live(gpui::accesskit::Live::Polite);
                    }
                    window.insert_accessibility_node(id, node, false, None);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }
    pub(crate) fn show_feedback(&mut self, message: &str, cx: &mut Context<Self>) {
        let dismiss = cx.spawn(async move |view, cx| {
            cx.background_executor().timer(visual_bell::DURATION).await;
            let _ = view.update(cx, |view, cx| {
                view.feedback = None;
                cx.notify();
            });
        });
        self.feedback = Some((visual_bell::message(message), dismiss));
        cx.notify();
    }
    fn render_feedback(&self) -> Option<gpui::Div> {
        let (message, _) = self.feedback.as_ref()?;
        Some(visual_bell::render("sign-in-warning", self.text(FEEDBACK_TEXT, message.clone())))
    }
    fn send(&mut self, work: Option<Work>, cx: &mut Context<Self>) {
        let work = work
            .or_else(|| if crate::credentials::available() { self.flow.prefill() } else { None });
        if let Some(work) = work {
            let ticket = work.ticket;
            if let Err(error) = (self.dispatch)(work, cx) {
                self.flow.receive(ticket, Err(error));
            }
        }
        cx.notify();
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.channel_busy {
            return;
        }
        let work = self.flow.submit(
            self.email.read(cx).text(),
            self.key.read(cx).text(),
            self.code.read(cx).text(),
            self.name.read(cx).text(),
        );
        self.send(work, cx);
    }
    fn back(&mut self, cx: &mut Context<Self>) {
        if self.channel_busy {
            return;
        }
        if self.flow.back() {
            self.code.update(cx, |editor, cx| editor.clear(cx));
            self.key.update(cx, |editor, cx| editor.clear(cx));
            if self.flow.step == Step::Reset {
                self.submit(cx);
            }
            cx.notify();
        }
    }
    pub fn channel_selection(&mut self, select: SelectChannel, cx: &mut Context<Self>) {
        self.select_channel = Some(select);
        cx.notify();
    }

    pub fn channel_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.channel_busy = false;
        self.flow.edited();
        self.flow.error = error;
        cx.notify();
    }

    fn change_channel(
        &mut self,
        channel: crate::release_channel::ReleaseChannel,
        cx: &mut Context<Self>,
    ) {
        if self.channel_busy
            || self.flow.busy()
            || self.flow.step == Step::Complete
            || channel == self.channel
        {
            return;
        }
        let Some(select) = self.select_channel.clone() else { return };
        self.channel_busy = true;
        if let Err(error) = select(channel, cx) {
            self.channel_busy = false;
            self.flow.edited();
            self.flow.error = error;
        }
        cx.notify();
    }

    fn render_channel(&mut self, busy: bool, cx: &mut Context<Self>) {
        let channel = self.channel;
        self.channel_control.update(cx, |control, cx| {
            control.set(
                controls::Properties {
                    kind: controls::Kind::Popup,
                    label: channel.label(),
                    disabled: channel.is_personal()
                        || busy
                        || self.flow.step == Step::Complete
                        || self.select_channel.is_none(),
                    theme: &theme(),
                },
                cx,
            );
            let items = [
                crate::release_channel::ReleaseChannel::Public,
                crate::release_channel::ReleaseChannel::Testing,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, item)| crate::runtime::MenuEntry::Action {
                icon: None,
                id: index as u32 + 1,
                label: item.label().into(),
                checked: Some(item == channel),
                disabled: false,
                destructive: false,
            })
            .collect::<Vec<_>>();
            control.set_popup_items(&items, cx);
        });
    }
    pub fn receive(
        &mut self,
        ticket: Ticket,
        result: Result<Reply, String>,
        cx: &mut Context<Self>,
    ) {
        let enrolled = self.flow.enrolled;
        let work = self.flow.receive(ticket, result);
        if enrolled && !self.flow.enrolled && self.flow.step == Step::Entry {
            for field in [&self.key, &self.code] {
                field.update(cx, |field, cx| field.clear(cx));
            }
            self.focus_step = None;
        }
        if let Some(value) = self.flow.prefilled.take() {
            // Any edit invalidates the ticket, including typing and clearing a field.
            self.email.update(cx, |editor, cx| editor.acknowledge(None, &value.email, cx));
            self.key.update(cx, |editor, cx| editor.acknowledge(None, &value.account_key, cx));
        }
        if self.flow.field_error.is_some() {
            self.focus_step = None;
            let field = if self.flow.step == Step::Code { &self.code } else { &self.key };
            field.update(cx, |field, cx| field.select_all(cx));
        }
        if self.flow.enrolled {
            self.email.update(cx, |editor, cx| editor.acknowledge(None, &self.flow.email, cx));
        }
        self.saved.update(cx, |editor, cx| editor.acknowledge(None, &self.flow.saved_key, cx));
        self.send(work, cx);
        self.finish(cx);
    }
    pub fn profile(
        &mut self,
        profile: &crate::engine::reply::AccountProfile,
        cx: &mut Context<Self>,
    ) {
        self.flow.profile(profile);
        self.finish(cx);
        cx.notify();
    }
    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.flow.step != Step::Complete || self.finished {
            return;
        }
        self.finished = true;
        self.flow.saved_key.clear();
        for field in [&self.key, &self.code, &self.saved] {
            field.update(cx, |field, cx| field.clear(cx));
        }
        (self.complete)(cx);
        if let Some(window) = self.window.take() {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            });
        }
    }
    fn valid(&self, cx: &App) -> bool {
        if self.flow.key_rejected() {
            return false;
        }
        match self.flow.step {
            Step::Entry => {
                (self.flow.enrolled || validation::email(self.email.read(cx).text()))
                    && ((!self.flow.enrolled && self.key.read(cx).text().is_empty())
                        || crate::store::account_key::parse(self.key.read(cx).text().trim())
                            .is_ok())
            }
            Step::Code => validation::code(self.code.read(cx).text()),
            Step::RestoreKey => {
                crate::store::account_key::parse(self.key.read(cx).text().trim()).is_ok()
            }
            Step::Name => validation::display_name(self.name.read(cx).text()),
            Step::Agreement => self.flow.agreement_read,
            Step::SaveKey | Step::Replace => true,
            Step::Reset | Step::Recovery | Step::Profile | Step::SignOut => {
                !self.flow.error.is_empty()
            }
            Step::Complete => false,
        }
    }
}
impl Render for SignIn {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        zoom::apply(window, cx);
        let size = window_size(zoom::factor(cx));
        if window.viewport_size() != size {
            window.resize(size);
        }
        let failure = self.flow.error_screen();
        if self.flow.step == Step::Agreement && !failure {
            if self.focus_error {
                self.focus_step = None;
                self.focus_error = false;
            }
            return self.render_agreement(window, cx).into_any_element();
        }
        let initializing =
            matches!(self.flow.step, Step::Reset | Step::Recovery | Step::Profile | Step::SignOut);
        let step = self.flow.presentation();
        let busy =
            self.channel_busy || self.flow.busy() || (initializing && self.flow.error.is_empty());
        self.render_channel(busy, cx);
        for (field, read_only) in [
            (&self.email, busy || initializing || failure || self.flow.enrolled),
            (&self.key, busy || initializing || failure),
            (&self.code, busy || initializing || failure),
            (&self.name, busy || initializing || failure),
        ] {
            field.update(cx, |field, cx| field.set_read_only(read_only, &theme(), cx));
        }
        if self.focus_step != Some(self.flow.step) || self.focus_error != failure {
            self.focus_step = Some(self.flow.step);
            self.focus_error = failure;
            let field = match step {
                Step::Entry if !self.flow.enrolled && self.flow.field_error.is_none() => {
                    &self.email
                }
                Step::Entry | Step::RestoreKey => &self.key,
                Step::Code => &self.code,
                Step::Name => &self.name,
                _ => &self.saved,
            };
            if failure {
                self.primary.read(cx).focus(window);
            } else if step == Step::Replace {
                self.sign_out.read(cx).focus(window);
            } else {
                window.focus(&field.focus_handle(cx));
            }
        }
        let title = if failure {
            "An error occurred"
        } else {
            match step {
                Step::SaveKey | Step::Name => "Welcome to Playbit",
                Step::RestoreKey => "Provide account key",
                Step::Replace => "Replace Existing Data?",
                _ => "Sign in to Playbit",
            }
        };
        let label = if busy {
            "Please wait…"
        } else if failure || initializing {
            "Retry"
        } else {
            match step {
                Step::Entry
                    if !self.flow.enrolled
                        && self.key.read(cx).text().is_empty()
                        && validation::email(self.email.read(cx).text()) =>
                {
                    "Request One-time Code"
                }
                Step::Entry
                    if !self.key.read(cx).text().is_empty()
                        && crate::store::account_key::parse(self.key.read(cx).text().trim())
                            .is_err()
                        && validation::email(self.email.read(cx).text()) =>
                {
                    validation::account_key_error(self.key.read(cx).text())
                }
                Step::Replace => "Replace",
                Step::Entry | Step::Code => "Sign in",
                Step::SaveKey if self.flow.saved_key.is_empty() => "Retry",
                Step::SaveKey => "I have saved my account key",
                Step::RestoreKey => "Use Account Key",
                Step::Name => "Done",
                _ => "Retry",
            }
        };
        let disabled = busy || (!failure && !self.valid(cx));
        self.primary.update(cx, |control, cx| {
            control.set_button_size(
                if failure {
                    109.
                } else {
                    match step {
                        Step::RestoreKey => 136.,
                        Step::Replace => 109.,
                        _ => 208.,
                    }
                },
                24.,
                cx,
            );
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(ButtonStyle::Primary),
                    label,
                    disabled,
                    theme: &theme(),
                },
                cx,
            )
        });
        self.sign_out.update(cx, |control, cx| {
            control.set_button_size(109., 24., cx);
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(ButtonStyle::Default),
                    label: "Sign Out",
                    disabled: busy,
                    theme: &theme(),
                },
                cx,
            );
        });
        self.back.update(cx, |control, cx| {
            control.set_button_size(32., 32., cx);
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(ButtonStyle::Default),
                    label: "Back",
                    disabled: busy,
                    theme: &theme(),
                },
                cx,
            )
        });
        self.help.update(cx, |control, cx| {
            control.set_button_size(64., 24., cx);
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(ButtonStyle::Default),
                    label: "Help",
                    disabled: self.flow.account.is_empty(),
                    theme: &theme(),
                },
                cx,
            );
        });
        let sp = |value| zoom::pixels(window, value);
        let mut form = div().flex().flex_col().flex_shrink_0().gap(sp(12.));
        if step == Step::Replace {
            form = div().flex().flex_col().flex_shrink_0().gap(sp(24.))
                .child(self.text(3, "This computer contains existing data for a different account. Continuing with signing in will replace that information.")
                    .text_size(sp(14.)).line_height(sp(16.)).text_color(gpui::rgba(0x000000e6)));
        }
        if !failure {
            form = match step {
                Step::Entry => form.child(self.email.clone()).child(self.key.clone()),
                Step::Code => form.child(self.code.clone()),
                Step::RestoreKey => form.child(self.key.clone()),
                Step::SaveKey if !self.flow.saved_key.is_empty() => {
                    form.child(self.saved.clone()).child(self.text(
                        3,
                        "Save this key somewhere safe. You need it to unlock your private account data on a new device.",
                    ))
                }
                Step::Name => form.child(self.name.clone()).child(self.text(
                    3,
                    "Please tell us what we should call you. You can change this later in settings.",
                )),
                _ => form,
            };
        }
        self.progress.update(cx, |indicator, cx| indicator.set_running(busy && !failure, cx));
        if busy && !failure {
            form = form.child(
                div()
                    .w_full()
                    .when(step == Step::Code, |row| row.mt(sp(16.)))
                    .h(sp(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.progress.clone()),
            );
        }
        if let Some(error) = self.flow.field_error {
            form = form.child(
                div()
                    .h(sp(24.))
                    .flex()
                    .items_center()
                    .gap(sp(6.))
                    .child(
                        gpui::svg()
                            .path("sign-in-warning")
                            .size(sp(16.))
                            .text_color(gpui::rgb(0x000000)),
                    )
                    .child(self.text(1, error).text_color(gpui::rgba(0x000000e6))),
            );
        }
        if step == Step::Replace && !busy && !failure {
            form = form.child(
                div().flex().gap(sp(8.)).child(self.sign_out.clone()).child(self.primary.clone()),
            );
        } else if step == Step::RestoreKey && !busy && !failure {
            self.account_id.update(cx, |control, cx| {
                control.set_button_size(224., 40., cx);
                control.set_copy_value(&self.flow.account, cx);
            });
            form = form
                .child(
                    div()
                        .flex()
                        .gap(sp(8.))
                        .child(self.help.clone())
                        .child(div().flex_1().child(self.primary.clone())),
                )
                .child(self.text(3, "Your account key is required to access your data."))
                .child(div().ml(sp(-8.)).w(sp(224.)).child(self.account_id.clone()));
        } else if !failure
            && !busy
            && self.flow.field_error.is_none()
            && (step != Step::Code || !self.flow.error.is_empty())
        {
            form = form.child(self.primary.clone());
        }
        if step == Step::Entry && !self.flow.enrolled && !busy && !failure {
            if self.flow.error.is_empty() && self.flow.field_error.is_none() {
                form = form.child(self.text(3, "Leave the key blank to get a code by email. We use your email for sign-in and account notices."));
            }
            form = form.child(self.privacy.clone());
        }
        if step == Step::Code && !busy && !failure {
            form = form.child(self.text(2, "Enter code sent via email")).child(
                div().opacity(0.5).child(self.text(
                    3,
                    "If you haven’t received a code within 5 minutes, tap ← to try again",
                )),
            );
        }
        self.error.update(cx, |error, cx| {
            if error.text() != self.flow.error {
                self.error_scrollbars.read(cx).handle().set_offset(gpui::point(px(0.), px(0.)));
                error.acknowledge(None, &self.flow.error, cx);
            }
        });
        let error_panel = div()
            .debug_selector(|| "sign-in-error".into())
            .relative()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(gpui::rgb(0xffffff))
            .border(sp(2.))
            .border_color(gpui::rgb(0xff5100))
            .rounded(sp(2.))
            .child(self.error.clone())
            .child(Scrollbars::overlay(&self.error_scrollbars));
        let (left, top, width) = if failure {
            (87., 137., 226.)
        } else if step == Step::Replace {
            (87., 132., 226.)
        } else {
            (96., 137., 208.)
        };
        // Round inward so fractional zoom cannot consume the window margin.
        let display = window.scale_factor();
        let form_top = (((size.height - sp(360.)) / 2. + sp(top)) * display).ceil() / display;
        let form_bottom = ((size.height - sp(24.)) * display).floor() / display;
        let container = div()
            .debug_selector(|| "sign-in-form".into())
            .absolute()
            .left(sp(left))
            .top(sp(top))
            .w(sp(width))
            .max_h(form_bottom - form_top)
            .flex()
            .flex_col()
            .gap(sp(12.));
        let form = if failure {
            container.child(error_panel.h(sp(102.)).flex_shrink_0()).child(
                div()
                    .debug_selector(|| "sign-in-error-actions".into())
                    .flex()
                    .gap(sp(8.))
                    .child(self.sign_out.clone())
                    .child(self.primary.clone()),
            )
        } else {
            container.child(form).when(!self.flow.error.is_empty(), |form| form.child(error_panel))
        };
        div()
            .size_full()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                {
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    for field in [&this.email, &this.key, &this.code, &this.name, &this.saved] {
                        if field.focus_handle(cx).is_focused(window) {
                            field.update(cx, |field, cx| field.select_all(cx));
                        }
                    }
                    for control in [
                        &this.primary,
                        &this.sign_out,
                        &this.back,
                        &this.help,
                        &this.privacy,
                        &this.account_id,
                        &this.channel_control,
                    ] {
                        control.update(cx, |control, cx| {
                            control.show_keyboard_focus_if_focused(window, cx);
                        });
                    }
                    cx.stop_propagation();
                }
            }))
            .relative()
            .rounded(sp(16.))
            .overflow_hidden()
            .font_family(super::UI_FONT_FAMILY)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(sp(11.))
            .line_height(sp(16.))
            .text_color(gpui::rgba(0x00000099))
            .child(
                gpui::img(artwork()[usize::from(self.flow.field_error.is_some())].clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(512. * zoom::factor(cx).max(1.)))
                    .h(px(512. * zoom::factor(cx).max(1.))),
            )
            .child(
                div()
                    .absolute()
                    .debug_selector(|| "sign-in-canvas".into())
                    .left((size.width - sp(400.)) / 2.)
                    .top((size.height - sp(360.)) / 2.)
                    .w(sp(400.))
                    .h(sp(360.))
                    .child(if let Some(bell) = self.render_feedback() {
                        // Use the logo's space so every form action stays available.
                        bell.top(sp(46.)).bottom(gpui::Length::Auto).into_any_element()
                    } else {
                        // The 4× export includes the shadows around the optical frame.
                        gpui::img(
                            artwork()[2 + usize::from(self.flow.field_error.is_some())].clone(),
                        )
                        .absolute()
                        .left(sp(176.))
                        .top(sp(34.))
                        .w(sp(48.))
                        .h(sp(40.))
                        .into_any_element()
                    })
                    .child(
                        div()
                            .absolute()
                            .debug_selector(|| "sign-in-heading".into())
                            .top(sp(if failure { 110. } else { 91. }))
                            .when(failure, |heading| heading.left(sp(87.)).w(sp(226.)))
                            .when(!failure, |heading| heading.w_full().text_center())
                            .text_size(sp(if failure { 12. } else { 22. }))
                            .line_height(sp(20.))
                            .child(self.text(0, title)),
                    )
                    .when(
                        !failure
                            && (self.flow.step == Step::RestoreKey
                                || (!busy && self.flow.step == Step::Code)),
                        |view| {
                            view.child(
                                div()
                                    .absolute()
                                    .left(sp(32.))
                                    .top(sp(133.))
                                    .w(sp(32.))
                                    .child(self.back.clone()),
                            )
                        },
                    )
                    .child(form)
                    .when(!failure, |canvas| {
                        canvas.child(
                            div()
                                .absolute()
                                .left(sp(24.))
                                .top(sp(16.))
                                .child(self.channel_control.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(sp(32.))
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests;
