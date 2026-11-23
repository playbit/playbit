//! Canvas rendering, bounded frame steps and local pointer coordinates.
use super::canvas_images::CanvasImages;
use crate::runtime::{CanvasItem, ViewNode};
use gpui::{Bounds, Pixels, Point, div, prelude::*, px, rgba, size};
use playbit_api::ui::{CanvasExtent, Color};
use std::{cell::Cell, rc::Rc, time::Duration};

/// Shared drawing surface and pointer gestures. `report` receives local pointer
/// values; the app scene encodes them when forwarding input to a guest.
pub(crate) fn canvas_element(
    id: u32,
    dimensions: (u32, u32),
    clear_color: Option<Color>,
    items: Vec<CanvasItem>,
    images: gpui::Entity<CanvasImages>,
    scale: f32,
    report: impl Fn(playbit_api::abi::syscall::PointerInput) + 'static,
) -> gpui::AnyElement {
    // Painting learns where the canvas sits; the pointer handlers need the same
    // origin to report positions inside it.
    let (width, height) = dimensions;
    let origin = Rc::new(Cell::new(gpui::point(px(0.), px(0.))));
    let painted = origin.clone();
    let report = Rc::new(report);
    let pointer = move |position: Point<Pixels>, phase: u32| {
        (report)(pointer_input(origin.get(), position, width, height, phase, scale));
    };
    let (down, moved, up, out) =
        (pointer.clone(), pointer.clone(), pointer.clone(), pointer.clone());
    let surface = div().w(px(width as f32 * scale)).h(px(height as f32 * scale)).child(
        gpui::canvas(
            move |bounds: Bounds<Pixels>, _, _| painted.set(bounds.origin),
            move |bounds, _, window, cx| {
                if let Some(color) = clear_color {
                    window.paint_quad(gpui::fill(bounds, rgba(color.0)));
                }
                let mut pixel_index = 0;
                for item in &items {
                    let rect = match item {
                        CanvasItem::Rect(rect) => rect,
                        CanvasItem::Text { bounds, .. } | CanvasItem::Pixels { bounds, .. } => {
                            bounds
                        }
                    };
                    let area = Bounds::new(
                        bounds.origin
                            + gpui::point(px(rect.x as f32 * scale), px(rect.y as f32 * scale)),
                        size(px(rect.width as f32 * scale), px(rect.height as f32 * scale)),
                    );
                    // GPUI rebuilds visible paint commands each frame. Do
                    // not shape labels or submit quads outside its current
                    // clip (viewport and enclosing scroll/clip elements).
                    let image = if matches!(item, CanvasItem::Pixels { .. }) {
                        let image = images.read(cx).image(pixel_index);
                        pixel_index += 1;
                        image
                    } else {
                        None
                    };
                    if !area.intersects(&window.content_mask().bounds) {
                        continue;
                    }
                    match item {
                        CanvasItem::Rect(_) => {
                            window.paint_quad(gpui::fill(area, rgba(rect.color.0)))
                        }
                        CanvasItem::Pixels { .. } => {
                            if let Some(image) = image {
                                #[cfg(target_os = "macos")]
                                if let Err(error) = window.paint_image_nearest(area, image) {
                                    log::error!("Canvas pixels could not be painted: {error}");
                                }
                                #[cfg(not(target_os = "macos"))]
                                let _ = image;
                            }
                        }
                        CanvasItem::Text { text, font_size, bold, .. } => {
                            let mut font = gpui::font(super::UI_FONT_FAMILY);
                            font.weight = if *bold {
                                gpui::FontWeight::BOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            };
                            let run = gpui::TextRun {
                                len: text.len(),
                                font,
                                color: rgba(rect.color.0).into(),
                                letter_spacing: Default::default(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let line = window.text_system().shape_line(
                                text.clone().into(),
                                px(*font_size as f32 * scale),
                                &[run],
                                None,
                            );
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds: area }),
                                |window| {
                                    line.paint(
                                        area.origin,
                                        px(*font_size as f32 * 1.25 * scale),
                                        window,
                                        cx,
                                    )
                                    .ok();
                                },
                            );
                        }
                    }
                }
            },
        )
        .w_full()
        .h_full(),
    );
    if id == 0 {
        return surface.into_any_element();
    }
    surface
        .id(("canvas", id))
        .on_mouse_down(gpui::MouseButton::Left, move |event, _, _| {
            down(event.position, 1);
        })
        .on_mouse_move(move |event, _, _| {
            if event.pressed_button == Some(gpui::MouseButton::Left) {
                moved(event.position, 0);
            }
        })
        .on_mouse_up(gpui::MouseButton::Left, move |event, _, _| {
            up(event.position, 2);
        })
        // Releasing away from the canvas still ends the gesture.
        .on_mouse_up_out(gpui::MouseButton::Left, move |event, _, _| {
            out(event.position, 3);
        })
        .into_any_element()
}

/// A pointer event in the canvas's own coordinates, clamped to its surface.
/// Phases match the web runtime: 0 drag, 1 press, 2 release, 3 cancel.
fn pointer_input(
    origin: Point<Pixels>,
    position: Point<Pixels>,
    width: u32,
    height: u32,
    phase: u32,
    scale: f32,
) -> playbit_api::abi::syscall::PointerInput {
    let local = (position - origin) / scale;
    let inside = |value: Pixels, limit: u32| (f32::from(value).round().max(0.) as u32).min(limit);
    playbit_api::abi::syscall::PointerInput {
        x: inside(local.x, width),
        y: inside(local.y, height),
        phase,
        buttons: u32::from(phase < 2),
    }
}

// FrameInput permits 1 microsecond through 100 ms. A paused window resumes with one
// bounded simulation step, never an invalid event or a backlog of missed frames.
// Trace timestamps and measured input latency continue to use actual time.
pub fn canvas_step(elapsed: Duration) -> u32 {
    elapsed.as_micros().clamp(1, 100_000) as u32
}

fn find_canvas<T>(
    view: &ViewNode,
    select: &impl Fn(u32, (u32, u32), playbit_api::ui::CanvasFrames) -> Option<T>,
) -> Option<T> {
    match view {
        ViewNode::Canvas { id, width, height, frames, .. } if *id != 0 => {
            select(*id, (*width, *height), *frames)
        }
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().find_map(|child| find_canvas(child, select))
        }
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => find_canvas(child, select),
        _ => None,
    }
}

pub fn canvas_id(view: &ViewNode) -> Option<u32> {
    find_canvas(view, &|id, _, _| Some(id))
}

pub fn canvas_frame_target(view: &ViewNode) -> Option<u32> {
    find_canvas(view, &|id, _, frames| {
        (frames == playbit_api::ui::CanvasFrames::Continuous).then_some(id)
    })
}

#[cfg(all(target_os = "macos", feature = "local-store", feature = "native-runtime"))]
pub(super) fn canvas_size(view: &ViewNode, target: u32) -> Option<(u32, u32)> {
    find_canvas(view, &|id, size, _| (id == target).then_some(size))
}

pub fn canvas_extent(view: &ViewNode) -> CanvasExtent {
    match view {
        ViewNode::Canvas { extent, .. } => *extent,
        ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => canvas_extent(child),
        _ => CanvasExtent::Inline,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    fn at(x: f32, y: f32) -> Point<Pixels> {
        gpui::point(px(x), px(y))
    }

    #[test]
    fn static_canvas_does_not_request_frame_input() {
        assert_eq!(
            canvas_frame_target(&ViewNode::Canvas {
                id: 0,
                width: 1,
                height: 1,
                frames: playbit_api::ui::CanvasFrames::Continuous,
                extent: playbit_api::ui::CanvasExtent::Inline,
                clear_color: None,
                items: vec![],
            }),
            None
        );
        assert_eq!(
            canvas_frame_target(&ViewNode::Canvas {
                id: 7,
                width: 1,
                height: 1,
                frames: playbit_api::ui::CanvasFrames::Static,
                extent: playbit_api::ui::CanvasExtent::Inline,
                clear_color: None,
                items: vec![],
            }),
            None
        );
        assert_eq!(
            canvas_frame_target(&ViewNode::Canvas {
                id: 7,
                width: 1,
                height: 1,
                frames: playbit_api::ui::CanvasFrames::Continuous,
                extent: playbit_api::ui::CanvasExtent::Inline,
                clear_color: None,
                items: vec![],
            }),
            Some(7)
        );
    }

    #[test]
    fn display_steps_remain_valid_after_a_pause_without_replaying_missed_frames() {
        use playbit_api::abi::syscall::FrameInput;
        for (elapsed, expected) in [
            (Duration::ZERO, 1),
            (Duration::from_micros(8_333), 8_333),
            (Duration::from_secs(60), 100_000),
        ] {
            let step = canvas_step(elapsed);
            assert_eq!(FrameInput::decode(&step.to_le_bytes()).unwrap().elapsed_us, expected);
        }
    }

    #[test]
    fn zoomed_canvas_maps_all_gesture_phases_to_logical_coordinates() {
        // Drawing and hit testing must agree at every supported UI/backing scale.
        for units in super::super::zoom::SAMPLES {
            let zoom = units as f32 / 10_000.;
            let origin = at(120., 64.);
            for phase in 0..=3 {
                let position = origin + gpui::point(px(43. * zoom), px(71. * zoom));
                let pointer = pointer_input(origin, position, 300, 180, phase, zoom);
                assert_eq!((pointer.x, pointer.y), (43, 71));
                assert_eq!(pointer.buttons, u32::from(phase < 2));
            }
        }
    }

    #[test]
    fn canvas_pointer_events_report_where_the_gesture_happened() {
        let origin = at(120., 64.);
        // A press reports its own position, not the middle of the surface.
        let press = pointer_input(origin, at(140., 96.), 300, 180, 1, 1.);
        assert_eq!((press.x, press.y, press.phase, press.buttons), (20, 32, 1, 1));
        // Dragging keeps the button held; releasing and cancelling let it go.
        let drag = pointer_input(origin, at(200., 100.), 300, 180, 0, 1.);
        assert_eq!((drag.x, drag.phase, drag.buttons), (80, 0, 1));
        for phase in [2, 3] {
            let ended = pointer_input(origin, at(200., 100.), 300, 180, phase, 1.);
            assert_eq!((ended.phase, ended.buttons), (phase, 0));
        }
        // Positions outside the surface land on its edges.
        let before = pointer_input(origin, at(0., 0.), 300, 180, 0, 1.);
        assert_eq!((before.x, before.y), (0, 0));
        let after = pointer_input(origin, at(9000., 9000.), 300, 180, 0, 1.);
        assert_eq!((after.x, after.y), (300, 180));
    }
}
