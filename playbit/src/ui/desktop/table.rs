//! Compact two-column table presentation. Controls retain selection, focus and accessibility.
use super::{UI_FONT_FAMILY, zoom};
use gpui::{AnyElement, Div, FontWeight, Hsla, SharedString, Window, div, prelude::*};
#[cfg(any(feature = "desktop", test))]
use gpui::{rgba, svg};

pub(crate) const ROW_HEIGHT: f32 = 24.;

/// The first column follows the store's ascending key order; this is an indicator, not an action.
#[cfg(any(feature = "desktop", test))]
pub(crate) fn header([key, value]: [Div; 2], window: &Window) -> Div {
    let sp = |value| zoom::pixels(window, value);
    div()
        .relative()
        .w_full()
        .h(sp(32.))
        .flex_none()
        .flex()
        .items_center()
        .gap(sp(8.))
        .pl(sp(12.))
        .pr(sp(4.))
        .py(sp(4.))
        .font_family(UI_FONT_FAMILY)
        .text_size(sp(11.))
        .line_height(sp(16.))
        .letter_spacing(zoom::scaled(window, 0.11))
        .child(
            div()
                .debug_selector(|| "table-key-header".into())
                .w(sp(104.))
                .flex_none()
                .h(sp(24.))
                .flex()
                .items_center()
                .gap(sp(8.))
                .pr(sp(4.))
                .py(sp(4.))
                .border_r(sp(1.))
                .border_color(rgba(0x0000001a))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0x000000e6))
                .child(key.flex_1().min_w_0().truncate())
                .child(
                    svg()
                        .path("table-sort-ascending")
                        .size(sp(16.))
                        .flex_none()
                        .text_color(rgba(0x000000e6)),
                ),
        )
        .child(
            div()
                .debug_selector(|| "table-value-header".into())
                .flex_1()
                .min_w_0()
                .h(sp(24.))
                .flex()
                .items_center()
                .pr(sp(4.))
                .py(sp(4.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(0x00000066))
                .child(value.flex_1().min_w_0().truncate()),
        )
        .child(div().absolute().bottom_0().left_0().right_0().h(sp(1.)).bg(rgba(0x0000001a)))
}

pub(crate) fn row(
    [key, value]: [SharedString; 2],
    binary_value: bool,
    foreground: Hsla,
    background: Option<Hsla>,
    focus: Option<Hsla>,
    accessory: Option<AnyElement>,
    window: &Window,
) -> Div {
    let sp = |value| zoom::pixels(window, value);
    div().w_full().h(sp(ROW_HEIGHT)).px(sp(4.)).child(
        div()
            .debug_selector(|| "table-row-content".into())
            .relative()
            .w_full()
            .h(sp(ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(sp(8.))
            .px(sp(8.))
            .rounded(sp(4.))
            .font_family(UI_FONT_FAMILY)
            .text_size(sp(11.))
            .line_height(sp(16.))
            .letter_spacing(zoom::scaled(window, 0.11))
            .text_color(foreground)
            .when_some(background, |row, color| row.bg(color))
            .child(
                div()
                    .debug_selector(|| "table-key-cell".into())
                    .w(sp(104.))
                    .flex_none()
                    .h(sp(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(div().min_w_0().flex_1().text_ellipsis().child(key)),
            )
            .child(value_cell(value, binary_value, accessory, window))
            .when_some(focus, |row, color| {
                row.child(
                    div()
                        .debug_selector(|| "table-focus-ring".into())
                        .absolute()
                        .inset_0()
                        .rounded(sp(4.))
                        .border(sp(2.))
                        .border_color(color),
                )
            }),
    )
}

fn value_cell(
    value: SharedString,
    binary_value: bool,
    accessory: Option<AnyElement>,
    window: &Window,
) -> Div {
    let sp = |value| zoom::pixels(window, value);
    let mut value_cell = div()
        .debug_selector(|| "table-value-cell".into())
        .flex_1()
        .min_w_0()
        .h(sp(ROW_HEIGHT))
        .flex()
        .items_center()
        .gap(sp(6.))
        .font_weight(FontWeight::NORMAL)
        .when(binary_value, |cell| cell.font_family("JetBrains Mono NL"))
        .child(div().min_w_0().flex_1().text_ellipsis().child(value))
        .children(accessory);
    static FEATURES: std::sync::OnceLock<gpui::FontFeatures> = std::sync::OnceLock::new();
    value_cell.text_style().get_or_insert_with(Default::default).font_features = Some(
        FEATURES
            .get_or_init(|| {
                gpui::FontFeatures(std::sync::Arc::new(vec![
                    ("ss02".into(), 1),
                    ("cv03".into(), 1),
                    ("cv04".into(), 1),
                ]))
            })
            .clone(),
    );
    value_cell
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Context, Pixels, Render, point, px, size};

    struct Table {
        width: f32,
        focused: bool,
        accessory: bool,
    }
    impl Render for Table {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(zoom::pixels(window, self.width))
                .flex()
                .flex_col()
                .child(header([div().child("Key"), div().child("Value")], window))
                .child(div().h(zoom::pixels(window, 8.)))
                .child(row(
                    ["a very long key that must fit its cell".into(), "0x9aa0c7f3".into()],
                    true,
                    rgba(0xffffffff).into(),
                    Some(rgba(0x000000b3).into()),
                    self.focused.then(|| rgba(0x000000e6).into()),
                    self.accessory.then(|| {
                        div()
                            .debug_selector(|| "table-accessory".into())
                            .size(zoom::pixels(window, 24.))
                            .flex_none()
                            .into_any_element()
                    }),
                    window,
                ))
        }
    }

    #[gpui::test]
    fn compact_cells_follow_the_header_grid_without_focus_layout_shift(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            for width in [480., 240.] {
                let window = cx.add_window(|window, cx| {
                    zoom::apply(window, cx);
                    Table { width, focused: false, accessory: false }
                });
                cx.simulate_window_scale_factor(window.into(), 2.);
                cx.run_until_parked();
                let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
                let factor = scale as f32 / 10_000.;
                let bounds = |visual: &mut gpui::VisualTestContext, name: &'static str| {
                    visual.debug_bounds(name).expect("current table geometry")
                };
                let key_header = bounds(&mut visual, "table-key-header");
                let value_header = bounds(&mut visual, "table-value-header");
                let content = bounds(&mut visual, "table-row-content");
                let key = bounds(&mut visual, "table-key-cell");
                let value = bounds(&mut visual, "table-value-cell");
                let close = |actual: Pixels, expected: f32| {
                    assert!((f32::from(actual) - expected * factor).abs() <= 0.5);
                };
                close(key_header.left(), 12.);
                close(key_header.top(), 4.);
                close(key_header.size.height, 24.);
                close(key_header.size.width, 104.);
                close(value_header.left(), 124.);
                close(content.left(), 4.);
                close(content.top(), 40.);
                close(content.size.width, width - 8.);
                close(content.size.height, ROW_HEIGHT);
                close(key.left(), 12.);
                close(value.left(), 124.);
                close(key.size.width, 104.);
                close(value.size.width, width - 136.);
                assert!(value.right() <= content.right());
                window
                    .update(cx, |table, _, cx| {
                        table.focused = true;
                        table.accessory = true;
                        cx.notify();
                    })
                    .expect("keyboard focus");
                visual.run_until_parked();
                assert_eq!(bounds(&mut visual, "table-key-cell"), key);
                assert_eq!(bounds(&mut visual, "table-value-cell"), value);
                assert_eq!(bounds(&mut visual, "table-row-content"), content);
                assert_eq!(bounds(&mut visual, "table-focus-ring"), content);
                let accessory = bounds(&mut visual, "table-accessory");
                assert_eq!(accessory.size, size(px(24. * factor), px(24. * factor)));
                assert_eq!(accessory.right(), value.right());
                assert_eq!(
                    content,
                    Bounds::new(
                        point(px(4. * factor), px(40. * factor)),
                        size(px((width - 8.) * factor), px(ROW_HEIGHT * factor))
                    )
                );
                window.update(cx, |_, window, _| window.remove_window()).expect("close table");
            }
        }
    }
}
