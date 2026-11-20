//! One reusable borderless child window for semantic guest menus on macOS.
#![allow(
    unsafe_code,
    reason = "AppKit owns menu attachment and filters input before GPUI dispatch."
)]

use super::{controls, zoom};
use crate::runtime::{MenuEntry, MenuGlyph};
use block::ConcreteBlock;
use cocoa::{
    appkit::NSWindowCollectionBehavior,
    foundation::{NSPoint, NSRect},
};
use gpui::{
    AnyWindowHandle, App, AsyncApp, Bounds, BoxShadow, Context, FocusHandle, Global, Hsla,
    MouseButton, Pixels, Render, ScrollHandle, WeakEntity, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, point, prelude::*, px, rgba, size,
};
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::Cell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

const ITEM_HEIGHT: f32 = 24.;
const PANEL_PAD: f32 = 4.;
static MENU_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_modal_active() -> bool {
    MENU_ACTIVE.load(Ordering::Acquire)
}

#[derive(Clone)]
pub struct Request {
    owner: AnyWindowHandle,
    pub(super) opening_press: bool,
    pub(super) update_action: Option<u32>,
    display: Bounds<Pixels>,
    anchor: Bounds<Pixels>,
    label: Option<String>,
    above: bool,
    below: bool,
    align_icon: bool,
    items: Vec<MenuEntry>,
    activate: controls::Activate,
    control: Option<WeakEntity<controls::Control>>,
    scale: f32,
    pixel_scale: f32,
}

impl Request {
    pub fn button(
        window: &Window,
        bounds: Bounds<Pixels>,
        label: String,
        items: Vec<MenuEntry>,
        activate: controls::Activate,
        control: WeakEntity<controls::Control>,
        cx: &App,
    ) -> Self {
        let origin = window.bounds().origin;
        let screen_bounds =
            Bounds::new(point(origin.x + bounds.origin.x, origin.y + bounds.origin.y), bounds.size);
        Self {
            owner: window.window_handle(),
            opening_press: false,
            update_action: None,
            display: window.display(cx).map(|display| display.bounds()).unwrap_or(window.bounds()),
            anchor: screen_bounds,
            label: Some(label),
            above: false,
            below: false,
            align_icon: false,
            items,
            activate,
            control: Some(control),
            scale: zoom::factor(cx),
            pixel_scale: window.scale_factor(),
        }
    }

    pub fn above_button(mut self) -> Self {
        self.label = None;
        self.above = true;
        self
    }

    pub fn below_button(mut self) -> Self {
        self.label = None;
        self.below = true;
        self
    }

    /// Center the first section's glyphs under a glyph-only trigger.
    pub fn align_icon_to_button(mut self) -> Self {
        self.align_icon = true;
        self
    }

    /// Retain the context target's presentation until this menu closes.
    pub fn target(mut self, control: WeakEntity<controls::Control>) -> Self {
        self.control = Some(control);
        self
    }

    pub fn context(
        window: &Window,
        position: gpui::Point<Pixels>,
        items: Vec<MenuEntry>,
        activate: controls::Activate,
        cx: &App,
    ) -> Self {
        let origin = window.bounds().origin;
        let at = point(origin.x + position.x, origin.y + position.y);
        Self {
            owner: window.window_handle(),
            opening_press: false,
            update_action: None,
            display: window.display(cx).map(|display| display.bounds()).unwrap_or(window.bounds()),
            anchor: Bounds::new(at, size(px(0.), px(0.))),
            label: None,
            above: false,
            below: false,
            align_icon: false,
            items,
            activate,
            control: None,
            scale: zoom::factor(cx),
            pixel_scale: window.scale_factor(),
        }
    }
}

#[derive(Default)]
struct Coordinator {
    active: Option<ActiveMenu>,
    idle_menu: Option<gpui::WindowHandle<MenuView>>,
}
impl Global for Coordinator {}

struct ActiveMenu {
    owner: AnyWindowHandle,
    menu: gpui::WindowHandle<MenuView>,
    monitor: *mut Object,
    deactivation_observer: *mut Object,
    _owner_closed: gpui::Subscription,
    _activation: gpui::Subscription,
    running: Rc<Cell<bool>>,
    selected: Rc<Cell<Option<u32>>>,
    activate: controls::Activate,
    control: Option<WeakEntity<controls::Control>>,
    opened_at: Instant,
    opening_press: bool,
}

fn item_height(item: &MenuEntry) -> f32 {
    if matches!(item, MenuEntry::Separator) { 8. } else { ITEM_HEIGHT }
}

fn row_offset(items: &[MenuEntry], scale: f32, display: f32) -> Pixels {
    items
        .iter()
        .fold(px(0.), |sum, item| sum + px((item_height(item) * scale * display).round() / display))
}

fn menu_height(items: &[MenuEntry], scale: f32, display: f32) -> Pixels {
    row_offset(items, scale, display) + px((PANEL_PAD * scale * display).round() / display) * 2.
}

fn selected_index(items: &[MenuEntry], label: Option<&str>) -> usize {
    label
        .and_then(|value| {
            items.iter().position(|item| {
                matches!(item,
        MenuEntry::Action { label, .. } if label == value)
            })
        })
        .or_else(|| {
            items
                .iter()
                .position(|item| matches!(item, MenuEntry::Action { checked: Some(true), .. }))
        })
        .unwrap_or(0)
}

fn has_check_column(items: &[MenuEntry]) -> bool {
    items.iter().any(|item| matches!(item, MenuEntry::Action { checked: Some(_), .. }))
}

fn item_icon(item: &MenuEntry) -> Option<MenuGlyph> {
    match item {
        MenuEntry::Action { icon, .. } | MenuEntry::Submenu { icon, .. } => *icon,
        MenuEntry::Separator => None,
    }
}

// Scan each section once. Measurement and rendering use identical column rules.
fn menu_rows(items: &[MenuEntry]) -> impl Iterator<Item = (&MenuEntry, bool)> {
    items.split_inclusive(|item| matches!(item, MenuEntry::Separator)).flat_map(|section| {
        let icons = section.iter().any(|item| item_icon(item).is_some());
        section.iter().map(move |item| (item, icons))
    })
}

fn icon_path(icon: MenuGlyph) -> &'static str {
    use playbit_api::ui::MenuIcon;
    match icon {
        MenuGlyph::Builtin(MenuIcon::Avatar) => "menu-avatar",
        MenuGlyph::Builtin(MenuIcon::Signature) => "menu-signature",
        MenuGlyph::Builtin(MenuIcon::Copy) => "menu-copy",
        MenuGlyph::Builtin(MenuIcon::ExitRight) => "menu-exit-right",
        MenuGlyph::Builtin(MenuIcon::OpenBook) => "menu-open-book",
        MenuGlyph::Builtin(MenuIcon::Talk) => "menu-talk",
        MenuGlyph::Asset(path) => path,
    }
}

fn menu_width(items: &[MenuEntry], scale: f32, pixel_scale: f32, window: &Window) -> Pixels {
    let style = gpui::TextStyle {
        font_family: super::UI_FONT_FAMILY.into(),
        font_weight: gpui::FontWeight::MEDIUM,
        letter_spacing: px(0.),
        ..Default::default()
    };
    let sp = |value: f32| px((value * scale * pixel_scale).round() / pixel_scale);
    let check_column = has_check_column(items);
    let width = menu_rows(items)
        .map(|(item, icons)| match item {
            MenuEntry::Action { label, .. } | MenuEntry::Submenu { label, .. } => {
                let line = label.replace(['\n', '\r'], " ");
                window
                    .text_system()
                    .shape_line(line.clone().into(), sp(11.), &[style.to_run(line.len())], None)
                    .width
                    + sp(if check_column { 4. } else { 12. })
                    + sp(8.)
                    + if check_column { sp(16.) + sp(4.) } else { px(0.) }
                    + if icons { sp(20.) + sp(4.) } else { px(0.) }
                    + if matches!(item, MenuEntry::Submenu { .. }) {
                        sp(8.) + sp(4.)
                    } else {
                        px(0.)
                    }
            }
            MenuEntry::Separator => px(0.),
        })
        .fold(px(0.), |a, b| a.max(b));
    px((f32::from(width) * pixel_scale).ceil() / pixel_scale)
}

fn request_width(request: &Request, window: &Window) -> Pixels {
    let content = menu_width(&request.items, request.scale, request.pixel_scale, window);
    let width =
        if request.label.is_some() { content.max(request.anchor.size.width) } else { content };
    let width = if request.above || request.below {
        width.max(px((159. * request.scale * request.pixel_scale).ceil() / request.pixel_scale))
    } else {
        width
    };
    width.min(request.display.size.width - px(8. * request.scale))
}

fn place(request: &Request, width: Pixels, height: Pixels) -> Bounds<Pixels> {
    let unit = |value: f32| {
        px((value * request.scale * request.pixel_scale).round() / request.pixel_scale)
    };
    let display = request.display;
    let right = display.right() - unit(4.);
    let bottom = display.bottom() - unit(4.);
    let top = display.top() + unit(4.);
    let (x, y) = if request.above {
        (request.anchor.left(), request.anchor.top() - height)
    } else if request.below {
        let icons = menu_rows(&request.items)
            .find(|(item, _)| !matches!(item, MenuEntry::Separator))
            .is_some_and(|(_, icons)| icons);
        let x = if request.align_icon && icons {
            let checks = has_check_column(&request.items);
            let icon_center = unit(if checks { 4. } else { 12. })
                + if checks { unit(16.) + unit(4.) } else { px(0.) }
                + unit(16.) / 2.;
            request.anchor.center().x - icon_center
        } else {
            request.anchor.left()
        };
        (x, request.anchor.bottom())
    } else if request.label.is_some() {
        let selected = selected_index(&request.items, request.label.as_deref());
        let before = row_offset(&request.items[..selected], request.scale, request.pixel_scale);
        let icons = menu_rows(&request.items).nth(selected).is_some_and(|(_, icons)| icons);
        let columns =
            if has_check_column(&request.items) { 12. } else { 0. } + if icons { 24. } else { 0. };
        (
            request.anchor.left() - unit(columns),
            request.anchor.top() + request.anchor.size.height / 2.
                - unit(PANEL_PAD)
                - before
                - unit(ITEM_HEIGHT) / 2.,
        )
    } else {
        (request.anchor.left() + unit(1.), request.anchor.top() - unit(PANEL_PAD + 1.))
    };
    Bounds::new(
        point(x.max(display.left() + unit(4.)).min(right - width), y.max(top).min(bottom - height)),
        size(width, height),
    )
}

pub fn open(request: Request, cx: &mut App) {
    if !cx.has_global::<Coordinator>() {
        cx.set_global(Coordinator::default());
    }
    if cx.global::<Coordinator>().active.is_some() || request.items.is_empty() {
        return;
    }
    if request.control.as_ref().is_some_and(|target| target.upgrade().is_none()) {
        return;
    }
    let Ok(width) = request.owner.update(cx, |_, window, _| request_width(&request, window)) else {
        return;
    };
    let full_height = menu_height(&request.items, request.scale, request.pixel_scale);
    let margin = px((4. * request.scale * request.pixel_scale).round() / request.pixel_scale);
    let height = full_height.min(request.display.size.height - margin * 2.);
    let bounds = place(&request, width, height);
    let selected_row = if request.label.is_some() {
        selected_index(&request.items, request.label.as_deref())
    } else {
        request.items.len().saturating_sub(1)
    };
    let before = row_offset(&request.items[..selected_row], request.scale, request.pixel_scale);
    let selected_center = before
        + px((PANEL_PAD * request.scale * request.pixel_scale).round() / request.pixel_scale)
        + px((ITEM_HEIGHT * request.scale * request.pixel_scale).round() / request.pixel_scale)
            / 2.;
    let anchor_center = request.anchor.top() + request.anchor.size.height / 2.;
    let scroll = if request.label.is_some() {
        (selected_center - (anchor_center - bounds.top())).max(px(0.)).min(full_height - height)
    } else {
        px(0.)
    };
    let display_id =
        request.owner.update(cx, |_, window, cx| window.display(cx).map(|d| d.id())).ok().flatten();
    let running = Rc::new(Cell::new(true));
    let selected = Rc::new(Cell::new(None));
    let recycled = cx.global_mut::<Coordinator>().idle_menu.take();
    let result = if let Some(handle) = recycled {
        handle
            .update(cx, |view, window, cx| {
                *view =
                    MenuView::new(&request, bounds, scroll, running.clone(), selected.clone(), cx);
                window.focus(&view.focus);
            })
            .map(|()| handle)
    } else {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(shadow_bounds(
                    bounds,
                    request.scale,
                    request.pixel_scale,
                ))),
                display_id,
                titlebar: None,
                kind: WindowKind::PopUp,
                window_background: WindowBackgroundAppearance::Transparent,
                is_movable: false,
                is_resizable: false,
                focus: false,
                show: false,
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    MenuView::new(&request, bounds, scroll, running.clone(), selected.clone(), cx)
                });
                window.focus(&view.read(cx).focus);
                view
            },
        )
    };
    let handle = match result {
        Ok(handle) => handle,
        Err(error) => {
            crate::diagnostic!("Cannot open guest menu: {error}");
            return;
        }
    };
    let native = handle
        .update(cx, |_, window, _| Some((native_window(window)?, native_view(window)?)))
        .ok()
        .flatten();
    let Some((native_window, native_view)) = native else {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        return;
    };
    if !show_menu(
        native_window,
        native_view,
        request.owner,
        shadow_bounds(bounds, request.scale, request.pixel_scale),
        running.clone(),
        cx,
    ) {
        crate::diagnostic!("Cannot attach guest menu to its owner");
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        return;
    }
    if let Some(control) = &request.control {
        let _ = control.update(cx, |control, cx| control.set_popup_open(true, cx));
    }
    let monitor = install_modal_monitor(native_window, native_view, running.clone(), cx.to_async());
    if monitor.is_null() {
        running.set(false);
        crate::diagnostic!("Cannot install modal menu input monitor");
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        if let Some(control) = &request.control {
            let _ = control.update(cx, |control, cx| control.set_popup_open(false, cx));
        }
        return;
    }
    let deactivation_observer = install_deactivation_observer(running.clone(), cx);
    let owner = request.owner;
    let owner_running = running.clone();
    let owner_closed = cx.on_window_closed(move |cx| {
        if !cx.windows().contains(&owner) {
            owner_running.set(false);
            cx.defer(finish);
        }
    });
    let activation = handle
        .update(cx, |_, window, cx| {
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && this.running.get() {
                    cx.defer(finish);
                }
            })
        })
        .expect("newly opened menu remains available");
    MENU_ACTIVE.store(true, Ordering::Release);
    cx.global_mut::<Coordinator>().active = Some(ActiveMenu {
        owner: request.owner,
        menu: handle,
        monitor,
        deactivation_observer,
        _owner_closed: owner_closed,
        _activation: activation,
        running: running.clone(),
        selected,
        activate: request.activate,
        control: request.control,
        opened_at: Instant::now(),
        opening_press: request.opening_press,
    });
    crate::trace::mark("ui.menu.open");
}

/// Cancel a menu before its owner changes scale, retaining no stale hit regions.
pub fn dismiss(cx: &mut App) {
    if !cx.has_global::<Coordinator>() {
        return;
    }
    if let Some(active) = &cx.global::<Coordinator>().active {
        active.selected.set(None);
    }
    finish(cx);
}

/// Retire cached presentation before replacing a host's profile/UI owners.
/// A later menu must receive a live window rather than a closed cached handle.
pub fn retire(cx: &mut App) {
    dismiss(cx);
    if cx.has_global::<Coordinator>()
        && let Some(menu) = cx.global_mut::<Coordinator>().idle_menu.take()
    {
        let _ = menu.update(cx, |_, window, _| window.remove_window());
    }
}

fn finish(cx: &mut App) {
    let Some(active) = cx.global_mut::<Coordinator>().active.take() else { return };
    active.running.set(false);
    unsafe {
        let _: () = msg_send![class!(NSEvent), removeMonitor: active.monitor];
        let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![center, removeObserver: active.deactivation_observer];
    }
    MENU_ACTIVE.store(false, Ordering::Release);
    let retained = active
        .menu
        .update(cx, |view, window, _| {
            view.cancel_tracking();
            if let Some(native) = native_window(window) {
                unsafe {
                    let _: () = msg_send![native, orderOut: std::ptr::null_mut::<Object>()];
                    let parent: *mut Object = msg_send![native, parentWindow];
                    if !parent.is_null() {
                        let _: () = msg_send![parent, removeChildWindow: native];
                    }
                }
                true
            } else {
                window.remove_window();
                false
            }
        })
        .unwrap_or(false);
    if retained {
        cx.global_mut::<Coordinator>().idle_menu = Some(active.menu);
    }
    if let Some(control) = active.control {
        let _ = control.update(cx, |control, cx| control.set_popup_open(false, cx));
    }
    let owner_live = active.owner.update(cx, |_, _, _| ()).is_ok();
    if owner_live && let Some(id) = active.selected.get() {
        crate::trace::mark("ui.menu.select");
        (active.activate)(id, cx);
    }
    crate::trace::mark("ui.menu.close");
}

fn opening_gesture(kind: NativeGesture, cx: &mut App) {
    let Some(active) = &mut cx.global_mut::<Coordinator>().active else { return };
    if !active.opening_press {
        return;
    }
    let held = active.opened_at.elapsed() >= std::time::Duration::from_millis(200);
    match kind {
        NativeGesture::Drag(position) if held => {
            let menu = active.menu;
            let _ = menu.update(cx, |view, window, cx| {
                view.drag_from_button(position, window, cx);
            });
        }
        NativeGesture::Release(position) => {
            active.opening_press = false;
            if held {
                let menu = active.menu;
                let _ = menu.update(cx, |view, window, cx| {
                    view.release_held_opening_press(position, window, cx);
                });
            }
        }
        _ => {}
    }
}

// Panel bounds are screen coordinates. The single transparent native window is
// the union of these panels plus shadow outsets; no panel is a second window.
struct MenuPanel {
    items: Vec<MenuEntry>,
    bounds: Bounds<Pixels>,
    selected: Option<usize>,
    scroll: ScrollHandle,
}
impl MenuPanel {
    fn enabled(&self, index: usize) -> bool {
        matches!(
            self.items.get(index),
            Some(
                MenuEntry::Action { disabled: false, .. }
                    | MenuEntry::Submenu { disabled: false, .. }
            )
        )
    }
    fn row_top(&self, index: usize, window: &Window) -> Pixels {
        self.bounds.top()
            + zoom::pixels(window, PANEL_PAD)
            + row_offset(
                &self.items[..index],
                f32::from(window.rem_size()) / 16.,
                window.scale_factor(),
            )
            + self.scroll.offset().y
    }
}
struct MenuView {
    panels: Vec<MenuPanel>,
    update_action: Option<u32>,
    focus: FocusHandle,
    keyboard_panel: usize,
    typeahead: String,
    last_typeahead: Option<Instant>,
    submenu_task: Option<gpui::Task<()>>,
    aim_task: Option<gpui::Task<()>>,
    pointer: Option<gpui::Point<Pixels>>,
    armed: Option<(usize, usize)>,
    #[cfg_attr(test, allow(dead_code, reason = "Test windows do not have a native frame"))]
    owner: AnyWindowHandle,
    display: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    running: Rc<Cell<bool>>,
    result: Rc<Cell<Option<u32>>>,
    scale: f32,
}

impl MenuView {
    fn new(
        request: &Request,
        bounds: Bounds<Pixels>,
        offset: Pixels,
        running: Rc<Cell<bool>>,
        result: Rc<Cell<Option<u32>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroll = ScrollHandle::new();
        scroll.set_offset(point(px(0.), -offset));
        Self {
            panels: vec![MenuPanel {
                items: request.items.clone(),
                bounds,
                selected: request
                    .label
                    .as_deref()
                    .map(|label| selected_index(&request.items, Some(label))),
                scroll,
            }],
            update_action: request.update_action,
            focus: cx.focus_handle(),
            keyboard_panel: 0,
            typeahead: String::new(),
            last_typeahead: None,
            submenu_task: None,
            aim_task: None,
            pointer: None,
            armed: None,
            owner: request.owner,
            display: request.display,
            bounds: shadow_bounds(bounds, request.scale, request.pixel_scale),
            running,
            result,
            scale: request.scale,
        }
    }
    fn cancel_tracking(&mut self) {
        self.running.set(false);
        self.submenu_task = None;
        self.aim_task = None;
        self.armed = None;
    }
    fn hit(&self, position: gpui::Point<Pixels>, window: &Window) -> Option<(usize, usize)> {
        let position = position + self.bounds.origin;
        for (panel_index, panel) in self.panels.iter().enumerate().rev() {
            if !panel.bounds.contains(&position) {
                continue;
            }
            for (index, item) in panel.items.iter().enumerate() {
                let top = panel.row_top(index, window);
                if position.y >= top && position.y < top + zoom::pixels(window, item_height(item)) {
                    return Some((panel_index, index));
                }
            }
        }
        None
    }
    fn fit_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut bounds = self.panels[0].bounds;
        for panel in &self.panels[1..] {
            bounds = bounds.union(&panel.bounds);
        }
        let next = shadow_bounds(bounds, self.scale, window.scale_factor());
        if next == self.bounds {
            cx.notify();
            return;
        }
        self.bounds = next;
        // GPUI test windows have no native handle. Geometry/state still run above.
        #[cfg(not(test))]
        if let Some(native) = native_window(window)
            && let Some(view) = native_view(window)
        {
            show_menu(native, view, self.owner, self.bounds, self.running.clone(), cx);
        }
        cx.notify();
    }
    fn select(
        &mut self,
        panel: usize,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.panels.get(panel) else { return };
        let index = index.filter(|index| current.enabled(*index));
        if current.selected == index {
            return;
        }
        self.submenu_task = None;
        self.panels[panel].selected = index;
        self.panels.truncate(panel + 1);
        if self.keyboard_panel != panel {
            self.last_typeahead = None;
        }
        self.keyboard_panel = panel;
        if index.is_some_and(|i| matches!(self.panels[panel].items[i], MenuEntry::Submenu { .. })) {
            // AppKit opens a selected submenu after roughly 200ms. A new
            // selection drops this task; Right opens it immediately below.
            self.submenu_task = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                let _ = cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        if this.running.get() {
                            this.open_child(panel, window, cx);
                            this.fit_window(window, cx);
                        }
                    })
                });
            }));
        }
        self.fit_window(window, cx);
    }
    fn open_child(&mut self, panel: usize, window: &mut Window, _cx: &mut Context<Self>) {
        if self.panels.len() > panel + 1 {
            return;
        }
        let parent = &self.panels[panel];
        let Some(index) = parent.selected else { return };
        let Some(MenuEntry::Submenu { items, disabled: false, .. }) = parent.items.get(index)
        else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let width = menu_width(items, self.scale, window.scale_factor(), window)
            .min(self.display.size.width - zoom::pixels(window, 8.));
        let full_height = menu_height(items, self.scale, window.scale_factor());
        let margin = zoom::pixels(window, 4.);
        let height = full_height.min(self.display.size.height - 2. * margin);
        let gap = zoom::pixels(window, 1.);
        let x = if parent.bounds.right() + gap + width <= self.display.right() - margin {
            parent.bounds.right() + gap
        } else {
            parent.bounds.left() - gap - width
        };
        let y = parent.row_top(index, window) - zoom::pixels(window, PANEL_PAD);
        let bounds = Bounds::new(
            point(
                x.max(self.display.left() + margin),
                y.max(self.display.top() + margin).min(self.display.bottom() - margin - height),
            ),
            size(width, height),
        );
        self.panels.push(MenuPanel {
            items: items.clone(),
            bounds,
            selected: None,
            scroll: ScrollHandle::new(),
        });
    }
    fn pointer_moved(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_typeahead = None;
        let screen = position + self.bounds.origin;
        let previous = self.pointer.replace(screen);
        let hit = self.hit(position, window);
        let travel_parent = (0..self.panels.len().saturating_sub(1)).rev().find(|&p| {
            if hit.is_some_and(|(panel, _)| panel > p) {
                return false;
            }
            let parent = &self.panels[p];
            let Some(selected) = parent.selected else {
                return false;
            };
            if hit == Some((p, selected)) {
                return false;
            }
            let child = self.panels[p + 1].bounds;
            let row = Bounds::new(
                point(parent.bounds.left(), parent.row_top(selected, window)),
                size(parent.bounds.size.width, zoom::pixels(window, ITEM_HEIGHT)),
            );
            previous.is_some_and(|from| {
                let toward = if child.left() > parent.bounds.left() {
                    screen.x >= from.x
                } else {
                    screen.x <= from.x
                };
                toward && in_submenu_corridor(row, child, screen)
            })
        });
        self.aim_task = None;
        if let Some(parent) = travel_parent {
            // Each movement restarts the Figma 100ms idle deadline. The corridor
            // includes gaps/outside rows, not just another row's hitbox.
            self.aim_task = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                let _ = cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        if this.running.get() {
                            let hit =
                                this.pointer.and_then(|p| this.hit(p - this.bounds.origin, window));
                            if let Some((p, i)) = hit {
                                this.select(p, Some(i), window, cx);
                            } else {
                                this.select(parent, None, window, cx);
                            }
                        }
                        this.aim_task = None;
                    })
                });
            }));
            return;
        }
        if let Some((panel, index)) = hit {
            self.select(panel, Some(index), window, cx);
        } else {
            let parent = self.panels.len().saturating_sub(2);
            self.select(parent, None, window, cx);
        }
    }
    fn activate_index(
        &mut self,
        panel: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.panels.get(panel).and_then(|p| p.items.get(index)) {
            Some(MenuEntry::Action { id, disabled: false, .. }) => {
                self.result.set(Some(*id));
                self.running.set(false);
                cx.defer(finish);
            }
            Some(MenuEntry::Submenu { disabled: false, .. }) => {
                self.select(panel, Some(index), window, cx);
                self.keyboard_panel = panel;
                self.enter_child(window, cx);
            }
            _ => {}
        }
    }
    fn drag_from_button(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hit = self.hit(position, window).filter(|(p, i)| self.panels[*p].enabled(*i));
        if let Some((p, i)) = hit {
            self.select(p, Some(i), window, cx);
        }
        self.armed = hit;
        cx.notify();
    }
    fn release_held_opening_press(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The opening hold arms the row under the release even if no drag event
        // occurred after the 200ms threshold (including the current value).
        self.drag_from_button(position, window, cx);
        self.release_from_button(position, window, cx);
    }
    fn release_from_button(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hit = self.hit(position, window);
        if let Some((p, i)) = self.armed.take().filter(|armed| Some(*armed) == hit) {
            self.activate_index(p, i, window, cx);
        }
        cx.notify();
    }
    fn type_ahead(
        &mut self,
        text: &str,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() || text.chars().any(char::is_control) {
            return;
        }
        let fresh = self
            .last_typeahead
            .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_millis(200));
        if fresh {
            self.typeahead.clear();
        }
        let text = text.to_lowercase();
        if self.typeahead.len() + text.len() > 256 {
            return;
        }
        self.typeahead.push_str(&text);
        self.last_typeahead = Some(now);
        let p = self.keyboard_panel;
        let panel = &self.panels[p];
        let start = panel.selected.map_or(0, |i| if fresh { i + 1 } else { i });
        let found = (0..panel.items.len()).map(|i| (start + i) % panel.items.len()).find(|&i| {
            if !panel.enabled(i) {
                return false;
            }
            let label = match &panel.items[i] {
                MenuEntry::Action { label, .. } | MenuEntry::Submenu { label, .. } => label,
                MenuEntry::Separator => return false,
            };
            label.trim_start().to_lowercase().starts_with(&self.typeahead)
        });
        if let Some(i) = found {
            self.select(p, Some(i), window, cx);
            self.panels[p].scroll.scroll_to_item(i);
            cx.notify();
        }
    }
    fn move_keyboard(&mut self, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        let p = self.keyboard_panel;
        let panel = &self.panels[p];
        let len = panel.items.len();
        let start = panel.selected.map(|i| i as isize).unwrap_or(if direction > 0 {
            -1
        } else {
            len as isize
        });
        for step in 1..=len {
            let index = (start + direction * step as isize).rem_euclid(len as isize) as usize;
            if panel.enabled(index) {
                self.select(p, Some(index), window, cx);
                self.panels[p].scroll.scroll_to_item(index);
                break;
            }
        }
    }
    fn enter_child(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.last_typeahead = None;
        let p = self.keyboard_panel;
        self.submenu_task = None;
        self.open_child(p, window, cx);
        if self.panels.len() > p + 1 {
            let parent = &self.panels[p];
            let parent_index = parent.selected.expect("open submenu has a selected parent");
            let parent_y = parent.row_top(parent_index, window)
                + zoom::pixels(window, item_height(&parent.items[parent_index]) / 2.);
            let child = &self.panels[p + 1];
            let nearest = (0..child.items.len()).filter(|&i| child.enabled(i)).min_by(|&a, &b| {
                let distance = |i| {
                    f32::from(
                        child.row_top(i, window)
                            + zoom::pixels(window, item_height(&child.items[i]) / 2.)
                            - parent_y,
                    )
                    .abs()
                };
                distance(a).total_cmp(&distance(b))
            });
            self.keyboard_panel = p + 1;
            self.select(p + 1, nearest, window, cx);
            if let Some(i) = nearest {
                self.panels[p + 1].scroll.scroll_to_item(i);
            }
        }
        self.fit_window(window, cx);
    }
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.last_typeahead = None;
        if self.keyboard_panel > 0 {
            self.submenu_task = None;
            self.panels.truncate(self.keyboard_panel);
            self.keyboard_panel -= 1;
            self.fit_window(window, cx);
        }
    }
}

fn menu_accessibility_id(entity: u64, panel: usize, slot: usize) -> gpui::accesskit::NodeId {
    gpui::accesskit::NodeId(
        entity
            .checked_mul(65536)
            .and_then(|base| base.checked_add((panel * 8192 + slot + 1) as u64))
            .expect("menu accessibility identity exhausted"),
    )
}

fn menu_highlight_images() -> &'static [std::sync::Arc<gpui::RenderImage>; 3] {
    static IMAGES: std::sync::LazyLock<[std::sync::Arc<gpui::RenderImage>; 3]> =
        std::sync::LazyLock::new(|| {
            [
                include_bytes!("../../../assets/menu-inner-highlight-tl.png").as_slice(),
                include_bytes!("../../../assets/menu-inner-highlight-t.png").as_slice(),
                include_bytes!("../../../assets/menu-inner-highlight-tr.png").as_slice(),
            ]
            .map(|bytes| {
                let image = image::load_from_memory(bytes)
                    .expect("embedded menu highlight must decode")
                    .to_rgba8();
                std::sync::Arc::new(gpui::RenderImage::new([image::Frame::new(image)]))
            })
        });
    &IMAGES
}

fn check_slot(checked: bool, foreground: Hsla, window: &Window) -> gpui::Div {
    let side = zoom::pixels(window, 16.);
    div().size(side).flex_none().when(checked, |slot| {
        slot.child(
            gpui::svg()
                .debug_selector(|| "menu-checked-glyph".into())
                .path("menu-check")
                .size(side)
                .flex_none()
                .text_color(foreground),
        )
    })
}

impl Render for MenuView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(16. * self.scale));
        let edge = zoom::pixels(window, 0.5);
        let color = |role| Hsla::from(rgba(playbit_api::ui::Theme::light().color(role).0));
        use playbit_api::ui::ThemeColor as Role;
        let panels = self.panels.iter().enumerate().map(|(p, panel)| {
            let check_column = has_check_column(&panel.items);
            let rows = menu_rows(&panel.items).enumerate().map(|(index,(item,icons))| {
                let highlight = panel.selected == Some(index);
                let h = zoom::pixels(window, item_height(item));
                if matches!(item,MenuEntry::Separator) {
                    return div().h(h).flex_none().flex().items_center()
                        .child(div().h(zoom::pixels(window,1.)).w_full().bg(color(Role::Separator)))
                        .into_any_element();
                }
                let (label,checked,disabled,submenu) = match item {
                    MenuEntry::Action {label,checked,disabled,..} => (label.as_str(),*checked,*disabled,false),
                    MenuEntry::Submenu {label,disabled,..} => (label.as_str(),None,*disabled,true),
                    MenuEntry::Separator => unreachable!(),
                };
                let update = p == 0 && matches!(item, MenuEntry::Action { id, .. } if Some(*id) == self.update_action);
                let foreground = if disabled {color(Role::Text).alpha(0.3)} else if update || highlight {gpui::white()} else {color(Role::Text).alpha(0.8)};
                let entity = cx.entity().downgrade();
                let accessible_label = label.to_owned();
                div().id(("menu-item",p*512+index)).relative().h(h).w_full().flex_none()
                    .flex().items_center().gap(zoom::pixels(window,4.))
                    .pl(zoom::pixels(window,if check_column { 4. } else { 12. })).pr(zoom::pixels(window,8.))
                    .when(highlight && !update,|v| v.bg(gpui::black().alpha(0.7)))
                    .text_color(foreground)
                    .when(update, |v| v.child(div().absolute().inset_0().left(zoom::pixels(window,4.)).right(zoom::pixels(window,4.))
                        .rounded(zoom::pixels(window,2.)).bg(Hsla::from(gpui::rgb(controls::UPDATE_GREEN))
                            .blend(gpui::black().alpha(if highlight { 0.1 } else { 0. })))))
                    .when(check_column, |v| v.child(check_slot(checked == Some(true),foreground,window)))
                    .when(icons || update, |v| v.child(div().w(zoom::pixels(window,20.)).h(zoom::pixels(window,16.)).flex_none()
                        .when_some(if update { Some("upgrade") } else { item_icon(item).map(icon_path) }, |v, path| v.child(gpui::svg().path(path).size(zoom::pixels(window,16.)).text_color(foreground)))))
                    .child(div().min_w_0().flex_1().text_ellipsis().child(label.to_owned()))
                    .when(submenu,|v| v.child(gpui::svg().path("menu-submenu").text_color(foreground).w(zoom::pixels(window,8.)).h(zoom::pixels(window,16.)).flex_none()))
                    .child(gpui::canvas(move |bounds,window,cx| {
                        use gpui::accesskit::{Action,Node,Rect,Role,Toggled};
                        let Some(entity) = entity.upgrade() else {return};
                        entity.update(cx,|this,cx| {
                            let mut node = Node::new(if checked.is_some() {Role::MenuItemCheckBox} else {Role::MenuItem});
                            node.set_label(accessible_label.clone());
                            if let Some(checked) = checked { node.set_toggled(if checked {Toggled::True} else {Toggled::False}); }
                            node.set_bounds(Rect::new(bounds.left().to_f64(),bounds.top().to_f64(),bounds.right().to_f64(),bounds.bottom().to_f64()));
                            node.set_selected(this.panels[p].selected == Some(index));
                            if disabled {node.set_disabled()} else {
                                node.add_action(Action::Click);
                                node.add_action(Action::Focus);
                            }
                            let entity_id = cx.entity_id().as_u64();
                            if submenu {
                                let child_id = menu_accessibility_id(entity_id,p,index*2+2);
                                node.set_has_popup(gpui::accesskit::HasPopup::Menu);
                                let expanded = this.panels[p].selected == Some(index) && this.panels.len() > p+1;
                                node.set_expanded(expanded);
                                node.set_children(vec![child_id]);
                                if !expanded {
                                    let mut child = Node::new(Role::Menu);
                                    child.set_label(accessible_label.clone());
                                    window.insert_accessibility_node(child_id,child,false,None);
                                }
                            }
                            let id = menu_accessibility_id(entity_id,p,index*2+1);
                            let focused = this.running.get() && this.keyboard_panel == p && this.panels[p].selected == Some(index);
                            window.insert_accessibility_node(id,node,focused,Some(Rc::new(cx.listener(move |this,action: &gpui::accesskit::ActionRequest,window,cx| {
                                match action.action {
                                    Action::Click => this.activate_index(p,index,window,cx),
                                    Action::Focus if !disabled => {
                                        this.last_typeahead = None;
                                        this.keyboard_panel = p;
                                        this.select(p,Some(index),window,cx);
                                        if submenu { this.open_child(p,window,cx); }
                                        this.fit_window(window,cx);
                                    }
                                    _ => {}
                                }
                            }))));
                        });
                    },|_,_,_,_| {}).absolute().size_full())
                    .on_mouse_down(MouseButton::Left,cx.listener(move |this,_,window,cx| {
                        window.prevent_default();
                        this.armed = this.panels[p].enabled(index).then_some((p,index));
                        this.select(p,Some(index),window,cx);
                    }))
                    .on_mouse_up(MouseButton::Left,cx.listener(move |this,_,window,cx| {
                        if this.armed.take() == Some((p,index)) {this.activate_index(p,index,window,cx);}
                        cx.notify();
                    })).into_any_element()
            }).collect::<Vec<_>>();
            let entity_id = cx.entity_id().as_u64();
            let panel_id = if p == 0 { menu_accessibility_id(entity_id,0,0) } else {
                menu_accessibility_id(entity_id,p-1,self.panels[p-1].selected.expect("open child has selected parent")*2+2)
            };
            let child_ids = panel.items.iter().enumerate().filter(|(_,item)| !matches!(item,MenuEntry::Separator))
                .map(|(index,_)| menu_accessibility_id(entity_id,p,index*2+1)).collect::<Vec<_>>();
            let entity = cx.entity().downgrade();
            let origin = panel.bounds.origin - self.bounds.origin;
            div().id(("menu-panel",p)).absolute().left(origin.x).top(origin.y)
                .w(panel.bounds.size.width).h(panel.bounds.size.height)
                .rounded(zoom::pixels(window,4.)).bg(color(Role::MenuBackground))
                .shadow(vec![
                    BoxShadow {color:color(Role::Shadow).alpha(0.10),offset:point(px(0.),px(0.)),blur_radius:px(0.),spread_radius:edge},
                    BoxShadow {color:color(Role::Shadow).alpha(0.15),offset:point(px(0.),zoom::pixels(window,2.)),blur_radius:zoom::pixels(window,3.5),spread_radius:px(0.)},
                    BoxShadow {color:color(Role::Shadow).alpha(0.2),offset:point(px(0.),zoom::pixels(window,5.)),blur_radius:zoom::pixels(window,8.5),spread_radius:px(0.)},
                ])
                .child(gpui::canvas(move |bounds,window,cx| {
                    use gpui::accesskit::{Action,Node,Rect,Role};
                    let Some(entity) = entity.upgrade() else {return};
                    entity.update(cx,|this,cx| {
                        let mut node = Node::new(Role::Menu);
                        node.set_children(child_ids.clone());
                        node.set_bounds(Rect::new(bounds.left().to_f64(),bounds.top().to_f64(),bounds.right().to_f64(),bounds.bottom().to_f64()));
                        node.add_action(Action::Focus);
                        let focused = this.running.get() && this.keyboard_panel == p && this.panels[p].selected.is_none();
                        window.insert_accessibility_node(panel_id,node,focused,Some(Rc::new(cx.listener(move |this,action: &gpui::accesskit::ActionRequest,_,cx| {
                            if action.action == Action::Focus { this.keyboard_panel=p; cx.notify(); }
                        }))));
                    });
                },|_,_,_,_| {}).absolute().size_full())
                // Figma's 0.5dp stroke is outside: it must not inset rows.
                .child(div().absolute().top(-edge).bottom(-edge).left(-edge).right(-edge)
                    .rounded(zoom::pixels(window,4.)+edge).border(edge).border_color(color(Role::MenuBorder)))
                .child(div().absolute().top_0().left_0().size_full().opacity(0.2).child(gpui::canvas(
                    |_,_,_| (), |bounds,_,window,_| {
                        let corner=zoom::pixels(window,6.);
                        let images=menu_highlight_images();
                        for (x,w,image) in [(bounds.left(),corner,&images[0]),
                            (bounds.left()+corner,bounds.size.width-corner*2.,&images[1]),
                            (bounds.right()-corner,corner,&images[2])] {
                            if let Err(error)=window.paint_image(Bounds::new(point(x,bounds.top()),size(w,corner)),Default::default(),image.clone(),0,false) {
                                crate::diagnostic!("Cannot paint menu inner highlight: {error}");
                            }
                        }
                    }).size_full()))
                .child(div().id(("menu-scroll",p)).size_full().rounded(zoom::pixels(window,4.))
                    .overflow_y_scroll().track_scroll(&panel.scroll).flex().flex_col()
                    .py(zoom::pixels(window,PANEL_PAD)).children(rows))
        }).collect::<Vec<_>>();
        div()
            .id("guest-menu")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .font_family(super::UI_FONT_FAMILY)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(zoom::pixels(window, 11.))
            .line_height(zoom::pixels(window, 16.))
            .letter_spacing(px(0.))
            .children(panels)
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                this.pointer_moved(event.position, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.hit(event.position, window).is_none() {
                        this.running.set(false);
                        cx.defer(finish);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.armed = None;
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                // The native modal monitor sends keys here instead of to the
                // owner. Let the shared application zoom bindings handle them.
                if event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                    && matches!(event.keystroke.key.as_str(), "+" | "=" | "-" | "0")
                {
                    return;
                }
                if matches!(
                    event.keystroke.key.as_str(),
                    "escape" | "up" | "down" | "left" | "right" | "enter" | "space"
                ) {
                    this.last_typeahead = None;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.running.set(false);
                        cx.defer(finish);
                    }
                    "up" => this.move_keyboard(-1, window, cx),
                    "down" => this.move_keyboard(1, window, cx),
                    "left" => this.back(window, cx),
                    "right" => this.enter_child(window, cx),
                    "enter" | "space" => {
                        let p = this.keyboard_panel;
                        if let Some(i) = this.panels[p].selected {
                            this.activate_index(p, i, window, cx);
                        }
                    }
                    _ => {
                        if !event.keystroke.modifiers.control
                            && !event.keystroke.modifiers.platform
                            && !event.keystroke.modifiers.alt
                            && let Some(text) = event.keystroke.key_char.as_deref()
                        {
                            this.type_ahead(text, Instant::now(), window, cx);
                        }
                    }
                }
                cx.stop_propagation();
            }))
    }
}

fn in_submenu_corridor(
    parent: Bounds<Pixels>,
    child: Bounds<Pixels>,
    to: gpui::Point<Pixels>,
) -> bool {
    let right = child.left() > parent.left();
    let start = if right { parent.left() } else { parent.right() };
    let end = if right { child.left() } else { child.right() };
    let t = f32::from(to.x - start) / f32::from(end - start);
    if !(0. ..=1.).contains(&t) {
        return false;
    }
    let top = parent.top() + (child.top() - parent.top()) * t;
    let bottom = parent.bottom() + (child.bottom() - parent.bottom()) * t;
    to.y >= top && to.y <= bottom
}

fn shadow_bounds(bounds: Bounds<Pixels>, scale: f32, pixel_scale: f32) -> Bounds<Pixels> {
    let margin = px((20. * scale * pixel_scale).round() / pixel_scale);
    Bounds::new(bounds.origin - point(margin, margin), bounds.size + size(margin * 2., margin * 2.))
}

fn native_view(window: &Window) -> Option<*mut Object> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
    Some(handle.ns_view.as_ptr() as *mut Object)
}

fn native_window(window: &Window) -> Option<*mut Object> {
    let view = native_view(window)?;
    Some(unsafe { msg_send![view, window] })
}

fn show_menu(
    native: *mut Object,
    view: *mut Object,
    owner: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    running: Rc<Cell<bool>>,
    cx: &mut App,
) -> bool {
    let parent = owner
        .update(cx, |_, window, _| {
            let owner = native_window(window)?;
            unsafe {
                let screen: *mut Object = msg_send![owner, screen];
                (!screen.is_null()).then(|| (owner, msg_send![screen, frame]))
            }
        })
        .ok()
        .flatten();
    let Some((parent, screen)): Option<(*mut Object, NSRect)> = parent else { return false };
    let frame = NSRect::new(
        NSPoint::new(
            screen.origin.x + f64::from(bounds.origin.x),
            screen.origin.y + screen.size.height
                - f64::from(bounds.origin.y)
                - f64::from(bounds.size.height),
        ),
        cocoa::foundation::NSSize::new(f64::from(bounds.size.width), f64::from(bounds.size.height)),
    );
    // AppKit calls the GPUI resize callback synchronously. Run outside an App
    // borrow so that callback can update the viewport instead of dropping it.
    unsafe {
        let _: *mut Object = msg_send![native, retain];
        let _: *mut Object = msg_send![parent, retain];
    }
    let app = cx.to_async();
    cx.foreground_executor()
        .spawn(async move {
            unsafe {
                if running.get() && !present_native_menu(native, view, parent, frame) {
                    crate::diagnostic!("Cannot show popup: its rendering layer is unavailable");
                    let _ = app.update(finish);
                }
                let _: () = msg_send![parent, release];
                let _: () = msg_send![native, release];
            }
        })
        .detach();
    true
}

// Both windows are retained on AppKit's UI thread, outside any GPUI borrow.
unsafe fn present_native_menu(
    native: *mut Object,
    view: *mut Object,
    parent: *mut Object,
    frame: NSRect,
) -> bool {
    unsafe {
        // GPUI installs its layer-backed view inside AppKit's content view.
        // The container itself has no layer. Use the exact raw-window view.
        let layer: *mut Object = msg_send![view, layer];
        if layer.is_null() {
            return false;
        }
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        let _: () = msg_send![native, setFrame: frame display: false];
        // displayLayer draws and presents with the same CA transaction
        // as the frame change; never stretch the previous drawable.
        let _: () = msg_send![layer, setNeedsDisplay];
        let _: () = msg_send![layer, displayIfNeeded];
        let _: () = msg_send![class!(CATransaction), commit];
        let level: isize = msg_send![parent, level];
        let _: () = msg_send![native, setLevel: level];
        let _: () = msg_send![native, setHasShadow: false];
        // Follow this owner's Space and stacking. Adding the child
        // shows it without raising the background owner's group.
        let behavior = NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary;
        let _: () = msg_send![native, setCollectionBehavior: behavior.bits()];
        let attached: *mut Object = msg_send![native, parentWindow];
        if attached.is_null() {
            let _: () = msg_send![parent, addChildWindow: native ordered: 1isize];
        }
    }
    true
}

fn install_deactivation_observer(running: Rc<Cell<bool>>, cx: &App) -> *mut Object {
    use cocoa::foundation::NSString;
    let app = cx.to_async();
    let executor = cx.foreground_executor().clone();
    let handler = ConcreteBlock::new(move |_: *mut Object| {
        let app = app.clone();
        let running = running.clone();
        executor
            .spawn(async move {
                if running.get() {
                    let _ = app.update(finish);
                }
            })
            .detach();
    })
    .copy();
    unsafe {
        let name = NSString::alloc(std::ptr::null_mut())
            .init_str("NSApplicationDidResignActiveNotification");
        let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
        let observer = msg_send![center, addObserverForName: name object: std::ptr::null_mut::<Object>() queue: std::ptr::null_mut::<Object>() usingBlock: handler];
        let _: () = msg_send![name, release];
        observer
    }
}

#[derive(Clone, Copy)]
enum NativeGesture {
    Drag(gpui::Point<Pixels>),
    Release(gpui::Point<Pixels>),
}

unsafe fn menu_pointer(menu: *mut Object) -> gpui::Point<Pixels> {
    let location: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    let frame: NSRect = unsafe { msg_send![menu, frame] };
    point(
        px((location.x - frame.origin.x) as f32),
        px((frame.size.height - (location.y - frame.origin.y)) as f32),
    )
}

/// AppKit filters events before GPUI dispatch while its ordinary executor keeps
/// running. This lets the same single menu window redraw during modal input.
fn install_modal_monitor(
    menu: *mut Object,
    view: *mut Object,
    running: Rc<Cell<bool>>,
    cx: AsyncApp,
) -> *mut Object {
    unsafe {
        let handler = ConcreteBlock::new(move |event: *mut Object| -> *mut Object {
            let kind: usize = msg_send![event, type];
            let target: *mut Object = msg_send![event, window];
            if kind == 10 {
                let flags: usize = msg_send![event, modifierFlags];
                if flags & ((1 << 20) | (1 << 18)) != 0 {
                    return std::ptr::null_mut();
                }
            }
            if matches!(kind, 10..=12) {
                // Keep the document key. The non-key menu receives keyboard input
                // through this monitor; the owner must never also receive it.
                // Dispatch to the GPUI view itself: NSPanel's responder/key-
                // equivalent routing can consume navigation before our handler.
                match kind {
                    10 => {
                        let _: () = msg_send![view, keyDown: event];
                    }
                    11 => {
                        let _: () = msg_send![view, keyUp: event];
                    }
                    12 => {
                        let _: () = msg_send![view, flagsChanged: event];
                    }
                    _ => unreachable!(),
                }
                return std::ptr::null_mut();
            }
            if kind == 6 {
                let position = menu_pointer(menu);
                let _ = cx.update(|cx| opening_gesture(NativeGesture::Drag(position), cx));
            }
            if kind == 2 {
                let position = menu_pointer(menu);
                let _ = cx.update(|cx| opening_gesture(NativeGesture::Release(position), cx));
            }
            if target == menu || !(1..=12).contains(&kind) && !(17..=34).contains(&kind) {
                return event;
            }
            if matches!(kind, 1 | 3 | 25) && running.replace(false) {
                let _ = cx.update(|cx| cx.defer(finish));
            }
            std::ptr::null_mut() // Consume background input, including the dismissing down.
        })
        .copy();
        msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: usize::MAX handler: handler]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert expected window and menu operations.")]
mod tests {
    use super::*;
    fn action(id: u32, label: &str, disabled: bool) -> MenuEntry {
        MenuEntry::Action {
            icon: None,
            id,
            label: label.into(),
            checked: None,
            disabled,
            destructive: false,
        }
    }
    #[gpui::test]
    fn account_menu_is_flush_above_its_button_and_clamps_to_display(cx: &mut gpui::TestAppContext) {
        // The sidebar menu shares its left/bottom edge with the account button,
        // except where keeping the whole panel on screen requires clamping.
        let handle = menu(cx);
        handle
            .update(cx, |_, window, cx| {
                let mut request = Request::context(
                    window,
                    point(px(200.), px(400.)),
                    vec![action(6, "Update Playbit", false)],
                    Rc::new(|_, _| {}),
                    cx,
                )
                .above_button();
                request.anchor.size = size(px(24.), px(24.));
                request.display = Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(900.)));
                let bounds = place(&request, px(159.), px(184.));
                assert_eq!(bounds.left(), request.anchor.left());
                assert_eq!(bounds.bottom(), request.anchor.top());
                request.anchor.origin = point(px(1190.), px(20.));
                let bounds = place(&request, px(159.), px(184.));
                assert_eq!(bounds.right(), px(1196.));
                assert_eq!(bounds.top(), px(4.));
            })
            .unwrap();
    }

    #[gpui::test]
    fn emphasized_update_keeps_ordinary_selection_and_keyboard_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        // Visual emphasis must not select or activate the update. Keyboard
        // navigation and accessibility still expose the actual selected row.
        let handle = menu(cx);
        handle
            .update(cx, |view, window, cx| {
                view.panels[0].items =
                    vec![action(1, "Documentation", false), action(6, "Update Playbit", false)];
                view.update_action = Some(6);
                view.move_keyboard(1, window, cx);
                assert_eq!(view.panels[0].selected, Some(0));
                assert_eq!(view.result.get(), None);
            })
            .unwrap();
        AnyWindowHandle::from(handle)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                let (_, update) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Update Playbit"))
                    .unwrap();
                assert_ne!(update.is_selected(), Some(true));
                assert!(update.supports_action(gpui::accesskit::Action::Click));
            })
            .unwrap();
        handle
            .update(cx, |view, window, cx| {
                view.move_keyboard(1, window, cx);
                assert_eq!(view.panels[0].selected, Some(1));
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "enter");
        handle
            .update(cx, |view, _, _| {
                assert_eq!(view.result.get(), Some(6));
                assert!(!view.running.get());
            })
            .unwrap();
    }

    #[gpui::test]
    fn update_row_supports_pointer_activation_and_escape_dismissal(cx: &mut gpui::TestAppContext) {
        // The inset background must leave the shared row hit target intact;
        // dismissing the same emphasized menu must never produce an action.
        for cancel in [true, false] {
            let handle = menu(cx);
            handle
                .update(cx, |view, _, cx| {
                    view.panels[0].items = vec![action(6, "Update Playbit", false)];
                    view.update_action = Some(6);
                    cx.notify();
                })
                .unwrap();
            let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
            if cancel {
                visual.simulate_keystrokes("escape");
            } else {
                let position = AnyWindowHandle::from(handle)
                    .update(&mut visual, |_, window, cx| {
                        let _ = window.draw(cx);
                        let bounds = window
                            .accessibility_nodes_for_test()
                            .find(|(_, node)| node.label() == Some("Update Playbit"))
                            .unwrap()
                            .1
                            .bounds()
                            .unwrap();
                        point(px((bounds.x0 + 12.) as f32), px((bounds.y0 + 12.) as f32))
                    })
                    .unwrap();
                visual.simulate_click(position, Default::default());
            }
            handle
                .update(&mut visual, |view, _, _| {
                    assert_eq!(view.result.get(), if cancel { None } else { Some(6) });
                    assert!(!view.running.get());
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn toolbar_menu_opens_below_button_with_native_rows(cx: &mut gpui::TestAppContext) {
        let handle = menu(cx);
        cx.update(|cx| {
            handle
                .update(cx, |_, window, cx| {
                    let mut request = Request::context(
                        window,
                        point(px(200.), px(200.)),
                        vec![action(1, "Trace", false)],
                        Rc::new(|_, _| {}),
                        cx,
                    )
                    .below_button();
                    request.anchor.size = size(px(24.), px(24.));
                    request.display = Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(900.)));
                    assert_eq!(request.scale, 1.);
                    assert_eq!(item_height(&request.items[0]), 24.);
                    let bounds = place(&request, px(159.), px(56.));
                    assert_eq!(
                        bounds.origin,
                        point(request.anchor.left(), request.anchor.bottom())
                    );

                    if let MenuEntry::Action { icon, .. } = &mut request.items[0] {
                        *icon = Some(MenuGlyph::Asset("entry-object-db"));
                    }
                    let mut request = request.align_icon_to_button();
                    // Match the actual snapped row padding, check column and 16dp glyph,
                    // including fractional zoom and a display with physical pixel snapping.
                    for (scale, display) in [(1., 1.), (0.75, 2.), (1.25, 1.), (2., 2.)] {
                        request.scale = scale;
                        request.pixel_scale = display;
                        let sp = |value: f32| px((value * scale * display).round() / display);
                        request.anchor.origin = point(px(200.), px(200.));
                        request.anchor.size = size(sp(24.), sp(24.));
                        for checked in [None, Some(false)] {
                            if let MenuEntry::Action { checked: check, .. } = &mut request.items[0]
                            {
                                *check = checked;
                            }
                            let bounds = place(&request, sp(159.), sp(56.));
                            let icon_left = bounds.left()
                                + if checked.is_some() {
                                    sp(4.) + sp(16.) + sp(4.)
                                } else {
                                    sp(12.)
                                };
                            assert_eq!(icon_left + sp(16.) / 2., request.anchor.center().x);
                            assert_eq!(bounds.top(), request.anchor.bottom());
                        }
                        request.anchor.origin.x = px(0.);
                        let bounds = place(&request, sp(159.), sp(56.));
                        assert_eq!(bounds.left(), sp(4.), "screen fitting takes precedence");
                    }
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn fractional_menu_rows_match_paint_hit_testing_and_anchors(cx: &mut gpui::TestAppContext) {
        // Summing snapped rows prevents accumulated errors in long fractional-scale menus.
        cx.update(zoom::install);
        let handle = menu(cx);
        for units in zoom::SAMPLES {
            cx.update(|cx| zoom::set_scale(units, cx).unwrap());
            for display in [1., 2.] {
                cx.simulate_window_scale_factor(handle.into(), display);
                handle
                    .update(cx, |view, window, cx| {
                        zoom::apply(window, cx);
                        let request = Request::context(
                            window,
                            point(px(200.), px(200.)),
                            (0..64)
                                .map(|id| action(id, "Menu item with a long label", id == 3))
                                .collect(),
                            Rc::new(|_, _| {}),
                            cx,
                        );
                        for request in [
                            request.clone(),
                            request.clone().above_button(),
                            request.below_button(),
                        ] {
                            assert_eq!(request.scale, units as f32 / 10_000.);
                            let width = request_width(&request, window);
                            let height = menu_height(&request.items, request.scale, display);
                            let bounds = place(&request, width, height.min(px(600.)));
                            *view = MenuView::new(
                                &request,
                                bounds,
                                px(0.),
                                Rc::new(Cell::new(true)),
                                Rc::new(Cell::new(None)),
                                cx,
                            );
                            let mut expected = bounds.top() + zoom::pixels(window, PANEL_PAD);
                            for index in 0..64 {
                                assert_eq!(view.panels[0].row_top(index, window), expected);
                                let position = point(bounds.left() + px(1.), expected + px(1.))
                                    - view.bounds.origin;
                                if bounds.contains(&(position + view.bounds.origin)) {
                                    assert_eq!(view.hit(position, window), Some((0, index)));
                                }
                                expected += zoom::pixels(window, ITEM_HEIGHT);
                            }
                            assert_eq!(
                                expected + zoom::pixels(window, PANEL_PAD) - bounds.top(),
                                height
                            );
                        }
                        window.refresh();
                    })
                    .unwrap();
            }
        }
    }

    fn menu(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<MenuView> {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        cx.update(|cx| cx.set_global(Coordinator::default()));
        cx.add_window(|window, cx| {
            let mut request = Request::context(
                window,
                point(px(200.), px(200.)),
                vec![
                    action(1, "First", false),
                    MenuEntry::Submenu {
                        icon: None,
                        label: "More".into(),
                        disabled: false,
                        items: vec![action(2, "Child", false), action(3, "Disabled", true)],
                    },
                    action(4, "Last", false),
                ],
                Rc::new(|_, _| {}),
                cx,
            );
            request.display = Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(900.)));
            let height = request.items.iter().map(item_height).sum::<f32>() + 2. * PANEL_PAD;
            let bounds = place(&request, px(140.), px(height));
            let view = MenuView::new(
                &request,
                bounds,
                px(0.),
                Rc::new(Cell::new(true)),
                Rc::new(Cell::new(None)),
                cx,
            );
            window.focus(&view.focus);
            view
        })
    }
    #[gpui::test]
    fn typeahead_selects_prefix_and_resets_at_200ms_without_activating(
        cx: &mut gpui::TestAppContext,
    ) {
        let menu = menu(cx);
        menu.update(cx, |view, window, cx| {
            view.panels[0].items = vec![
                action(1, "Anne", false),
                action(2, "Beatrice", false),
                action(3, "Bobby unavailable", true),
                action(4, "Bob", false),
                action(5, "Oscar", false),
            ];
            let now = Instant::now();
            view.type_ahead("b", now, window, cx);
            assert_eq!(view.panels[0].selected, Some(1));
            view.type_ahead("O", now + std::time::Duration::from_millis(199), window, cx);
            assert_eq!(view.panels[0].selected, Some(3));
            view.type_ahead("o", now + std::time::Duration::from_millis(399), window, cx);
            assert_eq!(view.panels[0].selected, Some(4));
            view.type_ahead("z", now + std::time::Duration::from_millis(400), window, cx);
            assert_eq!(view.panels[0].selected, Some(4), "unmatched prefix leaves selection alone");
            assert_eq!(view.result.get(), None);
            assert!(view.running.get());
            view.typeahead = "x".repeat(256);
            view.type_ahead("x", now + std::time::Duration::from_millis(401), window, cx);
            assert_eq!(view.typeahead.len(), 256);
        })
        .unwrap();
    }

    #[gpui::test]
    fn accessibility_submenu_is_a_child_menu_with_focusable_items(cx: &mut gpui::TestAppContext) {
        use gpui::accesskit::{Action, Role};
        let handle = menu(cx);
        let window: AnyWindowHandle = handle.into();
        let inspect = |window: &mut Window, cx: &mut App| {
            let _ = window.draw(cx);
            assert!(window.accessibility_error().is_none());
            window
                .accessibility_nodes_for_test()
                .map(|(id, node)| (id, node.clone()))
                .collect::<Vec<_>>()
        };
        let nodes = window.update(cx, |_, window, cx| inspect(window, cx)).unwrap();
        let (more_id, more) = nodes
            .iter()
            .find(|(_, n)| n.label() == Some("More") && n.role() == Role::MenuItem)
            .unwrap();
        assert!(more.supports_action(Action::Focus));
        let child_id = more.children()[0];
        assert!(nodes.iter().any(|(id, n)| *id == child_id && n.role() == Role::Menu));
        assert!(
            nodes.iter().any(|(_, n)| n.role() == Role::Menu && n.children().contains(more_id))
        );
        handle
            .update(cx, |view, window, cx| {
                view.select(0, Some(1), window, cx);
                view.enter_child(window, cx);
                assert_eq!(view.keyboard_panel, 1);
            })
            .unwrap();
        let nodes = window.update(cx, |_, window, cx| inspect(window, cx)).unwrap();
        let (_, child_menu) = nodes.iter().find(|(id, _)| *id == child_id).unwrap();
        let (child_id, child) = nodes.iter().find(|(_, n)| n.label() == Some("Child")).unwrap();
        assert!(child_menu.children().contains(child_id));
        assert!(child.supports_action(Action::Focus));
        assert_eq!(child.is_selected(), Some(true));
        handle
            .update(cx, |view, window, cx| {
                view.back(window, cx);
                assert_eq!(view.keyboard_panel, 0);
                assert_eq!(view.panels[0].selected, Some(1));
                assert_eq!(view.result.get(), None);
            })
            .unwrap();
    }

    #[gpui::test]
    fn popup_width_fits_longest_item_even_when_short_item_is_selected(
        cx: &mut gpui::TestAppContext,
    ) {
        let handle = menu(cx);
        handle
            .update(cx, |_, window, cx| {
                let items = vec![
                    action(1, "Asia", false),
                    action(2, "North America", false),
                    action(3, "South America", false),
                ];
                let mut request = Request::context(
                    window,
                    point(px(200.), px(200.)),
                    items,
                    Rc::new(|_, _| {}),
                    cx,
                );
                request.label = Some("Asia".into());
                request.anchor.size = size(px(86.), px(24.));
                let content = menu_width(&request.items, 1., window.scale_factor(), window);
                assert!(content > px(86.));
                assert_eq!(request_width(&request, window), content);
                request.anchor.size.width = px(250.);
                assert_eq!(request_width(&request, window), px(250.));
                for (checked, icon, label_offset) in [
                    (None, None, 12.),
                    (Some(false), None, 24.),
                    (None, Some(playbit_api::ui::MenuIcon::Copy.into()), 36.),
                    (Some(true), Some(playbit_api::ui::MenuIcon::Copy.into()), 48.),
                    (None, Some(MenuGlyph::Asset("menu-rename")), 36.),
                ] {
                    if let MenuEntry::Action { checked: check, icon: symbol, .. } =
                        &mut request.items[0]
                    {
                        *check = checked;
                        *symbol = icon;
                    }
                    let bounds = place(&request, px(250.), px(80.));
                    assert_eq!(bounds.left() + px(label_offset), request.anchor.left() + px(12.));
                }
            })
            .unwrap();
    }

    #[gpui::test]
    fn context_pointer_is_one_point_inside_first_row(cx: &mut gpui::TestAppContext) {
        let window = menu(cx);
        window
            .update(cx, |view, window, _| {
                let panel = &view.panels[0];
                assert_eq!(panel.bounds.left(), px(201.));
                assert_eq!(panel.row_top(0, window), px(199.));
                assert_eq!(panel.selected, None);
            })
            .unwrap();
    }
    #[gpui::test]
    fn hover_replaces_selection_and_opens_adjacent_submenu(cx: &mut gpui::TestAppContext) {
        let window = menu(cx);
        window
            .update(cx, |view, window, cx| {
                view.select(0, Some(0), window, cx);
                view.select(0, Some(1), window, cx);
                assert_eq!(view.panels.len(), 1, "hover selection waits for native submenu delay");
                view.open_child(0, window, cx);
                assert_eq!(view.panels.len(), 2);
                assert_eq!(view.panels[0].selected, Some(1));
                assert!(view.panels[1].bounds.left() > view.panels[0].bounds.right());
                assert_eq!(view.panels[1].row_top(0, window), view.panels[0].row_top(1, window));
                view.select(1, Some(0), window, cx);
                assert_eq!(view.panels[0].selected, Some(1));
                assert_eq!(view.panels[1].selected, Some(0));
                view.select(1, Some(1), window, cx);
                assert_eq!(view.panels[1].selected, None, "disabled rows never highlight");
                view.select(0, Some(2), window, cx);
                assert_eq!(view.panels.len(), 1);
                assert_eq!(view.panels[0].selected, Some(2));
                assert_eq!(view.result.get(), None, "hover never activates an action");
            })
            .unwrap();
    }
    #[gpui::test]
    fn dismiss_cancels_pending_submenu_and_pointer_tracking(cx: &mut gpui::TestAppContext) {
        let window = menu(cx);
        window
            .update(cx, |view, window, cx| {
                view.select(0, Some(1), window, cx);
                assert!(view.submenu_task.is_some());
                view.armed = Some((0, 1));
                view.cancel_tracking();
                assert!(view.submenu_task.is_none());
                assert!(view.aim_task.is_none());
                assert!(view.armed.is_none());
                assert!(!view.running.get());
            })
            .unwrap();
    }
    #[gpui::test]
    async fn submenu_motion_restarts_idle_deadline_even_between_panels(
        cx: &mut gpui::TestAppContext,
    ) {
        let handle = menu(cx);
        handle
            .update(cx, |view, window, cx| {
                view.select(0, Some(1), window, cx);
                view.open_child(0, window, cx);
                let parent = &view.panels[0];
                let start =
                    point(parent.bounds.left() + px(80.), parent.row_top(1, window) + px(10.));
                view.pointer = Some(start);
                let next = point(
                    view.panels[1].bounds.left() - px(0.5),
                    view.panels[1].bounds.bottom() - px(5.),
                );
                view.pointer_moved(next - view.bounds.origin, window, cx);
                assert!(view.aim_task.is_some(), "gap movement must retain child");
                assert_eq!(view.panels.len(), 2);
            })
            .unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_millis(75));
        cx.run_until_parked();
        handle
            .update(cx, |view, window, cx| {
                assert_eq!(view.panels.len(), 2);
                let position = view.pointer.unwrap() + point(px(0.1), px(0.));
                view.pointer_moved(position - view.bounds.origin, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_millis(75));
        cx.run_until_parked();
        handle.update(cx, |view, _, _| assert_eq!(view.panels.len(), 2)).unwrap();
        cx.executor().advance_clock(std::time::Duration::from_millis(30));
        cx.run_until_parked();
        handle.update(cx, |view, _, _| assert_eq!(view.panels.len(), 1)).unwrap();
    }

    #[gpui::test]
    fn keyboard_opens_submenu_and_left_restores_parent(cx: &mut gpui::TestAppContext) {
        let window = menu(cx);
        window
            .update(cx, |view, window, cx| {
                view.move_keyboard(1, window, cx);
                assert_eq!(view.panels[0].selected, Some(0));
                view.move_keyboard(1, window, cx);
                assert_eq!(view.panels.len(), 1);
                view.enter_child(window, cx);
                assert_eq!(view.keyboard_panel, 1);
                assert_eq!(view.panels[1].selected, Some(0));
                view.back(window, cx);
                assert_eq!(view.keyboard_panel, 0);
                assert_eq!(view.panels.len(), 1);
                assert_eq!(view.panels[0].selected, Some(1));
            })
            .unwrap();
    }
    #[gpui::test]
    fn keyboard_enters_nearest_enabled_row_in_shifted_submenu(cx: &mut gpui::TestAppContext) {
        let handle = menu(cx);
        handle
            .update(cx, |view, window, cx| {
                view.panels[0].items[1] = MenuEntry::Submenu {
                    icon: None,
                    label: "More".into(),
                    disabled: false,
                    items: vec![
                        action(2, "Top", false),
                        action(3, "Middle", false),
                        action(4, "Disabled", true),
                        action(5, "Bottom", false),
                    ],
                };
                view.select(0, Some(1), window, cx);
                view.open_child(0, window, cx);
                // Screen-edge fitting moves the submenu upward by two rows.
                view.panels[1].bounds.origin.y -= zoom::pixels(window, 2. * ITEM_HEIGHT);
                view.enter_child(window, cx);
                // The aligned row is disabled; ties prefer the previous enabled row.
                assert_eq!(view.panels[1].selected, Some(1));
                view.back(window, cx);
                assert_eq!(view.keyboard_panel, 0);
                assert_eq!(view.panels[0].selected, Some(1));
            })
            .unwrap();
    }

    #[gpui::test]
    fn menu_pointer_release_requires_the_same_enabled_row(cx: &mut gpui::TestAppContext) {
        let window = menu(cx);
        window
            .update(cx, |view, window, cx| {
                let at = |v: &MenuView, i| {
                    point(
                        v.panels[0].bounds.left() - v.bounds.left() + px(30.),
                        v.panels[0].row_top(i, window) - v.bounds.top() + px(10.),
                    )
                };
                let first = at(view, 0);
                let last = at(view, 2);
                view.release_from_button(first, window, cx);
                assert_eq!(view.result.get(), None, "unarmed release cannot activate");
                view.drag_from_button(first, window, cx);
                view.release_from_button(last, window, cx);
                assert_eq!(view.result.get(), None, "release on another item cancels");
                view.drag_from_button(first, window, cx);
                view.release_from_button(first, window, cx);
                assert_eq!(view.result.get(), Some(1));
            })
            .unwrap();
    }

    #[gpui::test]
    fn held_opening_release_accepts_current_item_without_movement(cx: &mut gpui::TestAppContext) {
        let menu = menu(cx);
        menu.update(cx, |view, window, cx| {
            view.panels[0].selected = Some(0);
            assert!(view.armed.is_none());
            let position = point(
                view.panels[0].bounds.left() - view.bounds.left() + px(30.),
                view.panels[0].row_top(0, window) - view.bounds.top() + px(10.),
            );
            view.release_held_opening_press(position, window, cx);
            assert_eq!(view.result.get(), Some(1));
            assert!(!view.running.get(), "stationary long release must close the menu");
        })
        .unwrap();
    }

    #[test]
    fn submenu_corridor_covers_gap_and_flipped_child() {
        let parent = Bounds::new(point(px(100.), px(80.)), size(px(100.), px(20.)));
        let child = Bounds::new(point(px(201.), px(76.)), size(px(140.), px(100.)));
        assert!(in_submenu_corridor(parent, child, point(px(190.), px(140.))));
        assert!(in_submenu_corridor(parent, child, point(px(200.5), px(120.))));
        assert!(!in_submenu_corridor(parent, child, point(px(190.), px(180.))));
        let left = Bounds::new(point(px(-41.), px(76.)), size(px(140.), px(100.)));
        assert!(in_submenu_corridor(parent, left, point(px(110.), px(140.))));
    }
    #[gpui::test]
    fn section_icons_and_menu_checks_share_measured_columns(cx: &mut gpui::TestAppContext) {
        // One icon reserves slots only in its section; an unchecked action reserves
        // checks in both sections. Width and hit testing use the same 24dp rows.
        let handle = menu(cx);
        handle
            .update(cx, |view, window, cx| {
                let mut items = vec![
                    action(1, "Same", false),
                    action(2, "Same", false),
                    MenuEntry::Separator,
                    action(3, "Same", false),
                ];
                let plain_width = menu_width(&items, 1., 2., window);
                if let MenuEntry::Action { icon, .. } = &mut items[1] {
                    *icon = Some(playbit_api::ui::MenuIcon::Copy.into());
                }
                assert_eq!(
                    menu_rows(&items).map(|(_, icons)| icons).collect::<Vec<_>>(),
                    [true, true, true, false]
                );
                assert!(!has_check_column(&items));
                assert_eq!(menu_width(&items, 1., 2., window), plain_width + px(24.));
                if let MenuEntry::Action { checked, .. } = &mut items[3] {
                    *checked = Some(false);
                }
                assert!(has_check_column(&items));
                assert_eq!(menu_width(&items, 1., 2., window), plain_width + px(36.));
                view.panels[0].items = items;
                assert_eq!(
                    view.panels[0].row_top(1, window) - view.panels[0].row_top(0, window),
                    px(24.)
                );
                assert_eq!(
                    view.panels[0].row_top(3, window) - view.panels[0].row_top(1, window),
                    px(32.)
                );
                let at = point(
                    view.panels[0].bounds.left() - view.bounds.left() + px(30.),
                    view.panels[0].row_top(0, window) - view.bounds.top() + px(23.),
                );
                view.drag_from_button(at, window, cx);
                view.release_from_button(at, window, cx);
                assert_eq!(view.result.get(), Some(1), "the bottom of a 24dp row is clickable");
            })
            .unwrap();
    }

    #[test]
    fn selected_menu_row_tracks_checked_value_or_label() {
        let items = [
            MenuEntry::Action {
                icon: None,
                id: 1,
                label: "A".into(),
                checked: None,
                disabled: false,
                destructive: false,
            },
            MenuEntry::Separator,
            MenuEntry::Action {
                icon: None,
                id: 2,
                label: "B".into(),
                checked: Some(true),
                disabled: false,
                destructive: false,
            },
        ];
        assert_eq!(selected_index(&items, Some("A")), 0);
        assert_eq!(selected_index(&items, Some("B")), 2);
        assert_eq!(selected_index(&items, None), 2);
    }

    #[gpui::test]
    fn checked_item_keeps_its_mark_and_accessibility_value_when_focus_moves(
        cx: &mut gpui::TestAppContext,
    ) {
        // The stored value is independent of the hovered row, with a full-sized mark at both app zooms.
        use gpui::accesskit::{Role, Toggled};
        let handle = menu(cx);
        cx.simulate_window_scale_factor(handle.into(), 2.);
        for scale in [1., 2.] {
            handle
                .update(cx, |view, window, cx| {
                    let mut items = vec![
                        action(1, "Current value", false),
                        action(2, "Focused alternative", false),
                        action(3, "Ordinary action", false),
                    ];
                    for (index, checked) in [(0, true), (1, false)] {
                        let MenuEntry::Action { checked: value, .. } = &mut items[index] else {
                            panic!("action fixture")
                        };
                        *value = Some(checked);
                    }
                    view.scale = scale;
                    let height = menu_height(&items, scale, window.scale_factor());
                    view.bounds.size.height = height;
                    view.panels[0].bounds.size.height = height;
                    view.panels[0].items = items;
                    view.panels[0].selected = Some(1);
                    cx.notify();
                })
                .unwrap();
            let window: AnyWindowHandle = handle.into();
            window
                .update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                    assert!(window.accessibility_error().is_none());
                    let nodes = window
                        .accessibility_nodes_for_test()
                        .map(|(_, node)| node.clone())
                        .collect::<Vec<_>>();
                    let current =
                        nodes.iter().find(|node| node.label() == Some("Current value")).unwrap();
                    assert_eq!(current.role(), Role::MenuItemCheckBox);
                    assert_eq!(current.toggled(), Some(Toggled::True));
                    assert_eq!(current.is_selected(), Some(false));
                    let other = nodes
                        .iter()
                        .find(|node| node.label() == Some("Focused alternative"))
                        .unwrap();
                    assert_eq!(other.role(), Role::MenuItemCheckBox);
                    assert_eq!(other.toggled(), Some(Toggled::False));
                    assert_eq!(other.is_selected(), Some(true));
                    let ordinary =
                        nodes.iter().find(|node| node.label() == Some("Ordinary action")).unwrap();
                    assert_eq!(ordinary.role(), Role::MenuItem);
                    assert_eq!(ordinary.toggled(), None);
                })
                .unwrap();
            let mut visual = gpui::VisualTestContext::from_window(window, cx);
            let mark = visual.debug_bounds("menu-checked-glyph").expect("checked SVG has geometry");
            assert_eq!(mark.size, size(px(16. * scale), px(16. * scale)));
        }
    }
}
