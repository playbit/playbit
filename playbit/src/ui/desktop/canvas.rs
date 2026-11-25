//! Canvas rendering, bounded frame steps and local pointer coordinates.
use super::canvas_images::CanvasImages;
use crate::runtime::{CanvasItem, ViewNode};
use gpui::{Bounds, Pixels, div, prelude::*, px, rgba, size};
use playbit_api::ui::{CanvasExtent, Color};
use std::time::Duration;

/// Draw a canvas and report its visible bounds for window input routing.
pub(crate) fn canvas_element(
    id: u32,
    dimensions: (u32, u32),
    clear_color: Option<Color>,
    items: Vec<CanvasItem>,
    images: gpui::Entity<CanvasImages>,
    scale: f32,
    layout: impl Fn(Bounds<Pixels>, Bounds<Pixels>) + 'static,
) -> gpui::AnyElement {
    #[cfg(not(target_os = "macos"))]
    let _ = id;
    let (width, height) = dimensions;
    div()
        .w(px(width as f32 * scale))
        .h(px(height as f32 * scale))
        .child(
            gpui::canvas(
                move |bounds: Bounds<Pixels>, window, _| {
                    #[cfg(target_os = "macos")]
                    if id != 0 {
                        window.insert_hitbox(bounds, gpui::HitboxBehavior::ExcludeWindowDrag);
                    }
                    layout(bounds, bounds.intersect(&window.content_mask().bounds));
                },
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
        )
        .into_any_element()
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
}
