use gpui::{Div, FontFeatures, Styled as _};
use std::sync::{Arc, OnceLock};

pub(crate) fn tabular_features() -> FontFeatures {
    static FEATURES: OnceLock<FontFeatures> = OnceLock::new();
    FEATURES.get_or_init(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)]))).clone()
}

pub(super) fn identifier_features() -> FontFeatures {
    static FEATURES: OnceLock<FontFeatures> = OnceLock::new();
    FEATURES
        .get_or_init(|| FontFeatures(Arc::new(vec![("ss02".into(), 1), ("tnum".into(), 1)])))
        .clone()
}

pub(crate) fn tabular_numerals(mut element: Div) -> Div {
    element.text_style().get_or_insert_with(Default::default).font_features =
        Some(tabular_features());
    element
}
