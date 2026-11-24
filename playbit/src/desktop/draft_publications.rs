//! Canonical write receipts belong to their draft, even after its view is gone.
use super::*;
use crate::store::browser_drafts::{BrowserDraft, BrowserDraftBody};
use crate::store::object_contract::Value;

#[derive(Default)]
struct Publications {
    requests: BTreeMap<u64, Publication>,
    values: BTreeMap<(u64, u64), Publication>,
}
impl gpui::Global for Publications {}
struct Publication {
    account: crate::store::Id,
    draft: Arc<BrowserDraft>,
    effect: Effect,
    conflicted: bool,
}
enum Effect {
    Permissions,
    Kv { key: Vec<u8>, value: Vec<u8> },
    Record { fields: BTreeMap<crate::store::Id, Option<Vec<Value>>> },
}
fn install(cx: &mut App) {
    if !cx.has_global::<Publications>() {
        cx.set_global(Publications::default());
    }
}
/// A retired worker cannot complete receipts, and its admission IDs may be reused.
pub(super) fn retire(cx: &mut App) {
    if cx.has_global::<Publications>() {
        cx.set_global(Publications::default());
    }
}

pub(super) fn track_permissions(admission: u64, draft: Arc<BrowserDraft>, cx: &mut App) {
    install(cx);
    let publication = Publication {
        account: drafts::account(cx).expect("editor write owns an account"),
        draft,
        conflicted: false,
        effect: Effect::Permissions,
    };
    cx.global_mut::<Publications>().requests.insert(admission, publication);
}
pub(super) fn track_record(
    admission: u64,
    draft: Arc<BrowserDraft>,
    fields: BTreeMap<crate::store::Id, Option<Vec<Value>>>,
    cx: &mut App,
) {
    install(cx);
    let publication = Publication {
        account: drafts::account(cx).expect("editor write owns an account"),
        draft,
        conflicted: false,
        effect: Effect::Record { fields },
    };
    cx.global_mut::<Publications>().requests.insert(admission, publication);
}
pub(super) fn track_kv(
    owner: u64,
    sequence: u64,
    draft: Arc<BrowserDraft>,
    key: Vec<u8>,
    value: Vec<u8>,
    cx: &mut App,
) {
    install(cx);
    let publication = Publication {
        account: drafts::account(cx).expect("editor write owns an account"),
        draft,
        conflicted: false,
        effect: Effect::Kv { key, value },
    };
    cx.global_mut::<Publications>().values.insert((owner, sequence), publication);
}
/// Admission is checked before enqueueing a canonical write; profile retirement
/// drops receipts from the old worker before admission identities can be reused.
pub(super) fn available(cx: &App) -> bool {
    cx.try_global::<Publications>().is_none_or(|state| {
        state.requests.len() + state.values.len() < crate::store::browser_drafts::MAX_DRAFTS
    })
}

pub(super) fn any_pending(cx: &App) -> bool {
    cx.try_global::<Publications>()
        .is_some_and(|state| !state.requests.is_empty() || !state.values.is_empty())
}

pub(super) fn pending(id: crate::store::Id, cx: &App) -> bool {
    cx.has_global::<Publications>() && {
        let writes = cx.global::<Publications>();
        writes
            .requests
            .values()
            .chain(writes.values.values())
            .any(|write| write.draft.id == id && Some(write.account) == drafts::account(cx))
    }
}

pub(super) fn receive(response: &WorkerResponse, cx: &mut App) {
    if !cx.has_global::<Publications>() {
        return;
    }
    if let Some(reply) = &response.data_reply {
        receive_kv(reply, cx);
    }
    let Some(admission) = response.credit.as_ref().map(|credit| credit.admission()) else {
        return;
    };
    let Some(mut publication) = cx.global_mut::<Publications>().requests.remove(&admission) else {
        return;
    };
    let acknowledged = response.result.is_ok()
        && match (&publication.effect, &response.browser_reply) {
            (Effect::Permissions, Some(Reply::EntryPermissions { volume, entry })) => {
                publication.draft.volume == *volume && publication.draft.entry == Some(*entry)
            }
            (Effect::Permissions, Some(Reply::Permissions { permissions_updated: true })) => {
                publication.draft.entry.is_none()
            }
            (Effect::Record { .. }, Some(Reply::Object(result))) => {
                publication.draft.entry == Some(result.database.entry)
                    && publication.draft.volume == result.database.volume
                    && matches!(&publication.draft.body, BrowserDraftBody::Record(draft) if draft.object == result.object.object_id)
            }
            _ => false,
        };
    if acknowledged {
        if let (Effect::Record { fields }, Some(Reply::Object(result))) =
            (&mut publication.effect, &response.browser_reply)
        {
            let count = fields.len();
            fields.retain(|field, values| {
                result.object.fields.get(field).and_then(|cell| cell.values.as_ref())
                    == values.as_ref()
            });
            publication.conflicted = fields.len() != count;
        }
        saved(publication, cx);
    }
}

pub(super) fn receive_kv(reply: &worker_data::DataReply, cx: &mut App) {
    if !cx.has_global::<Publications>() {
        return;
    }
    let matches = cx
        .global::<Publications>()
        .values
        .get(&(reply.owner, reply.sequence))
        .is_some_and(|publication| {
            publication.draft.volume == reply.volume && publication.draft.entry == Some(reply.entry)
        });
    if !matches {
        return;
    }
    let publication = cx.global_mut::<Publications>().values.remove(&(reply.owner, reply.sequence));
    if let Some(publication) = publication {
        match &reply.result {
            Ok(crate::runtime::host::DataResult::KvWritten) => saved(publication, cx),
            Err(error) => {
                let uncertain = matches!(
                    error.status,
                    None | Some(
                        playbit_api::abi::syscall::Error::Internal
                            | playbit_api::abi::syscall::Error::StaleHandle
                    )
                );
                failed_value(publication, uncertain, cx);
            }
            _ => failed_value(publication, true, cx),
        }
    }
}

pub(super) fn acknowledge_permissions(
    admission: Option<u64>,
    volume: crate::store::Id,
    entry: Option<crate::store::EntryId>,
    cx: &mut App,
) -> Option<(crate::store::object_contract::EntryAcl, Option<crate::store::object_contract::Acl>)> {
    let admission = admission?;
    if !cx.has_global::<Publications>() {
        return None;
    }
    let matches = cx.global::<Publications>().requests.get(&admission).is_some_and(|publication| {
        publication.draft.volume == volume
            && publication.draft.entry == entry
            && matches!(publication.effect, Effect::Permissions)
    });
    if !matches {
        return None;
    }
    let publication = cx.global_mut::<Publications>().requests.remove(&admission)?;
    let BrowserDraftBody::Permissions(state) = &publication.draft.body else {
        return None;
    };
    let baseline = (state.active.clone(), state.retained.clone());
    saved(publication, cx);
    Some(baseline)
}

fn saved(publication: Publication, cx: &mut App) {
    if drafts::account(cx) != Some(publication.account) {
        return;
    }
    let id = publication.draft.id;
    let applied_whole_draft = match &publication.draft.body {
        BrowserDraftBody::Permissions(state) => !state.adding_grant,
        BrowserDraftBody::Kv(_) => true,
        BrowserDraftBody::Record(_) => false,
    };
    if applied_whole_draft && drafts::remove_if(id, &publication.draft, cx) {
        return;
    }
    let Some(current) = drafts::get(id, cx) else {
        return;
    };
    let mut current = (*current).clone();
    match (&mut current.body, &publication.effect, &publication.draft.body) {
        (
            BrowserDraftBody::Permissions(current),
            Effect::Permissions,
            BrowserDraftBody::Permissions(submitted),
        ) => {
            current.base_active = submitted.active.clone();
            current.base_retained = submitted.retained.clone();
            if !current.adding_grant
                && current.active == current.base_active
                && current.retained == current.base_retained
            {
                drafts::remove(id, cx);
                return;
            }
        }
        (BrowserDraftBody::Kv(current), Effect::Kv { key, value }, _) => {
            current.original_key = Some(key.clone());
            current.creating = false;
            current.uncertain = false;
            current.complete_put = true;
            current.changed_during_edit = false;
            if let Some(base) = &mut current.base {
                base.offset = 0;
                base.total_bytes = value.len() as u64;
                base.bytes.clone_from(value);
            }
        }
        (
            BrowserDraftBody::Record(current_record),
            Effect::Record { fields },
            BrowserDraftBody::Record(submitted),
        ) => {
            current_record.changed_during_edit |= publication.conflicted;
            if browser::settle_record_publication(current_record, submitted, fields) {
                drafts::remove(id, cx);
                return;
            }
        }
        _ => return,
    }
    current.canonicalize();
    if let Err(error) = drafts::upsert(current, cx) {
        crate::ui::desktop::alert::show(crate::ui::desktop::alert::Kind::LocalDraft, &error, cx);
    }
}

fn failed_value(publication: Publication, uncertain: bool, cx: &mut App) {
    if drafts::account(cx) != Some(publication.account) {
        return;
    }
    let Some(current) = drafts::get(publication.draft.id, cx) else {
        return;
    };
    let mut current = (*current).clone();
    if let BrowserDraftBody::Kv(draft) = &mut current.body {
        draft.uncertain = uncertain;
        if let Err(error) = drafts::upsert(current, cx) {
            crate::ui::desktop::alert::show(
                crate::ui::desktop::alert::Kind::LocalDraft,
                &error,
                cx,
            );
        }
    }
}
