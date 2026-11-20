//! Bloblog completions encode directly into admitted caller storage.
use super::DataResult;
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
use super::{DataCall, DataOperation};
use crate::runtime::host::{HostError, HostResult};
use playbit_api::abi::bloblog as abi;

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
impl DataCall {
    pub(super) fn validate_bloblog_result(&self, result: &DataResult) -> HostResult<()> {
        let log = self.operation.entry_id().ok_or(HostError::Internal)?;
        let entry = crate::store::EntryId::from_bytes(log).map_err(|_| HostError::Internal)?;
        let volume = crate::store::Id::from_bytes(self.volume);
        match (&self.operation, result) {
            (DataOperation::BloblogAppend { .. }, DataResult::BloblogAppended(cursor))
                if cursor.matches_log(volume, entry) =>
            {
                Ok(())
            }
            (DataOperation::BloblogRead { .. }, DataResult::BloblogPage(page)) => {
                if page.entries.iter().any(|value| value.log != entry)
                    || (page.more && page.entries.is_empty())
                    || !page.cursor.matches_log(volume, entry)
                {
                    return Err(HostError::Internal);
                }
                crate::store::bloblog::encode(&page.entries).map_err(|_| HostError::Internal)?;
                Ok(())
            }
            _ => Err(HostError::Internal),
        }
    }
}

fn entry(value: &crate::store::bloblog::Entry) -> abi::Entry<'_> {
    abi::Entry {
        log: *value.log.as_bytes(),
        id: *value.id.as_bytes(),
        clock: value.clock,
        nonce: *value.nonce.as_bytes(),
        data: &value.data,
    }
}

impl DataResult {
    pub(crate) fn bloblog_output_bytes(&self) -> HostResult<usize> {
        match self {
            Self::BloblogAppended(cursor) => Ok(cursor.as_str().len()),
            Self::BloblogPage(page) => {
                abi::page_bytes(page.entries.iter().map(entry), page.cursor.as_str(), page.more)
            }
            _ => Err(HostError::Internal),
        }
    }

    pub(crate) fn encode_bloblog(&self, output: &mut [u8]) -> HostResult<usize> {
        match self {
            Self::BloblogAppended(cursor) => {
                let bytes = cursor.as_str().as_bytes();
                output
                    .get_mut(..bytes.len())
                    .ok_or(HostError::BufferTooSmall)?
                    .copy_from_slice(bytes);
                Ok(bytes.len())
            }
            Self::BloblogPage(page) => abi::encode_page(
                output,
                page.entries.iter().map(entry),
                page.cursor.as_str(),
                page.more,
            ),
            _ => Err(HostError::Internal),
        }
    }
}
