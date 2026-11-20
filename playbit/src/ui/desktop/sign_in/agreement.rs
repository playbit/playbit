//! The agreement uses one bounded bundled document and the shared native controls.
//! Scrolling only changes presentation; the engine durably owns acceptance.
use super::*;
use crate::ui::desktop::scrollbar::Scrollbars;
use gpui::{FocusHandle, ScrollHandle, point};
use std::cell::Cell;

pub(super) use crate::network::ap::registration::TERMS_DOCUMENT as DOCUMENT;
pub(super) const PRIVACY_URL: &str = "https://playbit.app/help/privacy";
const PARAGRAPHS_MAX: usize = 24;
const _: () = assert!(DOCUMENT.len() <= 8 * 1024);

pub(super) struct Agreement {
    scrollbars: Entity<Scrollbars>,
    focus: FocusHandle,
    saving: bool,
    read_pending: Rc<Cell<bool>>,
    text_ids: [Entity<()>; PARAGRAPHS_MAX],
}
impl Agreement {
    pub(super) fn new(cx: &mut Context<SignIn>) -> Self {
        Self {
            scrollbars: cx.new(|_| Scrollbars::new(false)),
            focus: cx.focus_handle(),
            saving: false,
            read_pending: Rc::new(Cell::new(false)),
            text_ids: std::array::from_fn(|_| cx.new(|_| ())),
        }
    }
}
impl SignIn {
    pub(super) fn save_agreement(&mut self, cx: &mut Context<Self>) {
        if self.agreement.saving {
            return;
        }
        self.agreement.saving = true;
        let directory = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
        let path = cx.prompt_for_new_path(&directory, Some("Playbit User Agreement.md"));
        cx.spawn(async move |this, cx| {
            let result = match path.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::fs::write(path, DOCUMENT).map_err(|error| error.to_string())
                        })
                        .await
                }
                Ok(Ok(None)) => Ok(()),
                _ => Err("Could not select a location for the user agreement.".into()),
            };
            let _ = this.update(cx, |this, cx| {
                this.agreement.saving = false;
                if let Err(error) = result {
                    this.flow.error = error;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_agreement(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let busy = self.flow.busy();
        if self.focus_step != Some(Step::Agreement) {
            self.focus_step = Some(Step::Agreement);
            window.focus(&self.agreement.focus);
        }
        self.agreement_buttons(busy, cx);
        let sp = |value| zoom::pixels(window, value);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .rounded(sp(16.))
            .overflow_hidden()
            .font_family(super::super::UI_FONT_FAMILY)
            .font_weight(gpui::FontWeight::NORMAL)
            .text_size(sp(11.))
            .line_height(sp(16.))
            .text_color(gpui::rgba(0x000000b3))
            .child(
                gpui::img(artwork()[0].clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(512. * zoom::factor(cx).max(1.)))
                    .h(px(512. * zoom::factor(cx).max(1.))),
            )
            .child(self.agreement_header(window))
            .child(self.agreement_viewport(window, cx))
            .when(!self.flow.error.is_empty(), |view| {
                view.child(
                    div()
                        .relative()
                        .bg(gpui::rgb(0xffffff))
                        .px(sp(16.))
                        .text_color(gpui::rgb(0xb3261e))
                        .child(self.text(1, self.flow.error.clone())),
                )
            })
            .child(self.agreement_footer(window))
            .on_key_down(cx.listener(Self::agreement_tab))
    }

    fn agreement_tab(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key != "tab"
            || modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.function
        {
            return;
        }
        if modifiers.shift {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        for control in [&self.privacy, &self.agreement_save, &self.agreement_decline, &self.primary]
        {
            control.update(cx, |control, cx| control.show_keyboard_focus_if_focused(window, cx));
        }
        cx.stop_propagation();
    }

    fn agreement_buttons(&self, busy: bool, cx: &mut Context<Self>) {
        self.agreement_button(
            &self.primary,
            "Accept",
            62.,
            ButtonStyle::Primary,
            busy || !self.flow.agreement_read,
            cx,
        );
        self.agreement_button(
            &self.agreement_save,
            "Save as…",
            76.,
            ButtonStyle::Default,
            self.agreement.saving,
            cx,
        );
        self.agreement_button(
            &self.agreement_decline,
            "Decline & Quit",
            100.,
            ButtonStyle::Default,
            busy,
            cx,
        );
    }
    fn agreement_header(&self, window: &Window) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        div()
            .relative()
            .h(sp(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(sp(14.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .window_control_area(gpui::WindowControlArea::Drag)
            .child(self.text(0, "Playbit User Agreement"))
    }
    fn agreement_footer(&self, window: &Window) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        div()
            .relative()
            .h(sp(56.))
            .flex_shrink_0()
            .p(sp(16.))
            .flex()
            .items_center()
            .gap(sp(8.))
            .child(self.agreement_save.clone())
            .child(div().flex_1())
            .child(self.agreement_decline.clone())
            .child(self.primary.clone())
    }
    fn agreement_document(&self, window: &Window) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        let mut document = div().flex().flex_col().gap(sp(16.)).p(sp(16.)).pr(sp(20.));
        for (index, paragraph) in DOCUMENT.split("\n\n").skip(1).enumerate() {
            assert!(index < PARAGRAPHS_MAX, "bundled agreement fits the text-node budget");
            if paragraph.trim().strip_prefix("[Privacy policy](").and_then(|s| s.strip_suffix(')'))
                == Some(PRIVACY_URL)
            {
                document = document.child(self.privacy.clone());
                continue;
            }
            let heading = paragraph.starts_with("## ");
            let text: gpui::SharedString =
                paragraph.trim().trim_start_matches("## ").to_owned().into();
            let identity = super::super::text_editor::accessibility_text_ids(
                self.agreement.text_ids[index].entity_id(),
            )
            .0;
            let accessible = text.clone();
            document = document.child(
                div()
                    .relative()
                    .when(heading, |p| p.font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(text)
                    .child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                window.insert_accessibility_node(
                                    identity,
                                    super::super::text_accessibility_node(
                                        &accessible,
                                        heading,
                                        bounds,
                                    ),
                                    false,
                                    None,
                                );
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            );
        }
        document
    }
    fn agreement_read_marker(&self, handle: ScrollHandle, cx: &Context<Self>) -> gpui::AnyElement {
        if self.flow.agreement_read {
            return div().into_any_element();
        }
        let read = handle;
        let view = cx.weak_entity();
        let pending = self.agreement.read_pending.clone();
        gpui::canvas(
            move |_, _, cx| {
                if read.bounds().size.height > px(0.)
                    && -read.offset().y >= read.max_offset().height - px(0.5)
                    && !pending.replace(true)
                {
                    let view = view.clone();
                    let pending = pending.clone();
                    cx.defer(move |cx| {
                        pending.set(false);
                        let _ = view.update(cx, |this, cx| {
                            if this.flow.step == Step::Agreement && !this.flow.agreement_read {
                                this.flow.agreement_read = true;
                                cx.notify();
                            }
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
    fn agreement_viewport(&self, window: &Window, cx: &Context<Self>) -> gpui::Div {
        let handle = self.agreement.scrollbars.read(cx).handle();
        let document = self.agreement_document(window);
        let reached_end = self.agreement_read_marker(handle.clone(), cx);
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .bg(gpui::rgb(0xffffff))
            .child(
                div()
                    .id("user-agreement")
                    .size_full()
                    .overflow_y_scroll()
                    .track_focus(&self.agreement.focus)
                    .track_scroll(&handle)
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            this.agreement_key(event, &handle, window, cx);
                        },
                    ))
                    .child(document),
            )
            .child(self.agreement_edges(window, cx))
            .child(Scrollbars::overlay(&self.agreement.scrollbars))
            .child(reached_end)
    }
    fn agreement_edges(&self, window: &Window, cx: &Context<Self>) -> gpui::AnyElement {
        let handle = self.agreement.scrollbars.read(cx).handle();
        let height = zoom::pixels(window, 16.);
        gpui::canvas(
            move |_, _, _| (handle.offset().y, handle.max_offset().height),
            move |bounds, (offset, maximum), window, _| {
                let solid = gpui::linear_color_stop(gpui::rgb(0xffffff), 0.);
                let clear = gpui::linear_color_stop(gpui::rgba(0xffffff00), 1.);
                let mut edge = gpui::Bounds {
                    origin: bounds.origin,
                    size: gpui::size(bounds.size.width, height),
                };
                if offset < px(0.) {
                    window.paint_quad(gpui::fill(edge, gpui::linear_gradient(180., solid, clear)));
                }
                if -offset < maximum {
                    edge.origin.y = bounds.bottom() - height;
                    window.paint_quad(gpui::fill(edge, gpui::linear_gradient(0., solid, clear)));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    fn agreement_button(
        &self,
        button: &Entity<controls::Control>,
        label: &str,
        width: f32,
        style: ButtonStyle,
        disabled: bool,
        cx: &mut Context<Self>,
    ) {
        button.update(cx, |button, cx| {
            button.set_button_size(width, 24., cx);
            button.set(
                controls::Properties {
                    kind: controls::Kind::Button(style),
                    label,
                    disabled,
                    theme: &theme(),
                },
                cx,
            );
        });
    }

    fn agreement_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        scroll: &ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut offset = scroll.offset();
        let maximum = scroll.max_offset().height;
        let page = scroll.bounds().size.height * 0.9;
        match event.keystroke.key.as_str() {
            "down" => offset.y -= zoom::pixels(window, 32.),
            "up" => offset.y += zoom::pixels(window, 32.),
            "pagedown" | "space" => offset.y -= page,
            "pageup" => offset.y += page,
            "end" => offset.y = -maximum,
            "home" => offset.y = px(0.),
            _ => return,
        }
        scroll.set_offset(point(offset.x, offset.y.clamp(-maximum, px(0.))));
        window.refresh();
        cx.stop_propagation();
    }
}
