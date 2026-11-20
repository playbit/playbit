//! Explicit Mac renderer qualification. Uses real GPUI/font shaping and the
//! shared Canvas renderer; no profile, storage, engine or product data. Timing
//! ends after scene paint commands, before GPU completion/physical presentation.
//! Run with --pixels for unchanged, sparse and dense 360x280 pixel workloads.
#![allow(clippy::unwrap_used, reason = "Diagnostic fixtures assert valid construction.")]
use gpui::{
    App, Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use playbit::{
    runtime::{CanvasItem, ViewNode},
    ui::desktop::{controls::Assets, scene::Scene},
};
use playbit_api::ui::{CanvasFrames, CanvasRect, Color};
use std::{borrow::Cow, cell::RefCell, rc::Rc, sync::Arc, time::Instant};

const COUNTS: [usize; 4] = [0, 64, 1024, 4096];
const WARMUP: usize = 100;
const SAMPLES: usize = 1000;
const PIXEL_WIDTH: u32 = 360;
const PIXEL_HEIGHT: u32 = 280;

#[derive(Clone, Copy)]
enum PixelChange {
    Unchanged,
    Sparse,
    Dense,
}

#[derive(Clone, Copy)]
enum Cohort {
    Rects(usize),
    Text(usize),
    Pixels(PixelChange),
}
impl Cohort {
    fn name(self) -> String {
        match self {
            Self::Rects(count) => format!("rects_{count}"),
            Self::Text(count) => format!("text_{count}"),
            Self::Pixels(PixelChange::Unchanged) => "pixels_unchanged_360x280".into(),
            Self::Pixels(PixelChange::Sparse) => "pixels_sparse_32x32_360x280".into(),
            Self::Pixels(PixelChange::Dense) => "pixels_dense_360x280".into(),
        }
    }
    fn count(self) -> usize {
        match self {
            Self::Rects(count) | Self::Text(count) => count,
            Self::Pixels(_) => 1,
        }
    }
    fn view(self, source: &Arc<[u8]>, sample: usize) -> ViewNode {
        let (width, height, items) = match self {
            Self::Pixels(change) => {
                let rgba = frame_pixels(source, change, sample);
                (
                    PIXEL_WIDTH,
                    PIXEL_HEIGHT,
                    vec![CanvasItem::Pixels {
                        bounds: CanvasRect {
                            x: 0,
                            y: 0,
                            width: PIXEL_WIDTH,
                            height: PIXEL_HEIGHT,
                            color: Color(0),
                        },
                        source_width: PIXEL_WIDTH,
                        source_height: PIXEL_HEIGHT,
                        rgba,
                    }],
                )
            }
            Self::Rects(count) | Self::Text(count) => {
                let items = (0..count)
                    .map(|index| {
                        let bounds = CanvasRect {
                            x: (index % 32) as u32 * 32,
                            y: (index / 32) as u32 * 16,
                            width: 32,
                            height: 16,
                            color: Color(0x203040ff),
                        };
                        if matches!(self, Self::Text(_)) {
                            CanvasItem::Text {
                                bounds,
                                font_size: 12,
                                bold: index % 2 == 0,
                                text: format!("Stage {index:04} {sample:05}"),
                            }
                        } else {
                            CanvasItem::Rect(bounds)
                        }
                    })
                    .collect();
                (1024, 2048, items)
            }
        };
        ViewNode::Canvas {
            id: 1,
            width,
            height,
            frames: CanvasFrames::Static,
            viewport: false,
            items,
        }
    }
}

fn frame_pixels(source: &Arc<[u8]>, change: PixelChange, sample: usize) -> Arc<[u8]> {
    if matches!(change, PixelChange::Unchanged) {
        return source.clone();
    }
    let mut bytes = source.to_vec();
    let amount = (sample as u8).wrapping_add(1);
    match change {
        PixelChange::Unchanged => unreachable!(),
        PixelChange::Sparse => {
            let left = sample * 3 % (PIXEL_WIDTH as usize - 31);
            let top = sample * 2 % (PIXEL_HEIGHT as usize - 31);
            for y in top..top + 32 {
                let start = (y * PIXEL_WIDTH as usize + left) * 4;
                for pixel in bytes[start..start + 32 * 4].as_chunks_mut::<4>().0 {
                    pixel[0] ^= amount;
                    pixel[1] ^= 0xa7;
                    pixel[2] ^= 0x5d;
                }
            }
        }
        PixelChange::Dense => {
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel[0] = pixel[0].wrapping_add(amount);
                pixel[1] = pixel[1].wrapping_add(amount.wrapping_mul(3));
                pixel[2] = pixel[2].wrapping_add(amount.wrapping_mul(7));
            }
        }
    }
    bytes.into()
}

struct Drawing {
    scene: Scene,
    samples: Rc<RefCell<Vec<u64>>>,
    cohort: usize,
    cohorts: Vec<Cohort>,
    pixels: Arc<[u8]>,
}
impl Render for Drawing {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cohort = self.cohorts[self.cohort];
        let sample = self.samples.borrow().len();
        // Fixture construction is outside the measured renderer boundary.
        let view = cohort.view(&self.pixels, sample);
        let start = Instant::now();
        let rendered = self.scene.render(Some(&view), window, cx);
        let samples = self.samples.clone();
        let entity = cx.entity().downgrade();
        div().relative().bg(gpui::white()).child(rendered).child(gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                samples.borrow_mut().push(start.elapsed().as_nanos() as u64);
                let finished = samples.borrow().len() == WARMUP + SAMPLES;
                window.on_next_frame(move |_, cx| {
                    entity.update(cx, |drawing, cx| {
                        if finished {
                            let values = drawing.samples.borrow();
                            println!("{}", serde_json::json!({
                                "workload": cohort.name(),
                                "count": cohort.count(), "text": matches!(cohort, Cohort::Text(_)),
                                "warmup": WARMUP, "sample_count": SAMPLES,
                                "boundary": "Scene::render through final GPUI paint callback; includes pixel conversion and atlas upload; excludes source generation, GPU completion and physical presentation",
                                "samples_ns": &values[WARMUP..],
                            }));
                            drop(values);
                            drawing.samples.borrow_mut().clear();
                            drawing.cohort += 1;
                            if drawing.cohort == drawing.cohorts.len() { cx.quit(); return; }
                        }
                        cx.notify();
                    }).unwrap();
                });
            },
        ).absolute().top_0().left_0().w(px(1.)).h(px(1.)))
    }
}
fn main() {
    let mut arguments = std::env::args().skip(1);
    let pixels_only = match (arguments.next().as_deref(), arguments.next()) {
        (None, None) => false,
        (Some("--pixels"), None) => true,
        _ => {
            eprintln!("usage: benchmark-canvas [--pixels]");
            std::process::exit(2);
        }
    };
    let mut cohorts: Vec<_> = if pixels_only {
        vec![]
    } else {
        COUNTS.into_iter().flat_map(|count| [Cohort::Rects(count), Cohort::Text(count)]).collect()
    };
    cohorts.extend([
        Cohort::Pixels(PixelChange::Unchanged),
        Cohort::Pixels(PixelChange::Sparse),
        Cohort::Pixels(PixelChange::Dense),
    ]);
    let mut pixels = vec![255; (PIXEL_WIDTH * PIXEL_HEIGHT * 4) as usize];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = index as u32 % PIXEL_WIDTH;
        let y = index as u32 / PIXEL_WIDTH;
        pixel[..3].copy_from_slice(&[(x * 5) as u8, (y * 7) as u8, (x + y * 3) as u8]);
    }
    let pixels: Arc<[u8]> = pixels.into();
    playbit::trace::initialize(4, &[]).unwrap();
    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../assets/InterVariable.ttf"))])
            .unwrap();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(800.)),
                    cx,
                ))),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Drawing {
                    scene: Scene::new(Arc::new(|_| true), Rc::new(|_, _| {})),
                    samples: Rc::new(RefCell::new(Vec::with_capacity(WARMUP + SAMPLES))),
                    cohort: 0,
                    cohorts,
                    pixels,
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
