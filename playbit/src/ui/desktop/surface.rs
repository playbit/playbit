//! Layout-driven surface delivery, retaining only the last admitted geometry.
//! A rejected enqueue keeps it dirty; the next layout after queue progress retries.
use super::scene::{Input, SendInput};
use gpui::{canvas, prelude::*};
use playbit_api::abi::{environment::Surface, syscall::EventKind};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
pub(super) struct Observer {
    sent: Rc<Cell<Option<Surface>>>,
}
impl Observer {
    pub fn reset(&self) {
        self.sent.set(None);
    }
    fn notify(&self, surface: Surface, send: &SendInput) {
        if self.sent.get() != Some(surface) {
            let data = surface.encode().expect("AppKit supplies a finite positive display scale");
            if send(Input {
                kind: EventKind::SurfaceChanged,
                target: 0,
                data: data.to_vec(),
                revision: None,
            }) {
                self.sent.set(Some(surface));
            }
        }
    }
    pub fn element(&self, send: SendInput, right_inset: f32) -> impl IntoElement {
        let observer = self.clone();
        canvas(
            move |bounds, window, _| {
                let zoom = f32::from(window.rem_size()) / 16.;
                let surface = Surface {
                    // Both Mac hosts use a 16-point inset on each viewport edge.
                    width: (f32::from(bounds.size.width) / zoom - 32. - right_inset).max(0.).floor()
                        as u32,
                    height: (f32::from(bounds.size.height) / zoom - 32.).max(0.).floor() as u32,
                    pixel_scale: window.scale_factor() * zoom,
                    inputs: Surface::POINTER | Surface::KEYBOARD,
                };
                observer.notify(surface, &send);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    #[test]
    fn accepted_geometry_deduplicates_and_failed_admission_retries_after_progress() {
        // Reject admission once, then accept and suppress repeats. Resetting for
        // an app replacement delivers the unchanged viewport to the new instance.
        let events = Arc::new(Mutex::new(Vec::new()));
        let accepts = Arc::new(AtomicBool::new(false));
        let retained = events.clone();
        let allowed = accepts.clone();
        let send: SendInput = Arc::new(move |input| {
            if !allowed.load(Ordering::Relaxed) {
                return false;
            }
            retained.lock().expect("test lock").push(input);
            true
        });
        let observer = Observer::default();
        let surface = Surface { width: 336, height: 416, pixel_scale: 2.0, inputs: 3 };
        observer.notify(surface, &send);
        assert_eq!(observer.sent.get(), None);
        accepts.store(true, Ordering::Relaxed);
        observer.notify(surface, &send);
        observer.notify(surface, &send);
        assert_eq!(events.lock().expect("test lock").len(), 1);
        observer.reset();
        observer.notify(surface, &send);
        observer.notify(Surface { width: 640, ..surface }, &send);
        let events = events.lock().expect("test lock");
        assert_eq!(events.len(), 3);
        for event in events.iter() {
            assert_eq!(event.kind, EventKind::SurfaceChanged);
            assert_eq!(event.target, 0);
            assert_eq!(event.data.len(), 16);
            assert_eq!(event.revision, None);
        }
    }
}
