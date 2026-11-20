//! Backend-independent root declarations and transactional seed construction.
//! This is trusted host setup, not an app authorization endpoint. The host keeps
//! all new databases private until catalog, credentials and receipt publication
//! succeeds. Callers supply generated IDs before opening the short seed writer.
use super::{
    EntryId, Error, Id, Result,
    object_contract::{Acl, Schema, Value},
    object_database,
    volume_index::{DatabaseProtocol, EntryKind, validate_name},
};
use kvdb::{Db, Storage};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

/// Includes the schema, ACL and all seed values in canonical JSON form.
pub const MAX_ROOT_INITIALIZATION_BYTES: usize = 64 * 1024;
/// Covers the existing 1/32/64-row native qualification fixtures.
pub const MAX_ROOT_INITIAL_OBJECTS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RootInitialization {
    pub permissions: Acl,
    pub databases: Vec<InitialDatabase>,
    pub blobs: Vec<InitialBlob>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct InitialBlob {
    pub id: EntryId,
    pub name: String,
    pub bytes: Vec<u8>,
}
/// Template identities are stable within each new volume. Cross-volume addressing
/// includes the volume ID, so copies of one template remain independent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct InitialDatabase {
    pub id: EntryId,
    pub name: String,
    pub kind: InitialDatabaseKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum InitialDatabaseKind {
    KvDb {},
    ObjectDb { schema: Schema, objects: Vec<InitialObject> },
    BlobLog {},
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct InitialObject {
    pub id: Id,
    pub type_id: Id,
    pub fields: BTreeMap<Id, Vec<Value>>,
}

impl RootInitialization {
    /// Read a declaration only after bounding its source bytes. Typed callers
    /// receive the same canonical-size check at initialization admission.
    // fuzz: store_contract
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ROOT_INITIALIZATION_BYTES {
            return Err(Error::LimitExceeded);
        }
        let declaration: Self = serde_json::from_slice(bytes)
            .map_err(|_| Error::InvalidInput("invalid root initialization JSON"))?;
        declaration.fingerprint()?;
        Ok(declaration)
    }
    pub(crate) fn fingerprint(&self) -> Result<[u8; 32]> {
        if self.databases.len().checked_add(self.blobs.len()).is_none_or(|count| count > 64) {
            return Err(Error::LimitExceeded);
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut objects = 0usize;
        for database in &self.databases {
            validate_name(&database.name)?;
            if !ids.insert(database.id) || !names.insert(&database.name) {
                return Err(Error::InvalidInput("initial databases need distinct IDs and names"));
            }
            if let InitialDatabaseKind::ObjectDb { schema, objects: seeds } = &database.kind {
                schema.validate()?;
                objects = objects.checked_add(seeds.len()).ok_or(Error::LimitExceeded)?;
                if objects > MAX_ROOT_INITIAL_OBJECTS {
                    return Err(Error::LimitExceeded);
                }
                let mut object_ids = BTreeSet::new();
                for object in seeds {
                    if object.id == Id::from_bytes([0; 16]) || !object_ids.insert(object.id) {
                        return Err(Error::InvalidInput(
                            "initial objects need distinct nonzero IDs",
                        ));
                    }
                    if !schema.types.contains_key(&object.type_id) {
                        return Err(Error::NotFound("initial object type"));
                    }
                    object.validate_fields(schema)?;
                }
            }
        }
        for blob in &self.blobs {
            validate_name(&blob.name)?;
            if !ids.insert(blob.id) || !names.insert(&blob.name) {
                return Err(Error::InvalidInput("initial entries need distinct IDs and names"));
            }
            if blob.bytes.len() > MAX_ROOT_INITIALIZATION_BYTES {
                return Err(Error::LimitExceeded);
            }
        }
        let mut writer = DeclarationHash {
            remaining: MAX_ROOT_INITIALIZATION_BYTES,
            hash: blake3::Hasher::new(),
        };
        serde_json::to_writer(&mut writer, self).map_err(|_| Error::LimitExceeded)?;
        self.permissions.validate()?;
        Ok(*writer.hash.finalize().as_bytes())
    }
}

impl InitialObject {
    fn validate_fields(&self, schema: &Schema) -> Result<()> {
        let fields = self.fields.iter().map(|(id, values)| (*id, Some(values.clone()))).collect();
        // Field admission needs a nonzero edit origin, independent of the later
        // account and transaction clock that will publish the actual seed.
        object_database::prepare_creation(
            schema,
            self.id,
            self.type_id,
            self.id,
            kvdb::Clock::from_u64(1),
            &fields,
        )
        .map(|_| ())
    }
}

struct DeclarationHash {
    remaining: usize,
    hash: blake3::Hasher,
}
fn reserve_bytes(remaining: &mut usize, bytes: usize) -> bool {
    if bytes > *remaining {
        return false;
    }
    *remaining -= bytes;
    true
}
impl Write for DeclarationHash {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !reserve_bytes(&mut self.remaining, bytes.len()) {
            return Err(std::io::Error::other("root initialization exceeds 64 KiB"));
        }
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl InitialDatabase {
    pub fn entry_kind(&self) -> EntryKind {
        match &self.kind {
            InitialDatabaseKind::ObjectDb { schema, .. } => {
                EntryKind::ObjectDb { schema: schema.clone() }
            }
            InitialDatabaseKind::KvDb {} | InitialDatabaseKind::BlobLog {} => EntryKind::KvDb {},
        }
    }
    pub fn protocol(&self) -> DatabaseProtocol {
        match self.kind {
            InitialDatabaseKind::BlobLog {} => DatabaseProtocol::BlobLog,
            _ => DatabaseProtocol::Kvdb,
        }
    }
    /// All objects are complete immutable births in one private backing writer.
    /// Publication of its index entry and credentials belongs to the host.
    pub fn write_objects(
        &self,
        database: &Db<impl Storage>,
        owner: Id,
        wall_time: u64,
    ) -> Result<()> {
        let mut tx = database.begin_write(wall_time)?;
        tx.visit::<Error>(b"", false, |_, _| Err(Error::AlreadyExists("initial database")))?;
        if let InitialDatabaseKind::ObjectDb { schema, objects } = &self.kind {
            if objects.len() > MAX_ROOT_INITIAL_OBJECTS {
                return Err(Error::LimitExceeded);
            }
            for seed in objects {
                let fields =
                    seed.fields.iter().map(|(id, values)| (*id, Some(values.clone()))).collect();
                object_database::create(&mut tx, schema, seed.id, seed.type_id, owner, &fields)?;
            }
        }
        let preparation = (self.preparation_digest(owner)?, tx.root_hash()?);
        tx.local_indexes().put(
            b"initialization/prepared",
            &serde_json::to_vec(&preparation)
                .map_err(|_| Error::Internal("could not encode a root preparation marker"))?,
        )?;
        tx.commit()?;
        Ok(())
    }
    fn preparation_digest(&self, owner: Id) -> Result<[u8; 32]> {
        let bytes = serde_json::to_vec(&(self, owner))
            .map_err(|_| Error::Internal("could not encode root initialization"))?;
        if bytes.len() > MAX_ROOT_INITIALIZATION_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(*blake3::hash(&bytes).as_bytes())
    }
    /// A private backing can survive a crash before index publication. Its local
    /// receipt binds both the exact declaration/owner and complete source root.
    #[cfg(any(test, target_arch = "wasm32"))]
    pub(crate) fn prepared_objects(&self, database: &Db<impl Storage>, owner: Id) -> Result<bool> {
        let snapshot = database.begin_read()?;
        let Some(bytes) = snapshot.local_get(b"initialization/prepared")? else {
            return Ok(false);
        };
        let (digest, root): ([u8; 32], super::ReplicaRoot) = serde_json::from_slice(&bytes)
            .map_err(|_| Error::corrupt("invalid root preparation marker"))?;
        if digest != self.preparation_digest(owner)? || root != snapshot.root_hash()? {
            return Err(Error::AccessDenied("prepared initialization changed"));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
