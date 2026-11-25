//! Desktop scrollbars sharing the viewport's existing GPUI scroll state.
use super::zoom;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Entity, HitboxBehavior, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle, Size, Subscription,
    Task, Timer, Window, accesskit, canvas, fill, point, prelude::*, px, rgba, size,
};
use std::time::{Duration, Instant};

const VISIBLE_FOR: Duration = Duration::from_secs(2);
const FADE_FOR: Duration = Duration::from_millis(200);
const RAIL_CLEARANCE: f32 = 8.;
const RAIL_MIN_CENTER: f32 = 8.;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    start: f32,
    length: f32,
    thumb_start: f32,
    thumb_length: f32,
    maximum: f32,
}
impl Geometry {
    fn new(
        extent: f32,
        viewport: f32,
        maximum: f32,
        offset: f32,
        inset: f32,
        end: f32,
        minimum: f32,
    ) -> Option<Self> {
        if maximum <= 0. || viewport <= 0. || extent <= inset + end {
            return None;
        }
        let length = extent - inset - end;
        let stretch = offset - offset.clamp(-maximum, 0.);
        let thumb_length =
            (length * viewport / (viewport + maximum) - stretch.abs()).max(minimum).min(length);
        let thumb_start = inset + (-offset / maximum).clamp(0., 1.) * (length - thumb_length);
        Some(Self { start: inset, length, thumb_start, thumb_length, maximum })
    }
    fn offset(self, pointer: f32, grab: f32) -> f32 {
        let travel = self.length - self.thumb_length;
        if travel <= 0. {
            return 0.;
        }
        -((pointer - grab - self.start) / travel).clamp(0., 1.) * self.maximum
    }
}
#[derive(Clone, Copy)]
struct Drag {
    axis: usize,
    grab: f32,
}

pub struct Scrollbars {
    handle: ScrollHandle,
    snapshot: Option<(Bounds<Pixels>, Size<Pixels>, Point<Pixels>)>,
    visible_until: Instant,
    hovered: bool,
    drag: Option<Drag>,
    hide: Option<Task<()>>,
    complex: bool,
    parent_corner_radius: f32,
    owner: Option<AnyWindowHandle>,
    activation: Option<Subscription>,
    identities: Option<[Entity<()>; 2]>,
    source: crate::trace::SpanId,
}
impl Scrollbars {
    pub fn new(complex_background: bool) -> Self {
        Self::with_corner_radius(complex_background, 0.)
    }

    /// Include a rounded parent whose clipping is outside the tracked viewport.
    pub fn with_corner_radius(complex_background: bool, parent_corner_radius: f32) -> Self {
        let handle = ScrollHandle::new();
        #[cfg(target_os = "macos")]
        {
            handle.enable_elastic_scroll();
            handle.reserve_permanent_scrollbar_gutter(
                zoom::length(parent_corner_radius.max(RAIL_MIN_CENTER)),
                zoom::length(RAIL_CLEARANCE),
            );
        }
        Self {
            handle,
            snapshot: None,
            visible_until: Instant::now(),
            hovered: false,
            drag: None,
            hide: None,
            complex: complex_background,
            parent_corner_radius,
            owner: None,
            activation: None,
            identities: None,
            source: Default::default(),
        }
    }
    pub fn set_complex_background(&mut self, complex: bool) {
        self.complex = complex;
    }
    pub fn handle(&self) -> ScrollHandle {
        self.handle.clone()
    }

    fn rail_center(&self, window: &Window) -> Pixels {
        self.handle
            .corner_radius()
            .max(zoom::pixels(window, self.parent_corner_radius.max(RAIL_MIN_CENTER)))
    }

    /// Gutter width for a viewport with square corners whose content is measured manually.
    pub(crate) fn gutter_width(&self, window: &Window, cx: &App) -> Pixels {
        if cfg!(target_os = "macos") && !cx.should_auto_hide_scrollbars() {
            self.rail_center(window) + zoom::pixels(window, RAIL_CLEARANCE)
        } else {
            px(0.)
        }
    }
    fn opacity(&self, now: Instant, always: bool) -> f32 {
        if always || self.hovered || self.drag.is_some() {
            return 1.;
        }
        1. - (now.saturating_duration_since(self.visible_until).as_secs_f32()
            / FADE_FOR.as_secs_f32())
        .min(1.)
    }
    fn reveal(&mut self, cx: &mut Context<Self>) {
        self.visible_until = Instant::now() + VISIBLE_FOR;
        self.hide = Some(cx.spawn(async move |this, cx| {
            Timer::after(VISIBLE_FOR).await;
            let owner = this
                .update(cx, |this, cx| {
                    cx.notify();
                    this.owner
                })
                .ok()
                .flatten();
            if let Some(owner) = owner {
                let _ = owner.update(cx, |_, window, _| window.refresh());
            }
        }));
    }
    fn observe(&mut self, cx: &mut Context<Self>) {
        let snapshot =
            (self.handle.bounds(), self.handle.max_offset(), self.handle.visual_offset());
        if self.snapshot != Some(snapshot) {
            self.snapshot = Some(snapshot);
            self.reveal(cx);
        }
    }
    /// Add after the scrollable child, outside its content mask and offset.
    pub fn overlay(entity: &Entity<Self>) -> impl IntoElement {
        let before = entity.downgrade();
        let paint = entity.downgrade();
        canvas(
            move |bounds, window, cx| {
                let Some(entity) = before.upgrade() else { return Vec::new() };
                entity.update(cx, |this, cx| {
                    this.owner = Some(window.window_handle());
                    if this.activation.is_none() {
                        this.activation =
                            Some(cx.observe_window_activation(window, |this, window, cx| {
                                if !window.is_window_active() {
                                    this.drag = None;
                                    this.hovered = false;
                                    cx.notify();
                                    window.refresh();
                                }
                            }));
                    }
                    this.observe(cx);
                    let identities = this
                        .identities
                        .get_or_insert_with(|| [cx.new(|_| ()), cx.new(|_| ())])
                        .clone();
                    let max = this.handle.max_offset();
                    let offsets = this.handle.visual_offset();
                    let always = !cx.should_auto_hide_scrollbars();
                    let expanded = always || this.hovered || this.drag.is_some();
                    let mut rails = Vec::with_capacity(2);
                    let viewport = this.handle.bounds().size;
                    for axis in 0..2 {
                        let (extent, view, maximum, offset, other) = if axis == 0 {
                            (bounds.size.width, viewport.width, max.width, offsets.x, max.height)
                        } else {
                            (bounds.size.height, viewport.height, max.height, offsets.y, max.width)
                        };
                        let center = this.rail_center(window);
                        let half = zoom::pixels(window, if expanded { 7. } else { 4. });
                        let inset = center - half;
                        let end =
                            if other > px(0.) { center + zoom::pixels(window, 7.) } else { inset };
                        let Some(geometry) = Geometry::new(
                            extent.into(),
                            if always && cfg!(target_os = "macos") { view } else { extent }.into(),
                            maximum.into(),
                            offset.into(),
                            inset.into(),
                            end.into(),
                            zoom::pixels(window, 24.).into(),
                        ) else {
                            continue;
                        };
                        let thickness = center + zoom::pixels(window, RAIL_CLEARANCE);
                        let hit_bounds = if axis == 0 {
                            Bounds::new(
                                point(bounds.left(), bounds.bottom() - thickness),
                                size(
                                    bounds.size.width
                                        - if other > px(0.) { thickness } else { px(0.) },
                                    thickness,
                                ),
                            )
                        } else {
                            Bounds::new(
                                point(bounds.right() - thickness, bounds.top()),
                                size(
                                    thickness,
                                    bounds.size.height
                                        - if other > px(0.) { thickness } else { px(0.) },
                                ),
                            )
                        };
                        rails.push((
                            axis,
                            geometry,
                            window.insert_hitbox(
                                hit_bounds,
                                if this.opacity(Instant::now(), always) > 0. {
                                    HitboxBehavior::BlockMouseExceptScroll
                                } else {
                                    HitboxBehavior::Normal
                                },
                            ),
                        ));
                    }
                    for (axis, geometry, hit) in &rails {
                        let id = accesskit::NodeId(identities[*axis].entity_id().as_u64() * 2);
                        let mut node = accesskit::Node::new(accesskit::Role::ScrollBar);
                        node.set_label(if *axis == 0 {
                            "Horizontal scroll"
                        } else {
                            "Vertical scroll"
                        });
                        node.set_orientation(if *axis == 0 {
                            accesskit::Orientation::Horizontal
                        } else {
                            accesskit::Orientation::Vertical
                        });
                        node.set_min_numeric_value(0.);
                        node.set_max_numeric_value(1.);
                        let offset = if *axis == 0 { offsets.x } else { offsets.y };
                        node.set_numeric_value(
                            (-f32::from(offset) / geometry.maximum).clamp(0., 1.) as f64,
                        );
                        node.set_numeric_value_step(0.1);
                        node.set_bounds(accesskit::Rect::new(
                            hit.left().to_f64(),
                            hit.top().to_f64(),
                            hit.right().to_f64(),
                            hit.bottom().to_f64(),
                        ));
                        for action in [
                            accesskit::Action::Increment,
                            accesskit::Action::Decrement,
                            accesskit::Action::SetValue,
                        ] {
                            node.add_action(action);
                        }
                        let axis = *axis;
                        window.insert_accessibility_node(
                            id,
                            node,
                            false,
                            Some(std::rc::Rc::new(cx.listener(
                                move |this, request: &accesskit::ActionRequest, window, cx| {
                                    if modal() {
                                        return;
                                    }
                                    let max = this.handle.max_offset();
                                    let offset = this.handle.offset();
                                    let (maximum, current) = if axis == 0 {
                                        (f32::from(max.width), f32::from(offset.x))
                                    } else {
                                        (f32::from(max.height), f32::from(offset.y))
                                    };
                                    let value = match request.action {
                                        accesskit::Action::Increment => current - maximum * 0.1,
                                        accesskit::Action::Decrement => current + maximum * 0.1,
                                        accesskit::Action::SetValue => match request.data {
                                            Some(accesskit::ActionData::NumericValue(v))
                                                if v.is_finite() =>
                                            {
                                                -(v.clamp(0., 1.) as f32) * maximum
                                            }
                                            _ => return,
                                        },
                                        _ => return,
                                    };
                                    this.scroll(axis, value.clamp(-maximum, 0.), window, cx);
                                },
                            ))),
                        );
                    }
                    rails
                })
            },
            move |bounds, rails, window, cx| {
                let Some(entity) = paint.upgrade() else { return };
                entity.update(cx, |this, cx| {
                    if this.source != Default::default() {
                        crate::trace::mark_related(
                            "ui.scrollbar.scene.input",
                            std::mem::take(&mut this.source),
                        );
                    }
                    let always = !cx.should_auto_hide_scrollbars();
                    let expanded = always || this.hovered || this.drag.is_some();
                    let now = Instant::now();
                    let opacity = this.opacity(now, always);
                    if !expanded && now >= this.visible_until && opacity > 0. {
                        window.request_animation_frame();
                    }
                    if opacity > 0. {
                        for (axis, geometry, _) in &rails {
                            let thickness = zoom::pixels(window, if expanded { 14. } else { 8. });
                            let center = this.rail_center(window);
                            let cross = center + thickness / 2.;
                            let display = window.scale_factor();
                            let rect = |start: f32, length: f32| {
                                let snap = |v: f32| px((v * display).round() / display);
                                if *axis == 0 {
                                    Bounds::new(
                                        point(
                                            snap(f32::from(bounds.left()) + start),
                                            bounds.bottom() - cross,
                                        ),
                                        size(snap(length), thickness),
                                    )
                                } else {
                                    Bounds::new(
                                        point(
                                            bounds.right() - cross,
                                            snap(f32::from(bounds.top()) + start),
                                        ),
                                        size(thickness, snap(length)),
                                    )
                                }
                            };
                            if this.hovered || this.drag.is_some() {
                                let color =
                                    if this.complex { rgba(0x99999966) } else { rgba(0x0000001a) };
                                let mut track = rect(geometry.start, geometry.length);
                                let border =
                                    if this.complex { zoom::pixels(window, 1.) } else { px(0.) };
                                track.origin.x -= border;
                                track.origin.y -= border;
                                track.size.width += border * 2.;
                                track.size.height += border * 2.;
                                window.paint_quad(
                                    fill(track, color).corner_radii(thickness / 2. + border),
                                );
                            }
                            let alpha = if this.drag.is_some_and(|d| d.axis == *axis) {
                                0xe6
                            } else if this.hovered || this.drag.is_some() {
                                0xb3
                            } else if this.complex {
                                0x99
                            } else {
                                0x80
                            };
                            let mut thumb = fill(
                                rect(geometry.thumb_start, geometry.thumb_length),
                                gpui::Hsla::from(rgba(alpha)).opacity(opacity),
                            )
                            .corner_radii(thickness / 2.);
                            if this.complex {
                                thumb = thumb.border_widths(zoom::pixels(window, 1.)).border_color(
                                    gpui::Hsla::from(rgba(if expanded {
                                        0xffffff12
                                    } else {
                                        0xffffff4d
                                    }))
                                    .opacity(opacity),
                                );
                            }
                            window.paint_quad(thumb);
                        }
                    }
                    let down = cx.weak_entity();
                    let down_rails = rails.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                        if !phase.bubble() || event.button != MouseButton::Left || modal() {
                            return;
                        }
                        if let Some((axis, geometry, _)) =
                            down_rails.iter().find(|(_, _, hit)| hit.is_hovered(window))
                        {
                            let _ = down.update(cx, |this, cx| {
                                if this.opacity(Instant::now(), !cx.should_auto_hide_scrollbars())
                                    == 0.
                                {
                                    return;
                                }
                                let pointer = coordinate(event.position, bounds, *axis);
                                let grab = if pointer >= geometry.thumb_start
                                    && pointer <= geometry.thumb_start + geometry.thumb_length
                                {
                                    pointer - geometry.thumb_start
                                } else {
                                    geometry.thumb_length / 2.
                                };
                                this.drag = Some(Drag { axis: *axis, grab });
                                this.scroll(*axis, geometry.offset(pointer, grab), window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                            });
                        }
                    });
                    let movement = cx.weak_entity();
                    let move_rails = rails;
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if modal() {
                            return;
                        }
                        let _ = movement.update(cx, |this, cx| {
                            // An armed scrollbar owns dragging until mouse-up, including
                            // outside its viewport. Capture before other controls can consume it.
                            if (this.drag.is_some() && !phase.capture())
                                || (this.drag.is_none() && !phase.bubble())
                            {
                                return;
                            }
                            let interactive = this
                                .opacity(Instant::now(), !cx.should_auto_hide_scrollbars())
                                > 0.;
                            let hovered = interactive
                                && move_rails.iter().any(|(_, _, hit)| hit.is_hovered(window));
                            if this.hovered != hovered {
                                this.hovered = hovered;
                                window.refresh();
                                this.reveal(cx);
                                cx.notify();
                            }
                            if let Some(drag) = this.drag {
                                if event.pressed_button != Some(MouseButton::Left) {
                                    // GPUI also synthesizes unpressed moves after layout.
                                    // Only mouse-up (or deactivation) releases pointer capture.
                                    return;
                                }
                                if let Some((_, g, _)) =
                                    move_rails.iter().find(|(a, _, _)| *a == drag.axis)
                                {
                                    this.scroll(
                                        drag.axis,
                                        g.offset(
                                            coordinate(event.position, bounds, drag.axis),
                                            drag.grab,
                                        ),
                                        window,
                                        cx,
                                    );
                                    cx.stop_propagation();
                                }
                            }
                        });
                    });
                    let exit = cx.weak_entity();
                    window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
                        if !phase.bubble() {
                            return;
                        }
                        let _ = exit.update(cx, |this, cx| {
                            if this.hovered {
                                this.hovered = false;
                                this.reveal(cx);
                                window.refresh();
                                cx.notify();
                            }
                        });
                    });
                    let up = cx.weak_entity();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if !phase.capture() || event.button != MouseButton::Left {
                            return;
                        }
                        let _ = up.update(cx, |this, cx| {
                            if this.drag.take().is_some() {
                                window.refresh();
                                this.reveal(cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                        });
                    });
                });
            },
        )
        .absolute()
        .size_full()
        .top_0()
        .left_0()
    }
    fn scroll(&mut self, axis: usize, value: f32, window: &mut Window, cx: &mut Context<Self>) {
        let timing = crate::trace::Span::new("ui.scrollbar.input");
        self.source = timing.id();
        let mut offset = self.handle.offset();
        let value = px((value * window.scale_factor()).round() / window.scale_factor());
        if axis == 0 {
            offset.x = value;
        } else {
            offset.y = value;
        }
        self.handle.set_offset(offset);
        self.reveal(cx);
        cx.notify();
        window.refresh();
    }
}
fn coordinate(point: Point<Pixels>, bounds: Bounds<Pixels>, axis: usize) -> f32 {
    if axis == 0 { (point.x - bounds.left()).into() } else { (point.y - bounds.top()).into() }
}
fn modal() -> bool {
    #[cfg(target_os = "macos")]
    {
        super::popup::is_modal_active()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
#[cfg(test)]
#[path = "scrollbar/focus_tests.rs"]
mod focus_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    struct BarFixture(Entity<Scrollbars>);
    impl gpui::Render for BarFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .relative()
                .w(px(300.))
                .h(px(180.))
                .child(
                    gpui::div()
                        .id("viewport")
                        .size_full()
                        .overflow_scroll()
                        .track_scroll(&self.0.read(cx).handle())
                        .child(
                            gpui::div()
                                .debug_selector(|| "scroll-content".into())
                                .w_full()
                                .h(px(800.))
                                .flex_none(),
                        ),
                )
                .child(Scrollbars::overlay(&self.0))
        }
    }

    struct RoundedFixture {
        bars: Entity<Scrollbars>,
        local_radius: f32,
        horizontal: bool,
    }
    impl gpui::Render for RoundedFixture {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .relative()
                .w(px(300.))
                .h(px(180.))
                .child(
                    gpui::div()
                        .id("rounded-viewport")
                        .size_full()
                        .rounded(zoom::pixels(window, self.local_radius))
                        .overflow_y_scroll()
                        .when(self.horizontal, |viewport| viewport.overflow_x_scroll())
                        .track_scroll(&self.bars.read(cx).handle())
                        .child(
                            gpui::div()
                                .w(px(if self.horizontal { 900. } else { 280. }))
                                .h(px(800.))
                                .flex_none(),
                        ),
                )
                .child(Scrollbars::overlay(&self.bars))
        }
    }
    #[gpui::test]
    fn rails_respect_local_and_parent_corners_at_each_zoom(cx: &mut gpui::TestAppContext) {
        // Both native hit regions follow the visible corner clearance, including a square
        // viewport clipped by a rounded parent, rather than keeping a fixed edge inset.
        cx.update(zoom::install);
        cx.set_auto_hide_scrollbars(false);
        let window = cx.add_window(|_, cx| RoundedFixture {
            bars: cx.new(|_| Scrollbars::new(false)),
            local_radius: 0.,
            horizontal: true,
        });
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).unwrap());
            for (local, parent, horizontal) in [
                (16., 0., true),
                (12., 0., true),
                (8., 0., true),
                (4., 0., true),
                (0., 0., true),
                (0., 16., true),
                (0., 16., false),
            ] {
                window
                    .update(cx, |fixture, window, cx| {
                        fixture.local_radius = local;
                        fixture.bars = cx.new(|_| Scrollbars::with_corner_radius(false, parent));
                        fixture.horizontal = horizontal;
                        window.refresh();
                        cx.notify();
                    })
                    .unwrap();
                cx.run_until_parked();
                window
                    .update(cx, |fixture, window, cx| {
                        let factor = zoom::factor(cx);
                        assert_eq!(
                            fixture.bars.read(cx).handle.corner_radius(),
                            px(local * factor)
                        );
                        let center = local.max(parent).max(8.) * factor;
                        let thickness = center + 8. * factor;
                        let rail = |label| {
                            window
                                .accessibility_nodes_for_test()
                                .find(|(_, node)| node.label() == Some(label))
                                .unwrap()
                                .1
                                .bounds()
                                .unwrap()
                        };
                        let vertical = rail("Vertical scroll");
                        assert_eq!(vertical.x0, f64::from(300. - thickness));
                        let viewport = fixture.bars.read(cx).handle.bounds();
                        assert_eq!(f64::from(viewport.right()), vertical.x0);
                        if horizontal {
                            let horizontal = rail("Horizontal scroll");
                            assert_eq!(horizontal.y0, f64::from(180. - thickness));
                            assert_eq!(f64::from(viewport.bottom()), horizontal.y0);
                            assert_eq!(horizontal.x1, f64::from(300. - thickness));
                            assert_eq!(vertical.y1, f64::from(180. - thickness));
                        } else {
                            assert_eq!(viewport.bottom(), px(180.));
                            assert_eq!(vertical.y1, 180.);
                            assert!(
                                window
                                    .accessibility_nodes_for_test()
                                    .all(|(_, node)| { node.label() != Some("Horizontal scroll") })
                            );
                        }
                    })
                    .unwrap();
            }
        }
    }

    #[gpui::test]
    fn hidden_bars_ignore_hover_and_click_until_scrolling_or_always(cx: &mut gpui::TestAppContext) {
        cx.set_auto_hide_scrollbars(true);
        let window = cx.add_window(|_, cx| BarFixture(cx.new(|_| Scrollbars::new(false))));
        cx.run_until_parked();
        let bars = window.update(cx, |root, _, _| root.0.clone()).unwrap();
        window
            .update(cx, |_, window, cx| {
                bars.update(cx, |bars, _| bars.visible_until = Instant::now() - VISIBLE_FOR);
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let rail = point(px(285.), px(140.));
        assert_eq!(visual.debug_bounds("scroll-content").unwrap().size.width, px(300.));
        visual.simulate_mouse_move(rail, None, Default::default());
        visual.simulate_click(rail, Default::default());
        bars.read_with(cx, |bars, _| {
            assert!(!bars.hovered);
            assert_eq!(bars.handle.offset().y, px(0.));
        });
        window
            .update(cx, |_, window, cx| {
                bars.update(cx, |bars, cx| bars.reveal(cx));
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        visual.simulate_mouse_move(rail, None, Default::default());
        assert!(bars.read_with(cx, |bars, _| bars.hovered));
        visual.simulate_mouse_move(point(px(100.), px(50.)), None, Default::default());
        cx.set_auto_hide_scrollbars(false);
        window
            .update(cx, |_, window, cx| {
                bars.update(cx, |bars, _| bars.visible_until = Instant::now() - VISIBLE_FOR);
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(visual.debug_bounds("scroll-content").unwrap().size.width, px(284.));
        visual.simulate_mouse_move(rail, None, Default::default());
        assert!(
            bars.read_with(cx, |bars, _| bars.hovered),
            "Always bars remain interactive after expiry"
        );
        visual.simulate_click(rail, Default::default());
        let offset = bars.read_with(cx, |bars, _| bars.handle.offset().y);
        assert!(offset < px(-40.));
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: rail,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(40.))),
            ..Default::default()
        });
        assert_eq!(bars.read_with(cx, |bars, _| bars.handle.offset().y), offset + px(40.));
        cx.set_auto_hide_scrollbars(true);
        cx.update(App::refresh_windows);
        cx.run_until_parked();
        assert_eq!(visual.debug_bounds("scroll-content").unwrap().size.width, px(300.));
    }
    #[gpui::test]
    fn dragging_survives_leaving_viewport_and_unpressed_layout_moves(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_window(|_, cx| BarFixture(cx.new(|_| Scrollbars::new(false))));
        cx.run_until_parked();
        let bars = window.update(cx, |root, _, _| root.0.clone()).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(point(px(285.), px(25.)), MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            point(px(600.), px(120.)),
            MouseButton::Left,
            Default::default(),
        );
        bars.read_with(cx, |bars, _| {
            assert!(bars.drag.is_some());
            assert!(!bars.hovered);
            assert!(bars.handle.offset().y < px(0.));
        });
        visual.simulate_mouse_move(point(px(600.), px(120.)), None, Default::default());
        assert!(bars.read_with(cx, |bars, _| bars.drag.is_some()));
        visual.simulate_mouse_up(point(px(600.), px(120.)), MouseButton::Left, Default::default());
        assert!(bars.read_with(cx, |bars, _| bars.drag.is_none()));
    }
    struct AxisFixture {
        columns: Entity<Scrollbars>,
        sidebar: Entity<Scrollbars>,
    }
    impl gpui::Render for AxisFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let sidebar = gpui::div()
                .relative()
                .w(px(240.))
                .h(px(160.))
                .flex_none()
                .child(
                    gpui::div()
                        .id("vertical-sidebar")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.sidebar.read(cx).handle())
                        .child(gpui::div().w(px(240.)).h(px(560.))),
                )
                .child(Scrollbars::overlay(&self.sidebar));
            gpui::div()
                .relative()
                .w(px(240.))
                .h(px(160.))
                .line_height(px(20.))
                .child(
                    gpui::div()
                        .id("horizontal-columns")
                        .size_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.columns.read(cx).handle())
                        .child(gpui::div().w(px(480.)).h(px(160.)).flex().child(sidebar)),
                )
                .child(Scrollbars::overlay(&self.columns))
        }
    }

    #[gpui::test]
    fn horizontal_input_over_a_vertical_sidebar_scrolls_the_enclosing_columns(
        cx: &mut gpui::TestAppContext,
    ) {
        // Exercise real nested hit-testing/routing with both rail overlays: only the
        // indicated axis moves, including when the sidebar is at either vertical edge.
        cx.set_auto_hide_scrollbars(false);
        let window = cx.add_window(|_, cx| AxisFixture {
            columns: cx.new(|_| Scrollbars::new(false)),
            sidebar: cx.new(|_| Scrollbars::new(false)),
        });
        cx.run_until_parked();
        let (columns, sidebar) = window
            .update(cx, |view, _, cx| {
                (view.columns.read(cx).handle(), view.sidebar.read(cx).handle())
            })
            .unwrap();
        assert_eq!(columns.max_offset(), size(px(240.), px(0.)));
        assert_eq!(sidebar.max_offset(), size(px(0.), px(400.)));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let reset = |visual: &mut gpui::VisualTestContext, y| {
            visual.update(|window, cx| {
                columns.set_offset(point(px(-40.), px(0.)));
                sidebar.set_offset(point(px(0.), px(y)));
                window.refresh();
                let _ = window.draw(cx);
            });
        };
        let wheel = |delta| gpui::ScrollWheelEvent {
            position: point(px(100.), px(50.)),
            delta,
            ..Default::default()
        };
        for y in [0., -200., -400.] {
            for direction in [-1., 1.] {
                for lines in [false, true] {
                    reset(&mut visual, y);
                    let delta = if lines {
                        gpui::ScrollDelta::Lines(point(direction, 0.))
                    } else {
                        gpui::ScrollDelta::Pixels(point(px(direction * 20.), px(0.)))
                    };
                    visual.simulate_event(wheel(delta));
                    assert_eq!(columns.offset().x, px(-40. + direction * 20.));
                    assert_eq!(sidebar.offset(), point(px(0.), px(y)));
                    assert_eq!(sidebar.visual_offset(), sidebar.offset());
                }
            }
        }
        reset(&mut visual, -200.);
        let mut vertical = -200.;
        for direction in [-1., 1.] {
            visual.simulate_event(wheel(gpui::ScrollDelta::Pixels(point(
                px(0.),
                px(direction * 20.),
            ))));
            vertical += direction * 20.;
            assert_eq!(sidebar.offset().y, px(vertical));
            assert_eq!(columns.offset().x, px(-40.));
        }
        assert_eq!(sidebar.offset().y, px(-200.), "ordinary vertical scrolling still works");
        // AppKit supplies Shift-wheel as X input; its modifier must not change routing.
        let mut shifted = wheel(gpui::ScrollDelta::Pixels(point(px(-20.), px(0.))));
        shifted.modifiers.shift = true;
        visual.simulate_event(shifted);
        assert_eq!(columns.offset().x, px(-60.));
        assert_eq!(sidebar.offset().y, px(-200.));
        // Outside the sidebar, a one-axis horizontal view keeps its wheel fallback.
        for direction in [-1., 1.] {
            reset(&mut visual, -200.);
            let mut event = wheel(gpui::ScrollDelta::Lines(point(0., direction)));
            event.position.x = px(220.);
            visual.simulate_event(event);
            assert_eq!(columns.offset().x, px(-40. + direction * 20.));
            assert_eq!(sidebar.offset().y, px(-200.));
        }
        #[cfg(target_os = "macos")]
        for y in [0., -200., -400.] {
            for direction in [-1., 1.] {
                reset(&mut visual, y);
                let mut event =
                    wheel(gpui::ScrollDelta::Pixels(point(px(direction * 20.), px(0.))));
                event.is_gesture = true;
                event.touch_phase = gpui::TouchPhase::Started;
                visual.simulate_event(event.clone());
                assert_eq!(columns.offset().x, px(-40. + direction * 20.));
                event.touch_phase = gpui::TouchPhase::Moved;
                event.delta = gpui::ScrollDelta::Pixels(point(px(direction * 10.), px(0.)));
                visual.simulate_event(event.clone());
                event.momentum_phase = Some(gpui::TouchPhase::Started);
                visual.simulate_event(event);
                assert_eq!(columns.offset().x, px(-40. + direction * 40.));
                assert_eq!(sidebar.offset(), point(px(0.), px(y)));
                assert_eq!(sidebar.visual_offset(), sidebar.offset());
            }
        }
    }

    struct FocusFixture {
        outer: ScrollHandle,
        inner: ScrollHandle,
        request: std::rc::Rc<std::cell::Cell<bool>>,
        target: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    }
    impl gpui::Render for FocusFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let request = self.request.clone();
            let target = self.target.clone();
            gpui::div()
                .id("outer")
                .w(px(300.))
                .h(px(180.))
                .overflow_scroll()
                .track_scroll(&self.outer)
                .child(
                    gpui::div().relative().w(px(300.)).h(px(800.)).flex_none().child(
                        gpui::div()
                            .id("inner")
                            .absolute()
                            .top(px(400.))
                            .w(px(280.))
                            .h(px(100.))
                            .overflow_scroll()
                            .track_scroll(&self.inner)
                            .child(
                                gpui::div().relative().w(px(280.)).h(px(400.)).flex_none().child(
                                    canvas(
                                        move |bounds, window, _| {
                                            target.set(bounds);
                                            if request.replace(false) {
                                                window.request_autoscroll(bounds.dilate(px(8.)));
                                            }
                                        },
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .top(px(200.))
                                    .w(px(120.))
                                    .h(px(24.)),
                                ),
                            ),
                    ),
                )
        }
    }
    #[gpui::test]
    fn focus_reveal_is_minimal_and_propagates_through_nested_clips(cx: &mut gpui::TestAppContext) {
        let outer = ScrollHandle::new();
        let inner = ScrollHandle::new();
        let request = std::rc::Rc::new(std::cell::Cell::new(true));
        let target = std::rc::Rc::new(std::cell::Cell::new(Bounds::default()));
        let window = cx.add_window(|_, _| FocusFixture {
            outer: outer.clone(),
            inner: inner.clone(),
            request: request.clone(),
            target: target.clone(),
        });
        cx.run_until_parked();
        assert_eq!(inner.offset().y, px(-132.));
        assert_eq!(outer.offset().y, px(-320.));
        assert_eq!(target.get().top(), px(148.));
        assert_eq!(target.get().bottom(), px(172.));
        window
            .update(cx, |_, window, _| {
                request.set(true);
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(outer.offset().y, px(-320.), "visible focus must not move the viewport");
        assert_eq!(inner.offset().y, px(-132.));
        window
            .update(cx, |_, window, _| {
                inner.set_offset(point(px(0.), px(-300.)));
                outer.set_offset(point(px(0.), px(-620.)));
                request.set(true);
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(inner.offset().y, px(-192.));
        assert_eq!(outer.offset().y, px(-400.));
        assert_eq!(target.get().top(), px(8.));
    }
    #[gpui::test]
    fn wheel_moves_only_deepest_viewport_that_can_reveal_content(cx: &mut gpui::TestAppContext) {
        let outer = ScrollHandle::new();
        let inner = ScrollHandle::new();
        let window = cx.add_window(|_, _| FocusFixture {
            outer: outer.clone(),
            inner: inner.clone(),
            request: std::rc::Rc::new(std::cell::Cell::new(false)),
            target: std::rc::Rc::new(std::cell::Cell::new(Bounds::default())),
        });
        window
            .update(cx, |_, window, _| {
                outer.set_offset(point(px(0.), px(-400.)));
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let wheel = |y| gpui::ScrollWheelEvent {
            position: point(px(100.), px(50.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(y))),
            ..Default::default()
        };
        visual.simulate_event(wheel(-40.));
        assert_eq!(inner.offset().y, px(-40.));
        assert_eq!(outer.offset().y, px(-400.));
        visual.simulate_event(wheel(-1000.));
        assert_eq!(inner.offset().y, px(-300.));
        assert_eq!(outer.offset().y, px(-400.));
        visual.simulate_event(wheel(-40.));
        assert_eq!(inner.offset().y, px(-300.));
        assert_eq!(outer.offset().y, px(-440.));
        visual.simulate_event(wheel(20.));
        assert_eq!(inner.offset().y, px(-280.));
        assert_eq!(outer.offset().y, px(-440.));
    }
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn trackpad_stretches_without_changing_range_and_programmatic_scroll_cancels(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_window(|_, cx| BarFixture(cx.new(|_| Scrollbars::new(false))));
        cx.run_until_parked();
        let bars = window.update(cx, |root, _, _| root.0.clone()).unwrap();
        let handle = bars.read_with(cx, |bars, _| bars.handle());
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let gesture = |y, phase| gpui::ScrollWheelEvent {
            position: point(px(100.), px(50.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(y))),
            touch_phase: phase,
            is_gesture: true,
            ..Default::default()
        };
        visual.simulate_event(gesture(100., gpui::TouchPhase::Started));
        assert_eq!(handle.offset().y, px(0.));
        assert!(handle.visual_offset().y > px(0.));
        let stretched = handle.visual_offset().y;
        visual.simulate_event(gesture(100., gpui::TouchPhase::Moved));
        assert!(handle.visual_offset().y > stretched);
        visual.simulate_event(gesture(-250., gpui::TouchPhase::Moved));
        assert_eq!(handle.elastic_offset().y, px(0.));
        assert!(handle.offset().y < px(0.));
        visual.simulate_event(gesture(1000., gpui::TouchPhase::Moved));
        assert!(handle.elastic_offset().y > px(0.));
        handle.set_offset(point(px(0.), px(-50.)));
        assert_eq!(handle.visual_offset(), handle.offset());
    }
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn trackpad_routes_at_gesture_start_then_keeps_momentum_in_that_view(
        cx: &mut gpui::TestAppContext,
    ) {
        let outer = ScrollHandle::new();
        let inner = ScrollHandle::new();
        outer.enable_elastic_scroll();
        inner.enable_elastic_scroll();
        let window = cx.add_window(|_, _| FocusFixture {
            outer: outer.clone(),
            inner: inner.clone(),
            request: std::rc::Rc::new(std::cell::Cell::new(false)),
            target: std::rc::Rc::new(std::cell::Cell::new(Bounds::default())),
        });
        cx.run_until_parked();
        window
            .update(cx, |_, w, _| {
                outer.set_offset(point(px(0.), px(-400.)));
                inner.set_offset(point(px(0.), px(-300.)));
                w.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let gesture = |y, phase| gpui::ScrollWheelEvent {
            position: point(px(100.), px(50.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(y))),
            is_gesture: true,
            touch_phase: phase,
            ..Default::default()
        };
        visual.simulate_event(gesture(-40., gpui::TouchPhase::Started));
        assert_eq!(
            outer.offset().y,
            px(-440.),
            "an inner edge yields to a parent at gesture start"
        );
        window
            .update(cx, |_, w, _| {
                outer.set_offset(point(px(0.), px(-400.)));
                inner.set_offset(Point::default());
                w.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        visual.simulate_event(gesture(-500., gpui::TouchPhase::Started));
        assert_eq!(inner.offset().y, px(-300.));
        assert!(inner.elastic_offset().y < px(0.));
        visual.simulate_event(gesture(-40., gpui::TouchPhase::Moved));
        assert_eq!(outer.offset().y, px(-400.), "the active gesture cannot leak into its parent");
        inner.set_offset(inner.offset());
    }
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn zero_delta_gesture_begin_still_initializes_its_eventual_owner(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_window(|_, cx| BarFixture(cx.new(|_| Scrollbars::new(false))));
        cx.run_until_parked();
        let handle = window.update(cx, |root, _, cx| root.0.read(cx).handle()).unwrap();
        handle.set_offset(Point::default());
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let mut event = gpui::ScrollWheelEvent {
            position: point(px(100.), px(50.)),
            is_gesture: true,
            touch_phase: gpui::TouchPhase::Started,
            ..Default::default()
        };
        visual.simulate_event(event.clone());
        event.touch_phase = gpui::TouchPhase::Moved;
        event.delta = gpui::ScrollDelta::Pixels(point(px(0.), px(-30.)));
        visual.simulate_event(event.clone());
        event.momentum_phase = Some(gpui::TouchPhase::Started);
        event.delta = gpui::ScrollDelta::Pixels(point(px(0.), px(-10.)));
        visual.simulate_event(event);
        assert_eq!(handle.offset().y, px(-40.));
    }
    #[test]
    fn fade_preserves_held_visibility_and_can_be_interrupted() {
        let mut bars = Scrollbars::new(false);
        let deadline = Instant::now();
        bars.visible_until = deadline;
        assert_eq!(bars.opacity(deadline - Duration::from_millis(1), false), 1.);
        assert_eq!(bars.opacity(deadline, false), 1.);
        assert_eq!(bars.opacity(deadline + FADE_FOR / 2, false), 0.5);
        let hidden = deadline + FADE_FOR;
        assert_eq!(bars.opacity(hidden, false), 0.);
        assert_eq!(bars.opacity(hidden + VISIBLE_FOR, false), 0.);
        assert_eq!(bars.opacity(hidden, true), 1.);
        bars.hovered = true;
        assert_eq!(bars.opacity(hidden, false), 1.);
        bars.hovered = false;
        bars.drag = Some(Drag { axis: 1, grab: 0. });
        assert_eq!(bars.opacity(hidden, false), 1.);
        bars.drag = None;
        bars.visible_until = hidden + VISIBLE_FOR;
        assert_eq!(bars.opacity(hidden, false), 1.);
    }

    #[test]
    fn overscroll_compresses_thumb_at_the_corresponding_edge() {
        let ordinary = Geometry::new(200., 200., 600., 0., 4., 4., 24.).unwrap();
        let top = Geometry::new(200., 200., 600., 10., 4., 4., 24.).unwrap();
        let bottom = Geometry::new(200., 200., 600., -610., 4., 4., 24.).unwrap();
        assert_eq!(top.thumb_length, ordinary.thumb_length - 10.);
        assert_eq!(top.thumb_start, 4.);
        assert_eq!(bottom.thumb_start + bottom.thumb_length, 196.);
    }
    #[test]
    fn thumb_tracks_content_and_both_endpoints() {
        let first = Geometry::new(336., 336., 672., 0., 12., 12., 24.).unwrap();
        assert_eq!(first.thumb_length, 104.);
        assert_eq!(first.thumb_start, 12.);
        assert_eq!(first.offset(12., 0.), 0.);
        let last = Geometry::new(336., 336., 672., -672., 12., 12., 24.).unwrap();
        assert_eq!(last.thumb_start + last.thumb_length, 324.);
        assert_eq!(last.offset(last.thumb_start, 0.), -672.);
        assert_eq!(last.offset(10000., 0.), -672.);
        assert_eq!(last.offset(-10000., 0.), 0.);
    }
    #[test]
    fn no_overflow_and_tiny_viewports_are_safe() {
        assert!(Geometry::new(300., 300., 0., 0., 12., 12., 24.).is_none());
        assert!(Geometry::new(20., 20., 400., 0., 12., 12., 24.).is_none());
        let tiny = Geometry::new(30., 30., 10000., 0., 12., 12., 24.).unwrap();
        assert_eq!(tiny.thumb_length, 6.);
        assert_eq!(tiny.offset(20., 3.), 0.);
    }
}
