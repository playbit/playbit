//! Native columns show the volume's entries, selected content and permissions.
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Browser {
    pub(super) fn directory_columns(
        &mut self,
        window: &mut Window,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::Div> {
        let parents: Vec<_> = self.catalog.columns.iter().map(|column| column.parent).collect();
        self.scrolling.directories.retain(|parent, _| parents.contains(parent));
        let outer = self.scrolling.columns.read(cx).handle();
        let outer_offset = outer.offset().x;
        let viewport = outer.bounds();
        let clip_left = viewport.left();
        let row_height = crate::ui::desktop::zoom::pixels(window, 32.);
        let clip_right = if viewport.size.width > px(0.) {
            viewport.right()
        } else {
            window.viewport_size().width
        };
        let mandatory = self.catalog.columns.iter().position(|column| {
            column.page.iter().flat_map(|page| &page.entries).any(|entry| {
                self.renaming.as_ref().is_some_and(|rename| rename.entry == entry.id)
                    || self
                        .controls
                        .get(&Self::entry_control_key(entry))
                        .is_some_and(|control| control.read(cx).focused(window))
            })
        });
        let mut budgets = vec![0; parents.len()];
        let content_rows = if let Some(details) = &self.details {
            match &details.entry.info.kind {
                EntryKind::ObjectDb { schema } => {
                    schema.types.len().min(32)
                        + self.records.page.as_ref().map_or(0, |page| page.objects.len().min(64))
                }
                EntryKind::KvDb {} if !self.overview => {
                    self.kv_navigation_keys().len() + self.kv_creating_drafts(cx).len()
                }
                _ => 0,
            }
        } else {
            0
        };
        let draft_controls = self.local_draft_summaries(cx).len();
        let mut budget = 256usize.saturating_sub(content_rows + draft_controls);
        // Only intersecting columns create row controls. New columns receive priority until
        // their actual bounds arrive from prepaint, preserving focused inline creation.
        let order = mandatory
            .into_iter()
            .chain((0..parents.len()).rev().filter(|index| Some(*index) != mandatory));
        for index in order {
            let viewport = self.scrolling.directory(parents[index], cx);
            let visible = viewport.bounds.get().is_none_or(|(mut bounds, previous)| {
                bounds.origin.x += outer_offset - previous;
                bounds.right() > clip_left && bounds.left() < clip_right
            });
            if visible || mandatory == Some(index) {
                let count =
                    self.catalog.columns[index].page.as_ref().map_or(0, |page| page.entries.len());
                budgets[index] = count.min(budget);
                budget -= budgets[index];
            }
        }
        let hidden = namespace::hidden(&self.catalog_summaries(), &self.catalog_exclusions());
        let mut columns = Vec::with_capacity(parents.len());
        for (index, parent) in parents.into_iter().enumerate() {
            let viewport = self.scrolling.directory(parent, cx);
            let name = self.catalog.columns[index].name.clone();
            let entries = self.catalog.columns[index]
                .page
                .as_ref()
                .map(|page| if budgets[index] == 0 { Vec::new() } else { page.entries.clone() });
            let omitted = budgets[index] == 0
                && self.catalog.columns[index]
                    .page
                    .as_ref()
                    .is_some_and(|page| !page.entries.is_empty());
            let mut list = div()
                .id(SharedString::from(format!("directory-list-{parent:?}")))
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&viewport.bars.read(cx).handle());
            if let Some(entries) = entries {
                let shown: Vec<_> =
                    entries.into_iter().filter(|entry| !hidden.contains(&entry.id)).collect();
                if omitted {
                    let count = self.catalog.columns[index]
                        .page
                        .as_ref()
                        .map_or(0, |page| page.entries.len());
                    list = list.child(div().h(ui(count as f32 * 32.)).flex_none());
                } else if shown.is_empty() {
                    list = list.child(
                        self.text(&format!("directory-empty-{parent:?}"), "Empty", cx)
                            .pl(ui(12.))
                            .pt(ui(4.))
                            .text_color(gpui::black().alpha(0.2)),
                    );
                } else {
                    let bars = viewport.bars.read(cx).handle();
                    let visible_height = bars.bounds().size.height.max(row_height);
                    let mut start = ((f32::from(-bars.offset().y) / f32::from(row_height))
                        .floor()
                        .max(0.) as usize)
                        .min(shown.len());
                    if let Some(target) = shown.iter().position(|entry| {
                        self.renaming.as_ref().is_some_and(|rename| rename.entry == entry.id)
                    }) {
                        let visible_rows =
                            (f32::from(visible_height) / f32::from(row_height)).floor() as usize;
                        if target < start || target >= start + visible_rows {
                            start = target;
                            bars.set_offset(gpui::point(px(0.), -(row_height * target as f32)));
                        }
                    }
                    let count =
                        ((f32::from(visible_height) / f32::from(row_height)).ceil() as usize + 1)
                            .min(budgets[index]);
                    let end = (start + count).min(shown.len());
                    let mut rows = div()
                        .flex()
                        .flex_col()
                        .px(ui(4.))
                        .child(div().h(ui(start as f32 * 32.)).flex_none());
                    for entry in shown.iter().take(end).skip(start) {
                        rows = rows.child(self.directory_entry_row(entry, cx));
                    }
                    rows = rows.child(div().h(ui((shown.len() - end) as f32 * 32.)).flex_none());
                    list = list.child(rows);
                }
            } else {
                list = list.child(
                    self.text(
                        &format!("directory-loading-{parent:?}"),
                        if self.permission_refresh.catalog {
                            "Refreshing permissions…"
                        } else if self.catalog.pending.is_some() {
                            "Loading entries…"
                        } else {
                            "Entries unavailable"
                        },
                        cx,
                    )
                    .pl(ui(12.))
                    .pt(ui(4.))
                    .text_color(gpui::black().alpha(0.2)),
                );
            }
            let bounds = viewport.bounds.clone();
            let weak = cx.weak_entity();
            let measure = gpui::canvas(
                move |bounds_now, window, cx| {
                    let state = (bounds_now, outer_offset);
                    if bounds.get() != Some(state) {
                        bounds.set(Some(state));
                        let weak = weak.clone();
                        window.defer(cx, move |_, cx| {
                            let _ = weak.update(cx, |_, cx| cx.notify());
                        });
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full();
            let mut column = div()
                .relative()
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .bg(rgba(theme().color(Role::WindowBackground).0))
                .border_r_1()
                .border_color(super::super::theme::column_divider_color())
                .horizontal_focus_reveal_scope();
            column = if let Some(parent) = parent {
                column.flex_1().min_w(ui(240.)).child(self.folder_header(
                    parent,
                    &name,
                    window_active,
                    cx,
                ))
            } else {
                column.w(ui(240.)).child(self.volume_header(window_active, cx))
            };
            column = column.child(scrolling::viewport(list, &viewport.bars)).child(measure);
            let page = &self.catalog.columns[index];
            if page.after.is_some() || page.page.as_ref().is_some_and(|page| page.next.is_some()) {
                let pending = self.catalog.pending.is_some();
                let first = self.button(
                    &format!("directory-first-{parent:?}"),
                    "First page",
                    pending || page.after.is_none(),
                    move |this, cx| this.page_directory(parent, true, cx),
                    cx,
                );
                let next_disabled = pending
                    || self.catalog.columns[index]
                        .page
                        .as_ref()
                        .is_none_or(|page| page.next.is_none());
                let next = self.button(
                    &format!("directory-next-{parent:?}"),
                    "Next page",
                    next_disabled,
                    move |this, cx| this.page_directory(parent, false, cx),
                    cx,
                );
                column = column
                    .child(div().flex().flex_none().gap(ui(6.)).p(ui(8.)).child(first).child(next));
            }
            columns.push(column);
        }
        columns
    }

    pub(super) fn directory_entry_row(
        &mut self,
        entry: &crate::store::volume_index::EntrySummary,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let id = entry.id;
        let glyph = entry_glyph(entry.kind, entry.protocol);
        if let Some(editor) = self
            .renaming
            .as_ref()
            .filter(|rename| rename.entry == id)
            .map(|rename| rename.editor.clone())
        {
            return div().h(ui(32.)).child(self.rename_row(glyph, editor, cx));
        }
        div().h(ui(32.)).child(self.entry_row(entry, cx))
    }

    pub(super) fn entry_row(
        &mut self,
        entry: &crate::store::volume_index::EntrySummary,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let label = if entry.name_conflict {
            format!("{} · Name conflict", entry.name)
        } else {
            entry.name.clone()
        };
        let id = entry.id;
        let glyph = entry_glyph(entry.kind, entry.protocol);
        let key = Self::entry_control_key(entry);
        let control = self.button(
            &key,
            &label,
            self.entry_deletion_pending(),
            move |this, cx| this.select(id, cx),
            cx,
        );
        let rename = self.entry_rename_callback(id, cx);
        self.bind_entry_menu(id, &control, cx);
        let selected = self.catalog.columns.iter().any(|column| column.selected == Some(id));
        let secondary = selected
            && (self.selected != Some(id)
                || (!self.overview
                    && ((entry.kind == crate::store::EntryType::KvDb && self.kv_editor_open())
                        || (entry.kind == crate::store::EntryType::ObjectDb
                            && self.records.type_id.is_some()))));
        let accessory = self
            .draft_for_entry(id, cx)
            .map(|draft| self.draft_button(draft, &format!("entry-{id}"), cx));
        control.update(cx, |control, cx| {
            control.set_navigation_accessory(accessory, cx);
            control.set_delayed_rename(rename);
            control.set_navigation_selection_active(!secondary, cx);
            control.select_navigation(selected, cx);
            control.set_navigation_glyph(glyph, cx);
            control.set_navigation_child(entry.kind == crate::store::EntryType::Directory, cx);
        });
        control
    }

    pub(super) fn entry_control_key(entry: &crate::store::volume_index::EntrySummary) -> String {
        format!(
            "{}-{}",
            if entry.kind == crate::store::EntryType::Directory { "directory" } else { "entry" },
            entry.id
        )
    }

    fn entry_rename_callback(&self, id: EntryId, cx: &Context<Self>) -> Option<controls::Activate> {
        let eligible = move |this: &Self| {
            this.selected == Some(id)
                && this.details.as_ref().is_some_and(|details| {
                    details.entry.id == id && details.writable && !details.entry.deleted
                })
                && this.creating_entry.is_none()
                && this.renaming.is_none()
        };
        if !eligible(self) {
            return None;
        }
        let weak = cx.weak_entity();
        Some(Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                if !eligible(this) || !this.prepare_navigation(cx) {
                    return;
                }
                if let Some(name) =
                    this.details.as_ref().map(|details| details.entry.info.name.clone())
                {
                    this.start_rename(id, &name, cx);
                }
            });
        }))
    }

    pub(super) fn detail_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if self.selected.is_some() && self.details.is_none() {
            return self.loading_panel(cx);
        }
        if !self.show_entry_details
            && self
                .details
                .as_ref()
                .is_some_and(|details| matches!(details.entry.info.kind, EntryKind::Blob { .. }))
        {
            return self.blob_preview(window, cx);
        }
        if !self.overview
            && (self.database_address().is_some()
                || (self.details.as_ref().is_some_and(|details| details.entry.deleted)
                    && self.has_record_draft()))
            && self.records.type_id.is_some()
        {
            return div().flex_1().min_w_0().min_h_0().child(self.records_panel(cx));
        }
        let detail = match self.details.clone() {
            Some(details) => self.entry_detail(&details, cx),
            None => self.volume_content(cx),
        };
        let content = div()
            .id("entry-details")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scrolling.details.read(cx).handle())
            .px(ui(12.))
            .pt(ui(4.))
            .pb(ui(12.))
            .child(detail);
        scrolling::viewport(content, &self.scrolling.details)
    }

    fn entry_detail(&mut self, details: &EntryDetails, cx: &mut Context<Self>) -> gpui::Div {
        let mut panel = div().flex().flex_col().gap(ui(12.));
        if !self.show_entry_details {
            panel = match &details.entry.info.kind {
                EntryKind::ObjectDb { .. } => panel.child(self.schema_navigation(cx)),
                EntryKind::Blob { .. } => panel,
                EntryKind::Directory {} => {
                    panel.child(self.text("entry-directory", "Directory", cx))
                }
                EntryKind::KvDb {} => panel.child(self.text(
                    "entry-database",
                    if details.entry.info.protocol == Some(DatabaseProtocol::BlobLog) {
                        "Append log"
                    } else {
                        "Key-value store"
                    },
                    cx,
                )),
            };
        } else {
            panel = panel
                .child(
                    self.text("entry-id-label", "ID", cx).font_weight(gpui::FontWeight::SEMIBOLD),
                )
                .child(
                    self.identifier("entry-id", details.entry.id.to_string(), cx)
                        .text_size(ui(11.)),
                )
                .child(self.name.clone())
                .child(self.entry_actions(details, cx));
        }
        panel.child(self.entry_status(details, cx))
    }

    fn entry_actions(&mut self, details: &EntryDetails, cx: &mut Context<Self>) -> gpui::Div {
        let id = details.entry.id;
        let valid = crate::store::volume_index::validate_name(self.name.read(cx).text()).is_ok();
        let rename = self.button(
            "rename-entry",
            "Rename",
            !details.writable
                || !valid
                || self.files.busy()
                || self.creating_entry.is_some()
                || self.renaming.as_ref().is_some_and(|rename| rename.submitted.is_some()),
            move |this, cx| this.rename_from_field(id, cx),
            cx,
        );
        let delete = self.button(
            "delete-entry",
            "Delete",
            !details.writable,
            move |this, cx| this.entry_action(id, "", 4, cx),
            cx,
        );
        div().flex().gap(ui(8.)).child(rename).child(delete)
    }

    fn entry_status(&mut self, details: &EntryDetails, cx: &mut Context<Self>) -> gpui::Div {
        let mut status = div().flex().flex_col().gap(ui(8.));
        for (condition, key, message) in [
            (
                !details.entry.deleted && !details.backing_ready,
                "unavailable",
                "Content has not arrived on this device yet.",
            ),
            (
                !details.entry.deleted && !details.readable,
                "read-denied",
                "You do not have permission to read this entry.",
            ),
            (
                details.entry.name_conflict,
                "conflict",
                "Another entry uses this name. Rename this entry to resolve the conflict.",
            ),
            (
                details.schema_pending,
                "schema-pending",
                "Some schema declarations have not arrived yet.",
            ),
        ] {
            if condition {
                status = status.child(self.text(key, message, cx));
            }
        }
        status
    }

    fn volume_content(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        self.text(
            "volume-empty",
            if self
                .catalog
                .columns
                .first()
                .and_then(|column| column.page.as_ref().map(|page| &page.entries))
                .is_some_and(|entries| !entries.is_empty())
            {
                "Select an entry"
            } else {
                "Empty"
            },
            cx,
        )
        .pl(ui(24.))
        .text_color(gpui::Hsla::from(rgba(theme().color(Role::Text).0)).alpha(0.2))
    }

    pub(super) fn loading_label(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        if self.detail_loading.visible && self.detail_is_loading() {
            self.text("detail-loading", "Loading...", cx).text_color(gpui::black().alpha(0.4))
        } else {
            div()
        }
    }

    pub(super) fn loading_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        div().size_full().pl(ui(36.)).pt(ui(4.)).child(self.loading_label(cx))
    }

    pub(super) fn retire_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable;
        let retiring_control_focus = self.controls.iter().find_map(|(key, control)| {
            (!self.rendered_controls.contains(key) && control.read(cx).focused(window)).then(|| {
                (key.starts_with("schema-type-"), control.read(cx).keyboard_focus_visible(window))
            })
        });
        let retiring_schema_focus =
            retiring_control_focus.filter(|(schema, _)| *schema).map(|(_, visible)| visible);
        let retired_focused = retiring_control_focus.is_some()
            || self.identifier_fields.iter().any(|(key, editor)| {
                !self.rendered_text.contains(*key)
                    && editor.read(cx).focus_handle(cx).is_focused(window)
            });
        let schema_target = retiring_schema_focus.and_then(|_| {
            self.records
                .page
                .iter()
                .flat_map(|page| &page.objects)
                .filter_map(|object| {
                    let key = format!("record-{}", object.object_id);
                    self.rendered_controls.contains(&key).then_some(key)
                })
                .next()
                .or_else(|| {
                    let entry = self.catalog_entry(self.selected?)?;
                    let key = Self::entry_control_key(entry);
                    self.rendered_controls.contains(&key).then_some(key)
                })
                .and_then(|key| self.controls.get(&key).cloned())
        });
        if let Some(control) = schema_target {
            control.update(cx, |control, cx| {
                if retiring_schema_focus == Some(true) {
                    control.focus(window);
                    control.show_keyboard_focus_if_focused(window, cx);
                } else {
                    control.focus_from_arrow(window, cx);
                }
            });
        } else if retired_focused {
            let control = if self.can_create_entry_in(None)
                && self.creating_entry.is_none()
                && !self.files.busy()
            {
                self.controls.get("create-entry-menu")
            } else if !self.entry_deletion_pending() {
                self.catalog
                    .columns
                    .iter()
                    .flat_map(|column| column.page.iter().flat_map(|page| &page.entries))
                    .find_map(|entry| {
                        let key = Self::entry_control_key(entry);
                        if self.rendered_controls.contains(&key) {
                            self.controls.get(&key)
                        } else {
                            None
                        }
                    })
            } else {
                None
            };
            if let Some(control) = control {
                control.update(cx, |control, cx| {
                    if retiring_control_focus.is_some_and(|(_, visible)| !visible) {
                        control.focus_without_ring(window, cx);
                    } else {
                        control.focus(window);
                    }
                });
            } else if let Some(identifier) = self
                .identifier_fields
                .iter()
                .find_map(|(key, editor)| self.rendered_text.contains(*key).then_some(editor))
            {
                identifier.update(cx, |editor, cx| {
                    if retiring_control_focus.is_none_or(|(_, visible)| visible) {
                        editor.focus_from_tab(window, cx);
                    } else {
                        editor.focus_handle(cx).focus(window);
                    }
                });
            }
        }
        self.controls.retain(|key, control| {
            let keep = self.rendered_controls.contains(key);
            if !keep {
                control.update(cx, |control, _| control.retire());
            }
            keep
        });
        self.text_ids.retain(|key, _| self.rendered_text.contains(key));
        self.identifier_fields.retain(|key, editor| {
            let keep = self.rendered_text.contains(*key);
            if !keep {
                editor.update(cx, |editor, _| editor.set_enabled(false));
            }
            keep
        });
    }
}

fn entry_glyph(kind: crate::store::EntryType, protocol: Option<DatabaseProtocol>) -> &'static str {
    match kind {
        crate::store::EntryType::Directory => "entry-directory",
        crate::store::EntryType::KvDb if protocol == Some(DatabaseProtocol::BlobLog) => {
            "entry-append-log"
        }
        crate::store::EntryType::KvDb => "entry-kv-db",
        crate::store::EntryType::ObjectDb => "entry-object-db",
        crate::store::EntryType::Blob => "entry-file",
    }
}
