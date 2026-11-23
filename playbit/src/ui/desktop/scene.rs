//! Reusable GPUI scene renderer. Owns controls/drafts; no engine, IPC or session.
use super::{
    UI_CONTROL_TRACKING, canvas::canvas_element, controls, segmented, text_accessibility_node,
    text_editor, zoom,
};
use crate::runtime::ViewNode;
use gpui::{App, Entity, Focusable, MouseButton, SharedString, Window, div, prelude::*, rgba};
use playbit_api::abi::syscall::EventKind;
use std::{cell::Cell, collections::BTreeMap, rc::Rc, sync::Arc};

/// One host-bound input; text revisions acknowledge local drafts, not app data.
pub struct Input {
    pub kind: EventKind,
    pub target: u32,
    pub data: Vec<u8>,
    pub revision: Option<u64>,
}
pub type SendInput = Arc<dyn Fn(Input) -> bool + Send + Sync>;
#[derive(Clone, Copy, Default)]
pub struct InputAck {
    pub target: u32,
    pub revision: u64,
}
impl InputAck {
    pub fn new(target: u32, revision: u64) -> Self {
        Self { target, revision }
    }
}
pub struct Scene {
    send: SendInput,
    active: bool,
    pointer: Rc<Cell<Option<u32>>>,
    activate: controls::Activate,
    pub(crate) editors: BTreeMap<u32, Entity<text_editor::TextEditor>>,
    controls: BTreeMap<u32, Entity<controls::Control>>,
    segments: BTreeMap<u32, Entity<segmented::Segmented>>,
    scrolls: BTreeMap<u32, Entity<super::scrollbar::Scrollbars>>,
    images: BTreeMap<u32, Entity<super::image::ImageView>>,
    canvas_images: Vec<Entity<super::canvas_images::CanvasImages>>,
    canvas_index: usize,
    // Stable identities for text by traversal position, with no duplicate text.
    pub(crate) text_ids: Vec<Entity<()>>,
    text_index: usize,
    context_ids: Vec<Entity<()>>,
    context_index: usize,
    surface: super::surface::Observer,
}
impl Scene {
    pub fn new(send: SendInput, activate: controls::Activate) -> Self {
        Self {
            send,
            active: true,
            pointer: Rc::new(Cell::new(None)),
            activate,
            editors: BTreeMap::new(),
            controls: BTreeMap::new(),
            segments: BTreeMap::new(),
            scrolls: BTreeMap::new(),
            images: BTreeMap::new(),
            canvas_images: Vec::new(),
            canvas_index: 0,
            text_ids: Vec::new(),
            text_index: 0,
            context_ids: Vec::new(),
            context_index: 0,
            surface: super::surface::Observer::default(),
        }
    }
    /// Observe the drawable viewport independently of the guest's scrollable content.
    pub fn surface_observer(&self, extent: playbit_api::ui::CanvasExtent) -> impl IntoElement {
        self.surface.element(self.send.clone(), extent)
    }
    /// A new app instance must receive the current viewport even without a resize.
    pub fn reset_surface(&self) {
        self.surface.reset();
    }
    pub fn cancel_pointer_presses(&self, cx: &mut App) -> bool {
        let mut canceled = false;
        for control in self.controls.values() {
            canceled |= control.update(cx, |control, cx| control.cancel_pointer_press(cx));
        }
        for group in self.segments.values() {
            canceled |= group.update(cx, |group, cx| group.cancel_pointer_press(cx));
        }
        canceled
    }
    pub fn update_keyboard_focus_after_tab(&self, window: &Window, cx: &mut App) {
        for editor in self.editors.values() {
            let focused = editor.read(cx).focus_handle(cx).is_focused(window);
            if focused {
                editor.update(cx, |editor, cx| editor.select_all(cx));
                return;
            }
        }
        for control in self.controls.values() {
            if control.update(cx, |control, cx| control.show_keyboard_focus_if_focused(window, cx))
            {
                return;
            }
        }
        for group in self.segments.values() {
            if group.update(cx, |group, cx| group.show_keyboard_focus_if_focused(window, cx)) {
                return;
            }
        }
    }
    pub fn invalidate_drafts(&self, cx: &mut App) {
        for editor in self.editors.values() {
            editor.update(cx, |editor, _| editor.invalidate_delivery());
        }
    }
    /// Bind a reconstructed owner without replaying input. Existing field text
    /// remains private until a later edit or explicit user action sends it.
    pub fn rebind(&mut self, send: SendInput, cx: &mut App) {
        self.pointer.set(None);
        self.send = send;
        self.active = true;
        for (id, editor) in &self.editors {
            let (change, commit) = text_delivery(*id, self.send.clone());
            editor.update(cx, |editor, _| editor.rebind_delivery(change, commit));
        }
        self.surface.reset();
    }
    /// The host can close input admission without changing the guest's scene.
    /// Retained fields remain selectable and copyable while stopped.
    pub fn set_active(&mut self, active: bool, view: Option<&ViewNode>, cx: &mut App) {
        self.active = active;
        if !active {
            self.pointer.set(None);
        }
        if let Some(view) = view {
            self.update_controls(view, &playbit_api::ui::Theme::light(), cx);
        }
    }
    pub fn send_drafts(&self, view: Option<&ViewNode>, retry: bool, cx: &mut App) -> bool {
        if retry {
            for editor in self.editors.values() {
                editor.update(cx, |editor, _| editor.retry_draft());
            }
        }
        for (id, editor) in &self.editors {
            if view.is_none_or(|view| !field_editable(view, *id)) {
                continue;
            }
            if !editor.update(cx, |editor, cx| editor.send_draft(cx)) {
                return false;
            }
        }
        true
    }
    /// Only an accepted canvas press may take keyboard focus from its shell.
    pub fn has_canvas_pointer(&self) -> bool {
        self.pointer.get().is_some()
    }
    /// Release only a gesture whose press was accepted by this exact owner.
    /// Queue refusal keeps it retained; no new owner receives a prior gesture.
    pub fn release_canvas_pointer(&self) -> bool {
        let Some(target) = self.pointer.get() else { return true };
        let data = playbit_api::abi::syscall::PointerInput { x: 0, y: 0, phase: 3, buttons: 0 }
            .encode()
            .to_vec();
        if !(self.send)(Input { kind: EventKind::Pointer, target, data, revision: None }) {
            return false;
        }
        self.pointer.set(None);
        true
    }
    /// The host supplies a scene accepted by the shared runtime decoder.
    /// Admit the control inventory before changing retained native entities.
    pub fn apply(
        &mut self,
        next_view: &ViewNode,
        input_ack: InputAck,
        cx: &mut App,
    ) -> Result<(), &'static str> {
        let ids = control_ids(next_view)?;
        self.scrolls.retain(|id, _| ids.contains(&(*id, InventoryKind::Scroll)));
        self.images.retain(|id, _| ids.contains(&(*id, InventoryKind::Image)));
        self.editors.retain(|id, editor| {
            let keep = ids.contains(&(*id, InventoryKind::Text));
            editor
                .update(cx, |editor, _| editor.set_enabled(keep && field_enabled(next_view, *id)));
            keep
        });
        self.controls.retain(|id, control| {
            let keep = ids.contains(&(*id, InventoryKind::Control));
            if !keep {
                control.update(cx, |control, _| control.retire());
            }
            keep
        });
        self.segments.retain(|id, group| {
            let keep = ids.contains(&(*id, InventoryKind::Segments));
            if !keep {
                group.update(cx, |group, _| group.retire());
            }
            keep
        });
        self.update_controls(next_view, &playbit_api::ui::Theme::light(), cx);
        for (id, editor) in &self.editors {
            if let Some(value) = field_value(next_view, *id) {
                editor.update(cx, |editor, cx| {
                    editor.acknowledge(
                        (input_ack.target == *id).then_some(input_ack.revision),
                        value,
                        cx,
                    )
                });
            }
        }
        Ok(())
    }
    pub fn render(
        &mut self,
        view: Option<&ViewNode>,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        self.text_index = 0;
        self.context_index = 0;
        self.canvas_index = 0;
        let content = match view {
            Some(view) => {
                let content =
                    self.render_node(view, &playbit_api::ui::Theme::light(), window, cx, false);
                if contains_scroll(view) {
                    div().size_full().flex().flex_col().child(content).into_any_element()
                } else {
                    content
                }
            }
            None => div().into_any_element(),
        };
        self.text_ids.truncate(self.text_index);
        self.context_ids.truncate(self.context_index);
        self.canvas_images.truncate(self.canvas_index);
        content
    }
    fn render_node(
        &mut self,
        node: &ViewNode,
        theme: &playbit_api::ui::Theme,
        window: &mut Window,
        cx: &mut App,
        inline: bool,
    ) -> gpui::AnyElement {
        match node {
            ViewNode::Theme { child, theme, .. } => {
                self.render_node(child, theme, window, cx, inline)
            }
            ViewNode::Scroll { id, complex_background, child } => {
                let bars = self
                    .scrolls
                    .entry(*id)
                    .or_insert_with(|| {
                        cx.new(|_| super::scrollbar::Scrollbars::new(*complex_background))
                    })
                    .clone();
                bars.update(cx, |bars, _| bars.set_complex_background(*complex_background));
                let content = self.render_node(child, theme, window, cx, false);
                div()
                    .relative()
                    .flex_1()
                    .min_w(gpui::px(0.))
                    .min_h(gpui::px(0.))
                    .child(
                        div()
                            .id(("scroll", *id))
                            .absolute()
                            .size_full()
                            .when(contains_scroll(child), |v| v.flex().flex_col())
                            .overflow_scroll()
                            .track_scroll(&bars.read(cx).handle())
                            .child(content),
                    )
                    .child(super::scrollbar::Scrollbars::overlay(&bars))
                    .into_any_element()
            }
            ViewNode::JpegImage { id, width, height, jpeg, label } => {
                let image = self.images.entry(*id).or_insert_with(|| {
                    cx.new(|cx| super::image::ImageView::new(jpeg, *width, *height, label, cx))
                });
                if image.read(cx).bytes != *jpeg {
                    *image =
                        cx.new(|cx| super::image::ImageView::new(jpeg, *width, *height, label, cx));
                }
                let image = image.clone();
                image.update(cx, |image, cx| {
                    if image.width != *width || image.height != *height || image.label != *label {
                        image.width = *width;
                        image.height = *height;
                        image.label = label.clone();
                        cx.notify();
                    }
                });
                image.into_any_element()
            }
            ViewNode::ContextMenu { items, child } => {
                let content = self.render_node(child, theme, window, cx, inline);
                let items = items.clone();
                let activate = self.activate.clone();
                if self.context_index == self.context_ids.len() {
                    self.context_ids.push(cx.new(|_| ()));
                }
                let id = gpui::accesskit::NodeId(
                    self.context_ids[self.context_index]
                        .entity_id()
                        .as_u64()
                        .checked_mul(2)
                        .expect("context accessibility identity exhausted"),
                );
                self.context_index += 1;
                let accessible_items = items.clone();
                let accessible_activate = activate.clone();
                div()
                    .relative()
                    .when(contains_scroll(child), |v| {
                        v.flex().flex_col().flex_1().min_w(gpui::px(0.)).min_h(gpui::px(0.))
                    })
                    .child(content)
                    .child(gpui::canvas(
                        move |bounds, window, _| {
                            use gpui::accesskit::{Action, Node, Rect, Role};
                            let mut node = Node::new(Role::Button);
                            node.set_label("Open context menu");
                            node.set_bounds(Rect::new(bounds.left().to_f64(), bounds.top().to_f64(),
                                bounds.right().to_f64(), bounds.bottom().to_f64()));
                            node.add_action(Action::Click);
                            let items = accessible_items.clone();
                            let activate = accessible_activate.clone();
                            window.insert_accessibility_node(id, node, false,
                                Some(Rc::new(move |request: &gpui::accesskit::ActionRequest, window: &mut Window, cx: &mut App| {
                                    if request.action == Action::Click {
                                        #[cfg(target_os = "macos")]
                                        {
                                            let menu = super::popup::Request::context(window,
                                                bounds.origin, items.clone(), activate.clone(), cx);
                                            cx.defer(move |cx| super::popup::open(menu, cx));
                                        }
                                    }
                                })));
                        }, |_, _, _, _| {},
                    ).absolute().size_full())
                    .on_mouse_down(MouseButton::Right, move |event: &gpui::MouseDownEvent, window, cx| {
                        window.prevent_default();
                        #[cfg(target_os = "macos")]
                        {
                            let request = super::popup::Request::context(
                                window, event.position, items.clone(), activate.clone(), cx,
                            );
                            cx.defer(move |cx| super::popup::open(request, cx));
                        }
                        #[cfg(not(target_os = "macos"))]
                        panic!("Context menus require a platform menu presenter");
                    })
                    .into_any_element()
            }
            ViewNode::Column(nodes) | ViewNode::List(nodes) | ViewNode::Row(nodes) => {
                let horizontal = matches!(node, ViewNode::Row(_));
                let children = nodes
                    .iter()
                    .map(|n| self.render_node(n, theme, window, cx, horizontal))
                    .collect::<Vec<_>>();
                div()
                    .flex()
                    .when(!horizontal, |v| v.flex_col())
                    .when(contains_scroll(node), |v| {
                        v.flex_1().min_w(gpui::px(0.)).min_h(gpui::px(0.))
                    })
                    .gap(zoom::pixels(window, if horizontal { 8. } else { 12. }))
                    .children(children)
                    .into_any_element()
            }
            ViewNode::Text { style, text } => {
                if self.text_index == self.text_ids.len() {
                    self.text_ids.push(cx.new(|_| ()));
                }
                let id =
                    text_editor::accessibility_text_ids(self.text_ids[self.text_index].entity_id())
                        .0;
                self.text_index += 1;
                use playbit_api::ui::TextStyle;
                let title = *style == TextStyle::Title;
                let (size, line, weight, tracking) = match style {
                    TextStyle::Title => (24., 28., gpui::FontWeight::MEDIUM, 0.),
                    TextStyle::Body => (14., 20., gpui::FontWeight::NORMAL, 0.),
                    TextStyle::Muted | TextStyle::Label => {
                        (11., 16., gpui::FontWeight::MEDIUM, 0.11)
                    }
                    TextStyle::SectionTitle => (14., 16., gpui::FontWeight::SEMIBOLD, 0.),
                    TextStyle::LabelLarge => (18., 20., gpui::FontWeight::BOLD, 0.),
                };
                let text = text.clone();
                let content = text.clone().into_any_element();
                div()
                    .relative()
                    .text_size(zoom::pixels(window, size))
                    .line_height(zoom::pixels(window, line))
                    .font_weight(weight)
                    .letter_spacing(zoom::scaled(window, tracking))
                    .text_color(rgba(theme.color(text_role(*style)).0))
                    .child(content)
                    .child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                window.insert_accessibility_node(
                                    id,
                                    text_accessibility_node(&text, title, bounds),
                                    false,
                                    None,
                                );
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .into_any_element()
            }
            ViewNode::TextField { id, value, placeholder, disabled, read_only, .. } => {
                let id = *id;
                let (change, commit) = text_delivery(id, self.send.clone());
                let editor = self.editors.entry(id).or_insert_with(|| {
                    cx.new(|cx| {
                        text_editor::TextEditor::new(value.clone(), cx)
                            .placeholder(SharedString::from(placeholder.clone()))
                            .on_edit(change)
                            .on_commit(commit)
                    })
                });
                editor.update(cx, |editor, cx| {
                    editor.set_theme(theme);
                    editor.set_enabled(!disabled);
                    editor.set_read_only(*read_only || !self.active, theme, cx);
                    editor.set_placeholder(placeholder);
                });
                // The runtime refuses input aimed at a disabled field, so an
                // editor here would only let someone type and then be told no.
                if *disabled {
                    use playbit_api::ui::ThemeColor as Role;
                    let node_id = text_editor::accessibility_text_ids(editor.entity_id()).0;
                    let value = value.clone();
                    let placeholder = placeholder.clone();
                    let field = div()
                        .relative()
                        .h(zoom::pixels(window, 24.))
                        .when(inline, |v| v.w(zoom::pixels(window, 117.333)).flex_none())
                        .when(!inline, |v| v.min_w(zoom::pixels(window, 300.)).w_full());
                    return field
                        .child(
                            div()
                                .h(zoom::pixels(window, 24.))
                                .w_full()
                                .flex()
                                .items_center()
                                .px(zoom::pixels(window, 4.))
                                .rounded(zoom::pixels(window, 2.))
                                .border(zoom::pixels(window, 0.5))
                                .border_color(
                                    gpui::Hsla::from(rgba(theme.color(Role::Text).0)).alpha(0.1),
                                )
                                .bg(rgba(theme.color(Role::ControlDisabled).0))
                                .text_color(
                                    gpui::Hsla::from(rgba(theme.color(Role::Text).0)).alpha(0.3),
                                )
                                .text_size(zoom::pixels(window, 11.))
                                .line_height(zoom::pixels(window, 16.))
                                .letter_spacing(zoom::scaled(window, UI_CONTROL_TRACKING))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_ellipsis()
                                .child(if value.is_empty() {
                                    placeholder.clone()
                                } else {
                                    value.clone()
                                }),
                        )
                        .child(
                            gpui::canvas(
                                move |bounds, window, _| {
                                    let mut node = gpui::accesskit::Node::new(
                                        gpui::accesskit::Role::TextInput,
                                    );
                                    node.set_disabled();
                                    node.set_value(value);
                                    node.set_placeholder(placeholder);
                                    node.set_bounds(gpui::accesskit::Rect::new(
                                        bounds.left().to_f64(),
                                        bounds.top().to_f64(),
                                        bounds.right().to_f64(),
                                        bounds.bottom().to_f64(),
                                    ));
                                    window.insert_accessibility_node(node_id, node, false, None);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .into_any_element();
                }

                div()
                    .h(zoom::pixels(window, 24.))
                    .when(inline, |v| v.w(zoom::pixels(window, 117.333)).flex_none())
                    .when(!inline, |v| v.min_w(zoom::pixels(window, 300.)).w_full())
                    .child(editor.clone())
                    .into_any_element()
            }
            ViewNode::SegmentedControl { id, segments, selected, disabled } => {
                let activate = self.activate.clone();
                self.segments
                    .entry(*id)
                    .or_insert_with(|| {
                        cx.new(|cx| {
                            segmented::Segmented::new(
                                segments,
                                *selected,
                                *disabled || !self.active,
                                theme,
                                activate,
                                cx,
                            )
                        })
                    })
                    .clone()
                    .into_any_element()
            }
            ViewNode::Checkbox { id, label, state, disabled } => self.control(
                *id,
                controls::Kind::Checkbox(*state),
                label,
                *disabled,
                theme,
                window,
                cx,
            ),
            ViewNode::Button { id, label, disabled, style } => self.control(
                *id,
                controls::Kind::Button(*style),
                label,
                *disabled,
                theme,
                window,
                cx,
            ),
            ViewNode::PopupButton { id, label, items, disabled } => {
                let properties = controls::Properties {
                    kind: controls::Kind::Popup,
                    label,
                    disabled: *disabled || !self.active,
                    theme,
                };
                let activate = self.activate.clone();
                let control = self.controls.entry(*id).or_insert_with(|| {
                    cx.new(|cx| controls::Control::new(*id, properties, activate, cx))
                });
                control.update(cx, |control, cx| control.set_popup_items(items, cx));
                controls::element(control, window, cx)
            }
            ViewNode::Canvas { id, width, height, clear_color, items, .. } => {
                if self.canvas_index == self.canvas_images.len() {
                    self.canvas_images.push(cx.new(super::canvas_images::CanvasImages::new));
                }
                let images = self.canvas_images[self.canvas_index].clone();
                self.canvas_index += 1;
                // Retire replaced pixels even when the canvas is currently clipped.
                images.update(cx, |images, _| images.prepare(items, window));
                let (id, width, height) = (*id, *width, *height);
                let send = self.send.clone();
                let pointer = self.pointer.clone();
                let active = self.active;
                canvas_element(
                    id,
                    (width, height),
                    *clear_color,
                    items.clone(),
                    images,
                    f32::from(window.rem_size()) / 16.,
                    move |data| {
                        // A release over host chrome reaches every canvas's
                        // mouse-up-out listener. Only its press owner may send it.
                        if !active || (data.phase != 1 && pointer.get() != Some(id)) {
                            return;
                        }
                        let pressed = data.buttons == 1 && data.phase < 2;
                        if send(Input {
                            kind: EventKind::Pointer,
                            target: id,
                            data: data.encode().to_vec(),
                            revision: None,
                        }) {
                            pointer.set(pressed.then_some(id));
                        }
                    },
                )
            }
            _ => div().child("This control is not supported by this host").into_any_element(),
        }
    }
    fn update_controls(&mut self, view: &ViewNode, theme: &playbit_api::ui::Theme, cx: &mut App) {
        let properties = match view {
            ViewNode::Theme { theme, child, .. } => {
                self.update_controls(child, theme, cx);
                return;
            }
            ViewNode::ContextMenu { child, .. } | ViewNode::Scroll { child, .. } => {
                self.update_controls(child, theme, cx);
                return;
            }
            ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
                for child in children {
                    self.update_controls(child, theme, cx);
                }
                return;
            }
            ViewNode::TextField { id, disabled, read_only, .. } => {
                if let Some(editor) = self.editors.get(id) {
                    editor.update(cx, |editor, cx| {
                        editor.set_enabled(!disabled);
                        editor.set_read_only(*read_only || !self.active, theme, cx);
                    });
                }
                return;
            }
            ViewNode::SegmentedControl { id, segments, selected, disabled } => {
                if let Some(group) = self.segments.get(id) {
                    group.update(cx, |group, cx| {
                        group.set(segments, *selected, *disabled || !self.active, theme, cx)
                    });
                }
                return;
            }
            ViewNode::Button { id, style, label, disabled } => (
                *id,
                controls::Properties {
                    kind: controls::Kind::Button(*style),
                    label,
                    disabled: *disabled || !self.active,
                    theme,
                },
            ),
            ViewNode::Checkbox { id, state, label, disabled } => (
                *id,
                controls::Properties {
                    kind: controls::Kind::Checkbox(*state),
                    label,
                    disabled: *disabled || !self.active,
                    theme,
                },
            ),
            ViewNode::PopupButton { id, label, disabled, .. } => (
                *id,
                controls::Properties {
                    kind: controls::Kind::Popup,
                    label,
                    disabled: *disabled || !self.active,
                    theme,
                },
            ),
            _ => return,
        };
        if let Some(control) = self.controls.get(&properties.0) {
            control.update(cx, |control, cx| control.set(properties.1, cx));
        }
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Keep explicit presentation inputs beside the window and UI context."
    )]
    fn control(
        &mut self,
        id: u32,
        kind: controls::Kind,
        label: &str,
        disabled: bool,
        theme: &playbit_api::ui::Theme,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let properties =
            controls::Properties { kind, label, disabled: disabled || !self.active, theme };
        let control = self.control_entity(id, properties, cx);
        controls::element(control, window, cx)
    }
    fn control_entity(
        &mut self,
        id: u32,
        properties: controls::Properties<'_>,
        cx: &mut App,
    ) -> &Entity<controls::Control> {
        let activate = self.activate.clone();
        self.controls
            .entry(id)
            .or_insert_with(|| cx.new(|cx| controls::Control::new(id, properties, activate, cx)))
    }
}

fn text_delivery(
    id: u32,
    send: SendInput,
) -> (text_editor::EditCallback, text_editor::CommitCallback) {
    let change_send = send.clone();
    (
        Arc::new(move |text, edits, revision, _| {
            let Ok(data) = playbit_api::abi::text::TextInput::encode(text, edits) else {
                return false;
            };
            change_send(Input {
                kind: EventKind::TextChanged,
                target: id,
                data,
                revision: Some(revision),
            })
        }),
        Arc::new(move |text, revision, _| {
            crate::trace::mark("ui.input.submit");
            send(Input {
                kind: EventKind::Submit,
                target: id,
                data: text.as_bytes().to_vec(),
                revision: Some(revision),
            })
        }),
    )
}

// Only branches containing a viewport need to pass available space down the layout tree.
fn contains_scroll(node: &ViewNode) -> bool {
    match node {
        ViewNode::Scroll { .. } => true,
        ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => {
            contains_scroll(child)
        }
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().any(contains_scroll)
        }
        _ => false,
    }
}

/// The theme role a guest text style reads its color from. Muted is the guest's
/// own secondary prose, so it follows whatever the view's theme sets there.
pub(crate) fn text_role(style: playbit_api::ui::TextStyle) -> playbit_api::ui::ThemeColor {
    match style {
        playbit_api::ui::TextStyle::Muted => playbit_api::ui::ThemeColor::TextSecondary,
        _ => playbit_api::ui::ThemeColor::Text,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InventoryKind {
    Text,
    Control,
    Segments,
    SegmentChild,
    Scroll,
    Image,
}
fn control_ids(view: &ViewNode) -> Result<Vec<(u32, InventoryKind)>, &'static str> {
    fn collect(
        view: &ViewNode,
        ids: &mut Vec<(u32, InventoryKind)>,
        labels: &mut usize,
    ) -> Result<(), &'static str> {
        match view {
            ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => {
                collect(child, ids, labels)?
            }
            ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
                for child in children {
                    collect(child, ids, labels)?;
                }
            }
            ViewNode::Scroll { id, child, .. } => {
                if ids.len() >= controls::MAX_CONTROLS
                    || ids.iter().filter(|(_, kind)| *kind == InventoryKind::Scroll).count() >= 16
                {
                    return Err("App view exceeds the 16-scroll-container limit.");
                }
                ids.push((*id, InventoryKind::Scroll));
                collect(child, ids, labels)?;
            }
            ViewNode::JpegImage { id, .. } => {
                if ids.len() >= controls::MAX_CONTROLS
                    || ids.iter().filter(|(_, kind)| *kind == InventoryKind::Image).count() >= 4
                {
                    return Err("App view exceeds the four-image limit.");
                }
                ids.push((*id, InventoryKind::Image));
            }
            ViewNode::SegmentedControl { id, segments, .. } => {
                if ids.len() + 1 + segments.len() > controls::MAX_CONTROLS {
                    return Err("App view exceeds the 256-control limit.");
                }
                ids.push((*id, InventoryKind::Segments));
                for segment in segments {
                    *labels = labels
                        .checked_add(segment.label.len())
                        .ok_or("App control labels are too large.")?;
                    if *labels > controls::MAX_LABEL_BYTES {
                        return Err("App control labels exceed 1 MiB.");
                    }
                    ids.push((segment.id, InventoryKind::SegmentChild));
                }
            }
            ViewNode::Button { id, label, .. }
            | ViewNode::Checkbox { id, label, .. }
            | ViewNode::PopupButton { id, label, .. } => {
                if ids.len() == controls::MAX_CONTROLS {
                    return Err("App view exceeds the 256-control limit.");
                }
                *labels =
                    labels.checked_add(label.len()).ok_or("App control labels are too large.")?;
                if *labels > controls::MAX_LABEL_BYTES {
                    return Err("App control labels exceed 1 MiB.");
                }
                ids.push((*id, InventoryKind::Control));
            }
            ViewNode::TextField { id, .. } => {
                if ids.len() == controls::MAX_CONTROLS {
                    return Err("App view exceeds the 256-control limit.");
                }
                ids.push((*id, InventoryKind::Text));
            }
            _ => {}
        }
        Ok(())
    }
    let mut ids = Vec::new();
    collect(view, &mut ids, &mut 0)?;
    Ok(ids)
}

fn field_enabled(view: &ViewNode, id: u32) -> bool {
    match view {
        ViewNode::TextField { id: target, disabled, .. } if *target == id => !disabled,
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => field_enabled(child, id),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().any(|child| field_enabled(child, id))
        }
        _ => false,
    }
}

fn field_editable(view: &ViewNode, id: u32) -> bool {
    match view {
        ViewNode::TextField { id: target, disabled, read_only, .. } if *target == id => {
            !disabled && !read_only
        }
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => field_editable(child, id),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().any(|child| field_editable(child, id))
        }
        _ => false,
    }
}

fn field_value(view: &ViewNode, id: u32) -> Option<&str> {
    match view {
        ViewNode::TextField { id: target, value, .. } if *target == id => Some(value),
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => field_value(child, id),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().find_map(|child| field_value(child, id))
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod control_inventory_tests {
    use super::*;
    fn button(id: u32, label: String) -> ViewNode {
        ViewNode::Button {
            id,
            label,
            style: playbit_api::ui::ButtonStyle::Default,
            disabled: false,
        }
    }
    #[test]
    fn decoded_image_inventory_is_bounded_before_starting_jobs() {
        let images = |count| {
            ViewNode::Column(
                (1..=count)
                    .map(|id| ViewNode::JpegImage {
                        id,
                        width: 512,
                        height: 341,
                        jpeg: b"jpeg".to_vec(),
                        label: "Photo".into(),
                    })
                    .collect(),
            )
        };
        assert!(control_ids(&images(4)).is_ok());
        assert!(control_ids(&images(5)).is_err());
    }
    #[test]
    fn inventory_bounds_state_and_distinguishes_replaced_control_kinds() {
        let view = ViewNode::Column((1..=256).map(|id| button(id, "A".into())).collect());
        assert_eq!(control_ids(&view).unwrap().len(), 256);
        let ViewNode::Column(mut children) = view else { unreachable!() };
        children.push(button(257, "B".into()));
        assert!(control_ids(&ViewNode::Column(children)).is_err());
        assert_eq!(control_ids(&button(1, "A".into())).unwrap(), [(1, InventoryKind::Control)]);
        let field = ViewNode::TextField {
            id: 1,
            value: String::new(),
            placeholder: String::new(),
            multiline: false,
            disabled: false,
            read_only: false,
        };
        assert_eq!(control_ids(&field).unwrap(), [(1, InventoryKind::Text)]);
        assert!(control_ids(&button(1, "a".repeat(controls::MAX_LABEL_BYTES + 1))).is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod rendering_tests {
    use super::*;
    use gpui::px;
    #[test]
    fn static_text_exposes_content_without_input_actions() {
        use gpui::accesskit::{Action, Role};
        let bounds =
            gpui::Bounds::new(gpui::point(px(16.), px(32.)), gpui::size(px(240.), px(24.)));
        for title in [false, true] {
            let node = text_accessibility_node("Café 👩🏽‍💻 — 2 actions", title, bounds);
            assert_eq!(node.role(), if title { Role::Heading } else { Role::Label });
            assert_eq!(
                if title { node.label() } else { node.value() },
                Some("Café 👩🏽‍💻 — 2 actions")
            );
            assert_eq!(node.bounds().unwrap(), gpui::accesskit::Rect::new(16., 32., 256., 56.));
            assert!(node.is_read_only());
            assert!(!node.supports_action(Action::Focus));
            assert!(!node.supports_action(Action::Click));
            assert!(!node.supports_action(Action::SetValue));
        }
    }
    #[test]
    fn muted_text_reads_the_secondary_role_and_follows_the_guest_theme() {
        use playbit_api::ui::{Color, TextStyle, Theme, ThemeColor};
        let mut theme = Theme::light();
        assert_eq!(text_role(TextStyle::Muted), ThemeColor::TextSecondary);
        assert_ne!(
            theme.color(text_role(TextStyle::Muted)),
            theme.color(text_role(TextStyle::Body))
        );
        for style in [TextStyle::Body, TextStyle::Title, TextStyle::Label, TextStyle::SectionTitle]
        {
            assert_eq!(text_role(style), ThemeColor::Text, "{style:?}");
        }
        theme.set_color(ThemeColor::TextSecondary, Color(0x3366ccff));
        assert_eq!(theme.color(text_role(TextStyle::Muted)), Color(0x3366ccff));
    }
    struct TestScene {
        scene: Scene,
        view: ViewNode,
    }
    impl gpui::Render for TestScene {
        fn render(
            &mut self,
            window: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            self.scene.render(Some(&self.view), window, cx)
        }
    }
    #[gpui::test]
    fn packed_canvas_frames_replace_images_and_release_removed_surfaces(
        cx: &mut gpui::TestAppContext,
    ) {
        // Repeated frame replacement retains one image, and removing the canvas retires its cache.
        let view = |red| ViewNode::Canvas {
            id: 7,
            width: 32,
            height: 16,
            frames: playbit_api::ui::CanvasFrames::Continuous,
            extent: playbit_api::ui::CanvasExtent::Inline,
            clear_color: None,
            items: vec![crate::runtime::CanvasItem::Pixels {
                bounds: playbit_api::ui::CanvasRect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 16,
                    color: playbit_api::ui::Color(0),
                },
                source_width: 2,
                source_height: 1,
                rgba: [red, 0, 0, 255, 0, 255, 0, 255].into(),
            }],
        };
        let window = cx.add_window(|_, _| TestScene {
            scene: Scene::new(Arc::new(|_| true), Rc::new(|_, _| {})),
            view: view(0),
        });
        cx.run_until_parked();
        let cache =
            window.update(cx, |owner, _, _| owner.scene.canvas_images[0].downgrade()).unwrap();
        for red in 1..=32 {
            let old = window
                .update(cx, |owner, _, cx| {
                    let images = owner.scene.canvas_images[0].read(cx);
                    assert!(images.image(1).is_none());
                    Arc::downgrade(&images.image(0).expect("pixel frame painted"))
                })
                .unwrap();
            window
                .update(cx, |owner, _, cx| {
                    owner.view = view(red);
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
            assert!(old.upgrade().is_none());
        }
        window
            .update(cx, |owner, _, cx| {
                owner.view = ViewNode::Column(Vec::new());
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        assert!(cache.upgrade().is_none());
    }
    #[gpui::test]
    fn text_identities_survive_value_changes_and_release_removed_nodes(
        cx: &mut gpui::TestAppContext,
    ) {
        let text = |value: &str| ViewNode::Text {
            style: playbit_api::ui::TextStyle::Body,
            text: value.into(),
        };
        let window = cx.add_window(|_, _| TestScene {
            scene: Scene::new(Arc::new(|_| true), Rc::new(|_, _| {})),
            view: ViewNode::Column(vec![text("First"), text("Second")]),
        });
        cx.run_until_parked();
        let before = window
            .update(cx, |owner, _, _| {
                owner.scene.text_ids.iter().map(Entity::downgrade).collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(before.len(), 2);
        window
            .update(cx, |owner, _, cx| {
                owner.view = ViewNode::Column(vec![text("Changed")]);
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |owner, _, _| {
                assert_eq!(owner.scene.text_ids.len(), 1);
                assert_eq!(owner.scene.text_ids[0].entity_id(), before[0].entity_id());
            })
            .unwrap();
        assert!(before[0].upgrade().is_some());
        assert!(before[1].upgrade().is_none());
    }

    #[gpui::test]
    fn scroll_viewports_follow_container_space_and_resize(cx: &mut gpui::TestAppContext) {
        // Check real layout and scroll extents through roots, both axes and nested containers.
        let canvas = |width, height| ViewNode::Canvas {
            id: 0,
            width,
            height,
            frames: playbit_api::ui::CanvasFrames::Static,
            extent: playbit_api::ui::CanvasExtent::Inline,
            clear_color: None,
            items: vec![],
        };
        let scroll =
            |id, child| ViewNode::Scroll { id, complex_background: false, child: Box::new(child) };
        let window = cx.add_window(|_, _| TestScene {
            scene: Scene::new(Arc::new(|_| true), Rc::new(|_, _| {})),
            view: scroll(1, canvas(1000, 800)),
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);

        for (view, reserved, columns) in [
            (scroll(1, canvas(1000, 800)), gpui::size(px(0.), px(0.)), 1.),
            (ViewNode::Column(vec![scroll(1, canvas(1000, 800))]), gpui::size(px(0.), px(0.)), 1.),
            (ViewNode::Row(vec![scroll(1, canvas(1000, 800))]), gpui::size(px(0.), px(0.)), 1.),
            (
                ViewNode::Column(vec![ViewNode::Row(vec![scroll(1, canvas(1000, 800))])]),
                gpui::size(px(0.), px(0.)),
                1.,
            ),
            (scroll(2, scroll(1, canvas(1000, 800))), gpui::size(px(0.), px(0.)), 1.),
            (
                ViewNode::Column(vec![canvas(80, 40), scroll(1, canvas(1000, 800))]),
                gpui::size(px(0.), px(52.)),
                1.,
            ),
            (
                ViewNode::Row(vec![canvas(80, 40), scroll(1, canvas(1000, 800))]),
                gpui::size(px(88.), px(0.)),
                1.,
            ),
            (
                ViewNode::Row(vec![scroll(1, canvas(1000, 800)), scroll(2, canvas(1000, 800))]),
                gpui::size(px(8.), px(0.)),
                2.,
            ),
        ] {
            window
                .update(cx, |owner, _, cx| {
                    owner.view = view;
                    cx.notify();
                })
                .unwrap();
            for size in [gpui::size(px(400.), px(300.)), gpui::size(px(600.), px(420.))] {
                visual.simulate_resize(size);
                cx.run_until_parked();
                let handle = window
                    .update(cx, |owner, _, cx| owner.scene.scrolls[&1].read(cx).handle())
                    .unwrap();
                let viewport = gpui::size(
                    (size.width - reserved.width) / columns,
                    size.height - reserved.height,
                );
                assert_eq!(handle.bounds().size, viewport);
                assert_eq!(
                    handle.max_offset(),
                    gpui::size(px(1000.) - viewport.width, px(800.) - viewport.height)
                );
                visual.simulate_event(gpui::ScrollWheelEvent {
                    position: handle.bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-40.))),
                    ..Default::default()
                });
                assert!(handle.offset().y < px(0.));
            }
        }

        window
            .update(cx, |owner, _, cx| {
                owner.view = scroll(1, canvas(100, 80));
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |owner, _, cx| {
                let handle = owner.scene.scrolls[&1].read(cx).handle();
                assert_eq!(handle.bounds().size, gpui::size(px(600.), px(420.)));
                assert_eq!(handle.max_offset(), gpui::size(px(0.), px(0.)));
                assert_eq!(handle.offset(), gpui::point(px(0.), px(0.)));
            })
            .unwrap();
    }
}
