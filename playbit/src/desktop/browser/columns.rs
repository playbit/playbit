//! Volume, content and permission headers align with the native titlebar.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use gpui::svg;

impl Browser {
    pub(super) fn column_header(
        &mut self,
        key: &str,
        title: &str,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .h(ui(44.))
            .min_h(ui(44.))
            .flex_none()
            .window_control_area(gpui::WindowControlArea::Drag)
            .flex()
            .items_center()
            .gap(ui(6.))
            .pl(ui(12.))
            .pr(ui(8.))
            .text_size(ui(14.))
            .line_height(ui(24.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(super::super::theme::section_title_color(window_active))
            .when(!title.is_empty(), |header| {
                header.child(self.text(key, title.to_owned(), cx).flex_1().min_w_0().truncate())
            })
    }

    pub(super) fn volume_header(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let name = self.volume.name.clone();
        self.directory_header(None, &name, window_active, cx)
    }

    pub(super) fn folder_header(
        &mut self,
        parent: EntryId,
        title: &str,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.directory_header(Some(parent), title, window_active, cx)
    }

    fn directory_header(
        &mut self,
        parent: Option<EntryId>,
        title: &str,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let disabled =
            !self.can_create_entry_in(parent) || self.creating_entry.is_some() || self.files.busy();
        let items = [
            (1, "Document database", "entry-object-db"),
            (0, "Key-value store", "entry-kv-db"),
            (2, "Append log", "entry-append-log"),
            (3, "Blob…", "entry-file"),
        ]
        .map(|(id, label, asset)| crate::runtime::MenuEntry::Action {
            icon: Some(crate::runtime::MenuGlyph::Asset(asset)),
            id,
            label: label.into(),
            checked: None,
            disabled,
            destructive: false,
        });
        let key = parent
            .map_or_else(|| "create-entry-menu".to_owned(), |id| format!("create-entry-menu-{id}"));
        let add = self.popup(
            &key,
            "New entry",
            disabled,
            &items,
            move |id, this, cx| match id {
                0 => this.create_entry_in(parent, EntryKind::KvDb {}, cx),
                1 => this.create_entry_in(
                    parent,
                    EntryKind::ObjectDb { schema: Schema::default() },
                    cx,
                ),
                2 => this.create_append_log_in(parent, cx),
                3 => this.import_file_in(parent, cx),
                _ => {}
            },
            cx,
        );
        add.update(cx, |control, cx| {
            control.set_sidebar_icon(controls::SidebarIcon::Glyph("add"), false, false, cx);
            control.set_popup_below();
        });
        let color = super::super::theme::section_title_color(window_active);
        let text_key =
            parent.map_or_else(|| "volume-title".to_owned(), |id| format!("directory-title-{id}"));
        self.column_header("directory-heading", "", window_active, cx)
            .child(
                svg()
                    .path(if parent.is_some() { "entry-directory" } else { "volume-cell" })
                    .size(ui(16.))
                    .flex_none()
                    .text_color(color),
            )
            .child(self.text(&text_key, title.to_owned(), cx).flex_1().min_w_0().truncate())
            .child(div().flex_none().occlude().child(add))
    }

    pub(super) fn content_header(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let heading = self.entry_title(cx);
        let Some(details) = self.details.clone() else {
            return self.column_header("content-heading", &heading, window_active, cx);
        };
        let actions = (!details.entry.deleted
            && !matches!(details.entry.info.kind, EntryKind::Blob { .. }))
        .then(|| self.entry_actions_menu(&details, cx));
        let glyph = match details.entry.info.kind {
            EntryKind::Directory {} => "entry-directory",
            EntryKind::KvDb {}
                if details.entry.info.protocol == Some(DatabaseProtocol::BlobLog) =>
            {
                "entry-append-log"
            }
            EntryKind::KvDb {} => "entry-kv-db",
            EntryKind::ObjectDb { .. } => "entry-object-db",
            EntryKind::Blob { .. } => "entry-file",
        };
        let color = super::super::theme::section_title_color(window_active);
        self.column_header("content-heading", "", window_active, cx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui(6.))
                    .flex_1()
                    .min_w_0()
                    .child(svg().path(glyph).size(ui(16.)).flex_none().text_color(color))
                    .child(self.text("entry-title", heading, cx).truncate()),
            )
            .children(actions.map(|actions| div().flex_none().occlude().child(actions)))
    }

    pub(super) fn entry_actions_menu(
        &mut self,
        details: &EntryDetails,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let id = details.entry.id;
        let title = details.entry.info.name.clone();
        let mut items =
            vec![menu_item(0, "Details", false), menu_item(2, "Rename", !details.writable)];
        items.push(menu_item(4, "Delete…", !details.writable || details.entry.deleted));
        let actions = self.popup(
            "entry-menu",
            "Entry actions",
            false,
            &items,
            move |item, this, cx| {
                this.entry_action(id, &title, item, cx);
            },
            cx,
        );
        actions.update(cx, |control, cx| {
            control.set_sidebar_icon(controls::SidebarIcon::Glyph("add"), false, false, cx);
            control.set_popup_below();
        });
        actions
    }

    pub(super) fn entry_action(
        &mut self,
        id: EntryId,
        title: &str,
        item: u32,
        cx: &mut Context<Self>,
    ) {
        if self.selected != Some(id) {
            return;
        }
        match item {
            0 if self.prepare_navigation(cx) => {
                self.overview = true;
                self.show_entry_details = true;
            }
            2 if self.prepare_navigation(cx) => {
                if self.catalog_entry(id).is_none() {
                    self.seek_entry(id);
                }
                self.start_rename(id, title, cx);
            }
            4 if self.prepare_navigation(cx) => {
                let Some(owner) = cx.active_window() else { return };
                let browser = cx.entity();
                let volume = self.volume.id;
                cx.defer(move |cx| Self::confirm_entry_delete(&browser, volume, id, owner, cx));
            }
            _ => {}
        }
    }

    pub(super) fn permissions_column(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if self.retired_kv_draft_pending() || self.retired_record_draft_pending() {
            return self.draft_recovery_inspector(window_active, cx);
        }
        if let Some(details) = self.details.clone() {
            return self.entry_inspector(&details, window_active, cx);
        }
        if self.selected.is_some() {
            return div()
                .w(ui(INSPECTOR_WIDTH))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(super::super::theme::column_divider_color())
                .when(self.permission_refresh.entry, |column| {
                    column.child(
                        self.text("entry-permissions-refresh", "Refreshing permissions…", cx)
                            .m(ui(12.))
                            .text_color(gpui::black().alpha(0.5)),
                    )
                });
        }
        self.volume_inspector(window_active, cx)
    }
}

fn menu_item(id: u32, label: &str, disabled: bool) -> crate::runtime::MenuEntry {
    crate::runtime::MenuEntry::Action {
        icon: None,
        id,
        label: label.into(),
        checked: None,
        disabled,
        destructive: false,
    }
}
