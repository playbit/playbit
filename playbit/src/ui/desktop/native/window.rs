//! AppKit window ownership, draft acknowledgments and display-driven canvas work.
use super::{
    MAX_LOG_BYTES, MAX_LOG_LINES,
    worker::{Handle, Output},
};
use crate::{
    runtime::ViewNode,
    ui::desktop::{
        UI_FONT_FAMILY,
        canvas::{canvas_frame_target, canvas_id, canvas_step, is_viewport_canvas},
        scene::{Input, InputAck, Scene},
        text_accessibility_node, text_editor,
    },
};
use gpui::{
    App, Bounds, Context, Entity, KeyBinding, Menu, MenuItem, OsAction, Render, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgba, size,
};
use playbit_api::{
    abi::syscall::EventKind,
    ui::{Theme, ThemeColor},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::Arc,
    time::Instant,
};
gpui::actions!(playbit_native, [Quit, CloseWindow, ToggleLog]);
#[cfg(feature = "desktop")]
#[cfg(all(target_os = "macos", feature = "desktop"))]
mod session_link;

pub(crate) struct AppWindow {
    handle: Option<Handle>,
    quit_on_last_window: bool,
    retained_owners: Rc<Cell<usize>>,
    visible: Rc<Cell<bool>>,
    scrollbars: Entity<crate::ui::desktop::scrollbar::Scrollbars>,
    scene: Scene,
    view: Option<ViewNode>,
    focus: gpui::FocusHandle,
    error: String,
    connection: String,
    #[cfg(feature = "connected-engine")]
    retry: Option<Entity<crate::ui::desktop::controls::Control>>,
    connection_id: Entity<()>,
    closing_id: Entity<()>,
    closing: bool,
    stopped: bool,
    #[cfg(all(target_os = "macos", feature = "desktop"))]
    session_link: Option<session_link::SessionLink>,
    #[cfg(feature = "process")]
    recovery_notice: Option<gpui::Task<()>>,
    draft_epoch: u64,
    frame_pending: bool,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_rendered: Option<bool>,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_sent: bool,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_presentation: u64,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_epoch: u64,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_requires_focus: bool,
    #[cfg(all(target_os = "macos", feature = "process"))]
    startup_activation: Option<gpui::Subscription>,
    #[cfg(all(feature = "native-audio", target_os = "macos"))]
    audio_visible: Option<bool>,
    #[cfg(all(feature = "native-audio", target_os = "macos"))]
    audio_activation: Option<gpui::Subscription>,
    frame_callback: bool,
    last_frame: Option<Instant>,
    held_keys: u8,
    log_open: bool,
    log_empty_id: Entity<()>,
    logs: VecDeque<(String, Entity<()>)>,
    log_bytes: usize,
}
impl AppWindow {
    fn new(handle: Option<Handle>, view: Option<ViewNode>, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let scene = Scene::new(
            input_sender(handle.clone()),
            Rc::new(move |id, cx| {
                let _ = weak.update(cx, |window: &mut Self, cx| window.click(id, cx));
            }),
        );
        let mut this = Self {
            handle,
            quit_on_last_window: true,
            retained_owners: Rc::new(Cell::new(0)),
            visible: Rc::new(Cell::new(true)),
            scene,
            scrollbars: cx.new(|_| crate::ui::desktop::scrollbar::Scrollbars::new(false)),
            view: None,
            focus: cx.focus_handle(),
            error: String::new(),
            connection: String::new(),
            #[cfg(feature = "connected-engine")]
            retry: None,
            connection_id: cx.new(|_| ()),
            closing_id: cx.new(|_| ()),
            closing: false,
            stopped: false,
            #[cfg(all(target_os = "macos", feature = "desktop"))]
            session_link: None,
            #[cfg(feature = "process")]
            recovery_notice: None,
            draft_epoch: 0,
            frame_pending: false,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_rendered: None,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_sent: false,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_presentation: 0,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_epoch: 0,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_requires_focus: true,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup_activation: None,
            #[cfg(all(feature = "native-audio", target_os = "macos"))]
            audio_visible: None,
            #[cfg(all(feature = "native-audio", target_os = "macos"))]
            audio_activation: None,
            frame_callback: false,
            last_frame: None,
            held_keys: 0,
            log_open: false,
            log_empty_id: cx.new(|_| ()),
            logs: VecDeque::new(),
            log_bytes: 0,
        };
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        this.reset_session_link(cx);
        if let Some(view) = view {
            this.apply(view, InputAck::default(), cx);
        }
        this
    }
    fn reconstruct(&mut self, handle: Handle, view: Option<ViewNode>, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "macos", feature = "process"))]
        {
            self.reset_startup(Some(0));
        }
        #[cfg(feature = "process")]
        self.recovery_notice.take();

        self.held_keys = 0;
        self.scene.cancel_pointer_presses(cx);
        self.scene.rebind(input_sender(Some(handle.clone())), cx);
        self.draft_epoch = handle.epoch();
        self.set_audio_visible(false);
        self.handle = Some(handle);
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        {
            self.audio_visible = None;
        }
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        self.reset_session_link(cx);
        self.closing = false;
        self.stopped = false;
        self.frame_pending = false;
        self.frame_callback = false;
        self.last_frame = None;
        self.view = None;
        self.error.clear();
        if let Some(view) = view {
            self.apply(view, InputAck::default(), cx);
        }
        cx.notify();
    }
    fn apply(&mut self, view: ViewNode, ack: InputAck, cx: &mut Context<Self>) -> bool {
        if self.view.as_ref().and_then(canvas_id) != canvas_id(&view) {
            self.held_keys = 0;
        }
        match self.scene.apply(&view, ack, cx) {
            Ok(()) => self.view = Some(view),
            Err(error) => {
                self.fail(error.into(), cx);
                return false;
            }
        }
        cx.notify();
        true
    }
    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        crate::ui::desktop::alert::show(crate::ui::desktop::alert::Kind::Recoverable, &error, cx);
        self.error = error;
        self.frame_pending = false;
        self.scene.invalidate_drafts(cx);
        cx.notify();
    }
    fn set_audio_visible(&mut self, active: bool) {
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        if self.audio_visible != Some(active) {
            self.audio_visible = Some(active);
            if let Some(instance) = self.handle.as_ref().and_then(Handle::audio_instance) {
                crate::runtime::audio::set_instance_active(instance, active);
            }
        }
        #[cfg(not(all(feature = "native-audio", target_os = "macos")))]
        let _ = active;
    }
    fn stop_inputs(&mut self, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        self.cancel_session_link();
        self.held_keys = 0;
        self.stopped = true;
        self.set_audio_visible(false);
        self.scene.set_active(false, self.view.as_ref(), cx);
    }
    fn click(&mut self, target: u32, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if crate::ui::desktop::popup::is_modal_active() {
            return;
        }
        if self.closing || self.stopped {
            return;
        }
        let Some(handle) = &self.handle else { return };
        let epoch = handle.epoch();
        let retry = epoch != self.draft_epoch;
        self.draft_epoch = epoch;
        if self.scene.send_drafts(self.view.as_ref(), retry, cx) {
            let accepted = handle.send(
                Input { kind: EventKind::Click, target, data: vec![], revision: None },
                Some(epoch),
            );

            let _ = accepted;
        }
    }
    fn begin_close(&mut self, cx: &mut Context<Self>) {
        if !self.closing {
            #[cfg(all(target_os = "macos", feature = "desktop"))]
            self.cancel_session_link();
            self.held_keys = 0;
            self.closing = true;
            self.set_audio_visible(false);
            self.scene.set_active(false, self.view.as_ref(), cx);
            if let Some(handle) = &self.handle {
                handle.close();
            }
            cx.notify();
        }
    }
    fn close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.stopped || self.handle.is_none() {
            self.set_audio_visible(false);
            self.visible.set(false);
            #[cfg(all(target_os = "macos", feature = "desktop"))]
            self.cancel_session_link();
            return true;
        }
        if !self.quit_on_last_window {
            if !self.release_keys() || !self.scene.release_canvas_pointer() {
                return false;
            }
            use playbit_api::abi::environment::Surface;
            let data = Surface { width: 0, height: 0, pixel_scale: 1., inputs: 0 }
                .encode()
                .expect("windowless surface is canonical");
            if !self.handle.as_ref().expect("live window owns its input handle").send(
                Input {
                    kind: EventKind::SurfaceChanged,
                    target: 0,
                    data: data.to_vec(),
                    revision: None,
                },
                None,
            ) {
                return false;
            }
            // Close only after the bounded visibility notification is accepted.
            // The worker, data host and control publisher retain the same owner.
            // The UI entity stays bound to it too: making its fields read-only
            // would acknowledge private drafts without a guest acknowledgment.
            self.set_audio_visible(false);
            self.visible.set(false);
            #[cfg(all(target_os = "macos", feature = "desktop"))]
            self.cancel_session_link();
            return true;
        }
        self.begin_close(cx);
        if cx.windows().len() == 1 && self.retained_owners.get() == 0 {
            // Release this window's entity borrow before AppKit's Quit callback
            // deactivates every window and starts the accepted-work barrier.
            cx.defer(|cx| cx.quit());
        }
        false
    }
    fn logs(&mut self, logs: Vec<String>, cx: &mut Context<Self>) {
        for line in logs {
            self.log_bytes += line.len();
            self.logs.push_back((line, cx.new(|_| ())));
            while self.logs.len() > MAX_LOG_LINES || self.log_bytes > MAX_LOG_BYTES {
                let (old, _) = self.logs.pop_front().expect("log limits require an existing entry");
                self.log_bytes -= old.len();
            }
        }
        if self.log_open {
            cx.notify();
        }
    }
    fn schedule_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing
            || self.stopped
            || !self.error.is_empty()
            || self.frame_pending
            || self.handle.as_ref().is_some_and(Handle::input_pending)
            || self.frame_callback
        {
            return;
        }
        let Some(target) = self.view.as_ref().and_then(canvas_frame_target) else { return };
        let previous = self.last_frame.unwrap_or_else(Instant::now);
        let generation = self.handle.as_ref().map(|handle| handle.generation);
        let weak = cx.weak_entity();
        self.frame_callback = true;
        window.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |this: &mut Self, _| {
                if this.handle.as_ref().map(|handle| handle.generation) != generation {
                    return;
                }
                this.frame_callback = false;
                if this.closing
                    || this.stopped
                    || !this.error.is_empty()
                    || this.frame_pending
                    || this.handle.as_ref().is_some_and(Handle::input_pending)
                    || this.view.as_ref().and_then(canvas_frame_target) != Some(target)
                {
                    return;
                }
                let now = Instant::now();
                let input = Input {
                    kind: EventKind::Frame,
                    target,
                    data: canvas_step(now.duration_since(previous)).to_le_bytes().to_vec(),
                    revision: None,
                };
                if this.handle.as_ref().is_some_and(|handle| handle.send(input, None)) {
                    this.last_frame = Some(now);
                    this.frame_pending = true;
                }
            });
        });
    }
    fn key(&mut self, key: u32, pressed: bool) -> bool {
        if self.closing || self.stopped || !(1..=5).contains(&key) {
            return false;
        }
        let Some(target) = self.view.as_ref().and_then(canvas_id) else { return false };
        let bit = 1 << (key - 1);
        if (self.held_keys & bit != 0) == pressed {
            return true;
        }
        let input = Input {
            kind: EventKind::Key,
            target,
            data: playbit_api::abi::syscall::KeyInput { key, pressed }.encode().to_vec(),
            revision: None,
        };
        if !self.handle.as_ref().is_some_and(|handle| handle.send(input, None)) {
            return false;
        }
        if pressed {
            self.held_keys |= bit;
        } else {
            self.held_keys &= !bit;
        }
        true
    }
    fn release_keys(&mut self) -> bool {
        for key in 1..=5 {
            if self.held_keys & (1 << (key - 1)) != 0 && !self.key(key, false) {
                return false;
            }
        }
        true
    }
}
fn game_key(key: &str) -> Option<u32> {
    match key {
        "left" => Some(1),
        "right" => Some(2),
        "up" => Some(3),
        "down" => Some(4),
        "space" => Some(5),
        _ => None,
    }
}
fn text(text: String, entity: &Entity<()>) -> gpui::Div {
    let id = text_editor::accessibility_text_ids(entity.entity_id()).0;
    div().relative().child(text.clone()).child(
        gpui::canvas(
            move |bounds, window, _| {
                window.insert_accessibility_node(
                    id,
                    text_accessibility_node(&text, false, bounds),
                    false,
                    None,
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full(),
    )
}
#[cfg(all(target_os = "macos", feature = "process"))]
impl AppWindow {
    fn reset_startup(&mut self, presentation: Option<u64>) {
        self.startup_epoch = self.startup_epoch.checked_add(1).expect("window observation epoch");
        if let Some(presentation) = presentation {
            self.startup_presentation = presentation;
        }
        self.startup_rendered = None;
        self.startup_sent = false;
        self.startup_activation = None;
    }
    fn observe_startup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.startup_sent || self.view.is_none() || self.stopped || self.closing {
            return;
        }
        let Some(startup) = self.handle.as_ref().and_then(|handle| handle.startup.clone()) else {
            return;
        };
        let context = startup.1;
        let epoch = self.startup_epoch;
        if self.startup_activation.is_none() {
            self.startup_activation =
                Some(cx.observe_window_activation(window, |_, _, cx| cx.notify()));
        }
        if self.startup_rendered.is_none() {
            self.startup_rendered = Some(false);
            let entity = cx.weak_entity();
            // The native draw completion establishes runtime readiness. Physical
            // presentation timestamps are a separate measurement and may be unavailable.
            window.on_next_frame_completed(move |_, cx| {
                let _ = entity.update(cx, |view, cx| {
                    if view.startup_epoch == epoch
                        && view
                            .handle
                            .as_ref()
                            .and_then(|handle| handle.startup.as_deref())
                            .is_some_and(|(_, current)| *current == context)
                    {
                        view.startup_rendered = Some(true);
                        cx.notify();
                    }
                });
            });
        }
        if self.startup_rendered == Some(true)
            && (!self.startup_requires_focus || window.is_window_active())
        {
            self.startup_sent = true;
            self.startup_activation = None;
            #[cfg(feature = "desktop")]
            crate::desktop::launch_timing::flush();
            crate::trace::mark("guest.launch.input_ready");
            self.report_startup(crate::engine::session_instance::StartupPhase::Ready, cx);
        }
    }
    fn report_startup(
        &self,
        phase: crate::engine::session_instance::StartupPhase,
        cx: &mut Context<Self>,
    ) {
        let Some(startup) = self.handle.as_ref().and_then(|handle| handle.startup.clone()) else {
            return;
        };
        let (profile, context) = (startup.0.clone(), startup.1);
        let presentation = self.startup_presentation;
        let epoch = self.startup_epoch;
        let work = cx.background_executor().spawn(async move {
            crate::engine::gui_guest::startup(&profile, context, Some(presentation), phase)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |entity, cx| {
            if let Err(error) = work.await {
                let _ = entity.update(cx, |view, cx| {
                    if view.startup_epoch == epoch
                        && view
                            .handle
                            .as_ref()
                            .and_then(|handle| handle.startup.as_deref())
                            .is_some_and(|(_, current)| *current == context)
                    {
                        view.error = format!("Could not confirm startup: {error}");
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        {
            if self.audio_activation.is_none() {
                self.audio_activation =
                    Some(cx.observe_window_activation(window, |_, _, cx| cx.notify()));
            }
            self.set_audio_visible(
                window.is_window_active() && !self.closing && !self.stopped && self.visible.get(),
            );
        }
        super::super::zoom::apply(window, cx);
        #[cfg(all(target_os = "macos", feature = "process"))]
        self.observe_startup(window, cx);

        self.schedule_frame(window, cx);
        let theme = Theme::light();
        let mut observe_surface = false;
        let right_inset = 0.;
        let viewport_canvas = !self.closing && self.view.as_ref().is_some_and(is_viewport_canvas);
        let mut content = div()
            .id("native-app-content")
            .relative()
            .rounded_b(super::super::zoom::length(16.))
            .flex_1()
            .min_h(px(0.))
            .p_4()
            .when(!viewport_canvas, |element| {
                element.overflow_scroll().track_scroll(&self.scrollbars.read(cx).handle())
            });
        if self.closing {
            content = content.child(text("Closing…".into(), &self.closing_id));
        } else {
            let scene = self.scene.render(self.view.as_ref(), window, cx);
            content = if viewport_canvas {
                // SurfaceChanged is asynchronous. Keep clipping even while the
                // last accepted canvas still has the previous window dimensions.
                content.min_w(px(0.)).child(div().size_full().overflow_hidden().child(scene))
            } else {
                content.child(scene)
            };
            if !self.stopped
                && self.error.is_empty()
                && self.view.is_some()
                && self.handle.is_some()
            {
                observe_surface = true;
            }
        }
        let mut root = div()
            .size_full()
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .font_family(UI_FONT_FAMILY)
            .text_color(rgba(theme.color(ThemeColor::Text).0))
            .bg(rgba(theme.color(ThemeColor::WindowBackground).0))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    if this.scene.has_canvas_pointer() {
                        // An accepted canvas press establishes keyboard ownership.
                        // Editor/control presses keep their own focus handlers.
                        window.focus(&this.focus);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                if this.close(cx) {
                    window.remove_window();
                }
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ToggleLog, _, cx| {
                this.log_open = !this.log_open;
                #[cfg(target_os = "macos")]
                update_log_menu(this.log_open);
                cx.notify();
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let m = event.keystroke.modifiers;
                if !event.is_held
                    && !m.control
                    && !m.alt
                    && !m.platform
                    && !m.function
                    && this.focus.is_focused(window)
                    && let Some(key) = game_key(&event.keystroke.key)
                    && this.key(key, true)
                {
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "tab"
                    && !m.control
                    && !m.alt
                    && !m.platform
                    && !m.function
                {
                    if m.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    this.scene.update_keyboard_focus_after_tab(window, cx);
                    #[cfg(all(target_os = "macos", feature = "desktop"))]
                    this.focus_session_link(window, cx);
                    #[cfg(feature = "connected-engine")]
                    if let Some(retry) = &this.retry {
                        retry.update(cx, |control, cx| {
                            control.show_keyboard_focus_if_focused(window, cx);
                        });
                    }
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, _, cx| {
                if let Some(key) = game_key(&event.keystroke.key)
                    && this.held_keys & (1 << (key - 1)) != 0
                    && this.key(key, false)
                {
                    cx.stop_propagation();
                }
            }));
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        if let Some(titlebar) = self.session_titlebar(window, cx) {
            root = root.child(titlebar);
        }
        root = root.child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(content)
                .when(observe_surface, |v| v.child(self.scene.surface_observer(right_inset)))
                .when(!viewport_canvas, |element| {
                    element
                        .child(crate::ui::desktop::scrollbar::Scrollbars::overlay(&self.scrollbars))
                }),
        );
        if !self.connection.is_empty() {
            let footer = div()
                .px_4()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .text_size(super::super::zoom::length(11.))
                .child(text(self.connection.clone(), &self.connection_id));
            #[cfg(feature = "connected-engine")]
            let footer = footer.children(self.retry.clone());
            root = root.child(footer);
        }
        if self.log_open {
            root = root.child(
                div()
                    .id("native-app-logs")
                    .h(super::super::zoom::length(140.))
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(rgba(0x00000028))
                    .bg(rgba(0xf6f6f6ff))
                    .p_3()
                    .text_size(super::super::zoom::length(11.))
                    .when(self.logs.is_empty(), |drawer| {
                        drawer.child(text("No log messages yet".into(), &self.log_empty_id))
                    })
                    .children(self.logs.iter().map(|(line, id)| text(line.clone(), id))),
            );
        }

        root
    }
}

fn input_sender(handle: Option<Handle>) -> crate::ui::desktop::scene::SendInput {
    Arc::new(move |input| {
        #[cfg(target_os = "macos")]
        if crate::ui::desktop::popup::is_modal_active() {
            return false;
        }
        handle.as_ref().is_some_and(|handle| handle.send_scene_input(input))
    })
}

pub(super) fn install_menus(cx: &mut App) {
    super::super::zoom::install(cx);
    #[cfg(feature = "desktop")]
    {}
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        active_window(cx, |this, window, cx| {
            if this.close(cx) {
                window.remove_window();
            }
        })
    });
    cx.on_action(|_: &ToggleLog, cx| {
        active_window(cx, |this, _, cx| {
            this.log_open = !this.log_open;
            #[cfg(target_os = "macos")]
            update_log_menu(this.log_open);
            cx.notify();
        })
    });
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-shift-l", ToggleLog, None),
    ]);
    cx.set_menus(vec![
        Menu { name: "App".into(), items: vec![MenuItem::action("Quit", Quit)] },
        super::super::zoom::menu(),
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", text_editor::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", text_editor::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", text_editor::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_editor::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_editor::Paste, OsAction::Paste),
                MenuItem::action("Delete", text_editor::Backspace),
                MenuItem::os_action("Select All", text_editor::SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Close", CloseWindow),
                MenuItem::action("Show Log", ToggleLog),
            ],
        },
    ]);
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs, unsafe_code, reason = "Update the AppKit-owned menu on the UI thread.")]
fn update_log_menu(log_open: bool) {
    use cocoa::foundation::NSString;
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    let (current, next) =
        if log_open { ("Show Log", "Close Log") } else { ("Close Log", "Show Log") };
    // Change only the title, preserving AppKit's Window menu and key equivalents.
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let menu: *mut Object = msg_send![app, windowsMenu];
        // Embedded windows can run without an application menu.
        if menu.is_null() {
            return;
        }
        let title = NSString::alloc(std::ptr::null_mut()).init_str(current);
        let item: *mut Object = msg_send![menu, itemWithTitle: title];
        let _: () = msg_send![title, release];
        if !item.is_null() {
            let title = NSString::alloc(std::ptr::null_mut()).init_str(next);
            let _: () = msg_send![item, setTitle: title];
            let _: () = msg_send![title, release];
        }
    }
}

pub(super) fn deactivate(windows: &Windows, cx: &mut App) {
    // Hidden owners retain an input channel too. Close it at this same AppKit
    // boundary, before a control exchange can admit another root of work.
    for owner in windows.owners.values() {
        owner.entity.update(cx, |this, cx| this.begin_close(cx));
    }
    for handle in cx.windows() {
        if let Some(handle) = handle.downcast::<AppWindow>() {
            let _ = handle.update(cx, |this, _, cx| {
                this.begin_close(cx);
            });
        }
    }
}
fn active_window(
    cx: &mut App,
    action: impl FnOnce(&mut AppWindow, &mut Window, &mut Context<AppWindow>) + 'static,
) {
    if let Some(handle) = cx.active_window() {
        // Menu/keyboard dispatch may already borrow the window. Update only after
        // that dispatch releases it, retaining the actual target window identity.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |root, window, cx| {
                if let Ok(entity) = root.downcast::<AppWindow>() {
                    entity.update(cx, |this, cx| action(this, window, cx));
                }
            });
        });
    }
}
fn open(
    title: String,
    handle: Option<Handle>,
    view: Option<ViewNode>,
    focus_window: bool,
    cx: &mut App,
) -> Result<gpui::WindowHandle<AppWindow>, String> {
    let entity = cx.new(|cx| AppWindow::new(handle, view, cx));
    show(title, entity, focus_window, cx)
}
fn show(
    title: String,
    entity: Entity<AppWindow>,
    focus_window: bool,
    cx: &mut App,
) -> Result<gpui::WindowHandle<AppWindow>, String> {
    let _timing = crate::trace::Span::new("ui.native.open_window");
    let bounds = Bounds::centered(None, size(px(640.), px(560.)), cx);
    #[cfg(all(target_os = "macos", feature = "desktop"))]
    let custom_titlebar = entity.read(cx).session_link.is_some();
    #[cfg(not(all(target_os = "macos", feature = "desktop")))]
    let custom_titlebar = false;
    cx.open_window(
        WindowOptions {
            focus: focus_window,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(title.into()),
                appears_transparent: custom_titlebar,
                traffic_light_position: custom_titlebar.then(|| gpui::point(px(9.), px(9.))),
            }),
            ..Default::default()
        },
        |window, cx| {
            window.focus(&entity.read(cx).focus);
            entity.update(cx, |this, cx| {
                #[cfg(target_os = "macos")]
                cx.observe_window_activation(window, |this, window, _| {
                    if window.is_window_active() {
                        update_log_menu(this.log_open);
                    }
                })
                .detach();
                #[cfg(all(target_os = "macos", feature = "process"))]
                {
                    this.startup_requires_focus = focus_window;
                    this.reset_startup(None);
                }
                cx.on_blur(&this.focus, window, |this, _, _| {
                    this.release_keys();
                    this.scene.release_canvas_pointer();
                })
                .detach();
            });
            let weak = entity.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |this, cx| this.close(cx)).unwrap_or(true)
            });
            entity
        },
    )
    .map_err(|error| error.to_string())
}
struct OwnedWindow {
    window: gpui::WindowHandle<AppWindow>,
    entity: Entity<AppWindow>,
    title: String,
    visible: Rc<Cell<bool>>,
    quit_on_last_window: bool,
    completed: bool,
}
#[derive(Default)]
pub(super) struct Windows {
    owners: BTreeMap<u64, OwnedWindow>,
    retained: Rc<Cell<usize>>,
    #[cfg(all(target_os = "macos", feature = "process"))]
    pending_show: Option<(u64, u64)>,
}

pub(super) fn reopen(windows: &mut Windows, cx: &mut App) {
    for owner in windows.owners.values_mut() {
        restore_window(owner, None, cx);
    }
}

fn restore_window(owner: &mut OwnedWindow, presentation: Option<u64>, cx: &mut App) {
    owner.entity.update(cx, |this, _| {
        #[cfg(all(target_os = "macos", feature = "process"))]
        {
            this.reset_startup(presentation);
            this.startup_requires_focus = true;
        }
        #[cfg(not(all(target_os = "macos", feature = "process")))]
        let _ = (this, presentation);
    });
    let result = if owner.entity.read(cx).closing || owner.entity.read(cx).stopped {
        Err("The app interface is stopping or has failed. Inspect the session log.".into())
    } else if owner.visible.get() {
        owner
            .window
            .update(cx, |_, window, cx| {
                window.activate_window();
                cx.notify();
                owner.window
            })
            .map_err(|error| error.to_string())
    } else {
        show(owner.title.clone(), owner.entity.clone(), true, cx)
    };
    match result {
        Ok(window) => {
            owner.window = window;
            owner.visible.set(true);
            owner.entity.update(cx, |this, cx| {
                this.error.clear();
                this.scene.set_active(!this.stopped, this.view.as_ref(), cx);
                this.scene.reset_surface();
                this.frame_callback = false;
                this.last_frame = None;
                cx.notify();
            });
        }
        Err(error) => owner.entity.update(cx, |this, cx| {
            this.error = error.clone();
            #[cfg(all(target_os = "macos", feature = "process"))]
            this.report_startup(crate::engine::session_instance::StartupPhase::Failed, cx);
            crate::diagnostic!("Cannot restore app window: {error}");
            cx.notify();
        }),
    }
}

#[cfg(all(target_os = "macos", feature = "process"))]
fn apply_pending_show(windows: &mut Windows, cx: &mut App) {
    let Some((generation, presentation)) = windows.pending_show else { return };
    for owner in windows.owners.values_mut() {
        let current = owner
            .entity
            .read(cx)
            .handle
            .as_ref()
            .and_then(|handle| handle.startup.as_deref())
            .map(|(_, context)| context.owner_generation);
        if current == Some(generation) {
            windows.pending_show = None;
            if presentation >= owner.entity.read(cx).startup_presentation {
                restore_window(owner, Some(presentation), cx);
            }
            return;
        }
        if current.is_some_and(|current| current > generation) {
            windows.pending_show = None;
            return;
        }
    }
    // Show may be admitted just before the first Opened output. Keep one
    // coalesced intent until that exact owner installs its native entity.
}

fn refresh_retained(windows: &BTreeMap<u64, OwnedWindow>, retained: &Cell<usize>) {
    retained.set(
        windows.values().filter(|owner| !owner.completed && !owner.quit_on_last_window).count(),
    );
}
fn bind_window(
    window: gpui::WindowHandle<AppWindow>,
    quit_on_last_window: bool,
    retained: Rc<Cell<usize>>,
    title: String,
    cx: &mut App,
) -> OwnedWindow {
    let entity = window.entity(cx).expect("new or reconstructed window is live");
    let visible = window
        .update(cx, |this, _, _| {
            this.quit_on_last_window = quit_on_last_window;
            this.retained_owners = retained;
            this.visible.set(true);
            this.visible.clone()
        })
        .expect("new or reconstructed window is live");
    OwnedWindow { window, entity, title, visible, quit_on_last_window, completed: false }
}
pub(super) fn receive(
    outputs: async_channel::Receiver<Output>,
    acknowledged: Rc<Cell<bool>>,
    windows: Rc<RefCell<Windows>>,
    focus_window: bool,
    cx: &mut App,
) {
    let retained = windows.borrow().retained.clone();
    let closed_retained = retained.clone();
    cx.on_window_closed(move |cx| {
        if cx.windows().is_empty() && closed_retained.get() == 0 {
            cx.quit();
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let mut failed = false;
        #[cfg(feature = "process")]
        let mut startup_notice = None;
        while let Ok(output) = outputs.recv().await {
            let output = output.resolve();
            #[cfg(feature = "process")]
            if let Output::Recovering { generation, active, started } = &output
                && !windows.borrow().owners.contains_key(generation)
            {
                let active = active.clone();
                let delay = crate::engine::restart::NOTICE_AFTER.saturating_sub(started.elapsed());
                startup_notice = Some(cx.spawn(async move |cx| {
                    gpui::Timer::after(delay).await;
                    if active.load(std::sync::atomic::Ordering::Acquire) {
                        let _ = cx.update(|cx| {
                            crate::ui::desktop::alert::show(
                                crate::ui::desktop::alert::Kind::RecoverySlow,
                                "App owner startup has not completed after 10 seconds",
                                cx,
                            )
                        });
                    }
                }));
            }
            #[cfg(feature = "process")]
            if matches!(output, Output::Opened { .. } | Output::Failed(_) | Output::Closed(_))
                || matches!(&output, Output::ContextUnavailable(generation)
                    if windows.borrow().owners.contains_key(generation))
            {
                startup_notice.take();
            }
            failed |= matches!(output, Output::Failed(_));
            let _ = cx.update(|cx| {
                let mut windows = windows.borrow_mut();
                #[cfg(feature = "process")]
                if let Output::ShowRequested { owner_generation, presentation } = &output {
                    #[cfg(target_os = "macos")]
                    {
                        windows.pending_show = Some((*owner_generation, *presentation));
                        apply_pending_show(&mut windows, cx);
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = (owner_generation, presentation);
                        reopen(&mut windows, cx);
                    }
                    return;
                }
                deliver(
                    output,
                    &mut windows.owners,
                    &retained,
                    acknowledged.clone(),
                    focus_window,
                    cx,
                );
                #[cfg(all(target_os = "macos", feature = "process"))]
                apply_pending_show(&mut windows, cx);
            });
        }
        if failed {
            return;
        }
        let _ = cx.update(|cx| {
            let windows = windows.borrow();
            if !windows.owners.is_empty() && windows.owners.values().all(|owner| owner.completed) {
                if cx.windows().is_empty() {
                    cx.quit();
                }
                return;
            }
            if windows.owners.is_empty() && cx.windows().is_empty() {
                cx.quit();
                return;
            }
            if cx.windows().is_empty() {
                show_failure(
                    "The app worker stopped without confirming its accepted work.".into(),
                    acknowledged,
                    focus_window,
                    cx,
                );
                return;
            }
            show_failure(
                "The native app worker stopped before confirming its accepted work.".into(),
                acknowledged,
                focus_window,
                cx,
            );
        });
    })
    .detach();
}
fn deliver(
    output: Output,
    windows: &mut BTreeMap<u64, OwnedWindow>,
    retained: &Rc<Cell<usize>>,
    acknowledged: Rc<Cell<bool>>,
    focus_window: bool,
    cx: &mut App,
) {
    let output = output.resolve();
    let _delivery = crate::trace::Span::new("ui.apply_response");
    match output {
        Output::UiZoom { scale, save } => super::super::zoom::configure(scale, save, cx),
        Output::Readiness(_) => unreachable!("readiness output was resolved above"),
        #[cfg(feature = "connected-engine")]
        Output::SignIn { enrolled, profile, actions, replies } => {
            // Keep the process alive across programmatic sign-in close and the
            // first session/selection window. Opened installs its actual owners.
            retained.set(retained.get() + 1);
            let complete = actions.clone();
            match crate::ui::desktop::sign_in::open(
                crate::engine::profile::current().1,
                enrolled,
                Rc::new(move |work, _| {
                    actions
                        .try_send(super::worker::setup::SignInAction::Work(work))
                        .map_err(|_| "Account worker unavailable".into())
                }),
                Rc::new(move |_| {
                    let _ = complete.try_send(super::worker::setup::SignInAction::Complete);
                }),
                cx,
            ) {
                Ok((_, view)) => {
                    view.update(cx, |view, cx| view.profile(&profile, cx));
                    cx.spawn(async move |cx| {
                        while let Ok((ticket, result)) = replies.recv().await {
                            if view.update(cx, |view, cx| view.receive(ticket, result, cx)).is_err()
                            {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                Err(error) => {
                    replies.close();
                    show_failure(error.to_string().into(), acknowledged, focus_window, cx);
                }
            }
        }
        #[cfg(feature = "process")]
        Output::QuitRequested => cx.quit(),
        #[cfg(feature = "process")]
        Output::ShowRequested { .. } => {
            unreachable!("Show is routed with the retained owner registry")
        }
        Output::Opened {
            replaces, handle, errors, title, view, logs, quit_on_last_window, ..
        } => {
            let generation = handle.generation;
            let opened = match replaces.and_then(|generation| windows.remove(&generation)) {
                Some(owner) => {
                    owner.entity.update(cx, |this, cx| this.reconstruct(handle.clone(), view, cx));
                    if owner.visible.get() {
                        owner
                            .window
                            .update(cx, |_, native_window, _| {
                                native_window.set_window_title(&title);
                                owner.window
                            })
                            .map_err(|error| error.to_string())
                    } else {
                        show(title.clone(), owner.entity, focus_window, cx)
                    }
                }
                None => open(title.clone(), Some(handle.clone()), view, focus_window, cx),
            };
            match opened {
                Ok(window) => {
                    windows.insert(
                        generation,
                        bind_window(window, quit_on_last_window, retained.clone(), title, cx),
                    );
                    // Install the app before closing setup so the last-window
                    // handler cannot quit between successful enrollment and launch.
                    if generation != 0
                        && let Some(setup) = windows.remove(&0)
                    {
                        let _ = setup.window.update(cx, |_, window, _| window.remove_window());
                    }
                    refresh_retained(windows, retained);
                    let entity = windows[&generation].entity.clone();
                    entity.update(cx, |this, cx| this.logs(logs, cx));
                    cx.spawn(async move |cx| {
                        while let Ok(error) = errors.recv().await {
                            let _ = cx.update(|cx| {
                                entity.update(cx, |this, cx| {
                                    if !this.stopped
                                        && this
                                            .handle
                                            .as_ref()
                                            .is_some_and(|handle| handle.generation == generation)
                                    {
                                        this.fail(error.into(), cx);
                                    }
                                })
                            });
                        }
                    })
                    .detach();
                }
                Err(error) => {
                    refresh_retained(windows, retained);
                    handle.close();
                    crate::diagnostic!("Cannot open native window: {error}");
                }
            }
        }
        Output::Frame {
            generation,
            result,
            logs,
            ack,
            frame_input,
            quit_on_last_window,
            pending: _pending,
            ..
        } => {
            if let Some(owner) = windows.get_mut(&generation)
                && !owner.completed
            {
                if let Some(intent) = quit_on_last_window {
                    owner.quit_on_last_window = intent;
                }
                owner.entity.update(cx, |this, cx| {
                    if let Some(intent) = quit_on_last_window {
                        this.quit_on_last_window = intent;
                    }
                    if frame_input {
                        this.frame_pending = false;
                    }
                    match result {
                        Ok(view) => {
                            if _pending.is_some() {
                                this.error.clear();
                            }

                            if let Some(view) = view
                                && !this.apply(view, ack, cx)
                            {
                                return;
                            }

                            this.logs(logs, cx);
                            // View adoption and visible logs notify themselves. Empty
                            // async completions do not change a static scene. Preserve
                            // input feedback and display-driven canvas progress.
                            if _pending.is_some()
                                || frame_input
                                || this.view.as_ref().and_then(canvas_frame_target).is_some()
                            {
                                cx.notify();
                            }
                        }
                        Err(error) => {
                            this.fail(error, cx);
                        }
                    }
                });
                if quit_on_last_window.is_some() {
                    refresh_retained(windows, retained);
                }
            }
        }
        #[cfg(feature = "connected-engine")]
        Output::Connection(status) => {
            for owner in windows.values() {
                owner.entity.update(cx, |this, cx| {
                    this.connection = status.message.clone();
                    this.retry = status.retry.as_ref().map(|retry| {
                        use crate::ui::desktop::controls::{Control, Kind, Properties};
                        let retry = retry.clone();
                        cx.new(|cx| {
                            Control::new(
                                0,
                                Properties {
                                    kind: Kind::Button(playbit_api::ui::ButtonStyle::Default),
                                    label: "Retry sync",
                                    disabled: false,
                                    theme: &Theme::light(),
                                },
                                Rc::new(move |_, _| {
                                    retry.request();
                                }),
                                cx,
                            )
                        })
                    });
                    cx.notify();
                });
            }
        }
        Output::Closed(generation) => {
            if let Some(owner) = windows.remove(&generation) {
                refresh_retained(windows, retained);
                let _ = owner.window.update(cx, |_, window, _| window.remove_window());
            }
        }
        Output::Failed(error) => {
            show_failure(error, acknowledged, focus_window, cx);
        }
        #[cfg(feature = "process")]
        Output::ContextUnavailable(generation) => {
            if let Some(owner) = windows.get_mut(&generation)
                && !owner.completed
            {
                owner.completed = true;
                let close = owner.entity.update(cx, |this, cx| {
                    let close = this.closing || !this.visible.get();
                    this.recovery_notice.take();
                    this.stop_inputs(cx);
                    if let Some(handle) = &this.handle {
                        handle.close();
                    }
                    this.error = crate::engine::service_events::StopReason::ContextUnavailable
                        .message()
                        .into();
                    cx.notify();
                    close
                });
                let closed = if close { windows.remove(&generation) } else { None };
                refresh_retained(windows, retained);
                if close {
                    if let Some(owner) = closed {
                        let _ = owner.window.update(cx, |_, window, _| window.remove_window());
                    }
                } else {
                    crate::ui::desktop::alert::show(
                        crate::ui::desktop::alert::Kind::AppContextUnavailable,
                        crate::engine::service_events::StopReason::ContextUnavailable.message(),
                        cx,
                    );
                }
            }
        }
        #[cfg(feature = "process")]
        Output::Recovering { generation, active, started } => {
            if let Some(owner) = windows.get_mut(&generation)
                && !owner.completed
            {
                owner.entity.update(cx, |this, cx| {
                    this.stop_inputs(cx);
                    if let Some(handle) = &this.handle {
                        handle.close();
                    }
                    let delay =
                        crate::engine::restart::NOTICE_AFTER.saturating_sub(started.elapsed());
                    this.recovery_notice = Some(cx.spawn(async move |_, cx| {
                        gpui::Timer::after(delay).await;
                        if active.load(std::sync::atomic::Ordering::Acquire) {
                            let _ = cx.update(|cx| {
                                crate::ui::desktop::alert::show(
                                    crate::ui::desktop::alert::Kind::RecoverySlow,
                                    "App reconstruction has not completed after 10 seconds",
                                    cx,
                                )
                            });
                        }
                    }));
                    cx.notify();
                });
            }
        }
    }
}

pub(super) fn show_failure(
    error: crate::ui::desktop::alert::Failure,
    acknowledged: Rc<Cell<bool>>,
    focus_window: bool,
    cx: &mut App,
) {
    let _ = focus_window;
    for window in cx.windows() {
        if let Some(window) = window.downcast::<AppWindow>() {
            let _ = window.update(cx, |this, _, cx| {
                this.stop_inputs(cx);
                this.error = error.details.clone();
                cx.notify();
            });
        }
    }
    crate::ui::desktop::alert::fatal(&error, acknowledged, cx);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert the window routing contract.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn viewport_canvas_clips_the_previous_scene_during_resize(cx: &mut gpui::TestAppContext) {
        use crate::runtime::{CanvasItem, MenuEntry};
        use playbit_api::ui::{CanvasFrames, CanvasRect, Color};
        cx.set_auto_hide_scrollbars(false);
        let canvas = |viewport| ViewNode::Canvas {
            id: 1,
            width: 600,
            height: 500,
            frames: CanvasFrames::Static,
            viewport,
            items: vec![CanvasItem::Rect(CanvasRect {
                x: 0,
                y: 0,
                width: 600,
                height: 500,
                color: Color(0x277582ff),
            })],
        };
        let with_menu = |child| ViewNode::Theme {
            theme: Theme::light(),
            child: Box::new(ViewNode::ContextMenu {
                items: vec![MenuEntry::Action {
                    id: 2,
                    label: "Pass".into(),
                    icon: None,
                    checked: None,
                    disabled: false,
                    destructive: false,
                }],
                child: Box::new(child),
            }),
        };
        for viewport in [false, true] {
            let window =
                cx.add_window(|_, cx| AppWindow::new(None, Some(with_menu(canvas(viewport))), cx));
            let visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.simulate_resize(gpui::size(px(640.), px(540.)));
            cx.run_until_parked();
            let handle =
                window.update(cx, |owner, _, cx| owner.scrollbars.read(cx).handle()).unwrap();
            assert_eq!(handle.max_offset(), gpui::size(px(0.), px(0.)));
            // Resize repeatedly without delivering a replacement scene. A
            // document measures overflow, while the viewport canvas clips it.
            for size in [gpui::size(px(420.), px(320.)), gpui::size(px(370.), px(260.))] {
                visual.simulate_resize(size);
                cx.run_until_parked();
                assert_eq!(handle.max_offset().height == px(0.), viewport);
                if viewport {
                    assert_eq!(handle.max_offset().width, px(0.));
                }
            }
        }
    }

    #[cfg(feature = "process")]
    #[gpui::test]
    fn retired_context_closes_and_ignores_late_output_without_failing_other_owners(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, output) = async_channel::bounded(16);
        let windows = Rc::new(RefCell::new(Windows::default()));
        let acknowledged = Rc::new(Cell::new(false));
        cx.update(|cx| receive(output, acknowledged.clone(), windows.clone(), true, cx));
        let (first, first_inputs) = Handle::fixture(10);
        let (other, other_inputs) = Handle::fixture(20);
        let (errors, first_errors) = async_channel::bounded(1);
        for (handle, errors) in
            [(first.clone(), first_errors), (other.clone(), async_channel::bounded(1).1)]
        {
            send.try_send(Output::Opened {
                replaces: None,
                handle,
                errors,
                title: "Fixture".into(),
                view: None,
                logs: vec![],
                quit_on_last_window: false,
            })
            .unwrap();
        }
        cx.run_until_parked();
        assert_eq!(windows.borrow().retained.get(), 2);
        send.try_send(Output::ContextUnavailable(10)).unwrap();
        cx.run_until_parked();
        assert_eq!(
            windows.borrow().retained.get(),
            1,
            "retired owner no longer keeps the process alive"
        );
        let first_window = windows.borrow().owners[&10].window;
        first_window
            .update(cx, |this, _, _| {
                assert!(this.stopped);
                assert!(this.recovery_notice.is_none());
            })
            .unwrap();
        assert!(first_inputs.is_closed());
        let other_window = windows.borrow().owners[&20].window;
        other_window.update(cx, |this, _, _| assert!(!this.stopped)).unwrap();
        assert!(
            other.send(
                Input { kind: EventKind::Click, target: 1, data: vec![], revision: None },
                None
            )
        );
        assert!(other_inputs.try_recv().is_ok(), "unrelated owner still accepts input");

        errors.try_send("late input failure").unwrap();
        send.try_send(Output::ContextUnavailable(10)).unwrap();
        send.try_send(Output::ContextUnavailable(999)).unwrap();
        send.try_send(Output::Recovering {
            generation: 10,
            active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            started: Instant::now(),
        })
        .unwrap();
        send.try_send(Output::Frame {
            generation: 10,
            pending: None,
            result: Ok(Some(ViewNode::TextField {
                id: 1,
                value: "Late private data".into(),
                placeholder: String::new(),
                multiline: false,
                disabled: false,
                read_only: false,
            })),
            logs: vec![],
            ack: InputAck::default(),
            frame_input: false,
            quit_on_last_window: Some(false),
        })
        .unwrap();
        cx.run_until_parked();
        first_window
            .update(cx, |this, _, _| {
                assert!(this.view.is_none(), "retired generation cannot adopt late data");
                assert!(
                    this.recovery_notice.is_none(),
                    "retired generation cannot restart its timer"
                );
                assert!(this.error.contains("data context is no longer available"));
            })
            .unwrap();
        assert_eq!(windows.borrow().retained.get(), 1);
        other_window
            .update(cx, |this, window, cx| {
                assert!(this.close(cx), "background owner may hide while retaining work");
                window.remove_window();
            })
            .unwrap();
        send.try_send(Output::ContextUnavailable(20)).unwrap();
        drop(send);
        cx.run_until_parked();
        assert!(
            !windows.borrow().owners.contains_key(&20),
            "closed owner retires without a new modal"
        );
        assert_eq!(windows.borrow().retained.get(), 0);
        let notices = cx.update(crate::ui::desktop::alert::take_presented);
        assert_eq!(
            notices.len(),
            1,
            "late output and normal channel completion do not add failures"
        );
        assert_eq!(notices[0].0, crate::ui::desktop::alert::Kind::AppContextUnavailable);
        assert!(!acknowledged.get(), "nonfatal retirement never requests Quit");
        first_window
            .update(cx, |this, window, cx| {
                assert!(this.close(cx), "stopped window must close without another worker reply");
                window.remove_window();
            })
            .unwrap();
    }

    #[cfg(feature = "connected-engine")]
    #[gpui::test]
    fn sign_in_retains_the_windowless_handoff_until_setup_opens(cx: &mut gpui::TestAppContext) {
        // Complete real sign-in replies while withholding the next owner output.
        // The windowless gap retains setup, then actual window intent takes over.
        use super::super::worker::setup::SignInAction;
        use crate::engine::reply::{AccountProfile, RecoveryStatus};
        use crate::ui::desktop::sign_in::flow::{Command, Reply};
        let profile = AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: Some("Robin".into()),
            unlocked: true,
            initialized: true,
        };
        let (actions, receive) = async_channel::bounded(1);
        let (replies, results) = async_channel::bounded(1);
        let retained = Rc::new(Cell::new(0));
        let mut owners = BTreeMap::new();
        let acknowledged = Rc::new(Cell::new(false));
        cx.update(|cx| {
            deliver(
                Output::SignIn {
                    enrolled: true,
                    profile: profile.clone(),
                    actions,
                    replies: results,
                },
                &mut owners,
                &retained,
                acknowledged.clone(),
                true,
                cx,
            );
        });
        cx.run_until_parked();
        let SignInAction::Work(work) = receive.try_recv().unwrap() else { panic!("recovery work") };
        assert!(matches!(work.command, Command::Recovery));
        replies
            .try_send((
                work.ticket,
                Ok(Reply::Recovery(RecoveryStatus::Ready {
                    revision: 1,
                    pending_save: false,
                    restored_sessions: None,
                    restored_volumes: None,
                })),
            ))
            .unwrap();
        cx.run_until_parked();
        let SignInAction::Work(work) = receive.try_recv().unwrap() else { panic!("profile work") };
        assert!(matches!(work.command, Command::Profile));
        replies
            .try_send((
                work.ticket,
                Ok(Reply::Profile {
                    account: crate::store::Id::from_bytes([1; 16]).to_string(),
                    email: "robin@example.test".into(),
                    profile,
                }),
            ))
            .unwrap();
        cx.run_until_parked();
        assert!(matches!(receive.try_recv().unwrap(), SignInAction::Complete));
        assert!(cx.update(|cx| cx.windows().is_empty()), "sign-in closes before setup is ready");
        assert_eq!(retained.get(), 1, "windowless sign-in handoff must prevent last-window quit");
        cx.update(|cx| {
            deliver(
                Output::Connection(super::super::network::Status {
                    message: String::new(),
                    retry: None,
                }),
                &mut owners,
                &retained,
                acknowledged.clone(),
                true,
                cx,
            );
        });
        assert_eq!(retained.get(), 1, "status does not replace setup ownership");
        let (handle, _inputs) = Handle::fixture(0);
        let (_errors, error_receive) = async_channel::bounded(1);
        cx.update(|cx| {
            deliver(
                Output::Opened {
                    replaces: None,
                    handle,
                    errors: error_receive,
                    title: "Set up app".into(),
                    view: None,
                    logs: vec![],
                    quit_on_last_window: true,
                },
                &mut owners,
                &retained,
                acknowledged.clone(),
                true,
                cx,
            );
        });
        assert_eq!(retained.get(), 0, "the real setup window replaces temporary retention");
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        for intent in [false, true] {
            cx.update(|cx| {
                deliver(
                    Output::Frame {
                        pending: None,
                        generation: 0,
                        result: Ok(None),
                        logs: vec![],
                        ack: InputAck::default(),
                        frame_input: false,
                        quit_on_last_window: Some(intent),
                    },
                    &mut owners,
                    &retained,
                    acknowledged.clone(),
                    true,
                    cx,
                );
            });
            assert_eq!(
                retained.get(),
                usize::from(!intent),
                "current owner intent controls retention"
            );
        }
        cx.update(|cx| {
            deliver(Output::Closed(0), &mut owners, &retained, acknowledged, true, cx);
        });
        assert!(owners.is_empty());
        assert_eq!(retained.get(), 0);
        assert!(cx.update(|cx| cx.windows().is_empty()));
    }

    #[cfg(all(target_os = "macos", feature = "process"))]
    #[gpui::test]
    fn startup_waits_for_current_frame_and_focus_after_reconstruction(
        cx: &mut gpui::TestAppContext,
    ) {
        // Advance CPU preparation and native frames separately. A replaced
        // owner's callback cannot complete the new view or bypass its focus gate.
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let startup = |owner_generation| {
            Arc::new((
                "/unused-test-profile".into(),
                crate::engine::session_instance::Context {
                    instance: crate::store::Id::from_bytes([1; 16]),
                    handle: crate::store::Id::from_bytes([2; 16]),
                    incarnation: crate::store::Id::from_bytes([3; 16]),
                    owner_generation,
                },
            ))
        };
        let (mut first, _first_inputs) = Handle::fixture(7);
        first.startup = Some(startup(4));
        let window = cx.update(|cx| open("Readiness".into(), Some(first), None, true, cx).unwrap());
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert!(this.startup_rendered.is_none(), "no valid guest view yet");
                assert!(!this.startup_sent);
                assert!(this.apply(ViewNode::Column(vec![]), InputAck::default(), cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.startup_rendered, Some(false));
                assert!(!this.startup_sent, "CPU preparation is not a native frame");
            })
            .unwrap();
        let (mut next, _next_inputs) = Handle::fixture(8);
        next.startup = Some(startup(5));
        window.update(cx, |this, _, cx| this.reconstruct(next, None, cx)).unwrap();
        cx.run_until_parked();
        cx.simulate_window_frame(window.into());
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert!(this.startup_rendered.is_none(), "the old frame belongs to another owner");
                assert!(!this.startup_sent);
                assert!(this.apply(ViewNode::Column(vec![]), InputAck::default(), cx));
            })
            .unwrap();
        let other = cx.update(|cx| open("Other".into(), None, None, true, cx).unwrap());
        other.update(cx, |_, window, _| window.activate_window()).unwrap();
        cx.run_until_parked();
        cx.simulate_window_frame(window.into());
        cx.run_until_parked();
        window
            .update(cx, |this, window, _| {
                assert_eq!(this.startup_rendered, Some(true));
                assert!(!window.is_window_active());
                assert!(!this.startup_sent, "a foreground launch still requires activation");
                window.activate_window();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(this.startup_sent);
                assert!(this.startup_activation.is_none());
            })
            .unwrap();
        cx.simulate_window_frame(window.into());
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(this.startup_sent, "later frames do not rearm readiness");
                assert_eq!(this.startup_rendered, Some(true));
                assert!(this.startup_activation.is_none());
            })
            .unwrap();
    }

    #[cfg(all(target_os = "macos", feature = "process"))]
    #[gpui::test]
    fn show_waits_for_its_owner_and_rearms_only_the_current_presentation(
        cx: &mut gpui::TestAppContext,
    ) {
        // Deliver Show before Opened, then restore a closed retained window.
        // Older Show messages must not reset the new presentation observation.
        let (mut handle, _inputs) = Handle::fixture(7);
        handle.startup = Some(Arc::new((
            "/unused-test-profile".into(),
            crate::engine::session_instance::Context {
                instance: crate::store::Id::from_bytes([1; 16]),
                handle: crate::store::Id::from_bytes([2; 16]),
                incarnation: crate::store::Id::from_bytes([3; 16]),
                owner_generation: 4,
            },
        )));
        let mut windows = Windows { pending_show: Some((4, 1)), ..Default::default() };
        cx.update(|cx| {
            apply_pending_show(&mut windows, cx);
            assert_eq!(windows.pending_show, Some((4, 1)));
            let window = open("Retained".into(), Some(handle), None, true, cx).unwrap();
            windows.owners.insert(
                7,
                bind_window(window, false, windows.retained.clone(), "Retained".into(), cx),
            );
            apply_pending_show(&mut windows, cx);
            assert!(windows.pending_show.is_none());
            windows.owners[&7]
                .window
                .update(cx, |this, window, cx| {
                    assert_eq!(this.startup_presentation, 1);
                    this.startup_sent = true;
                    this.startup_rendered = Some(true);
                    assert!(this.close(cx));
                    window.remove_window();
                })
                .unwrap();
            windows.pending_show = Some((4, 2));
            apply_pending_show(&mut windows, cx);
            let entity = windows.owners[&7].entity.clone();
            let epoch = entity.read(cx).startup_epoch;
            assert_eq!(entity.read(cx).startup_presentation, 2);
            assert!(!entity.read(cx).startup_sent);
            assert!(entity.read(cx).startup_rendered.is_none());
            windows.pending_show = Some((4, 1));
            apply_pending_show(&mut windows, cx);
            assert_eq!(entity.read(cx).startup_epoch, epoch);
            windows.pending_show = Some((3, 9));
            apply_pending_show(&mut windows, cx);
            assert!(windows.pending_show.is_none());
            assert_eq!(entity.read(cx).startup_epoch, epoch);
        });
    }

    #[gpui::test]
    fn canvas_pointer_focus_loss_and_window_close_release_only_accepted_gestures(
        cx: &mut gpui::TestAppContext,
    ) {
        let (handle, inputs) = Handle::fixture(1);
        let view = ViewNode::Canvas {
            id: 1,
            width: 32,
            height: 32,
            frames: playbit_api::ui::CanvasFrames::Continuous,
            viewport: false,
            items: vec![],
        };
        let window = cx.update(|cx| {
            let window = open("Pointer".into(), Some(handle), Some(view), true, cx).unwrap();
            bind_window(window, false, Rc::new(Cell::new(1)), "Pointer".into(), cx);
            window
        });
        window.update(cx, |_, window, _| window.activate_window()).unwrap();
        cx.run_until_parked();
        while inputs.try_recv().is_ok() {}
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(
            gpui::point(px(20.), px(20.)),
            gpui::MouseButton::Left,
            Default::default(),
        );
        let press = inputs.try_recv().unwrap().input;
        assert_eq!(press.kind, EventKind::Pointer);
        assert_eq!(playbit_api::abi::syscall::PointerInput::decode(&press.data).unwrap().phase, 1);
        window.update(cx, |this, window, _| assert!(this.focus.is_focused(window))).unwrap();
        window.update(cx, |_, window, _| window.blur()).unwrap();
        cx.run_until_parked();
        let release = inputs.try_recv().unwrap().input;
        assert_eq!(release.kind, EventKind::Pointer);
        assert_eq!(
            playbit_api::abi::syscall::PointerInput::decode(&release.data).unwrap().phase,
            3
        );
        assert!(inputs.is_empty());
        // The later distinct press is retained until a bounded cancellation
        // and zero-area surface are both accepted before window-only Close.
        visual.simulate_mouse_down(
            gpui::point(px(20.), px(20.)),
            gpui::MouseButton::Left,
            Default::default(),
        );
        assert_eq!(inputs.try_recv().unwrap().input.kind, EventKind::Pointer);
        window.update(cx, |this, _, cx| assert!(this.close(cx))).unwrap();
        assert_eq!(inputs.try_recv().unwrap().input.kind, EventKind::Pointer);
        assert_eq!(inputs.try_recv().unwrap().input.kind, EventKind::SurfaceChanged);
        assert!(inputs.is_empty());
        assert!(!inputs.is_closed());
    }
    #[gpui::test]
    fn canvas_keys_are_local_release_on_blur_and_do_not_cross_owners(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let view = || ViewNode::Canvas {
            id: 1,
            width: 32,
            height: 32,
            frames: playbit_api::ui::CanvasFrames::Continuous,
            viewport: false,
            items: vec![],
        };
        let (first, first_input) = Handle::fixture(1);
        let (second, second_input) = Handle::fixture(2);
        let first =
            cx.update(|cx| open("First kart".into(), Some(first), Some(view()), true, cx).unwrap());
        let second = cx
            .update(|cx| open("Second kart".into(), Some(second), Some(view()), true, cx).unwrap());
        cx.run_until_parked();
        while first_input.try_recv().is_ok() {}
        while second_input.try_recv().is_ok() {}
        first
            .update(cx, |this, window, _| {
                window.activate_window();
                window.focus(&this.focus);
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(first.into(), "up");
        cx.run_until_parked();
        let key = first_input.try_recv().unwrap().input;
        assert_eq!(key.kind, EventKind::Key);
        assert_eq!(key.target, 1);
        assert_eq!(
            playbit_api::abi::syscall::KeyInput::decode(&key.data).unwrap(),
            playbit_api::abi::syscall::KeyInput { key: 3, pressed: true }
        );
        assert!(second_input.is_empty(), "one window never sends input to its sibling");
        first.update(cx, |_, window, _| window.blur()).unwrap();
        cx.run_until_parked();
        let release = first_input.try_recv().unwrap().input;
        assert_eq!(
            playbit_api::abi::syscall::KeyInput::decode(&release.data).unwrap(),
            playbit_api::abi::syscall::KeyInput { key: 3, pressed: false }
        );
        assert!(first_input.is_empty());
        assert!(second_input.is_empty());
        second.update(cx, |this, window, _| window.focus(&this.focus)).unwrap();
        cx.simulate_keystrokes(second.into(), "right");
        cx.dispatch_key_up(second.into(), gpui::Keystroke::parse("right").unwrap());
        cx.run_until_parked();
        for pressed in [true, false] {
            let key = second_input.try_recv().unwrap().input;
            assert_eq!(
                playbit_api::abi::syscall::KeyInput::decode(&key.data).unwrap(),
                playbit_api::abi::syscall::KeyInput { key: 2, pressed }
            );
        }
        assert!(first_input.is_empty());
        assert!(second_input.is_empty());
        second
            .update(cx, |this, _, cx| {
                assert!(this.key(3, true));
                assert!(this.key(3, true), "held repeats add no root");
                this.begin_close(cx);
                assert_eq!(this.held_keys, 0);
                assert!(!this.key(3, false));
            })
            .unwrap();
        assert!(second_input.is_closed());
        assert_eq!(second_input.len(), 1);
    }
    #[gpui::test]
    fn restore_reuses_private_ui_and_accepts_no_old_input_or_new_owner(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let field = |value: &str| ViewNode::TextField {
            id: 1,
            value: value.into(),
            placeholder: String::new(),
            multiline: false,
            disabled: false,
            read_only: false,
        };
        let (handle, inputs) = Handle::fixture(7);
        let mut owners = Windows::default();
        let window = cx.update(|cx| {
            let window = open(
                "Retained".into(),
                Some(handle.clone()),
                Some(field("Private draft")),
                true,
                cx,
            )
            .unwrap();
            owners.owners.insert(
                7,
                bind_window(window, false, owners.retained.clone(), "Retained".into(), cx),
            );
            refresh_retained(&owners.owners, &owners.retained);
            window
        });
        let entity_id = owners.owners[&7].entity.entity_id();
        #[cfg(all(target_os = "macos", feature = "process"))]
        window
            .update(cx, |this, _, _| {
                this.startup_sent = true;
                this.startup_rendered = Some(true);
            })
            .unwrap();
        window
            .update(cx, |this, window, cx| {
                this.scene.editors[&1].update(cx, |editor, _| editor.invalidate_delivery());
                assert!(this.close(cx));
                window.remove_window();
            })
            .unwrap();
        assert!(cx.update(|cx| cx.windows().is_empty()));
        cx.update(|cx| {
            deliver(
                Output::Frame {
                    pending: None,
                    generation: 7,
                    result: Ok(Some(field("Committed update"))),
                    logs: vec![],
                    ack: InputAck::default(),
                    frame_input: false,
                    quit_on_last_window: Some(false),
                },
                &mut owners.owners,
                &owners.retained,
                Rc::new(Cell::new(false)),
                true,
                cx,
            )
        });
        cx.update(|cx| {
            assert_eq!(
                owners.owners[&7].entity.read(cx).scene.editors[&1].read(cx).text(),
                "Private draft"
            );
            reopen(&mut owners, cx);
            #[cfg(all(target_os = "macos", feature = "process"))]
            {
                let reopened = owners.owners[&7].entity.read(cx);
                assert!(!reopened.startup_sent, "the replacement window must render again");
                assert!(reopened.startup_rendered.is_none());
                assert!(reopened.startup_requires_focus);
            }
            reopen(&mut owners, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            1,
            "repeated explicit Show keeps one interface"
        );
        assert_eq!(owners.owners[&7].entity.entity_id(), entity_id);
        owners.owners[&7]
            .window
            .update(cx, |this, _, cx| {
                assert_eq!(this.handle.as_ref().unwrap().generation, 7);
                assert_eq!(this.scene.editors[&1].read(cx).text(), "Private draft");
            })
            .unwrap();
        while let Ok(message) = inputs.try_recv() {
            assert_eq!(
                message.input.kind,
                EventKind::SurfaceChanged,
                "restore never replays a draft/action"
            );
        }
        owners.owners[&7]
            .window
            .update(cx, |this, window, cx| {
                assert!(this.close(cx));
                window.remove_window();
            })
            .unwrap();
        assert!(!inputs.is_closed());
        cx.update(|cx| deactivate(&owners, cx));
        assert!(inputs.is_closed(), "Quit boundary closes hidden admission immediately");
        cx.update(|cx| reopen(&mut owners, cx));
        assert!(cx.update(|cx| cx.windows().is_empty()), "draining owner cannot restore admission");
    }

    #[gpui::test]
    fn windowless_intent_retains_admission_and_prevents_other_last_window_quit(
        cx: &mut gpui::TestAppContext,
    ) {
        use playbit_api::abi::environment::Surface;
        let (retained_handle, inputs) = Handle::fixture(1);
        let (quitting_handle, quitting_inputs) = Handle::fixture(2);
        let retained = Rc::new(Cell::new(0));
        let mut owners = BTreeMap::new();
        let first = cx.update(|cx| {
            let window =
                open("Retained".into(), Some(retained_handle.clone()), None, true, cx).unwrap();
            owners.insert(1, bind_window(window, false, retained.clone(), "Retained".into(), cx));
            refresh_retained(&owners, &retained);
            window
        });
        first
            .update(cx, |this, window, cx| {
                assert!(this.close(cx));
                window.remove_window();
            })
            .unwrap();
        assert!(!inputs.is_closed(), "window close retains the same input owner");
        let notification = inputs.try_recv().unwrap().input;
        assert_eq!(notification.kind, EventKind::SurfaceChanged);
        let surface = Surface::decode(&notification.data).unwrap();
        assert_eq!((surface.width, surface.height, surface.inputs), (0, 0, 0));
        assert!(!owners[&1].visible.get());
        assert_eq!(retained.get(), 1);
        let second = cx.update(|cx| {
            let window = open("Quitting".into(), Some(quitting_handle), None, true, cx).unwrap();
            owners.insert(2, bind_window(window, true, retained.clone(), "Quitting".into(), cx));
            refresh_retained(&owners, &retained);
            window
        });
        second.update(cx, |this, _, cx| assert!(!this.close(cx))).unwrap();
        assert!(quitting_inputs.is_closed(), "only the closing true-intent owner drains");
        assert!(!inputs.is_closed(), "hidden sibling remains independent");
        cx.update(|cx| {
            deliver(Output::Closed(2), &mut owners, &retained, Rc::new(Cell::new(false)), true, cx)
        });
        assert!(cx.update(|cx| cx.windows().is_empty()));
        assert_eq!(retained.get(), 1);
        #[cfg(feature = "process")]
        {
            let (handle, _) = Handle::fixture(3);
            let third = cx.update(|cx| {
                let window = open("Closing".into(), Some(handle), None, true, cx).unwrap();
                owners.insert(3, bind_window(window, true, retained.clone(), "Closing".into(), cx));
                refresh_retained(&owners, &retained);
                window
            });
            third.update(cx, |this, _, cx| assert!(!this.close(cx))).unwrap();
            cx.update(|cx| {
                deliver(
                    Output::ContextUnavailable(3),
                    &mut owners,
                    &retained,
                    Rc::new(Cell::new(false)),
                    true,
                    cx,
                );
            });
            assert!(!owners.contains_key(&3));
            assert!(cx.update(|cx| cx.windows().is_empty()));
            assert_eq!(retained.get(), 1, "closing completion preserves the hidden sibling");
            cx.run_until_parked();
            assert!(cx.update(crate::ui::desktop::alert::take_presented).is_empty());
        }
    }

    #[gpui::test]
    fn explicit_quit_deactivation_closes_false_intent_admission(cx: &mut gpui::TestAppContext) {
        let (handle, inputs) = Handle::fixture(1);
        let retained = Rc::new(Cell::new(1));
        let window = cx.update(|cx| {
            let window = open("Retained".into(), Some(handle), None, true, cx).unwrap();
            bind_window(window, false, retained, "Retained".into(), cx);
            window
        });
        cx.update(|cx| deactivate(&Windows::default(), cx));
        assert!(inputs.is_closed());
        window.update(cx, |this, _, _| assert!(this.closing)).unwrap();
    }

    #[gpui::test]
    fn retained_close_refuses_a_full_queue_before_removing_the_interface(
        cx: &mut gpui::TestAppContext,
    ) {
        let (handle, inputs) = Handle::fixture(1);
        let input = || Input { kind: EventKind::Click, target: 1, data: vec![], revision: None };
        while handle.send(input(), None) {}
        let window = cx.update(|cx| {
            let window = open("Retained".into(), Some(handle.clone()), None, true, cx).unwrap();
            bind_window(window, false, Rc::new(Cell::new(1)), "Retained".into(), cx);
            window
        });
        window
            .update(cx, |this, _, cx| {
                assert!(!this.close(cx));
                assert!(this.visible.get());
                assert!(!this.closing);
            })
            .unwrap();
        assert!(!inputs.is_closed());
        inputs.try_recv().unwrap();
        window
            .update(cx, |this, window, cx| {
                assert!(this.close(cx));
                window.remove_window();
            })
            .unwrap();
        assert!(!inputs.is_closed());
    }

    #[gpui::test]
    fn reconstructed_scene_keeps_private_draft_until_explicit_submission(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let field = |value: &str| ViewNode::TextField {
            id: 1,
            value: value.into(),
            placeholder: String::new(),
            multiline: false,
            disabled: false,
            read_only: false,
        };
        let window = cx.update(|cx| {
            open("Fixture".into(), None, Some(field("Private draft")), true, cx).unwrap()
        });
        cx.run_until_parked();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let old_sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        window
            .update(cx, |this, _, cx| {
                let old_sent = old_sent.clone();
                this.scene.rebind(
                    Arc::new(move |input| {
                        old_sent.lock().unwrap().push(input);
                        true
                    }),
                    cx,
                );
            })
            .unwrap();
        window.update(cx, |this, _, cx| this.stop_inputs(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert!(
                    !this.scene.send_drafts(this.view.as_ref(), false, cx),
                    "stopped fields cannot send input"
                );
                assert_eq!(this.scene.editors[&1].read(cx).text(), "Private draft");
            })
            .unwrap();
        assert!(
            old_sent.lock().unwrap().is_empty(),
            "stopped fields never invoke the retired sender"
        );
        window
            .update(cx, |this, _, cx| {
                let sent = sent.clone();
                this.scene.rebind(
                    Arc::new(move |input| {
                        sent.lock().unwrap().push(input);
                        true
                    }),
                    cx,
                );
                this.apply(field("Committed text"), InputAck::default(), cx);
                assert_eq!(this.scene.editors[&1].read(cx).text(), "Private draft");
            })
            .unwrap();
        cx.run_until_parked();
        assert!(sent.lock().unwrap().is_empty(), "reconstruction never sends a retained draft");
        window
            .update(cx, |this, _, cx| {
                assert!(this.scene.send_drafts(this.view.as_ref(), false, cx));
            })
            .unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, EventKind::TextChanged);
        assert_eq!(
            playbit_api::abi::text::TextInput::decode(&sent[0].data).expect("edit").text,
            "Private draft"
        );
    }
    #[gpui::test]
    fn async_completion_redraws_only_visible_changes_or_canvas_progress(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (handle, _inputs) = Handle::fixture(7);
        let mut owners = Windows::default();
        cx.update(|cx| {
            let window = open("Completion".into(), Some(handle), None, true, cx).unwrap();
            owners.owners.insert(
                7,
                bind_window(window, true, owners.retained.clone(), "Completion".into(), cx),
            );
        });
        cx.run_until_parked();
        let entity = owners.owners[&7].entity.clone();
        let notifications = Rc::new(Cell::new(0));
        let observed = notifications.clone();
        let _subscription = cx.update(|cx| {
            cx.observe(&owners.owners[&7].entity, move |_, _| {
                observed.set(observed.get() + 1);
            })
        });
        let mut complete = |view, logs, frame_input, ack, cx: &mut gpui::TestAppContext| {
            notifications.set(0);
            cx.update(|cx| {
                deliver(
                    Output::Frame {
                        pending: None,
                        generation: 7,
                        result: Ok(view),
                        logs,
                        ack,
                        frame_input,
                        quit_on_last_window: Some(true),
                    },
                    &mut owners.owners,
                    &owners.retained,
                    Rc::new(Cell::new(false)),
                    true,
                    cx,
                );
            });
            cx.run_until_parked();
            notifications.get()
        };
        assert_eq!(complete(None, vec![], false, InputAck::default(), cx), 0);
        assert_eq!(complete(None, vec!["Retained log".into()], false, InputAck::default(), cx), 0);
        assert_eq!(
            complete(
                None,
                vec!["é".repeat(MAX_LOG_BYTES), "After large message".into()],
                false,
                InputAck::default(),
                cx
            ),
            0
        );
        cx.update(|cx| {
            let state = entity.read(cx);
            assert!(state.error.is_empty());
            assert!(!state.stopped);
            assert_eq!(state.logs.len(), 1);
            assert_eq!(state.logs.back().unwrap().0, "After large message");
            assert_eq!(state.log_bytes, "After large message".len());
        });
        assert!(complete(None, vec![], true, InputAck::default(), cx) > 0);
        cx.update(|cx| entity.update(cx, |this, _| this.log_open = true));
        assert!(complete(None, vec!["Visible log".into()], false, InputAck::default(), cx) > 0);
        cx.update(|cx| entity.update(cx, |this, _| this.log_open = false));
        let canvas = |frames| ViewNode::Canvas {
            id: 1,
            width: 32,
            height: 32,
            frames,
            viewport: false,
            items: vec![],
        };
        assert!(
            complete(
                Some(canvas(playbit_api::ui::CanvasFrames::Static)),
                vec![],
                false,
                InputAck::default(),
                cx
            ) > 0
        );
        assert_eq!(complete(None, vec![], false, InputAck::default(), cx), 0);
        assert!(
            complete(
                Some(canvas(playbit_api::ui::CanvasFrames::Continuous)),
                vec![],
                false,
                InputAck::default(),
                cx
            ) > 0
        );
        assert!(complete(None, vec![], false, InputAck::default(), cx) > 0);
    }
    #[gpui::test]
    fn critical_failure_stops_existing_inputs_without_creating_an_error_window(
        cx: &mut gpui::TestAppContext,
    ) {
        let (handle, inputs) = Handle::fixture(1);
        let window =
            cx.update(|cx| open("Active app".into(), Some(handle), None, true, cx).unwrap());
        let windows = Rc::new(RefCell::new(Windows::default()));
        let acknowledged = Rc::new(Cell::new(false));
        let (send, output) = async_channel::bounded(1);
        cx.update(|cx| {
            let mut windows = windows.borrow_mut();
            let retained = windows.retained.clone();
            windows.owners.insert(1, bind_window(window, false, retained, "Active app".into(), cx));
        });
        cx.update(|cx| receive(output, acknowledged.clone(), windows, true, cx));
        drop(send);
        cx.run_until_parked();
        assert!(acknowledged.get(), "unexpected live-owner channel loss stays fatal");
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        window
            .update(cx, |this, _, cx| {
                assert!(this.stopped);
                this.click(1, cx);
            })
            .unwrap();
        assert!(inputs.is_empty());
    }
}
