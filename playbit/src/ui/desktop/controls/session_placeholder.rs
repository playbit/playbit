//! Sidebar loading decoration; painting schedules only the next display frame.
use super::*;
use std::time::{Duration, Instant};

const SHIMMER_DURATION: Duration = Duration::from_millis(1500);

impl Control {
    pub fn set_session_icon_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.app_icon_loading != loading {
            self.app_icon_loading = loading;
            cx.notify();
        }
    }

    pub fn set_session_info_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        let error = error.map(SharedString::from);
        if self.session_info_error != error {
            self.session_info_error = error;
            cx.notify();
        }
    }

    pub fn set_session_placeholders(
        &mut self,
        icon_loading: bool,
        name: Option<SessionName>,
        cx: &mut Context<Self>,
    ) {
        if self.app_icon_loading != icon_loading || self.session_name != name {
            let loading = matches!(name, Some(SessionName::Loading(_)));
            if loading && self.name_loading_since.is_none() {
                self.name_loading_since = Some(Instant::now());
            } else if !loading {
                self.name_loading_since = None;
            }
            self.app_icon_loading = icon_loading;
            self.session_name = name;
            cx.notify();
        }
    }

    pub(super) fn session_placeholder(
        &self,
        name: &SessionName,
        foreground: gpui::Hsla,
        window: &Window,
    ) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        let (id, failed) = match name {
            SessionName::Loading(id) => (id, false),
            SessionName::Unavailable(id) => (id, true),
        };
        let mut content = div().flex_1().min_w_0().flex().items_center().gap(sp(6.));
        if failed {
            content =
                content.child(div().flex_1().min_w_0().text_ellipsis().child("App unavailable"));
        } else {
            let start = self.name_loading_since;
            content = content.child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| paint_shimmer(bounds, start, foreground, window),
                )
                .flex_1()
                .min_w_0()
                .h(sp(9.)),
            );
        }
        content.child(div().flex_none().text_color(foreground.alpha(0.3)).child(id.clone()))
    }
}

fn paint_shimmer(
    bounds: Bounds<Pixels>,
    start: Option<Instant>,
    foreground: gpui::Hsla,
    window: &mut Window,
) {
    if bounds.size.width <= px(0.) || !window.content_mask().bounds.intersects(&bounds) {
        return;
    }
    window.paint_quad(gpui::fill(bounds, foreground.alpha(0.1)));
    #[cfg(target_os = "macos")]
    if crate::ui::desktop::appearance::reduce_motion() {
        return;
    }
    let Some(start) = start else { return };
    let phase = start.elapsed().as_secs_f32() / SHIMMER_DURATION.as_secs_f32();
    let width = bounds.size.width;
    let left = bounds.origin.x + width * (phase.fract() * 2. - 1.);
    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
        let clear = gpui::linear_color_stop(foreground.alpha(0.), 0.);
        let dark = gpui::linear_color_stop(foreground.alpha(0.1), 1.);
        for (x, angle) in [(left, 90.), (left + width / 2., 270.)] {
            let half = Bounds {
                origin: gpui::point(x, bounds.origin.y),
                size: gpui::size(width / 2., bounds.size.height),
            };
            window.paint_quad(gpui::fill(half, gpui::linear_gradient(angle, clear, dark)));
        }
    });
    window.request_animation_frame();
}
