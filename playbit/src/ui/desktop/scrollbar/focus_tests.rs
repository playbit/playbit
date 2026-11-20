//! Renderer coverage for logical focus, nested clipping and transactional retries.
#![allow(clippy::unwrap_used, reason = "Tests assert complete focus and scroll exchanges.")]
use super::*;
use gpui::{FocusHandle, Render, div};
use std::{cell::Cell, rc::Rc};

struct FocusFixture {
    outer: ScrollHandle,
    inner: ScrollHandle,
    sibling: ScrollHandle,
    focus: FocusHandle,
    navigation_focus: FocusHandle,
    target: Rc<Cell<Bounds<Pixels>>>,
    target_y: f32,
    tracked: bool,
    sibling_request: Rc<Cell<bool>>,
}

impl FocusFixture {
    fn sibling(&self) -> impl IntoElement {
        let request = self.sibling_request.clone();
        div()
            .id("unrelated-scroll-pane")
            .absolute()
            .top_0()
            .w(px(280.))
            .h(px(100.))
            .overflow_y_scroll()
            .track_scroll(&self.sibling)
            .child(
                div().relative().w(px(280.)).h(px(400.)).child(
                    canvas(
                        move |bounds, window, _| {
                            if request.replace(false) {
                                window.request_autoscroll(bounds.dilate(px(8.)));
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top(px(200.))
                    .w(px(120.))
                    .h(px(24.)),
                ),
            )
    }
}

impl Render for FocusFixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let target = self.target.clone();
        let focus = self.focus.clone();
        let control = div()
            .id("automatic-focus-target")
            .track_focus(&focus)
            .absolute()
            .top(px(self.target_y))
            .w(px(120.))
            .h(px(24.))
            .on_mouse_down(MouseButton::Left, move |_, window, _| focus.focus(window))
            .child(canvas(move |bounds, _, _| target.set(bounds), |_, _, _, _| {}).size_full());
        let inner = div()
            .id("automatic-focus-inner")
            .absolute()
            .top(px(400.))
            .w(px(280.))
            .h(px(100.))
            .overflow_y_scroll()
            .when(self.tracked, |view| view.track_scroll(&self.inner))
            .child(div().relative().w(px(280.)).h(px(400.)).child(control));
        div()
            .id("automatic-focus-outer")
            .track_focus(&self.navigation_focus)
            .w(px(300.))
            .h(px(180.))
            .overflow_y_scroll()
            .track_scroll(&self.outer)
            .on_key_down(|event, window, cx| {
                if event.keystroke.key == "tab" {
                    window.focus_next();
                    cx.stop_propagation();
                }
            })
            .child(div().relative().w(px(300.)).h(px(800.)).child(inner).child(self.sibling()))
    }
}

#[gpui::test]
fn logical_focus_reveals_tracked_and_untracked_ancestors_once(cx: &mut gpui::TestAppContext) {
    // Pointer, programmatic and Tab focus reveal the whole target with margin. A later
    // sibling's ordinary request still moves its own pane, while focus wins in the parent.
    for tracked in [true, false] {
        let outer = ScrollHandle::new();
        let inner = ScrollHandle::new();
        let sibling = ScrollHandle::new();
        let target = Rc::new(Cell::new(Bounds::default()));
        let window = cx.add_window(|window, cx| {
            let focus = cx.focus_handle().tab_stop(true);
            focus.focus(window);
            window.activate_window();
            FocusFixture {
                outer: outer.clone(),
                inner: inner.clone(),
                sibling: sibling.clone(),
                focus,
                navigation_focus: cx.focus_handle(),
                target: target.clone(),
                target_y: 200.,
                tracked,
                sibling_request: Rc::new(Cell::new(true)),
            }
        });
        cx.run_until_parked();
        assert_eq!(outer.offset().y, px(-320.));
        assert_eq!(sibling.offset().y, px(-132.), "ordinary requests remain local to their pane");
        if tracked {
            assert_eq!(inner.offset().y, px(-132.));
        }
        assert_eq!(target.get().top(), px(148.));
        assert_eq!(target.get().bottom(), px(172.));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        window
            .update(&mut visual, |_, window, _| {
                window.blur();
                outer.set_offset(point(px(0.), px(-300.)));
                window.refresh();
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(target.get().top(), px(168.), "the target is only partially visible");
        visual.simulate_click(point(px(60.), px(174.)), Default::default());
        visual.run_until_parked();
        assert_eq!(outer.offset().y, px(-320.), "pointer focus reveals the clipped remainder");
        window
            .update(&mut visual, |_, window, _| {
                outer.set_offset(point(px(0.), px(-280.)));
                window.refresh();
            })
            .unwrap();
        visual.run_until_parked();
        visual.deactivate_window();
        window
            .update(&mut visual, |view, window, _| {
                assert!(view.focus.is_focused(window));
                window.activate_window();
                window.refresh();
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(
            outer.offset().y,
            px(-280.),
            "same focus and reactivation preserve manual scroll"
        );
        // A real shell retains a focused ancestor while navigating with Tab.
        window.update(&mut visual, |view, window, _| view.navigation_focus.focus(window)).unwrap();
        visual.run_until_parked();
        visual.simulate_keystrokes("tab");
        window
            .update(&mut visual, |view, window, _| assert!(view.focus.is_focused(window)))
            .unwrap();
        assert_eq!(outer.offset().y, px(-320.), "a genuine Tab focus change reveals again");
    }
}

struct RetryFixture {
    state: gpui::ListState,
    inner: ScrollHandle,
    focus: FocusHandle,
    attempts: Rc<Cell<usize>>,
    target: Rc<Cell<Bounds<Pixels>>>,
}

impl Render for RetryFixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let inner = self.inner.clone();
        let attempts = self.attempts.clone();
        let target = self.target.clone();
        gpui::list(self.state.clone(), move |_, _, _| {
            let first = attempts.get() == 0;
            let attempts = attempts.clone();
            let target = target.clone();
            div()
                .relative()
                .w(px(300.))
                .h(px(400.))
                .child(
                    div()
                        .id("retry-inner")
                        .absolute()
                        .top(px(200.))
                        .w(px(280.))
                        .h(px(100.))
                        .overflow_y_scroll()
                        .track_scroll(&inner)
                        .child(
                            div().relative().w(px(280.)).h(px(400.)).child(
                                div()
                                    .id("retry-focused-control")
                                    .track_focus(&focus)
                                    .absolute()
                                    .top(px(if first { 200. } else { 8. }))
                                    .w(px(120.))
                                    .h(px(24.))
                                    .child(
                                        canvas(
                                            move |bounds, _, _| {
                                                attempts.set(attempts.get() + 1);
                                                target.set(bounds);
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .size_full(),
                                    ),
                            ),
                        ),
                )
                .into_any()
        })
        .w(px(300.))
        .h(px(100.))
    }
}

#[gpui::test]
fn failed_list_prepaint_cannot_apply_its_obsolete_focus_offset(cx: &mut gpui::TestAppContext) {
    // The first real List attempt queues an inner move, then retries at a new item origin.
    // Its replacement target already fits: only rollback cancellation can prevent the old move.
    let inner = ScrollHandle::new();
    let attempts = Rc::new(Cell::new(0));
    let target = Rc::new(Cell::new(Bounds::default()));
    let window = cx.add_window(|window, cx| {
        let focus = cx.focus_handle();
        focus.focus(window);
        RetryFixture {
            state: gpui::ListState::new(1, gpui::ListAlignment::Top, px(0.)),
            inner: inner.clone(),
            focus,
            attempts: attempts.clone(),
            target: target.clone(),
        }
    });
    cx.run_until_parked();
    assert!(attempts.get() >= 2, "List retried its first prepaint");
    assert_eq!(inner.max_offset().height, px(300.));
    assert_eq!(inner.offset().y, px(0.), "the discarded attempt's offset must never apply");
    assert_eq!(target.get().top(), px(8.));
    assert_eq!(target.get().bottom(), px(32.));
    window.update(cx, |view, window, _| assert!(view.focus.is_focused(window))).unwrap();
}

#[gpui::test]
fn deferred_focus_offsets_retire_across_repeated_draws_and_focus_replacement(
    cx: &mut gpui::TestAppContext,
) {
    // Change logical focus after prepaint queues its moves but before deferred effects run.
    // Repeated public draws must retain cancellation, including A→B→A with new geometry.
    for transition in ["blur", "other", "return"] {
        let outer = ScrollHandle::new();
        let inner = ScrollHandle::new();
        let window = cx.add_window(|_, cx| FocusFixture {
            outer: outer.clone(),
            inner: inner.clone(),
            sibling: ScrollHandle::new(),
            focus: cx.focus_handle(),
            navigation_focus: cx.focus_handle(),
            target: Rc::new(Cell::new(Bounds::default())),
            target_y: 200.,
            tracked: true,
            sibling_request: Rc::new(Cell::new(false)),
        });
        cx.run_until_parked();
        let (focus, other, view) = window
            .update(cx, |view, _, cx| (view.focus.clone(), cx.focus_handle(), cx.entity()))
            .unwrap();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                focus.focus(window);
                let _ = window.draw(cx);
                window.refresh();
                let _ = window.draw(cx);
                match transition {
                    "blur" => window.blur(),
                    "other" => other.focus(window),
                    "return" => {
                        other.focus(window);
                        focus.focus(window);
                        view.update(cx, |view, cx| {
                            view.target_y = 8.;
                            cx.notify();
                        });
                        let _ = window.draw(cx);
                    }
                    _ => unreachable!(),
                }
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(outer.offset().y, px(if transition == "return" { -260. } else { 0. }));
        assert_eq!(inner.offset().y, px(0.));
        window
            .update(cx, |view, window, cx| {
                view.target_y = 200.;
                window.blur();
                focus.focus(window);
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(outer.offset().y, px(-320.), "a later real focus change still reveals");
        assert_eq!(inner.offset().y, px(-132.));
    }
}
