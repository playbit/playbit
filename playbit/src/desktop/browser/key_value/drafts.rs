//! Keep raw editor input separately from canonical values and their write receipts.
use super::*;
use crate::desktop::drafts as repository;
use crate::store::browser_drafts::{
    BrowserDraft, BrowserDraftBody, BrowserDraftTarget, DraftEncoding, KvDraft, KvDraftBase,
};

pub(super) struct RetiredKvDraft {
    account: Id,
    existing: Option<Id>,
    draft: BrowserDraft,
}

impl Encoding {
    fn stored(self) -> DraftEncoding {
        match self {
            Self::Text => DraftEncoding::Text,
            Self::Hex => DraftEncoding::Hex,
        }
    }
    fn restored(encoding: DraftEncoding) -> Self {
        match encoding {
            DraftEncoding::Text => Self::Text,
            DraftEncoding::Hex => Self::Hex,
        }
    }
}

impl Browser {
    pub(in crate::desktop::browser) fn capture_kv_draft(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.capture_kv_draft_state(false, cx)
    }

    pub(super) fn capture_kv_draft_state(
        &mut self,
        admitting_put: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.store_retired_kv_draft(cx)?;
        let Some(body) = self.raw_kv_draft(admitting_put, cx) else {
            return Ok(());
        };
        let Some(entry) = self.selected else { return Ok(()) };
        let existing = self.kv.draft_id.or_else(|| {
            (!self.kv.creating)
                .then(|| self.kv.selected.as_ref().and_then(|key| self.kv_draft_for_key(key, cx)))
                .flatten()
        });
        self.kv.draft_id =
            Some(self.store_local_draft(BrowserDraftBody::Kv(body), Some(entry), existing, cx)?);
        self.kv.draft_revision = Some(self.kv.revision);
        Ok(())
    }

    fn raw_kv_draft(&mut self, admitting_put: bool, cx: &mut Context<Self>) -> Option<KvDraft> {
        let composing = self.kv_is_composing(cx);
        let publishing = admitting_put
            || self
                .kv
                .pending
                .as_ref()
                .is_some_and(|pending| matches!(pending.call.operation, O::KvPut { .. }));
        if !self.kv_editor_open()
            || !(self.kv.dirty || self.kv.creating || self.kv.uncertain || composing || publishing)
        {
            return None;
        }
        if composing && !self.kv.dirty {
            self.retain_composing_kv_draft(cx);
        }
        Some(KvDraft {
            creating: self.kv.creating,
            original_key: if self.kv.creating { None } else { self.kv.selected.clone() },
            key_text: self.kv.key.read(cx).text().to_owned(),
            value_text: self.kv.value.read(cx).text().to_owned(),
            key_encoding: self.kv.key_encoding.stored(),
            value_encoding: self.kv.value_encoding.stored(),
            base: self.kv.chunk.as_ref().map(|chunk| KvDraftBase {
                root: chunk.root,
                offset: chunk.offset,
                total_bytes: chunk.total_bytes,
                bytes: chunk.bytes.clone(),
            }),
            changed_during_edit: self.kv.changed_during_edit,
            uncertain: self.kv.uncertain || publishing,
            read_unavailable: self.kv.read_unavailable,
            complete_put: self.kv.complete_put,
        })
    }

    pub(in crate::desktop::browser) fn retired_kv_draft_pending(&self) -> bool {
        self.kv.retired.is_some()
    }

    pub(in crate::desktop::browser) fn retired_kv_draft_busy(&self, cx: &App) -> bool {
        self.kv.retired.as_ref().is_some_and(|retired| {
            retired.account != self.local_draft_account()
                || retired
                    .existing
                    .is_some_and(|id| crate::desktop::draft_publications::pending(id, cx))
        })
    }

    fn store_retired_kv_draft(&mut self, cx: &mut Context<Self>) -> Result<Option<Id>, String> {
        let Some(retired) = &self.kv.retired else { return Ok(None) };
        if retired.account != self.local_draft_account()
            || !repository::ready_for(retired.account, cx)
        {
            return Err("The account owning these retained local changes is not ready.".into());
        }
        let mut draft = retired.draft.clone();
        if retired.existing.is_none() {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes)
                .map_err(|error| format!("Couldn’t create a local draft identity: {error}"))?;
            draft.id = Id::from_bytes(bytes);
        }
        let id = repository::upsert(draft, cx)?;
        self.kv.retired = None;
        Ok(Some(id))
    }

    pub(in crate::desktop::browser) fn retry_retired_kv_draft(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.store_retired_kv_draft(cx) {
            Ok(id) => {
                if let Some(id) = id {
                    self.restore_kv_draft(id, cx);
                }
                cx.notify();
                true
            }
            Err(error) => {
                self.local_draft_error(&error, cx);
                false
            }
        }
    }

    pub(in crate::desktop::browser) fn discard_retired_kv_draft(&mut self, cx: &mut Context<Self>) {
        if self.retired_kv_draft_busy(cx) {
            return;
        }
        let Some(retired) = &self.kv.retired else { return };
        if retired.account != self.local_draft_account() {
            return;
        }
        if let Some(id) = retired.existing
            && repository::get_for(retired.account, id, cx).is_some_and(|draft| {
                draft.volume == retired.draft.volume && draft.entry == retired.draft.entry
            })
        {
            repository::remove(id, cx);
        }
        self.kv.retired = None;
        self.error.clear();
        self.alerted_error.clear();
        cx.notify();
    }

    pub(in crate::desktop::browser) fn retire_kv_selection(&mut self, cx: &mut Context<Self>) {
        if self.kv.retired.is_some() {
            return;
        }
        let failure = self.capture_kv_draft(cx).err();
        let mut retired = self.kv.retired.take();
        if retired.is_none()
            && failure.is_some()
            && let Some(body) = self.raw_kv_draft(false, cx)
            && let Some(entry) = self.selected
        {
            let existing = self.kv.draft_id.or_else(|| {
                self.kv.selected.as_ref().and_then(|key| self.kv_draft_for_key(key, cx))
            });
            let draft = BrowserDraft {
                id: existing.unwrap_or(Id::from_bytes([0; 16])),
                revision: 1,
                volume: self.volume.id,
                entry: Some(entry),
                ancestors: self
                    .catalog
                    .columns
                    .iter()
                    .filter_map(|column| column.parent)
                    .take_while(|id| *id != entry)
                    .collect(),
                body: BrowserDraftBody::Kv(body),
            };
            // Both editor fields and the sole validated chunk have smaller admitted
            // bounds; this DTO is one hidden replacement for those retiring entities.
            assert!(draft.encoded_len().is_ok(), "bounded KV editor produces a bounded draft");
            assert!(
                draft
                    .retained_bytes()
                    .is_ok_and(|bytes| bytes <= crate::store::browser_drafts::MAX_RECORD_BYTES),
                "one retired KV draft fits the 512 KiB editor recovery allocation"
            );
            retired = Some(RetiredKvDraft { account: self.local_draft_account(), existing, draft });
        }
        self.kv = KeyValues::new(cx);
        self.kv.retired = retired;
        if let Some(error) = failure {
            self.local_draft_error(&error, cx);
        }
    }

    pub(super) fn kv_draft_for_key(&self, key: &[u8], cx: &App) -> Option<Id> {
        self.find_local_draft(self.selected, |target| matches!(target, BrowserDraftTarget::Kv { original_key: Some(original), creating: false } if original == key), cx)
    }

    pub(in crate::desktop::browser) fn kv_creating_drafts(
        &self,
        cx: &App,
    ) -> Vec<Arc<BrowserDraft>> {
        if !self.details.as_ref().is_some_and(|details| details.readable) {
            return Vec::new();
        }
        self.local_draft_summaries(cx)
            .into_iter()
            .filter(|draft| {
                draft.volume == self.volume.id
                    && draft.entry == self.selected
                    && matches!(draft.target, BrowserDraftTarget::Kv { creating: true, .. })
            })
            .filter_map(|draft| self.local_draft_snapshot(draft.id, cx))
            .collect()
    }

    /// Retire local delivery ownership; an admitted canonical write keeps running globally.
    pub(super) fn retire_kv_editor(&mut self, cx: &mut Context<Self>) {
        let mut next = KeyValues::new(cx);
        next.page = self.kv.page.clone();
        next.root = self.kv.root;
        next.after = self.kv.after.clone();
        next.initialized = self.kv.initialized;
        next.readable = self.kv.readable;
        next.refresh_requested = self.kv.refresh_requested;
        next.retired = self.kv.retired.take();
        self.kv = next;
    }

    pub(in crate::desktop::browser) fn restore_kv_draft(
        &mut self,
        id: Id,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(draft) = self.local_draft_snapshot(id, cx) else {
            return false;
        };
        let Some(address) = self.kv_address() else { return false };
        if draft.volume != self.volume.id
            || draft.entry != self.selected
            || !self.details.as_ref().is_some_and(|details| details.readable)
        {
            return false;
        }
        let BrowserDraftBody::Kv(state) = &draft.body else {
            return false;
        };
        if !state.creating && state.original_key.is_none() {
            return false;
        }
        if let Err(error) = self.store_retired_kv_draft(cx) {
            self.local_draft_error(&error, cx);
            return false;
        }
        let keys = self
            .kv
            .pending
            .as_ref()
            .and_then(|pending| match &pending.call.operation {
                O::KvKeys { entry_id, after, root, limit } => {
                    Some((*entry_id, after.clone(), *root, *limit))
                }
                _ => None,
            })
            .or_else(|| {
                self.kv.page.is_none().then_some((*address.entry.as_bytes(), None, None, 64))
            });
        self.retire_kv_editor(cx);
        self.kv.draft_id = Some(id);
        self.kv.draft_revision = Some(self.kv.revision);
        self.kv.selected = state.original_key.clone();
        self.kv.creating = state.creating;
        self.kv.key_encoding = Encoding::restored(state.key_encoding);
        self.kv.value_encoding = Encoding::restored(state.value_encoding);
        self.kv.chunk = state.base.as_ref().map(|base| {
            Arc::new(Chunk {
                root: base.root,
                offset: base.offset,
                total_bytes: base.total_bytes,
                bytes: base.bytes.clone(),
            })
        });
        self.kv.complete_put = state.complete_put;
        self.kv.changed_during_edit = state.changed_during_edit;
        self.kv.uncertain = state.uncertain;
        self.kv.read_unavailable = state.read_unavailable;
        self.kv.dirty = true;
        self.kv.key.update(cx, |editor, cx| editor.acknowledge(None, &state.key_text, cx));
        let formatted = (self.kv.value_encoding == Encoding::Hex)
            .then(|| {
                parse_hex_value(&state.value_text, MAX_BYTES).ok().map(|bytes| hex_value(&bytes))
            })
            .flatten();
        // Older drafts may contain user-entered spaces or unfinished input.
        // Reformat complete bytes and retain the original input if it cannot be parsed.
        let value = formatted.as_deref().unwrap_or(&state.value_text);
        self.kv.value.update(cx, |editor, cx| editor.acknowledge(None, value, cx));
        self.overview = false;
        self.hint.clear();
        self.cancel_detail_loading();
        if let Some((entry_id, after, root, limit)) = keys {
            // Editor ownership also owns read delivery. Carry only the canceled
            // list read forward; its reply can then load this row's current value.
            if self.submit_kv(O::KvKeys { entry_id, after: after.clone(), root, limit }) {
                self.kv.after = after;
                self.kv.root = root;
                self.kv.refresh_requested = false;
            }
        } else if !state.creating {
            self.read_kv();
        }
        cx.notify();
        true
    }

    pub(in crate::desktop::browser) fn sync_kv_local_draft(&mut self, cx: &mut Context<Self>) {
        if self.kv.retired.is_some() {
            // Rejected admission also notifies global observers. Retry only on a
            // user action or metadata retirement, never from this notification.
            return;
        }
        let Some(id) = self.kv.draft_id else {
            return;
        };
        if !self.local_draft_ready(cx)
            || self.kv.pending.is_some()
            || self.kv_is_composing(cx)
            || self.kv.draft_revision != Some(self.kv.revision)
        {
            return;
        }
        if let Some(draft) = self.local_draft_snapshot(id, cx) {
            if draft.volume != self.volume.id || draft.entry != self.selected {
                return;
            }
            let BrowserDraftBody::Kv(state) = &draft.body else {
                return;
            };
            self.kv.uncertain = state.uncertain;
            self.kv.changed_during_edit = state.changed_during_edit;
            self.kv.complete_put = state.complete_put;
            if !state.creating {
                self.kv.selected = state.original_key.clone();
                self.kv.creating = false;
            }
            self.kv.chunk = state.base.as_ref().map(|base| {
                Arc::new(Chunk {
                    root: base.root,
                    offset: base.offset,
                    total_bytes: base.total_bytes,
                    bytes: base.bytes.clone(),
                })
            });
        } else {
            self.kv.draft_id = None;
            self.kv.draft_revision = None;
            self.kv.uncertain = false;
            self.kv.dirty = false;
            self.kv.saved = true;
            self.kv.complete_put = true;
            self.kv.creating = false;
            if let Ok((key, _)) = self.kv_draft_bytes(cx) {
                self.kv.selected = Some(key);
            }
        }
        cx.notify();
    }

    pub(super) fn discard_kv_draft(&mut self, cx: &mut Context<Self>) {
        if self.kv.pending.as_ref().is_some_and(|pending| pending.call.operation.is_mutation())
            || self
                .kv
                .draft_id
                .is_some_and(|id| crate::desktop::draft_publications::pending(id, cx))
            || self.kv_is_composing(cx)
        {
            return;
        }
        if let Some(id) = self.kv.draft_id {
            if !self.local_draft_ready(cx) {
                return;
            }
            if let Some(draft) = self.local_draft_snapshot(id, cx) {
                if draft.volume != self.volume.id || draft.entry != self.selected {
                    return;
                }
                repository::remove(id, cx);
            }
            self.kv.draft_id = None;
        }
        self.kv.draft_revision = None;
        self.kv.dirty = false;
        self.kv.uncertain = false;
        self.kv.creating = false;
        self.hint.clear();
        if let Some(key) = self.kv.selected.clone() {
            self.select_kv_key(key, cx);
        } else {
            self.show_kv_store(cx);
        }
    }
}
