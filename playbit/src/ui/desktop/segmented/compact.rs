//! The same two-choice owner/focus contract in a compact in-field presentation.
use super::*;

impl Segmented {
    fn compact_target(&self) -> Option<u32> {
        (!self.disabled)
            .then(|| {
                self.segments
                    .iter()
                    .find(|segment| segment.id != self.selected && !segment.disabled)
                    .map(|segment| segment.id)
            })
            .flatten()
    }

    pub(super) fn render_compact(
        &mut self,
        labels: [&'static str; 2],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let sp = |value| zoom::pixels(window, value);
        let target = self.compact_target();
        let index = self
            .segments
            .iter()
            .position(|segment| segment.id == self.selected)
            .expect("compact selection belongs to its choices");
        let focused = !self.disabled && self.show_focus && self.focus.is_focused(window);
        let weak = cx.weak_entity();
        let root =
            div()
                .id("compact-segmented-control")
                .relative()
                .size(sp(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(sp(2.))
                .font_family(super::super::UI_FONT_FAMILY)
                .font_weight(FontWeight::MEDIUM)
                .text_size(sp(10.))
                .line_height(sp(16.))
                .text_color(
                    Hsla::from(rgba(self.theme.color(ThemeColor::Text).0))
                        .alpha(if target.is_some() { 0.7 } else { 0.3 }),
                )
                .when(self.pressed.is_some() && self.hovered == self.pressed, |button| {
                    button.bg(rgba(0x00000014))
                })
                .when(focused, |button| button.border(sp(2.)).border_color(rgba(0x000000ff)))
                .track_focus(&self.focus)
                .child(labels[index]);
        self.compact_input(root, cx).child(
            canvas(
                move |bounds, window, cx| {
                    let _ =
                        weak.update(cx, |this, cx| this.compact_accessibility(bounds, window, cx));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    fn compact_input(
        &self,
        root: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        root.on_any_mouse_down(|_, window, _| window.prevent_default())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.pressed = this.compact_target();
                    this.hovered = this.pressed;
                    this.show_focus = false;
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(id) = this.pressed {
                        this.release(id, cx);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cancel_pointer_press(cx);
                }),
            )
            .on_hover(cx.listener(|this, hovered, _, cx| {
                this.hovered = if *hovered { this.compact_target() } else { None };
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event, _, cx| this.compact_key(event, cx)))
    }

    fn compact_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.disabled || event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "left" => self.move_selection(-1, cx),
            "right" => self.move_selection(1, cx),
            "space" | "enter" => {
                if let Some(id) = self.compact_target() {
                    self.choose(id, cx);
                }
            }
            "escape" => {
                self.cancel_pointer_press(cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn compact_accessibility(
        &mut self,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use accesskit::{Action, Node, Rect, Role};
        let rect = Rect::new(
            bounds.left().to_f64(),
            bounds.top().to_f64(),
            bounds.right().to_f64(),
            bounds.bottom().to_f64(),
        );
        if std::mem::take(&mut self.reveal_focus) {
            window.request_autoscroll(bounds);
        }
        let ids: Vec<_> = self
            .identities
            .iter()
            .map(|entity| accesskit::NodeId(entity.entity_id().as_u64() * 2))
            .collect();
        for index in 0..self.segments.len() {
            self.compact_choice(index, rect, window, cx);
        }
        let mut node = Node::new(Role::RadioGroup);
        node.set_label("Representation");
        node.set_children(ids);
        node.set_bounds(rect);
        if self.disabled {
            node.set_disabled();
        } else {
            node.add_action(Action::Focus);
        }
        window.insert_accessibility_node(
            accesskit::NodeId(cx.entity_id().as_u64() * 2),
            node,
            false,
            Some(Rc::new(cx.listener(|this, request: &accesskit::ActionRequest, window, cx| {
                if !this.disabled && request.action == Action::Focus {
                    this.compact_focus(window, cx);
                }
            }))),
        );
    }

    fn compact_choice(
        &mut self,
        index: usize,
        rect: accesskit::Rect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use accesskit::{Action, Node, Role, Toggled};
        let segment = &self.segments[index];
        let id = segment.id;
        let disabled = self.disabled || segment.disabled;
        let mut node = Node::new(Role::RadioButton);
        node.set_label(segment.label.clone());
        node.set_toggled(if self.selected == id { Toggled::True } else { Toggled::False });
        node.set_selected(self.selected == id);
        node.set_bounds(rect);
        if disabled {
            node.set_disabled();
        } else {
            node.add_action(Action::Click);
            node.add_action(Action::Focus);
        }
        window.insert_accessibility_node(
            accesskit::NodeId(self.identities[index].entity_id().as_u64() * 2),
            node,
            !disabled && self.selected == id && self.focus.is_focused(window),
            Some(Rc::new(cx.listener(
                move |this, request: &accesskit::ActionRequest, window, cx| {
                    if this.disabled || !this.segments.iter().any(|s| s.id == id && !s.disabled) {
                        return;
                    }
                    match request.action {
                        Action::Click => this.choose(id, cx),
                        Action::Focus => this.compact_focus(window, cx),
                        _ => {}
                    }
                },
            ))),
        );
    }

    fn compact_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        self.show_focus = true;
        self.reveal_focus = true;
        cx.notify();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise real compact input and accessible choices.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn compact_representation_preserves_choices_focus_and_disabled_admission(
        cx: &mut gpui::TestAppContext,
    ) {
        // Click and key the same retained entity, then disable Text and all editing in turn.
        let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let received = calls.clone();
        let choices = [
            SegmentEntry { id: 0, label: "Text".into(), disabled: false },
            SegmentEntry { id: 1, label: "Binary".into(), disabled: false },
        ];
        let window = cx.add_window(|window, cx| {
            let mut control = Segmented::new(
                &choices,
                0,
                false,
                &Theme::light(),
                Rc::new(move |id, _| received.borrow_mut().push(id)),
                cx,
            );
            control.set_compact(["T", "0x"], cx);
            window.activate_window();
            control.focus.focus(window);
            control
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        let point = visual.update(|window, _| {
            assert!(window.accessibility_error().is_none());
            let group = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.role() == accesskit::Role::RadioGroup)
                .unwrap()
                .1;
            let bounds = group.bounds().unwrap();
            assert_eq!((bounds.width(), bounds.height()), (24., 24.));
            assert_eq!(group.children().len(), 2);
            gpui::point(px((bounds.x0 + 12.) as f32), px((bounds.y0 + 12.) as f32))
        });
        visual.simulate_click(point, Default::default());
        assert_eq!(calls.borrow().as_slice(), [1]);
        visual.simulate_keystrokes("left right");
        assert_eq!(calls.borrow().as_slice(), [1, 0, 1]);
        let mut restricted = choices.clone();
        restricted[0].disabled = true;
        for disabled in [false, true] {
            window
                .update(&mut visual, |control, window, cx| {
                    assert!(control.focus.is_focused(window));
                    control.set(&restricted, 1, disabled, &Theme::light(), cx);
                })
                .unwrap();
            visual.run_until_parked();
            visual.simulate_click(point, Default::default());
            visual.simulate_keystrokes("left");
            assert_eq!(calls.borrow().as_slice(), [1, 0, 1]);
        }
    }
}
