use super::*;
use crate::ui::desktop::typography::tabular_numerals;
use crate::ui::desktop::zoom::length as ui;

impl Browser {
    pub(in crate::desktop::browser) fn kv_metadata(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let key = self.kv.selected.as_deref().map(|bytes| {
            printable_text(bytes).map(str::to_owned).unwrap_or_else(|| format!("0x{}", hex(bytes)))
        });
        let key = self.text("kv-metadata-key", key.unwrap_or_else(|| "Unsaved".into()), cx);
        let bytes = if self.kv.dirty || self.kv.creating {
            self.kv
                .value_encoding
                .parse_value(self.kv.value.read(cx).text(), MAX_BYTES)
                .ok()
                .map(|bytes| bytes.len() as u64)
                .or_else(|| self.kv.chunk.as_ref().map(|chunk| chunk.total_bytes))
        } else {
            self.kv.chunk.as_ref().map(|chunk| chunk.total_bytes)
        };
        let size = tabular_numerals(self.text(
            "kv-metadata-size",
            bytes.map(files::format_byte_size).unwrap_or_else(|| "—".into()),
            cx,
        ));
        div()
            .flex()
            .flex_col()
            .px(ui(12.))
            .child(self.metadata_row("kv-metadata-key", "Key", key.truncate(), cx))
            .child(self.metadata_row("kv-metadata-size", "Size", size, cx))
    }

    pub(in crate::desktop::browser) fn kv_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let labels =
            [self.text("kv-key-heading", "Key", cx), self.text("kv-value-heading", "Value", cx)];
        let add = self.button(
            "kv-add",
            "New entry",
            !self.kv_can_add(cx),
            |this, cx| {
                this.overview = false;
                this.new_kv_value(cx);
            },
            cx,
        );
        add.update(cx, |control, cx| {
            control.set_sidebar_icon(controls::SidebarIcon::Glyph("add"), false, false, cx);
        });
        let header = crate::ui::desktop::table::header(labels, window)
            .debug_selector(|| "kv-header".into())
            .child(div().flex_none().occlude().child(add));
        let content = if self.kv.page.is_none() && self.kv_is_loading() {
            self.loading_panel(cx)
        } else {
            self.kv_list(cx)
        };
        div().flex().flex_col().size_full().min_w_0().child(header).child(content)
    }

    fn kv_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let page = self.kv.page.clone();
        // An empty padded scroll Div counts its viewport height as content, then
        // adds padding as overflow. A spacer keeps this inset within the content.
        let mut rows = div()
            .id("kv-keys")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scrolling.records.read(cx).handle())
            .child(div().h(ui(8.)).flex_none());
        if let Some(page) = &page {
            for key in &page.keys {
                let bytes = key.bytes.clone();
                let full = hex(&bytes);
                let label = printable_text(&bytes)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("0x{full}"));
                let (preview, binary) = preview_label(&key.preview, key.len_bytes);
                let control = self.button(
                    &format!("kv-key-{full}"),
                    &label,
                    false,
                    move |this, cx| this.select_kv_key(bytes.clone(), cx),
                    cx,
                );
                let menu = self.kv_key_menu(key.bytes.clone(), cx);
                let accessory = self
                    .kv_draft_for_key(&key.bytes, cx)
                    .map(|id| self.draft_button(id, &format!("key-{full}"), cx));
                control.update(cx, |control, cx| {
                    control.set_context_menu(menu);
                    control.select_navigation(self.kv.selected.as_ref() == Some(&key.bytes), cx);
                    control.set_table_cells(&label, &preview, binary, cx);
                    control.set_navigation_accessory(accessory, cx);
                });
                rows = rows.child(div().h(ui(24.)).flex_none().child(control));
            }
        } else if !self.details.as_ref().is_some_and(|d| d.readable) {
            rows = rows.child(
                self.text("kv-read-denied", "Read access is required to list keys.", cx)
                    .px(ui(16.)),
            );
        } else if self.kv.read_unavailable && !self.kv_editor_open() {
            let reload = self.button(
                "kv-reload",
                "Reload",
                self.kv.pending.is_some() || self.kv.uncertain,
                |this, cx| this.reload_kv(cx),
                cx,
            );
            rows = rows.child(div().px(ui(8.)).child(reload));
        }
        for draft in self.kv_creating_drafts(cx) {
            let crate::store::browser_drafts::BrowserDraftBody::Kv(state) = &draft.body else {
                continue;
            };
            let id = draft.id;
            let label = if state.key_text.is_empty() { "New entry" } else { &state.key_text };
            let (preview, binary) = draft_preview_label(&state.value_text, state.value_encoding);
            let control = self.button(
                &format!("kv-draft-{id}"),
                label,
                false,
                move |this, cx| {
                    this.open_local_draft(id, cx);
                },
                cx,
            );
            let accessory = self.draft_button(id, &format!("new-key-{id}"), cx);
            control.update(cx, |control, cx| {
                control.select_navigation(self.kv.draft_id == Some(id), cx);
                control.set_table_cells(label, &preview, binary, cx);
                control.set_navigation_accessory(Some(accessory), cx);
            });
            rows = rows.child(div().h(ui(24.)).flex_none().child(control));
        }
        let panel = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .debug_selector(|| "kv-list".into())
            .child(scrolling::viewport(rows, &self.scrolling.records));
        self.kv_paging(panel, page.as_deref(), cx)
    }

    fn kv_key_menu(&self, key: Vec<u8>, cx: &Context<Self>) -> controls::ContextMenu {
        let owner = self.kv.owner();
        let weak = cx.weak_entity();
        Rc::new(move |event, window, cx| {
            let Some(browser) = weak.upgrade() else { return };
            let disabled =
                browser.read(cx).kv.owner() != owner || !browser.read(cx).kv_can_delete(cx);
            let weak = weak.clone();
            let key = key.clone();
            let activate = Rc::new(move |_, cx: &mut gpui::App| {
                let _ = weak.update(cx, |browser, cx| {
                    if browser.kv.owner() == owner {
                        browser.delete_kv(key.clone(), cx);
                    }
                });
            });
            let request = crate::ui::desktop::popup::Request::context(
                window,
                event.position,
                vec![crate::runtime::MenuEntry::Action {
                    id: 0,
                    label: "Delete".into(),
                    icon: Some(crate::runtime::MenuGlyph::Asset("menu-delete")),
                    checked: None,
                    disabled,
                    destructive: true,
                }],
                activate,
                cx,
            );
            cx.defer(move |cx| crate::ui::desktop::popup::open(request, cx));
        })
    }

    fn kv_paging(
        &mut self,
        mut panel: gpui::Div,
        page: Option<&Page>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let next = page.and_then(|page| page.next.clone());
        let root = page.and_then(|page| page.root);
        // Keep bounded key paging available; entry creation is the heading's direct + action.
        if self.kv.after.is_some() || next.is_some() {
            let first = self.button(
                "kv-first",
                "First",
                self.kv.after.is_none(),
                |this, cx| this.kv_page(None, None, cx),
                cx,
            );
            let more = self.button(
                "kv-next",
                "Next",
                next.is_none(),
                move |this, cx| this.kv_page(next.clone(), root, cx),
                cx,
            );
            panel = panel.child(div().flex().gap(ui(8.)).p(ui(8.)).child(first).child(more));
        }
        panel
    }

    pub(in crate::desktop::browser) fn kv_editor(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        if !self.kv.dirty
            && self.kv_is_loading()
            && self
                .kv
                .pending
                .as_ref()
                .is_some_and(|pending| matches!(&pending.call.operation, O::KvRead { .. }))
        {
            return self.loading_panel(cx);
        }
        let editable = self.kv_can_edit(cx);
        let renameable = self.kv_can_rename(cx);
        let theme = theme();
        self.kv.key.update(cx, |editor, cx| {
            editor.set_theme(&theme);
            editor.set_binary(self.kv.key_encoding == Encoding::Hex, cx);
            let partial = (self.kv.key_encoding == Encoding::Hex)
                .then(|| partial_hex_byte(editor.text()))
                .flatten();
            editor.set_validation_underline(partial, cx);
            editor.set_read_only(
                !editable
                    || (!self.kv.creating && !renameable)
                    || self
                        .kv
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.call.operation.is_mutation()),
                &theme,
                cx,
            );
            editor.set_trailing_padding(px(28.));
            editor.set_placeholder(if self.kv.key_encoding == Encoding::Text {
                "Key"
            } else {
                "Key (hexadecimal)"
            });
        });
        self.kv.value.update(cx, |editor, cx| {
            editor.set_theme(&theme);
            editor.set_read_only(!editable, &theme, cx);
            editor.set_binary(self.kv.value_encoding == Encoding::Hex, cx);
            let partial = (self.kv.value_encoding == Encoding::Hex)
                .then(|| partial_hex_byte(editor.text()))
                .flatten();
            editor.set_validation_underline(partial, cx);
            editor.set_trailing_padding(px(28.));
            editor.set_placeholder(if self.kv.value_encoding == Encoding::Text {
                "Value"
            } else {
                "Value (hexadecimal)"
            });
        });
        let panel = div()
            .id("kv-value")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(ui(8.))
            .px(ui(12.))
            .pb(ui(12.))
            .child(self.kv_field(DraftField::Key, cx))
            .child(self.kv_field(DraftField::Value, cx))
            .children(self.kv_insert_control(cx).map(|insert| div().flex_none().child(insert)));
        if self.kv.saved {
            crate::trace::mark("ui.browser.kv.saved_rendered");
        }
        div().debug_selector(|| "kv-editor".into()).size_full().min_h_0().child(panel)
    }

    fn kv_field(&mut self, field: DraftField, cx: &mut Context<Self>) -> gpui::Div {
        let (encoding, editor, representation) = match field {
            DraftField::Key => {
                (self.kv.key_encoding, self.kv.key.clone(), self.kv.key_representation.clone())
            }
            DraftField::Value => (
                self.kv.value_encoding,
                self.kv.value.clone(),
                self.kv.value_representation.clone(),
            ),
        };
        let text_disabled = !self.kv_text_available(field, cx);
        let composing = editor.read(cx).is_composing();
        representation.update(cx, |control, cx| {
            control.set(
                &representation_choices(text_disabled),
                if encoding == Encoding::Text { 0 } else { 1 },
                self.kv.pending.is_some() || composing,
                &theme(),
                cx,
            );
            control.set_compact(["T", "0x"], cx);
        });
        let key = matches!(field, DraftField::Key);
        let toggle = div()
            .debug_selector(move || {
                if key { "kv-key-representation" } else { "kv-value-representation" }.into()
            })
            .absolute()
            .right_0()
            .size(ui(24.))
            .occlude()
            .when(key, |toggle| toggle.top_0())
            .when(!key, |toggle| toggle.bottom_0())
            .child(representation);
        div()
            .debug_selector(move || if key { "kv-key-field" } else { "kv-value-field" }.into())
            .relative()
            .w_full()
            .min_w_0()
            .when(key, |field| field.h(ui(24.)).flex_none())
            .when(!key, |field| field.flex_1().min_h_0())
            .child(editor)
            .child(toggle)
    }

    fn kv_insert_control(&mut self, cx: &mut Context<Self>) -> Option<Entity<controls::Control>> {
        if !self.kv_uses_binary_value() || !self.kv_can_edit(cx) {
            return None;
        }
        let owner = self.kv.owner();
        let editor = self.kv.value.read(cx);
        let state = if editor.inserts_hex_bytes() { CheckState::On } else { CheckState::Off };
        let disabled = !editor.can_insert_hex();
        Some(self.control(
            "kv-insert".into(),
            "Insert bytes",
            controls::Kind::Checkbox(state),
            disabled,
            move |this, cx| {
                if this.kv.owner() == owner && this.kv_uses_binary_value() && this.kv_can_edit(cx) {
                    this.kv.value.update(cx, |editor, cx| editor.toggle_hex_insert(cx));
                }
            },
            cx,
        ))
    }

    pub(in crate::desktop::browser) fn kv_actions(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let mut actions = div().flex().gap(ui(8.));
        if self.kv.read_unavailable {
            actions = actions.child(self.button(
                "kv-reload",
                "Reload",
                self.kv.pending.is_some() || self.kv.uncertain,
                |this, cx| this.reload_kv(cx),
                cx,
            ));
        }
        if !(self.kv.creating || self.kv.dirty || self.kv.uncertain) {
            return actions;
        }
        let disabled = !self.kv_can_save(cx);
        let save = self.control(
            "kv-save".into(),
            "Save Changes",
            controls::Kind::Button(ButtonStyle::Primary),
            disabled,
            |this, cx| this.save_kv(cx),
            cx,
        );
        save.update(cx, |control, cx| control.set_button_size(100., 24., cx));
        let disabled =
            self.kv.pending.as_ref().is_some_and(|pending| pending.call.operation.is_mutation())
                || self
                    .kv
                    .draft_id
                    .is_some_and(|id| crate::desktop::draft_publications::pending(id, cx))
                || self.kv_is_composing(cx)
                || !(self.kv.creating || self.kv.dirty || self.kv.uncertain);
        let discard = self.button(
            "kv-discard",
            "Discard Changes",
            disabled,
            |this, cx| {
                this.discard_kv_draft(cx);
            },
            cx,
        );
        discard.update(cx, |control, cx| control.set_button_size(114., 24., cx));
        actions.child(discard).child(save)
    }
}

pub(super) fn draft_preview_label(
    text: &str,
    encoding: crate::store::browser_drafts::DraftEncoding,
) -> (String, bool) {
    use crate::store::{browser_drafts::DraftEncoding, key_value::PREVIEW_BYTES};
    if encoding == DraftEncoding::Text {
        return preview_label(&text.as_bytes()[..text.len().min(PREVIEW_BYTES)], text.len() as u64);
    }
    // Binary drafts can contain unfinished bytes; flatten only their bounded caption.
    let label = text
        .chars()
        .take(PREVIEW_BYTES)
        .map(|character| if character.is_whitespace() { ' ' } else { character })
        .collect();
    (label, true)
}

pub(super) fn preview_label(bytes: &[u8], len_bytes: u64) -> (String, bool) {
    let truncated = (bytes.len() as u64) < len_bytes;
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
    .filter(|text| printable_text(text.as_bytes()).is_some());
    let binary = text.is_none();
    let mut label = text.map(str::to_owned).unwrap_or_else(|| format!("0x{}", hex(bytes)));
    if truncated {
        label.push('…');
    }
    (label, binary)
}
