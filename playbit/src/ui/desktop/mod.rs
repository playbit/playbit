//! Shared desktop presentation; independent of the macOS shell and app executor.
pub mod canvas;
pub(crate) mod canvas_images;
pub mod controls;
mod image;
#[cfg(target_os = "macos")]
pub mod popup;
pub mod scene;
pub mod scrollbar;
pub mod segmented;
mod surface;
pub(crate) mod table;
pub mod text_editor;
pub(crate) mod typography;
#[cfg(all(target_os = "macos", feature = "local-store"))]
pub(crate) mod visual_bell;
pub mod zoom;
pub const UI_FONT_FAMILY: &str = "Inter Variable";
pub(crate) static UI_FONT_DATA: &[u8] = include_bytes!("../../../assets/InterVariable.ttf");
pub const UI_CONTROL_TRACKING: f32 = 0.11;

pub(crate) fn bundled_example_dir(app_id: &str) -> std::io::Result<std::path::PathBuf> {
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(std::path::Path::parent)
        .filter(|path| path.ends_with("Contents"))
        .ok_or_else(|| {
            std::io::Error::other("Open the example from a complete Playbit.app installation")
        })?;
    Ok(contents.join("Resources/examples").join(app_id))
}

#[cfg(all(target_os = "macos", feature = "local-store", feature = "native-runtime"))]
pub mod native;

#[cfg(all(target_os = "macos", feature = "local-store"))]
pub mod alert;
#[cfg(target_os = "macos")]
pub mod appearance;

#[cfg(all(target_os = "macos", feature = "connected-engine"))]
pub mod sign_in;

pub fn text_accessibility_node(
    text: &str,
    title: bool,
    bounds: gpui::Bounds<gpui::Pixels>,
) -> gpui::accesskit::Node {
    use gpui::accesskit::{Node, Rect, Role};
    let mut node = Node::new(if title { Role::Heading } else { Role::Label });
    node.set_read_only();
    if title {
        node.set_label(text);
    } else {
        node.set_value(text);
    }
    node.set_bounds(Rect::new(
        bounds.left().to_f64(),
        bounds.top().to_f64(),
        bounds.right().to_f64(),
        bounds.bottom().to_f64(),
    ));
    node
}
