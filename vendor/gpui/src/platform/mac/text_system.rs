use crate::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, Point,
    RenderGlyphParams, Result, SUBPIXEL_VARIANTS_X, ShapedGlyph, ShapedRun, SharedString, Size,
    point, px, size, swap_rgba_pa_to_bgra,
};
use anyhow::anyhow;
use cocoa::appkit::CGFloat;
use collections::HashMap;
use core_foundation::{
    attributed_string::CFMutableAttributedString,
    base::{CFRange, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_graphics::{
    base::{CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::{CGContext, CGTextDrawingMode},
    display::CGPoint,
};
use core_text::{
    font::CTFont,
    font_descriptor::{
        kCTFontSlantTrait, kCTFontSymbolicTrait, kCTFontWeightTrait, kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    metrics::Metrics,
    properties::{Style as FontkitStyle, Weight as FontkitWeight},
    source::SystemSource,
    sources::mem::MemSource,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc};

use super::open_type::apply_features_and_fallbacks;

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;

// Available since macOS 10.12; the core-text crate does not expose it yet.
unsafe extern "C" {
    static kCTTrackingAttributeName: CFStringRef;
    static kCTFontVariationAxisIdentifierKey: CFStringRef;
}

const WEIGHT_AXIS: i64 = 0x7767_6874; // OpenType `wght`.

fn font_at_variable_weight(font: &FontKitFont, weight: FontWeight) -> Option<FontKitFont> {
    if weight == FontWeight::NORMAL {
        return None;
    }
    let native = font.native_font();
    let identifier = unsafe { CFString::wrap_under_get_rule(kCTFontVariationAxisIdentifierKey) };
    let has_weight_axis = native.get_variation_axes()?.iter().any(|axis| {
        axis.find(identifier.clone())
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i64())
            == Some(WEIGHT_AXIS)
    });
    if !has_weight_axis {
        return None;
    }
    let axis_values = CFDictionary::from_CFType_pairs(&[(
        CFNumber::from(WEIGHT_AXIS),
        CFNumber::from(f64::from(weight.0)),
    )]);
    let key = unsafe {
        CFString::wrap_under_get_rule(core_text::font_descriptor::kCTFontVariationAttribute)
    };
    let attributes = CFDictionary::from_CFType_pairs(&[(key, axis_values)]);
    let descriptor =
        native.copy_descriptor().create_copy_with_attributes(attributes.to_untyped()).ok()?;
    let varied = core_text::font::new_from_descriptor(&descriptor, native.pt_size());
    // The FontKit instance owns its cloned CoreText handle after this call.
    Some(unsafe { FontKitFont::from_native_font(&varied) })
}

pub(crate) struct MacTextSystem(RwLock<MacTextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    font_family: SharedString,
    font_features: FontFeatures,
    font_fallbacks: Option<FontFallbacks>,
}

struct MacTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_postscript_name: HashMap<String, FontId>,
    font_ids_by_font_key: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
    /// UTF-16 indices of ZWNJS
    zwnjs_scratch_space: Vec<usize>,
    // Least recently used first. Font IDs include the resolved face/features;
    // text remains in WindowTextSystem's existing line-layout cache.
    sized_fonts: Vec<SizedFont>,
}

const SIZED_FONT_LIMIT: usize = 32;

struct SizedFont {
    id: FontId,
    size: Pixels,
    font: CTFont,
    ascent: f32,
    descent: f32,
}

const _: () = assert!(
    std::mem::size_of::<SizedFont>() * SIZED_FONT_LIMIT + std::mem::size_of::<Vec<SizedFont>>()
        <= 4096
);

impl MacTextSystem {
    pub(crate) fn new() -> Self {
        Self(RwLock::new(MacTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            font_selections: HashMap::default(),
            font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_font_key: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
            zwnjs_scratch_space: Vec::new(),
            sized_fonts: Vec::with_capacity(SIZED_FONT_LIMIT),
        }))
    }
}

impl Default for MacTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for MacTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let collection = core_text::font_collection::create_for_all_families();
        let Some(descriptors) = collection.get_descriptors() else {
            return names;
        };
        for descriptor in descriptors.into_iter() {
            names.extend(lenient_font_attributes::family_name(&descriptor));
        }
        if let Ok(fonts_in_memory) = self.0.read().memory_source.all_families() {
            names.extend(fonts_in_memory);
        }
        names
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let font_key = FontKey {
                font_family: font.family.clone(),
                font_features: font.features.clone(),
                font_fallbacks: font.fallbacks.clone(),
            };
            let candidates = if let Some(font_ids) = lock.font_ids_by_font_key.get(&font_key) {
                font_ids.as_slice()
            } else {
                let font_ids =
                    lock.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
                lock.font_ids_by_font_key.insert(font_key.clone(), font_ids);
                lock.font_ids_by_font_key[&font_key].as_ref()
            };

            let candidate_properties = candidates
                .iter()
                .map(|font_id| lock.fonts[font_id.0].properties())
                .collect::<SmallVec<[_; 4]>>();

            let ix = font_kit::matching::find_best_match(
                &candidate_properties,
                &font_kit::properties::Properties {
                    style: font.style.into(),
                    weight: font.weight.into(),
                    stretch: Default::default(),
                },
            )?;

            let mut font_id = candidates[ix];
            if let Some(weighted) = font_at_variable_weight(&lock.fonts[font_id.0], font.weight) {
                let weighted_id = FontId(lock.fonts.len());
                if let Some(name) = weighted.postscript_name() {
                    lock.postscript_names_by_font_id.insert(weighted_id, name);
                }
                lock.fonts.push(weighted);
                font_id = weighted_id;
            }
            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.read().fonts[font_id.0].metrics().into()
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(self.0.read().fonts[font_id.0].typographic_bounds(glyph_id.0)?.into())
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        glyph_id: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(glyph_id, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let wait = playbit_trace::Span::detail("gpui.text.native_lock");
        let mut state = self.0.write();
        drop(wait);
        state.layout_line(text, font_size, font_runs)
    }
}

impl MacTextSystemState {
    fn sized_font(&mut self, id: FontId, size: Pixels) -> &SizedFont {
        if let Some(index) =
            self.sized_fonts.iter().position(|entry| entry.id == id && entry.size == size)
        {
            self.sized_fonts[index..].rotate_left(1);
        } else {
            let font = &self.fonts[id.0];
            let metrics = font.metrics();
            let scale = size.0 / metrics.units_per_em as f32;
            let entry = SizedFont {
                id,
                size,
                font: font.native_font().clone_with_font_size(size.into()),
                ascent: metrics.ascent * scale,
                descent: -metrics.descent * scale,
            };
            if self.sized_fonts.len() == SIZED_FONT_LIMIT {
                self.sized_fonts.remove(0);
            }
            self.sized_fonts.push(entry);
        }
        self.sized_fonts.last().unwrap()
    }

    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let fonts = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(embedded_font) => {
                    let data_provider = unsafe {
                        core_graphics::data_provider::CGDataProvider::from_slice(embedded_font)
                    };
                    let font = core_graphics::font::CGFont::from_data_provider(data_provider)
                        .map_err(|()| anyhow!("Could not load an embedded font."))?;
                    let font = font_kit::loaders::core_text::Font::from_core_graphics_font(font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(fonts.into_iter())?;
        Ok(())
    }

    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let name = crate::text_system::font_name_with_fallbacks(name, ".AppleSystemUIFont");

        let mut font_ids = SmallVec::new();
        let family = self
            .memory_source
            .select_family_by_name(name)
            .or_else(|_| self.system_source.select_family_by_name(name))?;
        for font in family.fonts() {
            let mut font = font.load()?;

            apply_features_and_fallbacks(&mut font, features, fallbacks)?;
            // This block contains a precautionary fix to guard against loading fonts
            // that might cause panics due to `.unwrap()`s up the chain.
            {
                // We use the 'm' character for text measurements in various spots
                // (e.g., the editor). However, at time of writing some of those usages
                // will panic if the font has no 'm' glyph.
                //
                // Therefore, we check up front that the font has the necessary glyph.
                let has_m_glyph = font.glyph_for_char('m').is_some();

                // HACK: The 'Segoe Fluent Icons' font does not have an 'm' glyph,
                // but we need to be able to load it for rendering Windows icons in
                // the Storybook (on macOS).
                let is_segoe_fluent_icons = font.full_name() == "Segoe Fluent Icons";

                if !has_m_glyph && !is_segoe_fluent_icons {
                    // I spent far too long trying to track down why a font missing the 'm'
                    // character wasn't loading. This log statement will hopefully save
                    // someone else from suffering the same fate.
                    log::warn!(
                        "font '{}' has no 'm' character and was not loaded",
                        font.full_name()
                    );
                    continue;
                }
            }

            // We've seen a number of panics in production caused by calling font.properties()
            // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
            // and to try and identify the incalcitrant font.
            let traits = font.native_font().all_traits();
            if unsafe {
                !(traits.get(kCTFontSymbolicTrait).downcast::<CFNumber>().is_some()
                    && traits.get(kCTFontWidthTrait).downcast::<CFNumber>().is_some()
                    && traits.get(kCTFontWeightTrait).downcast::<CFNumber>().is_some()
                    && traits.get(kCTFontSlantTrait).downcast::<CFNumber>().is_some())
            } {
                log::error!("Failed to read traits for font {:?}", font.postscript_name().unwrap());
                continue;
            }

            let font_id = FontId(self.fonts.len());
            font_ids.push(font_id);
            let postscript_name = font.postscript_name().unwrap();
            self.font_ids_by_postscript_name.insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id.insert(font_id, postscript_name);
            self.fonts.push(font);
        }
        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(self.fonts[font_id.0].advance(glyph_id.0)?.into())
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts[font_id.0].glyph_for_char(ch).map(GlyphId)
    }

    fn id_for_native_font(&mut self, requested_font: CTFont) -> FontId {
        let postscript_name = requested_font.postscript_name();
        if let Some(font_id) = self.font_ids_by_postscript_name.get(&postscript_name) {
            *font_id
        } else {
            let font_id = FontId(self.fonts.len());
            self.font_ids_by_postscript_name.insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id.insert(font_id, postscript_name);
            self.fonts.push(font_kit::font::Font::from_core_graphics_font(
                requested_font.copy_to_CGFont(),
            ));
            font_id
        }
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id.get(&font_id).is_some_and(|postscript_name| {
            postscript_name == "AppleColorEmoji" || postscript_name == ".AppleColorEmojiUI"
        })
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let scale = Transform2F::from_scale(params.scale_factor);
        Ok(font
            .raster_bounds(
                params.glyph_id.0,
                params.font_size.into(),
                scale,
                HintingOptions::None,
                font_kit::canvas::RasterizationOptions::GrayscaleAa,
            )?
            .into())
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            if params.subpixel_variant.x > 0 {
                bitmap_size.width += DevicePixels(1);
            }
            if params.subpixel_variant.y > 0 {
                bitmap_size.height += DevicePixels(1);
            }
            let bitmap_size = bitmap_size;

            let mut bytes;
            let cx;
            if params.is_emoji {
                bytes = vec![0; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaPremultipliedLast,
                );
            } else {
                bytes = vec![0; bitmap_size.width.0 as usize * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize,
                    &CGColorSpace::create_device_gray(),
                    kCGImageAlphaOnly,
                );
            }

            // Move the origin to bottom left and account for scaling, this
            // makes drawing text consistent with the font-kit's raster_bounds.
            cx.translate(
                -glyph_bounds.origin.x.0 as CGFloat,
                (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
            );
            cx.scale(params.scale_factor as CGFloat, params.scale_factor as CGFloat);

            let subpixel_shift =
                params.subpixel_variant.map(|v| v as f32 / SUBPIXEL_VARIANTS_X as f32);
            cx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            cx.set_gray_fill_color(0.0, 1.0);
            cx.set_allows_antialiasing(true);
            cx.set_should_antialias(true);
            cx.set_allows_font_subpixel_positioning(true);
            cx.set_should_subpixel_position_fonts(true);
            cx.set_allows_font_subpixel_quantization(false);
            cx.set_should_subpixel_quantize_fonts(false);
            self.fonts[params.font_id.0]
                .native_font()
                .clone_with_font_size(f32::from(params.font_size) as CGFloat)
                .draw_glyphs(
                    &[params.glyph_id.0 as CGGlyph],
                    &[CGPoint::new(
                        (subpixel_shift.x / params.scale_factor) as CGFloat,
                        (subpixel_shift.y / params.scale_factor) as CGFloat,
                    )],
                    cx,
                );

            if params.is_emoji {
                // Convert from RGBA with premultiplied alpha to BGRA with straight alpha.
                for pixel in bytes.chunks_exact_mut(4) {
                    swap_rgba_pa_to_bgra(pixel);
                }
            }

            Ok((bitmap_size, bytes))
        }
    }

    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let attributes = playbit_trace::Span::detail("gpui.text.native_attributes");
        const ZWNJ: char = '\u{200C}';
        const ZWNJ_STR: &str = "\u{200C}";
        const ZWNJ_SIZE_16: usize = ZWNJ.len_utf16();

        self.zwnjs_scratch_space.clear();
        // Construct the attributed string, converting UTF8 ranges to UTF16 ranges.
        let mut string = CFMutableAttributedString::new();
        let mut max_ascent = 0.0f32;
        let mut max_descent = 0.0f32;
        let has_tracking = font_runs.iter().any(|run| run.letter_spacing != Pixels::ZERO);

        {
            let mut ix_converter = StringIndexConverter::new(&text);
            let mut last_font_run = None;
            for run in font_runs {
                let text = &text[ix_converter.utf8_ix..][..run.len];
                // if the fonts are the same, we need to disconnect the text with a ZWNJ
                // to prevent core text from forming ligatures between them
                let needs_zwnj = last_font_run.replace(run.font_id) == Some(run.font_id);

                let utf16_start = string.char_len(); // insert at end of string
                ix_converter.advance_to_utf8_ix(ix_converter.utf8_ix + run.len);

                // note: replace_str may silently ignore codepoints it dislikes (e.g., BOM at start of string)
                string.replace_str(&CFString::new(text), CFRange::init(utf16_start, 0));
                if needs_zwnj {
                    let zwnjs_pos = string.char_len();
                    self.zwnjs_scratch_space.push(zwnjs_pos as usize);
                    string.replace_str(
                        &CFString::from_static_string(ZWNJ_STR),
                        CFRange::init(zwnjs_pos, 0),
                    );
                }
                let utf16_end = string.char_len();

                let cf_range = CFRange::init(utf16_start, utf16_end - utf16_start);
                let font = self.sized_font(run.font_id, font_size);
                max_ascent = max_ascent.max(font.ascent);
                max_descent = max_descent.max(font.descent);

                unsafe {
                    string.set_attribute(cf_range, kCTFontAttributeName, &font.font);
                    if has_tracking {
                        string.set_attribute(
                            cf_range,
                            kCTTrackingAttributeName,
                            &CFNumber::from(run.letter_spacing.0),
                        );
                        if needs_zwnj {
                            string.set_attribute(
                                CFRange::init(utf16_end - 1, 1),
                                kCTTrackingAttributeName,
                                &CFNumber::from(0.0f32),
                            );
                        }
                    }
                }
            }
        }
        // Retrieve the glyphs from the shaped line, converting UTF16 offsets to UTF8 offsets.
        drop(attributes);
        let shape = playbit_trace::Span::detail("gpui.text.native_shape");
        let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
        drop(shape);
        let _extract = playbit_trace::Span::detail("gpui.text.native_extract");
        let glyph_runs = line.glyph_runs();
        let mut runs = <Vec<ShapedRun>>::with_capacity(glyph_runs.len() as usize);
        let mut ix_converter = StringIndexConverter::new(text);
        for run in glyph_runs.into_iter() {
            let attributes = run.attributes().unwrap();
            let font =
                unsafe { attributes.get(kCTFontAttributeName).downcast::<CTFont>().unwrap() };
            let font_id = self.id_for_native_font(font);

            let mut glyphs = match runs.last_mut() {
                Some(run) if run.font_id == font_id => &mut run.glyphs,
                _ => {
                    runs.push(ShapedRun {
                        font_id,
                        glyphs: Vec::with_capacity(run.glyph_count().try_into().unwrap_or(0)),
                    });
                    &mut runs.last_mut().unwrap().glyphs
                }
            };
            for ((&glyph_id, position), &glyph_utf16_ix) in
                run.glyphs().iter().zip(run.positions().iter()).zip(run.string_indices().iter())
            {
                let mut glyph_utf16_ix = usize::try_from(glyph_utf16_ix).unwrap();
                let r = self.zwnjs_scratch_space.binary_search_by(|&it| it.cmp(&glyph_utf16_ix));
                match r {
                    // this glyph is a ZWNJ, skip it
                    Ok(_) => continue,
                    // adjust the index to account for the ZWNJs we've inserted
                    Err(idx) => glyph_utf16_ix -= idx * ZWNJ_SIZE_16,
                }
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(glyph_id as u32),
                    position: point(position.x as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
            }
        }
        let typographic_bounds = line.get_typographic_bounds();
        LineLayout {
            runs,
            font_size,
            width: typographic_bounds.width.into(),
            ascent: max_ascent.into(),
            descent: max_descent.into(),
            len: text.len(),
        }
    }
}

#[derive(Debug, Clone)]
struct StringIndexConverter<'a> {
    text: &'a str,
    /// Index in UTF-8 bytes
    utf8_ix: usize,
    /// Index in UTF-16 code units
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, utf8_ix: 0, utf16_ix: 0 }
    }

    fn advance_to_utf8_ix(&mut self, utf8_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf8_ix + ix >= utf8_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

impl From<Metrics> for FontMetrics {
    fn from(metrics: Metrics) -> Self {
        FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: metrics.bounding_box.into(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for FontkitWeight {
    fn from(value: FontWeight) -> Self {
        FontkitWeight(value.0)
    }
}

impl From<FontStyle> for FontkitStyle {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => FontkitStyle::Normal,
            FontStyle::Italic => FontkitStyle::Italic,
            FontStyle::Oblique => FontkitStyle::Oblique,
        }
    }
}

// Some fonts may have no attributes despite `core_text` requiring them (and panicking).
// This is the same version as `core_text` has without `expect` calls.
mod lenient_font_attributes {
    use core_foundation::{
        base::{CFRetain, CFType, TCFType},
        string::{CFString, CFStringRef},
    };
    use core_text::font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttribute, kCTFontFamilyNameAttribute,
    };

    pub fn family_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { get_string_attribute(descriptor, kCTFontFamilyNameAttribute) }
    }

    fn get_string_attribute(
        descriptor: &CTFontDescriptor,
        attribute: CFStringRef,
    ) -> Option<String> {
        unsafe {
            let value = CTFontDescriptorCopyAttribute(descriptor.as_concrete_TypeRef(), attribute);
            if value.is_null() {
                return None;
            }

            let value = CFType::wrap_under_create_rule(value);
            assert!(value.instance_of::<CFString>());
            let s = wrap_under_get_rule(value.as_CFTypeRef() as CFStringRef);
            Some(s.to_string())
        }
    }

    unsafe fn wrap_under_get_rule(reference: CFStringRef) -> CFString {
        unsafe {
            assert!(!reference.is_null(), "Attempted to create a NULL object.");
            let reference = CFRetain(reference as *const ::std::os::raw::c_void) as CFStringRef;
            TCFType::wrap_under_create_rule(reference)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{FontRun, FontWeight, GlyphId, MacTextSystem, PlatformTextSystem, font, px};

    #[test]
    fn playbit_inter_variable_medium_resolves_a_distinct_weight() {
        use std::borrow::Cow;
        let fonts = MacTextSystem::new();
        fonts
            .add_fonts(vec![Cow::Borrowed(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../playbit/assets/InterVariable.ttf"
            )))])
            .unwrap();
        let mut regular = font("Inter Variable");
        regular.weight = FontWeight::NORMAL;
        let mut medium = regular.clone();
        medium.weight = FontWeight::MEDIUM;
        let regular_id = fonts.font_id(&regular).unwrap();
        let medium_id = fonts.font_id(&medium).unwrap();
        assert_ne!(regular_id, medium_id);
        let state = fonts.0.read();
        assert!(
            state.fonts[medium_id.0].properties().weight.0
                > state.fonts[regular_id.0].properties().weight.0
        );
    }

    #[test]
    fn test_layout_line_bom_char() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let line = "\u{feff}";
        let mut style = FontRun { letter_spacing: Default::default(), font_id, len: line.len() };

        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b

        let line = "\u{feff}ab";
        let font_runs = &[
            FontRun { letter_spacing: Default::default(), len: "\u{feff}".len(), font_id },
            FontRun { letter_spacing: Default::default(), len: "ab".len(), font_id },
        ];
        let layout = fonts.layout_line(line, px(16.), font_runs);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b
    }

    #[test]
    fn test_layout_line_zwnj_insertion() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello world";
        let font_runs = &[
            FontRun { letter_spacing: Default::default(), font_id, len: 5 }, // "hello"
            FontRun { letter_spacing: Default::default(), font_id, len: 6 }, // " world"
        ];

        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with different font runs - should not insert ZWNJ
        let font_id2 = fonts.font_id(&font("Times")).unwrap_or(font_id);
        let font_runs_different = &[
            FontRun { letter_spacing: Default::default(), font_id, len: 5 }, // "hello"
            // " world"
            FontRun { letter_spacing: Default::default(), font_id: font_id2, len: 6 },
        ];

        let layout2 = fonts.layout_line(text, px(16.), font_runs_different);
        assert_eq!(layout2.len, text.len());

        for run in &layout2.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }
    }

    #[test]
    fn test_layout_line_zwnj_edge_cases() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello";
        let font_runs = &[FontRun { letter_spacing: Default::default(), font_id, len: 5 }];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        let text = "abc";
        let font_runs = &[
            FontRun { letter_spacing: Default::default(), font_id, len: 1 }, // "a"
            FontRun { letter_spacing: Default::default(), font_id, len: 1 }, // "b"
            FontRun { letter_spacing: Default::default(), font_id, len: 1 }, // "c"
        ];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with empty text
        let text = "";
        let font_runs = &[];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, 0);
        assert!(layout.runs.is_empty());
    }
}

#[cfg(test)]
mod tracking_tests {
    use super::*;
    use crate as gpui;

    fn text_system() -> gpui::WindowTextSystem {
        let native = MacTextSystem::new();
        native
            .add_fonts(vec![Cow::Borrowed(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../playbit/assets/InterVariable.ttf"
            )))])
            .unwrap();
        gpui::WindowTextSystem::new(Arc::new(gpui::TextSystem::new(Arc::new(native))))
    }

    fn style() -> gpui::TextStyle {
        gpui::TextStyle {
            font_family: "Inter Variable".into(),
            font_weight: gpui::FontWeight::MEDIUM,
            font_size: px(11.).into(),
            letter_spacing: px(0.22),
            ..Default::default()
        }
    }

    fn check_benchmark_budget(failed: bool, name: &str) {
        assert!(!failed, "{name} budget exceeded; retain every cohort above");
    }

    #[test]
    fn playbit_sized_fonts_preserve_identity_metrics_and_bounded_recency() {
        let native = MacTextSystem::new();
        let id = native.font_id(&gpui::font("Menlo")).unwrap();
        let other = native.font_id(&gpui::font("Helvetica")).unwrap();
        let mut state = native.0.write();
        let metrics = state.fonts[id.0].metrics();
        let first = state.sized_font(id, px(11.)).font.clone();
        for size in 12..43 {
            state.sized_font(id, px(size as f32));
        }
        assert_eq!(state.sized_fonts.len(), SIZED_FONT_LIMIT);
        let reused = state.sized_font(id, px(11.));
        assert_eq!(first.as_concrete_TypeRef(), reused.font.as_concrete_TypeRef());
        assert_eq!(reused.ascent, metrics.ascent * (11. / metrics.units_per_em as f32));
        assert_eq!(reused.descent, -metrics.descent * (11. / metrics.units_per_em as f32));
        let different = state.sized_font(other, px(11.)).font.clone();
        assert_ne!(first.as_concrete_TypeRef(), different.as_concrete_TypeRef());
        assert!(!state.sized_fonts.iter().any(|entry| entry.id == id && entry.size == px(12.)));
        assert!(state.sized_fonts.iter().any(|entry| entry.id == id && entry.size == px(11.)));
        for size in 43..1000 {
            let entry = state.sized_font(id, px(size as f32));
            assert_eq!(entry.font.pt_size(), size as f64);
            assert_eq!(state.sized_fonts.len(), SIZED_FONT_LIMIT);
            assert_eq!(state.sized_fonts.capacity(), SIZED_FONT_LIMIT);
        }
    }

    #[test]
    fn playbit_reusing_sized_fonts_preserves_native_glyph_geometry() {
        let native = MacTextSystem::new();
        native
            .add_fonts(vec![Cow::Borrowed(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../playbit/assets/InterVariable.ttf"
            )))])
            .unwrap();
        let id = native.font_id(&style().to_run(0).font).unwrap();
        let mut state = native.0.write();
        for size in [11., 12., 14., 28.] {
            for text in ["Connected", "Pending synchronization", "e\u{301}x", "👩🏽‍💻x", "office"]
            {
                for spacing in [0., 0.22] {
                    let runs =
                        [FontRun { len: text.len(), font_id: id, letter_spacing: px(spacing) }];
                    state.sized_fonts.clear();
                    let fresh = state.layout_line(text, px(size), &runs);
                    let retained = state.layout_line(text, px(size), &runs);
                    assert_eq!(
                        (fresh.width, fresh.ascent, fresh.descent, fresh.len),
                        (retained.width, retained.ascent, retained.descent, retained.len)
                    );
                    assert_eq!(fresh.runs.len(), retained.runs.len());
                    for (a, b) in fresh.runs.iter().zip(&retained.runs) {
                        assert_eq!(a.font_id, b.font_id);
                        assert_eq!(a.glyphs.len(), b.glyphs.len());
                        for (a, b) in a.glyphs.iter().zip(&b.glyphs) {
                            assert_eq!(
                                (a.id, a.position, a.index, a.is_emoji),
                                (b.id, b.position, b.index, b.is_emoji)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn playbit_tracking_preserves_clusters_and_shared_layout_geometry() {
        let system = text_system();
        let style = style();
        for text in ["Text value", "e\u{301}x", "👩🏽‍💻x", "office"] {
            let tracked = style.to_run(text.len());
            let mut plain = tracked.clone();
            plain.letter_spacing = px(0.);
            let before = system.layout_line(text, px(11.), &[plain], None);
            let after = system.layout_line(text, px(11.), &[tracked.clone()], None);
            assert!(after.width > before.width, "tracking must affect {text:?}");
            assert!(!std::sync::Arc::ptr_eq(&before, &after), "tracking belongs in the cache key");
            let again = system.layout_line(text, px(11.), &[tracked.clone()], None);
            assert!(std::sync::Arc::ptr_eq(&after, &again), "reuse the existing shaping cache");
            let wrapped =
                system.shape_text(text.to_owned().into(), px(11.), &[tracked], None, None).unwrap();
            assert_eq!(after.width, wrapped[0].unwrapped_layout.width);
            for run in &after.runs {
                for glyph in &run.glyphs {
                    assert!(text.is_char_boundary(glyph.index));
                    assert_eq!(
                        after.x_for_index(glyph.index),
                        wrapped[0].unwrapped_layout.x_for_index(glyph.index)
                    );
                }
            }
            if let Some(end) = text.strip_suffix('x') {
                let indices: Vec<_> = after
                    .runs
                    .iter()
                    .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
                    .collect();
                assert!(
                    indices.iter().all(|i| *i == 0 || *i == end.len()),
                    "no spacing inside a composed cluster: {indices:?}"
                );
            }
        }
        let plain = {
            let mut run = style.to_run(2);
            run.letter_spacing = px(0.);
            run
        };
        let tracked = style.to_run(2);
        // A zero-tracking run following a tracked run must not inherit tracking.
        let separate = system.layout_line("AB", px(11.), &[tracked.clone()], None).width
            + system.layout_line("CD", px(11.), &[plain.clone()], None).width;
        let mixed = system.layout_line("ABCD", px(11.), &[tracked, plain], None);
        assert!((mixed.width - separate).abs() < px(0.01));
        let first = style.to_run(1);
        let mut second = first.clone();
        second.font = gpui::font("Menlo");
        let expected = system.layout_line("A", px(11.), &[first.clone()], None).width
            + system.layout_line("B", px(11.), &[second.clone()], None).width;
        let mixed = system.layout_line("AB", px(11.), &[first, second], None);
        assert_eq!(mixed.runs.len(), 2, "equal decoration/tracking must not merge distinct fonts");
        assert!((mixed.width - expected).abs() < px(0.01));
    }

    #[test]
    #[ignore = "optimized native shaping benchmark; excludes rendering and input"]
    fn benchmark_control_tracking() {
        assert!(!cfg!(debug_assertions), "run native shaping benchmarks with --release");
        let style = style();
        let system = text_system();
        let native = MacTextSystem::new();
        native
            .add_fonts(vec![Cow::Borrowed(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../playbit/assets/InterVariable.ttf"
            )))])
            .unwrap();
        let id = native.font_id(&style.to_run(0).font).unwrap();
        let state = native.0.read();
        let font = &state.fonts[id.0];
        for compact in [false, true] {
            let mut samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                let start = std::time::Instant::now();
                if compact {
                    let sized = font.native_font().clone_with_font_size(11.);
                    std::hint::black_box((sized.ascent(), sized.descent()));
                } else {
                    let metrics = font.metrics();
                    let scale = 11. / metrics.units_per_em as f32;
                    std::hint::black_box((metrics.ascent * scale, -metrics.descent * scale));
                    std::hint::black_box(font.native_font().clone_with_font_size(11.));
                }
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            eprintln!(
                "font_metrics compact={compact} n=1000 p50={} p95={} p99={} max={} ns",
                samples[499], samples[949], samples[989], samples[999]
            );
        }
        drop(state);
        let mut failed = false;
        for bytes in [128, 4096] {
            for unit in ["a", "e\u{301}", "👩🏽‍💻"] {
                let suffix = unit.repeat((bytes - 4) / unit.len());
                for tracking in [0., 0.22] {
                    let mut run = style.to_run(suffix.len() + 4);
                    run.letter_spacing = px(tracking);
                    let mut cold = Vec::with_capacity(1000);
                    let mut warm = Vec::with_capacity(1000);
                    for index in 0..1000 {
                        // Unique equal-sized content forces uncached shaping; the second
                        // lookup uses the same text and the ordinary layout cache.
                        let text: gpui::SharedString = format!("{index:04}{suffix}").into();
                        let start = std::time::Instant::now();
                        std::hint::black_box(system.layout_line(
                            &text,
                            px(11.),
                            &[run.clone()],
                            None,
                        ));
                        cold.push(start.elapsed().as_nanos());
                        let start = std::time::Instant::now();
                        std::hint::black_box(system.layout_line(
                            &text,
                            px(11.),
                            &[run.clone()],
                            None,
                        ));
                        warm.push(start.elapsed().as_nanos());
                    }
                    cold.sort_unstable();
                    warm.sort_unstable();
                    eprintln!(
                        "tracking={tracking} bytes={} unit={unit:?} n=1000 cold_ns p50={} p95={} p99={} max={} warm_ns p50={} p95={} p99={} max={}",
                        suffix.len() + 4,
                        cold[499],
                        cold[949],
                        cold[989],
                        cold[999],
                        warm[499],
                        warm[949],
                        warm[989],
                        warm[999]
                    );
                    failed |= cold[989] >= if bytes == 128 { 300_000 } else { 2_000_000 };
                    failed |= warm[989] >= 20_000;
                }
            }
        }
        check_benchmark_budget(failed, "native shaping");
    }

    #[test]
    fn playbit_tracked_truncation_fits_the_painted_width() {
        let system = text_system();
        let style = style();
        let text: gpui::SharedString = "a".repeat(128).into();
        let tracked = style.to_run(text.len());
        let mut plain = tracked.clone();
        plain.letter_spacing = px(0.);
        let width = system.layout_line(&text, px(11.), &[plain], None).width.ceil() + px(1.);
        let mut runs = vec![tracked];
        let (shortened, changed) = system.truncate_line(text, px(11.), width, "…", &runs);
        if let Some(changed) = changed {
            runs = changed;
        }
        let painted = system.layout_line(&shortened, px(11.), &runs, None);
        assert!(
            painted.width <= width,
            "truncated label paints at {}px within {}px",
            painted.width.0,
            width.0
        );
    }

    #[test]
    fn playbit_truncation_keeps_graphemes_and_original_styled_runs() {
        use unicode_segmentation::UnicodeSegmentation;
        let system = text_system();
        let style = style();
        for unit in ["office", "e\u{301}", "👩🏽‍💻", "العربية"] {
            let text: gpui::SharedString = unit.repeat(12).into();
            let mut other = style.to_run(text.len() - unit.len());
            other.font = gpui::font("Menlo");
            other.letter_spacing = px(0.);
            let runs = vec![style.to_run(unit.len()), other];
            let original = system.layout_line(&text, px(11.), &runs, None);
            for width in [px(0.), px(4.), px(40.), original.width / 2., original.width] {
                let (result, shortened) =
                    system.truncate_line(text.clone(), px(11.), width, "…", &runs);
                let painted = system.layout_line(
                    &result,
                    px(11.),
                    shortened.as_deref().unwrap_or(&runs),
                    None,
                );
                assert!(painted.width <= width, "{unit:?}: {result:?} exceeds {width:?}");
                if let Some(prefix) = result.strip_suffix('…') {
                    assert!(text.starts_with(prefix));
                    assert!(text.grapheme_indices(true).any(|(index, _)| index == prefix.len()));
                }
                assert_eq!(system.layout_line(&text, px(11.), &runs, None).width, original.width);
                assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), text.len());
            }
        }
    }

    #[test]
    #[ignore = "optimized native truncation benchmark; includes shaping, excludes painting"]
    fn benchmark_control_truncation() {
        assert!(!cfg!(debug_assertions), "run native truncation benchmarks with --release");
        let style = style();
        let mut failed = false;
        for bytes in [128, 4096] {
            for unit in ["a", "e\u{301}", "👩🏽‍💻"] {
                for tracking in [0., 0.22] {
                    let system = text_system();
                    let tail = unit.repeat((bytes - 4) / unit.len());
                    let mut run = style.to_run(tail.len() + 4);
                    run.letter_spacing = px(tracking);
                    let sample = format!("0000{tail}");
                    let width =
                        system.layout_line(&sample, px(11.), &[run.clone()], None).width / 2.;
                    let mut cold = Vec::with_capacity(1000);
                    let mut warm = Vec::with_capacity(1000);
                    for index in 0..1000 {
                        let text: gpui::SharedString = format!("{:04}{tail}", index + 1000).into();
                        for cached in [false, true] {
                            let start = std::time::Instant::now();
                            let (result, runs) = system.truncate_line(
                                text.clone(),
                                px(11.),
                                width,
                                "…",
                                &[run.clone()],
                            );
                            let elapsed = start.elapsed().as_nanos();
                            if cached {
                                warm.push(elapsed);
                            } else {
                                cold.push(elapsed);
                            }
                            assert!(
                                system
                                    .layout_line(&result, px(11.), runs.as_deref().unwrap(), None)
                                    .width
                                    <= width
                            );
                        }
                    }
                    cold.sort_unstable();
                    warm.sort_unstable();
                    eprintln!(
                        "truncation tracking={tracking} bytes={} unit={unit:?} n=1000 cold_ns p50={} p99={} max={} warm_ns p50={} p99={} max={}",
                        sample.len(),
                        cold[499],
                        cold[989],
                        cold[999],
                        warm[499],
                        warm[989],
                        warm[999]
                    );
                    failed |= cold[989] >= if bytes == 128 { 1_000_000 } else { 4_000_000 };
                    failed |= warm[989] >= 100_000;
                }
            }
        }
        check_benchmark_budget(failed, "native truncation");
    }
}
