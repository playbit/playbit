//! Passive AP transport status. Animation is driven only by rendered frames.
use super::*;
use crate::engine::reply::{ApConnectionState, ApConnectionStatus};
use crate::ui::desktop::zoom;
use std::time::Instant;

mod spinner;

pub(super) struct Indicator {
    status: Option<ApConnectionStatus>,
    hovered: bool,
    tooltip: String,
    tooltip_dirty: bool,
    window: Option<gpui::AnyWindowHandle>,
    animation_start: Instant,
    spinner: Option<spinner::Spinner>,
    frame: Option<Arc<gpui::RenderImage>>,
}

impl Indicator {
    pub(super) fn new(_: &mut Context<Self>) -> Self {
        Self {
            status: None,
            hovered: false,
            tooltip: String::new(),
            tooltip_dirty: false,
            window: None,
            animation_start: Instant::now(),
            spinner: None,
            frame: None,
        }
    }

    pub(super) fn known(&self) -> bool {
        self.status.is_some()
    }

    pub(super) fn live(&self) -> bool {
        self.status.as_ref().is_some_and(|status| status.state == ApConnectionState::Live)
    }

    pub(super) fn hostname(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.hostname.as_str())
    }

    pub(super) fn work_offline(&self) -> bool {
        self.status.as_ref().is_some_and(|status| status.state == ApConnectionState::OfflineMode)
    }

    pub(super) fn set(&mut self, status: Option<ApConnectionStatus>, cx: &mut Context<Self>) {
        if self.status == status {
            return;
        }
        if self.status.as_ref().map(|status| status.state)
            != status.as_ref().map(|status| status.state)
        {
            self.animation_start = Instant::now();
        }
        self.tooltip = status.as_ref().map_or_else(String::new, |status| match status.state {
            ApConnectionState::Live => format!("Connected to {}", status.hostname),
            ApConnectionState::Connecting => format!("Connecting to {}", status.hostname),
            ApConnectionState::Offline => format!("Offline — can’t reach {}", status.hostname),
            ApConnectionState::OfflineMode => {
                "Offline mode. Turn off Work Offline to connect.".into()
            }
        });
        self.status = status;
        self.tooltip_dirty = true;
        if self.status.is_none() {
            self.hovered = false;
            let frame = self.frame.take();
            if let Some(window) = self.window.take() {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| {
                        controls::set_native_tooltip(window, None);
                        if let Some(frame) = frame {
                            let _ = window.drop_image(frame);
                        }
                    });
                });
            }
        }
        cx.notify();
    }
}

impl Render for Indicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(status) = &self.status else { return div().into_any_element() };
        self.window = Some(window.window_handle());
        if self.hovered && self.tooltip_dirty {
            controls::set_native_tooltip(window, Some(&self.tooltip));
        }
        self.tooltip_dirty = false;
        let (asset, label) = match status.state {
            ApConnectionState::Live => ("ap-live", "Live"),
            ApConnectionState::Connecting => ("ap-connecting", "Connecting"),
            ApConnectionState::Offline => ("ap-offline", "Offline"),
            ApConnectionState::OfflineMode => ("ap-offline", "Offline mode"),
        };
        let scale = f32::from(window.rem_size()) / 16.;
        let display = window.scale_factor();
        let sp = |value: f32| px((value * scale * display).round() / display);
        let icon = if status.state == ApConnectionState::Connecting {
            let angle =
                self.animation_start.elapsed().as_secs_f64().fract() as f32 * std::f32::consts::TAU;
            let frame = self
                .spinner
                .get_or_insert_with(spinner::Spinner::new)
                .frame(angle, f32::from(sp(24.)) * window.scale_factor() / 24.);
            let image = gpui::img(frame.clone()).size(sp(24.)).into_any_element();
            if let Some(previous) = self.frame.replace(frame) {
                let _ = window.drop_image(previous);
            }
            window.request_animation_frame();
            image
        } else {
            if let Some(frame) = self.frame.take() {
                let _ = window.drop_image(frame);
            }
            gpui::img(asset).size(sp(24.)).into_any_element()
        };
        let description = self.tooltip.clone();
        let id = gpui::accesskit::NodeId(cx.entity_id().as_u64() * 2);
        div()
            .w(sp(96.))
            .h(sp(24.))
            .flex_none()
            .relative()
            .debug_selector(|| "ap-status".into())
            .when(self.hovered, |root| {
                root.child(
                    div()
                        .absolute()
                        .right(sp(26.))
                        .h_full()
                        .flex()
                        .items_center()
                        .font_family(UI_FONT_FAMILY)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(sp(11.))
                        .line_height(sp(16.))
                        .letter_spacing(zoom::scaled(window, 0.11))
                        .text_color(rgba(0x00000080))
                        .whitespace_nowrap()
                        .debug_selector(|| "ap-status-label".into())
                        .child(label),
                )
            })
            .child(
                div()
                    .id("ap-status-icon")
                    .absolute()
                    .right_0()
                    .size(sp(24.))
                    .cursor_default()
                    .debug_selector(|| "ap-status-icon".into())
                    .on_hover(cx.listener(|this, hovered, window, cx| {
                        this.hovered = *hovered;
                        controls::set_native_tooltip(
                            window,
                            hovered.then_some(this.tooltip.as_str()),
                        );
                        cx.notify();
                    }))
                    .child(icon)
                    .child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                let mut node = crate::ui::desktop::text_accessibility_node(
                                    &description,
                                    false,
                                    bounds,
                                );
                                node.set_live(gpui::accesskit::Live::Polite);
                                window.insert_accessibility_node(id, node, false, None);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests;
