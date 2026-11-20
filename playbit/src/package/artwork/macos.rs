//! The same composed SVG artwork supplies Finder thumbnails and guest wrappers.
use super::{render_vector, straight_rgba, tiny_skia, vector_tree};

const LIGHT: &[u8] = include_bytes!("../../../platform/macos/icon-guest-template.png");
const DARK: &[u8] = include_bytes!("../../../platform/macos/icon-guest-template-dark.png");
const TEMPLATE_SIDE: u32 = 1024;
// Figma's optical center is 12 pixels above the geometric center.
const ARTWORK: [f32; 4] = [200., 188., 624., 624.];

/// PNG/JPEG artwork is a complete icon. Only SVG glyphs receive the template.
/// One trusted 4 MiB template plus a <=1 MiB canvas is decoded at a time;
/// resampling uses tiny-skia's bounded bilinear shader, without float images.
pub fn icon(bytes: &[u8], side: u32, dark: bool) -> Result<image::RgbaImage, String> {
    if !matches!(side, 128 | 256 | 512) {
        return Err("unsupported macOS icon size".into());
    }
    if image::guess_format(bytes).is_ok() {
        return super::decode(bytes, side);
    }
    let tree = vector_tree(bytes)?;
    let template = tiny_skia::Pixmap::decode_png(if dark { DARK } else { LIGHT })
        .map_err(|error| format!("invalid bundled icon template: {error}"))?;
    assert_eq!(template.width(), TEMPLATE_SIDE, "bundled template has its authored width");
    assert_eq!(template.height(), TEMPLATE_SIDE, "bundled template has its authored height");
    let mut canvas = tiny_skia::Pixmap::new(side, side).expect("validated macOS icon size");
    let scale = side as f32 / TEMPLATE_SIDE as f32;
    canvas.draw_pixmap(
        0,
        0,
        template.as_ref(),
        &tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        tiny_skia::Transform::from_scale(scale, scale),
        None,
    );
    drop(template);
    let [x, y, width, height] = ARTWORK.map(|value| value * scale);
    // Clip to the SVG's viewport before placement. Shapes outside the viewBox
    // must not escape the safe area and paint over the template's edges.
    let mut glyph = tiny_skia::Pixmap::new(width as u32, height as u32)
        .expect("the authored artwork box scales to 78, 156 or 312 pixels");
    render_vector(&tree, &mut glyph, [0., 0., width, height]);
    canvas.draw_pixmap(
        0,
        0,
        glyph.as_ref(),
        &tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        tiny_skia::Transform::from_translate(x, y),
        None,
    );
    Ok(straight_rgba(canvas))
}

/// Asset-only updates must invalidate previously generated guest app wrappers.
pub fn template_digest() -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    hash.update(LIGHT);
    hash.update(DARK);
    hash.finalize()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful artwork decoding.")]
mod tests {
    use super::*;

    #[test]
    fn svg_uses_optical_bounds_and_appearance_without_recoloring() {
        let svg = br#"<svg width="2" height="1"><rect width="1" height="1" fill="red"/><rect x="1" width="1" height="1" fill="blue"/></svg>"#;
        for side in [128, 256, 512] {
            let light = icon(svg, side, false).unwrap();
            let dark = icon(svg, side, true).unwrap();
            let s = side / 128;
            for image in [&light, &dark] {
                assert_eq!(image.get_pixel(0, 0).0[3], 0);
                assert_eq!(image.get_pixel(30 * s, 60 * s).0, [255, 0, 0, 255]);
                assert_eq!(image.get_pixel(95 * s, 60 * s).0, [0, 0, 255, 255]);
                // 2:1 artwork is centered inside the 624-square safe area.
                assert_ne!(image.get_pixel(30 * s, 30 * s).0, [255, 0, 0, 255]);
                assert_ne!(image.get_pixel(24 * s, 60 * s).0, [255, 0, 0, 255]);
                assert_ne!(image.get_pixel(104 * s, 60 * s).0, [0, 0, 255, 255]);
            }
            assert!(light.get_pixel(64 * s, 30 * s).0[0] > dark.get_pixel(64 * s, 30 * s).0[0]);
        }
        let square = br#"<svg width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#;
        let image = icon(square, 512, false).unwrap();
        assert_ne!(image.get_pixel(256, 93).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(256, 94).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(256, 405).0, [255, 0, 0, 255]);
        assert_ne!(image.get_pixel(256, 406).0, [255, 0, 0, 255]);
        let overflow = br#"<svg width="1" height="1"><rect x="-2" y="-2" width="5" height="5" fill="red"/></svg>"#;
        assert_eq!(icon(overflow, 512, false).unwrap(), image);
        assert!(icon(square, 1024, false).is_err());
        assert!(icon(b"not an SVG", 512, false).is_err());
    }
}
