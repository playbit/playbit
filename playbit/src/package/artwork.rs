//! Bounded display-only package artwork, shared by the native hosts.
//! Call from a background worker, never from an input or rendering callback.
use image::ImageEncoder;
use resvg::{tiny_skia, usvg};
use std::io::Cursor;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{icon as macos_icon, template_digest as macos_template_digest};

/// Largest 48-point launcher tile at 200% zoom on a 2× Mac display.
pub(crate) const MAX_TILE_PIXELS: u32 = 192;

/// Decode to a centered straight-RGBA canvas at the host's requested pixel size.
/// No file, network, font or external resource resolution.
pub fn decode(bytes: &[u8], side: u32) -> Result<image::RgbaImage, String> {
    if !matches!(side, 48..=MAX_TILE_PIXELS | 256 | 512) {
        return Err("unsupported display icon size".into());
    }
    if bytes.len() > super::codec::MAX_ICON_BYTES {
        return Err("source icon exceeds 1 MiB".into());
    }
    let rgba = match image::guess_format(bytes) {
        Ok(format @ (image::ImageFormat::Png | image::ImageFormat::Jpeg)) => {
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(1024);
            limits.max_image_height = Some(1024);
            limits.max_alloc = Some(4 * 1024 * 1024);
            reader.limits(limits);
            let mut image = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
            // Area sampling avoids resize's float intermediate (8 MiB for a
            // 1024px input at 512px). Premultiply first so transparent colors
            // cannot bleed into visible edges. Peak image storage is <6 MiB.
            for pixel in image.pixels_mut() {
                let [r, g, b, a] = pixel.0;
                let color = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
                pixel.0 = [color.red(), color.green(), color.blue(), color.alpha()];
            }
            let mut image =
                image::DynamicImage::ImageRgba8(image).thumbnail(side, side).into_rgba8();
            for pixel in image.pixels_mut() {
                let [r, g, b, a] = pixel.0;
                let color = tiny_skia::PremultipliedColorU8::from_rgba(r, g, b, a)
                    .expect("area averages preserve premultiplied color bounds")
                    .demultiply();
                pixel.0 = [color.red(), color.green(), color.blue(), color.alpha()];
            }
            let mut canvas = image::RgbaImage::new(side, side);
            image::imageops::overlay(
                &mut canvas,
                &image,
                (side - image.width()) as i64 / 2,
                (side - image.height()) as i64 / 2,
            );
            canvas
        }
        Ok(_) => return Err("app icons support PNG, JPEG or simple SVG".into()),
        Err(_) => vector(bytes, side)?,
    };
    Ok(rgba)
}

/// Keep display responses small independently of the original package artwork.
/// Runs on the existing upload worker, with the same bounded decoder as clients.
pub fn preview(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = decode(bytes, 120)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(image.as_raw(), 120, 120, image::ExtendedColorType::Rgba8)
    .map_err(|error| error.to_string())?;
    // Even incompressible RGBA pixels fit: 57,600 bytes plus PNG/zlib framing.
    if png.len() > 65536 {
        return Err("icon preview exceeds 64 KiB".into());
    }
    Ok(png)
}

fn vector(bytes: &[u8], side: u32) -> Result<image::RgbaImage, String> {
    let tree = vector_tree(bytes)?;
    let mut pixmap =
        tiny_skia::Pixmap::new(side, side).expect("artwork side length is bounded and nonzero");
    render_vector(&tree, &mut pixmap, [0., 0., side as f32, side as f32]);
    Ok(straight_rgba(pixmap))
}

fn vector_tree(bytes: &[u8]) -> Result<usvg::Tree, String> {
    if bytes.len() > 16384 {
        return Err("SVG icon exceeds 16 KiB".into());
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "app icons support PNG, JPEG or simple SVG")?;
    let doc = usvg::roxmltree::Document::parse_with_options(
        text,
        usvg::roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 512 },
    )
    .map_err(|e| e.to_string())?;
    for node in doc.descendants().filter(|n| n.is_element()) {
        if !matches!(
            node.tag_name().name(),
            "svg"
                | "g"
                | "path"
                | "rect"
                | "circle"
                | "ellipse"
                | "line"
                | "polygon"
                | "polyline"
                | "defs"
                | "linearGradient"
                | "radialGradient"
                | "stop"
                | "title"
                | "desc"
        ) {
            return Err("SVG icons support shapes and gradients; use PNG for other artwork".into());
        }
        for attr in node.attributes() {
            // Only local gradient references are permitted. Never resolve files,
            // remote resources, reusable subtrees or embedded raster images.
            if attr.name() == "href"
                || attr.value().contains("url(") && !matches!(attr.name(), "fill" | "stroke")
            {
                return Err("SVG icon references are limited to local gradients".into());
            }
            if attr.value().contains("url(") && !attr.value().starts_with("url(#") {
                return Err("SVG icon cannot refer to external resources".into());
            }
        }
    }
    let options = usvg::Options {
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    usvg::Tree::from_xmltree(&doc, &options).map_err(|e| e.to_string())
}

fn render_vector(
    tree: &usvg::Tree,
    pixmap: &mut tiny_skia::Pixmap,
    [x, y, width, height]: [f32; 4],
) {
    let size = tree.size();
    let scale = (width / size.width()).min(height / size.height());
    let transform = tiny_skia::Transform::from_row(
        scale,
        0.,
        0.,
        scale,
        x + (width - size.width() * scale) / 2.,
        y + (height - size.height() * scale) / 2.,
    );
    resvg::render(tree, transform, &mut pixmap.as_mut());
}

fn straight_rgba(pixmap: tiny_skia::Pixmap) -> image::RgbaImage {
    // tiny-skia uses premultiplied RGBA; return straight RGBA for native adapters.
    let bytes = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let p = p.demultiply();
            [p.red(), p.green(), p.blue(), p.alpha()]
        })
        .collect();
    image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), bytes)
        .expect("pixmap pixels contain exactly width times height RGBA values")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut data = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 128]))
            .write_to(&mut data, image::ImageFormat::Png)
            .unwrap();
        data.into_inner()
    }
    #[test]
    fn native_sizes_preserve_straight_rgba_and_center_raster_and_vector() {
        let svg = b"<svg width=\"24\" height=\"12\"><rect width=\"24\" height=\"12\" \
            fill=\"red\" opacity=\"0.5\"/></svg>";
        for side in [48, 120, 512] {
            for bytes in [svg.to_vec(), png(24, 12)] {
                let image = decode(&bytes, side).unwrap();
                assert_eq!(image.dimensions(), (side, side));
                assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0, 0]);
                assert_eq!(image.get_pixel(side / 2, side / 2).0, [255, 0, 0, 128]);
                assert_eq!(image.get_pixel(side - 1, side - 1).0, [0, 0, 0, 0]);
            }
        }
        assert!(decode(svg, 0).is_err());
        assert!(decode(svg, u32::MAX).is_err());
    }
    #[test]
    fn large_raster_sources_generate_small_png_previews() {
        let mut state = 1u32;
        let noisy = image::RgbImage::from_fn(512, 512, |_, _| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
        });
        let mut output = Cursor::new(Vec::new());
        noisy.write_to(&mut output, image::ImageFormat::Png).unwrap();
        let noisy = output.into_inner();
        assert!(noisy.len() > 65536 && noisy.len() <= super::super::codec::MAX_ICON_BYTES);
        for source in [png(1024, 1024), noisy] {
            let bytes = preview(&source).unwrap();
            assert!(bytes.len() <= 65536);
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            let image = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert_eq!(image.dimensions(), (120, 120));
            assert!(decode(&source, 512).is_ok());
        }
    }
    #[test]
    fn raster_downsampling_ignores_color_in_transparent_pixels() {
        let image = image::RgbaImage::from_fn(96, 48, |x, _| {
            if x % 2 == 0 { image::Rgba([255, 0, 0, 0]) } else { image::Rgba([0, 0, 255, 255]) }
        });
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let scaled = decode(bytes.get_ref(), 48).unwrap();
        assert_eq!(scaled.get_pixel(24, 24).0, [0, 0, 255, 128]);
        assert_eq!(scaled.get_pixel(24, 0).0, [0, 0, 0, 0]);
    }
    #[test]
    fn native_sizes_keep_the_same_untrusted_resource_limits() {
        for side in [48, 120, 512] {
            assert!(decode(&png(1025, 1), side).is_err());
            assert!(decode(&vec![0; super::super::codec::MAX_ICON_BYTES + 1], side).is_err());
            assert!(decode(format!("<svg>{}</svg>", "<g/>".repeat(513)).as_bytes(), side).is_err());
            assert!(decode(b"<!DOCTYPE svg [<!ENTITY x 'x'>]><svg>&x;</svg>", side).is_err());
            for body in [
                "<image href='/etc/passwd'/>",
                "<use href='#x'/>",
                "<filter/>",
                "<text>x</text>",
                "<rect fill='url(https://example.com)'/>",
                "<linearGradient href='#x'/>",
            ] {
                assert!(
                    decode(format!("<svg width='24' height='24'>{body}</svg>").as_bytes(), side)
                        .is_err()
                );
            }
        }
    }
}
