use super::*;
use crate::desktop::worker_data::RenameSource;

impl Browser {
    pub(super) fn kv_can_rename(&self, cx: &Context<Self>) -> bool {
        self.kv_can_edit(cx)
            && self.details.as_ref().is_some_and(|details| details.readable && details.writable)
            && !self.kv.creating
            && self.kv.selected.is_some()
            && self.kv.chunk.as_ref().is_some_and(|chunk| {
                chunk.offset == 0
                    && chunk.total_bytes == chunk.bytes.len() as u64
                    && chunk.bytes.len() <= MAX_BYTES
            })
    }

    pub(super) fn kv_rename_source(&self, new_key: &[u8]) -> Option<RenameSource> {
        if self.kv.creating {
            return None;
        }
        let old_key = self.kv.selected.as_ref()?;
        if old_key == new_key {
            return None;
        }
        Some(RenameSource { old_key: old_key.clone(), expected: self.kv.chunk.as_ref()?.root })
    }

    pub(super) fn accept_kv_rename(&mut self, pending: &Pending, root: Option<Root>) -> bool {
        let (Some(root), O::KvPut { key, value, .. }) = (root, &pending.call.operation) else {
            self.kv.uncertain = true;
            self.error = "The rename receipt is incomplete; reload before retrying.".into();
            return false;
        };
        self.kv.selected = Some(key.clone());
        self.kv.chunk = Some(Arc::new(Chunk {
            root,
            offset: 0,
            total_bytes: value.len() as u64,
            bytes: value.clone(),
        }));
        // Retire the old table cut before the authoritative new-key refresh.
        self.kv.page = None;
        self.kv.root = None;
        self.kv.after = None;
        true
    }
}
