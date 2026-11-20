//! Bounded pixel-frame images. Old atlas entries are removed before replacement.
use crate::runtime::CanvasItem;
use gpui::{Context, RenderImage, Window};
use std::sync::{Arc, LazyLock};

// Bound transform setup cost for tall narrow images as well as span storage.
const MAX_CHANGED_SPANS: usize = 256;

struct PixelImage {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
    image: Arc<RenderImage>,
}

#[derive(Default)]
pub(crate) struct CanvasImages {
    images: Vec<PixelImage>,
    window: Option<gpui::AnyWindowHandle>,
}
impl CanvasImages {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| {
            let images = std::mem::take(&mut this.images);
            if let Some(window) = this.window.take() {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| {
                        for item in images {
                            let _ = window.drop_image(item.image);
                        }
                    });
                });
            }
        })
        .detach();
        Self::default()
    }
    pub(crate) fn prepare(&mut self, items: &[CanvasItem], window: &mut Window) {
        self.window = Some(window.window_handle());
        let mut index = 0;
        for item in items {
            let CanvasItem::Pixels { source_width, source_height, rgba, .. } = item else {
                continue;
            };
            let unchanged = self.images.get(index).is_some_and(|image| {
                image.width == *source_width
                    && image.height == *source_height
                    && (Arc::ptr_eq(&image.rgba, rgba) || image.rgba == *rgba)
            });
            if !unchanged {
                match pixels_image(*source_width, *source_height, rgba, self.images.get(index)) {
                    Ok(image) => {
                        let item = PixelImage {
                            width: *source_width,
                            height: *source_height,
                            rgba: rgba.clone(),
                            image,
                        };
                        if index < self.images.len() {
                            let previous = std::mem::replace(&mut self.images[index], item);
                            let _ = window.drop_image(previous.image);
                        } else {
                            self.images.push(item);
                        }
                    }
                    Err(error) => {
                        log::error!("Canvas pixels could not be displayed: {error}");
                        for previous in self.images.drain(..) {
                            let _ = window.drop_image(previous.image);
                        }
                        return;
                    }
                }
            }
            index += 1;
        }
        for previous in self.images.drain(index..) {
            let _ = window.drop_image(previous.image);
        }
        if self.images.capacity() > self.images.len().saturating_mul(2) {
            self.images.shrink_to_fit();
        }
    }
    pub(crate) fn image(&self, index: usize) -> Option<Arc<RenderImage>> {
        self.images.get(index).map(|item| item.image.clone())
    }
}

fn pixels_image(
    width: u32,
    height: u32,
    rgba: &[u8],
    previous: Option<&PixelImage>,
) -> Result<Arc<RenderImage>, String> {
    type Transform = Arc<dyn moxcms::InPlaceTransformExecutor<u8> + Send + Sync>;
    static TRANSFORM: LazyLock<Result<Transform, moxcms::CmsError>> = LazyLock::new(|| {
        moxcms::ColorProfile::new_srgb().create_in_place_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_display_p3(),
            Default::default(),
        )
    });
    let transform = TRANSFORM.as_ref().map_err(|error| error.to_string())?;
    let bytes = if let Some(previous) = previous.filter(|image| {
        image.width == width && image.height == height && image.rgba.len() == rgba.len()
    }) && let Some(spans) = changed_spans(&previous.rgba, rgba, width as usize * 4)
    {
        let mut bytes = previous
            .image
            .as_bytes(0)
            .ok_or_else(|| "Missing previous packed image".to_owned())?
            .to_vec();
        for span in spans {
            let output = &mut bytes[span.clone()];
            output.copy_from_slice(&rgba[span]);
            transform.transform(output).map_err(|error| error.to_string())?;
            for pixel in output.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        bytes
    } else {
        let mut bytes = rgba.to_vec();
        transform.transform(&mut bytes).map_err(|error| error.to_string())?;
        for pixel in bytes.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        bytes
    };
    let buffer = image::RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| "Invalid packed RGBA8 dimensions".to_owned())?;
    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

fn changed_spans(
    previous: &[u8],
    rgba: &[u8],
    stride: usize,
) -> Option<Vec<std::ops::Range<usize>>> {
    if stride == 0 || previous.len() != rgba.len() || !rgba.len().is_multiple_of(stride) {
        return None;
    }
    let mut spans = Vec::with_capacity((rgba.len() / stride).min(MAX_CHANGED_SPANS));
    let mut changed = 0;
    for (row, (old, new)) in
        previous.chunks_exact(stride).zip(rgba.chunks_exact(stride)).enumerate()
    {
        if old == new {
            continue;
        }
        let left = old.iter().zip(new).position(|(a, b)| a != b)? / 4 * 4;
        let right = (old.iter().zip(new).rposition(|(a, b)| a != b)? / 4 + 1) * 4;
        changed += right - left;
        // Dense frames keep one full conversion instead of many small calls.
        if changed > rgba.len() / 2 || spans.len() == MAX_CHANGED_SPANS {
            return None;
        }
        let start = row * stride;
        spans.push(start + left..start + right);
    }
    Some(spans)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests inspect validated packed pixel fixtures.")]
mod tests {
    use super::*;

    #[test]
    fn packed_srgb_pixels_convert_to_p3_bgra_and_keep_straight_alpha() {
        // GPUI's native atlas is Display P3 BGRA8; guest pixels are sRGB RGBA8.
        let image = pixels_image(2, 1, &[255, 0, 0, 128, 0, 255, 0, 255], None).unwrap();
        let pixels = image.as_bytes(0).unwrap();
        assert_eq!(pixels.len(), 8);
        for (value, expected) in pixels[..3].iter().zip([35i32, 51, 234]) {
            assert!((i32::from(*value) - expected).abs() <= 2);
        }
        assert_eq!(pixels[3], 128);
        assert_eq!(pixels[7], 255);
        assert!(pixels_image(2, 1, &[0; 4], None).is_err());
    }

    #[test]
    fn sparse_conversion_matches_full_conversion_through_random_frames_and_resize() {
        let mut random = 0x4171_b8f9_u32;
        let mut byte = || {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random as u8
        };
        let mut previous: Option<PixelImage> = None;
        for (width, height) in [(1, 3), (2, 3), (3, 7), (7, 9), (31, 11), (360, 280), (361, 13)] {
            let mut rgba: Vec<u8> = (0..width * height * 4).map(|_| byte()).collect();
            for frame in 0..32 {
                if frame % 8 == 0 {
                    for value in &mut rgba {
                        *value = byte();
                    }
                } else {
                    for pixel in 0..(width * height).min(32) {
                        let offset = ((pixel * 97 + frame * 13) % (width * height)) as usize * 4;
                        // Exercise every channel independently, including alpha-only updates.
                        rgba[offset + frame as usize % 4] = byte();
                    }
                }
                let expected = pixels_image(width, height, &rgba, None).unwrap();
                let actual = pixels_image(width, height, &rgba, previous.as_ref()).unwrap();
                assert_eq!(
                    actual.as_bytes(0),
                    expected.as_bytes(0),
                    "{width}x{height}, frame {frame}"
                );
                previous =
                    Some(PixelImage { width, height, rgba: rgba.clone().into(), image: actual });
            }
        }
    }

    #[test]
    fn sparse_rows_use_pixel_aligned_spans_and_dense_frames_use_full_conversion() {
        let old = [0u8; 32];
        let mut new = old;
        new[5] = 1;
        new[11] = 2;
        let spans = changed_spans(&old, &new, 16).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0], 4..12);
        assert_eq!(changed_spans(&old, &old, 16), Some(vec![]));
        assert_eq!(changed_spans(&old, &[1; 32], 16), None);
        assert_eq!(changed_spans(&old, &new[..16], 16), None);

        let old = vec![0u8; 600 * 8];
        let mut tall = old.clone();
        for row in (0..600).step_by(2) {
            tall[row * 8 + 3] = 1; // Alpha-only change in one pixel, every other row.
        }
        let spans = changed_spans(&old[..512 * 8], &tall[..512 * 8], 8).unwrap();
        assert_eq!(spans.len(), MAX_CHANGED_SPANS);
        assert_eq!(spans.capacity(), MAX_CHANGED_SPANS);
        assert_eq!(changed_spans(&old, &tall, 8), None);
    }
}
