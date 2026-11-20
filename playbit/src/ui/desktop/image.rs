//! Bounded embedded artwork, decoded off the UI thread and uploaded in Display P3.
use super::zoom;
use gpui::{Context, Render, RenderImage, Task, Window, div, img, prelude::*};
use std::{io::Cursor, sync::Arc};

pub(super) struct ImageView {
    pub bytes: Vec<u8>,
    result: Option<Result<Arc<RenderImage>, String>>,
    pub width: u32,
    pub height: u32,
    pub label: String,
    _decode: Task<()>,
}
impl ImageView {
    pub fn new(bytes: &[u8], width: u32, height: u32, label: &str, cx: &mut Context<Self>) -> Self {
        let input = bytes.to_vec();
        let job = cx.background_executor().spawn(async move { decode(&input) });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.result = Some(result);
                cx.notify();
            });
        });
        Self {
            bytes: bytes.to_vec(),
            result: None,
            width,
            height,
            label: label.into(),
            _decode: task,
        }
    }
}
impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = match &self.result {
            Some(Ok(image)) => img(image.clone()).size_full().into_any_element(),
            Some(Err(error)) => {
                div().child(format!("Image unavailable: {error}")).into_any_element()
            }
            None => div().child("Loading image…").into_any_element(),
        };
        let label = self.label.clone();
        let id = gpui::accesskit::NodeId(cx.entity_id().as_u64() * 2);
        div()
            .relative()
            .flex_none()
            .w(zoom::pixels(window, self.width as f32))
            .h(zoom::pixels(window, self.height as f32))
            .child(image)
            .child(
                gpui::canvas(
                    move |bounds, window, _| {
                        if label.is_empty() {
                            return;
                        }
                        let mut node = gpui::accesskit::Node::new(gpui::accesskit::Role::Image);
                        node.set_label(label);
                        node.set_bounds(gpui::accesskit::Rect::new(
                            bounds.left().to_f64(),
                            bounds.top().to_f64(),
                            bounds.right().to_f64(),
                            bounds.bottom().to_f64(),
                        ));
                        window.insert_accessibility_node(id, node, false, None);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}
fn decode(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    use image::{ImageDecoder, codecs::jpeg::JpegDecoder, metadata::Cicp};
    if bytes.is_empty() || bytes.len() > playbit_api::ui::MAX_IMAGE_BYTES {
        return Err("JPEG exceeds 128 KiB".into());
    }
    let mut decoder = JpegDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    if width.checked_mul(height).is_none_or(|pixels| pixels > playbit_api::ui::MAX_IMAGE_PIXELS) {
        return Err("JPEG exceeds one megapixel".into());
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(8 * 1024 * 1024);
    decoder.set_limits(limits).map_err(|e| e.to_string())?;
    let mut rgba =
        image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?.into_rgba8();
    // JPEG decoding produces sRGB RGBA8. image's generic conversion also builds
    // unused f32/16-bit transforms whose tables overflow a debug GCD worker's
    // stack. Build only the in-place 8-bit transform, with no second pixel buffer.
    let transform = moxcms::ColorProfile::new_srgb()
        .create_in_place_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_display_p3(),
            Default::default(),
        )
        .map_err(|e| e.to_string())?;
    transform.transform(&mut rgba).map_err(|e| e.to_string())?;
    rgba.set_color_space(Cicp::DISPLAY_P3).map_err(|e| e.to_string())?;
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn jpeg_decode_rejects_invalid_and_excessive_input() {
        // Reject malformed payloads and enforce byte/pixel admission before conversion.
        assert!(decode(b"not JPEG").is_err());
        assert!(decode(&vec![0; playbit_api::ui::MAX_IMAGE_BYTES + 1]).is_err());
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut encoded)
            .encode(&vec![0; 1025 * 1024 * 3], 1025, 1024, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(decode(&encoded).is_err());
    }
    #[test]
    fn srgb_jpeg_is_converted_to_p3_before_upload() {
        // Exercise color conversion on the real platform executor: macOS GCD workers
        // have a smaller stack than Rust test threads. Include a minimal reproduction,
        // the reported display size, and the largest admitted pixel count.
        let executor = gpui::background_executor();
        for (width, height) in [(1, 1), (240, 160), (1024, 1024)] {
            let mut encoded = Vec::new();
            let pixels = [255, 0, 0].repeat(width as usize * height as usize);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 100)
                .encode(&pixels, width, height, image::ExtendedColorType::Rgb8)
                .unwrap();
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let _task = executor.spawn(async move { send.send(decode(&encoded)).unwrap() });
            let image = receive.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();
            let bgra = image.as_bytes(0).unwrap();
            assert_eq!(bgra.len(), width as usize * height as usize * 4);
            for pixel in bgra.as_chunks::<4>().0 {
                assert!((pixel[2] as i32 - 234).abs() <= 2);
                assert!((pixel[1] as i32 - 51).abs() <= 2);
                assert!((pixel[0] as i32 - 35).abs() <= 2);
                assert_eq!(pixel[3], 255);
            }
        }
    }
}
