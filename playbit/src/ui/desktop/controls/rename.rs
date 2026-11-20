//! Selected rows may rename after one uninterrupted second following a click.
//! Each control retains at most one task; pointer dispatch invalidates it without listeners.
use super::*;

const RENAME_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

impl Control {
    /// Refreshing an enabled callback preserves a pending click. Invoke the
    /// latest callback only after rechecking selection and pointer activity.
    pub fn set_delayed_rename(&mut self, rename: Option<Activate>) {
        if rename.is_none() {
            self.cancel_delayed_rename();
        }
        self.delayed_rename = rename;
    }

    pub(super) fn cancel_delayed_rename(&mut self) {
        self.rename_press = None;
        self.rename_task = None;
    }

    pub(super) fn prepare_delayed_rename(&mut self, event: &gpui::MouseDownEvent, window: &Window) {
        self.cancel_delayed_rename();
        if self.delayed_rename.is_some()
            && self.navigation == Some(true)
            && !self.disabled
            && event.button == MouseButton::Left
            && event.click_count == 1
            && !event.modifiers.modified()
        {
            self.rename_press = Some(window.pointer_interaction_epoch());
        }
    }

    pub(super) fn arm_delayed_rename(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(press) = self.rename_press.take() else { return false };
        let epoch = window.pointer_interaction_epoch();
        if press.checked_add(1) != Some(epoch)
            || event.click_count != 1
            || event.modifiers.modified()
            || self.navigation != Some(true)
            || self.disabled
            || self.delayed_rename.is_none()
        {
            return false;
        }
        self.rename_task = Some(cx.spawn_in(window, async move |control, cx| {
            cx.background_executor().timer(RENAME_DELAY).await;
            let _ = cx.update(|window, cx| {
                let callback = control
                    .update(cx, |this, _| {
                        this.rename_task = None;
                        (window.pointer_interaction_epoch() == epoch
                            && window.is_window_active()
                            && this.focus.is_focused(window)
                            && this.navigation == Some(true)
                            && !this.disabled)
                            .then(|| this.delayed_rename.clone().map(|rename| (rename, this.id)))
                            .flatten()
                    })
                    .ok()
                    .flatten();
                if let Some((rename, id)) = callback {
                    rename(id, cx);
                }
            });
        }));
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated pointer and timer behavior.")]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fixture(
        cx: &mut gpui::TestAppContext,
        selected: bool,
    ) -> (gpui::WindowHandle<Control>, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let renamed = Rc::new(Cell::new(0));
        let activated = Rc::new(Cell::new(0));
        let window = cx.add_window(|_, cx| {
            let activated = activated.clone();
            let weak: gpui::WeakEntity<Control> = cx.weak_entity();
            let mut control = Control::new(
                1,
                Properties {
                    kind: Kind::Button(ButtonStyle::Default),
                    label: "Renameable row",
                    disabled: false,
                    theme: &Theme::light(),
                },
                Rc::new(move |_, cx| {
                    activated.set(activated.get() + 1);
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |this, cx| this.select_navigation(true, cx));
                    });
                }),
                cx,
            );
            let renamed = renamed.clone();
            control.set_delayed_rename(Some(Rc::new(move |_, _| renamed.set(renamed.get() + 1))));
            control.select_navigation(selected, cx);
            control
        });
        window.update(cx, |_, window, _| window.activate_window()).unwrap();
        cx.run_until_parked();
        (window, renamed, activated)
    }

    #[gpui::test]
    fn selected_row_renames_after_one_second_with_the_current_callback(
        cx: &mut gpui::TestAppContext,
    ) {
        // Timer expiry uses the latest binding without restarting when a row is rendered again.
        let (window, old_callback, activated) = fixture(cx, true);
        window
            .update(cx, |control, _, cx| control.set_navigation_selection_active(false, cx))
            .unwrap();
        let current = Rc::new(Cell::new(0));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(point(px(12.), px(12.)), Default::default());
        visual.executor().advance_clock(std::time::Duration::from_millis(999));
        visual.run_until_parked();
        assert_eq!(old_callback.get(), 0);
        let callback = current.clone();
        window
            .update(cx, |this, _, _| {
                this.set_delayed_rename(Some(Rc::new(move |_, _| {
                    callback.set(callback.get() + 1)
                })));
            })
            .unwrap();
        visual.executor().advance_clock(std::time::Duration::from_millis(1));
        visual.run_until_parked();
        assert_eq!(old_callback.get(), 0);
        assert_eq!(current.get(), 1);
        assert_eq!(activated.get(), 0);
    }

    #[gpui::test]
    fn selecting_a_row_does_not_arm_rename(cx: &mut gpui::TestAppContext) {
        // Selection during mousedown cannot turn the initial click into a rename candidate.
        let (window, renamed, activated) = fixture(cx, false);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(point(px(12.), px(12.)), MouseButton::Left, Default::default());
        assert_eq!(activated.get(), 1, "initial selection dispatches on pointer down");
        window.update(cx, |control, _, _| assert_eq!(control.navigation, Some(true))).unwrap();
        visual.simulate_mouse_up(point(px(12.), px(12.)), MouseButton::Left, Default::default());
        assert_eq!(activated.get(), 1, "release cannot repeat the selection action");
        visual.executor().advance_clock(RENAME_DELAY);
        visual.run_until_parked();
        assert_eq!(renamed.get(), 0);
        visual.simulate_click(point(px(12.), px(12.)), Default::default());
        assert_eq!(activated.get(), 1, "an already-selected name click waits for rename");
        visual.executor().advance_clock(RENAME_DELAY);
        visual.run_until_parked();
        assert_eq!(renamed.get(), 1);
    }

    #[gpui::test]
    fn intervening_pointer_input_cancels_rename(cx: &mut gpui::TestAppContext) {
        // Outside input, zero-delta gestures and an exited file drag retire deferred intent.
        let position = point(px(12.), px(12.));
        let events = [
            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: position + point(px(1.), px(0.)),
                ..Default::default()
            }),
            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position,
                delta: gpui::ScrollDelta::Lines(point(0., 1.)),
                ..Default::default()
            }),
            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position,
                is_gesture: true,
                touch_phase: gpui::TouchPhase::Started,
                ..Default::default()
            }),
            gpui::PlatformInput::MouseExited(gpui::MouseExitEvent::default()),
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                position: point(px(5000.), px(5000.)),
                button: MouseButton::Right,
                click_count: 1,
                ..Default::default()
            }),
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                position: point(px(5000.), px(5000.)),
                button: MouseButton::Right,
                click_count: 1,
                ..Default::default()
            }),
            gpui::PlatformInput::PointerActivity,
            gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Exited),
        ];
        for event in events {
            let (window, renamed, _) = fixture(cx, true);
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.simulate_click(position, Default::default());
            gpui::AnyWindowHandle::from(window)
                .update(cx, |_, window, cx| window.dispatch_event(event, cx))
                .unwrap();
            visual.executor().advance_clock(RENAME_DELAY);
            visual.run_until_parked();
            assert_eq!(renamed.get(), 0);
            window.update(cx, |_, window, _| window.remove_window()).unwrap();
        }
    }

    #[gpui::test]
    fn double_click_keeps_activation_without_renaming(cx: &mut gpui::TestAppContext) {
        // A second press cancels the first click's timer and preserves ordinary double-click work.
        let (window, renamed, activated) = fixture(cx, true);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let position = point(px(12.), px(12.));
        visual.simulate_click(position, Default::default());
        visual.executor().advance_clock(std::time::Duration::from_millis(100));
        visual.simulate_event(gpui::MouseDownEvent {
            position,
            button: MouseButton::Left,
            click_count: 2,
            ..Default::default()
        });
        visual.simulate_event(gpui::MouseUpEvent {
            position,
            button: MouseButton::Left,
            click_count: 2,
            ..Default::default()
        });
        visual.executor().advance_clock(RENAME_DELAY);
        visual.run_until_parked();
        assert_eq!(renamed.get(), 0);
        assert_eq!(activated.get(), 1);
    }

    #[gpui::test]
    fn control_lifecycle_changes_cancel_rename(cx: &mut gpui::TestAppContext) {
        // Temporary deselection, disabling, blur or deactivation cannot revive a previous click.
        for action in 0..7 {
            let (window, renamed, _) = fixture(cx, true);
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.simulate_click(point(px(12.), px(12.)), Default::default());
            match action {
                0 => window
                    .update(cx, |this, _, cx| {
                        this.select_navigation(false, cx);
                        this.select_navigation(true, cx);
                    })
                    .unwrap(),
                1 => window.update(cx, |this, _, _| this.retire()).unwrap(),
                2 => window
                    .update(cx, |this, _, cx| {
                        for disabled in [true, false] {
                            this.set(
                                Properties {
                                    kind: this.kind,
                                    label: "Renameable row",
                                    disabled,
                                    theme: &Theme::light(),
                                },
                                cx,
                            );
                        }
                    })
                    .unwrap(),
                3 => {
                    visual.deactivate_window();
                    window.update(cx, |_, window, _| window.activate_window()).unwrap();
                }
                4 => {
                    visual.simulate_keystrokes("a");
                }
                5 => {
                    let other = window
                        .update(cx, |_, window, cx| {
                            let other = cx.focus_handle();
                            window.focus(&other);
                            other
                        })
                        .unwrap();
                    visual.run_until_parked();
                    window.update(cx, |this, window, _| window.focus(&this.focus)).unwrap();
                    drop(other);
                }
                _ => window.update(cx, |this, _, _| this.set_delayed_rename(None)).unwrap(),
            }
            visual.executor().advance_clock(RENAME_DELAY);
            visual.run_until_parked();
            assert_eq!(renamed.get(), 0);
            window.update(cx, |_, window, _| window.remove_window()).unwrap();
        }
    }
}
