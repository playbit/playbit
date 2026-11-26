//! Arrow navigation follows the same bounded, visible rows as the rendered lists.
use super::*;

pub(in crate::desktop) enum ListNavigation {
    Unhandled,
    Handled,
    PreviousColumn,
}

enum Target {
    Entry(EntryId),
    Type(Id),
    Key(Vec<u8>),
    LocalDraft(Id),
    Record(Id),
}

struct Row {
    target: Target,
    control: Option<Entity<controls::Control>>,
    selected: bool,
}

impl Browser {
    pub(in crate::desktop) fn focus_entry_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.focus_list(&self.entry_navigation_rows(0), window, cx)
    }

    pub(in crate::desktop) fn navigate_list(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ListNavigation {
        let Some(column) = self.focused_entry_column(window, cx) else {
            return ListNavigation::Unhandled;
        };
        let count = self.catalog.columns.len();
        let rows = if column < count {
            self.entry_navigation_rows(column)
        } else {
            self.content_navigation_rows(cx)
        };
        match key {
            "up" | "down" => {
                let Some(index) = rows.iter().position(|row| {
                    row.control.as_ref().is_some_and(|control| control.read(cx).focused(window))
                }) else {
                    return ListNavigation::Handled;
                };
                let target = if key == "down" {
                    (index + 1).min(rows.len() - 1)
                } else {
                    index.saturating_sub(1)
                };
                if (target != index || !rows[target].selected)
                    && self.select_navigation_target(&rows[target].target, cx)
                {
                    self.focus_navigation_row(
                        &rows[target],
                        if key == "down" {
                            gpui::ScrollStrategy::Bottom
                        } else {
                            gpui::ScrollStrategy::Top
                        },
                        window,
                        cx,
                    );
                }
            }
            "left" if column == 0 => return self.select_volume_for_navigation(cx),
            "left" if column < count || self.kv_address().is_some() => {
                let rows = self.entry_navigation_rows(column - 1);
                if let Some(row) = rows.iter().find(|row| row.selected).or_else(|| rows.first())
                    && self.select_navigation_target(&row.target, cx)
                {
                    self.focus_navigation_row(row, gpui::ScrollStrategy::Top, window, cx);
                }
            }
            "left" => {
                self.focus_list(&self.entry_navigation_rows(column - 1), window, cx);
            }
            "right" if column + 1 < count => {
                self.focus_list(&self.entry_navigation_rows(column + 1), window, cx);
            }
            "right" if column < count => {
                self.focus_list(&self.content_navigation_rows(cx), window, cx);
            }
            "right" => {}
            _ => return ListNavigation::Unhandled,
        }
        ListNavigation::Handled
    }

    fn select_volume_for_navigation(&mut self, cx: &mut Context<Self>) -> ListNavigation {
        if self.show_volume(cx) { ListNavigation::PreviousColumn } else { ListNavigation::Handled }
    }

    fn focus_list(&mut self, rows: &[Row], window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(row) = rows.iter().find(|row| row.selected).or_else(|| rows.first()) else {
            return false;
        };
        if !row.selected && !self.select_navigation_target(&row.target, cx) {
            return false;
        }
        let focused = self.focus_navigation_row(row, gpui::ScrollStrategy::Top, window, cx);
        if focused && row.selected {
            self.scrolling.reveal_entry = None;
        }
        focused
    }

    fn focus_navigation_row(
        &mut self,
        row: &Row,
        strategy: gpui::ScrollStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Target::Record(id) = &row.target
            && let Some(index) = self
                .records
                .page
                .as_ref()
                .and_then(|page| page.objects.iter().position(|object| object.object_id == *id))
        {
            // Virtualized rows cannot consume their own reveal request until mounted.
            self.scrolling.list.scroll_to_item(index, strategy);
            cx.notify();
        }
        let control = if let Target::Entry(id) = row.target {
            let Some(entry) = self.catalog_entry(id).cloned() else { return false };
            if let Some((parent, index)) = self.catalog.columns.iter().find_map(|column| {
                column
                    .page
                    .as_ref()?
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .map(|index| (column.parent, index))
            }) {
                let viewport = self.scrolling.directory(parent, cx);
                let handle = viewport.bars.read(cx).handle();
                let top = crate::ui::desktop::zoom::pixels(window, index as f32 * 32.);
                let height = handle.bounds().size.height;
                let current = -handle.offset().y;
                if top < current
                    || top + crate::ui::desktop::zoom::pixels(window, 32.) > current + height
                {
                    handle.set_offset(gpui::point(px(0.), -top));
                }
            }
            Some(self.entry_row(&entry, cx))
        } else {
            row.control.clone()
        };
        let focused = control.is_some_and(|control| {
            control.update(cx, |control, cx| control.focus_from_arrow(window, cx))
        });
        if focused {
            cx.notify();
        }
        focused
    }

    fn focused_entry_column(&self, window: &Window, cx: &App) -> Option<usize> {
        let (key, _) = self
            .controls
            .iter()
            .find(|(_, control)| control.read(cx).has_navigation_focus(window))?;
        for (index, column) in self.catalog.columns.iter().enumerate() {
            if column
                .page
                .iter()
                .flat_map(|page| &page.entries)
                .any(|entry| Self::entry_control_key(entry) == *key)
            {
                return Some(index);
            }
        }
        if key.starts_with("schema-type-")
            || key.starts_with("kv-key-")
            || key.starts_with("record-")
        {
            Some(self.catalog.columns.len())
        } else {
            None
        }
    }

    fn navigation_row(&self, key: &str, target: Target, cx: &App) -> Option<Row> {
        if !self.rendered_controls.contains(key) {
            return None;
        }
        let control = self.controls.get(key)?;
        let selected = control.read(cx).navigation_selection()?;
        Some(Row { target, control: Some(control.clone()), selected })
    }

    fn entry_navigation_rows(&self, column: usize) -> Vec<Row> {
        let Some(column) = self.catalog.columns.get(column) else { return Vec::new() };
        let hidden = namespace::hidden(&self.catalog_summaries(), &self.catalog_exclusions());
        let mut rows = Vec::new();
        for entry in column
            .page
            .iter()
            .flat_map(|page| &page.entries)
            .filter(|entry| !hidden.contains(&entry.id))
        {
            let id = entry.id;
            let key = Self::entry_control_key(entry);
            rows.push(Row {
                target: Target::Entry(id),
                control: self.controls.get(&key).cloned(),
                selected: column.selected == Some(id),
            });
        }
        rows
    }

    fn content_navigation_rows(&self, cx: &App) -> Vec<Row> {
        if self.overview {
            let Some(details) = &self.details else { return Vec::new() };
            let EntryKind::ObjectDb { schema } = &details.entry.info.kind else {
                return Vec::new();
            };
            return schema
                .types
                .keys()
                .skip(self.schema_offset)
                .take(32)
                .filter_map(|id| {
                    self.navigation_row(&format!("schema-type-{id}"), Target::Type(*id), cx)
                })
                .collect();
        }
        if self.kv_address().is_some() {
            return self
                .kv_navigation_keys()
                .into_iter()
                .filter_map(|(control, key)| self.navigation_row(&control, Target::Key(key), cx))
                .chain(self.kv_creating_drafts(cx).into_iter().filter_map(|draft| {
                    self.navigation_row(
                        &format!("kv-draft-{}", draft.id),
                        Target::LocalDraft(draft.id),
                        cx,
                    )
                }))
                .collect();
        }
        if self.database_address().is_none() || self.records.type_id.is_none() {
            return Vec::new();
        }
        self.records
            .page
            .iter()
            .flat_map(|page| &page.objects)
            .filter_map(|object| {
                let id = object.object_id;
                self.navigation_row(&format!("record-{id}"), Target::Record(id), cx)
            })
            .collect()
    }

    fn select_navigation_target(&mut self, target: &Target, cx: &mut Context<Self>) -> bool {
        let already_selected = match target {
            Target::Entry(id) => {
                self.selected == Some(*id)
                    && self.records.type_id.is_none()
                    && !self.kv_editor_open()
            }
            Target::Type(id) => self.records.type_id == Some(*id) && !self.overview,
            Target::Record(id) => self.records.selected == Some(*id),
            Target::Key(_) | Target::LocalDraft(_) => false,
        };
        if already_selected {
            return true;
        }
        let opened = match target {
            Target::LocalDraft(id) => self.open_local_draft(*id, cx),
            Target::Key(key) => self.navigate_kv_key(key, cx),
            _ if !self.prepare_entry_navigation(cx) => false,
            Target::Entry(id) => {
                self.select(*id, cx);
                self.selected == Some(*id)
            }
            Target::Type(id) if self.database_address().is_some() => {
                self.select_type(*id, cx);
                self.records.type_id == Some(*id)
            }
            Target::Record(id) if self.database_address().is_some() => {
                self.select_record(*id, cx);
                self.records.selected == Some(*id)
            }
            _ => false,
        };
        if opened {
            self.did_interact();
        }
        opened
    }
}

#[cfg(test)]
mod tests;
