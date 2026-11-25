use super::*;
use crate::ui::desktop::scrollbar::Scrollbars;

impl Shell {
    pub(super) fn application_scrollbars(&mut self, cx: &mut Context<Self>) -> Entity<Scrollbars> {
        self.applications
            .scrolling
            .get_or_insert_with(|| cx.new(|_| Scrollbars::with_corner_radius(false, 16.)))
            .clone()
    }

    pub(in crate::desktop) fn application_columns_viewport(
        &mut self,
        content: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let bars = self.application_scrollbars(cx);
        let handle = bars.read(cx).handle();
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .child(content.id("application-columns").overflow_x_scroll().track_scroll(&handle))
            .child(Scrollbars::overlay(&bars))
    }
}

/// Keep each vertical rail outside its viewport's content mask and offset.
pub(super) fn vertical_viewport(
    content: gpui::Stateful<gpui::Div>,
    scrolling: &mut Option<Entity<Scrollbars>>,
    cx: &mut Context<Shell>,
) -> gpui::Div {
    let bars = scrolling.get_or_insert_with(|| cx.new(|_| Scrollbars::new(false)));
    let handle = bars.read(cx).handle();
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(content.track_scroll(&handle))
        .child(Scrollbars::overlay(bars))
}
