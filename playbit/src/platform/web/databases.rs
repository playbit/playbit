//! Owned named-backing cache. Persistent index records supply identity and rights.
use super::*;
use crate::store::{
    InitialDatabase,
    volume_index::{self, DatabaseProtocol, EntryInfo, IndexSnapshot},
};

pub(super) const NAMED_LIMIT: usize = 27;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Address {
    pub volume: Id,
    pub database: DatabaseId,
}
pub(super) struct Named {
    pub address: Address,
    pub slot: u32,
    pub db: Rc<Database>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    address: Address,
    slot: u32,
    replica_id: u64,
    generation: Id,
}
thread_local! {
    static NEEDED: RefCell<Vec<Address>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn open(slot: u32, key: [u8; 32], replica_id: u64) -> Result<Rc<Database>> {
    if replica_id == 0 {
        return Err(Error::InvalidArgument);
    }
    let db = Db::open_or_create(
        OpfsStorage(slot),
        &CreateOptions::syncable(key, replica_id),
        &Default::default(),
    )
    .map_err(map_error)?;
    if !db.meta().syncable || db.meta().hash_key != key {
        return Err(map_error("database identity does not match its volume and entry"));
    }
    Ok(Rc::new(db))
}
impl Volume {
    pub(super) fn open(
        record: VolumeRecord,
        key: [u8; 32],
        base: u32,
        replicas: &[u64],
    ) -> Result<Self> {
        if replicas.len() != 2 {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            id: record.id,
            key,
            index: open(base, DatabaseId::Index.hash_key(&key, record.id), replicas[0])?,
            revocations: open(
                base + 1,
                DatabaseId::Revocations.hash_key(&key, record.id),
                replicas[1],
            )?,
            record,
        })
    }
}
impl Host {
    pub(super) fn discard_named_backings(&self, id: Id) {
        self.named.borrow_mut().retain(|named| named.address.volume != id);
        self.named_failures.borrow_mut().retain(|(address, _)| address.volume != id);
        NEEDED.with(|needed| needed.borrow_mut().retain(|address| address.volume != id));
    }

    pub(super) fn replica_volume(&self, id: Id) -> Result<Rc<Volume>> {
        self.volume_objects.borrow().get(&id).cloned().ok_or(Error::AccessDenied)
    }

    fn control(&self, slot: usize) -> Result<Option<(Rc<Volume>, DatabaseId)>> {
        if slot == 0 || slot > 62 {
            return Ok(None);
        }
        let base = ((slot - 1) & !1) as u32;
        let volume = if base == 0 {
            self.volumes.borrow()[0].clone()
        } else {
            let bases = self.secondary_bases.borrow();
            let Some((id, _)) = bases.iter().find(|(_, offset)| **offset == base) else {
                return Ok(None);
            };
            self.replica_volume(*id)?
        };
        Ok(Some((volume, if slot % 2 == 1 { DatabaseId::Index } else { DatabaseId::Revocations })))
    }

    pub(super) fn database(&self, slot: usize) -> Result<Rc<Database>> {
        if let Some((volume, db)) = self.control(slot)? {
            return Ok(if db == DatabaseId::Index {
                volume.index.clone()
            } else {
                volume.revocations.clone()
            });
        }
        self.named
            .borrow()
            .iter()
            .find(|db| db.slot as usize == slot)
            .map(|db| db.db.clone())
            .ok_or(Error::NotFound)
    }
    pub(super) fn database_address(&self, slot: usize) -> Result<Address> {
        if let Some((volume, database)) = self.control(slot)? {
            return Ok(Address { volume: volume.id, database });
        }
        self.named
            .borrow()
            .iter()
            .find(|db| db.slot as usize == slot)
            .map(|db| db.address.clone())
            .ok_or(Error::NotFound)
    }
    pub(super) fn entry_database(&self, volume: &Volume, entry: EntryId) -> Result<Rc<Database>> {
        if IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?)
            .backing_quarantined(entry)
            .map_err(store_error)?
        {
            return Err(Error::Unavailable);
        }
        let address = Address { volume: volume.id, database: DatabaseId::Entry(entry) };
        if let Some((_, error)) =
            self.named_failures.borrow().iter().find(|(failed, _)| *failed == address)
        {
            return Err(*error);
        }
        let mut named = self.named.borrow_mut();
        if let Some(index) = named.iter().position(|db| db.address == address) {
            let db = named.remove(index);
            let result = db.db.clone();
            named.push(db);
            return Ok(result);
        }
        self.need_entry(volume, entry)?;
        Err(Error::WouldBlock)
    }
    pub(super) fn need_entry(&self, volume: &Volume, entry: EntryId) -> Result<()> {
        let address = Address { volume: volume.id, database: DatabaseId::Entry(entry) };
        if let Some((_, error)) =
            self.named_failures.borrow().iter().find(|(failed, _)| *failed == address)
        {
            return Err(*error);
        }
        NEEDED.with(|needed| {
            let mut needed = needed.borrow_mut();
            if !needed.contains(&address) {
                if needed.len() == NAMED_LIMIT {
                    return Err(Error::LimitExceeded);
                }
                needed.push(address);
            }
            Ok(())
        })
    }

    pub(super) fn authorize_entry_read(&self, volume: &Volume, entry: EntryId) -> Result<()> {
        // Unknown metadata can be fetched only through an AP-authorized scoped
        // capture. No source facts or read result are visible before admission.
        self.admit_volume(volume, Permission::Read)?;
        if !volume
            .index
            .begin_read()
            .map_err(map_error)?
            .has(&volume_index::entry_key(entry))
            .map_err(map_error)?
        {
            self.need_entry(volume, entry)?;
            return Err(Error::WouldBlock);
        }
        self.authorize(volume, Some(entry), Permission::Read)?;
        Ok(())
    }

    fn authorize_local_world_mutation(
        &self,
        volume: &Volume,
        snapshot: &IndexSnapshot<'_, OpfsStorage>,
        entry: EntryId,
    ) -> Result<()> {
        if !self.is_projected_world(volume)
            || !snapshot.backing_ready(entry).map_err(store_error)?
        {
            return Err(Error::AccessDenied);
        }
        let policy = snapshot.entry_policy(entry).map_err(store_error)?;
        if policy.kind != crate::store::EntryType::KvDb
            || policy.protocol != Some(DatabaseProtocol::Kvdb)
        {
            return Err(Error::AccessDenied);
        }
        match self.authorize(volume, Some(entry), Permission::Write) {
            Err(Error::AccessDenied) => self.authorize(volume, Some(entry), Permission::Append)?,
            other => other?,
        };
        Ok(())
    }

    fn make_room(&self) -> Result<()> {
        let mut named = self.named.borrow_mut();
        if named.len() < NAMED_LIMIT {
            return Ok(());
        }
        let index =
            named.iter().position(|db| Rc::strong_count(&db.db) == 1).ok_or(Error::WouldBlock)?;
        named.remove(index);
        Ok(())
    }

    fn failed_database(&self, address: Address, status: i32) -> Result<()> {
        if !matches!(address.database, DatabaseId::Entry(_)) {
            return Err(Error::InvalidArgument);
        }
        self.replica_volume(address.volume)?;
        let error = Error::try_from(status).map_err(|_| Error::InvalidArgument)?;
        if error == Error::WouldBlock || error == Error::BufferTooSmall {
            return Err(Error::InvalidArgument);
        }
        let mut failures = self.named_failures.borrow_mut();
        if let Some((_, previous)) = failures.iter_mut().find(|(failed, _)| *failed == address) {
            *previous = error;
        } else {
            if failures.len() == NAMED_LIMIT {
                return Err(Error::LimitExceeded);
            }
            failures.push((address, error));
        }
        Ok(())
    }
    fn register(&self, registration: Register) -> Result<()> {
        if registration.slot <= 64 {
            return Err(Error::InvalidArgument);
        }
        if self
            .named
            .borrow()
            .iter()
            .any(|db| db.address == registration.address || db.slot == registration.slot)
        {
            return Err(Error::Conflict);
        }
        self.make_room()?;
        let volume = self.backing_volume(&registration.address)?;
        let DatabaseId::Entry(entry) = registration.address.database else {
            return Err(Error::InvalidArgument);
        };
        if backing::location(&volume, entry)?
            .is_some_and(|current| current != registration.generation)
        {
            return Err(Error::Conflict);
        }
        let db = open(
            registration.slot,
            registration.address.database.hash_key(&volume.key, volume.id),
            registration.replica_id,
        )?;
        if backing::location(&volume, entry)?.is_none() {
            backing::bind(&volume, entry, registration.generation)?;
        }
        self.named.borrow_mut().push(Named {
            address: registration.address,
            slot: registration.slot,
            db,
        });
        Ok(())
    }
    fn backing_volume(&self, address: &Address) -> Result<Rc<Volume>> {
        let volume = self.replica_volume(address.volume)?;
        let DatabaseId::Entry(entry) = address.database else {
            return Err(Error::InvalidArgument);
        };
        let allowed_seed = self.account == volume.record.owner
            && self
                .initializer_for(volume.id)
                .as_deref()
                .is_some_and(|init| init.databases.iter().any(|seed| seed.id == entry));
        if !allowed_seed && self.replica_policy(&volume, address.database, true).is_err() {
            match self.authorize(&volume, Some(entry), Permission::Read) {
                Err(Error::AccessDenied) => {
                    match self.authorize(&volume, Some(entry), Permission::Write) {
                        Err(Error::AccessDenied) => {
                            let snapshot =
                                IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
                            self.authorize_local_world_mutation(&volume, &snapshot, entry)?;
                        }
                        other => {
                            other?;
                        }
                    }
                }
                other => {
                    other?;
                }
            }
        }
        Ok(volume)
    }
    fn seed(&self, volume: Id, seed: &InitialDatabase) -> Result<()> {
        let volume = self.replica_volume(volume)?;
        if self.account != volume.record.owner {
            return Err(Error::AccessDenied);
        }
        let initializer = self.initializer_for(volume.id).ok_or(Error::AccessDenied)?;
        let expected = initializer
            .databases
            .iter()
            .find(|item| item.id == seed.id)
            .ok_or(Error::AccessDenied)?;
        if serde_json::to_vec(seed).map_err(map_error)?
            != serde_json::to_vec(expected).map_err(map_error)?
        {
            return Err(Error::AccessDenied);
        }
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        match snapshot.entry(seed.id) {
            Ok(_) => return Ok(()),
            Err(crate::store::Error::NotFound(_)) => {}
            Err(error) => return Err(map_error(error)),
        }
        drop(snapshot);
        let backing = self.entry_database(&volume, seed.id)?;
        if !seed.prepared_objects(&backing, self.account).map_err(store_error)? {
            seed.write_objects(&backing, self.account, unsafe { now() } as u64)
                .map_err(store_error)?;
        }
        let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
        volume_index::create_at_path(
            &mut tx,
            seed.id,
            &seed.name,
            &EntryInfo {
                namespace: Default::default(),
                origin: crate::store::volume_index::namespace::Origin {
                    installation: self.installation,
                    label: None,
                },
                initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                created_revision: random_entry()?,
                name: seed.name.rsplit('/').next().expect("validated seed path").to_owned(),
                kind: seed.entry_kind(),
                protocol: Some(seed.protocol()),
            },
        )
        .map_err(map_error)?;
        volume_index::mark_backing_ready(&mut tx, seed.id).map_err(map_error)?;
        tx.commit().map_err(map_error)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_database_failed(length: usize) -> i32 {
    crate::platform::failure::guard();
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Failure {
        address: Address,
        status: i32,
    }
    let input = match admit_input(length, 512) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    HOST.with(|host| {
        let host = host.borrow();
        host.as_ref()
            .ok_or(Error::Unavailable)
            .and_then(|host| {
                let failure: Failure =
                    serde_json::from_slice(&input[..length]).map_err(map_error)?;
                host.failed_database(failure.address, failure.status)
            })
            .map_or_else(|error| error as i32, |()| 0)
    })
}

/// Explicit recovery only. Failed openings stay terminal on ordinary app turns.
#[unsafe(no_mangle)]
pub extern "C" fn pb_database_retry() -> i32 {
    crate::platform::failure::guard();
    HOST.with(|host| {
        let host = host.borrow();
        host.as_ref()
            .ok_or(Error::Unavailable)
            .map(|host| {
                host.named_failures.borrow_mut().clear();
                host.retry_automatic();
            })
            .map_or_else(|error| error as i32, |()| 0)
    })
}

pub(super) fn random_entry() -> Result<EntryId> {
    let mut bytes = [0; 15];
    unsafe {
        random(bytes.as_mut_ptr(), bytes.len());
    }
    EntryId::from_bytes(bytes).map_err(map_error)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_database_needed() -> i32 {
    crate::platform::failure::guard();
    NEEDED.with(|needed| {
        match serde_json::to_vec(&std::mem::take(&mut *needed.borrow_mut())).map_err(map_error) {
            Ok(bytes) => publish_output(bytes),
            Err(error) => error as i32,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_database_preparation(length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, 512)?;
        let address: Address = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        let volume = host.replica_volume(address.volume)?;
        let DatabaseId::Entry(entry) = address.database else {
            return Err(Error::InvalidArgument);
        };
        host.admit_volume(&volume, Permission::Read)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let kind = match snapshot.live_entry(entry) {
            Ok(_) => {
                match host.authorize(&volume, Some(entry), Permission::Read) {
                    Err(Error::AccessDenied) => {
                        host.authorize_local_world_mutation(&volume, &snapshot, entry)?;
                        return serde_json::to_vec("local").map_err(map_error);
                    }
                    other => {
                        other?;
                    }
                }
                if snapshot.backing_ready(entry).map_err(store_error)? {
                    "local"
                } else if host.authorize(&volume, None, Permission::Read).is_ok() {
                    "volume"
                } else {
                    "entry"
                }
            }
            Err(crate::store::Error::NotFound("entry")) => {
                if host.authorize(&volume, None, Permission::Read).is_ok() {
                    "volume"
                } else {
                    "entry"
                }
            }
            Err(error) => return Err(store_error(error)),
        };
        serde_json::to_vec(kind).map_err(map_error)
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_database_make_room() -> i32 {
    crate::platform::failure::guard();
    HOST.with(|host| {
        host.borrow()
            .as_ref()
            .ok_or(Error::Unavailable)
            .and_then(|host| host.make_room())
            .map_or_else(|error| error as i32, |()| 0)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_database_backing(length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, 512)?;
        let address: Address = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        let volume = host.backing_volume(&address)?;
        let DatabaseId::Entry(entry) = address.database else {
            return Err(Error::InvalidArgument);
        };
        let ready = match IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?)
            .backing_ready(entry)
        {
            Ok(ready) => ready,
            // Trusted root seeds may open an empty backing before declaration.
            // backing_volume already admitted the exact initializer identity.
            Err(crate::store::Error::NotFound(_)) => false,
            Err(error) => return Err(store_error(error)),
        };
        serde_json::to_vec(&serde_json::json!({
            "ready": ready,
            "generation": backing::location(&volume, entry)?
        }))
        .map_err(map_error)
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_database_retained_generation(length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, 512)?;
        let address: Address = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let DatabaseId::Entry(entry) = address.database else {
            return Err(Error::InvalidArgument);
        };
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        // Trusted OPFS housekeeping only; never exported through the guest ABI.
        // Unreadable and retired entries must still retain their physical data.
        let volume = host.replica_volume(address.volume)?;
        serde_json::to_vec(&backing::location(&volume, entry)?).map_err(map_error)
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_register_database(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 512) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    HOST.with(|host| {
        let host = host.borrow();
        host.as_ref()
            .ok_or(Error::Unavailable)
            .and_then(|host| {
                host.register(serde_json::from_slice(&input[..length]).map_err(map_error)?)
            })
            .map_or_else(|error| error as i32, |()| 0)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_seed_database(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input =
        match admit_input(length, crate::store::initialization::MAX_ROOT_INITIALIZATION_BYTES) {
            Ok(input) => input,
            Err(error) => return error as i32,
        };
    HOST.with(|host| {
        let host = host.borrow();
        host.as_ref()
            .ok_or(Error::Unavailable)
            .and_then(|host| {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum Request {
                    Scoped { volume: Id, seed: InitialDatabase },
                    Primary(InitialDatabase),
                }
                match serde_json::from_slice(&input[..length]).map_err(map_error)? {
                    Request::Scoped { volume, seed } => host.seed(volume, &seed),
                    Request::Primary(seed) => host.seed(host.volumes.borrow()[0].id, &seed),
                }
            })
            .map_or_else(|error| error as i32, |()| 0)
    })
}
