//! Entry menus keep their original identity across native popup and dialog delivery.
use super::*;
use crate::store::volume_index::{EntryAddress, EntrySummary};

const RENAME: u32 = 0;
const DUPLICATE: u32 = 1;
const DELETE: u32 = 2;
const REVEAL: u32 = 3;
const OPEN: u32 = 4;

impl Browser {
    pub(super) fn bind_entry_menu(
        &self,
        id: EntryId,
        control: &Entity<controls::Control>,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let target = control.downgrade();
        let volume = self.volume.id;
        control.update(cx, |control, _| {
            control.set_context_menu(Rc::new(move |event, window, cx| {
                let Some(browser) = weak.upgrade() else { return };
                if browser.read(cx).volume.id != volume {
                    return;
                }
                let items = browser.read(cx).entry_menu_items(id, cx);
                if items.is_empty() {
                    return;
                }
                let owner = window.window_handle();
                let weak = browser.downgrade();
                let activate = Rc::new(move |action, cx: &mut App| {
                    let Some(browser) = weak.upgrade() else { return };
                    if action == DELETE {
                        Self::confirm_entry_delete(&browser, volume, id, owner, cx);
                    } else {
                        browser.update(cx, |this, cx| {
                            this.entry_context_action(volume, id, action, cx)
                        });
                    }
                });
                let request = crate::ui::desktop::popup::Request::context(
                    window,
                    event.position,
                    items,
                    activate,
                    cx,
                )
                .target(target.clone());
                cx.defer(move |cx| crate::ui::desktop::popup::open(request, cx));
                cx.stop_propagation();
            }));
        });
    }

    fn menu_entry(&self, id: EntryId) -> Option<&EntrySummary> {
        if self.entry_is_hidden(id) {
            return None;
        }
        self.catalog_entry(id).filter(|entry| !entry.deleted)
    }

    fn entry_action_name(&self, id: EntryId) -> Option<&str> {
        if self.entry_is_hidden(id) {
            return None;
        }
        self.menu_entry(id).map(|entry| entry.name.as_str()).or_else(|| {
            self.details
                .as_ref()
                .filter(|details| details.entry.id == id && !details.entry.deleted)
                .map(|details| details.entry.info.name.as_str())
        })
    }

    fn entry_action_enabled(&self, id: EntryId, action: u32, _cx: &App) -> bool {
        if self.entry_action_name(id).is_none() || self.has_pending_entry_operation() {
            return false;
        }
        let details = self.details.as_ref().filter(|details| details.entry.id == id);
        match action {
            RENAME | DELETE => details.is_none_or(|details| details.writable),
            DUPLICATE => details.is_none_or(|details| {
                details.readable
                    && !details.schema_pending
                    && (details.backing_ready
                        || matches!(details.entry.info.kind, EntryKind::Directory {}))
            }),
            REVEAL => self.signed_in_account.is_some(),
            OPEN => {
                self.signed_in_account.is_some()
                    && self
                        .menu_entry(id)
                        .is_some_and(|entry| entry.kind == crate::store::EntryType::Blob)
                    && details.is_none_or(|details| details.readable && details.backing_ready)
            }
            _ => false,
        }
    }

    fn entry_menu_items(&self, id: EntryId, cx: &App) -> Vec<crate::runtime::MenuEntry> {
        if self.menu_entry(id).is_none() {
            return vec![];
        }
        let mut items = Vec::with_capacity(5);
        if self.menu_entry(id).is_some_and(|entry| entry.kind == crate::store::EntryType::Blob) {
            items.push((OPEN, "Open", "menu-open"));
        }
        items.extend([
            (RENAME, "Rename", "menu-rename"),
            (DUPLICATE, "Duplicate", "menu-duplicate"),
            (DELETE, "Delete…", "menu-delete"),
            (REVEAL, "Reveal in Finder", "menu-finder"),
        ]);
        items
            .into_iter()
            .map(|(action, label, glyph)| crate::runtime::MenuEntry::Action {
                id: action,
                label: label.into(),
                icon: Some(crate::runtime::MenuGlyph::Asset(glyph)),
                checked: None,
                disabled: !self.entry_action_enabled(id, action, cx),
                destructive: action == DELETE,
            })
            .collect()
    }

    fn entry_context_action(
        &mut self,
        volume: Id,
        id: EntryId,
        action: u32,
        cx: &mut Context<Self>,
    ) {
        if self.volume.id != volume
            || !self.entry_action_enabled(id, action, cx)
            || !self.prepare_navigation(cx)
        {
            return;
        }
        match action {
            RENAME => {
                let Some(name) = self.menu_entry(id).map(|entry| entry.name.clone()) else {
                    return;
                };
                self.select(id, cx);
                if self.selected == Some(id) {
                    self.start_rename(id, &name, cx);
                }
            }
            DUPLICATE => {
                self.start_file(Request::DuplicateEntry {
                    address: EntryAddress { volume, entry: id },
                });
            }
            OPEN => {
                if let Some(account) = self.signed_in_account {
                    super::super::file_provider::open_entry(
                        account,
                        EntryAddress { volume, entry: id },
                        cx,
                    );
                }
            }
            REVEAL => {
                if let Some(account) = self.signed_in_account {
                    super::super::file_provider::reveal_entry(
                        account,
                        EntryAddress { volume, entry: id },
                        cx,
                    );
                }
            }
            _ => return,
        }
        cx.notify();
    }

    pub(super) fn confirm_entry_delete(
        browser: &Entity<Self>,
        volume: Id,
        id: EntryId,
        owner: gpui::AnyWindowHandle,
        cx: &mut App,
    ) {
        let title = browser.update(cx, |this, cx| {
            if this.volume.id != volume
                || !this.entry_action_enabled(id, DELETE, cx)
                || !this.prepare_navigation(cx)
            {
                return None;
            }
            this.entry_action_name(id).map(|name| format!("Delete “{name}”?"))
        });
        let Some(title) = title else { return };
        let response = owner.update(cx, |_, window, cx| {
            window.prompt(
                PromptLevel::Warning,
                &title,
                Some("This removes the item from this volume on all your devices."),
                &[PromptButton::Cancel("Cancel".into()), PromptButton::Ok("Delete".into())],
                cx,
            )
        });
        let Ok(response) = response else { return };
        browser.update(cx, |this, cx| {
            this.files.dialog = true;
            cx.notify();
        });
        let browser = browser.downgrade();
        cx.spawn(async move |cx| {
            let confirmed = response.await == Ok(1);
            let _ = browser.update(cx, |this, cx| {
                this.files.dialog = false;
                if confirmed
                    && this.volume.id == volume
                    && this.entry_action_enabled(id, DELETE, cx)
                    && this.prepare_navigation(cx)
                {
                    // A directory may contain the active entry outside this catalog page.
                    // Retire clean editors and fence selection until the delete completes.
                    if !this.show_volume(cx) {
                        return;
                    }
                    this.start_file(Request::RemoveEntry { volume, entry: id });
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert correlated native entry actions.")]
mod tests {
    use super::super::tests::{details, entry, volume};
    use super::*;
    use crate::engine::reply::DiagnosticReply;

    fn page() -> EntryPage {
        EntryPage {
            entries: (1..=2)
                .map(|byte| EntrySummary {
                    id: entry(byte).id,
                    parent: None,
                    name: format!("Item {byte}"),
                    kind: crate::store::EntryType::KvDb,
                    protocol: Some(DatabaseProtocol::Kvdb),
                    deleted: false,
                    name_conflict: false,
                })
                .collect(),
            next: None,
        }
    }

    #[gpui::test]
    fn entry_actions_keep_the_popup_identity_and_correlate_copy_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |this, cx| {
            this.fixture_catalog(page());
            this.signed_in_account = Some(volume(1).owner);
            this.selected = Some(entry(1).id);
            this.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
            assert!(!this.entry_action_enabled(entry(1).id, RENAME, cx));
            assert!(this.entry_action_enabled(entry(2).id, RENAME, cx));
            assert_eq!(this.entry_menu_items(entry(2).id, cx).len(), 4);
            while requests.try_recv().is_ok() {}
            this.entry_context_action(volume(1).id, entry(2).id, DUPLICATE, cx);
            assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
                request: Request::DuplicateEntry { address }, ..
            }) if address == EntryAddress { volume: volume(1).id, entry: entry(2).id }));
            assert_eq!(this.selected, Some(entry(1).id));
            assert_eq!(
                this.files.pending,
                Some(files::Pending::Duplicate(EntryAddress {
                    volume: volume(1).id,
                    entry: entry(2).id,
                }))
            );
            this.entry_context_action(volume(1).id, entry(1).id, DUPLICATE, cx);
            assert!(requests.try_recv().is_err());
            assert!(!this.receive_file(
                DiagnosticReply::EntryDuplicated {
                    source: EntryAddress { volume: volume(1).id, entry: entry(1).id },
                    entry: entry(3),
                },
                cx
            ));
            assert!(this.receive_file(
                DiagnosticReply::EntryDuplicated {
                    source: EntryAddress { volume: volume(1).id, entry: entry(2).id },
                    entry: entry(3),
                },
                cx
            ));
            assert!(this.files.pending.is_none());
            assert_eq!(this.selected, Some(entry(3).id));
            assert_eq!(this.renaming.as_ref().unwrap().entry, entry(3).id);
        });
    }

    #[gpui::test]
    fn entry_delete_confirms_once_and_removes_the_original_item(cx: &mut gpui::TestAppContext) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, requests, _) = worker_queue::channel();
        let window = cx.add_window(|_, cx| {
            let mut this = Browser::new(volume(1), send, cx);
            this.fixture_catalog(page());
            this
        });
        let browser = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        while requests.try_recv().is_ok() {}
        cx.update(|cx| {
            Browser::confirm_entry_delete(&browser, volume(1).id, entry(2).id, window.into(), cx);
            Browser::confirm_entry_delete(&browser, volume(1).id, entry(2).id, window.into(), cx);
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert!(requests.try_recv().is_err());
        cx.update(|cx| {
            Browser::confirm_entry_delete(&browser, volume(1).id, entry(2).id, window.into(), cx)
        });
        browser.update(cx, |this, _| this.selected = Some(entry(1).id));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::RemoveEntry { volume: got_volume, entry: got_entry }, ..
        }) if got_volume == volume(1).id && got_entry == entry(2).id));
        assert!(requests.try_recv().is_err());
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.label() == Some("Item 1"))
                );
                assert!(
                    !window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.label() == Some("Item 2"))
                );
            })
            .unwrap();
        browser.update(cx, |this, cx| {
            assert!(this.files.pending.is_some());
            this.select(entry(1).id, cx);
            assert_eq!(this.selected, None);
            this.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(2).id }, cx);
            assert!(this.files.pending.is_none());
            assert_eq!(this.selected, None);
        });
    }

    #[gpui::test]
    fn delete_projection_restores_failure_and_keeps_confirmed_descendants_hidden(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, _requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |this, cx| {
            let mut original = page();
            original.entries[0].kind = crate::store::EntryType::Directory;
            original.entries[0].protocol = None;
            original.entries[1].parent = Some(entry(1).id);
            this.selected = Some(entry(2).id);
            this.fixture_namespace(original.entries.clone());
            this.selected = None;
            let original_catalog = this.catalog.clone();
            let request = Request::RemoveEntry { volume: volume(1).id, entry: entry(1).id };
            this.start_file(request.clone());
            assert!(this.entry_is_hidden(entry(1).id));
            assert!(this.entry_is_hidden(entry(2).id));
            assert!(this.entry_menu_items(entry(2).id, cx).is_empty());
            this.failed_file(
                files::Pending::Delete(EntryAddress { volume: volume(2).id, entry: entry(1).id }),
                "Unrelated failure",
                cx,
            );
            assert!(this.entry_is_hidden(entry(2).id));
            this.failed(Failure::for_request(&request).unwrap(), "Delete failed", cx);
            assert!(!this.entry_is_hidden(entry(1).id));
            assert!(!this.entry_is_hidden(entry(2).id));
            assert_eq!(this.catalog, original_catalog);
            this.start_file(request);
            this.refresh();
            assert!(this.entry_is_hidden(entry(2).id), "pre-ack catalog cannot undo projection");
            this.receive(Reply::EntryChanged { volume: volume(1).id, entry: entry(1).id }, cx);
            assert!(this.files.pending.is_none());
            assert!(this.catalog_summaries().iter().all(|entry| entry.deleted));
            assert!(
                this.entry_is_hidden(entry(2).id),
                "confirmed removal survives refresh latency"
            );
            assert_eq!(this.catalog_summaries().len(), 2, "retained catalog is intact");
            this.select(entry(2).id, cx);
            assert!(this.selected.is_none());
        });
    }

    #[gpui::test]
    fn selected_entry_header_confirms_deletion_after_its_catalog_page_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, requests, _) = worker_queue::channel();
        let window = cx.add_window(|_, cx| {
            let mut this = Browser::new(volume(1), send, cx);
            let mut selected = details(2, EntryAcl::Inherit);
            selected.entry.info.kind = EntryKind::Directory {};
            selected.writable = true;
            this.selected = Some(selected.entry.id);
            this.details = Some(Arc::new(selected));
            let mut page = page();
            page.entries.truncate(1);
            this.fixture_catalog(page);
            this
        });
        let activate = window
            .update(cx, |this, window, cx| {
                window.activate_window();
                this.content_header(true, cx);
                this.controls["entry-menu"].read(cx).popup_for_test().2
            })
            .unwrap();
        while requests.try_recv().is_ok() {}
        cx.update(|cx| activate(4, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
    }
}
