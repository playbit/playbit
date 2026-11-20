//! One KV page, one root-bound value and one pending exchange. Full binary keys
//! identify rows; captions never identify storage or delayed completions.
use super::*;
use crate::{
    runtime::host::{DataCall, DataOperation as O, DataResult as R},
    store::{
        key_value::{Chunk, MAX_BYTES, Page, Root},
        volume_index::EntryAddress,
    },
};
mod drafts;
mod focus;
mod layout;
mod rename;
#[cfg(test)]
mod tests;

// One read contains a complete writable value or a bounded read-only preview.
// Hex values use four 4-byte groups per 16-byte line.
const CHUNK_BYTES: u32 = MAX_BYTES as u32;
const MAX_DISPLAY: usize = MAX_BYTES * 2 + (MAX_BYTES - 1) / 4;
const RETIRED_READ_HINT: &str = "The data changed while loading. Reload to continue.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Text,
    Hex,
}

#[derive(Clone, Copy)]
enum DraftField {
    Key,
    Value,
}

impl Encoding {
    fn for_bytes(bytes: &[u8]) -> Self {
        if printable_text(bytes).is_some() { Self::Text } else { Self::Hex }
    }
    fn for_value(bytes: &[u8], truncated: bool) -> Self {
        if value_text(bytes, truncated).is_some() { Self::Text } else { Self::Hex }
    }

    fn display_value(self, bytes: &[u8]) -> Result<String, &'static str> {
        match self {
            Self::Text => std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| "This value contains bytes that are not valid UTF-8. Use Binary."),
            Self::Hex => Ok(hex_value(bytes)),
        }
    }

    fn parse_value(self, text: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Text => self.parse(text, limit, true),
            Self::Hex => parse_hex_value(text, limit),
        }
    }
    fn display_preview(self, chunk: &Chunk) -> Result<String, &'static str> {
        if self == Self::Text {
            value_text(&chunk.bytes, (chunk.bytes.len() as u64) < chunk.total_bytes)
                .map(str::to_owned)
                .ok_or("This value contains bytes that are not valid UTF-8. Use Binary.")
        } else {
            self.display_value(&chunk.bytes)
        }
    }

    fn display(self, bytes: &[u8]) -> Result<String, &'static str> {
        match self {
            Self::Text => printable_text(bytes)
                .map(str::to_owned)
                .ok_or("These bytes cannot be displayed as single-line text. Use Hexadecimal."),
            Self::Hex => Ok(hex(bytes)),
        }
    }

    fn parse(self, text: &str, limit: usize, empty: bool) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Hex => parse_hex(text, limit, empty),
            Self::Text if !empty && text.is_empty() => Err("Enter a key."),
            Self::Text if text.len() > limit => Err("The text exceeds the byte limit."),
            Self::Text => Ok(text.as_bytes().to_vec()),
        }
    }
}

fn printable_text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|text| {
        text.chars().all(|character| {
            !character.is_control() && !matches!(character, '\u{2028}' | '\u{2029}')
        })
    })
}

fn value_text(bytes: &[u8], truncated: bool) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        // A preview may end inside a valid character; Binary still retains every byte.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

pub(super) struct KeyValues {
    identity: Entity<()>,
    sequence: u64,
    pending: Option<Pending>,
    after: Option<Vec<u8>>,
    root: Option<Root>,
    page: Option<Arc<Page>>,
    selected: Option<Vec<u8>>,
    draft_id: Option<Id>,
    draft_revision: Option<u64>,
    creating: bool,
    pub(super) focus_key: bool,
    chunk: Option<Arc<Chunk>>,
    // A successful full Put supplies complete local bytes before a snapshot read returns.
    complete_put: bool,
    key: Entity<text_editor::TextEditor>,
    value: Entity<text_editor::TextEditor>,
    key_representation: Entity<crate::ui::desktop::segmented::Segmented>,
    value_representation: Entity<crate::ui::desktop::segmented::Segmented>,
    key_encoding: Encoding,
    value_encoding: Encoding,
    value_edit_revision: Option<u64>,
    dirty: bool,
    saved: bool,
    revision: u64,
    changed_during_edit: bool,
    uncertain: bool,
    initialized: bool,
    readable: bool,
    refresh_requested: bool,
    read_unavailable: bool,
    retired: Option<drafts::RetiredKvDraft>,
}
struct Pending {
    sequence: u64,
    call: DataCall,
    revision: u64,
    value_edit_revision: Option<u64>,
    read_retried: bool,
    renaming: bool,
    _delivery: crate::trace::Interval,
}
impl KeyValues {
    pub fn new(cx: &mut Context<Browser>) -> Self {
        let identity = cx.new(|_| ());
        let owner = identity.entity_id().as_u64();
        let weak = cx.weak_entity();
        let changed = |field: DraftField| -> text_editor::CommitCallback {
            let weak = weak.clone();
            Arc::new(move |_, revision, cx| {
                weak.update(cx, |browser, cx| {
                    if browser.kv.identity.entity_id().as_u64() == owner {
                        if matches!(field, DraftField::Value) {
                            browser.kv.value_edit_revision = Some(revision);
                        }
                        browser.hint.clear();
                        browser.kv.dirty = true;
                        browser.kv.saved = false;
                        browser.kv.revision = browser
                            .kv
                            .revision
                            .checked_add(1)
                            .expect("browser draft revision exhausted");
                        browser.did_change_draft(cx);
                        cx.notify();
                    }
                })
                .is_ok()
            })
        };
        let key_representation = representation(DraftField::Key, owner, cx);
        let value_representation = representation(DraftField::Value, owner, cx);
        Self {
            key_representation,
            value_representation,
            identity,
            sequence: 0,
            pending: None,
            after: None,
            root: None,
            page: None,
            selected: None,
            draft_id: None,
            draft_revision: None,
            creating: false,
            focus_key: false,
            chunk: None,
            complete_put: false,
            key_encoding: Encoding::Text,
            value_encoding: Encoding::Text,
            value_edit_revision: None,
            dirty: false,
            saved: false,
            revision: 0,
            changed_during_edit: false,
            uncertain: false,
            initialized: false,
            readable: false,
            refresh_requested: false,
            read_unavailable: false,
            retired: None,
            key: cx.new(|cx| {
                text_editor::TextEditor::new("", cx)
                    .inspector_field()
                    .literal()
                    .placeholder("Key")
                    .bound_edits(kvdb::KEY_SIZE_MAX * 2)
                    .on_change(changed(DraftField::Key))
            }),
            value: cx.new(|cx| {
                text_editor::TextEditor::inspector_text_area("", cx)
                    .literal()
                    .placeholder("Value")
                    .bound_edits(MAX_DISPLAY)
                    .on_change(changed(DraftField::Value))
            }),
        }
    }
    fn owner(&self) -> u64 {
        self.identity.entity_id().as_u64()
    }
}

impl Browser {
    pub(super) fn kv_navigation_keys(&self) -> Vec<(String, Vec<u8>)> {
        if self.overview
            || self.kv_address().is_none()
            || !self.details.as_ref().is_some_and(|details| details.readable)
        {
            return Vec::new();
        }
        self.kv
            .page
            .as_ref()
            .map(|page| {
                page.keys
                    .iter()
                    .map(|key| (format!("kv-key-{}", hex(&key.bytes)), key.bytes.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn navigate_kv_key(&mut self, key: &[u8], cx: &mut Context<Self>) -> bool {
        if !self.details.as_ref().is_some_and(|details| details.readable) {
            return false;
        }
        if self.kv.selected.as_deref() == Some(key) && !self.kv.creating {
            return true;
        }
        self.select_kv_key(key.to_vec(), cx);
        self.kv.selected.as_deref() == Some(key) && !self.kv.creating
    }

    pub(super) fn kv_can_add(&self, cx: &App) -> bool {
        self.kv.retired.is_none()
            && self.local_draft_ready(cx)
            && self.details.as_ref().is_some_and(|details| details.writable || details.appendable)
    }
    pub(super) fn show_kv_store(&mut self, cx: &mut Context<Self>) {
        if !self.prepare_entry_navigation(cx) {
            return;
        }
        // Retire the editor's in-flight read without discarding the visible table page.
        let mut kv = KeyValues::new(cx);
        kv.page = self.kv.page.clone();
        kv.root = self.kv.root;
        kv.after = self.kv.after.clone();
        kv.initialized = self.kv.initialized;
        kv.readable = self.kv.readable;
        kv.refresh_requested = self.kv.refresh_requested
            || kv.page.is_none()
            || self
                .kv
                .pending
                .as_ref()
                .is_some_and(|pending| matches!(pending.call.operation, O::KvKeys { .. }));
        self.kv = kv;
        self.overview = false;
        self.cancel_detail_loading();
        self.refresh_kv();
        cx.notify();
    }

    pub(super) fn focus_kv_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.kv.focus_key && self.kv_editor_open() {
            self.kv.focus_key = false;
            self.scrolling.reveal_entry = None;
            let editor = if self.kv.creating { &self.kv.key } else { &self.kv.value };
            editor.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
        }
    }
    pub(super) fn kv_is_loading(&self) -> bool {
        self.kv_address().is_some()
            && self.details.as_ref().is_some_and(|details| details.readable)
            && self.kv.pending.as_ref().is_some_and(|pending| match &pending.call.operation {
                O::KvKeys { .. } => self.kv.page.is_none(),
                O::KvRead { .. } => self.kv.chunk.is_none() && !self.kv.complete_put,
                _ => false,
            })
    }

    pub(super) fn kv_editor_open(&self) -> bool {
        self.kv.selected.is_some() || self.kv.creating
    }

    pub(super) fn kv_status_hint(&self) -> Option<&'static str> {
        if !self.kv_editor_open() || self.kv_address().is_none() {
            return None;
        }
        if self.kv.uncertain {
            return Some(
                "The write outcome could not be confirmed. Inspect the value or discard the draft before another save.",
            );
        }
        if self.kv.read_unavailable {
            return Some(RETIRED_READ_HINT);
        }
        if self.kv.changed_during_edit {
            return Some("This value changed. Review your draft before saving.");
        }
        if self
            .kv
            .chunk
            .as_ref()
            .is_some_and(|chunk| (chunk.bytes.len() as u64) < chunk.total_bytes)
        {
            return Some("Read-only preview. This value is too large to edit here.");
        }
        None
    }

    pub(super) fn has_kv_draft(&self) -> bool {
        self.kv.retired.is_some()
            || self.kv.dirty
            || self.kv.creating
            || self.kv.uncertain
            || self.kv.pending.as_ref().is_some_and(|pending| pending.call.operation.is_mutation())
    }

    pub(super) fn kv_is_composing(&self, cx: &gpui::App) -> bool {
        self.kv.key.read(cx).is_composing() || self.kv.value.read(cx).is_composing()
    }

    pub(super) fn retain_composing_kv_draft(&mut self, cx: &App) {
        if self.kv_is_composing(cx) {
            // Readonly completion cannot deliver a native draft callback. Keep
            // these visible bytes dirty and fence receipts for an earlier Put.
            self.kv.dirty = true;
            self.kv.saved = false;
            self.kv.revision =
                self.kv.revision.checked_add(1).expect("browser draft revision exhausted");
        }
    }

    pub(super) fn kv_address(&self) -> Option<EntryAddress> {
        let details = self.details.as_ref()?;
        if details.entry.deleted
            || !details.backing_ready
            || !matches!(details.entry.info.kind, EntryKind::KvDb {})
            || details.entry.info.protocol != Some(DatabaseProtocol::Kvdb)
        {
            return None;
        }
        Some(EntryAddress { volume: self.volume.id, entry: details.entry.id })
    }
    pub(super) fn update_kv_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if self.kv_address().is_none() {
            self.retire_kv_selection(cx);
            return false;
        }
        if !self.kv.initialized {
            self.kv.initialized = true;
        }
        let readable = self.details.as_ref().is_some_and(|details| details.readable);
        if self.kv.readable && !readable {
            // Keep user input local, then retire all plaintext from the visible view.
            self.retire_kv_selection(cx);
            self.kv.initialized = true;
            return true;
        }
        self.kv.readable = readable;
        if !readable {
            return false;
        }
        if !self.overview {
            let changed = self.kv.pending.is_none() && !self.kv.dirty;
            self.kv.refresh_requested = true;
            self.refresh_kv();
            return changed;
        }
        false
    }
    fn submit_kv(&mut self, operation: O) -> bool {
        self.submit_kv_change(operation, None)
    }
    fn submit_kv_change(
        &mut self,
        operation: O,
        rename: Option<crate::desktop::worker_data::RenameSource>,
    ) -> bool {
        if self.kv.pending.is_some() {
            return false;
        }
        let Some(sequence) = self.kv.sequence.checked_add(1) else {
            self.error = "The browser operation counter is exhausted.".into();
            return false;
        };
        let call = DataCall { volume: *self.volume.id.as_bytes(), operation };
        let delivery = crate::trace::Interval::new(match call.operation {
            O::KvKeys { .. } => "ui.browser.kv.keys",
            O::KvRead { .. } => "ui.browser.kv.read",
            O::KvPut { .. } => "ui.browser.kv.put",
            O::KvDelete { .. } => "ui.browser.kv.delete",
            _ => "ui.browser.kv.invalid",
        });
        let renaming = rename.is_some();
        let admitted = match rename {
            Some(source) => self.send.data_rename(self.kv.owner(), sequence, call.clone(), source),
            None => self.send.data(self.kv.owner(), sequence, call.clone()),
        };
        if let Err(error) = admitted {
            self.error = error.into();
            return false;
        }
        self.kv.sequence = sequence;
        self.kv.read_unavailable = false;
        self.kv.pending = Some(Pending {
            sequence,
            call,
            revision: self.kv.revision,
            value_edit_revision: self.kv.value_edit_revision,
            read_retried: false,
            renaming,
            _delivery: delivery,
        });
        self.error.clear();
        if self.hint == RETIRED_READ_HINT {
            self.hint.clear();
        }
        true
    }
    fn refresh_kv(&mut self) {
        let Some(address) = self.kv_address() else { return };
        if self.kv.pending.is_some() || self.kv.dirty || !self.kv.refresh_requested {
            return;
        }
        self.kv.after = None;
        self.kv.root = None;
        self.kv.refresh_requested = false;
        self.submit_kv(O::KvKeys {
            entry_id: *address.entry.as_bytes(),
            after: None,
            root: None,
            limit: 64,
        });
    }
    fn reload_kv(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.kv_address() else { return };
        if self.kv.pending.is_some()
            || self.kv.uncertain
            || !self.details.as_ref().is_some_and(|details| details.readable)
        {
            return;
        }
        // This explicit read can recover a draft without submitting or replacing it.
        if self.submit_kv(O::KvKeys {
            entry_id: *address.entry.as_bytes(),
            after: None,
            root: None,
            limit: 64,
        }) {
            self.kv.after = None;
            self.kv.root = None;
            self.kv.refresh_requested = false;
        }
        cx.notify();
    }
    fn read_kv(&mut self) {
        let (Some(address), Some(key)) = (self.kv_address(), self.kv.selected.clone()) else {
            return;
        };
        if !self.details.as_ref().is_some_and(|details| details.readable) {
            return;
        }
        self.submit_kv(O::KvRead {
            entry_id: *address.entry.as_bytes(),
            key,
            offset: 0,
            limit: CHUNK_BYTES,
            root: None,
        });
    }
    pub(in crate::desktop) fn receive_kv(
        &mut self,
        reply: super::super::worker_data::DataReply,
        cx: &mut Context<Self>,
    ) {
        crate::desktop::draft_publications::receive_kv(&reply, cx);
        if reply.owner != self.kv.owner()
            || reply.volume != self.volume.id
            || Some(reply.entry) != self.selected
            || self.kv.pending.as_ref().is_none_or(|pending| pending.sequence != reply.sequence)
        {
            return;
        }
        let pending = self.kv.pending.take().expect("correlated pending exchange");
        match reply.result {
            Ok(result) => self.accept_kv(pending, result, reply.committed_root, cx),
            Err(error) => {
                self.kv.uncertain = pending.call.operation.is_mutation()
                    && matches!(
                        error.status,
                        None | Some(
                            playbit_api::abi::syscall::Error::Internal
                                | playbit_api::abi::syscall::Error::StaleHandle
                        )
                    );
                if !pending.call.operation.is_mutation() {
                    self.kv.page = None;
                    self.kv.chunk = None;
                    self.kv.complete_put = false;
                    if !self.kv.dirty && !self.kv_is_composing(cx) {
                        self.kv.value.update(cx, |editor, cx| editor.clear(cx));
                    }
                }
                if matches!(
                    error.status,
                    Some(
                        playbit_api::abi::syscall::Error::Unavailable
                            | playbit_api::abi::syscall::Error::Conflict
                    )
                ) && matches!(pending.call.operation, O::KvKeys { .. } | O::KvRead { .. })
                {
                    // A fenced read contains no publishable plaintext. One fresh
                    // admission can recover; writes and uncertain exchanges never retry.
                    if error.status == Some(playbit_api::abi::syscall::Error::Unavailable)
                        && !pending.read_retried
                        && !self.kv.refresh_requested
                        && self.submit_kv(pending.call.operation)
                    {
                        self.kv.pending.as_mut().expect("submitted fresh read").read_retried = true;
                    } else if !self.kv.refresh_requested || self.kv.dirty {
                        self.kv.read_unavailable = true;
                        self.show_hint(RETIRED_READ_HINT, cx);
                    }
                } else {
                    self.error = error.message;
                }
            }
        }
        self.refresh_kv();
        self.settle_detail_loading();
        self.sync_kv_local_draft(cx);
        self.did_change_draft(cx);
        cx.notify();
    }
    fn accept_kv(
        &mut self,
        pending: Pending,
        result: R,
        committed_root: Option<Root>,
        cx: &mut Context<Self>,
    ) {
        if pending.call.validate_result(&result, Default::default()).is_err() {
            self.error = "The key-value reply does not match its request.".into();
            return;
        }
        match result {
            R::KvKeys(page) if self.details.as_ref().is_some_and(|details| details.readable) => {
                self.kv.page = Some(Arc::new(page));
                if !self.kv.refresh_requested {
                    self.read_kv();
                }
            }
            R::KvRead(chunk) if self.details.as_ref().is_some_and(|details| details.readable) => {
                self.accept_value(chunk, cx);
            }
            R::KvWritten => {
                if pending.renaming && !self.accept_kv_rename(&pending, committed_root) {
                    return;
                }
                self.kv.creating = false;
                self.kv.complete_put = true;
                self.kv.dirty = self.kv.revision != pending.revision;
                self.kv.saved = !self.kv.dirty;
                if self.kv.saved {
                    let value = self.kv.value.read(cx).text().to_owned();
                    self.kv.value.update(cx, |editor, cx| {
                        editor.acknowledge(pending.value_edit_revision, &value, cx)
                    });
                }
                crate::trace::mark(if self.kv.saved {
                    "ui.browser.kv.saved"
                } else {
                    "ui.browser.kv.saved_with_newer_draft"
                });
                self.kv.changed_during_edit = false;
                self.kv.refresh_requested = true;
            }
            R::KvDeleted => {
                if matches!(&pending.call.operation, O::KvDelete { key, .. } if self.kv.selected.as_ref() == Some(key))
                {
                    self.new_kv_value(cx);
                } else {
                    self.kv.refresh_requested = true;
                }
            }
            _ => self.error = "The entry is no longer readable.".into(),
        }
    }
    fn accept_value(&mut self, chunk: Chunk, cx: &mut Context<Self>) {
        self.kv.complete_put = false;
        if self.kv.dirty || self.kv_is_composing(cx) {
            self.kv.changed_during_edit |= self.kv.chunk.as_ref().is_some_and(|previous| {
                previous.offset != chunk.offset
                    || previous.total_bytes != chunk.total_bytes
                    || previous.bytes != chunk.bytes
            });
        } else {
            let preferred =
                Encoding::for_value(&chunk.bytes, (chunk.bytes.len() as u64) < chunk.total_bytes);
            let encoding = if self.kv.chunk.is_none()
                || (self.kv.value_encoding == Encoding::Text && preferred == Encoding::Hex)
            {
                preferred
            } else {
                self.kv.value_encoding
            };
            if let Ok(value) = encoding.display_preview(&chunk) {
                self.kv.value_encoding = encoding;
                self.kv.value.update(cx, |editor, cx| editor.acknowledge(None, &value, cx));
            }
        }
        self.kv.chunk = Some(Arc::new(chunk));
    }
    fn select_kv_key(&mut self, key: Vec<u8>, cx: &mut Context<Self>) {
        if !self.retain_local_drafts(cx) {
            return;
        }
        self.retire_kv_editor(cx);
        if let Some(id) = self.kv_draft_for_key(&key, cx)
            && self.restore_kv_draft(id, cx)
        {
            return;
        }
        self.hint.clear();
        self.kv.selected = Some(key.clone());
        self.kv.key_encoding = Encoding::for_bytes(&key);
        if let Ok(text) = self.kv.key_encoding.display(&key) {
            self.kv.key.update(cx, |editor, cx| editor.acknowledge(None, &text, cx));
        }
        // Opening a key reads its current value independently of the list root.
        self.read_kv();
        self.start_detail_loading(cx);
    }
    pub(super) fn new_kv_value(&mut self, cx: &mut Context<Self>) {
        if !self.kv_can_add(cx) || !self.retain_local_drafts(cx) {
            return;
        }
        self.retire_kv_editor(cx);
        self.hint.clear();
        self.cancel_detail_loading();
        self.kv.creating = true;
        self.kv.focus_key = true;
        self.kv.refresh_requested = true;
        self.did_change_draft(cx);
    }
    fn save_kv(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.kv_address() else { return };
        if !self.kv_can_save(cx) {
            return;
        }
        let Ok((key, value)) = self.kv_draft_bytes(cx) else { return };
        if let Err(error) = self.capture_kv_draft_state(true, cx) {
            self.local_draft_error(&error, cx);
            return;
        }
        let Some(draft) = self.kv.draft_id.and_then(|id| self.local_draft_snapshot(id, cx)) else {
            return;
        };
        let rename = self.kv_rename_source(&key);
        let renaming = rename.is_some();
        if self.submit_kv_change(
            O::KvPut {
                entry_id: *address.entry.as_bytes(),
                key: key.clone(),
                value: value.clone(),
            },
            rename,
        ) {
            if !renaming {
                self.kv.selected = Some(key.clone());
            }
            self.hint.clear();
            crate::desktop::draft_publications::track_kv(
                self.kv.owner(),
                self.kv.sequence,
                draft,
                key,
                value,
                cx,
            );
        } else if let Err(error) = self.capture_kv_draft(cx) {
            self.local_draft_error(&error, cx);
        }
    }

    fn kv_can_delete(&self, cx: &gpui::App) -> bool {
        !self.kv.dirty
            && !self.kv.uncertain
            && self.kv.pending.is_none()
            && self.details.as_ref().is_some_and(|details| details.writable)
            && self.kv_address().is_some()
            && !self.kv_is_composing(cx)
    }

    fn delete_kv(&mut self, key: Vec<u8>, cx: &mut Context<Self>) {
        if !self.kv_can_delete(cx) {
            return;
        }
        let Some(address) = self.kv_address() else { return };
        self.submit_kv(O::KvDelete { entry_id: *address.entry.as_bytes(), key });
        cx.notify();
    }

    fn kv_draft_bytes(&self, cx: &Context<Self>) -> Result<(Vec<u8>, Vec<u8>), String> {
        let key = self
            .kv
            .key_encoding
            .parse(self.kv.key.read(cx).text(), kvdb::KEY_SIZE_MAX, false)
            .map_err(|error| format!("Key: {error}"))?;
        let value = self
            .kv
            .value_encoding
            .parse_value(self.kv.value.read(cx).text(), MAX_BYTES)
            .map_err(|error| format!("Value: {error}"))?;
        Ok((key, value))
    }

    fn kv_can_save(&self, cx: &Context<Self>) -> bool {
        self.local_draft_ready(cx)
            && crate::desktop::draft_publications::available(cx)
            && !self
                .kv
                .draft_id
                .is_some_and(|id| crate::desktop::draft_publications::pending(id, cx))
            && !self.kv.uncertain
            && self.kv.pending.is_none()
            && self.kv.dirty
            && !self.kv_is_composing(cx)
            && self.kv_can_edit(cx)
            && self.kv_draft_bytes(cx).is_ok()
    }
    fn kv_text_available(&self, field: DraftField, cx: &Context<Self>) -> bool {
        match field {
            DraftField::Key => self
                .kv
                .key_encoding
                .parse(self.kv.key.read(cx).text(), kvdb::KEY_SIZE_MAX, true)
                .is_ok_and(|bytes| printable_text(&bytes).is_some()),
            DraftField::Value => {
                if !self.kv.dirty
                    && let Some(chunk) = &self.kv.chunk
                {
                    return value_text(
                        &chunk.bytes,
                        (chunk.bytes.len() as u64) < chunk.total_bytes,
                    )
                    .is_some();
                }
                self.kv
                    .value_encoding
                    .parse_value(self.kv.value.read(cx).text(), MAX_BYTES)
                    .is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok())
            }
        }
    }
    fn set_kv_encoding(&mut self, field: DraftField, encoding: Encoding, cx: &mut Context<Self>) {
        if self.kv.pending.is_some() {
            return;
        }
        let (editor, previous, limit) = match field {
            DraftField::Key => (self.kv.key.clone(), self.kv.key_encoding, kvdb::KEY_SIZE_MAX),
            DraftField::Value => (self.kv.value.clone(), self.kv.value_encoding, MAX_BYTES),
        };
        if editor.read(cx).is_composing() {
            self.show_hint("Finish composing text before changing its representation.", cx);
            return;
        }
        if previous == encoding {
            return;
        }
        let text =
            match field {
                DraftField::Key => previous
                    .parse(editor.read(cx).text(), limit, true)
                    .and_then(|bytes| encoding.display(&bytes)),
                DraftField::Value
                    if !self.kv.dirty
                        && self.kv.chunk.as_ref().is_some_and(|chunk| {
                            (chunk.bytes.len() as u64) < chunk.total_bytes
                        }) =>
                {
                    encoding.display_preview(self.kv.chunk.as_ref().expect("a retained preview"))
                }
                DraftField::Value => previous
                    .parse_value(editor.read(cx).text(), limit)
                    .and_then(|bytes| encoding.display_value(&bytes)),
            };
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                self.show_hint(error, cx);
                return;
            }
        };
        editor.update(cx, |editor, cx| {
            editor.clear(cx);
            editor.acknowledge(None, &text, cx);
        });
        match field {
            DraftField::Key => self.kv.key_encoding = encoding,
            DraftField::Value => self.kv.value_encoding = encoding,
        }
        self.hint.clear();
        self.did_change_draft(cx);
        cx.notify();
    }

    fn kv_can_edit(&self, cx: &App) -> bool {
        if !self.local_draft_ready(cx) {
            return false;
        }
        let Some(details) = &self.details else { return false };
        if self.kv.uncertain {
            return false;
        }
        if self
            .kv
            .pending
            .as_ref()
            .is_some_and(|pending| matches!(&pending.call.operation, O::KvDelete { .. }))
        {
            return false;
        }
        (details.writable || (details.appendable && self.kv.creating && self.kv.pending.is_none()))
            && (self.kv.creating
                || self.kv.complete_put
                || self.kv.chunk.as_ref().is_some_and(|chunk| {
                    chunk.offset == 0
                        && chunk.bytes.len() as u64 == chunk.total_bytes
                        && chunk.bytes.len() <= MAX_BYTES
                })
                || (!details.readable && details.writable))
    }
    fn kv_page(&mut self, after: Option<Vec<u8>>, root: Option<Root>, cx: &mut Context<Self>) {
        let Some(address) = self.kv_address() else { return };
        if !self.retain_local_drafts(cx) {
            return;
        }
        self.retire_kv_editor(cx);
        self.hint.clear();
        self.kv.after = after.clone();
        self.kv.root = root;
        self.kv.page = None;
        self.kv.refresh_requested = false;
        self.submit_kv(O::KvKeys { entry_id: *address.entry.as_bytes(), after, root, limit: 64 });
        self.start_detail_loading(cx);
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("formatting into a string");
    }
    text
}
fn hex_value(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len() * 2 + bytes.len().saturating_sub(1) / 4);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index.is_multiple_of(4) {
            text.push(if index.is_multiple_of(16) { '\n' } else { ' ' });
        }
        write!(text, "{byte:02x}").expect("formatting into a string");
    }
    text
}

fn parse_hex_value(text: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    if text.len() > MAX_DISPLAY {
        return Err("The value exceeds the byte limit.");
    }
    let compact: String =
        text.chars().filter(|character| !character.is_ascii_whitespace()).collect();
    parse_hex(&compact, limit, true)
}
fn partial_hex_byte(text: &str) -> Option<std::ops::Range<usize>> {
    if text.len() > MAX_DISPLAY {
        return None;
    }
    let mut digits: usize = 0;
    let mut last = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte.is_ascii_hexdigit() {
            digits += 1;
            last = index;
        } else if !byte.is_ascii_whitespace() {
            return None;
        }
    }
    (digits % 2 == 1 && digits.div_ceil(2) <= MAX_BYTES).then_some(last..last + 1)
}
fn parse_hex(text: &str, limit: usize, empty: bool) -> Result<Vec<u8>, &'static str> {
    if !text.len().is_multiple_of(2) || text.len() / 2 > limit || (!empty && text.is_empty()) {
        return Err("Enter an even number of hexadecimal digits within the byte limit.");
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            Ok(digit(pair[0]).ok_or("Use hexadecimal digits (0–9, a–f).")? << 4
                | digit(pair[1]).ok_or("Use hexadecimal digits (0–9, a–f).")?)
        })
        .collect()
}

fn representation(
    field: DraftField,
    owner: u64,
    cx: &mut Context<Browser>,
) -> Entity<crate::ui::desktop::segmented::Segmented> {
    let weak = cx.weak_entity();
    cx.new(|cx| {
        crate::ui::desktop::segmented::Segmented::new(
            &representation_choices(false),
            0,
            false,
            &theme(),
            Rc::new(move |id, cx| {
                let _ = weak.update(cx, |browser, cx| {
                    if browser.kv.owner() != owner {
                        return;
                    }
                    match id {
                        0 => browser.set_kv_encoding(field, Encoding::Text, cx),
                        1 => browser.set_kv_encoding(field, Encoding::Hex, cx),
                        _ => return,
                    }
                    cx.notify();
                });
            }),
            cx,
        )
    })
}

fn representation_choices(text_disabled: bool) -> Vec<crate::runtime::SegmentEntry> {
    vec![
        crate::runtime::SegmentEntry { id: 0, label: "Text".into(), disabled: text_disabled },
        crate::runtime::SegmentEntry { id: 1, label: "Binary".into(), disabled: false },
    ]
}
