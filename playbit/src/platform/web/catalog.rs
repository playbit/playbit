//! Bounded, source-root-fenced enumeration for initialization and gap recovery.
use super::*;
use crate::store::{
    catalog::PLAYBIT_REPLICA_PRINCIPAL,
    object_contract::Principal,
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
fn page(host: &Host, request: Request, protocol: DatabaseProtocol, upload: bool) -> Result<Page> {
    let volume = host.replica_volume(request.volume)?;
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
        return Ok(Page { databases: Vec::new(), next: None, root });
    }
    let snapshot = IndexSnapshot::new(read);
    let page = snapshot.headers(request.after, 64).map_err(store_error)?;
    let mut databases = Vec::with_capacity(66);
    if request.after.is_none() && !upload {
        databases.extend([DatabaseId::Index, DatabaseId::Revocations]);
    }
    for entry in page.entries {
        if entry.deleted || entry.protocol != Some(protocol) {
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
        if authorize(Principal::Human(host.account), Permission::Read).is_ok()
            && authorize(PLAYBIT_REPLICA_PRINCIPAL, Permission::Read).is_ok()
        {
            if upload
                && (!snapshot.backing_ready(entry.id).map_err(store_error)?
                    || snapshot.backing_quarantined(entry.id).map_err(store_error)?)
            {
                continue;
            }
            if upload {
                let writable = match authorize(Principal::Human(host.account), Permission::Write) {
                    Err(crate::store::Error::AccessDenied(_))
                        if protocol == DatabaseProtocol::Kvdb =>
                    {
                        authorize(Principal::Human(host.account), Permission::Append)
                    }
                    result => result,
                };
                match writable {
                    Ok(()) => {}
                    Err(crate::store::Error::AccessDenied(_)) => continue,
                    Err(error) => return Err(store_error(error)),
                }
            }
            databases.push(DatabaseId::Entry(entry.id));
        }
    }
    Ok(Page { databases, next: page.next, root })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_catalog_page(length: usize) -> i32 {
    catalog_page(length, DatabaseProtocol::Kvdb, false)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_upload_catalog_page(length: usize) -> i32 {
    catalog_page(length, DatabaseProtocol::Kvdb, true)
}
fn catalog_page(length: usize, protocol: DatabaseProtocol, upload: bool) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, LIMIT)?;
        let request: Request = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        if SYNC.with(|sync| sync.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        serde_json::to_vec(&page(&host, request, protocol, upload)?).map_err(map_error)
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
