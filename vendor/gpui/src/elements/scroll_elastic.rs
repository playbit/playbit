//! macOS trackpad elasticity. AppKit supplies the same platform equations used
//! by WebKit's ScrollAnimationRubberBand/ScrollingEffectsController. These are
//! private AppKit SPI: intentionally platform-local, with no guest ABI exposure.
use super::ScrollHandle;
use crate::{App, Pixels, Point, ScrollWheelEvent, Size, TouchPhase, Window, point, px};
use std::time::Instant;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn _NSElasticDeltaForReboundDelta(delta: f64) -> f64;
    fn _NSReboundDeltaForElasticDelta(delta: f64) -> f64;
    fn _NSElasticDeltaForTimeDelta(position: f64, velocity: f64, elapsed: f64) -> f64;
}

#[derive(Debug, Default)]
pub(super) struct ElasticScroll {
    stretch: Point<Pixels>,
    velocity: Point<Pixels>,
    last_input: Option<Instant>,
    rebound: Option<(Instant, Point<Pixels>, Point<Pixels>)>,
    scheduled: bool,
    ignore_momentum: bool,
}

impl ElasticScroll {
    pub(super) fn reset(&mut self) {
        self.stretch = Point::default();
        self.rebound = None;
        self.velocity = Point::default();
        self.ignore_momentum = true;
    }
}
impl ScrollHandle {
    /// Enable native macOS trackpad elasticity for this retained viewport.
    pub fn enable_elastic_scroll(&self) {
        self.0.borrow_mut().elastic.get_or_insert_with(Default::default);
    }
    pub(super) fn elastic_enabled(&self) -> bool {
        self.0.borrow().elastic.is_some()
    }
    /// Visual displacement outside the ordinary clamped scroll range.
    pub fn elastic_offset(&self) -> Point<Pixels> {
        self.0.borrow().elastic.as_ref().map_or(Point::default(), |e| e.stretch)
    }
    pub(super) fn reset_elastic(&self) {
        if let Some(e) = self.0.borrow_mut().elastic.as_mut() {
            e.reset();
        }
    }
    pub(super) fn elastic_input(
        &self,
        event: &ScrollWheelEvent,
        delta: Point<Pixels>,
        maximum: Size<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let now = Instant::now();
        {
            let mut state = self.0.borrow_mut();
            let offset = state.offset.clone();
            let Some(e) = state.elastic.as_mut() else { return };
            if event.momentum_phase.is_none() && matches!(event.touch_phase, TouchPhase::Started) {
                e.rebound = None;
                e.ignore_momentum = false;
                e.last_input = None;
            }
            if event.momentum_phase.is_some() && e.ignore_momentum {
                return;
            }
            let ended = matches!(event.momentum_phase, Some(TouchPhase::Ended))
                || (event.momentum_phase.is_none()
                    && matches!(event.touch_phase, TouchPhase::Ended));
            if !ended {
                e.rebound = None;
                let mut base = offset.borrow_mut();
                let base = &mut *base;
                let raw = event.unaccelerated_delta.unwrap_or(delta);
                for (position, stretch, change, raw_change, max) in [
                    (&mut base.x, &mut e.stretch.x, delta.x, raw.x, maximum.width),
                    (&mut base.y, &mut e.stretch.y, delta.y, raw.y, maximum.height),
                ] {
                    if max <= px(0.) || change == px(0.) {
                        continue;
                    }
                    if *stretch != px(0.) {
                        let force = unsafe { _NSReboundDeltaForElasticDelta(stretch.to_f64()) }
                            + raw_change.to_f64();
                        if force.signum() != stretch.to_f64().signum() {
                            *stretch = px(0.);
                            *position = (*position + px(force as f32)).clamp(-max, px(0.));
                        } else {
                            *stretch = px(unsafe { _NSElasticDeltaForReboundDelta(force) } as f32)
                                .clamp(px(-512.), px(512.));
                        }
                    } else {
                        let next = *position + change;
                        *position = next.clamp(-max, px(0.));
                        *stretch = px(unsafe {
                            _NSElasticDeltaForReboundDelta((next - *position).to_f64())
                        } as f32)
                        .clamp(px(-512.), px(512.));
                    }
                }
                e.velocity = e.last_input.map_or(Point::default(), |last| {
                    let dt = now.duration_since(last).as_secs_f32();
                    if dt > 0. && dt < 0.1 {
                        (delta / dt).map(|v| v.clamp(px(-100_000.), px(100_000.)))
                    } else {
                        Point::default()
                    }
                });
                e.last_input = Some(now);
            }
            if e.stretch != Point::default() && (ended || event.momentum_phase.is_some()) {
                // AppKit's velocity parameter uses the direction opposite the
                // content translation. A paused gesture releases without velocity.
                let velocity = if e
                    .last_input
                    .is_some_and(|last| now.duration_since(last).as_secs_f32() < 0.1)
                {
                    e.velocity
                } else {
                    Point::default()
                };
                e.rebound = Some((now, e.stretch, velocity));
                e.ignore_momentum = true;
            }
        }
        window.refresh();
        self.schedule_elastic(window, cx);
    }
    fn schedule_elastic(&self, window: &mut Window, _: &mut App) {
        {
            let mut state = self.0.borrow_mut();
            let Some(e) = state.elastic.as_mut() else { return };
            if e.scheduled || e.rebound.is_none() {
                return;
            }
            e.scheduled = true;
        }
        let handle = self.clone();
        window.on_next_frame(move |window, cx| {
            {
                let mut state = handle.0.borrow_mut();
                let Some(e) = state.elastic.as_mut() else { return };
                e.scheduled = false;
                let Some((start, position, velocity)) = e.rebound else { return };
                let elapsed = start.elapsed().as_secs_f64();
                let evaluate = |p: Pixels, v: Pixels| {
                    // Do not manufacture overscroll on an axis which stayed in range.
                    if p == px(0.) {
                        return px(0.);
                    }
                    let value =
                        unsafe { _NSElasticDeltaForTimeDelta(p.to_f64(), -v.to_f64(), elapsed) };
                    px((value as f32 * window.scale_factor()).round() / window.scale_factor())
                        .clamp(px(-512.), px(512.))
                };
                e.stretch =
                    point(evaluate(position.x, velocity.x), evaluate(position.y, velocity.y));
                if e.stretch == Point::default() || elapsed >= 2. {
                    e.stretch = Point::default();
                    e.rebound = None;
                }
            }
            window.refresh();
            handle.schedule_elastic(window, cx);
        });
    }
}
