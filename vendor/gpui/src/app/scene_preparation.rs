//! Prepare dirty scenes independently of the macOS display clock. Presentation
//! and native-frame callbacks remain owned by the platform frame loop.
use super::App;
use crate::AppContext;
use std::{cell::Cell, rc::Rc};

#[derive(Default)]
pub(super) struct ScenePreparation {
    callback: Option<Rc<dyn Fn()>>,
    next_window: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Render, TestAppContext, Window, WindowHandle};
    use std::cell::RefCell;

    struct Content {
        id: usize,
        renders: Rc<RefCell<Vec<usize>>>,
    }
    impl Render for Content {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl crate::IntoElement {
            self.renders.borrow_mut().push(self.id);
            crate::div()
        }
    }
    fn windows(
        cx: &mut TestAppContext,
        renders: &Rc<RefCell<Vec<usize>>>,
    ) -> Vec<WindowHandle<Content>> {
        (0..4).map(|id| cx.add_window(|_, _| Content { id, renders: renders.clone() })).collect()
    }

    #[crate::test]
    fn playbit_preparation_coalesces_and_stays_idle_without_a_display_callback(
        cx: &mut TestAppContext,
    ) {
        let renders = Rc::new(RefCell::new(Vec::new()));
        let windows = windows(cx, &renders);
        cx.run_until_parked();
        renders.borrow_mut().clear();
        cx.update(|cx| {
            for _ in 0..100 {
                for handle in &windows {
                    handle.update(cx, |_, window, _| window.refresh()).unwrap();
                }
            }
        });
        assert!(
            renders.borrow().is_empty(),
            "CPU work must be queued, not inline in notification dispatch"
        );
        cx.run_until_parked();
        assert_eq!(*renders.borrow(), [0, 1, 2, 3]);
        cx.run_until_parked();
        assert_eq!(renders.borrow().len(), 4, "no periodic work after becoming clean");
    }

    #[crate::test]
    fn playbit_preparation_rotates_and_removed_windows_do_not_retain_work(cx: &mut TestAppContext) {
        let renders = Rc::new(RefCell::new(Vec::new()));
        let windows = windows(cx, &renders);
        cx.run_until_parked();
        renders.borrow_mut().clear();
        cx.update(|cx| {
            for handle in &windows {
                handle.update(cx, |_, window, _| window.refresh()).unwrap();
            }
            cx.scene_preparation.next_window = 0;
            cx.prepare_one_scene();
            assert_eq!(*renders.borrow(), [0], "at most one window per executor turn");
            windows[0].update(cx, |_, window, _| window.refresh()).unwrap();
            windows[1].update(cx, |_, window, _| window.remove_window()).unwrap();
        });
        cx.run_until_parked();
        assert_eq!(
            *renders.borrow(),
            [0, 2, 3, 0],
            "a busy first window must not overtake siblings"
        );
    }

    #[crate::test]
    fn playbit_preparation_does_not_advance_display_driven_animation(cx: &mut TestAppContext) {
        let renders = Rc::new(RefCell::new(Vec::new()));
        let window = windows(cx, &renders)[0];
        cx.run_until_parked();
        let callbacks = Rc::new(Cell::new(0));
        window
            .update(cx, |_, window, _| {
                let callbacks = callbacks.clone();
                window.on_next_frame(move |window, _| {
                    callbacks.set(callbacks.get() + 1);
                    window.refresh();
                });
                window.refresh();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(callbacks.get(), 0);
        let count = renders.borrow().len();
        cx.test_window(window.into()).simulate_frame(crate::RequestFrameOptions::default());
        assert_eq!(callbacks.get(), 1);
        assert_eq!(renders.borrow().len(), count + 1);
        cx.run_until_parked();
        assert_eq!(renders.borrow().len(), count + 1);
    }

    #[crate::test]
    fn playbit_frame_completion_replaces_pending_work_and_follows_native_draw(
        cx: &mut TestAppContext,
    ) {
        // Retain only the latest observation while native frames are paused.
        // CPU preparation cannot deliver it, and reentrant work waits a frame.
        const REPLACEMENTS: usize = 1_000;
        let renders = Rc::new(RefCell::new(Vec::new()));
        let window = windows(cx, &renders)[0];
        cx.run_until_parked();
        let retained = Rc::new(());
        let completions = Rc::new(RefCell::new(Vec::new()));
        for revision in 0..REPLACEMENTS {
            let pending = retained.clone();
            let completions = completions.clone();
            window
                .update(cx, |_, window, _| {
                    window.on_next_frame_completed(move |window, _| {
                        assert!(!window.needs_present.get(), "platform draw must have returned");
                        completions.borrow_mut().push(revision);
                        drop(pending);
                        window.on_next_frame_completed(move |window, _| {
                            assert!(!window.needs_present.get());
                            completions.borrow_mut().push(REPLACEMENTS);
                        });
                    });
                    window.refresh();
                })
                .unwrap();
            assert_eq!(Rc::strong_count(&retained), 2, "one pending callback per window");
        }
        cx.run_until_parked();
        assert!(
            completions.borrow().is_empty(),
            "CPU scene preparation cannot complete a native frame"
        );
        window.update(cx, |_, window, _| assert!(window.needs_present.get())).unwrap();
        assert_eq!(Rc::strong_count(&retained), 2);
        cx.simulate_window_frame(window.into());
        assert_eq!(*completions.borrow(), [REPLACEMENTS - 1]);
        assert_eq!(Rc::strong_count(&retained), 1);
        cx.run_until_parked();
        assert_eq!(*completions.borrow(), [REPLACEMENTS - 1]);
        cx.simulate_window_frame(window.into());
        assert_eq!(*completions.borrow(), [REPLACEMENTS - 1, REPLACEMENTS]);
        cx.simulate_window_frame(window.into());
        assert_eq!(*completions.borrow(), [REPLACEMENTS - 1, REPLACEMENTS]);
    }

    #[crate::test]
    fn playbit_frame_completion_drops_work_on_window_removal(cx: &mut TestAppContext) {
        // Closing from a completion drops reentrant work. Removing a window
        // before its native frame or after frame entry also cancels observation.
        let renders = Rc::new(RefCell::new(Vec::new()));
        let windows = windows(cx, &renders);
        cx.run_until_parked();
        let retained = Rc::new(());
        let completions = Rc::new(Cell::new(0));
        windows[0]
            .update(cx, |_, window, _| {
                let retained = retained.clone();
                let completions = completions.clone();
                window.on_next_frame_completed(move |window, _| {
                    completions.set(completions.get() + 1);
                    let pending = retained.clone();
                    window.on_next_frame_completed(move |_, _| drop(pending));
                    window.remove_window();
                    window.on_next_frame_completed(move |_, _| drop(retained));
                });
            })
            .unwrap();
        cx.simulate_window_frame(windows[0].into());
        assert_eq!(completions.get(), 1);
        assert_eq!(Rc::strong_count(&retained), 1);
        windows[1]
            .update(cx, |_, window, _| {
                let retained = retained.clone();
                window.on_next_frame_completed(move |_, _| drop(retained));
                window.remove_window();
            })
            .unwrap();
        assert_eq!(Rc::strong_count(&retained), 1);
        windows[2]
            .update(cx, |_, window, _| {
                let retained = retained.clone();
                window.on_next_frame_completed(move |_, _| {
                    drop(retained);
                    panic!("a removed window cannot complete its frame");
                });
                window.on_next_frame(|window, _| window.remove_window());
            })
            .unwrap();
        cx.simulate_window_frame(windows[2].into());
        assert_eq!(Rc::strong_count(&retained), 1);
    }

    #[crate::test]
    fn playbit_frame_completion_registered_after_draw_waits_for_another_frame(
        cx: &mut TestAppContext,
    ) {
        // Register from a render's deferred effect, after its platform draw.
        // That observation must see a subsequent native draw before completion.
        struct DeferredCompletion {
            armed: bool,
            completions: Rc<Cell<usize>>,
        }
        impl Render for DeferredCompletion {
            fn render(
                &mut self,
                window: &mut Window,
                cx: &mut Context<Self>,
            ) -> impl crate::IntoElement {
                if self.armed {
                    self.armed = false;
                    let completions = self.completions.clone();
                    window.defer(cx, move |window, _| {
                        window.on_next_frame_completed(move |window, _| {
                            assert!(!window.needs_present.get());
                            completions.set(completions.get() + 1);
                        });
                    });
                }
                crate::div()
            }
        }
        let completions = Rc::new(Cell::new(0));
        let window = cx.add_window(|_, _| DeferredCompletion {
            armed: false,
            completions: completions.clone(),
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, _| {
                view.armed = true;
                window.refresh();
            })
            .unwrap();
        cx.simulate_window_frame(window.into());
        assert_eq!(completions.get(), 0, "the deferred registration happened after this draw");
        cx.run_until_parked();
        assert_eq!(completions.get(), 0);
        cx.simulate_window_frame(window.into());
        assert_eq!(completions.get(), 1);
        cx.simulate_window_frame(window.into());
        assert_eq!(completions.get(), 1);
    }
}

impl App {
    pub(crate) fn scene_preparation_callback(&mut self) -> Rc<dyn Fn()> {
        if let Some(callback) = &self.scene_preparation.callback {
            return callback.clone();
        }
        let pending = Rc::new(Cell::new(false));
        let cx = self.to_async(); // Weak app ownership; windows cannot keep the app alive.
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            if pending.replace(true) {
                return;
            }
            let pending = pending.clone();
            let delivery = playbit_trace::Interval::new("gpui.scene.preparation_delivery");
            cx.spawn(async move |cx| {
                drop(delivery);
                pending.set(false);
                let _ = cx.update(|cx| cx.prepare_one_scene());
            })
            .detach();
        });
        self.scene_preparation.callback = Some(callback.clone());
        callback
    }

    fn prepare_one_scene(&mut self) {
        // Use the registered windows themselves as the work set. Choosing by ID
        // gives round-robin service even if an earlier window stays busy, without
        // retaining removed windows or allocating a second request queue.
        let mut first = None;
        let mut next = None;
        for window in self.windows.values().filter_map(Option::as_deref) {
            if !window.invalidator.is_dirty() {
                continue;
            }
            let id = window.handle.window_id().as_u64();
            if first.is_none_or(|(key, _)| id < key) {
                first = Some((id, window.handle));
            }
            if id >= self.scene_preparation.next_window && next.is_none_or(|(key, _)| id < key) {
                next = Some((id, window.handle));
            }
        }
        let Some((id, handle)) = next.or(first) else {
            return;
        };
        self.scene_preparation.next_window = id.wrapping_add(1);
        self.update_window(handle, |_, window, cx| window.draw(cx).clear()).unwrap();
        if self
            .windows
            .values()
            .filter_map(Option::as_deref)
            .any(|window| window.invalidator.is_dirty())
        {
            self.scene_preparation_callback()();
        }
    }
}
