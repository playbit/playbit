//! KV input/result checks and allocation-free caller-buffer framing.
use super::DataResult;
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
use super::{DataCall, DataOperation};
use crate::{
    runtime::host::{HostError, HostResult},
    store::key_value,
};
use playbit_api::abi::kv as abi;

pub(crate) fn root(value: key_value::Root) -> abi::Root {
    abi::Root { level: u32::from(value.level), digest: value.digest }
}
pub(crate) fn stored_root(present: u32, value: abi::Root) -> HostResult<Option<key_value::Root>> {
    abi::root(present, value).map(|value| {
        value.map(|value| key_value::Root { level: value.level as u8, digest: value.digest })
    })
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
impl DataOperation {
    pub(super) fn validate_kv(&self) -> HostResult<()> {
        let key = |key: &[u8]| key_value::validate_key(key).map_err(|_| HostError::InvalidArgument);
        match self {
            Self::KvKeys { after, root, limit, .. } => {
                if *limit == 0
                    || *limit as usize > key_value::MAX_KEYS
                    || (after.is_some() && root.is_none())
                {
                    return Err(HostError::InvalidArgument);
                }
                after.as_deref().map(key).transpose().map(|_| ())
            }
            Self::KvRead { key: bytes, offset, limit, root, .. } => {
                key(bytes)?;
                if *limit == 0
                    || *limit as usize > key_value::MAX_BYTES
                    || *offset > key_value::MAX_STORED_BYTES as u64
                    || (*offset != 0 && root.is_none())
                {
                    return Err(HostError::InvalidArgument);
                }
                Ok(())
            }
            Self::KvPut { key: bytes, value, .. } => {
                key(bytes)?;
                if value.len() > key_value::MAX_BYTES {
                    return Err(HostError::LimitExceeded);
                }
                Ok(())
            }
            Self::KvDelete { key: bytes, .. } => key(bytes),
            _ => Err(HostError::Internal),
        }
    }
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
impl DataCall {
    pub(super) fn execute_kv(
        &self,
        host: &mut dyn crate::runtime::host::Host,
    ) -> HostResult<DataResult> {
        let address = crate::runtime::host::EntryAddress {
            volume_id: self.volume,
            entry_id: self.operation.entry_id().ok_or(HostError::InvalidArgument)?,
        };
        Ok(match &self.operation {
            DataOperation::KvKeys { after, root, limit, .. } => {
                DataResult::KvKeys(host.kv_keys(address, after.as_deref(), *root, *limit)?)
            }
            DataOperation::KvRead { key, offset, limit, root, .. } => {
                DataResult::KvRead(host.kv_read(address, key, *offset, *limit, *root)?)
            }
            DataOperation::KvPut { key, value, .. } => {
                host.kv_put(address, key, value)?;
                DataResult::KvWritten
            }
            DataOperation::KvDelete { key, .. } => {
                host.kv_delete(address, key)?;
                DataResult::KvDeleted
            }
            _ => return Err(HostError::Unsupported),
        })
    }
    pub(super) fn validate_kv_result(&self, result: &DataResult) -> HostResult<()> {
        match (&self.operation, result) {
            (DataOperation::KvKeys { after, root, limit, .. }, DataResult::KvKeys(page)) => {
                result.kv_output_bytes()?;
                if page.keys.iter().any(|key| {
                    key.preview.len() > key_value::PREVIEW_BYTES
                        || key.preview.len() as u64 > key.len_bytes
                }) || page.keys.len() > *limit as usize
                    || (root.is_some() && page.root != *root)
                    || (page.root.is_none() && (!page.keys.is_empty() || page.next.is_some()))
                    || after.as_ref().is_some_and(|after| {
                        page.keys.iter().any(|key| key.bytes <= *after)
                            || page.next.as_ref().is_some_and(|next| next <= after)
                    })
                {
                    return Err(HostError::Internal);
                }
                Ok(())
            }
            (
                DataOperation::KvRead { root: expected, offset, limit, .. },
                DataResult::KvRead(chunk),
            ) => {
                if expected.is_some_and(|root| root != chunk.root)
                    || chunk.offset != *offset
                    || chunk.offset > chunk.total_bytes
                    || chunk.total_bytes > key_value::MAX_STORED_BYTES as u64
                    || chunk.bytes.len() as u64
                        != u64::from(*limit).min(chunk.total_bytes - chunk.offset)
                {
                    return Err(HostError::Internal);
                }
                result.kv_output_bytes().map(|_| ())
            }
            (DataOperation::KvPut { .. }, DataResult::KvWritten)
            | (DataOperation::KvDelete { .. }, DataResult::KvDeleted) => Ok(()),
            _ => Err(HostError::Internal),
        }
    }
}

impl DataResult {
    pub(crate) fn kv_output_bytes(&self) -> HostResult<usize> {
        match self {
            Self::KvKeys(page) => abi::page_bytes(
                page.keys
                    .iter()
                    .map(|key| abi::Key { bytes: &key.bytes, len_bytes: key.len_bytes }),
                page.next.as_deref(),
            ),
            Self::KvRead(chunk) if chunk.bytes.len() <= key_value::MAX_BYTES => {
                Ok(49 + chunk.bytes.len())
            }
            _ => Err(HostError::Internal),
        }
    }
    pub(crate) fn encode_kv(&self, output: &mut [u8]) -> HostResult<usize> {
        match self {
            Self::KvKeys(page) => abi::encode_page(
                output,
                page.root.map(root),
                page.keys
                    .iter()
                    .map(|key| abi::Key { bytes: &key.bytes, len_bytes: key.len_bytes }),
                page.next.as_deref(),
            ),
            Self::KvRead(chunk) => abi::encode_chunk(
                output,
                abi::Chunk {
                    root: root(chunk.root),
                    total_bytes: chunk.total_bytes,
                    offset: chunk.offset,
                    bytes: &chunk.bytes,
                },
            ),
            _ => Err(HostError::Internal),
        }
    }
}
