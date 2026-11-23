use super::*;
use crate::{
    engine::{Request, reply::Reply},
    store::{key_value, volume_index::EntryAddress},
};

#[derive(Clone)]
pub(crate) struct RenameSource {
    pub old_key: Vec<u8>,
    pub expected: key_value::Root,
}

impl RenameSource {
    pub(in crate::desktop) fn retained_bytes(&self, call: &DataCall) -> Option<usize> {
        let DataOperation::KvPut { key, .. } = &call.operation else { return None };
        key_value::validate_key(&self.old_key).ok()?;
        if key == &self.old_key {
            return None;
        }
        // Original key, owned IPC request and framed request may coexist.
        self.old_key
            .len()
            .checked_mul(3)?
            .checked_add(3 * std::mem::size_of::<key_value::Root>())?
            .checked_add(std::mem::size_of::<crate::engine::KvRenameMutation>())
    }
}

pub(super) fn execute(
    profile: &Path,
    session: Option<[u8; 32]>,
    call: &DataCall,
    source: &RenameSource,
) -> Result<key_value::Root, DataFailure> {
    let result: crate::engine::Result<key_value::Root> = (|| {
        let DataOperation::KvPut { entry_id, key, value } = &call.operation else {
            return Err("Invalid desktop KV rename effect".into());
        };
        let address = EntryAddress {
            volume: Id::from_bytes(call.volume),
            entry: EntryId::from_bytes(*entry_id)?,
        };
        let request = Request::KvRename {
            session: session.map(Box::new),
            mutation: Box::new(crate::engine::KvRenameMutation {
                address,
                old_key: source.old_key.clone(),
                new_key: key.clone(),
                expected: source.expected,
                value: value.clone(),
            }),
        };
        // call_archive verifies exact address and both keys before materializing.
        let Reply::KvRenamed { receipt } = ipc::call_archive(profile, &request)?.into_owned()?
        else {
            return Err("Unexpected KV rename receipt".into());
        };
        Ok(receipt.root)
    })();
    result.map_err(|error| DataFailure {
        status: error
            .downcast_ref::<ipc::RejectedReply>()
            .map(|_| playbit_api::abi::syscall::Error::Conflict),
        message: error.to_string(),
    })
}
