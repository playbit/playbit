//! Launch review keeps app identity and device-local directory grants together until consent.
use super::*;
use crate::runtime::MenuEntry;
use crate::{package::AppIdentity, store::ReceivedInvitation};
use gpui::{AnimationExt, AnyElement, FontWeight, RenderImage};

pub(super) struct AppReview {
    pub name: String,
    pub build: String,
    pub app: Option<AppIdentity>,
    pub icon: Option<Arc<RenderImage>>,
}

impl AppReview {
    pub(super) fn published_name(
        &self,
        channel: crate::release_channel::ReleaseChannel,
    ) -> Option<&'static str> {
        let app = self.app.as_ref()?;
        let evidence = crate::package::bundled_examples::Evidence::for_build(&self.build)?;
        evidence.identifies(channel, app).then_some("Playbit")
    }
    pub(super) fn from_package(package: &crate::package::Package) -> crate::engine::Result<Self> {
        use crate::package::codec::{ICON, MAX_ICON_BYTES};
        let icon = if package.index().entry(ICON).is_some() {
            let bytes = package.read(ICON, 0, MAX_ICON_BYTES)?;
            session_icons::decode_review(&bytes).ok()
        } else {
            None
        };
        Ok(Self {
            name: package.name().into(),
            build: package.id()?,
            app: package.publisher_certificate().map(|cert| AppIdentity {
                publisher: cert.claims.publisher,
                app_id: package.app_id().to_owned(),
            }),
            icon,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum PreparationState {
    Reading,
    StartWhenReady,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Join {
    Review,
    Transferring,
}

impl Form {
    pub(in crate::desktop) fn invitation(
        received: &ReceivedInvitation,
        send: RequestSender,
    ) -> Self {
        let session = &received.session;
        let mut form = Self::new(Target::Session(session.id.clone()), send);
        form.set_review(
            session.app.app_id.clone(),
            session.resolved_build_id.clone(),
            session.app.clone(),
            None,
        );
        form.join = Some(Join::Review);
        form
    }

    pub(in crate::desktop) fn set_review(
        &mut self,
        name: String,
        build: String,
        app: AppIdentity,
        icon: Option<Arc<RenderImage>>,
    ) {
        self.review = Some(AppReview { name, build, app: Some(app), icon });
    }

    pub(in crate::desktop) fn reviewed_invitation(&self) -> Option<&str> {
        if self.join.is_some()
            && let Target::Session(session) = &self.target
        {
            Some(session)
        } else {
            None
        }
    }

    pub(in crate::desktop) fn is_waiting_for_join(&self, session: &str) -> bool {
        self.join == Some(Join::Transferring)
            && matches!(&self.target, Target::Session(id) if id == session)
    }

    pub(in crate::desktop) fn join_completed(&mut self, cx: &mut Context<Self>) {
        if self.join == Some(Join::Transferring) {
            self.join = None;
            self.open(cx);
            cx.notify();
        }
    }

    pub(in crate::desktop) fn join_failed(&mut self, error: String, cx: &mut Context<Self>) {
        if self.join == Some(Join::Transferring) {
            self.join = Some(Join::Review);
            self.error = error;
            cx.notify();
        }
    }

    pub(in crate::desktop) fn is_launch_dialog(&self) -> bool {
        !matches!(self.target, Target::Folders { .. } | Target::Configuration(_))
    }

    pub(in crate::desktop) fn can_cancel(&self) -> bool {
        !self.picker
    }

    fn editing_disabled(&self) -> bool {
        self.picker
            || self.busy
            || self.join == Some(Join::Transferring)
            || self.opened
            || self.uncertain
            || self.preparing == Some(PreparationState::StartWhenReady)
    }

    fn can_start(&self) -> bool {
        !self.editing_disabled()
            && (!self.creates_session()
                || self.prepared
                || self.preparing == Some(PreparationState::Reading))
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_start() {
            return;
        }
        if self.preparing == Some(PreparationState::Reading) {
            self.preparing = Some(PreparationState::StartWhenReady);
            cx.notify();
            return;
        }
        if self.join != Some(Join::Review) {
            self.open(cx);
            return;
        }
        let Target::Session(session) = &self.target else { return };
        let Some(owner) = self.owner.clone() else { return };
        let session = session.clone();
        let form = cx.entity();
        self.join = Some(Join::Transferring);
        self.error.clear();
        cx.notify();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |host, cx| {
                if host.new_session.as_ref() == Some(&form)
                    && form.read(cx).is_waiting_for_join(&session)
                    && let Err(error) = host.start_reviewed_join(&session, cx)
                {
                    form.update(cx, |form, cx| form.join_failed(error.into(), cx));
                }
            });
        });
    }

    fn cancel_button(&mut self, cx: &mut Context<Self>) -> Entity<controls::Control> {
        self.button(
            (0, 6),
            if self.busy { "Close" } else { "Cancel" },
            !self.can_cancel(),
            |_, cx| cx.emit(FormEvent::Cancel),
            cx,
        )
    }

    fn start_button(&mut self, cx: &mut Context<Self>) -> Entity<controls::Control> {
        self.control(
            (0, 2),
            controls::Kind::Button(playbit_api::ui::ButtonStyle::Primary),
            "Start Session",
            !self.can_start(),
            |_, form, cx| form.submit(cx),
            cx,
        )
    }

    pub(in crate::desktop) fn focus_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let button = if self.can_start() { self.start_button(cx) } else { self.cancel_button(cx) };
        button.read(cx).focus(window);
    }

    fn directory_permission(
        &mut self,
        id: u32,
        write: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let labels = ["Read only", "Read + Write"];
        let control = self.control(
            (id, 1),
            controls::Kind::Popup,
            labels[usize::from(write)],
            self.editing_disabled(),
            move |choice, form, _| {
                if choice <= 1
                    && !form.editing_disabled()
                    && let Some(row) = form.rows.iter_mut().find(|r| r.id == id)
                {
                    row.write = choice == 1;
                }
            },
            cx,
        );
        let items: [_; 2] = std::array::from_fn(|index| MenuEntry::Action {
            id: index as u32,
            icon: None,
            label: labels[index].into(),
            checked: Some(index == usize::from(write)),
            disabled: false,
            destructive: false,
        });
        control.update(cx, |control, cx| {
            control.set_popup_items(&items, cx);
            control.set_popup_context(None, "", cx);
        });
        control
    }

    fn directories(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let disabled = self.editing_disabled();
        let add = self.button(
            (0, 1),
            "Add folder…",
            disabled || self.rows.len() == MAX_MOUNTS,
            |form, cx| form.pick(None, cx),
            cx,
        );
        add.update(cx, |control, cx| {
            control.set_sidebar_icon(
                controls::SidebarIcon::Glyph("permission-add"),
                false,
                false,
                cx,
            )
        });
        if std::mem::take(&mut self.focus_add_folder) {
            add.read(cx).focus(window);
        }
        let mut list = div()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .gap(ui(8.))
            .rounded(ui(8.))
            .bg(rgb(0xf7f7f7))
            .pt(ui(8.))
            .pl(ui(12.))
            .pr(ui(8.))
            .pb(ui(if self.rows.is_empty() { 8. } else { 16. }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(ui(24.))
                    .child(
                        self.text((0, 1), "Local directories".into(), true, false, cx)
                            .text_size(ui(14.))
                            .line_height(ui(16.))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(add),
            );
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for i in 0..self.rows.len() {
            let row = &self.rows[i];
            let id = row.id;
            let write = row.write;
            let path = home
                .as_ref()
                .and_then(|home| row.path.strip_prefix(home).ok())
                .map(|relative| Path::new("~").join(relative))
                .unwrap_or_else(|| row.path.clone());
            let label = row.label.clone();
            label.update(cx, |editor, cx| editor.set_read_only(disabled, &theme(), cx));
            let access = self.directory_permission(id, write, cx);
            let remove = self.button(
                (id, 2),
                "Remove folder",
                disabled,
                move |form, _| {
                    if !form.editing_disabled() {
                        form.remove_folder(id);
                        form.focus_add_folder = true;
                    }
                },
                cx,
            );
            remove.update(cx, |control, cx| {
                control.set_sidebar_icon(
                    controls::SidebarIcon::Glyph("permission-remove"),
                    false,
                    false,
                    cx,
                )
            });
            list = list.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(ui(32.))
                            .flex()
                            .items_center()
                            .gap(ui(8.))
                            .child(
                                gpui::svg()
                                    .path("app-browser-local")
                                    .size(ui(16.))
                                    .flex_none()
                                    .text_color(rgb(0x000000)),
                            )
                            .child(div().flex_1().h(ui(24.)).min_w_0().child(label))
                            .child(div().w(ui(112.)).flex_none().child(access))
                            .child(remove),
                    )
                    .child(
                        div()
                            .h(ui(16.))
                            .flex()
                            .items_center()
                            .gap(ui(8.))
                            .text_color(rgba(0x00000066))
                            .child(
                                gpui::svg()
                                    .path("entry-directory")
                                    .size(ui(16.))
                                    .flex_none()
                                    .text_color(rgba(0x00000066)),
                            )
                            .child(
                                self.text((id, 0), path.display().to_string(), false, false, cx)
                                    .min_w_0()
                                    .truncate(),
                            ),
                    ),
            );
        }
        list
    }

    fn app_summary(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let (name, build, publisher, icon) = if let Some(review) = &self.review {
            let publisher = review.app.as_ref().map(|app| {
                self.owner
                    .as_ref()
                    .and_then(gpui::WeakEntity::upgrade)
                    .and_then(|owner| {
                        let shell = owner.read(cx);
                        if let Some(name) = review.published_name(shell.channel) {
                            return Some(name.to_owned());
                        }
                        (app.publisher == shell.account.id)
                            .then(|| shell.account_profile.display_name.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| app.publisher.to_string())
            });
            (
                review.name.clone(),
                Some(review.build.clone()),
                publisher,
                review.icon.clone().map(gpui::ImageSource::from),
            )
        } else if let Target::Template { example, .. } = &self.target {
            (example.name.into(), None, None, Some(gpui::ImageSource::from(example.app_id)))
        } else {
            ("App".into(), None, None, None)
        };
        let icon: AnyElement = match icon {
            Some(icon) => gpui::img(icon).size(ui(48.)).into_any_element(),
            None => gpui::svg()
                .path("app-default")
                .size(ui(48.))
                .text_color(rgba(0x000000cc))
                .into_any_element(),
        };
        div()
            .w_full()
            .flex_none()
            .min_w_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(ui(4.))
            .pt(ui(8.))
            .text_center()
            .child(icon)
            .child(
                self.text((0, 7), name, false, false, cx)
                    .w_full()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM),
            )
            .when_some(build, |summary, build| {
                summary.child(
                    self.text((0, 8), format!("(build {build})"), false, false, cx)
                        .w_full()
                        .truncate(),
                )
            })
            .when_some(publisher, |summary, publisher| {
                summary.child(
                    self.text((0, 9), format!("Published by {publisher}"), false, false, cx)
                        .w_full()
                        .truncate(),
                )
            })
    }

    pub(super) fn launch_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let title = session_dialog::title(self.text((0, 0), "New Session".into(), true, false, cx));
        let scrollbars = self.scrollbars(cx);
        let handle = scrollbars.read(cx).handle();
        let mut content = div()
            .id("session-review-scroll")
            .overflow_y_scroll()
            .track_scroll(&handle)
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .gap(ui(12.))
            .px(ui(16.))
            .child(self.app_summary(cx))
            .child(self.directories(window, cx));
        if !self.error.is_empty() {
            content = content.child(
                self.text((0, 2), self.error.clone(), false, true, cx).text_color(rgb(0xa32820)),
            );
        }
        let mut footer =
            div().flex_none().flex().items_center().justify_end().gap(ui(8.)).p(ui(16.));
        let progress = if self.join == Some(Join::Transferring) {
            Some("Joining session…")
        } else if self.busy {
            self.progress
                .as_ref()
                .filter(|progress| progress.visible)
                .map(|progress| progress.phase.label())
        } else if self.preparing.is_some() {
            Some("Preparing app…")
        } else {
            None
        };
        if let Some(status) = progress {
            footer = footer
                .child(
                    gpui::svg()
                        .path("progress-spinner")
                        .size(ui(16.))
                        .text_color(rgb(0x000000))
                        .with_animation(
                            "session-launch-progress",
                            gpui::Animation::new(std::time::Duration::from_secs(1)).repeat(),
                            |svg, progress| {
                                svg.with_transformation(gpui::Transformation::rotate(
                                    gpui::radians(progress * std::f32::consts::TAU),
                                ))
                            },
                        ),
                )
                .child(
                    self.text((0, 4), status.into(), false, true, cx).min_w_0().flex_1().truncate(),
                );
        }
        footer = footer.child(self.cancel_button(cx)).child(self.start_button(cx));
        div()
            .min_h_0()
            .min_w_0()
            .w_full()
            .flex()
            .flex_col()
            .text_size(ui(11.))
            .line_height(ui(16.))
            .text_color(rgba(0x000000e6))
            .child(title)
            .child(
                div()
                    .relative()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(content)
                    .child(Scrollbars::overlay(&scrollbars)),
            )
            .child(footer)
    }
}
