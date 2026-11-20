//! Bounded native database-browser calls. Every call uses current local-account
//! authority and the same store methods as the desktop editor and guest hosts.
use crate::store::{
    Access, DatabaseReference, EntryId, Error, Id, LocalStore, Result, key_value,
    object_contract::Value,
    volume_index::{EntryAddress, EntryKind},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_REPLY_BYTES: usize = 256 * 1024;
const PAGE_ITEMS: usize = 32;
const VALUE_BYTES: u32 = 16 * 1024;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Volumes {},
    Entries { volume: Id, parent: Option<EntryId>, after: Option<EntryId> },
    Open { reference: String },
    Keys { address: EntryAddress, after: Option<Vec<u8>>, root: Option<key_value::Root> },
    Read { address: EntryAddress, key: Vec<u8> },
    Put { address: EntryAddress, key: Vec<u8>, value: Vec<u8> },
    Insert { address: EntryAddress, key: Vec<u8>, value: Vec<u8> },
    DeleteKey { address: EntryAddress, key: Vec<u8> },
    Records { address: EntryAddress, type_id: Id, after: Option<Id> },
    Record { address: EntryAddress, object: Id },
    Edit { address: EntryAddress, object: Id, fields: BTreeMap<Id, Option<Vec<Value>>> },
    Create { address: EntryAddress, type_id: Id },
    DeleteRecord { address: EntryAddress, object: Id },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Reply {
    Volumes {
        volumes: Vec<crate::store::Volume>,
    },
    Entries {
        page: crate::store::file_provider::ProviderPage,
    },
    Database {
        volume: crate::store::Volume,
        parent: Option<EntryId>,
        details: Box<crate::store::volume_index::EntryDetails>,
    },
    Keys {
        page: key_value::Page,
    },
    Value {
        value: key_value::Chunk,
    },
    Records {
        page: crate::store::ObjectPage,
    },
    Record {
        record: crate::store::object_contract::ObjectFields,
    },
    Saved,
}

pub fn call(store: &LocalStore, bytes: &[u8]) -> (bool, Result<Vec<u8>>) {
    if bytes.len() > MAX_REQUEST_BYTES {
        return (false, Err(Error::LimitExceeded));
    }
    let request = match serde_json::from_slice(bytes).map_err(Error::decode::<Request>) {
        Ok(request) => request,
        Err(error) => return (false, Err(error)),
    };
    let changed = matches!(
        request,
        Request::Put { .. }
            | Request::Insert { .. }
            | Request::DeleteKey { .. }
            | Request::Edit { .. }
            | Request::Create { .. }
            | Request::DeleteRecord { .. }
    );
    (changed, encode_reply(execute(store, request)))
}

fn encode_reply(reply: Result<Reply>) -> Result<Vec<u8>> {
    let reply = reply?;
    let mut output = Output(Vec::new());
    serde_json::to_writer(&mut output, &reply).map_err(|_| Error::System(
        "The database response exceeds 256 KiB. The action may have completed; do not repeat it automatically.".into()
    ))?;
    Ok(output.0)
}

fn execute(store: &LocalStore, request: Request) -> Result<Reply> {
    let access = Access::owner(store.account().id);
    Ok(match request {
        Request::Volumes {} => Reply::Volumes { volumes: store.personal_volumes()? },
        Request::Entries { volume, parent, after } => {
            Reply::Entries { page: store.provider_entries(volume, parent, after, PAGE_ITEMS)? }
        }
        Request::Open { reference } => {
            open_database(store, &access, &reference).map_err(|error| match error {
                Error::NotFound(_) => Error::System(
                    "This database is unavailable on this device. Sign in to its account and open its volume in Playbit, then try again.".into(),
                ),
                Error::AccessDenied(_) => Error::System(
                    "Your account does not currently have access to this database.".into(),
                ),
                other => other,
            })?
        }
        Request::Keys { address, after, root } => Reply::Keys {
            page: store.kv_keys(&access, address, after.as_deref(), root, PAGE_ITEMS)?,
        },
        Request::Read { address, key } => {
            Reply::Value { value: store.kv_read(&access, address, &key, 0, VALUE_BYTES, None)? }
        }
        Request::Put { address, key, value } => {
            store.kv_put(&access, address, &key, &value)?;
            Reply::Saved
        }
        Request::Insert { address, key, value } => {
            store.kv_insert(&access, address, &key, &value)?;
            Reply::Saved
        }
        Request::DeleteKey { address, key } => {
            store.kv_delete(&access, address, &key)?;
            Reply::Saved
        }
        Request::Records { address, type_id, after } => Reply::Records {
            page: store.query_objects(&access, address, type_id, after, PAGE_ITEMS)?,
        },
        Request::Record { address, object } => {
            Reply::Record { record: store.read_object(&access, address, object)? }
        }
        Request::Edit { address, object, fields } => {
            Reply::Record { record: store.edit_fields(&access, address, object, &fields)? }
        }
        Request::Create { address, type_id } => {
            Reply::Record { record: store.create_object(&access, address, type_id)? }
        }
        Request::DeleteRecord { address, object } => {
            store.remove_object(&access, address, object)?;
            Reply::Saved
        }
    })
}

fn open_database(store: &LocalStore, access: &Access, reference: &str) -> Result<Reply> {
    let reference = DatabaseReference::parse(reference.as_bytes())?;
    // Credential recovery can precede the authoritative namespace. Report
    // pending initialization before validating its policy or entries.
    if store.replica_root(reference.volume, crate::store::DatabaseId::Index)?.is_none() {
        return Err(Error::System("This volume is waiting for initial synchronization.".into()));
    }
    let entry = store.entry(access, reference.volume, reference.entry)?;
    if !matches!(entry.info.kind, EntryKind::KvDb {} | EntryKind::ObjectDb { .. }) {
        return Err(Error::InvalidInput("reference is not a database"));
    }
    let parent = store.provider_parent(reference.volume, reference.entry)?;
    let details = store.entry_details(access, reference.volume, reference.entry)?;
    if details.entry.deleted || !details.readable {
        return Err(Error::AccessDenied("database Read"));
    }
    Ok(Reply::Database {
        parent,
        volume: store.volume(reference.volume)?,
        details: Box::new(details),
    })
}

struct Output(Vec<u8>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_REPLY_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("database browser reply exceeds its limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
