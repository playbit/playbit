//! Layout fixed-height controls only where they can be seen or receive input.
use super::Control;
use gpui::{Bounds, Entity, Pixels, canvas, point, prelude::*, px, size};
use std::rc::Rc;

/// Keep accessibility and the tab order complete while drawing a bounded viewport.
pub(crate) fn virtual_rows(
    rows: Vec<Entity<Control>>,
    height: Pixels,
    gap: Pixels,
) -> impl IntoElement {
    let rows: Rc<[_]> = rows.into();
    let pitch = height + gap;
    let extent = height * rows.len() as f32 + gap * rows.len().saturating_sub(1) as f32;
    let paint_rows = rows.clone();
    canvas(
        move |bounds, window, cx| {
            let clip = window.content_mask().bounds;
            let scale = window.scale_factor();
            let mut mounted = Vec::new();
            // Publish semantics in document order. Keep keyboard and
            // pointer-release handlers even when their row scrolls out.
            for (index, row) in rows.iter().enumerate() {
                let top = px((f32::from(pitch) * index as f32 * scale).round() / scale);
                let row_bounds = Bounds::new(
                    point(bounds.left(), bounds.top() + top),
                    size(bounds.size.width, height),
                );
                let control = row.read(cx);
                if row_bounds.intersects(&clip)
                    || control.focused(window)
                    || control.has_pointer_press()
                {
                    let mut element = super::element(row, window, cx);
                    element.prepaint_as_root(row_bounds.origin, row_bounds.size.into(), window, cx);
                    mounted.push((index, element));
                } else {
                    row.update(cx, |row, cx| {
                        row.publish_accessibility(row_bounds, false, window, cx);
                    });
                }
            }
            mounted
        },
        move |_, mounted, window, cx| {
            let mut mounted = mounted.into_iter().peekable();
            for (index, row) in paint_rows.iter().enumerate() {
                if let Some((_, mut element)) = mounted.next_if(|(i, _)| *i == index) {
                    element.paint(window, cx);
                } else {
                    row.read(cx).register_tab_stop(window);
                }
            }
        },
    )
    .w_full()
    .h(extent)
    .flex_none()
}
