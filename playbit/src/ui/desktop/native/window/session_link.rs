//! Retained guests copy invitations through local IPC without blocking input.
use super::*;
use crate::ui::desktop::{controls, zoom::length as ui};
use std::{os::unix::net::UnixStream, sync::Mutex};

#[cfg(test)]
mod tests;

const COPY_FEEDBACK_DURATION: std::time::Duration = std::time::Duration::from_secs(2);
const COPY_LABEL: &str = "Copy session link";
// Let the guest color show through while keeping dark artwork legible on black.
const BUTTON_BACKING: u32 = 0xecececbf;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Placement {
    Hidden,
    Native,
    Titlebar,
    Overlay(gpui::Pixels),
}

pub(super) struct SessionLink {
    control: Entity<controls::Control>,
    copying: Option<Copying>,
    copied: Option<gpui::Task<()>>,
}

#[derive(Default)]
struct CopyState {
    cancelled: bool,
    socket: Option<UnixStream>,
}

impl CopyState {
    fn cancel(&mut self) {
        self.cancelled = true;
        if let Some(socket) = self.socket.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

struct Copying {
    _task: gpui::Task<()>,
    state: Arc<Mutex<CopyState>>,
}

impl Drop for Copying {
    fn drop(&mut self) {
        // A running synchronous IPC read cannot be canceled by dropping its
        // future. Interrupt it before retiring the foreground completion.
        self.state.lock().expect("session link copy lock is not poisoned").cancel();
    }
}

fn invitation(
    profile: &std::path::Path,
    session: String,
    state: &Mutex<CopyState>,
) -> crate::engine::Result<String> {
    use crate::engine::{Request, ipc, reply::Reply};
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    // Do not get stuck in connect before the owner can register cancellation.
    socket.set_nonblocking(true)?;
    socket
        .connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)
        .map_err(|error| format!("Session link endpoint unavailable: {error}"))?;
    let mut stream: UnixStream = socket.into();
    {
        let mut state = state.lock().expect("session link copy lock is not poisoned");
        if state.cancelled {
            return Err("Session link copy canceled".into());
        }
        state.socket = Some(stream.try_clone()?);
    }
    stream.set_nonblocking(false)?;
    match ipc::call_stream_archive(&mut stream, &Request::Invitation { session })?.into_owned()? {
        Reply::Invitation { invitation, .. } => Ok(invitation),
        Reply::Error { error } => Err(error.into()),
        _ => Err("Session invitation unavailable".into()),
    }
}

fn properties(disabled: bool) -> controls::Properties<'static> {
    const THEME: Theme = Theme::light();
    controls::Properties {
        kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
        label: COPY_LABEL,
        disabled,
        theme: &THEME,
    }
}

impl AppWindow {
    pub(super) fn cancel_session_link(&mut self) {
        if let Some(link) = &mut self.session_link {
            link.copying = None;
            link.copied = None;
        }
    }

    pub(super) fn reset_session_link(&mut self, cx: &mut Context<Self>) {
        // Replacing the control cancels the old completion before an owner rebind.
        self.session_link = self.handle.as_ref().and_then(|handle| {
            let profile = handle.startup.as_ref()?.0.clone();
            let session = crate::identifier::encode(&handle.session?);
            let generation = handle.generation;
            let owner = cx.weak_entity();
            let control = cx.new(|cx| {
                controls::Control::new(
                    0,
                    properties(false),
                    Rc::new(move |_, cx| {
                        let _ = owner.update(cx, |this: &mut Self, cx| {
                            this.copy_session_link(
                                profile.clone(),
                                session.clone(),
                                generation,
                                cx,
                            );
                        });
                    }),
                    cx,
                )
                .toolbar_icon("session-icon")
            });
            Some(SessionLink { control, copying: None, copied: None })
        });
    }

    pub(super) fn copy_current_session_link(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.as_ref() else { return };
        let (Some(startup), Some(session)) = (handle.startup.as_ref(), handle.session) else {
            return;
        };
        self.copy_session_link(
            startup.0.clone(),
            crate::identifier::encode(&session),
            handle.generation,
            cx,
        );
    }

    fn copy_session_link(
        &mut self,
        profile: std::path::PathBuf,
        session: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.stopped
            || !self.visible.get()
            || self.handle.as_ref().is_none_or(|handle| handle.generation != generation)
        {
            return;
        }
        let Some(link) = &mut self.session_link else { return };
        if link.copying.is_some() {
            return;
        }
        let state = Arc::new(Mutex::new(CopyState::default()));
        let shared = state.clone();
        let work =
            cx.background_executor().spawn(async move { invitation(&profile, session, &shared) });
        let task = cx.spawn(async move |owner, cx| {
            let result: crate::engine::Result<String> = work.await;
            let _ = owner.update(cx, |this, cx| {
                if this.handle.as_ref().is_none_or(|handle| handle.generation != generation) {
                    return;
                }
                let Some(link) = &mut this.session_link else { return };
                link.copying = None;
                if this.closing || this.stopped || !this.visible.get() {
                    return;
                }
                link.control.update(cx, |control, cx| control.set(properties(false), cx));
                match result {
                    Ok(invitation) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(invitation));
                        this.show_session_link_copied(cx);
                    }
                    Err(error) => this.report_action(
                        super::super::super::alert::Kind::Recoverable,
                        &format!("Couldn’t copy session link: {error}"),
                        cx,
                    ),
                }
            });
        });
        link.copying = Some(Copying { _task: task, state });
        // The control still owns its activation borrow. Rendering disables it
        // after that dispatch returns; copying already bounds duplicate actions.
        cx.notify();
    }

    fn show_session_link_copied(&mut self, cx: &mut Context<Self>) {
        let Some(link) = &mut self.session_link else { return };
        // Replacing this task renews feedback from the latest successful copy.
        link.copied = Some(cx.spawn(async move |owner, cx| {
            cx.background_executor().timer(COPY_FEEDBACK_DURATION).await;
            let _ = owner.update(cx, |this, cx| {
                if let Some(link) = &mut this.session_link {
                    link.copied = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn focus_session_link(&self, window: &Window, cx: &mut Context<Self>) {
        if let Some(link) = &self.session_link {
            link.control.update(cx, |control, cx| {
                control.show_keyboard_focus_if_focused(window, cx);
            });
        }
    }

    pub(super) fn hide_session_button(&self, window: &mut Window, cx: &mut App) {
        let Some(link) = &self.session_link else { return };
        if link.control.read(cx).focused(window) {
            self.focus.focus(window);
        }
        link.control.update(cx, |control, cx| {
            control.clear_native_tooltip(window);
            control.set(properties(true), cx);
        });
    }

    pub(super) fn session_button_placement(
        &self,
        style: Option<WindowStyle>,
        chrome: gpui::MacWindowStyle,
        session_bar: bool,
        window: &Window,
    ) -> Placement {
        if self.session_link.is_none()
            || style.is_some_and(|s| s.contains(WindowStyle::HIDE_SESSION_BUTTON))
        {
            return Placement::Hidden;
        }
        let inset = crate::ui::desktop::zoom::pixels(window, 4.);
        // In fullscreen the system titlebar can retract.
        if !chrome.titled || window.is_fullscreen() {
            return if session_bar { Placement::Titlebar } else { Placement::Overlay(inset) };
        }
        let native = !(chrome.full_size_content && chrome.transparent_titlebar);
        let width = if native { px(38.) } else { crate::ui::desktop::zoom::pixels(window, 38.) };
        let controls_right = window.macos_traffic_light_right();
        if controls_right > px(0.) && window.viewport_size().width - width < controls_right {
            // Keep both controls clickable when the guest fixes a very small width.
            let top =
                if chrome.full_size_content { window.macos_titlebar_height() } else { px(0.) };
            return Placement::Overlay(top + inset);
        }
        if native {
            Placement::Native
        } else if session_bar {
            Placement::Titlebar
        } else {
            Placement::Overlay(inset)
        }
    }

    pub(super) fn native_session_button(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(gpui::MacTitlebarButton, gpui::Stateful<gpui::Div>)> {
        let link = self.session_link.as_ref()?;
        let disabled = self.closing || self.stopped || link.copying.is_some();
        let keyboard_target = link.control.update(cx, |control, cx| {
            control.set(properties(disabled), cx);
            control.titlebar_keyboard_target(window, cx)
        });
        let button = gpui::MacTitlebarButton {
            label: COPY_LABEL,
            image: if link.copied.is_some() {
                controls::CHECKMARK_ICON
            } else {
                controls::SESSION_ICON
            },
            action: <CopySessionLink as gpui::Action>::name_for_type(),
            enabled: !disabled,
            keyboard_pressed: !disabled && link.control.read(cx).has_key_press(),
            background: rgba(BUTTON_BACKING),
            focus_ring: (window.is_window_active()
                && link.control.read(cx).keyboard_focus_visible(window))
            .then(|| rgba(Theme::light().color(ThemeColor::FocusRing).0)),
        };
        Some((button, keyboard_target))
    }

    pub(super) fn session_button(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let link = self.session_link.as_ref()?;
        let icon = if link.copied.is_some() { "checkmark" } else { "session-icon" };
        link.control.update(cx, |control, cx| {
            control.set(properties(self.closing || self.stopped || link.copying.is_some()), cx);
            control.set_toolbar_icon(icon, cx);
        });
        Some(
            div()
                .absolute()
                .right(ui(4.))
                .top(ui(4.))
                .rounded(ui(12.))
                .bg(rgba(BUTTON_BACKING))
                .debug_selector(|| "copy-session-link".into())
                // A one-point highlight beneath the toolbar glyph gives it a recessed edge.
                .child(
                    gpui::svg()
                        .absolute()
                        .left(ui(9.))
                        .top(ui(5.))
                        .size(ui(16.))
                        .path(icon)
                        .text_color(rgba(0xffffff66)),
                )
                .child(link.control.clone()),
        )
    }

    pub(super) fn session_titlebar(
        &self,
        show_button: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("app-titlebar")
            .debug_selector(|| "app-titlebar".into())
            .relative()
            .flex_none()
            .h(ui(32.))
            .opacity(if window.is_window_active() { 1. } else { 0.5 })
            .window_control_area(gpui::WindowControlArea::Drag)
            .child(
                div()
                    .absolute()
                    .left(ui(80.))
                    .right(ui(80.))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .text_size(ui(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(window.window_title()),
            )
            .when(show_button, |bar| bar.children(self.session_button(cx)))
    }
}
