//! Independent browser viewports share the desktop scrollbar implementation.
use super::*;
use crate::ui::desktop::scrollbar::Scrollbars;

pub(super) struct Scrolling {
    pub columns: Entity<Scrollbars>,
    pub content_columns: Entity<Scrollbars>,
    // Preserve the selection-time position while its catalog path is still loading.
    pub reveal_entry: Option<(EntryId, gpui::Pixels)>,
    pub entries: Entity<Scrollbars>,
    pub directories: BTreeMap<Option<EntryId>, DirectoryViewport>,
    pub details: Entity<Scrollbars>,
    pub permissions: Entity<Scrollbars>,
    pub records: Entity<Scrollbars>,
    pub fields: Entity<Scrollbars>,
    pub list: gpui::UniformListScrollHandle,
}

impl Scrolling {
    pub fn new(cx: &mut Context<Browser>) -> Self {
        let entries = cx.new(|_| Scrollbars::new(false));
        let details = cx.new(|_| Scrollbars::new(false));
        let permissions = cx.new(|_| Scrollbars::new(false));
        let records = cx.new(|_| Scrollbars::new(false));
        let fields = cx.new(|_| Scrollbars::new(false));
        let list = gpui::UniformListScrollHandle::new();
        list.0.borrow_mut().base_handle = records.read(cx).handle();
        let columns = cx.new(|_| Scrollbars::new(false));
        Self {
            content_columns: cx.new(|_| Scrollbars::new(false)),
            columns,
            reveal_entry: None,
            entries,
            directories: BTreeMap::new(),
            details,
            permissions,
            records,
            fields,
            list,
        }
    }

    pub fn directory(
        &mut self,
        parent: Option<EntryId>,
        cx: &mut Context<Browser>,
    ) -> DirectoryViewport {
        self.directories
            .entry(parent)
            .or_insert_with(|| DirectoryViewport {
                bars: if parent.is_none() {
                    self.entries.clone()
                } else {
                    cx.new(|_| Scrollbars::new(false))
                },
                bounds: Default::default(),
            })
            .clone()
    }

    pub fn inspector_clearance(&self, cx: &App) -> f32 {
        if self.permissions.read(cx).handle().max_offset().height > px(0.) { 16. } else { 0. }
    }
}

impl Browser {
    /// Reveal a new folder column, keeping its containing list visible first.
    pub(super) fn entry_reveal(&self, cx: &Context<Self>) -> Option<gpui::Canvas<()>> {
        let (entry, offset) = self.scrolling.reveal_entry?;
        let parent = self
            .catalog
            .columns
            .iter()
            .find(|column| {
                column.page.iter().flat_map(|page| &page.entries).any(|row| row.id == entry)
            })?
            .parent;
        let selected = self.scrolling.directories.get(&parent)?.bounds.clone();
        let contents = self
            .catalog
            .columns
            .iter()
            .any(|column| column.parent == Some(entry))
            .then(|| self.scrolling.directories.get(&Some(entry)).map(|view| view.bounds.clone()))
            .flatten();
        let columns = self.scrolling.columns.read(cx).handle();
        let weak = cx.weak_entity();
        Some(
            gpui::canvas(
                move |_, window, cx| {
                    let contents =
                        contents.as_ref().and_then(|bounds| bounds.get().map(|(bounds, _)| bounds));
                    let (Some((selected, _)), Some(contents)) = (selected.get(), contents) else {
                        return;
                    };
                    let previous = columns.offset();
                    let Some(next) = entry_reveal_offset(&columns, selected, contents) else {
                        return;
                    };
                    let focus = window.focused(cx);
                    let columns = columns.clone();
                    let weak = weak.clone();
                    // Run before the viewport's ordinary focus reveal, whose offset guard
                    // then retires the smaller single-column adjustment.
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if this.scrolling.reveal_entry != Some((entry, offset))
                                || this.selected != Some(entry)
                            {
                                return;
                            }
                            this.scrolling.reveal_entry = None;
                            if columns.offset() == previous
                                && previous.x == offset.clamp(-columns.max_offset().width, px(0.))
                                && window.focused(cx) == focus
                                && next != previous
                            {
                                columns.set_offset(next);
                                cx.notify();
                            }
                        });
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }
}

fn entry_reveal_offset(
    columns: &gpui::ScrollHandle,
    selected: Bounds<gpui::Pixels>,
    contents: Bounds<gpui::Pixels>,
) -> Option<gpui::Point<gpui::Pixels>> {
    let viewport = columns.bounds();
    if viewport.size.width <= px(0.) {
        return None;
    }
    let previous = columns.offset();
    // Column bounds include elastic displacement; set_offset restores logical
    // coordinates and cancels that displacement.
    let elastic = columns.visual_offset().x - previous.x;
    let left = selected.left() - elastic;
    let right = left + (contents.right() - selected.left()).min(viewport.size.width);
    let delta = if left < viewport.left() {
        viewport.left() - left
    } else {
        (viewport.right() - right).min(px(0.))
    };
    Some(gpui::point((previous.x + delta).clamp(-columns.max_offset().width, px(0.)), previous.y))
}

/// The upper strip does not touch the window's bottom corners.
pub(super) fn viewport_overlay(bars: &Entity<Scrollbars>) -> impl IntoElement {
    Scrollbars::overlay_with_corner_radius(bars, 0.)
}

/// The lower viewport shares the native window's rounded bottom corners.
pub(super) fn columns_overlay(bars: &Entity<Scrollbars>) -> impl IntoElement {
    Scrollbars::overlay_with_corner_radius(bars, 16.)
}

/// Keep the overlay outside the content's clipping and scroll offset.
pub(super) fn viewport(content: impl IntoElement, bars: &Entity<Scrollbars>) -> gpui::Div {
    viewport_with_corner_radius(content, bars, 0.)
}

/// Reserve a trailing gutter only while the outer inspector actually overflows.
pub(super) fn inspector_viewport(
    content: impl IntoElement,
    bars: &Entity<Scrollbars>,
    cx: &mut Context<Browser>,
) -> gpui::Div {
    let handle = bars.read(cx).handle();
    let overflow = handle.max_offset().height > px(0.);
    let weak = cx.weak_entity();
    viewport_with_corner_radius(content, bars, 0.).child(
        gpui::canvas(
            move |_, window, cx| {
                if (handle.max_offset().height > px(0.)) != overflow {
                    let weak = weak.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = weak.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full(),
    )
}

/// Include a rounded parent whose clipping is outside the tracked content.
pub(super) fn viewport_with_corner_radius(
    content: impl IntoElement,
    bars: &Entity<Scrollbars>,
    corner_radius: f32,
) -> gpui::Div {
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .h_full()
        .child(content)
        .child(Scrollbars::overlay_with_corner_radius(bars, corner_radius))
}

type DirectoryBounds = Option<(Bounds<gpui::Pixels>, gpui::Pixels)>;

#[derive(Clone)]
pub(super) struct DirectoryViewport {
    pub bars: Entity<Scrollbars>,
    // Bounds and outer scroll offset from the last prepaint make offscreen columns cheap.
    pub bounds: Rc<std::cell::Cell<DirectoryBounds>>,
}
