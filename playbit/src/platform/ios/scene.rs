//! Borrowed semantic scene fields. Pointers live until the owning reply is freed.
use crate::runtime::{CanvasItem, ViewNode};

#[repr(C)]
#[derive(Default)]
pub struct SceneNode {
    pub kind: u32,
    pub id: u32,
    pub style: u32,
    pub state: u32,
    pub window_input: u32,
    pub disabled: u8,
    pub multiline: u8,
    pub read_only: u8,
    pub text: *const u8,
    pub text_len: usize,
    pub placeholder: *const u8,
    pub placeholder_len: usize,
    pub children: usize,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub items: usize,
    pub clear_color: u32,
    pub has_clear_color: u8,
}
#[repr(C)]
#[derive(Default)]
pub struct SceneCanvasItem {
    pub kind: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub color: u32,
    pub font_size: u32,
    pub bold: u8,
    pub text: *const u8,
    pub text_len: usize,
    pub source_width: u32,
    pub source_height: u32,
    pub pixels: *const u8,
    pub pixels_len: usize,
}
fn semantic(mut node: &ViewNode) -> &ViewNode {
    while let ViewNode::Theme { child, .. } = node {
        node = child;
    }
    node
}
/// Validate UIKit support and count retained heap storage before delivery.
pub(super) fn storage(node: &ViewNode) -> Result<usize, String> {
    let extra = match node {
        ViewNode::Theme { child, .. } => storage(child)?,
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            let mut bytes = children.capacity().saturating_mul(std::mem::size_of::<ViewNode>());
            for child in children {
                bytes = bytes.saturating_add(
                    storage(child)?.saturating_sub(std::mem::size_of::<ViewNode>()),
                );
            }
            bytes
        }
        ViewNode::Text { text, .. } => text.capacity(),
        ViewNode::Button { label, .. } | ViewNode::Checkbox { label, .. } => label.capacity(),
        ViewNode::TextField { value, placeholder, .. } => {
            value.capacity().saturating_add(placeholder.capacity())
        }
        ViewNode::Canvas { items, .. } => {
            let mut bytes = items.capacity().saturating_mul(std::mem::size_of::<CanvasItem>());
            for item in items {
                match item {
                    CanvasItem::Text { text, .. } => bytes = bytes.saturating_add(text.capacity()),
                    CanvasItem::Pixels { rgba, .. } => bytes = bytes.saturating_add(rgba.len()),
                    CanvasItem::Rect(_) => {}
                }
            }
            bytes
        }
        _ => return Err("This iPhone renderer does not support the submitted control".into()),
    };
    Ok(std::mem::size_of::<ViewNode>().saturating_add(extra))
}
/// # Safety
/// `node` is borrowed from a live IosReply; no call may race with reply_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_scene_node(node: *const ViewNode) -> SceneNode {
    let Some(node) = (unsafe { node.as_ref() }) else { return SceneNode::default() };
    let mut out = SceneNode { window_input: node.input_families(), ..SceneNode::default() };
    let text = match semantic(node) {
        ViewNode::Column(children) | ViewNode::List(children) => {
            out.kind = 1;
            out.children = children.len();
            ""
        }
        ViewNode::Row(children) => {
            out.kind = 2;
            out.children = children.len();
            ""
        }
        ViewNode::Text { text, style } => {
            out.kind = 3;
            out.style = *style as u32;
            text
        }
        ViewNode::Button { id, label, disabled, style } => {
            out.kind = 4;
            out.id = *id;
            out.disabled = *disabled as u8;
            out.style = *style as u32;
            label
        }
        ViewNode::TextField { id, value, placeholder, disabled, multiline, read_only } => {
            out.kind = 5;
            out.id = *id;
            out.disabled = *disabled as u8;
            out.multiline = *multiline as u8;
            out.read_only = *read_only as u8;
            out.placeholder = placeholder.as_ptr();
            out.placeholder_len = placeholder.len();
            value
        }
        ViewNode::Checkbox { id, label, state, disabled } => {
            out.kind = 6;
            out.id = *id;
            out.state = *state as u32;
            out.disabled = *disabled as u8;
            label
        }
        ViewNode::Canvas { id, width, height, frames, clear_color, items, .. } => {
            out.kind = 7;
            out.id = *id;
            out.width = *width;
            out.height = *height;
            out.frames = *frames as u32;
            out.items = items.len();
            if let Some(color) = clear_color {
                out.clear_color = color.0;
                out.has_clear_color = 1;
            }
            ""
        }
        _ => "", // Rejected by storage before exposing a scene.
    };
    out.text = text.as_ptr();
    out.text_len = text.len();
    out
}
/// # Safety
/// Same lifetime rules as pb_ios_scene_node. Out-of-range indices return null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_scene_child(
    node: *const ViewNode,
    index: usize,
) -> *const ViewNode {
    let Some(node) = (unsafe { node.as_ref() }) else { return std::ptr::null() };
    match semantic(node) {
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.get(index).map_or(std::ptr::null(), |child| child)
        }
        _ => std::ptr::null(),
    }
}
/// # Safety
/// Same lifetime rules as pb_ios_scene_node. Out-of-range indices return zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_scene_canvas_item(
    node: *const ViewNode,
    index: usize,
) -> SceneCanvasItem {
    let Some(node) = (unsafe { node.as_ref() }) else { return SceneCanvasItem::default() };
    if let ViewNode::Canvas { items, .. } = semantic(node)
        && let Some(item) = items.get(index)
    {
        let (rect, kind, font_size, bold, text) = match item {
            CanvasItem::Rect(rect) => (rect, 0, 0, false, ""),
            CanvasItem::Text { bounds, font_size, bold, text } => {
                (bounds, 1, *font_size, *bold, text.as_str())
            }
            CanvasItem::Pixels { bounds, .. } => (bounds, 2, 0, false, ""),
        };
        let mut out = SceneCanvasItem::default();
        if let CanvasItem::Pixels { source_width, source_height, rgba, .. } = item {
            out.source_width = *source_width;
            out.source_height = *source_height;
            out.pixels = rgba.as_ptr();
            out.pixels_len = rgba.len();
        }
        return SceneCanvasItem {
            kind,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            color: rect.color.0,
            font_size,
            bold: u8::from(bold),
            text: text.as_ptr(),
            text_len: text.len(),
            ..out
        };
    }
    SceneCanvasItem::default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests inspect valid native scene fixtures.")]
mod tests {
    use super::*;
    #[test]
    fn borrowed_pixels_keep_source_geometry_and_charge_retained_storage() {
        // The C span borrows the reply's validated bytes, while Swift copies before reply_free.
        let rgba: std::sync::Arc<[u8]> = [255, 0, 0, 255, 0, 255, 0, 128].into();
        let node = ViewNode::Canvas {
            id: 7,
            width: 16,
            height: 8,
            frames: playbit_api::ui::CanvasFrames::Static,
            extent: playbit_api::ui::CanvasExtent::Inline,
            clear_color: None,
            items: vec![CanvasItem::Pixels {
                bounds: playbit_api::ui::CanvasRect {
                    x: 2,
                    y: 3,
                    width: 12,
                    height: 4,
                    color: playbit_api::ui::Color(0),
                },
                source_width: 2,
                source_height: 1,
                rgba: rgba.clone(),
            }],
        };
        assert!(storage(&node).unwrap() >= std::mem::size_of::<ViewNode>() + rgba.len());
        // SAFETY: node and its pixel allocation remain alive throughout the borrowed-span reads.
        unsafe {
            let item = pb_ios_scene_canvas_item(&node, 0);
            assert_eq!(
                (item.kind, item.source_width, item.source_height, item.pixels_len),
                (2, 2, 1, 8)
            );
            assert_eq!(std::slice::from_raw_parts(item.pixels, item.pixels_len), rgba.as_ref());
        }
    }
    #[test]
    fn borrowed_fields_preserve_text_children_and_canvas_values() {
        let root = ViewNode::Column(vec![
            ViewNode::TextField {
                id: 7,
                value: "A\0é draft".into(),
                placeholder: "Type here".into(),
                multiline: true,
                disabled: false,
                read_only: true,
            },
            ViewNode::Canvas {
                id: 9,
                width: 320,
                height: 200,
                frames: playbit_api::ui::CanvasFrames::Static,
                extent: playbit_api::ui::CanvasExtent::Inline,
                clear_color: Some(playbit_api::ui::Color(0x22334480)),
                items: vec![
                    CanvasItem::Rect(playbit_api::ui::CanvasRect {
                        x: 1,
                        y: 2,
                        width: 30,
                        height: 40,
                        color: playbit_api::ui::Color(0x11223344),
                    }),
                    CanvasItem::Text {
                        bounds: playbit_api::ui::CanvasRect {
                            x: 4,
                            y: 8,
                            width: 100,
                            height: 20,
                            color: playbit_api::ui::Color(0x55667788),
                        },
                        font_size: 14,
                        bold: true,
                        text: "Trace é path".into(),
                    },
                ],
            },
        ]);
        assert!(storage(&root).unwrap() >= 3 * std::mem::size_of::<ViewNode>());
        unsafe {
            let fields = pb_ios_scene_node(&root);
            assert_eq!((fields.kind, fields.children), (1, 2));
            let field = pb_ios_scene_node(pb_ios_scene_child(&root, 0));
            assert_eq!((field.kind, field.id, field.multiline, field.read_only), (5, 7, 1, 1));
            assert_eq!(
                std::slice::from_raw_parts(field.text, field.text_len),
                b"A\0\xc3\xa9 draft"
            );
            let canvas = pb_ios_scene_child(&root, 1);
            let fields = pb_ios_scene_node(canvas);
            assert_eq!((fields.has_clear_color, fields.clear_color), (1, 0x22334480));
            assert_eq!(
                (fields.width, fields.height, fields.items, fields.frames),
                (320, 200, 2, 0)
            );
            let rect = pb_ios_scene_canvas_item(canvas, 0);
            assert_eq!(
                (rect.x, rect.y, rect.width, rect.height, rect.color),
                (1, 2, 30, 40, 0x11223344)
            );
            assert!(pb_ios_scene_child(&root, 2).is_null());
            let text = pb_ios_scene_canvas_item(canvas, 1);
            assert_eq!((text.kind, text.font_size, text.bold, text.color), (1, 14, 1, 0x55667788));
            assert_eq!(
                std::slice::from_raw_parts(text.text, text.text_len),
                "Trace é path".as_bytes()
            );
            assert_eq!(pb_ios_scene_canvas_item(canvas, 2).width, 0);
        }
    }
}
