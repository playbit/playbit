//! One focus stop and one single-selection group, independent of guest execution.
use super::{controls, zoom};
use crate::runtime::SegmentEntry;
use gpui::{
    AlignSelf, App, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, KeyDownEvent,
    MouseButton, Rgba, SharedString, Subscription, Window, accesskit, canvas, div, prelude::*, px,
    rgba,
};
use playbit_api::ui::{Theme, ThemeColor};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

const TRANSITION_DURATION: Duration = Duration::from_millis(240);
mod compact;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Highlight {
    left: f32,
    right: f32,
    radius: f32,
}

#[derive(Clone, Copy)]
struct Transition {
    from: Highlight,
    to: Highlight,
    start: Instant,
}
impl Transition {
    fn sample(self, now: Instant) -> Highlight {
        let t = (now.saturating_duration_since(self.start).as_secs_f32()
            / TRANSITION_DURATION.as_secs_f32())
        .min(1.);
        // Figma 4487:8835: normalize the seven illustrated edge positions.
        // The leading edge extends first, then the trailing edge catches up.
        const TRAILING: [f32; 7] = [0., 5. / 35., 11. / 35., 15. / 35., 26. / 35., 33. / 35., 1.];
        const LEADING: [f32; 7] = [0., 21. / 85., 55. / 85., 76. / 85., 1., 1., 1.];
        const ROUNDING: [f32; 7] = [0., 1. / 3., 2. / 3., 1., 2. / 3., 1. / 3., 0.];
        let step = (t * 6.).floor() as usize;
        let next = (step + 1).min(6);
        let fraction = t * 6. - step as f32;
        let curve = |values: [f32; 7]| values[step] + (values[next] - values[step]) * fraction;
        let forward = self.to.left + self.to.right >= self.from.left + self.from.right;
        let (left, right) = if forward {
            (curve(TRAILING), curve(LEADING))
        } else {
            (curve(LEADING), curve(TRAILING))
        };
        Highlight {
            left: self.from.left + (self.to.left - self.from.left) * left,
            right: self.from.right + (self.to.right - self.from.right) * right,
            radius: self.from.radius
                + (self.to.radius - self.from.radius) * t
                + (10. - self.from.radius.max(self.to.radius)).max(0.) * curve(ROUNDING),
        }
    }
}

pub struct Segmented {
    segments: Vec<SegmentEntry>,
    selected: u32,
    disabled: bool,
    theme: Theme,
    focus: FocusHandle,
    show_focus: bool,
    reveal_focus: bool,
    pressed: Option<u32>,
    hovered: Option<u32>,
    identities: Vec<Entity<()>>,
    subscriptions: Option<[Subscription; 2]>,
    activate: controls::Activate,
    highlight: Option<(u32, Highlight)>,
    transition: Option<Transition>,
    compact: Option<[&'static str; 2]>,
}
impl Segmented {
    pub fn new(
        segments: &[SegmentEntry],
        selected: u32,
        disabled: bool,
        theme: &Theme,
        activate: controls::Activate,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            segments: segments.to_vec(),
            selected,
            disabled,
            theme: theme.clone(),
            focus: cx.focus_handle().tab_stop(!disabled && segments.iter().any(|s| !s.disabled)),
            show_focus: false,
            reveal_focus: false,
            pressed: None,
            hovered: None,
            identities: segments.iter().map(|_| cx.new(|_| ())).collect(),
            subscriptions: None,
            activate,
            highlight: None,
            transition: None,
            compact: None,
        }
    }
    pub fn set(
        &mut self,
        segments: &[SegmentEntry],
        selected: u32,
        disabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) {
        if self.segments == segments
            && self.selected == selected
            && self.disabled == disabled
            && &self.theme == theme
        {
            return;
        }
        if self.segments != segments || disabled {
            self.highlight = None;
            self.transition = None;
            self.pressed = None;
            self.hovered = None;
        }
        self.segments = segments.to_vec();
        self.selected = selected;
        self.disabled = disabled;
        self.theme = theme.clone();
        self.focus = self.focus.clone().tab_stop(!disabled && segments.iter().any(|s| !s.disabled));
        while self.identities.len() < segments.len() {
            self.identities.push(cx.new(|_| ()));
        }
        self.identities.truncate(segments.len());
        cx.notify();
    }
    pub fn retire(&mut self) {
        self.disabled = true;
        self.transition = None;
        self.pressed = None;
    }
    /// Present the two existing choices as one small in-field toggle.
    pub fn set_compact(&mut self, labels: [&'static str; 2], cx: &mut Context<Self>) {
        assert_eq!(self.segments.len(), 2, "compact representation has two choices");
        if self.compact != Some(labels) {
            self.compact = Some(labels);
            self.highlight = None;
            self.transition = None;
            cx.notify();
        }
    }
    pub fn cancel_pointer_press(&mut self, cx: &mut Context<Self>) -> bool {
        let canceled = self.pressed.take().is_some();
        if canceled {
            cx.notify();
        }
        canceled
    }
    pub fn show_keyboard_focus_if_focused(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.disabled || !self.focus.is_focused(window) {
            return false;
        }
        self.show_focus = true;
        self.reveal_focus = true;
        cx.notify();
        true
    }
    fn choose(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.disabled
            || self.selected == id
            || !self.segments.iter().any(|s| s.id == id && !s.disabled)
        {
            return;
        }
        #[cfg(target_os = "macos")]
        if super::popup::is_modal_active() {
            return;
        }
        self.selected = id;
        (self.activate)(id, cx);
        cx.notify();
    }
    fn move_selection(&mut self, direction: isize, cx: &mut Context<Self>) {
        let Some(index) = self.segments.iter().position(|s| s.id == self.selected) else {
            return;
        };
        let mut next = index as isize + direction;
        while next >= 0 && (next as usize) < self.segments.len() {
            let segment = &self.segments[next as usize];
            if !segment.disabled {
                self.choose(segment.id, cx);
                break;
            }
            next += direction;
        }
    }
    fn release(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.pressed.take() == Some(id) {
            self.choose(id, cx);
        }
        cx.notify();
    }
}
impl Focusable for Segmented {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for Segmented {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.subscriptions.is_none() {
            self.subscriptions = Some([
                cx.on_focus(&self.focus, window, |this, _, cx| {
                    this.show_focus = true;
                    cx.notify();
                }),
                cx.on_blur(&self.focus, window, |this, _, cx| {
                    this.show_focus = false;
                    this.pressed = None;
                    cx.notify();
                }),
            ]);
        }
        if let Some(labels) = self.compact {
            return self.render_compact(labels, window, cx);
        }
        let sp = |v| zoom::pixels(window, v);
        let focused = !self.disabled && self.show_focus && self.focus.is_focused(window);
        let base = rgba(self.theme.color(ThemeColor::WindowBackground).0);
        // Figma Plus lighter: add 13% white to the 8%-black group surface.
        // Semantic scenes place controls on their theme's window background.
        let selected_fill = Rgba {
            r: (base.r * 0.92 + 0.13).min(1.),
            g: (base.g * 0.92 + 0.13).min(1.),
            b: (base.b * 0.92 + 0.13).min(1.),
            a: 1.,
        };
        let mut style = window.text_style();
        style.font_family = super::UI_FONT_FAMILY.into();
        style.font_weight = FontWeight::MEDIUM;
        style.font_size = sp(11.).into();
        style.line_height = sp(16.).into();
        style.letter_spacing = px(0.);
        let mut children = Vec::with_capacity(self.segments.len());
        let mut x = 2.;
        let mut target = None;
        for (index, segment) in self.segments.iter().enumerate() {
            let id = segment.id;
            let disabled = self.disabled || segment.disabled;
            let pressed = !disabled && self.pressed == Some(id) && self.hovered == Some(id);
            let label: SharedString = segment.label.clone().into();
            let width = match window.text_system().shape_text(
                label.clone(),
                sp(11.),
                &[style.to_run(label.len())],
                None,
                None,
            ) {
                Ok(lines) => {
                    lines.iter().fold(px(0.), |w, line| w.max(line.unwrapped_layout.width))
                }
                Err(error) => {
                    crate::diagnostic!("Cannot shape segment label: {error}");
                    px(0.)
                }
            };
            // Figma intrinsic text boxes round up to dp; origins and final boxes
            // are then snapped to physical pixels at the current app zoom.
            let scale = zoom::factor(cx);
            let logical_width = (f32::from(width) / scale).ceil() + 16.;
            let width = sp(logical_width);
            if self.selected == id {
                target =
                    Some(Highlight { left: x, right: x + f32::from(width) / scale, radius: 4. });
            }
            x += f32::from(width + sp(4.)) / scale;
            let mut child = div()
                .id(("segment", id as u64))
                .relative()
                .flex_none()
                .w(width)
                .h(sp(20.))
                .rounded(sp(4.))
                .flex()
                .items_center()
                .justify_center()
                .text_center()
                .px(sp(8.))
                .text_color(Hsla::from(rgba(self.theme.color(ThemeColor::Text).0)).alpha(
                    if disabled {
                        0.3
                    } else if pressed {
                        0.7
                    } else {
                        0.9
                    },
                ))
                .when(pressed, |v| v.bg(rgba(0x00000026)))
                .child(label);
            if !disabled {
                child = child
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            window.prevent_default();
                            this.pressed = Some(id);
                            this.hovered = Some(id);
                            this.show_focus = false;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.release(id, cx)),
                    )
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.hovered = Some(id);
                        } else if this.hovered == Some(id) {
                            this.hovered = None;
                        }
                        if this.pressed.is_some() {
                            cx.notify();
                        }
                    }));
            }
            let entity = cx.entity().downgrade();
            let node_id = accesskit::NodeId(self.identities[index].entity_id().as_u64() * 2);
            child = child.child(
                canvas(
                    move |bounds, window, cx| {
                        let Some(entity) = entity.upgrade() else { return };
                        entity.update(cx, |this, cx| {
                            use accesskit::{Action, Node, Rect, Role, Toggled};
                            let Some(segment) = this.segments.get(index) else { return };
                            let disabled = this.disabled || segment.disabled;
                            let mut node = Node::new(Role::RadioButton);
                            node.set_label(segment.label.clone());
                            node.set_toggled(if this.selected == id {
                                Toggled::True
                            } else {
                                Toggled::False
                            });
                            node.set_selected(this.selected == id);
                            node.set_bounds(Rect::new(
                                bounds.left().to_f64(),
                                bounds.top().to_f64(),
                                bounds.right().to_f64(),
                                bounds.bottom().to_f64(),
                            ));
                            if disabled {
                                node.set_disabled();
                            } else {
                                node.add_action(Action::Click);
                                node.add_action(Action::Focus);
                            }
                            let focused =
                                !disabled && this.selected == id && this.focus.is_focused(window);
                            window.insert_accessibility_node(
                                node_id,
                                node,
                                focused,
                                Some(Rc::new(cx.listener(
                                    move |this, request: &accesskit::ActionRequest, window, cx| {
                                        if this.disabled
                                            || !this
                                                .segments
                                                .iter()
                                                .any(|s| s.id == id && !s.disabled)
                                        {
                                            return;
                                        }
                                        match request.action {
                                            Action::Click => this.choose(id, cx),
                                            Action::Focus => {
                                                this.focus.focus(window);
                                                this.show_focus = true;
                                                this.reveal_focus = true;
                                                cx.notify();
                                            }
                                            _ => {}
                                        }
                                    },
                                ))),
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
            children.push(child);
        }
        let now = Instant::now();
        let mut highlight = target;
        #[cfg(target_os = "macos")]
        if self.highlight.is_some_and(|(id, _)| id != self.selected)
            && super::appearance::reduce_motion()
        {
            self.highlight = None;
            self.transition = None;
        }
        if let Some(target) = target {
            match self.highlight {
                Some((id, old)) if id != self.selected && !self.disabled => {
                    let from = self.transition.map_or(old, |motion| motion.sample(now));
                    self.transition = Some(Transition { from, to: target, start: now });
                }
                Some((_, old)) if old != target => self.transition = None,
                _ => {}
            }
            self.highlight = Some((self.selected, target));
            if let Some(motion) = self.transition {
                if now.saturating_duration_since(motion.start) < TRANSITION_DURATION {
                    highlight = Some(motion.sample(now));
                    window.request_animation_frame();
                } else {
                    self.transition = None;
                }
            }
        } else {
            self.highlight = None;
            self.transition = None;
        }
        let selected_pressed =
            self.pressed == Some(self.selected) && self.hovered == Some(self.selected);
        let scale = zoom::factor(cx);
        let background = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                if let Some(highlight) = highlight.filter(|_| !selected_pressed) {
                    let display = window.scale_factor();
                    let snap = |v: gpui::Pixels| px((f32::from(v) * display).round() / display);
                    let left = snap(bounds.left() + px(highlight.left * scale));
                    let right = snap(bounds.left() + px(highlight.right * scale));
                    let top = snap(bounds.top() + px(2. * scale));
                    window.paint_quad(
                        gpui::fill(
                            gpui::Bounds::new(
                                gpui::point(left, top),
                                gpui::size(right - left, snap(px(20. * scale))),
                            ),
                            selected_fill,
                        )
                        .corner_radii(snap(px(highlight.radius * scale))),
                    );
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let ids = self
            .identities
            .iter()
            .map(|e| accesskit::NodeId(e.entity_id().as_u64() * 2))
            .collect::<Vec<_>>();
        let group_id = accesskit::NodeId(cx.entity_id().as_u64() * 2);
        let entity = cx.entity().downgrade();
        let mut root = div()
            .id("segmented-control")
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .h(sp(24.))
            .p(sp(2.))
            .gap(sp(4.))
            .rounded(sp(6.))
            .bg(rgba(0x00000014))
            .font_family(super::UI_FONT_FAMILY)
            .font_weight(FontWeight::MEDIUM)
            .text_size(sp(11.))
            .line_height(sp(16.))
            .letter_spacing(px(0.))
            .track_focus(&self.focus)
            .child(background)
            .children(children)
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cancel_pointer_press(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cancel_pointer_press(cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.disabled || event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "left" => this.move_selection(-1, cx),
                    "right" => this.move_selection(1, cx),
                    "escape" => {
                        this.cancel_pointer_press(cx);
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let Some(entity) = entity.upgrade() else { return };
                        entity.update(cx, |this, cx| {
                            use accesskit::{Action, Node, Rect, Role};
                            if std::mem::take(&mut this.reveal_focus) {
                                window.request_autoscroll(bounds.dilate(zoom::pixels(window, 8.)));
                            }
                            let mut node = Node::new(Role::RadioGroup);
                            node.set_children(ids.clone());
                            node.set_bounds(Rect::new(
                                bounds.left().to_f64(),
                                bounds.top().to_f64(),
                                bounds.right().to_f64(),
                                bounds.bottom().to_f64(),
                            ));
                            if this.disabled {
                                node.set_disabled();
                            } else {
                                node.add_action(Action::Focus);
                            }
                            window.insert_accessibility_node(
                                group_id,
                                node,
                                false,
                                Some(Rc::new(cx.listener(
                                    |this, request: &accesskit::ActionRequest, window, cx| {
                                        if !this.disabled && request.action == Action::Focus {
                                            this.focus.focus(window);
                                            this.show_focus = true;
                                            this.reveal_focus = true;
                                            cx.notify();
                                        }
                                    },
                                ))),
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        root.style().align_self = Some(AlignSelf::FlexStart);
        if focused {
            root = root.child(
                div()
                    .absolute()
                    .top(-sp(1.))
                    .left(-sp(1.))
                    .right(-sp(1.))
                    .bottom(-sp(1.))
                    .rounded(sp(7.))
                    .border(sp(2.))
                    .border_color(rgba(0x000000ff)),
            );
        }
        root
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert expected segmented control operations.")]
mod tests {
    use super::*;
    use gpui::{AnyWindowHandle, TestAppContext};
    #[test]
    fn highlight_matches_figma_storyboard_and_settles() {
        let start = Instant::now();
        let from = Highlight { left: 60., right: 91., radius: 4. };
        let to = Highlight { left: 95., right: 176., radius: 4. };
        let motion = Transition { from, to, start };
        let frames = [
            (60., 31., 4.),
            (65., 47., 6.),
            (71., 75., 8.),
            (75., 92., 10.),
            (86., 90., 8.),
            (93., 83., 6.),
            (95., 81., 4.),
        ];
        for (i, (left, width, radius)) in frames.into_iter().enumerate() {
            let frame = motion.sample(start + Duration::from_millis(i as u64 * 40));
            assert!((frame.left - left).abs() < 0.001);
            assert!((frame.right - frame.left - width).abs() < 0.001);
            assert!((frame.radius - radius).abs() < 0.001);
        }
        assert_eq!(motion.sample(start + Duration::from_secs(2)), to);
    }
    #[test]
    fn interrupted_reverse_transition_starts_from_current_shape() {
        let start = Instant::now();
        let from = Highlight { left: 2., right: 58., radius: 4. };
        let to = Highlight { left: 97., right: 178., radius: 4. };
        let motion = Transition { from, to, start };
        let interrupted = start + Duration::from_millis(90);
        let current = motion.sample(interrupted);
        let reverse = Transition { from: current, to: from, start: interrupted };
        assert_eq!(reverse.sample(interrupted), current);
        for ms in 0..=240 {
            let frame = reverse.sample(interrupted + Duration::from_millis(ms));
            assert!(frame.right > frame.left);
            assert!(frame.left >= from.left && frame.right <= to.right);
            assert!((4. ..=10.).contains(&frame.radius));
        }
        assert_eq!(reverse.sample(interrupted + TRANSITION_DURATION), from);
    }
    fn choices() -> Vec<SegmentEntry> {
        vec![
            SegmentEntry { id: 2, label: "Slow".into(), disabled: false },
            SegmentEntry { id: 3, label: "Medium".into(), disabled: true },
            SegmentEntry { id: 4, label: "Fast".into(), disabled: false },
        ]
    }
    #[gpui::test]
    fn selection_skips_disabled_stops_at_edges_and_preserves_group_focus(cx: &mut TestAppContext) {
        let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let received = calls.clone();
        let handle = cx.add_window(|window, cx| {
            let group = Segmented::new(
                &choices(),
                2,
                false,
                &Theme::light(),
                Rc::new(move |id, _| received.borrow_mut().push(id)),
                cx,
            );
            group.focus.focus(window);
            group
        });
        handle
            .update(cx, |group, window, cx| {
                group.move_selection(1, cx);
                assert_eq!(group.selected, 4);
                assert!(group.focus.is_focused(window));
                group.move_selection(1, cx);
                assert_eq!(calls.borrow().as_slice(), [4]);
                group.move_selection(-1, cx);
                assert_eq!(group.selected, 2);
                group.move_selection(-1, cx);
                assert_eq!(calls.borrow().as_slice(), [4, 2]);
                group.choose(3, cx);
                assert_eq!(group.selected, 2);
                group.set(&choices(), 2, true, &Theme::light(), cx);
                group.move_selection(1, cx);
                assert_eq!(calls.borrow().as_slice(), [4, 2]);
            })
            .unwrap();
    }
    #[gpui::test]
    fn pointer_release_requires_same_enabled_segment_and_cancels_on_update(
        cx: &mut TestAppContext,
    ) {
        let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let received = calls.clone();
        let handle = cx.add_window(|_, cx| {
            Segmented::new(
                &choices(),
                2,
                false,
                &Theme::light(),
                Rc::new(move |id, _| received.borrow_mut().push(id)),
                cx,
            )
        });
        handle
            .update(cx, |group, _, cx| {
                group.pressed = Some(2);
                group.release(4, cx);
                group.release(4, cx); // Unarmed release.
                assert!(calls.borrow().is_empty());
                group.pressed = Some(4);
                group.release(4, cx);
                assert_eq!(calls.borrow().as_slice(), [4]);
                group.pressed = Some(2);
                group.cancel_pointer_press(cx);
                group.release(2, cx);
                assert_eq!(calls.borrow().as_slice(), [4]);
                group.pressed = Some(2);
                group.set(&choices(), 4, true, &Theme::light(), cx);
                group.release(2, cx);
                assert_eq!(calls.borrow().as_slice(), [4]);
            })
            .unwrap();
    }
    #[gpui::test]
    fn accessible_group_has_one_selected_child_and_disables_actions(cx: &mut TestAppContext) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let handle = cx.add_window(|_, cx| {
            Segmented::new(&choices(), 2, false, &Theme::light(), Rc::new(|_, _| {}), cx)
        });
        let window: AnyWindowHandle = handle.into();
        window
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                assert!(window.accessibility_error().is_none());
                let nodes = window
                    .accessibility_nodes_for_test()
                    .map(|(_, n)| n.clone())
                    .collect::<Vec<_>>();
                let group = nodes.iter().find(|n| n.role() == accesskit::Role::RadioGroup).unwrap();
                assert_eq!(group.children().len(), 3);
                let rows = nodes
                    .iter()
                    .filter(|n| n.role() == accesskit::Role::RadioButton)
                    .collect::<Vec<_>>();
                assert_eq!(rows.len(), 3);
                assert_eq!(rows.iter().filter(|n| n.is_selected() == Some(true)).count(), 1);
                let disabled = rows.iter().find(|n| n.label() == Some("Medium")).unwrap();
                assert!(disabled.is_disabled());
                assert!(!disabled.supports_action(accesskit::Action::Click));
            })
            .unwrap();
    }
}
