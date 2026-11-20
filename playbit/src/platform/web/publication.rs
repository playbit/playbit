//! Detached volume receipt. Only publication replaces guest-visible handles.
//! Rust qualifies source cuts, current admission, identities and candidate policy;
//! JS qualifies the local write epoch and commits the durable OPFS routing pointer.
use super::*;
use crate::store::{
    DatabasePolicy, ReplicaReader, SnapshotCut, SnapshotToken,
    volume_index::{self, DatabaseProtocol, IndexSnapshot},
};
use databases::{Address, Named};
use std::cell::Cell;
mod archive;
mod backings;
mod namespace;

thread_local! {
    static PREPARED: RefCell<Option<Rc<Preparation>>> = const { RefCell::new(None) };
}
struct Preparation {
    volume: Rc<Volume>,
    original: Rc<Volume>,
    accepted_index: Rc<Database>,
    accepted_slot: u32,
    slots: [u32; 2],
    named: RefCell<Option<Named>>,
    session: Session,
    consent_revision: u32,
    selection: Option<SelectionCatalog>,
    validated: Cell<bool>,
    log_validation: RefCell<Option<Vec<u8>>>,
    log_complete: Cell<bool>,
    log_prepared: Cell<bool>,
    source_cut: RefCell<Option<SnapshotCut>>,
    namespace_receipt: RefCell<Option<crate::store::ReplicaReceipt>>,
    source_failed: Cell<bool>,
    controls_received: Cell<u8>,
    backing_received: Cell<bool>,
    authority_generation: u64,
    authority_qualified: Cell<bool>,
    local_backing_used: Cell<bool>,
    generation: Id,
    entry_scope: Option<(EntryId, u64)>,
    archive: RefCell<archive::State>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Begin {
    volume: Id,
    base: u32,
    replica_ids: [u64; 2],
    accepted_slot: u32,
    accepted_replica_id: u64,
    selection: Option<SelectionCatalog>,
    session: Session,
    consent_revision: u32,
    generation: Id,
    entry_scope: Option<EntryId>,
    authority_generation: [u8; 8],
    entry_generation: Option<[u8; 8]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Backing {
    entry: EntryId,
    slot: u32,
    replica_id: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    slot: u32,
    receipt: crate::store::ReplicaReceipt,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    after: Option<EntryId>,
    expected_root: Option<[u8; 32]>,
    copy_local: bool,
}
#[derive(Serialize)]
struct Page {
    backings: Vec<(EntryId, DatabaseProtocol)>,
    next: Option<EntryId>,
    root: Option<[u8; 32]>,
}

fn prepared() -> Result<Rc<Preparation>> {
    PREPARED.with(|cell| cell.borrow().clone().ok_or(Error::Unavailable))
}
pub(super) fn call(length: usize, run: impl FnOnce(&Host, &[u8]) -> Result<Vec<u8>>) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, LIMIT)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        if SYNC.with(|sync| sync.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        run(&host, &input[..length])
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}

impl Preparation {
    fn policy_index(&self) -> &Database {
        if self.controls_received.get() & 1 != 0 {
            &self.accepted_index
        } else {
            &self.volume.index
        }
    }
    fn check_base(&self) -> Result<()> {
        let original = self.original.index.begin_read().map_err(map_error)?;
        let working = self.volume.index.begin_read().map_err(map_error)?;
        let accepted = self.accepted_index.begin_read().map_err(map_error)?;
        let root = original.root_hash().map_err(map_error)?;
        if working.root_hash().map_err(map_error)? != root
            || accepted.root_hash().map_err(map_error)?.is_some()
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    fn accept_source(&self, source: SnapshotToken, policy_revision: u64) -> Result<()> {
        if self.source_failed.get() {
            return Err(Error::Conflict);
        }
        let mut current = self.source_cut.borrow_mut();
        match *current {
            Some(cut) if !cut.accepts(source, policy_revision) => {
                self.source_failed.set(true);
                self.validated.set(false);
                Err(Error::Conflict)
            }
            Some(_) => Ok(()),
            None => {
                *current = Some(SnapshotCut::new(source, policy_revision).map_err(store_error)?);
                Ok(())
            }
        }
    }
    fn pending_personal(&self, host: &Host) -> Result<bool> {
        let primary = host.volumes.borrow();
        if primary.iter().any(|volume| volume.id == self.volume.id) {
            return Ok(false);
        }
        let selection = self.selection.as_ref().ok_or(Error::AccessDenied)?;
        host.validate_grants(&selection.grants)?;
        if self.volume.record.owner != host.account
            || !selection.grants.iter().any(|grant| grant.volume == self.volume.id)
        {
            return Err(Error::AccessDenied);
        }
        host.authorize_current(&primary[0], None, Permission::Read)?;
        let selections = host.selections.borrow();
        if !selections.get(&selection.session.id).is_some_and(|grants| {
            grants.iter().map(PersonalVolumeSelection::grant).eq(selection.grants.iter().cloned())
        }) {
            return Err(Error::AccessDenied);
        }
        Ok(true)
    }
    fn current_authority(&self, host: &Host, database: DatabaseId) -> Result<()> {
        if self
            .entry_scope
            .is_some_and(|(entry, _)| matches!(database, DatabaseId::Entry(id) if id != entry))
        {
            return Err(Error::AccessDenied);
        }
        if *host.session.borrow() != self.session
            || host.consent_revision.get() != self.consent_revision
        {
            return Err(Error::AccessDenied);
        }
        let personal = self.pending_personal(host)?;
        let current = host.replica_volume(self.volume.id)?;
        if !Rc::ptr_eq(&current, &self.original) {
            return Err(Error::AccessDenied);
        }
        let read = current.index.begin_read().map_err(map_error)?;
        if read.root_hash().map_err(map_error)?.is_none() {
            return Ok(());
        }
        drop(read);
        if self.entry_scope.is_some() {
            host.admit_volume(&current, Permission::Read)?;
            return Ok(());
        }
        if personal {
            let policy = MetadataPolicy::read(&current.index).map_err(map_error)?;
            policy.authorize(host.account, Permission::Read).map_err(store_error)?;
            if current
                .revocations
                .begin_read()
                .map_err(map_error)?
                .has(host.account.to_string().as_bytes())
                .map_err(map_error)?
            {
                return Err(Error::AccessDenied);
            }
        } else {
            host.authorize_current(&current, None, Permission::Read)?;
        }
        if let DatabaseId::Entry(id) = database {
            let index = self.policy_index();
            let snapshot = IndexSnapshot::new(index.begin_read().map_err(map_error)?);
            match snapshot.source_entry(id) {
                Ok(entry) if entry.deleted => return Err(Error::AccessDenied),
                Ok(_) => {
                    let policy =
                        DatabasePolicy::read_source(index, database).map_err(store_error)?;
                    policy
                        .authorize_reader(ReplicaReader::Participant(host.account))
                        .map_err(store_error)?;
                }
                Err(crate::store::Error::NotFound(_)) => {}
                Err(error) => return Err(store_error(error)),
            }
        }
        Ok(())
    }
    fn policy(&self, host: &Host, database: DatabaseId) -> Result<DatabasePolicy> {
        self.current_authority(host, database)?;
        let index = self.policy_index();
        let read = index.begin_read().map_err(map_error)?;
        let empty_bootstrap =
            database == DatabaseId::Index && read.root_hash().map_err(map_error)?.is_none();
        let policy = if empty_bootstrap {
            DatabasePolicy::reserved(
                MetadataPolicy::new(self.volume.record.clone(), Acl::default())
                    .map_err(store_error)?,
                database,
            )
            .map_err(store_error)?
        } else {
            drop(read);
            DatabasePolicy::read_source(index, database).map_err(store_error)?
        };
        if !policy.volume.volume().same_identity(&self.volume.record) {
            return Err(Error::AccessDenied);
        }
        // AP admits initial receipt. Candidate read authority must qualify
        // before publishing any state into the guest-visible generation.
        if !empty_bootstrap && let Some((entry, _)) = self.entry_scope {
            match DatabasePolicy::read_source(index, DatabaseId::Entry(entry)) {
                Ok(selected) => {
                    let readable = selected.metadata_readable(host.account).map_err(store_error)?;
                    if !readable {
                        if matches!(database, DatabaseId::Entry(_)) {
                            return Err(Error::AccessDenied);
                        }
                        host.admit_volume(&self.original, Permission::Write)?;
                    }
                    selected.authorize_reader(ReplicaReader::Service).map_err(store_error)?;
                }
                Err(crate::store::Error::NotFound(_))
                    if database == DatabaseId::Index && self.controls_received.get() & 1 == 0 => {}
                Err(error) => return Err(store_error(error)),
            }
        } else if !empty_bootstrap {
            policy
                .authorize_reader(ReplicaReader::Participant(host.account))
                .map_err(store_error)?;
        }
        policy.authorize_reader(ReplicaReader::Service).map_err(store_error)?;
        Ok(policy)
    }
    fn database(&self, slot: u32) -> Result<(Rc<Database>, Address)> {
        let (database, db) = if slot == self.slots[0] {
            (DatabaseId::Index, self.volume.index.clone())
        } else if slot == self.slots[1] {
            (DatabaseId::Revocations, self.volume.revocations.clone())
        } else {
            let named = self.named.borrow();
            let named = named.as_ref().filter(|named| named.slot == slot).ok_or(Error::NotFound)?;
            (named.address.database, named.db.clone())
        };
        Ok((db, Address { volume: self.volume.id, database }))
    }
    fn validate(&self, host: &Host) -> Result<()> {
        self.archive.borrow().validate()?;
        if self.archive.borrow().restored_quarantine && !self.authority_qualified.get() {
            return Err(Error::AccessDenied);
        }
        if self.named.borrow().is_some()
            || self.controls_received.get() != 3
            || self.source_failed.get()
        {
            return Err(Error::Unavailable);
        }
        let policy = self.policy(host, DatabaseId::Index)?;
        if self.entry_scope.is_none() {
            policy.volume.authorize(host.account, Permission::Read).map_err(store_error)?;
        }
        if self.volume.id == self.session.replica_volume() {
            for volume in host.volumes.borrow().iter() {
                policy.volume.admit(host.account, &self.session, volume.id).map_err(store_error)?;
            }
        }
        if self
            .volume
            .revocations
            .begin_read()
            .map_err(map_error)?
            .has(host.account.to_string().as_bytes())
            .map_err(map_error)?
        {
            return Err(Error::AccessDenied);
        }
        authority::publish_marker(
            host,
            &self.original,
            &self.volume,
            self.authority_generation,
            self.entry_scope,
            self.authority_qualified.get(),
        )?;
        self.validated.set(true);
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_prepare(length: usize) -> i32 {
    call(length, |host, bytes| {
        if PREPARED.with(|cell| cell.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        let args: Begin = serde_json::from_slice(bytes).map_err(map_error)?;
        if args.base <= 64 || args.base >= i32::MAX as u32 {
            return Err(Error::InvalidArgument);
        }
        if args.accepted_slot <= 64
            || args.accepted_slot >= i32::MAX as u32
            || args.accepted_slot == args.base
            || args.accepted_slot == args.base + 1
        {
            return Err(Error::InvalidArgument);
        }
        if args.generation.as_bytes() == &[0; 16] {
            return Err(Error::InvalidArgument);
        }
        if args.entry_scope.is_some() != args.entry_generation.is_some() {
            return Err(Error::InvalidArgument);
        }
        let original = host.replica_volume(args.volume)?;
        let volume = Rc::new(Volume::open(
            original.record.clone(),
            original.key,
            args.base,
            &args.replica_ids,
        )?);
        let stage = Rc::new(Preparation {
            accepted_index: databases::open(
                args.accepted_slot,
                DatabaseId::Index.hash_key(&original.key, original.id),
                args.accepted_replica_id,
            )?,
            accepted_slot: args.accepted_slot,
            volume,
            original: original.clone(),
            slots: [args.base, args.base + 1],
            named: RefCell::new(None),
            session: args.session,
            consent_revision: args.consent_revision,
            selection: args.selection,
            validated: Cell::new(false),
            log_validation: RefCell::new(None),
            log_complete: Cell::new(false),
            log_prepared: Cell::new(false),
            source_cut: RefCell::new(None),
            namespace_receipt: RefCell::new(None),
            source_failed: Cell::new(false),
            controls_received: Cell::new(0),
            backing_received: Cell::new(false),
            authority_generation: u64::from_le_bytes(args.authority_generation),
            authority_qualified: Cell::new(false),
            local_backing_used: Cell::new(false),
            generation: args.generation,
            entry_scope: args.entry_scope.zip(args.entry_generation.map(u64::from_le_bytes)),
            archive: RefCell::new(archive::State::default()),
        });
        stage.check_base()?;
        stage.current_authority(host, DatabaseId::Index)?;
        PREPARED.with(|cell| *cell.borrow_mut() = Some(stage));
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_register_backing(length: usize) -> i32 {
    call(length, |host, bytes| {
        let args: Backing = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        if args.slot <= 64
            || stage.slots.contains(&args.slot)
            || args.slot == stage.accepted_slot
            || stage.named.borrow().is_some()
            || stage.archive.borrow().started()
        {
            return Err(Error::InvalidArgument);
        }
        let address = Address { volume: stage.volume.id, database: DatabaseId::Entry(args.entry) };
        stage.policy(host, address.database)?;
        let key = volume_index::entry_key(args.entry);
        let before = stage
            .original
            .index
            .begin_read()
            .map_err(map_error)?
            .get_with_clock(&key)
            .map_err(map_error)?;
        let candidate = stage
            .volume
            .index
            .begin_read()
            .map_err(map_error)?
            .get_with_clock(&key)
            .map_err(map_error)?;
        if let Some((clock, birth)) = before
            && candidate.as_ref().is_none_or(|(candidate_clock, candidate)| {
                clock != *candidate_clock || birth.as_ref() != candidate.as_ref()
            })
        {
            return Err(Error::AccessDenied);
        }
        let db = databases::open(
            args.slot,
            address.database.hash_key(&stage.volume.key, address.volume),
            args.replica_id,
        )?;
        *stage.named.borrow_mut() = Some(Named { address, slot: args.slot, db });
        *stage.log_validation.borrow_mut() = None;
        stage.log_complete.set(false);
        stage.log_prepared.set(false);
        stage.backing_received.set(false);
        stage.authority_qualified.set(false);
        stage.validated.set(false);
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_close_backing() -> i32 {
    call(0, |host, _| {
        let stage = prepared()?;
        if let Some(named) = stage.named.borrow().as_ref() {
            if !stage.backing_received.get() {
                return Err(Error::Unavailable);
            }
            let policy = stage.policy(host, named.address.database)?;
            if policy
                .entry
                .as_ref()
                .is_some_and(|entry| entry.protocol == Some(DatabaseProtocol::BlobLog))
                && !stage.log_complete.get()
            {
                return Err(Error::Unavailable);
            }
            let DatabaseId::Entry(entry) = named.address.database else {
                return Err(Error::InvalidArgument);
            };
            backing::bind(&stage.volume, entry, stage.generation)?;
        }
        stage.named.borrow_mut().take();
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_local_location(length: usize) -> i32 {
    call(length, |host, bytes| {
        let entry: EntryId = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        stage.current_authority(host, DatabaseId::Index)?;
        serde_json::to_vec(&backing::location(&stage.original, entry)?).map_err(map_error)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_bind_backing(length: usize) -> i32 {
    call(length, |host, bytes| {
        let entry: EntryId = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        stage.current_authority(host, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(stage.volume.index.begin_read().map_err(map_error)?);
        if !snapshot.source_backing_ready(entry).map_err(store_error)? {
            return Err(Error::Unavailable);
        }
        drop(snapshot);
        backing::bind(&stage.volume, entry, stage.generation)?;
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_local_backing(length: usize) -> i32 {
    call(length, |host, bytes| {
        let entry: EntryId = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        let policy = stage.policy(host, DatabaseId::Entry(entry))?;
        let source = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?);
        if !source.backing_ready(entry).map_err(store_error)? {
            return Err(Error::Unavailable);
        }
        let key = volume_index::entry_key(entry);
        let before = stage
            .original
            .index
            .begin_read()
            .map_err(map_error)?
            .get_with_clock(&key)
            .map_err(map_error)?;
        let candidate = stage
            .volume
            .index
            .begin_read()
            .map_err(map_error)?
            .get_with_clock(&key)
            .map_err(map_error)?;
        match (before, candidate) {
            (Some((a, av)), Some((b, bv))) if a == b && av.as_ref() == bv.as_ref() => {}
            _ => return Err(Error::AccessDenied),
        }
        if !IndexSnapshot::new(stage.volume.index.begin_read().map_err(map_error)?)
            .source_backing_ready(entry)
            .map_err(store_error)?
        {
            return Err(Error::Unavailable);
        }
        policy.authorize_reader(ReplicaReader::Participant(host.account)).map_err(store_error)?;
        let named = stage.named.borrow();
        if named.as_ref().is_none_or(|named| named.address.database != DatabaseId::Entry(entry)) {
            return Err(Error::InvalidArgument);
        }
        stage.local_backing_used.set(true);
        stage.authority_qualified.set(false);
        stage.backing_received.set(true);
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_catalog_page(length: usize) -> i32 {
    call(length, |host, bytes| {
        let request: Catalog = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        stage.current_authority(host, DatabaseId::Index)?;
        if !request.copy_local && stage.controls_received.get() & 1 == 0 {
            return Err(Error::Unavailable);
        }
        let index = if request.copy_local { &stage.original.index } else { &stage.accepted_index };
        let read = index.begin_read().map_err(map_error)?;
        let root = read.root_hash().map_err(map_error)?.map(|(_, hash)| hash);
        if request.after.is_some() && request.expected_root.is_none()
            || request.expected_root.is_some() && request.expected_root != root
        {
            return Err(map_error("prepared database catalog changed"));
        }
        let snapshot = IndexSnapshot::new(read);
        if let Some((entry, _)) = stage.entry_scope {
            let backings =
                catalog_entry(host, &stage, index, &snapshot, entry, request.copy_local)?;
            return serde_json::to_vec(&Page { backings, next: None, root }).map_err(map_error);
        }
        let page = if request.copy_local {
            snapshot.headers(request.after, 64)
        } else {
            snapshot.source_headers(request.after, 64)
        }
        .map_err(store_error)?;
        let mut backings = Vec::with_capacity(64);
        for entry in page.entries {
            let Some(protocol) = entry.protocol else {
                continue;
            };
            if request.copy_local {
                if snapshot.backing_ready(entry.id).map_err(store_error)? {
                    backings.push((entry.id, protocol));
                }
            } else if !entry.deleted {
                let policy = DatabasePolicy::read_source(index, DatabaseId::Entry(entry.id))
                    .map_err(store_error)?;
                if policy.authorize_reader(ReplicaReader::Participant(host.account)).is_ok()
                    && policy.authorize_reader(ReplicaReader::Service).is_ok()
                {
                    backings.push((entry.id, protocol));
                }
            }
        }
        serde_json::to_vec(&Page { backings, next: page.next, root }).map_err(map_error)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_admission(length: usize) -> i32 {
    call(length, |host, bytes| {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            volume: Id,
            entry_scope: Option<EntryId>,
        }
        let request: Request = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(request.volume)?;
        let (volume, entry) = authority::generations(host, &volume, request.entry_scope)?;
        serde_json::to_vec(&serde_json::json!({"session": &*host.session.borrow(),
            "consent_revision":host.consent_revision.get(),
            "authority_generation": volume.to_le_bytes(),
            "entry_generation": entry.map(u64::to_le_bytes)}))
        .map_err(map_error)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_log_page(length: usize) -> i32 {
    call(length, |host, bytes| {
        if bytes.len() > 384 * 1024 {
            return Err(Error::LimitExceeded);
        }
        let receipt: crate::store::bloblog::ReplicaPage =
            serde_json::from_slice(bytes).map_err(map_error)?;
        receipt.cut().map_err(store_error)?;
        let stage = prepared()?;
        if !stage.log_prepared.get() {
            return Err(Error::Unavailable);
        }
        let named = stage.named.borrow();
        let named = named.as_ref().ok_or(Error::Unavailable)?;
        let policy = stage.policy(host, named.address.database)?;
        if !policy
            .entry
            .as_ref()
            .is_some_and(|entry| entry.protocol == Some(DatabaseProtocol::BlobLog))
        {
            return Err(Error::InvalidArgument);
        }
        let DatabaseId::Entry(entry) = named.address.database else {
            return Err(Error::InvalidArgument);
        };
        stage.accept_source(receipt.source, receipt.policy_revision)?;
        crate::store::bloblog::Log {
            database: &*named.db,
            volume: stage.volume.id,
            entry,
            encryption_key: None,
            now: unsafe { now() } as u64,
        }
        .apply_page(&receipt.page)
        .map_err(store_error)?;
        *stage.log_validation.borrow_mut() = None;
        stage.log_complete.set(false);
        stage.validated.set(false);
        stage.backing_received.set(true);
        serde_json::to_vec(&receipt.page.cursor).map_err(map_error)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamespaceEvidence {
    slot: u32,
    prepared: bool,
    entry: Option<EntryId>,
    receipt: crate::store::ReplicaReceipt,
}
/// The shared input contains bounded JSON metadata followed by binary evidence.
/// Rust owns root, scope, digest and source-identity checks for the web host too.
#[unsafe(no_mangle)]
pub extern "C" fn pb_namespace_evidence(length: usize, metadata_length: usize) -> i32 {
    call(length, |host, bytes| {
        if metadata_length > 4096 || metadata_length > bytes.len() {
            return Err(Error::InvalidArgument);
        }
        let input: NamespaceEvidence =
            serde_json::from_slice(&bytes[..metadata_length]).map_err(map_error)?;
        if !input.prepared {
            if host.database_address(input.slot as usize)?.database != DatabaseId::Index {
                return Err(Error::AccessDenied);
            }
            // Reconnect first merges into the live index, which may retain
            // offline sources absent from AP. Validate the transport evidence,
            // but never turn this mixed cut into confirmed observations.
            input
                .receipt
                .namespace
                .verify(input.entry, &bytes[metadata_length..])
                .map_err(store_error)?;
            return Ok(Vec::new());
        }
        let stage = prepared()?;
        let (_, address) = stage.database(input.slot)?;
        if address.database != DatabaseId::Index
            || stage.entry_scope.map(|scope| scope.0) != input.entry
        {
            return Err(Error::AccessDenied);
        }
        stage.policy(host, DatabaseId::Index)?;
        stage.accept_source(input.receipt.source, input.receipt.durable.policy_revision)?;
        volume_index::namespace::receipt::retain(
            &stage.accepted_index,
            &input.receipt,
            input.entry,
            &bytes[metadata_length..],
        )
        .map_err(store_error)?;
        *stage.namespace_receipt.borrow_mut() = Some(input.receipt);
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_receipt(length: usize) -> i32 {
    call(length, |host, bytes| {
        let receipt: Receipt = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        let (_, address) = stage.database(receipt.slot)?;
        stage.policy(host, address.database)?.ordinary_protocol().map_err(store_error)?;
        stage.accept_source(receipt.receipt.source, receipt.receipt.durable.policy_revision)?;
        match address.database {
            DatabaseId::Index => stage.controls_received.set(stage.controls_received.get() | 1),
            DatabaseId::Revocations => {
                stage.controls_received.set(stage.controls_received.get() | 2)
            }
            DatabaseId::Entry(_) => stage.backing_received.set(true),
        }
        stage.validated.set(false);
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_qualify(length: usize) -> i32 {
    call(length, |host, bytes| {
        let receipt: crate::store::ReplicaReceipt =
            serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        if stage.named.borrow().is_some()
            || stage.controls_received.get() != 3
            || stage.local_backing_used.get()
            || stage.source_cut.borrow().is_none()
        {
            return Err(Error::Unavailable);
        }
        stage.policy(host, DatabaseId::Index)?;
        stage.accept_source(receipt.source, receipt.durable.policy_revision)?;
        if matches!(receipt.namespace, volume_index::namespace::receipt::Receipt::Accepted { .. }) {
            if stage.namespace_receipt.borrow().as_ref() != Some(&receipt) {
                return Err(Error::AccessDenied);
            }
            namespace::qualify(&stage, host.account, true).map_err(store_error)?;
        } else {
            if stage.namespace_receipt.borrow().is_some() {
                return Err(Error::AccessDenied);
            }
            namespace::qualify(&stage, host.account, false).map_err(store_error)?;
        }
        stage.authority_qualified.set(true);
        stage.validated.set(false);
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_log_validate() -> i32 {
    call(0, |host, _| {
        let stage = prepared()?;
        let named = stage.named.borrow();
        let named = named.as_ref().ok_or(Error::Unavailable)?;
        let policy = stage.policy(host, named.address.database)?;
        if !policy
            .entry
            .as_ref()
            .is_some_and(|entry| entry.protocol == Some(DatabaseProtocol::BlobLog))
        {
            return Err(Error::InvalidArgument);
        }
        let DatabaseId::Entry(entry) = named.address.database else {
            return Err(Error::InvalidArgument);
        };
        if !stage.log_complete.get() {
            let after = crate::store::bloblog::Log {
                database: &*named.db,
                volume: stage.volume.id,
                entry,
                encryption_key: None,
                now: unsafe { now() } as u64,
            }
            .validate_page(stage.log_validation.borrow().as_deref(), 64)
            .map_err(store_error)?;
            if after.is_none() {
                let mut tx =
                    stage.volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
                volume_index::mark_source_backing_ready(&mut tx, entry).map_err(store_error)?;
                tx.commit().map_err(map_error)?;
                stage.log_complete.set(true);
            }
            *stage.log_validation.borrow_mut() = after;
        }
        serde_json::to_vec(&stage.log_complete.get()).map_err(map_error)
    })
}

pub(super) fn begin_sync(slot: u32) -> Result<(Rc<Database>, Address)> {
    let stage = prepared()?;
    if stage.archive.borrow().started() {
        return Err(Error::Unavailable);
    }
    let host = HOST.with(|cell| cell.borrow().clone()).ok_or(Error::Unavailable)?;
    let (db, address) = stage.database(slot)?;
    stage.policy(&host, address.database)?.ordinary_protocol().map_err(store_error)?;
    match address.database {
        DatabaseId::Index => {
            stage.controls_received.set(stage.controls_received.get() & !1);
            stage.namespace_receipt.borrow_mut().take();
        }
        DatabaseId::Revocations => stage.controls_received.set(stage.controls_received.get() & !2),
        DatabaseId::Entry(_) => stage.backing_received.set(false),
    }
    stage.validated.set(false);
    // The transfer must start from the exact received projection. Comparing
    // against offline data can omit equal sources and cannot bind its root.
    let db = if address.database == DatabaseId::Index { stage.accepted_index.clone() } else { db };
    Ok((db, address))
}
pub(super) fn authorize_transfer(host: &Host, address: &Address) -> Result<()> {
    let stage = prepared()?;
    if stage.volume.id != address.volume {
        return Err(Error::AccessDenied);
    }
    stage.policy(host, address.database)?.ordinary_protocol().map_err(store_error)
}
pub(super) fn merge(
    host: &Host,
    address: &Address,
    database: &Database,
    changes: kvdb::sync::PreparedChanges<OpfsStaging>,
) -> Result<()> {
    let stage = prepared()?;
    let policy = stage.policy(host, address.database)?;
    stage.authority_qualified.set(false);
    stage.validated.set(false);
    if address.database == DatabaseId::Index {
        merge_accepted_index(host, &stage, &changes, &policy)?;
    }
    if let DatabaseId::Entry(entry) = address.database {
        return backings::merge(host, &stage, entry, database, &changes, &policy);
    }
    let working =
        if address.database == DatabaseId::Index { &stage.volume.index } else { database };
    host.merge_prepared_in(
        &stage.volume,
        address,
        working,
        &changes,
        &policy,
        stage.entry_scope.map(|(entry, _)| entry),
    )
}

fn merge_accepted_index(
    host: &Host,
    stage: &Preparation,
    changes: &kvdb::sync::PreparedChanges<OpfsStaging>,
    policy: &DatabasePolicy,
) -> Result<()> {
    use kvdb::sync::{Resolution, SyncEntState};
    let rejected = RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if let Some((entry, _)) = stage.entry_scope
            && live::selected_key(host, DatabaseId::Index, entry, delta.key).is_err()
        {
            if !matches!(delta.remote, SyncEntState::Missing) {
                *rejected.borrow_mut() = Some(Error::AccessDenied);
            }
            return Ok(Resolution::KeepLocal);
        }
        if !matches!(delta.remote, SyncEntState::Missing) {
            let admission = if volume_index::is_index_key(delta.key) {
                volume_index::validate_sync_source(delta).map(|_| ())
            } else {
                policy.validate_sync_source(delta, None).map(|_| ())
            };
            if let Err(error) = admission {
                *rejected.borrow_mut() = Some(store_error(error));
                return Ok(Resolution::KeepLocal);
            }
        }
        Ok(Resolution::UseRemote)
    };
    let merged = changes
        .merge(&stage.accepted_index, resolver, unsafe { now() } as u64, 0)
        .map_err(map_error)?;
    if let Some(error) = rejected.borrow_mut().take() {
        return Err(error);
    }
    MetadataPolicy::validate_pending(
        merged.pending_transaction().map_err(map_error)?,
        MetadataPolicy::MAX_MATERIALIZED_BYTES,
    )
    .map_err(store_error)?;
    merged
        .repair_local_indexes(volume_index::repair_all)
        .map_err(store_error)?
        .finish_with_delta()
        .map_err(map_error)?;
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_validate() -> i32 {
    call(0, |host, _| {
        prepared()?.validate(host)?;
        Ok(Vec::new())
    })
}

/// One bounded page of locally winning facts rejected by the received AP ACL.
/// This is preparation data only; qualification is required before publication.
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_rejected_page(length: usize) -> i32 {
    call(length, |host, bytes| {
        let after: Option<EntryId> = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        stage.current_authority(host, DatabaseId::Index)?;
        if stage.controls_received.get() & 1 == 0 {
            return Err(Error::Unavailable);
        }
        let policy = MetadataPolicy::read(&stage.accepted_index).map_err(store_error)?;
        let current = stage.original.index.begin_read().map_err(map_error)?;
        let snapshot = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?);
        let page = if let Some((entry, _)) = stage.entry_scope {
            if after.is_some() {
                return Err(Error::InvalidArgument);
            }
            let entries = match snapshot.source_header(entry) {
                Ok(header) => vec![header],
                Err(crate::store::Error::NotFound(_)) => Vec::new(),
                Err(error) => return Err(store_error(error)),
            };
            volume_index::HeaderPage { entries, next: None }
        } else {
            snapshot.source_headers(after, 64).map_err(store_error)?
        };
        let mut rejected = Vec::new();
        for entry in page.entries {
            if stage.entry_scope.is_some_and(|(selected, _)| selected != entry.id) {
                continue;
            }
            if backings::rejected(&stage, host.account, entry.id)?
                || volume_index::rejected_sources(
                    &current,
                    &stage.accepted_index,
                    &policy,
                    host.account,
                    entry.id,
                )
                .map_err(store_error)?
            {
                rejected.push(entry.id);
            }
        }
        serde_json::to_vec(&(rejected, page.next)).map_err(map_error)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_publish() -> i32 {
    call(0, |host, _| {
        let stage = prepared()?;
        if !stage.validated.get() {
            return Err(Error::InvalidArgument);
        }
        stage.current_authority(host, DatabaseId::Index)?;
        host.changes
            .borrow_mut()
            .retire_backings(stage.volume.id, &stage.archive.borrow().rejected_backings);
        host.named.borrow_mut().retain(|named| named.address.volume != stage.volume.id);
        host.named_failures.borrow_mut().retain(|(address, _)| address.volume != stage.volume.id);
        storage::invalidate();
        let mut primary = host.volumes.borrow_mut();
        if let Some(current) = primary.iter_mut().find(|volume| volume.id == stage.volume.id) {
            *current = stage.volume.clone();
        } else {
            let mut personal = host.personal.borrow_mut();
            let current = personal
                .iter_mut()
                .find(|volume| volume.id == stage.volume.id)
                .ok_or(Error::AccessDenied)?;
            *current = stage.volume.clone();
        }
        host.volume_objects.borrow_mut().insert(stage.volume.id, stage.volume.clone());
        PREPARED.with(|cell| cell.borrow_mut().take());
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_abort() -> i32 {
    call(0, |_, _| {
        PREPARED.with(|cell| cell.borrow_mut().take());
        Ok(Vec::new())
    })
}

fn catalog_entry(
    host: &Host,
    stage: &Preparation,
    index: &Database,
    snapshot: &IndexSnapshot<'_, OpfsStorage>,
    entry: EntryId,
    copy_local: bool,
) -> Result<Vec<(EntryId, DatabaseProtocol)>> {
    let mut backings = Vec::with_capacity(1);
    match if copy_local { snapshot.header(entry) } else { snapshot.source_header(entry) } {
        Ok(info) => {
            if let Some(protocol) = info.protocol {
                if copy_local && snapshot.backing_ready(entry).map_err(store_error)? {
                    backings.push((entry, protocol));
                } else if !copy_local && !info.deleted {
                    let policy = DatabasePolicy::read_source(index, DatabaseId::Entry(entry))
                        .map_err(store_error)?;
                    if policy.authorize_reader(ReplicaReader::Participant(host.account)).is_ok() {
                        stage.policy(host, DatabaseId::Entry(entry))?;
                        backings.push((entry, protocol));
                    }
                }
            }
        }
        Err(crate::store::Error::NotFound(_)) if copy_local => {}
        Err(error) => return Err(store_error(error)),
    }
    Ok(backings)
}
