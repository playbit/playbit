//! Bounded, ephemeral tool transfers. Only the final shared write commits data.
//! No guest opcode, host path, timer, retry, or durable staging record exists.
use super::data_host::DataHost;
use crate::{
    runtime::host::{DataCall, DataOperation, DataResult, Host, HostResult},
    store::Id,
};
use playbit_api::abi::syscall::Error as HostError;
use std::sync::{Arc, Mutex};

pub(super) const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_STAGED_BYTES: usize = 128 * 1024 * 1024;

fn admits_bytes(used: usize, requested: usize) -> bool {
    requested <= super::blob::MAX_FILE_BYTES as usize
        && used.checked_add(requested).is_some_and(|total| total <= MAX_STAGED_BYTES)
}
fn chunk_end(total: usize, received: usize, offset: u64, length: usize) -> Option<usize> {
    if offset != received as u64 || length == 0 || length > MAX_CHUNK_BYTES {
        return None;
    }
    received.checked_add(length).filter(|end| *end <= total)
}
#[derive(Default)]
pub(super) struct Budget(Mutex<usize>);
struct Permit {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Permit {
    fn reserve(budget: &Arc<Budget>, bytes: usize) -> HostResult<Self> {
        let mut used = budget.0.lock().map_err(|_| HostError::Internal)?;
        if !admits_bytes(*used, bytes) {
            return Err(HostError::LimitExceeded);
        }
        *used += bytes;
        Ok(Self { budget: budget.clone(), bytes })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.budget.0.lock().expect("tool staging accounting must remain intact");
        *used = used.checked_sub(self.bytes).expect("staged bytes own their reserved credit");
    }
}
struct Staged {
    id: Id,
    volume: [u8; 16],
    entry: [u8; 15],
    bytes: Vec<u8>,
    received: usize,
    _permit: Permit,
}
pub(super) struct Transfers {
    budget: Arc<Budget>,
    staged: Mutex<Option<Staged>>,
}
impl Transfers {
    pub fn new(budget: Arc<Budget>) -> Self {
        Self { budget, staged: Mutex::new(None) }
    }
    pub fn execute(&self, call: DataCall, host: &mut DataHost) -> HostResult<DataResult> {
        use crate::runtime::store_bridge::{error, sid};
        match call.operation {
            DataOperation::FileTransferStart { entry_id, total_bytes } => {
                self.start_transfer(call.volume, entry_id, total_bytes, host)
            }
            DataOperation::FileTransferChunk { transfer, offset, bytes } => {
                let mut staged = self.staged.lock().map_err(|_| HostError::Internal)?;
                let staged = staged.as_mut().ok_or(HostError::StaleHandle)?;
                if staged.id != sid(transfer) || staged.volume != call.volume {
                    return Err(HostError::StaleHandle);
                }
                host.store
                    .admit_blob_mutation(
                        &host.access,
                        sid(call.volume),
                        Some(crate::store::EntryId::from_bytes(staged.entry).map_err(error)?),
                    )
                    .map_err(error)?;
                let end = chunk_end(staged.bytes.len(), staged.received, offset, bytes.len())
                    .ok_or(HostError::InvalidArgument)?;
                staged.bytes[staged.received..end].copy_from_slice(&bytes);
                staged.received = end;
                Ok(DataResult::FileTransferred { offset: end as u64 })
            }
            DataOperation::FileTransferCommit { transfer } => {
                let mut slot = self.staged.lock().map_err(|_| HostError::Internal)?;
                let staged = slot.as_ref().ok_or(HostError::StaleHandle)?;
                if staged.id != sid(transfer) || staged.volume != call.volume {
                    return Err(HostError::StaleHandle);
                }
                if staged.received != staged.bytes.len() {
                    return Err(HostError::InvalidArgument);
                }
                // Consume before writing. Even a lost/failed commit cannot run twice.
                let staged = slot.take().expect("checked transfer remains under its lock");
                drop(slot);
                host.write_file(
                    crate::runtime::host::EntryAddress {
                        volume_id: staged.volume,
                        entry_id: staged.entry,
                    },
                    &staged.bytes,
                )?;
                Ok(DataResult::Written)
            }
            _ => Err(HostError::Unsupported),
        }
    }

    fn start_transfer(
        &self,
        volume: [u8; 16],
        entry_id: [u8; 15],
        total_bytes: u64,
        host: &mut DataHost,
    ) -> HostResult<DataResult> {
        use crate::runtime::store_bridge::{error, sid};
        let total = usize::try_from(total_bytes).map_err(|_| HostError::LimitExceeded)?;
        if !admits_bytes(0, total) {
            return Err(HostError::LimitExceeded);
        }
        let mut staged = self.staged.lock().map_err(|_| HostError::Internal)?;
        if staged.is_some() {
            return Err(HostError::LimitExceeded);
        }
        host.store
            .admit_blob_mutation(
                &host.access,
                sid(volume),
                Some(crate::store::EntryId::from_bytes(entry_id).map_err(error)?),
            )
            .map_err(error)?;
        let permit = Permit::reserve(&self.budget, total)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(total).map_err(|_| HostError::LimitExceeded)?;
        if bytes.capacity() != total {
            return Err(HostError::LimitExceeded);
        }
        bytes.resize(total, 0);
        let id = Id::generate().map_err(error)?;
        *staged = Some(Staged { id, volume, entry: entry_id, bytes, received: 0, _permit: permit });
        Ok(DataResult::FileTransfer { transfer: *id.as_bytes() })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests check bounded staging and actual durable file publication."
)]
mod tests {
    use super::*;
    use crate::store::{Access, LocalStore};
    use std::sync::atomic::AtomicBool;
    struct Fixture {
        host: DataHost,
        volume: [u8; 16],
        entry: [u8; 15],
        _directory: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(LocalStore::open(directory.path()).unwrap());
            let volume = *store.create_volume("Files", false).unwrap().id.as_bytes();
            let access = Access::owner(store.account().id);
            let entry = *store
                .create_blob(&access, Id::from_bytes(volume), "file.bin", b"")
                .unwrap()
                .id
                .as_bytes();
            let host = DataHost { access, store, changed: Arc::new(AtomicBool::new(false)) };
            Self { host, volume, entry, _directory: directory }
        }
        fn address(&self) -> crate::runtime::host::EntryAddress {
            crate::runtime::host::EntryAddress { volume_id: self.volume, entry_id: self.entry }
        }

        fn call(
            &mut self,
            transfers: &Transfers,
            operation: DataOperation,
        ) -> HostResult<DataResult> {
            transfers.execute(DataCall { volume: self.volume, operation }, &mut self.host)
        }
        fn start(&mut self, transfers: &Transfers, total: u64) -> [u8; 16] {
            let DataResult::FileTransfer { transfer } = self
                .call(
                    transfers,
                    DataOperation::FileTransferStart { entry_id: self.entry, total_bytes: total },
                )
                .unwrap()
            else {
                panic!("transfer")
            };
            transfer
        }
    }
    #[test]
    fn chunks_are_sequential_and_the_entry_changes_only_at_commit() {
        assert_eq!(chunk_end(MAX_CHUNK_BYTES, 0, 0, MAX_CHUNK_BYTES), Some(MAX_CHUNK_BYTES));
        assert_eq!(chunk_end(MAX_CHUNK_BYTES + 1, 0, 0, MAX_CHUNK_BYTES + 1), None);
        let mut f = Fixture::new();
        let transfers = Transfers::new(Default::default());
        f.host.write_file(f.address(), b"old").unwrap();
        let id = f.start(&transfers, 3);
        assert_eq!(
            f.call(&transfers, DataOperation::FileTransferCommit { transfer: id }),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(
            f.call(
                &transfers,
                DataOperation::FileTransferChunk { transfer: id, offset: 1, bytes: vec![1] }
            ),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(
            f.call(
                &transfers,
                DataOperation::FileTransferChunk {
                    transfer: id,
                    offset: 0,
                    bytes: b"new".to_vec()
                }
            ),
            Ok(DataResult::FileTransferred { offset: 3 })
        );
        assert_eq!(
            f.call(
                &transfers,
                DataOperation::FileTransferChunk {
                    transfer: id,
                    offset: 0,
                    bytes: b"new".to_vec()
                }
            ),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(f.host.read_file(f.address(), 0, 8).unwrap().bytes, b"old");
        assert_eq!(
            f.call(&transfers, DataOperation::FileTransferCommit { transfer: id }),
            Ok(DataResult::Written)
        );
        assert_eq!(f.host.read_file(f.address(), 0, 8).unwrap().bytes, b"new");
        assert_eq!(
            f.call(&transfers, DataOperation::FileTransferCommit { transfer: id }),
            Err(HostError::StaleHandle)
        );
    }
    #[test]
    fn a_different_context_or_volume_cannot_finish_a_transfer() {
        let mut f = Fixture::new();
        let budget = Arc::new(Budget::default());
        let first = Transfers::new(budget.clone());
        let second = Transfers::new(budget);
        let id = f.start(&first, 1);
        assert_eq!(
            f.call(&second, DataOperation::FileTransferCommit { transfer: id }),
            Err(HostError::StaleHandle)
        );
        assert_eq!(
            first.execute(
                DataCall {
                    volume: [9; 16],
                    operation: DataOperation::FileTransferChunk {
                        transfer: id,
                        offset: 0,
                        bytes: vec![1]
                    }
                },
                &mut f.host
            ),
            Err(HostError::StaleHandle)
        );
        f.call(
            &first,
            DataOperation::FileTransferChunk { transfer: id, offset: 0, bytes: vec![1] },
        )
        .unwrap();
        f.host.access = Access::owner(Id::from_bytes([9; 16]));
        assert_eq!(
            f.call(&first, DataOperation::FileTransferCommit { transfer: id }),
            Err(HostError::AccessDenied)
        );
        f.host.access = Access::owner(f.host.store.account().id);
        assert_eq!(
            f.call(&first, DataOperation::FileTransferCommit { transfer: id }),
            Err(HostError::StaleHandle)
        );
        assert_eq!(f.host.read_file(f.address(), 0, 8).unwrap().bytes, b"");
    }
    #[test]
    fn staging_credit_survives_a_pending_owner_and_reclaims_on_final_drop() {
        let budget = Arc::new(Budget::default());
        let large = super::super::blob::MAX_FILE_BYTES as usize;
        let a = Permit::reserve(&budget, large).unwrap();
        let b = Permit::reserve(&budget, large).unwrap();
        assert!(Permit::reserve(&budget, MAX_CHUNK_BYTES).is_err());
        assert!(Permit::reserve(&budget, large + 1).is_err());
        drop(a);
        drop(b);
        assert_eq!(*budget.0.lock().unwrap(), 0);
        let mut f = Fixture::new();
        let owner = Arc::new(Transfers::new(budget.clone()));
        f.start(&owner, MAX_CHUNK_BYTES as u64 + 17);
        let accepted = owner.clone();
        drop(owner);
        assert_eq!(*budget.0.lock().unwrap(), MAX_CHUNK_BYTES + 17);
        drop(accepted);
        assert_eq!(*budget.0.lock().unwrap(), 0);
        assert_eq!(f.host.read_file(f.address(), 0, 8).unwrap().bytes, b"");
    }
    #[test]
    fn full_file_limit_commits_through_bounded_chunks() {
        let mut f = Fixture::new();
        let budget = Arc::new(Budget::default());
        let transfers = Transfers::new(budget.clone());
        let total = super::super::blob::MAX_FILE_BYTES;
        assert_eq!(
            f.call(
                &transfers,
                DataOperation::FileTransferStart { entry_id: f.entry, total_bytes: total + 1 }
            ),
            Err(HostError::LimitExceeded)
        );
        let id = f.start(&transfers, total);
        let mut offset = 0;
        while offset < total {
            let length = (total - offset).min(MAX_CHUNK_BYTES as u64) as usize;
            f.call(
                &transfers,
                DataOperation::FileTransferChunk {
                    transfer: id,
                    offset,
                    bytes: vec![0x5a; length],
                },
            )
            .unwrap();
            offset += length as u64;
        }
        f.call(&transfers, DataOperation::FileTransferCommit { transfer: id }).unwrap();
        assert_eq!(*budget.0.lock().unwrap(), 0);
        let bytes = f
            .host
            .store
            .read_blob_entry(
                &f.host.access,
                Id::from_bytes(f.volume),
                crate::store::EntryId::from_bytes(f.entry).unwrap(),
            )
            .unwrap();
        assert_eq!(bytes.len() as u64, total);
        assert!(bytes.iter().all(|byte| *byte == 0x5a));
    }
    #[test]
    fn empty_file_requires_no_chunk_and_still_commits_atomically() {
        let mut f = Fixture::new();
        let transfers = Transfers::new(Default::default());
        let id = f.start(&transfers, 0);
        f.call(&transfers, DataOperation::FileTransferCommit { transfer: id }).unwrap();
        assert_eq!(f.host.read_file(f.address(), 0, 8).unwrap().bytes, b"");
    }
}
