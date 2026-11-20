use super::*;
use crate::{MacTextSystem, point};

fn native_system() -> (Arc<MacTextSystem>, TextSystem, RenderGlyphParams) {
    let native = Arc::new(MacTextSystem::new());
    let font_id = native.font_id(&font("Helvetica")).unwrap();
    let glyph_id = native.glyph_for_char(font_id, 'A').unwrap();
    let params = RenderGlyphParams {
        font_id,
        glyph_id,
        font_size: px(13.),
        subpixel_variant: point(0, 0),
        scale_factor: 2.,
        is_emoji: false,
    };
    let system = TextSystem::new(native.clone());
    (native, system, params)
}

#[test]
fn playbit_turnover_recomputes_identical_native_geometry_without_table_growth() {
    let (native, system, original) = native_system();
    let expected = native.glyph_raster_bounds(&original).unwrap();
    assert_eq!(system.raster_bounds(&original).unwrap(), expected);
    // Thousands of valid size/scale keys model repeated changing-text scenes.
    // Bounds are compared with the native implementation, not a copied formula.
    let mut full_capacity = None;
    for index in 0..RASTER_BOUNDS_LIMIT * 3 {
        let params = RenderGlyphParams {
            font_size: px(10. + (index % 256) as f32 / 16.),
            scale_factor: 1. + (index / 256) as f32 / 64.,
            ..original.clone()
        };
        let expected = native.glyph_raster_bounds(&params).unwrap();
        assert_eq!(system.raster_bounds(&params).unwrap(), expected);
        let cache = system.raster_bounds.read();
        assert!(cache.len() <= RASTER_BOUNDS_LIMIT);
        if cache.len() == RASTER_BOUNDS_LIMIT {
            match full_capacity {
                Some(capacity) => assert_eq!(cache.capacity(), capacity),
                None => full_capacity = Some(cache.capacity()),
            }
        }
    }
    assert!(full_capacity.is_some(), "exercise actual full-cache turnover");
    assert!(!system.raster_bounds.read().contains_key(&original));
    assert_eq!(system.raster_bounds(&original).unwrap(), expected);
    assert_eq!(system.raster_bounds(&original).unwrap(), expected);
    assert_eq!(system.raster_bounds.read().capacity(), full_capacity.unwrap());
}

#[test]
fn playbit_all_render_key_dimensions_remain_isolated_across_eviction() {
    let (native, system, original) = native_system();
    let other_font = native.font_id(&font("Menlo")).unwrap();
    let cases = [
        original.clone(),
        RenderGlyphParams {
            font_id: other_font,
            glyph_id: native.glyph_for_char(other_font, 'A').unwrap(),
            ..original.clone()
        },
        RenderGlyphParams {
            glyph_id: native.glyph_for_char(original.font_id, 'W').unwrap(),
            ..original.clone()
        },
        RenderGlyphParams { font_size: px(27.), ..original.clone() },
        RenderGlyphParams { subpixel_variant: point(1, 2), ..original.clone() },
        RenderGlyphParams { scale_factor: 1., ..original.clone() },
        RenderGlyphParams { is_emoji: true, ..original.clone() },
    ];
    let expected: Vec<_> =
        cases.iter().map(|params| native.glyph_raster_bounds(params).unwrap()).collect();
    assert_ne!(expected[0], expected[3], "font size changes real geometry");
    assert_ne!(expected[0], expected[5], "backing scale changes real geometry");
    for (params, bounds) in cases.iter().zip(&expected) {
        assert_eq!(system.raster_bounds(params).unwrap(), *bounds);
    }
    assert_eq!(system.raster_bounds.read().len(), cases.len());
    for index in 0..RASTER_BOUNDS_LIMIT {
        system
            .raster_bounds(&RenderGlyphParams {
                font_size: px(10. + index as f32 / 256.),
                ..original.clone()
            })
            .unwrap();
    }
    assert!(cases.iter().all(|params| !system.raster_bounds.read().contains_key(params)));
    for (params, bounds) in cases.iter().zip(&expected) {
        assert_eq!(system.raster_bounds(params).unwrap(), *bounds);
    }
    assert!(cases.iter().all(|params| system.raster_bounds.read().contains_key(params)));
}
