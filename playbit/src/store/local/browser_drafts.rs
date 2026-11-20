//! The engine's profile writer owns bounded, non-replicating editor recovery.
use super::*;
use crate::store::browser_drafts::{
    BrowserDraft, BrowserDraftBody, BrowserDraftOperation, BrowserDraftResult, MAX_ACCOUNT_BYTES,
    MAX_DRAFTS, MAX_RECORD_BYTES,
};
use kvdb::storage::ValueRead;

fn prefix(account: Id) -> String {
    format!("settings/browser-drafts/{account}/")
}
fn key(account: Id, id: Id) -> String {
    format!("{}{id}", prefix(account))
}

fn read(reader: &dyn ValueRead) -> Result<BrowserDraft> {
    if reader.len() > MAX_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = vec![0; reader.len()];
    reader.read_exact_at(0, &mut bytes)?;
    let draft: BrowserDraft = record::decode(&bytes)?;
    draft.validate()?;
    Ok(draft)
}

fn validate_target(old: &BrowserDraft, draft: &BrowserDraft) -> Result<()> {
    if old.id != draft.id
        || old.volume != draft.volume
        || old.entry != draft.entry
        || std::mem::discriminant(&old.body) != std::mem::discriminant(&draft.body)
    {
        return Err(Error::InvalidInput("browser draft target cannot change"));
    }
    if let (BrowserDraftBody::Record(old), BrowserDraftBody::Record(new)) = (&old.body, &draft.body)
        && (old.type_id != new.type_id || old.object != new.object)
    {
        return Err(Error::InvalidInput("record draft target cannot change"));
    }
    Ok(())
}

impl LocalStore {
    pub fn browser_drafts(
        &self,
        account: Id,
        operation: BrowserDraftOperation,
    ) -> Result<BrowserDraftResult> {
        if account != self.account.id {
            return Err(Error::AccessDenied("browser drafts belong to another account"));
        }
        match operation {
            BrowserDraftOperation::List => self.list_browser_drafts(account),
            BrowserDraftOperation::Get { id } => {
                let tx = self.metadata.begin_read()?;
                let draft = tx
                    .value_reader(key(account, id).as_bytes())?
                    .map(|reader| read(&reader))
                    .transpose()?;
                if draft.as_ref().is_some_and(|draft| draft.id != id) {
                    return Err(Error::corrupt("browser draft key mismatch"));
                }
                Ok(BrowserDraftResult::Draft { id, draft: draft.map(Box::new) })
            }
            BrowserDraftOperation::Upsert { expected_revision, mut draft } => {
                draft.canonicalize();
                draft.validate()?;
                self.upsert_browser_draft(account, expected_revision, *draft)
            }
            BrowserDraftOperation::Remove { id, expected_revision } => {
                self.remove_browser_draft(account, id, expected_revision)
            }
        }
    }

    fn list_browser_drafts(&self, account: Id) -> Result<BrowserDraftResult> {
        let prefix = prefix(account);
        let tx = self.metadata.begin_read()?;
        let mut summaries = Vec::with_capacity(MAX_DRAFTS);
        let mut bytes = 0usize;
        tx.visit::<Error>(prefix.as_bytes(), false, |stored_key, reader| {
            if !stored_key.starts_with(prefix.as_bytes()) {
                return Ok(false);
            }
            if summaries.len() == MAX_DRAFTS {
                return Err(Error::LimitExceeded);
            }
            bytes = bytes
                .checked_add(reader.len())
                .filter(|bytes| *bytes <= MAX_ACCOUNT_BYTES)
                .ok_or(Error::LimitExceeded)?;
            let draft = read(reader)?;
            if stored_key != key(account, draft.id).as_bytes() {
                return Err(Error::corrupt("browser draft key mismatch"));
            }
            summaries.push(draft.summary());
            Ok(true)
        })?;
        Ok(BrowserDraftResult::List(summaries))
    }

    fn upsert_browser_draft(
        &self,
        account: Id,
        expected_revision: Option<u64>,
        draft: BrowserDraft,
    ) -> Result<BrowserDraftResult> {
        if expected_revision.unwrap_or(0).checked_add(1) != Some(draft.revision) {
            return Err(Error::InvalidInput("browser draft revision must advance by one"));
        }
        // Bound the caller's payload before opening the profile writer.
        let encoded = record::encode(&draft)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let draft_key = key(account, draft.id);
        let prefix = prefix(account);
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let mut old = None;
        tx.visit::<Error>(draft_key.as_bytes(), false, |stored_key, reader| {
            if stored_key == draft_key.as_bytes() {
                old = Some(read(reader)?);
            }
            Ok(false)
        })?;
        if old.as_ref().map(|old| old.revision) != expected_revision {
            return Err(Error::LocalContention("browser draft changed; reload before updating it"));
        }
        if let Some(old) = &old {
            validate_target(old, &draft)?;
        }
        // Preserve optional future PBR fields on an existing record.
        let encoded = if old.is_some() {
            let old = tx
                .get(draft_key.as_bytes())?
                .ok_or(Error::Internal("draft disappeared inside profile transaction"))?;
            record::rewrite(&old, &draft)?
        } else {
            encoded
        };
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let mut count = 0usize;
        let mut total = encoded.len();
        tx.visit::<Error>(prefix.as_bytes(), false, |stored_key, reader| {
            if !stored_key.starts_with(prefix.as_bytes()) {
                return Ok(false);
            }
            count += 1;
            if count > MAX_DRAFTS || reader.len() > MAX_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
            if stored_key != draft_key.as_bytes() {
                total = total
                    .checked_add(reader.len())
                    .filter(|total| *total <= MAX_ACCOUNT_BYTES)
                    .ok_or(Error::LimitExceeded)?;
            }
            Ok(true)
        })?;
        if old.is_none() && count == MAX_DRAFTS {
            return Err(Error::LimitExceeded);
        }
        tx.put(draft_key.as_bytes(), &encoded)?;
        tx.commit()?;
        Ok(BrowserDraftResult::Stored { id: draft.id, revision: draft.revision })
    }

    fn remove_browser_draft(
        &self,
        account: Id,
        id: Id,
        expected_revision: u64,
    ) -> Result<BrowserDraftResult> {
        let draft_key = key(account, id);
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let mut old = None;
        tx.visit::<Error>(draft_key.as_bytes(), false, |stored_key, reader| {
            if stored_key == draft_key.as_bytes() {
                old = Some(read(reader)?);
            }
            Ok(false)
        })?;
        if old.as_ref().is_none_or(|old| old.id != id || old.revision != expected_revision) {
            return Err(Error::LocalContention(
                "browser draft changed; an older completion cannot remove it",
            ));
        }
        tx.del(draft_key.as_bytes())?;
        tx.commit()?;
        Ok(BrowserDraftResult::Removed { id, revision: expected_revision })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert durable local editor recovery.")]
mod tests {
    use super::*;
    use crate::store::EntryId;
    use crate::store::browser_drafts::*;
    use crate::store::object_contract::{Value, ValueType};

    fn draft(id: u8) -> BrowserDraft {
        BrowserDraft {
            id: Id::from_bytes([id; 16]),
            revision: 1,
            volume: Id::from_bytes([42; 16]),
            entry: Some(EntryId::from_bytes([43; 15]).unwrap()),
            ancestors: vec![EntryId::from_bytes([44; 15]).unwrap()],
            body: BrowserDraftBody::Kv(KvDraft {
                creating: false,
                original_key: Some(vec![0, 255, 128]),
                key_text: "00ff800".into(),
                value_text: "ff 0\nabc".into(),
                key_encoding: DraftEncoding::Hex,
                value_encoding: DraftEncoding::Hex,
                base: Some(KvDraftBase {
                    root: crate::store::key_value::Root { level: 2, digest: [9; 32] },
                    offset: 0,
                    total_bytes: crate::store::key_value::MAX_BYTES as u64,
                    bytes: vec![0x80; crate::store::key_value::MAX_BYTES],
                }),
                changed_during_edit: true,
                uncertain: true,
                read_unavailable: true,
                complete_put: false,
            }),
        }
    }
    fn put(
        store: &LocalStore,
        account: Id,
        expected_revision: Option<u64>,
        draft: BrowserDraft,
    ) -> Result<BrowserDraftResult> {
        store.browser_drafts(
            account,
            BrowserDraftOperation::Upsert { expected_revision, draft: Box::new(draft) },
        )
    }
    fn get(store: &LocalStore, account: Id, id: Id) -> BrowserDraft {
        let BrowserDraftResult::Draft { draft: Some(draft), .. } =
            store.browser_drafts(account, BrowserDraftOperation::Get { id }).unwrap()
        else {
            panic!("retained draft")
        };
        *draft
    }

    #[test]
    fn binary_partial_input_modes_new_rows_and_record_values_survive_reopen_without_publication() {
        let directory = tempfile::tempdir().unwrap();
        let existing = draft(1);
        let mut creating = draft(2);
        let BrowserDraftBody::Kv(value) = &mut creating.body else { unreachable!() };
        value.creating = true;
        value.original_key = None;
        value.key_text.clear();
        value.base = None;
        let mut record_draft = draft(3);
        record_draft.body = BrowserDraftBody::Record(RecordDraft {
            type_id: Id::from_bytes([4; 16]),
            object: Id::from_bytes([5; 16]),
            fields: vec![RecordFieldDraft {
                field: Id::from_bytes([6; 16]),
                text: "unfinished scalar".into(),
                base_values: Some(vec![Value::Text("x".repeat(MAX_FIELD_TEXT))]),
            }],
            values: vec![
                RecordValuesDraft {
                    field: Id::from_bytes([7; 16]),
                    kind: ValueType::Bytes,
                    repeated: true,
                    base_values: None,
                    values: Some(vec![Value::Bytes(vec![1, 2, 3])]),
                    input: "ff0".into(),
                    editing: Some(0),
                    after: 0,
                    input_dirty: true,
                    dirty: true,
                },
                RecordValuesDraft {
                    field: Id::from_bytes([8; 16]),
                    kind: ValueType::Text,
                    repeated: true,
                    base_values: Some(vec![]),
                    values: None,
                    input: "second unfinished value".into(),
                    editing: None,
                    after: 0,
                    input_dirty: true,
                    dirty: true,
                },
            ],
            active_values: Some(Id::from_bytes([8; 16])),
            changed_during_edit: true,
        });
        let account;
        {
            let store = LocalStore::open(directory.path()).unwrap();
            account = store.account.id;
            let pending = store.pending_change_count();
            for draft in [existing.clone(), creating.clone(), record_draft.clone()] {
                put(&store, account, None, draft).unwrap();
            }
            assert_eq!(
                store.pending_change_count(),
                pending,
                "local drafts publish no replica notice"
            );
        }
        let store = LocalStore::open(directory.path()).unwrap();
        for expected in [existing, creating, record_draft] {
            assert_eq!(get(&store, account, expected.id), expected);
        }
        let BrowserDraftResult::List(summaries) =
            store.browser_drafts(account, BrowserDraftOperation::List).unwrap()
        else {
            panic!("list")
        };
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].ancestors, vec![EntryId::from_bytes([44; 15]).unwrap()]);
    }

    #[test]
    fn account_gate_and_compare_and_swap_preserve_newer_edits_and_reject_old_deletion_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account.id;
        let mut edited = draft(1);
        let id = edited.id;
        put(&store, account, None, edited.clone()).unwrap();
        edited.revision = 2;
        let BrowserDraftBody::Kv(value) = &mut edited.body else { unreachable!() };
        value.value_text = "newer".into();
        put(&store, account, Some(1), edited.clone()).unwrap();
        assert!(put(&store, account, Some(1), edited.clone()).is_err());
        assert!(
            store
                .browser_drafts(account, BrowserDraftOperation::Remove { id, expected_revision: 1 })
                .is_err()
        );
        assert_eq!(get(&store, account, id), edited);
        for operation in [
            BrowserDraftOperation::List,
            BrowserDraftOperation::Get { id },
            BrowserDraftOperation::Upsert {
                expected_revision: Some(2),
                draft: Box::new(edited.clone()),
            },
            BrowserDraftOperation::Remove { id, expected_revision: 2 },
        ] {
            assert!(store.browser_drafts(Id::from_bytes([99; 16]), operation).is_err());
        }
        store
            .browser_drafts(account, BrowserDraftOperation::Remove { id, expected_revision: 2 })
            .unwrap();
        edited.revision = 1;
        put(&store, account, None, edited.clone()).unwrap();
        assert_eq!(
            get(&store, account, id),
            edited,
            "physical deletion permits a fresh revision sequence"
        );
    }

    #[test]
    fn quota_never_evicts_and_invalid_or_damaged_records_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account.id;
        for id in 1..=MAX_DRAFTS as u8 {
            put(&store, account, None, draft(id)).unwrap();
        }
        assert!(put(&store, account, None, draft(100)).is_err());
        assert_eq!(get(&store, account, draft(1).id), draft(1));
        let mut oversized = draft(1);
        oversized.revision = 2;
        let BrowserDraftBody::Kv(value) = &mut oversized.body else { unreachable!() };
        value.value_text = "a".repeat(MAX_VALUE_TEXT + 1);
        assert!(put(&store, account, Some(1), oversized).is_err());
        let stored_key = key(account, draft(1).id);
        let damaged = vec![3; MAX_RECORD_BYTES + 1];
        let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
        tx.put(stored_key.as_bytes(), &damaged).unwrap();
        tx.commit().unwrap();
        assert!(
            store.browser_drafts(account, BrowserDraftOperation::Get { id: draft(1).id }).is_err()
        );
        assert!(
            store
                .browser_drafts(
                    account,
                    BrowserDraftOperation::Remove { id: draft(1).id, expected_revision: 1 }
                )
                .is_err()
        );
        let tx = store.metadata.begin_read().unwrap();
        assert_eq!(tx.value_reader(stored_key.as_bytes()).unwrap().unwrap().len(), damaged.len());
    }

    #[test]
    fn dormant_permissions_remain_local_and_replacement_acl_is_canonical() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account.id;
        let acl = Acl {
            grants: vec![crate::store::object_contract::Grant {
                subject: crate::store::object_contract::Subject::Public,
                permissions: vec![crate::store::object_contract::Permission::Read],
            }],
        };
        let mut permissions = draft(1);
        permissions.body = BrowserDraftBody::Permissions(PermissionDraft {
            base_active: EntryAcl::Inherit,
            base_retained: None,
            active: EntryAcl::Inherit,
            retained: Some(acl.clone()),
            adding_grant: true,
            grant_input: "unfinished ID".into(),
            grant_target: None,
            grant_service: true,
        });
        put(&store, account, None, permissions.clone()).unwrap();
        assert_eq!(get(&store, account, permissions.id), permissions);
        permissions.revision = 2;
        let BrowserDraftBody::Permissions(value) = &mut permissions.body else { unreachable!() };
        value.active = EntryAcl::Replace(acl.clone());
        value.retained = None;
        put(&store, account, Some(1), permissions.clone()).unwrap();
        permissions.canonicalize();
        assert_eq!(get(&store, account, permissions.id), permissions);
    }

    #[test]
    fn updating_a_draft_preserves_unknown_optional_record_fields() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account.id;
        let mut draft = draft(1);
        put(&store, account, None, draft.clone()).unwrap();
        let stored_key = key(account, draft.id);
        let mut bytes = record::encode(&draft).unwrap();
        let extension = [99, 0, 2, 0, 0, 0, record::TEXT, b'x'];
        bytes.extend_from_slice(&extension);
        let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
        tx.put(stored_key.as_bytes(), &bytes).unwrap();
        tx.commit().unwrap();
        draft.revision = 2;
        put(&store, account, Some(1), draft.clone()).unwrap();
        assert_eq!(get(&store, account, draft.id), draft);
        let tx = store.metadata.begin_read().unwrap();
        assert!(tx.get(stored_key.as_bytes()).unwrap().unwrap().ends_with(&extension));
    }
}
