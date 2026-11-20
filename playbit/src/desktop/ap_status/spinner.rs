//! Rotate the source silhouette first, then shade in window coordinates.
//! The Figma effects have a one-point downward offset and zero blur. No shaded
//! frame or rotation atlas is retained: one source tree and one displayed image.
use gpui::RenderImage;
use resvg::{tiny_skia, usvg};
use std::sync::Arc;

// 24 points × 200% app zoom × 4× display scale. Two scratch images and the
// current/preceding display images are bounded to 576 KiB during replacement.
const MAX_SIDE: u32 = 192;

pub(super) struct Spinner {
    source: usvg::Tree,
}
impl Spinner {
    pub(super) fn new() -> Self {
        Self {
            source: usvg::Tree::from_data(
                include_bytes!("../../../assets/ap-connecting.svg"),
                &usvg::Options::default(),
            )
            .expect("bundled spinner silhouette is valid SVG"),
        }
    }

    pub(super) fn frame(&self, angle: f32, pixels: f32) -> Arc<RenderImage> {
        let image = self.rasterize(angle, pixels);
        Arc::new(RenderImage::new([image::Frame::new(image)]))
    }

    fn rasterize(&self, angle: f32, pixels: f32) -> image::RgbaImage {
        let side = (24. * pixels).ceil().clamp(1., MAX_SIDE as f32) as u32;
        let scale = side as f32 / 24.;
        let (sin, cos) = angle.sin_cos();
        let center = side as f32 / 2.;
        let transform = tiny_skia::Transform::from_row(
            cos * scale,
            sin * scale,
            -sin * scale,
            cos * scale,
            center * (1. - cos + sin),
            center * (1. - cos - sin),
        );
        let mut mask = tiny_skia::Pixmap::new(side, side).expect("bounded nonzero spinner size");
        resvg::render(&self.source, transform, &mut mask.as_mut());
        shade(mask.data(), side, scale)
    }
}

fn shade(mask: &[u8], side: u32, offset: f32) -> image::RgbaImage {
    let alpha = |x: u32, y: i32| {
        if y < 0 || y >= side as i32 {
            return 0.;
        }
        f32::from(mask[((y as u32 * side + x) * 4 + 3) as usize]) / 255.
    };
    image::RgbaImage::from_fn(side, side, |x, y| {
        let shape = alpha(x, y as i32);
        // Translation follows rotation; +Y is always down on screen.
        let source_y = y as f32 - offset;
        let row = source_y.floor() as i32;
        let fraction = source_y - row as f32;
        let shifted = alpha(x, row) * (1. - fraction) + alpha(x, row + 1) * fraction;
        let drop = shifted * (1. - shape) * 0.7;
        let fill = shape * 0.6;
        let inner = shape * (1. - shifted) * 0.25;
        let opacity = inner + (fill + drop * (1. - fill)) * (1. - inner);
        let white = drop * (1. - fill) * (1. - inner);
        let gray = if opacity > 0. { (255. * white / opacity).round() as u8 } else { 0 };
        // Gray is identical in sRGB and Display P3; GPUI expects BGRA pixels.
        image::Rgba([gray, gray, gray, (opacity * 255.).round() as u8])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_and_inner_shadow_stay_downward_after_rotation() {
        let mut mask = vec![0; 9 * 9 * 4];
        for y in 2..5 {
            for x in 2..7 {
                mask[(y * 9 + x) * 4 + 3] = 255;
            }
        }
        let image = shade(&mask, 9, 1.);
        assert_eq!(image.get_pixel(4, 2).0, [0, 0, 0, 179]);
        assert_eq!(image.get_pixel(4, 3).0, [0, 0, 0, 153]);
        assert_eq!(image.get_pixel(4, 5).0, [255, 255, 255, 179]);
        assert_eq!(image.get_pixel(4, 1).0, [0; 4]);

        let spinner = Spinner::new();
        for angle in [0., 0.7, 1.8, 3.5, 5.2] {
            let image = spinner.rasterize(angle, 2.);
            assert!(image.pixels().any(|pixel| pixel.0[3] > 0));
            // A downward offset increases the light pixels' screen Y.
            let mut dark = (0., 0.);
            let mut light = (0., 0.);
            for (_, y, pixel) in image.enumerate_pixels() {
                let alpha = f32::from(pixel.0[3]);
                let white = f32::from(pixel.0[0]) / 255.;
                dark.0 += alpha * (1. - white) * y as f32;
                dark.1 += alpha * (1. - white);
                light.0 += alpha * white * y as f32;
                light.1 += alpha * white;
            }
            assert!(light.0 / light.1 > dark.0 / dark.1);
        }
    }

    #[test]
    #[ignore = "optimized rendering measurement; run with --release --ignored --nocapture"]
    fn benchmark_spinner_frames() {
        let spinner = Spinner::new();
        for pixels in [1., 2., 4., 8.] {
            let mut samples = Vec::with_capacity(1000);
            for i in 0..1020 {
                let start = std::time::Instant::now();
                std::hint::black_box(spinner.frame(i as f32 * 0.1, pixels));
                if i >= 20 {
                    samples.push(start.elapsed().as_secs_f64() * 1000.);
                }
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "spinner pixels={pixels} n=1000 p50={:.3} p95={:.3} p99={:.3} max={:.3} ms",
                samples[499], samples[949], samples[989], samples[999]
            );
        }
    }
}
