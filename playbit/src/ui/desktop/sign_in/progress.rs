//! Shared progress artwork, animated at display cadence only while busy.
use gpui::{Context, Render, Window, prelude::*};
use std::time::Instant;

#[derive(Default)]
pub(super) struct Progress {
    started: Option<Instant>,
}

impl Progress {
    pub(super) fn set_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if running == self.started.is_some() {
            return;
        }
        self.started = running.then(Instant::now);
        cx.notify();
    }
}

impl Render for Progress {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let angle = self.started.map_or(0., |started| {
            window.request_animation_frame();
            started.elapsed().as_secs_f64().fract() as f32 * std::f32::consts::TAU
        });
        gpui::svg()
            .path("progress-spinner")
            .size(crate::ui::desktop::zoom::pixels(window, 16.))
            .flex_shrink_0()
            .text_color(gpui::rgb(0x000000))
            .with_transformation(gpui::Transformation::rotate(gpui::radians(angle)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[gpui::test]
    fn display_frames_animate_only_while_running(cx: &mut gpui::TestAppContext) {
        // Display callbacks advance busy progress without waiting for a timer;
        // form redraws preserve the start, and stopping drains animation demand.
        let window = cx.add_window(|_, _| Progress::default());
        let indicator = window.root(cx).expect("progress window exists");
        let notifications = Rc::new(Cell::new(0));
        let observed = notifications.clone();
        let _subscription =
            cx.update(|cx| cx.observe(&indicator, move |_, _| observed.set(observed.get() + 1)));
        indicator.update(cx, |indicator, cx| {
            assert!(indicator.started.is_none());
            indicator.set_running(true, cx);
            let started = indicator.started.expect("busy progress starts animation");
            indicator.set_running(true, cx);
            assert_eq!(indicator.started, Some(started), "redraw must not restart animation");
        });
        cx.run_until_parked();
        for _ in 0..3 {
            let before = notifications.get();
            cx.simulate_window_frame(window.into());
            cx.run_until_parked();
            assert!(notifications.get() > before, "each display callback must advance progress");
        }
        indicator.update(cx, |indicator, cx| {
            indicator.set_running(false, cx);
            assert!(indicator.started.is_none());
        });
        cx.run_until_parked();
        cx.simulate_window_frame(window.into());
        cx.run_until_parked();
        let stopped = notifications.get();
        for _ in 0..3 {
            cx.simulate_window_frame(window.into());
            cx.run_until_parked();
        }
        assert_eq!(notifications.get(), stopped, "idle progress must stop requesting frames");
    }
}
