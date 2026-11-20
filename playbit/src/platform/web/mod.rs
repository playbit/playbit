//! Web kvdb and structured-object host. JavaScript supplies synchronous OPFS
//! transactions; object field meaning and merges use the public store contract.
//! One worker calls exports synchronously. Admit input against both the per-call
//! budget and allocated buffer before slicing; assert internal output lengths.
//! Dispatch and sync are data-plane paths, with no automatic tree verification.
#![allow(
    unsafe_code,
    reason = "This module owns the wasm host imports and exported buffer interface."
)]
#![deny(clippy::unwrap_used)]
#![cfg(target_arch = "wasm32")]

mod account;
mod authority;
mod backing;
mod blob;
mod blob_cache;
mod bloblog;
mod catalog;
mod channels;
mod crypto;
mod databases;
mod files;
mod initial_blobs;
mod key_value;
mod live;
mod objects;
mod operations;
mod package;
mod publication;
mod storage;
mod transfers;
mod work;
use crate::store::{
    DatabaseId, EntryId, Id,
    object_contract::{Schema, Value},
};
use crate::store::{
    MetadataPolicy, PersonalVolumeGrant, PersonalVolumeSelection, Session, Volume as VolumeRecord,
    object_contract::{Acl, Permission},
};
use kvdb::{
    CreateOptions, Db, Lww, SyncSender,
    sync::{PreparedInspection, PreparedSync, SyncStatus},
};
use playbit_api::abi::syscall::{Error, Opcode};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::Mutex,
};
use storage::{OpfsStaging, OpfsStorage};
use transfers::{Sync, Transfer};

type Database = Db<OpfsStorage>;
type Result<T> = std::result::Result<T, Error>;
const LIMIT: usize = 1024 * 1024;
const INPUT_LIMIT: usize = 64 * 1024 * 1024;
const SYNC_FRAGMENT: usize = 256 * 1024;
const _: () = {
    assert!(SYNC_FRAGMENT == kvdb::sync::DEFAULT_MSG_CAP);
    assert!(SYNC_FRAGMENT <= LIMIT);
    assert!(LIMIT <= INPUT_LIMIT);
    assert!(INPUT_LIMIT <= i32::MAX as usize);
};
static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
thread_local! {
    // One owned host per worker. The transport retains its own database pin.
    static HOST: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
    static SYNC: RefCell<Option<Sync>> = const { RefCell::new(None) };
}

#[link(wasm_import_module = "opfs")]
unsafe extern "C" {
    fn random(pointer: *mut u8, length: usize);
    fn now() -> f64;
    fn engine_panic(pointer: *const u8, length: usize);
}

#[derive(Deserialize)]
struct Bootstrap {
    installation: Id,
    account: Id,
    root: VolumeRecord,

    session: Session,
    root_key: [u8; 32],
    replica_ids: [u64; 2],
    local: Option<crate::store::initialization::RootInitialization>,
}
struct Volume {
    id: Id,
    record: VolumeRecord,
    key: [u8; 32],
    index: Rc<Database>,
    revocations: Rc<Database>,
}
struct Host {
    installation: Id,
    account: Id,
    session: RefCell<Session>,
    previous_session: RefCell<Option<Session>>,
    volumes: RefCell<[Rc<Volume>; 1]>,
    personal: RefCell<Vec<Rc<Volume>>>,
    volume_objects: RefCell<BTreeMap<Id, Rc<Volume>>>,
    selections: RefCell<BTreeMap<String, Vec<PersonalVolumeSelection>>>,
    consent_revision: Cell<u32>,
    changes: RefCell<live::Changes>,
    named: RefCell<Vec<databases::Named>>,
    named_failures: RefCell<Vec<(databases::Address, Error)>>,
    files: RefCell<files::Files>,
    blob_cache: Rc<RefCell<blob_cache::Cache>>,
    initializer: Option<crate::store::initialization::RootInitialization>,
}
#[derive(Deserialize)]
struct PersonalVolume {
    volume: VolumeRecord,
    key: [u8; 32],
    replica_ids: [u64; 2],
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonalSelection {
    session: String,
    grants: Vec<PersonalVolumeSelection>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionCatalog {
    session: Session,
    grants: Vec<PersonalVolumeGrant>,
}

fn map_error(error: impl std::fmt::Display) -> Error {
    let mut message = error.to_string();
    if message.len() > LIMIT {
        message = "web host error exceeds diagnostic byte limit".into();
    }
    *LAST_ERROR.lock().expect("mutex is not poisoned") = message.clone();
    if message.contains("access denied") { Error::AccessDenied } else { Error::Internal }
}
fn store_error(error: crate::store::Error) -> Error {
    let status = crate::runtime::store_bridge::error_ref(&error);
    map_error(error);
    status
}
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

impl Host {
    fn open(bootstrap: Bootstrap) -> Result<Self> {
        bootstrap.session.validate_shared_volume().map_err(store_error)?;
        if bootstrap.installation == Id::from_bytes([0; 16])
            || bootstrap.session.replica_volume() != bootstrap.root.id
        {
            return Err(Error::InvalidArgument);
        }
        let root = Volume::open(
            bootstrap.root.clone(),
            bootstrap.root_key,
            1,
            &bootstrap.replica_ids[..2],
        )?;
        if let Some(initializer) = &bootstrap.local {
            initializer.fingerprint().map_err(map_error)?;
            if !root
                .index
                .begin_read()
                .map_err(map_error)?
                .has(&crate::store::catalog::VOLUME_KEY)
                .map_err(map_error)?
            {
                MetadataPolicy::new(root.record.clone(), initializer.permissions.clone())
                    .map_err(map_error)?
                    .write(&root.index, unsafe { now() } as u64)
                    .map_err(map_error)?;
            }
        }
        let blob_cache = Rc::default();
        let root = Rc::new(root);
        let volume_objects = BTreeMap::from([(root.id, root.clone())]);
        Ok(Self {
            installation: bootstrap.installation,
            account: bootstrap.account,
            session: RefCell::new(bootstrap.session),
            previous_session: RefCell::new(None),
            volumes: RefCell::new([root]),
            volume_objects: RefCell::new(volume_objects),
            personal: RefCell::new(Vec::new()),
            selections: RefCell::new(BTreeMap::new()),
            consent_revision: Cell::new(0),
            changes: RefCell::new(live::Changes::default()),
            named: RefCell::new(Vec::new()),
            named_failures: RefCell::new(Vec::new()),
            files: RefCell::new(files::Files::new(&blob_cache)),
            blob_cache,
            initializer: bootstrap.local,
        })
    }

    fn authorize(
        &self,
        volume: &Volume,
        entry: Option<EntryId>,
        permission: Permission,
    ) -> Result<MetadataPolicy> {
        if permission != Permission::ManagePermissions {
            authority::check(self, volume, entry)?;
        }
        self.authorize_current(volume, entry, permission)
    }

    fn authorize_entry_metadata(&self, volume: &Volume, entry: EntryId) -> Result<bool> {
        authority::check(self, volume, Some(entry))?;
        let policy = crate::store::DatabasePolicy::read(&volume.index, DatabaseId::Entry(entry))
            .map_err(store_error)?;
        let readable = policy.metadata_readable(self.account).map_err(store_error)?;
        self.admit_volume(volume, if readable { Permission::Read } else { Permission::Write })?;
        Ok(readable)
    }

    fn authorize_current(
        &self,
        volume: &Volume,
        entry: Option<EntryId>,
        permission: Permission,
    ) -> Result<MetadataPolicy> {
        let policy = self.admit_volume(volume, permission)?;
        if let Some(entry) = entry {
            let snapshot = crate::store::volume_index::IndexSnapshot::new(
                volume.index.begin_read().map_err(map_error)?,
            );
            snapshot
                .authorize(
                    policy.volume(),
                    policy.default_permissions(),
                    crate::store::object_contract::Principal::Human(self.account),
                    entry,
                    permission,
                )
                .map_err(store_error)?;
        } else {
            policy.authorize(self.account, permission).map_err(map_error)?;
        }
        Ok(policy)
    }

    /// Participant/session admission is separate from catalog or entry Read.
    fn admit_volume(&self, volume: &Volume, permission: Permission) -> Result<MetadataPolicy> {
        authority::check_session(self)?;
        let volumes = self.volumes.borrow();
        let root = MetadataPolicy::read(&volumes[0].index).map_err(map_error)?;
        if !root.volume().same_identity(&volumes[0].record) {
            return Err(Error::AccessDenied);
        }
        let session = self.session.borrow();
        root.admit(self.account, &session, session.replica_volume()).map_err(map_error)?;
        if volume.id != session.replica_volume() {
            let selected = self.selections.borrow();
            let grant = selected
                .get(&session.id)
                .and_then(|grants| grants.iter().find(|grant| grant.volume == volume.id))
                .ok_or(Error::AccessDenied)?;
            if permission != Permission::Read && !grant.write {
                return Err(Error::AccessDenied);
            }
        }
        self.check_participant(volume, &volumes)?;
        // This dispatch is synchronous on its worker. Reuse this operation's
        // current root policy; never retain an authorization result across calls.
        let policy = if volume.id == session.replica_volume() {
            root
        } else {
            drop(root);
            MetadataPolicy::read(&volume.index).map_err(map_error)?
        };
        if !policy.volume().same_identity(&volume.record) {
            return Err(Error::AccessDenied);
        }
        if volume.id != session.replica_volume() && policy.volume().owner != self.account {
            return Err(Error::AccessDenied);
        }
        Ok(policy)
    }
    fn check_participant(&self, target: &Volume, dependencies: &[Rc<Volume>; 1]) -> Result<()> {
        self.check_revocation(target)?;
        if dependencies[0].id != target.id {
            self.check_revocation(&dependencies[0])?;
        }
        Ok(())
    }
    fn check_revocation(&self, volume: &Volume) -> Result<()> {
        if volume
            .revocations
            .begin_read()
            .map_err(map_error)?
            .has(self.account.to_string().as_bytes())
            .map_err(map_error)?
        {
            return Err(Error::AccessDenied);
        }
        Ok(())
    }

    fn adopt(&self, candidate: Session) -> Result<()> {
        let selections = self.selections.borrow();
        let selections = selections.get(&candidate.id).map_or(&[][..], Vec::as_slice);
        let grants = selections.iter().map(PersonalVolumeSelection::grant).collect::<Vec<_>>();
        self.visit_candidate(&candidate, &grants, |_| Ok(()))?;
        self.files.borrow_mut().retain_selection(selections)?;
        *self.session.borrow_mut() = candidate;
        Ok(())
    }

    // One synchronous pass over current policy. The visitor must not retain a
    // policy; both validation and catalog preparation keep only one decoded copy.
    fn visit_candidate(
        &self,
        candidate: &Session,
        grants: &[PersonalVolumeGrant],
        mut visit: impl FnMut(&MetadataPolicy) -> Result<()>,
    ) -> Result<()> {
        authority::check_session(self)?;
        let volumes = self.volumes.borrow();
        self.validate_grants(grants)?;
        let current = self.session.borrow();
        let mut identity = candidate.clone();
        identity.selector = current.selector.clone();
        identity.resolved_build_id = current.resolved_build_id.clone();
        if identity != *current {
            return Err(map_error("session update changed immutable identity"));
        }
        use crate::store::BuildSelector;
        match (&current.selector, &candidate.selector) {
            (BuildSelector::Exact { .. }, BuildSelector::Exact { build_id })
                if build_id == &candidate.resolved_build_id => {}
            (BuildSelector::App { app: old }, BuildSelector::App { app }) if old == app => {}
            _ => return Err(map_error("session update changed selector authority")),
        }
        let root = MetadataPolicy::read(&volumes[0].index).map_err(map_error)?;
        for volume in volumes.iter() {
            root.admit(self.account, candidate, volume.id).map_err(map_error)?;
        }
        self.check_participant(&volumes[0], &volumes)?;
        root.authorize(self.account, Permission::Read).map_err(map_error)?;
        visit(&root)?;
        drop(root);
        {
            let personal = self.personal.borrow();
            if grants.iter().any(|grant| !personal.iter().any(|volume| volume.id == grant.volume)) {
                return Err(Error::AccessDenied);
            }
            // Keep catalog order aligned with retained host slots, not chooser order.
            for volume in personal.iter() {
                let Some(grant) = grants.iter().find(|grant| grant.volume == volume.id) else {
                    continue;
                };
                let policy = MetadataPolicy::read(&volume.index).map_err(map_error)?;
                if policy.volume().owner != self.account {
                    return Err(Error::AccessDenied);
                }
                policy.authorize(self.account, Permission::Read).map_err(map_error)?;
                if grant.write {
                    policy.authorize(self.account, Permission::Write).map_err(map_error)?;
                }
                self.check_revocation(volume)?;
                visit(&policy)?;
            }
        }
        Ok(())
    }

    fn selected_volumes(&self) -> Vec<(usize, Rc<Volume>)> {
        let mut result: Vec<_> = self.volumes.borrow().iter().cloned().enumerate().collect();
        let selections = self.selections.borrow();
        if let Some(grants) = selections.get(&self.session.borrow().id) {
            for (index, volume) in self.personal.borrow().iter().enumerate() {
                if grants.iter().any(|grant| grant.volume == volume.id) {
                    result.push((index + 1, volume.clone()));
                }
            }
        }
        result
    }

    fn register_personal(&self, entry: PersonalVolume) -> Result<()> {
        let mut personal = self.personal.borrow_mut();
        if personal.len() >= 30 {
            return Err(Error::LimitExceeded);
        }
        if entry.volume.owner != self.account
            || self.volumes.borrow().iter().any(|v| v.id == entry.volume.id)
            || personal.iter().any(|v| v.id == entry.volume.id)
        {
            return Err(Error::InvalidArgument);
        }
        let base = 3 + personal.len() as u32 * 2;
        let volume = Rc::new(Volume::open(entry.volume, entry.key, base, &entry.replica_ids)?);
        self.volume_objects.borrow_mut().insert(volume.id, volume.clone());
        personal.push(volume);
        Ok(())
    }

    fn validate_grants(&self, grants: &[PersonalVolumeGrant]) -> Result<()> {
        if grants.len() > 30 {
            return Err(Error::LimitExceeded);
        }
        let personal = self.personal.borrow();
        let mut seen = std::collections::BTreeSet::new();
        for grant in grants {
            if !seen.insert(grant.volume) || !personal.iter().any(|v| v.id == grant.volume) {
                return Err(Error::InvalidArgument);
            }
        }
        Ok(())
    }

    fn select_personal(&self, selection: PersonalSelection) -> Result<()> {
        if !crate::identifier::valid(&selection.session, 32) {
            return Err(Error::InvalidArgument);
        }
        let grants =
            selection.grants.iter().map(PersonalVolumeSelection::grant).collect::<Vec<_>>();
        self.validate_grants(&grants)?;
        let mut incarnations = std::collections::BTreeSet::new();
        if selection.grants.iter().any(|grant| {
            grant.incarnation == Id::from_bytes([0; 16]) || !incarnations.insert(grant.incarnation)
        }) {
            return Err(Error::InvalidArgument);
        }
        let revision = self.consent_revision.get().checked_add(1).ok_or(Error::LimitExceeded)?;
        if selection.session == self.session.borrow().id {
            self.files.borrow_mut().retain_selection(&selection.grants)?;
        }
        self.selections.borrow_mut().insert(selection.session, selection.grants);
        self.consent_revision.set(revision);
        Ok(())
    }

    fn volume_handle(&self, generation: u64, slot: u32) -> Result<Rc<Volume>> {
        let generation = u32::try_from(generation).map_err(|_| Error::StaleHandle)?;
        let id = work::capabilities(|capabilities| {
            Ok(Id::from_bytes(
                capabilities
                    .volume(
                        crate::capability::Handle::new(generation, slot),
                        crate::capability::Rights::NONE,
                    )?
                    .id,
            ))
        })?;
        self.volume_objects.borrow().get(&id).cloned().ok_or(Error::Unsupported)
    }
}

// The limit belongs to this private caller; the requested length belongs to JS.
fn admit_input(length: usize, limit: usize) -> Result<std::sync::MutexGuard<'static, Vec<u8>>> {
    assert!(limit <= INPUT_LIMIT, "entry-point budget fits the shared input buffer");
    if length > limit {
        return Err(Error::LimitExceeded);
    }
    let input = INPUT.lock().expect("mutex is not poisoned");
    if length > input.len() {
        return Err(Error::InvalidArgument);
    }
    assert!(input.len() <= INPUT_LIMIT, "only bounded buffer allocation publishes input");
    Ok(input)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_web_validate_text_input(length: usize) -> i32 {
    crate::platform::failure::guard();
    let validate = || {
        let input = admit_input(length, playbit_api::abi::syscall::MAX_EVENT_BYTES)?;
        playbit_api::abi::text::TextInput::decode(&input[..length]).map(|_| ())
    };
    validate().map_or_else(|error| error as i32, |()| 0)
}
fn publish_output(bytes: Vec<u8>) -> i32 {
    if bytes.len() > LIMIT {
        return Error::LimitExceeded as i32;
    }
    let length = bytes.len();
    *OUTPUT.lock().expect("mutex is not poisoned") = bytes;
    length as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_input(length: usize) -> *mut u8 {
    crate::platform::failure::guard();
    crate::platform::failure::install(|report| {
        // The worker synchronously copies this bounded slice and terminates.
        unsafe { engine_panic(report.as_ptr(), report.len()) };
    });
    if length > INPUT_LIMIT {
        return std::ptr::null_mut();
    }
    let mut input = INPUT.lock().expect("mutex is not poisoned");
    input.resize(length, 0);
    input.as_mut_ptr()
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_version() -> i32 {
    crate::platform::failure::guard();
    publish_output(crate::VERSION.as_bytes().to_vec())
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_output() -> *const u8 {
    crate::platform::failure::guard();
    let output = OUTPUT.lock().expect("mutex is not poisoned");
    assert!(output.len() <= LIMIT, "exported output respects the shared byte limit");
    output.as_ptr()
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_initialize(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    if HOST.with(|cell| cell.borrow().is_some()) {
        return Error::InvalidArgument as i32;
    }
    match serde_json::from_slice(&input[..length]).map_err(map_error).and_then(Host::open) {
        Ok(host) => HOST.with(|cell| {
            if cell.borrow().is_some() {
                return Error::InvalidArgument as i32;
            }
            *cell.borrow_mut() = Some(Rc::new(host));
            0
        }),
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_error() -> i32 {
    crate::platform::failure::guard();
    let bytes = LAST_ERROR.lock().expect("mutex is not poisoned").as_bytes().to_vec();
    publish_output(bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_sync_begin(slot: usize, direction: u32) -> i32 {
    sync_begin(slot, direction, false)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_sync_begin(slot: u32) -> i32 {
    sync_begin(slot as usize, 0, true)
}
fn sync_begin(slot: usize, direction: u32, prepared_volume: bool) -> i32 {
    crate::platform::failure::guard();
    let result = HOST.with(|host| {
        SYNC.with(|sync| {
            if sync.borrow().is_some() {
                return Err(Error::InvalidArgument);
            }
            let (database, address) = if prepared_volume {
                let (db, address) = publication::begin_sync(slot as u32)?;
                (db, Some(address))
            } else if slot == 0 {
                (account::database()?, None)
            } else {
                let host = host.borrow().clone().ok_or(Error::Unavailable)?;
                let address = host.database_address(slot)?;
                let volume = host.replica_volume(address.volume)?;
                host.replica_policy(&volume, address.database, direction == 0)?;
                (host.database(slot)?, Some(address))
            };
            *INPUT.lock().expect("mutex is not poisoned") = Vec::with_capacity(SYNC_FRAGMENT);
            let mut output = OUTPUT.lock().expect("mutex is not poisoned");
            *output = Vec::with_capacity(SYNC_FRAGMENT);
            let mut prepared = Sync::new(database, address, direction, &mut output)?;
            prepared.prepared = prepared_volume;
            prepared.authorize()?;
            *sync.borrow_mut() = Some(prepared);
            Ok(output.len())
        })
    });
    match result {
        Ok(length) => {
            assert!(length <= SYNC_FRAGMENT, "sync emits one bounded transport fragment");
            length as i32
        }
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_sync_step(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, SYNC_FRAGMENT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let mut output = OUTPUT.lock().expect("mutex is not poisoned");
    output.clear();
    let result: Result<usize> = SYNC.with(|sync| {
        let mut sync = sync.borrow_mut();
        let sync = sync.as_mut().ok_or(Error::Unavailable)?;
        sync.authorize()?;
        let status = match &mut sync.transfer {
            Transfer::Pull(receiver) => receiver.process(&input[..length], &mut output),
            Transfer::Push(sender) => sender.process(&input[..length]),
            Transfer::Inspect(_) => return Err(Error::InvalidArgument),
        }
        .map_err(map_error)?;
        sync.done = status == SyncStatus::Done;
        Ok(output.len())
    });
    match result {
        Ok(length) => {
            assert!(length <= SYNC_FRAGMENT, "sync emits one bounded transport fragment");
            length as i32
        }
        Err(error) => error as i32,
    }
}
/// Drain one bounded sender fragment, including after it reports Done.
#[unsafe(no_mangle)]
pub extern "C" fn pb_sync_output() -> i32 {
    crate::platform::failure::guard();
    let mut output = OUTPUT.lock().expect("mutex is not poisoned");
    output.resize(SYNC_FRAGMENT, 0);
    let result = SYNC.with(|sync| {
        let mut sync = sync.borrow_mut();
        let sync = sync.as_mut().ok_or(Error::Unavailable)?;
        sync.authorize()?;
        match &mut sync.transfer {
            Transfer::Push(sender) => sender.write_output(&mut output).map_err(map_error),
            Transfer::Pull(_) | Transfer::Inspect(_) => Err(Error::InvalidArgument),
        }
    });
    match result {
        Ok(length) => {
            assert!(length <= output.len(), "sync writer respects destination capacity");
            output.truncate(length);
            length as i32
        }
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_sync_done() -> u32 {
    crate::platform::failure::guard();
    SYNC.with(|sync| u32::from(sync.borrow().as_ref().is_some_and(|sync| sync.done)))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_sync_finish(commit: u32) -> i32 {
    crate::platform::failure::guard();
    let result = SYNC.with(|sync| {
        let sync = sync.borrow_mut().take().ok_or(Error::Unavailable)?;
        if commit != 0 {
            sync.authorize()?;
            if !sync.done {
                return Err(Error::InvalidArgument);
            }
            let changes = match sync.transfer {
                Transfer::Pull(receiver) if !sync.account => {
                    Some(receiver.finish().map_err(map_error)?)
                }
                Transfer::Pull(_) => return Err(Error::InvalidArgument),
                Transfer::Inspect(inspection) => Some(inspection.finish().map_err(map_error)?),
                Transfer::Push(_) => None,
            };
            if let Some(changes) = changes {
                if let Some(address) = &sync.address {
                    HOST.with(|host| {
                        let host = host.borrow();
                        let host = host.as_ref().ok_or(Error::Unavailable)?;
                        if sync.prepared {
                            publication::merge(host, address, &sync.database, changes)
                        } else {
                            host.merge_prepared(address, &sync.database, changes)
                        }
                    })?;
                } else {
                    changes
                        .merge(&sync.database, Lww, unsafe { now() } as u64, 0)
                        .map_err(map_error)?
                        .finish_with_delta()
                        .map_err(map_error)?;
                }
            }
        }
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_select_personal(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|host| {
        let host = host.borrow().clone().ok_or(Error::Unavailable)?;
        host.select_personal(
            serde_json::from_slice(input.get(..length).ok_or(Error::InvalidArgument)?)
                .map_err(map_error)?,
        )
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
/// Register one retained storage slot. This grants no guest access.
#[unsafe(no_mangle)]
pub extern "C" fn pb_register_personal(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|host| {
        let host = host.borrow().clone().ok_or(Error::Unavailable)?;
        host.register_personal(
            serde_json::from_slice(input.get(..length).ok_or(Error::InvalidArgument)?)
                .map_err(map_error)?,
        )
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_adopt_session(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|host| {
        if SYNC.with(|sync| sync.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        let host = host.borrow().clone().ok_or(Error::Unavailable)?;
        if host.previous_session.borrow().is_some() {
            return Err(Error::Conflict);
        }
        host.adopt(serde_json::from_slice(&input[..length]).map_err(map_error)?)
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
/// Candidate startup is tentative. Rollback restores the exact prior identity;
/// subsequent operations still check the latest policy and revocations.
#[unsafe(no_mangle)]
pub extern "C" fn pb_begin_adoption(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|host| {
        if SYNC.with(|sync| sync.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        let host = host.borrow().clone().ok_or(Error::Unavailable)?;
        if host.previous_session.borrow().is_some() {
            return Err(Error::Conflict);
        }
        let previous = host.session.borrow().clone();
        host.adopt(serde_json::from_slice(&input[..length]).map_err(map_error)?)?;
        *host.previous_session.borrow_mut() = Some(previous);
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_finish_adoption(commit: u32) -> i32 {
    crate::platform::failure::guard();
    if commit > 1 {
        return Error::InvalidArgument as i32;
    }
    let result: Result<()> = HOST.with(|host| {
        let host = host.borrow().clone().ok_or(Error::Unavailable)?;
        let previous = host.previous_session.borrow_mut().take().ok_or(Error::InvalidArgument)?;
        if commit == 0 {
            *host.session.borrow_mut() = previous;
        }
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}
#[cfg(feature = "test-support")]
#[unsafe(no_mangle)]
pub extern "C" fn pb_test_panic() {
    crate::platform::failure::guard();
    panic!("W7 injected web engine panic");
}

fn new_bundle_id() -> [u8; 16] {
    let mut bytes = [0; 16];
    // SAFETY: synchronous import fills the exclusive fixed output buffer.
    unsafe { random(bytes.as_mut_ptr(), bytes.len()) };
    bytes
}
