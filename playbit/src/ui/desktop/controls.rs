//! GPUI rendering of the shared checkbox and push-button semantics.
use crate::runtime::MenuEntry;
#[cfg(target_os = "macos")]
use crate::ui::desktop::popup;
use crate::ui::desktop::zoom;
#[cfg(test)]
use gpui::point;
use gpui::{
    AssetSource, Bounds, Context, FocusHandle, MouseButton, Pixels, Refineable, SharedString,
    Subscription, Window, div, prelude::*, px, rgb, rgba, svg,
};
use playbit_api::ui::{ButtonStyle, CheckState, Theme, ThemeColor as Role};
use std::{borrow::Cow, rc::Rc};
mod navigation;
mod rename;
mod render;
mod session_placeholder;

pub const MAX_CONTROLS: usize = 256;
pub const MAX_LABEL_BYTES: usize = 1024 * 1024;
pub(super) const UPDATE_GREEN: u32 = 0x15bd55;
pub type Activate = Rc<dyn Fn(u32, &mut gpui::App)>;
pub type ContextMenu = Rc<dyn Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App)>;

fn label_style(window: &Window) -> gpui::TextStyleRefinement {
    gpui::TextStyleRefinement {
        font_family: Some(crate::ui::desktop::UI_FONT_FAMILY.into()),
        font_weight: Some(gpui::FontWeight::MEDIUM),
        font_size: Some(zoom::pixels(window, 11.).into()),
        letter_spacing: Some(zoom::scaled(window, crate::ui::desktop::UI_CONTROL_TRACKING)),
        line_height: Some(zoom::pixels(window, 16.).into()),
        ..Default::default()
    }
}

pub fn element(
    control: &gpui::Entity<Control>,
    window: &mut Window,
    cx: &mut gpui::App,
) -> gpui::AnyElement {
    let state = control.read(cx);
    if !matches!(state.kind, Kind::Checkbox(_)) {
        return control.clone().into_any_element();
    }
    let mut text_style = window.text_style();
    text_style.refine(&label_style(window));
    // Use the existing shaping cache, without retaining another width cache.
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let lines = match window.text_system().shape_text(
        state.label.clone(),
        font_size,
        &[text_style.to_run(state.label.len())],
        None,
        text_style.line_clamp,
    ) {
        Ok(lines) => lines,
        Err(error) => {
            crate::diagnostic!("Checkbox intrinsic layout failed: {error}");
            return control.clone().into_any_element();
        }
    };
    let width =
        lines.iter().fold(px(0.), |width, line| width.max(line.unwrapped_layout.width).ceil())
            + zoom::pixels(window, 24.);
    let mut layout =
        div().w(width).min_w(zoom::pixels(window, 24.)).max_w_full().h(zoom::pixels(window, 24.));
    layout.style().align_self = Some(gpui::AlignSelf::FlexStart);
    gpui::AnyView::from(control.clone()).cached(layout.style().clone()).into_any_element()
}

pub struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "sign-in-back" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/sign-in-back.svg")))
            }
            "sign-in-warning" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/sign-in-warning.svg")))
            }
            "progress-spinner" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/progress-spinner.svg")))
            }
            "ap-live" => Some(Cow::Borrowed(include_bytes!("../../../assets/ap-live.svg"))),
            "ap-connecting" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/ap-connecting.svg")))
            }
            "ap-offline" => Some(Cow::Borrowed(include_bytes!("../../../assets/ap-offline.svg"))),
            "status-sync" => Some(Cow::Borrowed(include_bytes!("../../../assets/status-sync.svg"))),
            "status-plug" => Some(Cow::Borrowed(include_bytes!("../../../assets/status-plug.svg"))),
            "status-connection" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/status-connection.svg")))
            }
            "sidebar-volume" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/sidebar-volume.svg")))
            }
            "sidebar-session" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/sidebar-session.svg")))
            }
            "app-browser-apps" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-browser-apps.svg")))
            }
            "app-browser-session" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-browser-session.svg")))
            }
            "app-browser-instance" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-browser-instance.svg")))
            }
            "app-browser-local" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-browser-local.svg")))
            }
            "app-browser-stop" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-browser-stop.svg")))
            }
            "lock" => Some(Cow::Borrowed(include_bytes!("../../../assets/lock.svg"))),
            "app-placeholder" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/app-placeholder.svg")))
            }
            "app-default" => Some(Cow::Borrowed(include_bytes!("../../../assets/app-default.svg"))),
            "upgrade" => Some(Cow::Borrowed(include_bytes!("../../../assets/upgrade.svg"))),
            "session-overflow" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/session-overflow.svg")))
            }
            "session-clear" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/session-clear.svg")))
            }
            "session-start" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/session-start.svg")))
            }
            "session-share" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/session-share.svg")))
            }
            "add" => Some(Cow::Borrowed(include_bytes!("../../../assets/add.svg"))),
            "session-icon" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/session-icon.svg")))
            }
            "checkmark" => Some(Cow::Borrowed(include_bytes!("../../../assets/checkmark.svg"))),
            "menu-avatar" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-avatar.svg"))),
            "menu-signature" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/menu-signature.svg")))
            }
            "menu-copy" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-copy.svg"))),
            "menu-exit-right" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/menu-exit-right.svg")))
            }
            "menu-open-book" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/menu-open-book.svg")))
            }
            "menu-talk" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-talk.svg"))),
            "menu-check" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-check.svg"))),
            "menu-submenu" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/menu-submenu.svg")))
            }
            "popup-glyph" => Some(Cow::Borrowed(include_bytes!("../../../assets/popup-glyph.svg"))),
            "permission-person" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-person.svg")))
            }
            "permission-person-popup" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-person-popup.svg")))
            }
            "permission-service" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-service.svg")))
            }
            "permission-remove" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-remove.svg")))
            }
            "permission-add" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-add.svg")))
            }
            "permission-popup-glyph" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/permission-popup-glyph.svg")))
            }
            "volume" => Some(Cow::Borrowed(include_bytes!("../../../assets/volume.png"))),
            "volume-cell" => Some(Cow::Borrowed(include_bytes!("../../../assets/volume-cell.svg"))),
            "menu-rename" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-rename.svg"))),
            "menu-delete" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-delete.svg"))),
            "menu-finder" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-finder.svg"))),
            "menu-open" => Some(Cow::Borrowed(include_bytes!("../../../assets/menu-open.svg"))),
            "entry-child" => Some(Cow::Borrowed(include_bytes!("../../../assets/entry-child.svg"))),
            "draft-changes" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/draft-changes.svg")))
            }
            "menu-duplicate" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/menu-duplicate.svg")))
            }
            "volume-owner" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/volume-owner.svg")))
            }
            "entry-object-db" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/entry-object-db.svg")))
            }

            "entry-kv-db" => Some(Cow::Borrowed(include_bytes!("../../../assets/entry-kv-db.svg"))),

            "entry-append-log" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/entry-append-log.svg")))
            }

            "entry-type" => Some(Cow::Borrowed(include_bytes!("../../../assets/entry-type.svg"))),
            "entry-directory" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/entry-directory.svg")))
            }
            "entry-file" => Some(Cow::Borrowed(include_bytes!("../../../assets/entry-file.svg"))),

            "table-sort-ascending" => {
                Some(Cow::Borrowed(include_bytes!("../../../assets/table-sort-ascending.svg")))
            }
            "check-on" => Some(Cow::Borrowed(include_bytes!("../../../assets/check-on.svg"))),
            "check-mixed" => Some(Cow::Borrowed(include_bytes!("../../../assets/check-mixed.svg"))),
            _ => None,
        })
    }
    fn list(&self, _: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(vec![
            "sign-in-back".into(),
            "progress-spinner".into(),
            "sign-in-warning".into(),
            "status-sync".into(),
            "status-plug".into(),
            "status-connection".into(),
            "sidebar-volume".into(),
            "volume-cell".into(),
            "volume-owner".into(),
            "sidebar-session".into(),
            "app-browser-apps".into(),
            "app-browser-session".into(),
            "app-browser-instance".into(),
            "app-browser-local".into(),
            "app-browser-stop".into(),
            "lock".into(),
            "app-placeholder".into(),
            "app-default".into(),
            "upgrade".into(),
            "session-overflow".into(),
            "session-clear".into(),
            "session-start".into(),
            "session-share".into(),
            "add".into(),
            "check-on".into(),
            "check-mixed".into(),
            "session-icon".into(),
            "popup-glyph".into(),
            "permission-person".into(),
            "permission-person-popup".into(),
            "permission-service".into(),
            "permission-remove".into(),
            "permission-add".into(),
            "permission-popup-glyph".into(),
            "menu-avatar".into(),
            "menu-signature".into(),
            "menu-copy".into(),
            "menu-rename".into(),
            "menu-delete".into(),
            "menu-finder".into(),
            "menu-open".into(),
            "entry-child".into(),
            "draft-changes".into(),
            "menu-duplicate".into(),
            "menu-exit-right".into(),
            "menu-open-book".into(),
            "menu-talk".into(),
            "menu-check".into(),
            "menu-submenu".into(),
            "volume".into(),
            "entry-object-db".into(),
            "entry-kv-db".into(),
            "entry-append-log".into(),
            "entry-type".into(),
            "entry-directory".into(),
            "entry-file".into(),
            "table-sort-ascending".into(),
        ])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Checkbox(CheckState),
    Button(ButtonStyle),
    Popup,
}

/// Presentation for the session header and app launcher; input stays shared.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionStyle {
    Tab(bool),
    Template,
    Recent,
    Share,
    Stop,
}

#[derive(Clone, Copy)]
pub struct Properties<'a> {
    pub kind: Kind,
    pub label: &'a str,
    pub disabled: bool,
    pub theme: &'a Theme,
}

#[derive(Default)]
struct Press {
    pointer: bool,
    hovered: bool,
    key: Option<gpui::KeyboardButton>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FocusIntent {
    #[default]
    None,
    PendingSelection {
        rename: bool,
    },
    WithoutRing,
}
impl Press {
    fn active(&self) -> bool {
        self.key.is_some() || self.pointer && self.hovered
    }
    fn cancel(&mut self) {
        *self = Self::default();
    }
    fn release_key(&mut self, key: Option<gpui::KeyboardButton>) -> bool {
        let armed = self.key.take();
        armed.is_some() && armed == key
    }
}
fn activation_key(stroke: &gpui::Keystroke) -> Option<gpui::KeyboardButton> {
    if stroke.modifiers.modified() {
        return None;
    }
    match stroke.key.as_str() {
        "space" => Some(gpui::KeyboardButton::Space),
        "enter" => Some(gpui::KeyboardButton::Enter),
        _ => None,
    }
}

#[derive(Clone, PartialEq)]
pub enum SidebarIcon {
    Glyph(&'static str),
    Account(SharedString),
    Update,
}

#[derive(Clone, PartialEq)]
pub enum SessionName {
    Loading(SharedString),
    Unavailable(SharedString),
}

pub struct Control {
    sidebar_icon: Option<SidebarIcon>,
    sidebar_selected: bool,
    sidebar_row: bool,
    sidebar_muted: bool,
    #[cfg(test)]
    renders: usize,
    id: u32,
    button_size: Option<(f32, f32)>,
    switch: bool,
    session_style: Option<SessionStyle>,
    toolbar_icon: Option<&'static str>,
    native_tooltip_visible: bool,
    inspector_icon: Option<&'static str>,
    navigation_accessory_icon: Option<&'static str>,
    circle_icon: Option<&'static str>,
    navigation_glyph: Option<&'static str>,
    navigation_detail: Option<SharedString>,
    navigation_child: bool,
    navigation_accessory: Option<gpui::Entity<Control>>,
    app_icon: Option<std::sync::Arc<gpui::RenderImage>>,
    app_icon_failed: bool,
    compact_app_icon: bool,
    app_icon_loading: bool,
    session_name: Option<SessionName>,
    session_info_error: Option<SharedString>,
    launch_progress: Option<(&'static str, std::time::Instant)>,
    name_loading_since: Option<std::time::Instant>,
    navigation: Option<bool>,
    navigation_selection_active: bool,
    table_cells: Option<TableCells>,
    copy_value: Option<CopyValue>,
    kind: Kind,
    popup_items: Vec<MenuEntry>,
    popup_context: Option<PopupContext>,
    popup_update_action: Option<u32>,
    popup_open: bool,
    popup_below: bool,
    bounds: Option<Bounds<Pixels>>,
    label: SharedString,
    accessibility_label: Option<SharedString>,
    disabled: bool,
    theme: Theme,
    focus: FocusHandle,
    show_focus_ring: bool,
    focus_intent: FocusIntent,
    subscriptions: Option<[Subscription; 2]>,
    activation_subscription: Option<Subscription>,
    press: Press,
    accessibility_scroll: bool,
    activate: Activate,
    context_menu: Option<ContextMenu>,
    single_click: Option<Activate>,
    delayed_rename: Option<Activate>,
    rename_press: Option<u64>,
    rename_task: Option<gpui::Task<()>>,
}
struct CopyValue {
    value: SharedString,
    copied: bool,
}
struct TableCells {
    values: [SharedString; 2],
    binary_value: bool,
}
struct PopupContext {
    icon: Option<&'static str>,
    detail: SharedString,
}
impl Control {
    /// Inspector popups retain the ordinary menu activation and accessible identity.
    pub fn set_popup_context(
        &mut self,
        icon: Option<&'static str>,
        detail: &str,
        cx: &mut Context<Self>,
    ) {
        if self
            .popup_context
            .as_ref()
            .is_none_or(|context| context.icon != icon || context.detail.as_ref() != detail)
        {
            self.popup_context = Some(PopupContext { icon, detail: detail.to_owned().into() });
            cx.notify();
        }
    }

    /// A compact inspector action uses the existing button input and AX behavior.
    pub fn set_inspector_icon(&mut self, asset: &'static str, cx: &mut Context<Self>) {
        if self.inspector_icon != Some(asset) {
            self.inspector_icon = Some(asset);
            self.button_size = Some((24., 24.));
            cx.notify();
        }
    }

    /// A row accessory inherits its row's foreground and uses its label as the tooltip.
    pub fn set_navigation_accessory_icon(&mut self, asset: &'static str, cx: &mut Context<Self>) {
        if self.navigation_accessory_icon != Some(asset) {
            self.navigation_accessory_icon = Some(asset);
            self.button_size = Some((24., 24.));
            cx.notify();
        }
    }

    /// Switch presentation retains the checkbox's toggle action and accessible label.
    pub fn set_switch(&mut self, cx: &mut Context<Self>) {
        if !self.switch {
            self.switch = true;
            self.button_size = Some((36., 24.));
            cx.notify();
        }
    }

    pub fn set_table_cells(
        &mut self,
        key: &str,
        value: &str,
        binary_value: bool,
        cx: &mut Context<Self>,
    ) {
        if self.table_cells.as_ref().is_none_or(|cells| {
            cells.values[0].as_ref() != key
                || cells.values[1].as_ref() != value
                || cells.binary_value != binary_value
        }) {
            self.table_cells = Some(TableCells {
                values: [key.to_owned().into(), value.to_owned().into()],
                binary_value,
            });
            cx.notify();
        }
    }
    fn clear_copy_feedback(&mut self) {
        if let Some(copy) = &mut self.copy_value {
            copy.copied = false;
        }
    }

    /// Two-line copy action using the same pointer, keyboard and AX semantics as buttons.
    pub fn set_copy_value(&mut self, value: &str, cx: &mut Context<Self>) {
        if self.copy_value.as_ref().is_none_or(|copy| copy.value.as_ref() != value) {
            self.copy_value = Some(CopyValue { value: value.to_owned().into(), copied: false });
            cx.notify();
        }
    }
    /// Fixed-size form actions retain the ordinary button input and AX behavior.
    pub fn set_button_size(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        let size = Some((width, height));
        if self.button_size != size {
            self.button_size = size;
            cx.notify();
        }
    }
    pub fn new(
        id: u32,
        properties: Properties<'_>,
        activate: Activate,
        cx: &mut Context<Self>,
    ) -> Self {
        let Properties { kind, label, disabled, theme } = properties;
        Self {
            #[cfg(test)]
            renders: 0,
            id,
            sidebar_icon: None,
            sidebar_selected: false,
            sidebar_row: false,
            sidebar_muted: false,
            context_menu: None,
            single_click: None,
            delayed_rename: None,
            rename_press: None,
            rename_task: None,
            button_size: None,
            switch: false,
            session_style: None,
            toolbar_icon: None,
            native_tooltip_visible: false,
            inspector_icon: None,
            navigation_accessory_icon: None,
            circle_icon: None,
            navigation_glyph: None,
            navigation_detail: None,
            navigation_child: false,
            navigation_accessory: None,
            app_icon: None,
            app_icon_failed: false,
            compact_app_icon: false,
            app_icon_loading: false,
            session_name: None,
            session_info_error: None,
            launch_progress: None,
            name_loading_since: None,
            navigation: None,
            navigation_selection_active: true,
            table_cells: None,
            copy_value: None,
            kind,
            popup_items: Vec::new(),
            popup_context: None,
            popup_update_action: None,
            popup_open: false,
            popup_below: false,
            bounds: None,
            label: label.to_owned().into(),
            accessibility_label: None,
            disabled,
            theme: theme.clone(),
            focus: cx.focus_handle().tab_stop(!disabled),
            show_focus_ring: false,
            focus_intent: FocusIntent::None,
            subscriptions: None,
            activation_subscription: None,
            press: Press::default(),
            accessibility_scroll: false,
            activate,
        }
    }

    pub fn set_sidebar_row(&mut self) {
        self.sidebar_row = true;
    }

    pub fn set_sidebar_icon(
        &mut self,
        icon: SidebarIcon,
        selected: bool,
        muted: bool,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_icon.as_ref() != Some(&icon)
            || self.sidebar_selected != selected
            || self.sidebar_muted != muted
        {
            self.sidebar_icon = Some(icon);
            self.sidebar_selected = selected;
            self.sidebar_muted = muted;
            self.button_size = Some((24., 24.));
            cx.notify();
        }
    }
    pub fn set_session_style(&mut self, style: SessionStyle, cx: &mut Context<Self>) {
        if self.session_style != Some(style) {
            self.session_style = Some(style);
            self.button_size = match style {
                SessionStyle::Template => Some((200., 162.)),
                SessionStyle::Recent => Some((88., 90.)),
                SessionStyle::Stop => Some((24., 24.)),
                _ => None,
            };
            cx.notify();
        }
    }

    pub fn toolbar_icon(mut self, asset: &'static str) -> Self {
        self.toolbar_icon = Some(asset);
        self
    }

    pub fn set_toolbar_icon(&mut self, asset: &'static str, cx: &mut Context<Self>) {
        if self.toolbar_icon != Some(asset) {
            self.toolbar_icon = Some(asset);
            cx.notify();
        }
    }

    pub fn circle_icon(mut self, asset: &'static str) -> Self {
        self.circle_icon = Some(asset);
        self.button_size = Some((32., 32.));
        self
    }
    pub fn accessibility_label(mut self, label: String) -> Self {
        self.accessibility_label = Some(label.into());
        self
    }
    pub fn set_context_menu(&mut self, menu: ContextMenu) {
        self.context_menu = Some(menu);
    }
    /// Pointer selection is separate from double-click, keyboard and AX activation.
    pub fn set_single_click(&mut self, select: Activate) {
        self.single_click = Some(select);
    }
    pub fn navigation(mut self) -> Self {
        self.navigation = Some(false);
        self
    }
    pub fn set_compact_app_icon(&mut self, cx: &mut Context<Self>) {
        if !self.compact_app_icon {
            self.compact_app_icon = true;
            cx.notify();
        }
    }
    pub fn set_app_icon(
        &mut self,
        icon: Option<std::sync::Arc<gpui::RenderImage>>,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.app_icon != icon || self.app_icon_failed != failed {
            self.app_icon = icon;
            self.app_icon_failed = failed;
            cx.notify();
        }
    }
    pub fn select_navigation(&mut self, selected: bool, cx: &mut Context<Self>) {
        if !selected {
            self.cancel_delayed_rename();
        }
        if self.navigation != Some(selected) {
            self.navigation = Some(selected);
            cx.notify();
        }
    }
    pub fn set_activation(&mut self, activate: Activate) {
        self.activate = activate;
    }
    pub fn set_popup_items(&mut self, items: &[MenuEntry], cx: &mut Context<Self>) {
        if self.popup_items != items {
            self.popup_items = items.to_vec();
            cx.notify();
        }
    }
    pub fn popup_is_open(&self) -> bool {
        self.popup_open
    }
    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn popup_for_test(&self) -> (&[MenuEntry], Option<u32>, Activate) {
        (&self.popup_items, self.popup_update_action, self.activate.clone())
    }
    /// The host account menu emphasizes its prepared-update action locally.
    #[cfg(feature = "desktop")]
    pub(crate) fn set_popup_update_action(&mut self, action: Option<u32>, cx: &mut Context<Self>) {
        if self.popup_update_action != action {
            self.popup_update_action = action;
            cx.notify();
        }
    }
    pub fn set_popup_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.popup_open != open {
            self.popup_open = open;
            if !open {
                // The modal menu consumed the corresponding pointer/key release.
                self.press.cancel();
            }
            cx.notify();
        }
    }
    /// Toolbar icon menus unfold below their button rather than above the sidebar footer.
    pub fn set_popup_below(&mut self) {
        self.popup_below = true;
    }
    fn open_popup(&self, window: &Window, cx: &mut Context<Self>) {
        if self.disabled || self.popup_open || self.popup_items.is_empty() {
            return;
        }
        let Some(bounds) = self.bounds else { return };
        #[cfg(target_os = "macos")]
        {
            let mut request = popup::Request::button(
                window,
                bounds,
                self.label.to_string(),
                self.popup_items.clone(),
                self.activate.clone(),
                cx.entity().downgrade(),
                cx,
            );
            if self.popup_below {
                request = request.below_button();
                if self.sidebar_icon.is_some() {
                    request = request.align_icon_to_button();
                }
            } else if self.sidebar_icon.is_some() {
                request = request.above_button();
            }
            request.opening_press = self.press.pointer;
            request.update_action = self.popup_update_action;
            cx.defer(move |cx| popup::open(request, cx));
        }
        #[cfg(not(target_os = "macos"))]
        panic!("Pop-up menus require a platform menu presenter: {bounds:?}");
    }
    pub fn focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }
    #[cfg(any(test, feature = "desktop"))]
    pub(crate) fn keyboard_focus_visible(&self, window: &Window) -> bool {
        !self.disabled && self.show_focus_ring && self.focus.is_focused(window)
    }
    pub fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }
    /// Restore logical focus after pointer or arrow navigation without entering Tab mode.
    #[cfg(any(test, feature = "desktop"))]
    pub(crate) fn focus_without_ring(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.disabled {
            return false;
        }
        self.cancel_delayed_rename();
        self.press.cancel();
        self.focus_intent = FocusIntent::WithoutRing;
        self.show_focus_ring = false;
        self.accessibility_scroll = true;
        self.focus.focus(window);
        cx.notify();
        true
    }
    pub fn show_keyboard_focus_if_focused(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.disabled || !self.focus.is_focused(window) {
            // Pointer activation need not focus the control. Keyboard navigation
            // must also retire its copy feedback when focus moves elsewhere.
            if self.copy_value.as_ref().is_some_and(|copy| copy.copied) {
                self.clear_copy_feedback();
                cx.notify();
            }
            return false;
        }
        self.show_focus_ring = true;
        self.focus_intent = FocusIntent::None;
        self.accessibility_scroll = true;
        cx.notify();
        true
    }
    /// Apply a received presentation before the next frame starts.
    pub fn set(&mut self, properties: Properties<'_>, cx: &mut Context<Self>) {
        let Properties { kind, label, disabled, theme } = properties;
        if self.kind == kind
            && self.label.as_ref() == label
            && self.disabled == disabled
            && &self.theme == theme
        {
            return;
        }
        self.kind = kind;
        if kind != Kind::Popup {
            self.popup_items.clear();
            self.popup_open = false;
        }
        if self.label.as_ref() != label {
            self.label = label.to_owned().into();
        }
        if self.disabled != disabled {
            self.focus = self.focus.clone().tab_stop(!disabled);
            self.disabled = disabled;
            if disabled {
                self.show_focus_ring = false;
            }
        }
        self.theme = theme.clone();
        if disabled {
            self.press.cancel();
            self.cancel_delayed_rename();
            // A read can disable a row before its queued focus event is delivered.
            // Retain established pointer/arrow modality; only cancel an unaccepted press.
            if matches!(self.focus_intent, FocusIntent::PendingSelection { .. }) {
                self.focus_intent = FocusIntent::None;
            }
        }
        cx.notify();
    }
    pub fn retire(&mut self) {
        self.disabled = true;
        self.press.cancel();
        self.cancel_delayed_rename();
        self.focus_intent = FocusIntent::None;
    }
    pub(crate) fn clear_native_tooltip(&mut self, window: &Window) {
        if std::mem::take(&mut self.native_tooltip_visible) {
            set_native_tooltip(window, None);
        }
    }
    pub fn cancel_pointer_press(&mut self, cx: &mut Context<Self>) -> bool {
        self.cancel_delayed_rename();
        if matches!(self.focus_intent, FocusIntent::PendingSelection { .. }) {
            self.focus_intent = FocusIntent::None;
        }
        let active = self.press.pointer;
        if active {
            self.press.cancel();
            cx.notify();
        }
        active
    }
    fn activate(&mut self, cx: &mut Context<Self>) {
        self.cancel_delayed_rename();
        if self.disabled {
            return;
        }
        #[cfg(target_os = "macos")]
        if popup::is_modal_active() {
            return;
        }
        if let Some(copy) = &mut self.copy_value {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.value.to_string()));
            copy.copied = true;
            cx.notify();
        } else {
            (self.activate)(self.id, cx);
        }
    }
}

/// Keep the tooltip in AppKit's native tooltip manager while GPUI determines
/// the actual icon hit area.
#[cfg(all(target_os = "macos", not(test)))]
#[allow(unsafe_code, reason = "The NSView belongs to this main-thread GPUI window.")]
pub(crate) fn set_native_tooltip(window: &Window, text: Option<&str>) {
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    let view = handle.ns_view.as_ptr() as *mut Object;
    unsafe {
        let text: *mut Object =
            if let Some(label) = text.and_then(|text| std::ffi::CString::new(text).ok()) {
                msg_send![class!(NSString), stringWithUTF8String: label.as_ptr()]
            } else {
                std::ptr::null_mut()
            };
        let _: () = msg_send![view, setToolTip: text];
    }
}

#[cfg(any(not(target_os = "macos"), test))]
pub(crate) fn set_native_tooltip(_: &Window, _: Option<&str>) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[cfg(feature = "desktop")]
    #[gpui::test]
    fn specialized_controls_scale_in_focus_hover_pressed_and_disabled_states(
        cx: &mut gpui::TestAppContext,
    ) {
        // Every specialized family uses the same scale for its surface and root hit target.
        for family in 0..15 {
            for state in 0..4 {
                let window = cx.add_window(|window, cx| {
                    let kind = match family {
                        1 => Kind::Checkbox(CheckState::Off),
                        2 => Kind::Checkbox(CheckState::On),
                        3 => Kind::Checkbox(CheckState::Mixed),
                        4 => Kind::Button(ButtonStyle::Primary),
                        5 => Kind::Button(ButtonStyle::Destructive),
                        6 => Kind::Popup,
                        _ => Kind::Button(ButtonStyle::Default),
                    };
                    let mut control = Control::new(
                        1,
                        Properties {
                            kind,
                            label: "A control label",
                            disabled: state == 3,
                            theme: &Theme::light(),
                        },
                        Rc::new(|_, _| {}),
                        cx,
                    );
                    match family {
                        7 => control = control.toolbar_icon("session-icon"),
                        8 => control.set_sidebar_icon(
                            SidebarIcon::Account("A".into()),
                            true,
                            false,
                            cx,
                        ),
                        9 => {
                            control = control.navigation();
                            control.set_sidebar_row();
                        }
                        10 => control.set_session_style(SessionStyle::Tab(true), cx),
                        11 => control.set_session_style(SessionStyle::Template, cx),
                        12 => control.set_session_style(SessionStyle::Recent, cx),
                        13 => control.set_session_style(SessionStyle::Share, cx),
                        14 => control.set_session_style(SessionStyle::Stop, cx),
                        _ => {}
                    }
                    control.press.hovered = state == 1 || state == 2;
                    control.press.pointer = state == 2;
                    control.show_focus_ring = state == 0;
                    window.focus(&control.focus);
                    control
                });
                cx.run_until_parked();
                crate::desktop::test_support::check_zoom_geometry(window.into(), cx);
                window.update(cx, |_, window, _| window.remove_window()).unwrap();
            }
        }
    }

    #[gpui::test]
    fn header_icon_targets_stay_square_in_a_narrow_flex_header(cx: &mut gpui::TestAppContext) {
        struct Header {
            button: gpui::Entity<Control>,
        }
        impl gpui::Render for Header {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .w(zoom::pixels(window, 40.))
                    .flex()
                    .items_center()
                    .child(div().w(zoom::pixels(window, 32.)).flex_none())
                    .child(self.button.clone())
            }
        }
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).unwrap());
            for popup in [false, true] {
                let activated = Rc::new(std::cell::Cell::new(0));
                let count = activated.clone();
                let window = cx.add_window(|window, cx| {
                    zoom::apply(window, cx);
                    let button = cx.new(|cx| {
                        let mut button = Control::new(
                            1,
                            Properties {
                                kind: if popup {
                                    Kind::Popup
                                } else {
                                    Kind::Button(ButtonStyle::Default)
                                },
                                label: "New item",
                                disabled: false,
                                theme: &Theme::light(),
                            },
                            Rc::new(move |_, _| count.set(count.get() + 1)),
                            cx,
                        );
                        if !popup {
                            button = button.navigation().toolbar_icon("add");
                        }
                        button.set_sidebar_icon(SidebarIcon::Glyph("add"), false, false, cx);
                        button
                    });
                    Header { button }
                });
                cx.run_until_parked();
                let bounds = window
                    .update(cx, |header, window, cx| {
                        let bounds = header.button.read(cx).bounds.unwrap();
                        assert_eq!(
                            bounds.size,
                            gpui::size(zoom::pixels(window, 24.), zoom::pixels(window, 24.))
                        );
                        assert_eq!(bounds.left(), zoom::pixels(window, 32.));
                        bounds
                    })
                    .unwrap();
                if !popup {
                    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
                    let corner = point(bounds.left() + px(1.), bounds.top() + px(1.));
                    visual.simulate_mouse_down(corner, gpui::MouseButton::Left, Default::default());
                    assert_eq!(activated.get(), 0, "header actions still wait for release");
                    visual.simulate_mouse_up(corner, gpui::MouseButton::Left, Default::default());
                    assert_eq!(activated.get(), 1, "the square corner is part of the hit target");
                }
                window.update(cx, |_, window, _| window.remove_window()).unwrap();
            }
        }
    }

    #[gpui::test]
    fn sidebar_pointer_selection_double_click_and_context_menu(cx: &mut gpui::TestAppContext) {
        let selected = Rc::new(std::cell::Cell::new(0));
        let opened = Rc::new(std::cell::Cell::new(0));
        let menus = Rc::new(std::cell::Cell::new(0));
        let window = cx.add_window(|_, cx| {
            let opened = opened.clone();
            let selected = selected.clone();
            let menus = menus.clone();
            let mut control = Control::new(
                1,
                Properties {
                    kind: Kind::Button(ButtonStyle::Default),
                    label: "Session",
                    disabled: false,
                    theme: &Theme::light(),
                },
                Rc::new(move |_, _| opened.set(opened.get() + 1)),
                cx,
            );
            control.set_single_click(Rc::new(move |_, _| selected.set(selected.get() + 1)));
            control.set_context_menu(Rc::new(move |_, _, _| menus.set(menus.get() + 1)));
            control
        });
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let position = gpui::point(px(12.), px(12.));
        visual.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
        assert_eq!((selected.get(), opened.get()), (1, 0), "selection happens before release");
        visual.simulate_mouse_up(position, gpui::MouseButton::Left, Default::default());
        assert_eq!((selected.get(), opened.get()), (1, 0));
        visual.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: 2,
            ..Default::default()
        });
        visual.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: 2,
            ..Default::default()
        });
        assert_eq!((selected.get(), opened.get()), (2, 1));
        visual.simulate_mouse_down(position, gpui::MouseButton::Right, Default::default());
        visual.simulate_mouse_up(position, gpui::MouseButton::Right, Default::default());
        visual.simulate_click(position, gpui::Modifiers { control: true, ..Default::default() });
        assert_eq!((selected.get(), opened.get(), menus.get()), (2, 1, 2));
        window.update(cx, |control, window, _| window.focus(&control.focus)).unwrap();
        visual.simulate_keystrokes("enter");
        visual.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert_eq!(opened.get(), 2);
    }

    #[gpui::test]
    fn closing_popup_disarms_the_opening_press(cx: &mut gpui::TestAppContext) {
        let control = cx.new(|cx| {
            Control::new(
                1,
                Properties {
                    kind: Kind::Popup,
                    label: "Asia",
                    disabled: false,
                    theme: &Theme::light(),
                },
                Rc::new(|_, _| {}),
                cx,
            )
        });
        control.update(cx, |control, cx| {
            control.press.pointer = true;
            control.press.hovered = true;
            control.set_popup_open(true, cx);
            control.set_popup_open(false, cx);
            control.press.hovered = true;
            assert!(!control.press.active(), "hover after dismissal must not re-arm a press");
        });
    }

    struct CachedControls {
        controls: [gpui::Entity<Control>; 2],
        width: f32,
    }
    impl Render for CachedControls {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).flex().flex_col().children(
                self.controls
                    .iter()
                    .map(|control| element(control, window, cx))
                    .collect::<Vec<_>>(),
            )
        }
    }
    #[gpui::test]
    fn cached_checkbox_reuses_siblings_and_rebuilds_changed_state_and_bounds(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/InterVariable.ttf"))])
            .unwrap();
        let controls = std::array::from_fn(|id| {
            cx.new(|cx| {
                Control::new(
                    id as u32,
                    Properties {
                        kind: Kind::Checkbox(CheckState::Off),
                        label: "A long checkbox label",
                        disabled: false,
                        theme: &Theme::light(),
                    },
                    Rc::new(|_, _| {}),
                    cx,
                )
            })
        });
        let window =
            cx.add_window(|_, _| CachedControls { controls: controls.clone(), width: 300. });
        cx.run_until_parked();
        let counts = |cx: &gpui::TestAppContext| {
            controls.each_ref().map(|control| control.read_with(cx, |control, _| control.renders))
        };
        let initial = counts(cx);
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();
        assert_eq!(counts(cx), initial, "parent invalidation must reuse unchanged controls");
        controls[0].update(cx, |control, cx| {
            control.set(
                Properties {
                    kind: Kind::Checkbox(CheckState::Mixed),
                    label: "A long checkbox label",
                    disabled: true,
                    theme: &Theme::light(),
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            counts(cx),
            [initial[0] + 1, initial[1]],
            "state change must rebuild only its own control"
        );
        window
            .update(cx, |root, _, cx| {
                root.width = 80.;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            counts(cx),
            [initial[0] + 2, initial[1] + 1],
            "new bounds must invalidate both cached subtrees"
        );
    }
    #[gpui::test]
    fn checkbox_layout_preserves_its_surface_and_truncates_long_labels(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::{AvailableSpace, Empty, size};
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/InterVariable.ttf"))])
            .unwrap();
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(800.), px(600.)), |window, cx| {
            let theme = Theme::light();
            for state in [CheckState::Off, CheckState::On, CheckState::Mixed] {
                for disabled in [false, true] {
                    let control = cx.new(|cx| {
                        Control::new(
                            1,
                            Properties {
                                kind: Kind::Checkbox(state),
                                label: "A long task with café and 日本語 repeated across the row",
                                disabled,
                                theme: &theme,
                            },
                            Rc::new(|_, _| {}),
                            cx,
                        )
                    });
                    for width in [8., 24., 100., 800.] {
                        let mut element = control.update(cx, |control, cx| {
                            control.render(window, cx).into_any_element()
                        });
                        let measured = element.layout_as_root(
                            size(AvailableSpace::Definite(px(width)), AvailableSpace::MaxContent),
                            window,
                            cx,
                        );
                        assert_eq!(measured.height, px(24.));
                        assert!(
                            measured.width >= px(24.),
                            "the 16 px surface and 8 px padding must remain usable"
                        );
                        assert!(
                            measured.width <= px(width.max(24.)),
                            "labels must shrink within available row width"
                        );
                    }
                }
            }
            Empty
        });
    }

    #[gpui::test]
    fn cached_checkbox_matches_intrinsic_and_constrained_layout(cx: &mut gpui::TestAppContext) {
        use gpui::{AvailableSpace, Empty, size};
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/InterVariable.ttf"))])
            .unwrap();
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(800.), px(600.)), |window, cx| {
            let theme = Theme::light();
            for label in [
                "",
                "Task",
                "A long task with café and 日本語 repeated across the row",
                "first\nlonger second line",
            ] {
                let control = cx.new(|cx| {
                    Control::new(
                        1,
                        Properties {
                            kind: Kind::Checkbox(CheckState::Off),
                            label,
                            disabled: false,
                            theme: &theme,
                        },
                        Rc::new(|_, _| {}),
                        cx,
                    )
                });
                for available in [
                    AvailableSpace::MinContent,
                    AvailableSpace::MaxContent,
                    AvailableSpace::Definite(px(8.)),
                    AvailableSpace::Definite(px(24.)),
                    AvailableSpace::Definite(px(100.)),
                    AvailableSpace::Definite(px(800.)),
                ] {
                    for nested in 0..3 {
                        let wrap = |child: gpui::AnyElement| match nested {
                            1 => div()
                                .flex()
                                .gap(px(12.))
                                .child(div().w(px(40.)).h(px(20.)))
                                .child(child)
                                .into_any_element(),
                            2 => div()
                                .flex()
                                .flex_col()
                                .child(div().w(px(40.)).h(px(20.)))
                                .child(child)
                                .into_any_element(),
                            _ => child,
                        };
                        let mut plain = wrap(control.update(cx, |control, cx| {
                            control.render(window, cx).into_any_element()
                        }));
                        let mut cached = wrap(element(&control, window, cx));
                        let available = size(available, AvailableSpace::MaxContent);
                        let expected = plain.layout_as_root(available, window, cx);
                        let actual = cached.layout_as_root(available, window, cx);
                        assert_eq!(
                            actual, expected,
                            "label={label:?}, nested={nested}, available={available:?}"
                        );
                    }
                }
            }
            Empty
        });
    }

    #[gpui::test]
    fn retired_control_cannot_dispatch_from_an_older_frame(cx: &mut gpui::TestAppContext) {
        let count = Rc::new(std::cell::Cell::new(0));
        let counter = count.clone();
        let theme = Theme::light();
        let control = cx.new(|cx| {
            Control::new(
                1,
                Properties {
                    kind: Kind::Checkbox(CheckState::Off),
                    label: "Task",
                    disabled: false,
                    theme: &theme,
                },
                Rc::new(move |_, _| counter.set(counter.get() + 1)),
                cx,
            )
        });
        control.update(cx, |this, cx| this.activate(cx));
        assert_eq!(count.get(), 1);
        control.update(cx, |this, cx| {
            this.retire();
            this.activate(cx);
        });
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn keyboard_release_requires_matching_uncancelled_press() {
        let mut press = Press::default();
        assert!(!press.release_key(Some(gpui::KeyboardButton::Space)));
        press.key = Some(gpui::KeyboardButton::Space);
        assert!(!press.release_key(Some(gpui::KeyboardButton::Enter)));
        press.key = Some(gpui::KeyboardButton::Space);
        press.cancel();
        assert!(!press.release_key(Some(gpui::KeyboardButton::Space)));
        press.key = Some(gpui::KeyboardButton::Enter);
        assert!(press.release_key(Some(gpui::KeyboardButton::Enter)));
        assert!(!press.release_key(Some(gpui::KeyboardButton::Enter)));
    }
    #[gpui::test]
    fn pointer_release_requires_an_uncancelled_press(cx: &mut gpui::TestAppContext) {
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/InterVariable.ttf"))])
            .unwrap();
        for kind in [
            Kind::Checkbox(CheckState::Off),
            Kind::Button(ButtonStyle::Primary),
            Kind::Button(ButtonStyle::Destructive),
        ] {
            let count = Rc::new(std::cell::Cell::new(0));
            let counter = count.clone();
            let window = cx.add_window(|_, cx| {
                Control::new(
                    1,
                    Properties { kind, label: "Activate", disabled: false, theme: &Theme::light() },
                    Rc::new(move |_, _| counter.set(counter.get() + 1)),
                    cx,
                )
            });
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            let inside = point(px(12.), px(12.));
            let outside = point(px(400.), px(100.));
            let focused_before_click =
                window.update(cx, |control, window, _| control.focus.is_focused(window)).unwrap();
            visual.simulate_mouse_move(inside, None, Default::default());
            visual.simulate_mouse_down(inside, MouseButton::Right, Default::default());
            visual.simulate_mouse_up(inside, MouseButton::Right, Default::default());
            window
                .update(cx, |control, window, _| {
                    assert_eq!(
                        control.focus.is_focused(window),
                        focused_before_click,
                        "right click must not transfer keyboard focus"
                    );
                })
                .unwrap();
            assert_eq!(count.get(), 0);
            visual.simulate_click(inside, Default::default());
            assert_eq!(count.get(), 1, "ordinary release activates once");
            window
                .update(cx, |control, window, _| {
                    assert_eq!(
                        control.focus.is_focused(window),
                        focused_before_click,
                        "pointer activation leaves keyboard focus alone"
                    );
                })
                .unwrap();
            window.update(cx, |control, window, _| window.focus(&control.focus)).unwrap();
            cx.run_until_parked();
            window
                .update(cx, |control, window, cx| {
                    assert!(control.show_keyboard_focus_if_focused(window, cx));
                    assert!(control.show_focus_ring);
                })
                .unwrap();
            visual.simulate_mouse_down(inside, MouseButton::Left, Default::default());
            visual.simulate_keystrokes("escape");
            visual.simulate_mouse_up(inside, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 1, "Escape cancels the pending pointer activation");
            visual.simulate_mouse_down(inside, MouseButton::Left, Default::default());
            visual.simulate_mouse_move(outside, MouseButton::Left, Default::default());
            visual.simulate_mouse_up(outside, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 1, "outside release cancels");
            visual.simulate_mouse_move(inside, None, Default::default());
            visual.simulate_click(inside, Default::default());
            assert_eq!(count.get(), 2, "a later fresh press still activates once");
            visual.simulate_mouse_down(inside, MouseButton::Left, Default::default());
            visual.simulate_mouse_move(outside, MouseButton::Left, Default::default());
            visual.simulate_mouse_move(inside, MouseButton::Left, Default::default());
            visual.simulate_mouse_up(inside, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 3, "dragging back inside retains the original press");
            visual.simulate_mouse_down(inside, MouseButton::Left, Default::default());
            for disabled in [true, false] {
                window
                    .update(cx, |control, _, cx| {
                        control.set(
                            Properties {
                                kind,
                                label: "Activate",
                                disabled,
                                theme: &Theme::light(),
                            },
                            cx,
                        )
                    })
                    .unwrap();
                cx.run_until_parked();
            }
            visual.simulate_mouse_up(inside, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 3, "re-enabling does not revive a canceled press");
            visual.simulate_mouse_down(inside, MouseButton::Left, Default::default());
            window
                .update(cx, |control, _, cx| {
                    assert!(control.cancel_pointer_press(cx));
                })
                .unwrap();
            cx.run_until_parked();
            visual.simulate_mouse_up(inside, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 3, "explicit cancellation prevents pointer activation");
        }
    }
    #[gpui::test]
    async fn volume_asset_retains_color_and_bounded_pixels(cx: &mut gpui::TestAppContext) {
        let bytes = Assets.load("volume").unwrap().unwrap();
        assert!(bytes.len() < 4096);
        let source = gpui::Resource::Path(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/volume.png")).into(),
        );
        let (task, first) = cx.update(|cx| cx.fetch_asset::<gpui::ImageAssetLoader>(&source));
        assert!(first);
        let image = task.await.unwrap();
        let (task, first) = cx.update(|cx| cx.fetch_asset::<gpui::ImageAssetLoader>(&source));
        assert!(!first);
        assert!(std::sync::Arc::ptr_eq(&image, &task.await.unwrap()));
        assert_eq!(image.size(0), gpui::size(gpui::DevicePixels(32), gpui::DevicePixels(32)));
        assert_eq!(image.frame_count(), 1);
        let pixels = image.as_bytes(0).unwrap();
        assert_eq!(pixels.len(), 4096);
        assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
        for (x, y) in [(8, 18), (24, 18), (16, 6)] {
            let pixel = &pixels[(y * 32 + x) * 4..][..4];
            assert_eq!(pixel[3], 255);
            assert!(
                pixel[2] > pixel[0] && pixel[1] > pixel[0],
                "The volume must retain its BGRA color"
            );
        }
    }
    #[gpui::test]
    #[ignore = "Reference-device artwork timing; never run on CI"]
    async fn benchmark_volume_asset_load(cx: &mut gpui::TestAppContext) {
        let source = gpui::Resource::Path(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/volume.png")).into(),
        );
        let mut elapsed = Vec::with_capacity(1000);
        for _ in 0..1000 {
            cx.update(|cx| cx.remove_asset::<gpui::ImageAssetLoader>(&source));
            let start = std::time::Instant::now();
            std::hint::black_box(
                cx.update(|cx| cx.fetch_asset::<gpui::ImageAssetLoader>(&source).0).await.unwrap(),
            );
            elapsed.push(start.elapsed());
        }
        elapsed.sort_unstable();
        crate::diagnostic!(
            "Volume artwork file read + uncached decode: n=1000 p50={:?} p99={:?} max={:?}",
            elapsed[499],
            elapsed[989],
            elapsed[999]
        );
        assert!(elapsed[989] < std::time::Duration::from_millis(1));
    }
    #[test]
    fn launch_spinner_renders_a_transparent_gradient_mask() {
        // Render through the production SVG decoder: embedded images must not
        // disappear, become an opaque square, or lose their gradient alpha.
        let bytes = Assets.load("progress-spinner").unwrap().unwrap();
        let tree = resvg::usvg::Tree::from_data(&bytes, &Default::default()).unwrap();
        let mut pixels = resvg::tiny_skia::Pixmap::new(32, 32).unwrap();
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(2., 2.), &mut pixels.as_mut());
        let alpha: std::collections::BTreeSet<_> =
            pixels.pixels().iter().map(|pixel| pixel.alpha()).collect();
        let visible = pixels.pixels().iter().filter(|pixel| pixel.alpha() > 0).count();
        assert!(visible > 200 && visible < 800);
        assert!(alpha.len() > 32);
        assert_eq!(pixels.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(pixels.pixel(16, 16).unwrap().alpha(), 0);
    }

    #[test]
    fn state_and_assets_fit_control_budget() {
        assert!(std::mem::size_of::<Control>() * MAX_CONTROLS <= 256 * 1024);
        let bytes = Assets.load("check-on").unwrap().unwrap().len()
            + Assets.load("check-mixed").unwrap().unwrap().len();
        assert!(bytes < 4096);
        assert!(Assets.load("missing").unwrap().is_none());
    }
}
