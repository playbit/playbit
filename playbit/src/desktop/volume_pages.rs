//! Page ordinary volumes and invitations together without letting either group hide the other.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use std::ops::Range;

const ROWS: usize = 256;
pub(super) struct Page {
    pub(super) ordinary: Range<usize>,
    pub(super) invited: Range<usize>,
    pub(super) pages: usize,
    pub(super) index: usize,
    ordinary_size: usize,
}

impl Shell {
    pub(super) fn visible_volume_page(&self) -> Page {
        let ordinary = self.volumes.len();
        let invited = self.invited_volumes.len();
        // Reserve half a page per group when both are large. All spare rows
        // are available to the other group when one group is small or empty.
        let ordinary_size = ordinary.min(ROWS - invited.min(ROWS / 2));
        let invited_size = ROWS - ordinary_size;
        let pages = ordinary
            .div_ceil(ordinary_size.max(1))
            .max(invited.div_ceil(invited_size.max(1)))
            .max(1);
        let index = self.volume_page.min(pages - 1);
        let range = |size: usize, count| (index * size).min(count)..((index + 1) * size).min(count);
        Page {
            ordinary: range(ordinary_size, ordinary),
            invited: range(invited_size, invited),
            pages,
            index,
            ordinary_size,
        }
    }

    pub(super) fn insert_ordinary_volume(&mut self, volume: crate::store::Volume) -> usize {
        let index = self
            .volumes
            .partition_point(|row| super::sidebar_view::compare_volumes(row, &volume).is_lt());
        self.volumes.insert(index, volume);
        index
    }

    pub(super) fn reveal_ordinary_volume_page(&mut self, index: usize) {
        assert!(index < self.volumes.len());
        self.volume_page = index / self.visible_volume_page().ordinary_size;
    }

    pub(super) fn volume_page_controls(
        &mut self,
        page: &Page,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut bar =
            div().h(ui(32.)).flex_none().flex().items_center().justify_between().px(ui(6.));
        for (key, label, target) in [
            ("previous-volume-page", "Previous page", page.index.checked_sub(1)),
            (
                "next-volume-page",
                "Next page",
                (page.index + 1 < page.pages).then_some(page.index + 1),
            ),
        ] {
            if key == "next-volume-page" {
                bar = bar.child(
                    self.host_text(
                        HostText::VolumePage,
                        format!("{} / {}", page.index + 1, page.pages),
                        cx,
                    )
                    .text_size(ui(11.)),
                );
            }
            let host = cx.weak_entity();
            let button = self.host_button(
                key,
                label,
                target.is_none(),
                Rc::new(move |_, cx| {
                    if let Some(target) = target {
                        let _ = host.update(cx, |host, cx| {
                            host.volume_page = target;
                            host.volume_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                            cx.notify();
                        });
                    }
                }),
                cx,
            );
            bar = bar.child(button);
        }
        bar
    }
}
