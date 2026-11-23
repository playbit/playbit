//! Bounded, installation-local editor recovery. Rendering never waits for storage.
use crate::store::{
    EntryId, Id,
    browser_drafts::{
        BrowserDraft, BrowserDraftBody, BrowserDraftOperation as Operation,
        BrowserDraftResult as Outcome, BrowserDraftSummary, MAX_ACCOUNT_BYTES, MAX_DRAFTS,
    },
};
use gpui::{App, Global};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

struct Record {
    target: Target,
    draft: Option<Arc<BrowserDraft>>,
    disk_revision: Option<u64>,
    generation: u64,
    bytes: usize,
}

#[derive(PartialEq, Eq)]
struct Target {
    volume: Id,
    entry: Option<EntryId>,
    body: std::mem::Discriminant<BrowserDraftBody>,
    object: Option<(Id, Id)>,
}

impl Target {
    fn of(draft: &BrowserDraft) -> Self {
        Self {
            volume: draft.volume,
            entry: draft.entry,
            body: std::mem::discriminant(&draft.body),
            object: match &draft.body {
                BrowserDraftBody::Record(record) => Some((record.type_id, record.object)),
                _ => None,
            },
        }
    }
}

#[derive(Clone)]
struct Write {
    id: Id,
    generation: u64,
    expected: Option<u64>,
    draft: Option<Arc<BrowserDraft>>,
    bytes: usize,
}

enum Completion {
    Acknowledged { revision: Option<u64>, persisted: bool },
    Conflict(Option<Box<BrowserDraft>>),
}

/// One profile/account cache. Observers receive local edits and durable completions.
pub(super) struct Drafts {
    account: Id,
    profile: Option<PathBuf>,
    ready: bool,
    loading: bool,
    records: BTreeMap<Id, Record>,
    pending: BTreeMap<Id, u64>,
    active: Option<Write>,
    failed: Option<Write>,
    paused: bool,
    quit_after_save: bool,
}
impl Global for Drafts {}

impl Drafts {
    fn new(account: Id, profile: Option<PathBuf>) -> Self {
        Self {
            account,
            ready: profile.is_none(),
            profile,
            loading: false,
            records: BTreeMap::new(),
            pending: BTreeMap::new(),
            active: None,
            failed: None,
            paused: false,
            quit_after_save: false,
        }
    }

    fn busy(&self) -> bool {
        self.active.is_some() || self.failed.is_some() || !self.pending.is_empty()
    }

    fn retained_bytes(&self) -> usize {
        let bytes: usize = self.records.values().map(|record| record.bytes).sum();
        let held = self.active.as_ref().or(self.failed.as_ref());
        bytes
            + held.map_or(0, |write| {
                if self.records.get(&write.id).is_some_and(|record| {
                    record.generation == write.generation && record.draft.is_some()
                }) {
                    0
                } else {
                    write.bytes
                }
            })
    }

    fn upsert(&mut self, mut draft: BrowserDraft) -> Result<Id, String> {
        if !self.ready {
            return Err("Local drafts have not finished loading. Your editor is unchanged.".into());
        }
        let old = self.records.get(&draft.id);
        let target = Target::of(&draft);
        if old.is_some_and(|record| record.target != target) {
            return Err("A local draft cannot change its volume, entry, or record target.".into());
        }
        // Disk CAS belongs to this repository, never to the editor's revision counter.
        draft.revision = old.and_then(|record| record.disk_revision).unwrap_or(1);
        draft.canonicalize();
        draft.validate().map_err(|error| error.to_string())?;
        draft.encoded_len().map_err(|error| error.to_string())?;
        if old.and_then(|record| record.draft.as_deref()) == Some(&draft) {
            return Ok(draft.id);
        }
        if old.is_none() && self.records.len() == MAX_DRAFTS {
            return Err("There are already 32 local drafts. Save or discard one first.".into());
        }
        let bytes = draft.retained_bytes().map_err(|error| error.to_string())?;
        let generation = old
            .map_or(Some(1), |record| record.generation.checked_add(1))
            .ok_or("The local draft revision counter is exhausted.")?;
        let old_bytes = old.map_or(0, |record| record.bytes);
        let held = self.active.as_ref().or(self.failed.as_ref());
        let detached = held
            .filter(|write| {
                write.id == draft.id
                    && old.is_some_and(|record| record.generation == write.generation)
                    && write.draft.is_some()
            })
            .map_or(0, |write| write.bytes);
        if self.retained_bytes() - old_bytes + bytes + detached > MAX_ACCOUNT_BYTES {
            return Err(
                "Local drafts exceed the 8 MiB limit. Save or discard a draft first.".into()
            );
        }
        let id = draft.id;
        let disk_revision = old.and_then(|record| record.disk_revision);
        self.records.insert(
            id,
            Record { target, draft: Some(Arc::new(draft)), disk_revision, generation, bytes },
        );
        if self.profile.is_some() {
            self.pending.insert(id, generation);
            self.paused = false;
        }
        Ok(id)
    }

    fn remove(&mut self, id: Id) -> bool {
        let Some(record) = self.records.get_mut(&id).filter(|record| record.draft.is_some()) else {
            return false;
        };
        record.generation =
            record.generation.checked_add(1).expect("local draft generation exhausted");
        record.draft = None;
        record.bytes = 0;
        let generation = record.generation;
        if self.profile.is_none()
            || record.disk_revision.is_none()
                && self.active.as_ref().is_none_or(|write| write.id != id)
                && self.failed.as_ref().is_none_or(|write| write.id != id)
        {
            self.records.remove(&id);
            self.pending.remove(&id);
        } else {
            self.pending.insert(id, generation);
            self.paused = false;
        }
        true
    }

    fn next_write(&mut self) -> Result<Option<Write>, String> {
        while let Some((id, generation)) = self.pending.pop_first() {
            let record = self.records.get(&id).expect("pending draft has retained state");
            if record.draft.is_none() && record.disk_revision.is_none() {
                self.records.remove(&id);
                continue;
            }
            if record.draft.is_some()
                && record.disk_revision.is_some_and(|revision| revision == u64::MAX)
            {
                self.pending.insert(id, generation);
                return Err("The saved draft revision counter is exhausted.".into());
            }
            return Ok(Some(Write {
                id,
                generation,
                expected: record.disk_revision,
                draft: record.draft.clone(),
                bytes: record.bytes,
            }));
        }
        Ok(None)
    }

    fn acknowledge(&mut self, write: &Write, disk: Option<u64>, persisted: bool) {
        let record = self.records.get_mut(&write.id).expect("active draft has retained state");
        record.disk_revision = disk;
        if let Some(draft) = &mut record.draft
            && draft.revision != disk.unwrap_or(1)
        {
            Arc::make_mut(draft).revision = disk.unwrap_or(1);
        }
        if persisted && record.generation == write.generation {
            self.pending.remove(&write.id);
            if record.draft.is_none() {
                self.records.remove(&write.id);
            }
        } else {
            self.pending.insert(write.id, record.generation);
        }
    }

    fn complete(&mut self, write: &Write, completion: Completion) -> Result<bool, String> {
        let peer = match completion {
            Completion::Acknowledged { revision, persisted } => {
                self.acknowledge(write, revision, persisted);
                return Ok(false);
            }
            Completion::Conflict(peer) => peer,
        };
        let record = self.records.get(&write.id).expect("active draft has retained state");
        if record.draft.is_some() {
            return Err("Another local copy changed this draft. Your current edit is retained. Discard it to keep the other local copy.".into());
        }
        let bytes = peer
            .as_deref()
            .map_or(Ok(0), BrowserDraft::retained_bytes)
            .map_err(|error| error.to_string())?;
        if self.retained_bytes() + bytes > MAX_ACCOUNT_BYTES {
            return Err("The other local copy exceeds the 8 MiB draft memory limit. Save or discard another draft, then retry.".into());
        }
        // A tombstone is the user's decision to retire this local intent. Never send
        // its stale CAS deletion against the other writer's replacement.
        let adopted = peer.is_some();
        if let Some(peer) = peer {
            self.records.insert(
                write.id,
                Record {
                    target: Target::of(&peer),
                    disk_revision: Some(peer.revision),
                    draft: Some(Arc::from(peer)),
                    generation: record.generation,
                    bytes,
                },
            );
        } else {
            self.records.remove(&write.id);
        }
        self.pending.remove(&write.id);
        Ok(adopted)
    }
}

pub(super) fn configure(account: Id, profile: PathBuf, cx: &mut App) -> Result<(), String> {
    if let Some(state) = cx.try_global::<Drafts>() {
        if state.account == account && state.profile.as_ref() == Some(&profile) {
            return Ok(());
        }
        if state.loading || state.busy() {
            return Err(
                "Local drafts are still being saved. Keep this profile open and retry.".into()
            );
        }
    }
    cx.set_global(Drafts::new(account, Some(profile)));
    load(cx);
    Ok(())
}

#[cfg(test)]
pub(super) fn configure_memory(account: Id, cx: &mut App) {
    if cx
        .try_global::<Drafts>()
        .is_some_and(|state| state.account == account && state.profile.is_none())
    {
        return;
    }
    cx.set_global(Drafts::new(account, None));
}

#[cfg(test)]
pub(super) fn ready(cx: &App) -> bool {
    cx.try_global::<Drafts>().is_some_and(|state| state.ready)
}

pub(super) fn account(cx: &App) -> Option<Id> {
    cx.try_global::<Drafts>().map(|state| state.account)
}

pub(super) fn ready_for(account: Id, cx: &App) -> bool {
    cx.try_global::<Drafts>().is_some_and(|state| state.account == account && state.ready)
}

pub(super) fn get(id: Id, cx: &App) -> Option<Arc<BrowserDraft>> {
    cx.try_global::<Drafts>()?.records.get(&id)?.draft.clone()
}

pub(super) fn get_for(account: Id, id: Id, cx: &App) -> Option<Arc<BrowserDraft>> {
    if ready_for(account, cx) { get(id, cx) } else { None }
}

pub(super) fn snapshots(cx: &App) -> Vec<BrowserDraftSummary> {
    cx.try_global::<Drafts>()
        .map(|state| {
            state
                .records
                .values()
                .filter_map(|record| record.draft.as_ref())
                .map(|draft| draft.summary())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn snapshots_for(account: Id, cx: &App) -> Vec<BrowserDraftSummary> {
    if ready_for(account, cx) { snapshots(cx) } else { Vec::new() }
}

pub(super) fn pending(cx: &App) -> bool {
    cx.try_global::<Drafts>().is_some_and(Drafts::busy)
}

pub(super) fn upsert(draft: BrowserDraft, cx: &mut App) -> Result<Id, String> {
    if !cx.has_global::<Drafts>() {
        return Err("Local draft recovery is not configured. Your editor is unchanged.".into());
    }
    let id = cx.global_mut::<Drafts>().upsert(draft)?;
    pump(cx);
    Ok(id)
}

pub(super) fn remove(id: Id, cx: &mut App) -> bool {
    let removed = cx.has_global::<Drafts>() && cx.global_mut::<Drafts>().remove(id);
    if removed {
        pump(cx);
    }
    removed
}

/// A receipt for an older canonical save cannot erase a newer local edit.
pub(super) fn remove_if(id: Id, expected: &BrowserDraft, cx: &mut App) -> bool {
    let mut expected = expected.clone();
    expected.canonicalize();
    let matches = get(id, cx).is_some_and(|current| same_payload(&current, &expected));
    matches && remove(id, cx)
}

fn same_payload(left: &BrowserDraft, right: &BrowserDraft) -> bool {
    left.id == right.id
        && left.volume == right.volume
        && left.entry == right.entry
        && left.body == right.body
}

/// Explicit retry; no timer or freshness work retries a failed local save.
pub(super) fn flush(cx: &mut App) {
    if !cx.has_global::<Drafts>() {
        return;
    }
    let state = cx.global_mut::<Drafts>();
    state.paused = false;
    if !state.ready {
        load(cx);
    } else {
        pump(cx);
    }
}

#[cfg(target_os = "macos")]
pub(super) fn defer_quit(cx: &mut App) -> bool {
    if !pending(cx) {
        return false;
    }
    cx.global_mut::<Drafts>().quit_after_save = true;
    cx.reply_to_quit(false).expect("AppKit owns this pending termination request");
    flush(cx);
    true
}

fn invoke(profile: &std::path::Path, account: Id, operation: Operation) -> Result<Outcome, String> {
    let reply = crate::engine::ipc::call_archive(
        profile,
        &crate::engine::Request::BrowserDrafts { account, operation },
    )
    .and_then(|reply| reply.into_owned())
    .map_err(|error| error.to_string())?;
    match reply {
        crate::engine::reply::Reply::BrowserDrafts { account: found, result }
            if found == account =>
        {
            Ok(result)
        }
        _ => Err("The local draft reply does not match this account.".into()),
    }
}

fn read_all(profile: &std::path::Path, account: Id) -> Result<BTreeMap<Id, Record>, String> {
    let Outcome::List(summaries) = invoke(profile, account, Operation::List)? else {
        return Err("The local draft list reply is invalid.".into());
    };
    if summaries.len() > MAX_DRAFTS {
        return Err("The local draft count exceeds its limit.".into());
    }
    let mut records = BTreeMap::new();
    let mut bytes = 0usize;
    for summary in summaries {
        let Outcome::Draft { id, draft: Some(draft) } =
            invoke(profile, account, Operation::Get { id: summary.id })?
        else {
            return Err("A local draft changed while it was loading. Retry loading drafts.".into());
        };
        if id != summary.id || draft.summary() != summary || records.contains_key(&id) {
            return Err("A local draft changed while it was loading. Retry loading drafts.".into());
        }
        draft.validate().map_err(|error| error.to_string())?;
        draft.encoded_len().map_err(|error| error.to_string())?;
        let retained = draft.retained_bytes().map_err(|error| error.to_string())?;
        bytes = bytes.checked_add(retained).ok_or("Local draft memory exceeds its limit.")?;
        if bytes > MAX_ACCOUNT_BYTES {
            return Err("Local drafts exceed the 8 MiB memory limit.".into());
        }
        records.insert(
            id,
            Record {
                target: Target::of(&draft),
                disk_revision: Some(draft.revision),
                draft: Some(Arc::from(draft)),
                generation: 0,
                bytes: retained,
            },
        );
    }
    Ok(records)
}

fn load(cx: &mut App) {
    let state = cx.global_mut::<Drafts>();
    if state.loading || state.ready {
        return;
    }
    let Some(profile) = state.profile.clone() else { return };
    let account = state.account;
    state.loading = true;
    let task = cx.background_executor().spawn(async move { read_all(&profile, account) });
    cx.spawn(async move |cx| {
        let result = task.await;
        let _ = cx.update(|cx| {
            let state = cx.global_mut::<Drafts>();
            state.loading = false;
            match result {
                Ok(records) => {
                    state.records = records;
                    state.ready = true;
                }
                Err(error) => report(&error, cx),
            }
        });
    })
    .detach();
}

fn execute(profile: &std::path::Path, account: Id, write: &Write) -> Result<Option<u64>, String> {
    let operation = if let Some(draft) = &write.draft {
        let mut draft = (**draft).clone();
        draft.revision = write.expected.map_or(1, |revision| revision + 1);
        Operation::Upsert { expected_revision: write.expected, draft: Box::new(draft) }
    } else {
        Operation::Remove {
            id: write.id,
            expected_revision: write.expected.expect("a disk removal names its revision"),
        }
    };
    match invoke(profile, account, operation)? {
        Outcome::Stored { id, revision }
            if write.draft.is_some()
                && id == write.id
                && revision == write.expected.map_or(1, |revision| revision + 1) =>
        {
            Ok(Some(revision))
        }
        Outcome::Removed { id, revision }
            if write.draft.is_none() && id == write.id && Some(revision) == write.expected =>
        {
            Ok(None)
        }
        _ => Err("The local draft acknowledgment does not match its save.".into()),
    }
}

/// Resolve a lost local acknowledgment before retrying its latest replacement.
fn reconcile(profile: &std::path::Path, account: Id, write: &Write) -> Result<Completion, String> {
    let Outcome::Draft { id, draft } = invoke(profile, account, Operation::Get { id: write.id })?
    else {
        return Err("The local draft readback is invalid.".into());
    };
    if id != write.id {
        return Err("The local draft readback names another draft.".into());
    }
    readback(write, draft)
}

fn readback(write: &Write, draft: Option<Box<BrowserDraft>>) -> Result<Completion, String> {
    if let Some(draft) = &draft {
        if draft.id != write.id {
            return Err("The local draft readback contains another draft.".into());
        }
        draft.validate().map_err(|error| error.to_string())?;
        draft.encoded_len().map_err(|error| error.to_string())?;
        if draft.retained_bytes().map_err(|error| error.to_string())? > MAX_ACCOUNT_BYTES {
            return Err("The local draft readback exceeds its memory limit.".into());
        }
    }
    let revision = draft.as_ref().map(|draft| draft.revision);
    let persisted = match (&write.draft, &draft) {
        (None, None) => true,
        (Some(submitted), Some(found)) => {
            found.revision == write.expected.map_or(1, |revision| revision + 1)
                && same_payload(submitted, found)
                && submitted.ancestors == found.ancestors
        }
        _ => false,
    };
    if persisted || revision == write.expected {
        Ok(Completion::Acknowledged { revision, persisted })
    } else {
        Ok(Completion::Conflict(draft))
    }
}

fn pump(cx: &mut App) {
    let state = cx.global_mut::<Drafts>();
    if !state.ready || state.paused || state.active.is_some() || state.profile.is_none() {
        return;
    }
    let recovering = state.failed.is_some();
    let write = if let Some(write) = state.failed.take() {
        Some(write)
    } else {
        match state.next_write() {
            Ok(write) => write,
            Err(error) => {
                state.paused = true;
                state.quit_after_save = false;
                report(&error, cx);
                return;
            }
        }
    };
    let Some(write) = write else {
        if std::mem::take(&mut state.quit_after_save) {
            cx.quit();
        }
        return;
    };
    state.active = Some(write.clone());
    let profile = state.profile.clone().expect("durable draft worker has a profile");
    let account = state.account;
    let task = cx.background_executor().spawn(async move {
        if recovering {
            reconcile(&profile, account, &write)
        } else {
            execute(&profile, account, &write)
                .map(|revision| Completion::Acknowledged { revision, persisted: true })
        }
    });
    cx.spawn(async move |cx| {
        let result = task.await;
        let _ = cx.update(|cx| complete(result, recovering, cx));
    })
    .detach();
}

fn complete(result: Result<Completion, String>, recovering: bool, cx: &mut App) {
    let state = cx.global_mut::<Drafts>();
    let write = state.active.take().expect("one local draft operation is active");
    match result.and_then(|completion| state.complete(&write, completion)) {
        Ok(true) => crate::ui::desktop::alert::show(
            crate::ui::desktop::alert::Kind::DraftRetained,
            "Your local edit was discarded. Another local copy of this draft remains available.",
            cx,
        ),
        Ok(false) => {}
        Err(error) => {
            let discarded =
                state.records.get(&write.id).is_some_and(|record| record.draft.is_none());
            let reconcile_discard = !recovering && write.draft.is_some() && discarded;
            state.failed = Some(write);
            // Discard may have happened during this save. Read it back once before
            // retiring it; a failed readback still pauses without a retry loop.
            if !reconcile_discard {
                state.paused = true;
                state.quit_after_save = false;
                report(&error, cx);
                return;
            }
        }
    }
    pump(cx);
}

fn report(error: &str, cx: &mut App) {
    crate::ui::desktop::alert::show(
        crate::ui::desktop::alert::Kind::LocalDraft,
        &format!(
            "Playbit couldn’t save or restore local drafts. Current edits are retained. Retry saving drafts or quitting. {error}"
        ),
        cx,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Tests assert valid bounded local draft fixtures.")]
    use super::*;
    use crate::store::{
        EntryId,
        browser_drafts::{BrowserDraftBody, DraftEncoding, KvDraft},
    };

    fn draft(byte: u8, text: &str) -> BrowserDraft {
        BrowserDraft {
            id: Id::from_bytes([byte; 16]),
            revision: 0,
            volume: Id::from_bytes([1; 16]),
            entry: Some(EntryId::from_bytes([1; 15]).unwrap()),
            ancestors: vec![],
            body: BrowserDraftBody::Kv(KvDraft {
                creating: false,
                original_key: Some(b"key".to_vec()),
                key_text: "key".into(),
                value_text: text.into(),
                key_encoding: DraftEncoding::Text,
                value_encoding: DraftEncoding::Text,
                base: None,
                changed_during_edit: false,
                uncertain: false,
                read_unavailable: false,
                complete_put: false,
            }),
        }
    }

    fn durable() -> Drafts {
        let mut state = Drafts::new(Id::from_bytes([1; 16]), Some("unused-test-profile".into()));
        state.ready = true;
        state
    }

    fn loaded() -> (Drafts, BrowserDraft) {
        let mut state = durable();
        let first = draft(1, "first");
        state.upsert(first.clone()).unwrap();
        let write = state.next_write().unwrap().unwrap();
        state.acknowledge(&write, Some(1), true);
        let first = (**state.records[&first.id].draft.as_ref().unwrap()).clone();
        (state, first)
    }

    #[gpui::test]
    fn account_scoped_reads_expose_only_the_ready_owner(cx: &mut gpui::TestAppContext) {
        // Keep another account's loaded cache private, including during a refused switch.
        cx.update(|cx| {
            let owner = Id::from_bytes([1; 16]);
            let other = Id::from_bytes([2; 16]);
            let (mut state, first) = loaded();
            state.upsert(draft(1, "unsaved edit")).unwrap();
            cx.set_global(state);
            assert!(configure(other, "other-test-profile".into(), cx).is_err());
            assert_eq!(account(cx), Some(owner));
            assert!(ready_for(owner, cx));
            assert!(get_for(owner, first.id, cx).is_some());
            assert_eq!(snapshots_for(owner, cx).len(), 1);
            assert!(!ready_for(other, cx));
            assert!(get_for(other, first.id, cx).is_none());
            assert!(snapshots_for(other, cx).is_empty());
            cx.global_mut::<Drafts>().ready = false;
            assert!(!ready_for(owner, cx));
            assert!(get_for(owner, first.id, cx).is_none());
            assert!(snapshots_for(owner, cx).is_empty());
        });
    }

    #[gpui::test]
    fn discard_retires_a_failed_conflicting_save_without_deleting_the_peer(
        cx: &mut gpui::TestAppContext,
    ) {
        // Model rev1 -> peer rev2 -> failed local save, then explicit Discard.
        cx.update(|cx| {
            let (mut state, first) = loaded();
            let mut raw = draft(1, "00 zz\n0");
            if let BrowserDraftBody::Kv(value) = &mut raw.body {
                value.value_encoding = DraftEncoding::Hex;
            }
            state.upsert(raw.clone()).unwrap();
            let write = state.next_write().unwrap().unwrap();
            let mut peer = draft(1, "other local copy");
            peer.revision = 2;
            assert!(
                state
                    .complete(&write, readback(&write, Some(Box::new(peer.clone()))).unwrap())
                    .is_err()
            );
            assert_eq!(state.records[&first.id].draft.as_ref().unwrap().body, raw.body);
            assert_eq!(state.records[&first.id].disk_revision, Some(1));
            state.failed = Some(write.clone());
            state.paused = true;
            assert!(state.remove(first.id));
            state.active = state.failed.take();
            cx.set_global(state);
            complete(readback(&write, Some(Box::new(peer.clone()))), true, cx);
            assert_eq!(*get(first.id, cx).unwrap(), peer);
            assert!(!pending(cx));
            assert!(cx.global::<Drafts>().failed.is_none());
            assert!(cx.global_mut::<Drafts>().next_write().unwrap().is_none());
            #[cfg(target_os = "macos")]
            assert!(!defer_quit(cx));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let alerts = crate::ui::desktop::alert::take_presented(cx);
            assert_eq!(alerts.len(), 1);
            assert_eq!(alerts[0].0, crate::ui::desktop::alert::Kind::DraftRetained);
            assert!(alerts[0].1.contains("Another local copy"));
        });
    }

    #[gpui::test]
    fn a_conflict_readback_cannot_replace_new_input_after_discard(cx: &mut gpui::TestAppContext) {
        // New input during the discard's readback revokes adoption of the peer.
        cx.update(|cx| {
            let (mut state, first) = loaded();
            state.upsert(draft(1, "failed edit")).unwrap();
            let write = state.next_write().unwrap().unwrap();
            state.active = Some(write.clone());
            assert!(state.remove(first.id));
            let latest = draft(1, "new raw edit");
            state.upsert(latest.clone()).unwrap();
            let mut peer = draft(1, "other local copy");
            peer.revision = 2;
            cx.set_global(state);
            complete(readback(&write, Some(Box::new(peer))), true, cx);
            assert_eq!(get(first.id, cx).unwrap().body, latest.body);
            assert_eq!(cx.global::<Drafts>().records[&first.id].disk_revision, Some(1));
            assert!(pending(cx));
            assert!(cx.global::<Drafts>().paused);
            assert!(cx.global::<Drafts>().failed.is_some());
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(crate::ui::desktop::alert::take_presented(cx).len(), 1));
    }

    #[gpui::test]
    fn discard_during_a_save_reconciles_once_and_pauses_if_readback_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        // A late save failure honors Discard without creating an automatic retry loop.
        let profile = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            let (mut state, first) = loaded();
            state.profile = Some(profile.path().to_owned());
            state.upsert(draft(1, "in flight")).unwrap();
            let write = state.next_write().unwrap().unwrap();
            state.active = Some(write);
            assert!(state.remove(first.id));
            cx.set_global(state);
            complete(Err("The local save failed.".into()), false, cx);
            assert!(cx.global::<Drafts>().active.is_some());
            assert!(crate::ui::desktop::alert::take_presented(cx).is_empty());
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(pending(cx));
            assert!(cx.global::<Drafts>().active.is_none());
            assert!(cx.global::<Drafts>().failed.is_some());
            assert!(cx.global::<Drafts>().paused);
            assert_eq!(crate::ui::desktop::alert::take_presented(cx).len(), 1);
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(crate::ui::desktop::alert::take_presented(cx).is_empty()));
    }

    #[test]
    fn readback_resolves_own_lost_receipts_and_a_discarded_peer_removal() {
        // Only an exact committed payload advances CAS; peer removal needs Discard.
        let (mut state, first) = loaded();
        state.upsert(draft(1, "submitted")).unwrap();
        let write = state.next_write().unwrap().unwrap();
        let mut unrelated = first.clone();
        unrelated.id = Id::from_bytes([2; 16]);
        assert!(readback(&write, Some(Box::new(unrelated))).is_err());
        state.upsert(draft(1, "latest")).unwrap();
        state.complete(&write, readback(&write, Some(Box::new(first.clone()))).unwrap()).unwrap();
        assert_eq!(state.next_write().unwrap().unwrap().expected, Some(1));
        let mut committed = (**write.draft.as_ref().unwrap()).clone();
        committed.revision = 2;
        state.complete(&write, readback(&write, Some(Box::new(committed))).unwrap()).unwrap();
        assert_eq!(state.records[&first.id].draft.as_ref().unwrap().body, draft(1, "latest").body);
        assert_eq!(state.next_write().unwrap().unwrap().expected, Some(2));

        let (mut state, first) = loaded();
        state.upsert(draft(1, "failed edit")).unwrap();
        let write = state.next_write().unwrap().unwrap();
        assert!(state.complete(&write, readback(&write, None).unwrap()).is_err());
        assert!(state.remove(first.id));
        assert!(!state.complete(&write, readback(&write, None).unwrap()).unwrap());
        assert!(state.records.is_empty());
        assert!(!state.busy());
    }

    #[test]
    fn adopting_a_peer_obeys_the_existing_account_memory_limit() {
        // A peer can be larger than the discarded copy; preserve the admission cap.
        let (mut state, first) = loaded();
        assert!(state.remove(first.id));
        let deletion = state.next_write().unwrap().unwrap();
        let mut filler = draft(2, "");
        filler.revision = 1;
        let fixed = filler.retained_bytes().unwrap();
        if let BrowserDraftBody::Kv(value) = &mut filler.body {
            value.value_text = String::with_capacity(MAX_ACCOUNT_BYTES - fixed);
        }
        state.upsert(filler).unwrap();
        assert_eq!(state.retained_bytes(), MAX_ACCOUNT_BYTES);
        let mut peer = draft(1, "peer revision");
        peer.revision = 2;
        assert!(
            state
                .complete(&deletion, readback(&deletion, Some(Box::new(peer.clone()))).unwrap())
                .is_err()
        );
        assert!(state.records[&first.id].draft.is_none());
        assert!(state.remove(Id::from_bytes([2; 16])));
        assert!(
            state.complete(&deletion, readback(&deletion, Some(Box::new(peer))).unwrap()).unwrap()
        );
        assert!(!state.busy());
    }

    #[test]
    fn cache_rejects_target_changes_before_replacing_live_or_discarded_payloads() {
        // Match disk target immutability while allowing ordinary KV draft edits.
        let (mut state, first) = loaded();
        let mut changed = first.clone();
        changed.volume = Id::from_bytes([2; 16]);
        assert!(state.upsert(changed).is_err());
        let mut changed = first.clone();
        changed.entry = Some(EntryId::from_bytes([2; 15]).unwrap());
        assert!(state.upsert(changed).is_err());
        let mut record = first.clone();
        record.body = BrowserDraftBody::Record(crate::store::browser_drafts::RecordDraft {
            type_id: Id::from_bytes([2; 16]),
            object: Id::from_bytes([3; 16]),
            fields: vec![],
            values: vec![],
            active_values: None,
            changed_during_edit: false,
        });
        assert!(state.upsert(record.clone()).is_err());
        assert_eq!(state.records[&first.id].draft.as_deref(), Some(&first));
        assert!(state.pending.is_empty());
        assert!(state.remove(first.id));
        assert!(state.upsert(record.clone()).is_err());
        let deletion = state.next_write().unwrap().unwrap();
        state.acknowledge(&deletion, None, true);
        state.upsert(record.clone()).unwrap();
        for type_change in [true, false] {
            let mut changed = record.clone();
            if let BrowserDraftBody::Record(value) = &mut changed.body {
                if type_change {
                    value.type_id = Id::from_bytes([4; 16]);
                } else {
                    value.object = Id::from_bytes([4; 16]);
                }
            }
            assert!(state.upsert(changed).is_err());
        }
        let (mut state, mut editable) = loaded();
        if let BrowserDraftBody::Kv(value) = &mut editable.body {
            value.original_key = None;
            value.creating = true;
            value.key_text = "replacement key".into();
        }
        assert!(state.upsert(editable).is_ok());
    }

    #[test]
    fn newer_edits_coalesce_without_advancing_the_disk_revision_before_acknowledgment() {
        let mut state = durable();
        let first = draft(1, "first");
        state.upsert(first.clone()).unwrap();
        let write = state.next_write().unwrap().unwrap();
        assert_eq!(write.expected, None);
        state.active = Some(write.clone());
        state.upsert(draft(1, "second")).unwrap();
        state.upsert(draft(1, "third")).unwrap();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.records[&first.id].disk_revision, None);
        state.active = None;
        state.acknowledge(&write, Some(1), true);
        assert_eq!(state.records[&first.id].draft.as_ref().unwrap().body, draft(1, "third").body);
        let next = state.next_write().unwrap().unwrap();
        assert_eq!(next.expected, Some(1));
        assert_eq!(next.draft.as_ref().unwrap().body, draft(1, "third").body);
    }

    #[test]
    fn an_acknowledged_delete_cannot_remove_a_newer_local_replacement() {
        let mut state = durable();
        let first = draft(1, "first");
        state.upsert(first.clone()).unwrap();
        let stored = state.next_write().unwrap().unwrap();
        state.acknowledge(&stored, Some(1), true);
        assert!(state.remove(first.id));
        let deletion = state.next_write().unwrap().unwrap();
        assert!(deletion.draft.is_none());
        state.active = Some(deletion.clone());
        state.upsert(draft(1, "replacement")).unwrap();
        state.active = None;
        state.acknowledge(&deletion, None, true);
        assert_eq!(
            state.records[&first.id].draft.as_ref().unwrap().body,
            draft(1, "replacement").body
        );
        let replacement = state.next_write().unwrap().unwrap();
        assert_eq!(replacement.expected, None);
    }

    #[gpui::test]
    fn persistence_failure_keeps_newer_edits_and_pauses_background_retries(
        cx: &mut gpui::TestAppContext,
    ) {
        let profile = tempfile::tempdir().unwrap();
        let first = draft(1, "first");
        cx.update(|cx| {
            let mut state = Drafts::new(Id::from_bytes([1; 16]), Some(profile.path().to_owned()));
            state.ready = true;
            cx.set_global(state);
            upsert(first.clone(), cx).unwrap();
            upsert(draft(1, "newer"), cx).unwrap();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(get(first.id, cx).unwrap().body, draft(1, "newer").body);
            assert!(pending(cx));
            assert!(cx.global::<Drafts>().paused);
            assert_eq!(cx.global::<Drafts>().records[&first.id].disk_revision, None);
            assert_eq!(crate::ui::desktop::alert::take_presented(cx).len(), 1);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(crate::ui::desktop::alert::take_presented(cx).is_empty());
            flush(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(get(first.id, cx).unwrap().body, draft(1, "newer").body);
            assert_eq!(crate::ui::desktop::alert::take_presented(cx).len(), 1);
        });
    }

    #[gpui::test]
    fn failed_initial_load_admits_no_editor_replacement(cx: &mut gpui::TestAppContext) {
        let profile = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            configure(Id::from_bytes([1; 16]), profile.path().to_owned(), cx).unwrap();
            assert!(!ready(cx));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(!ready(cx));
            assert!(upsert(draft(1, "retained by its editor"), cx).is_err());
            assert!(snapshots(cx).is_empty());
            assert_eq!(crate::ui::desktop::alert::take_presented(cx).len(), 1);
        });
    }

    #[test]
    fn cache_admission_preserves_existing_drafts_at_count_and_capacity_limits() {
        let mut state = durable();
        for byte in 1..=MAX_DRAFTS as u8 {
            state.upsert(draft(byte, "retained")).unwrap();
        }
        assert!(state.upsert(draft(33, "overflow")).is_err());
        assert_eq!(state.records.len(), MAX_DRAFTS);
        let mut state = durable();
        let mut oversized = draft(1, "a");
        if let BrowserDraftBody::Kv(value) = &mut oversized.body {
            let mut text = String::with_capacity(MAX_ACCOUNT_BYTES);
            text.push('a');
            value.value_text = text;
        }
        assert!(state.upsert(oversized).is_err(), "charge capacity, not the one-byte text length");
        assert!(state.records.is_empty());
    }

    #[gpui::test]
    fn canonical_receipts_remove_only_the_matching_current_payload(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            configure_memory(Id::from_bytes([1; 16]), cx);
            let older = draft(1, "older");
            upsert(older.clone(), cx).unwrap();
            upsert(draft(1, "newer"), cx).unwrap();
            assert!(!remove_if(older.id, &older, cx));
            let mut current = (*get(older.id, cx).unwrap()).clone();
            current.revision = 99;
            current.ancestors = vec![EntryId::from_bytes([2; 15]).unwrap()];
            assert!(
                remove_if(current.id, &current, cx),
                "disk revision and path hints are not edits"
            );
            assert!(snapshots(cx).is_empty());
            assert!(!pending(cx));
        });
    }
}
