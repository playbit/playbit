//! Bounded, source-root-fenced enumeration for initialization and gap recovery.
use super::*;
use crate::store::{
    catalog::PLAYBIT_REPLICA_PRINCIPAL,
    volume_index::{DatabaseProtocol, IndexSnapshot},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    volume: Id,
    after: Option<EntryId>,
    expected_root: Option<[u8; 32]>,
    selection: Option<SelectionCatalog>,
}
#[derive(Serialize)]
struct Page {
    databases: Vec<DatabaseId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blind: Vec<EntryId>,
    next: Option<EntryId>,
    root: [u8; 32],
}
fn policy(host: &Host, request: &Request, volume: &Volume) -> Result<MetadataPolicy> {
    if let Some(selection) = &request.selection {
        let mut selected = None;
        host.visit_candidate(&selection.session, &selection.grants, |policy| {
            if policy.volume().id == volume.id {
                selected = Some(policy.clone());
            }
            Ok(())
        })?;
        selected.ok_or(Error::AccessDenied)
    } else {
        host.authorize(volume, None, Permission::Read)
    }
}
fn upload_admitted(host: &Host, volume: &Volume) -> Result<bool> {
    // Personal read-only consent caps an entry's Write grant. Other failures
    // must stop preparation rather than silently hiding a broken catalog.
    match host.admit_volume(volume, Permission::Write) {
        Ok(_) => Ok(true),
        Err(Error::AccessDenied) => Ok(false),
        Err(error) => Err(error),
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Read,
    Upload,
    WorldAppends,
}

fn uploadable(
    check: impl Fn(Permission) -> crate::store::Result<()>,
    allow_append: bool,
) -> Result<bool> {
    let permission = match check(Permission::Write) {
        Err(crate::store::Error::AccessDenied(_)) if allow_append => check(Permission::Append),
        other => other,
    };
    match permission {
        Ok(()) => Ok(true),
        Err(crate::store::Error::AccessDenied(_)) => Ok(false),
        Err(error) => Err(store_error(error)),
    }
}

fn page(host: &Host, request: Request, purpose: Purpose) -> Result<Page> {
    let volume = host.replica_volume(request.volume)?;
    let projected = host.is_projected_world(&volume);
    if purpose == Purpose::WorldAppends && !projected {
        return Err(Error::AccessDenied);
    }
    let upload = purpose != Purpose::Read;
    let policy = policy(host, &request, &volume)?;
    crate::store::DatabasePolicy::reserved(policy.clone(), DatabaseId::Index)
        .map_err(store_error)?
        .authorize_reader(crate::store::ReplicaReader::Service)
        .map_err(store_error)?;
    let read = volume.index.begin_read().map_err(map_error)?;
    let (_, root) = read.root_hash().map_err(map_error)?.ok_or(Error::Unavailable)?;
    if request.after.is_some() && request.expected_root.is_none() {
        return Err(Error::InvalidArgument);
    }
    if request.expected_root.is_some_and(|expected| expected != root) {
        return Err(map_error("database catalog changed during gap recovery"));
    }
    if upload && !upload_admitted(host, &volume)? {
        return Ok(Page { databases: Vec::new(), blind: Vec::new(), next: None, root });
    }
    let snapshot = IndexSnapshot::new(read);
    let page = snapshot.headers(request.after, 64).map_err(store_error)?;
    let mut databases = Vec::with_capacity(66);
    let mut blind = Vec::new();
    if request.after.is_none() && !upload {
        databases.extend([DatabaseId::Index, DatabaseId::Revocations]);
    }
    for entry in page.entries {
        if entry.deleted
            || entry.protocol != Some(DatabaseProtocol::Kvdb)
            || projected
                && upload
                && snapshot.entry_policy(entry.id).map_err(store_error)?.kind
                    != crate::store::EntryType::KvDb
        {
            continue;
        }
        let authorize = |principal, permission| {
            snapshot.authorize(
                policy.volume(),
                policy.default_permissions(),
                principal,
                entry.id,
                permission,
            )
        };
        if authorize(PLAYBIT_REPLICA_PRINCIPAL.into(), Permission::Read).is_err() {
            continue;
        }
        let readable = authorize(host.actor(&volume), Permission::Read).is_ok();
        if !readable && !(projected && upload) {
            continue;
        }
        if upload && !snapshot.backing_ready(entry.id).map_err(store_error)? {
            continue;
        }
        let allow_append = !projected || purpose == Purpose::WorldAppends;
        if upload && !uploadable(|right| authorize(host.actor(&volume), right), allow_append)? {
            continue;
        }
        if projected && purpose == Purpose::Upload && !readable {
            blind.push(entry.id);
        }
        databases.push(DatabaseId::Entry(entry.id));
    }
    Ok(Page { databases, blind, next: page.next, root })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_catalog_page(length: usize) -> i32 {
    catalog_page(length, Purpose::Read)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_upload_catalog_page(length: usize) -> i32 {
    catalog_page(length, Purpose::Upload)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_world_append_catalog_page(length: usize) -> i32 {
    catalog_page(length, Purpose::WorldAppends)
}
fn catalog_page(length: usize, purpose: Purpose) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, LIMIT)?;
        let request: Request = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        if SYNC.with(|sync| sync.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        serde_json::to_vec(&page(&host, request, purpose)?).map_err(map_error)
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_validate_selection(length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, LIMIT)?;
        let selection: SelectionCatalog =
            serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        host.visit_candidate(&selection.session, &selection.grants, |_| Ok(()))
    })();
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
