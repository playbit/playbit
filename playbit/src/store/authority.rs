//! Replica-local negative evidence. Clearing a generation never grants permission.
use super::{EntryId, Error, Id, Result};

#[cfg(any(feature = "ap-protocol", target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalCause {
    AuthorityChanged = 1,
    ParticipantDenied = 2,
    ServiceUnavailable = 3,
    EntryAuthorityChanged = 4,
}

const BARRIER: u8 = 12;
const ENTRY_CAPTURE: u8 = 13;
const CATALOG_CEILING: u8 = 15;
const ENTRY_CEILING: u8 = 16;
const ENTRY_BARRIER: u8 = 17;
const BINDING_CAPTURE: u8 = 18;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Barrier {
    pub(crate) generation: u64,
    pub(crate) fenced: bool,
}
impl Barrier {
    #[cfg(any(feature = "local-store", target_arch = "wasm32", test))]
    pub(crate) fn invalidate(self) -> Self {
        Self { generation: self.generation.saturating_add(1), fenced: true }
    }
    pub(crate) fn accepts(self, generation: u64) -> bool {
        self.generation == generation && generation != u64::MAX
    }
    pub(crate) fn permits(self, entry_capture: Option<u64>) -> bool {
        !self.fenced || entry_capture.is_some_and(|generation| self.accepts(generation))
    }
    pub(crate) fn cleared_full(self) -> Self {
        let generation = self.generation.saturating_add(1);
        Self { generation, fenced: generation == u64::MAX }
    }
    pub(crate) fn decode(bytes: Option<&[u8]>) -> Result<Self> {
        let Some(bytes) = bytes else { return Ok(Self::default()) };
        if bytes.len() != 9 || bytes[8] > 1 {
            return Err(Error::corrupt("volume authority barrier has an invalid length or flag"));
        }
        Ok(Self {
            generation: u64::from_le_bytes(bytes[..8].try_into().expect("checked marker width")),
            fenced: bytes[8] == 1,
        })
    }
    pub(crate) fn encode(self) -> [u8; 9] {
        let mut bytes = [0; 9];
        bytes[..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8] = u8::from(self.fenced);
        bytes
    }
}

pub(crate) fn barrier_key(account: Id) -> [u8; 17] {
    let mut key = [0; 17];
    key[0] = BARRIER;
    key[1..].copy_from_slice(account.as_bytes());
    key
}
pub(crate) fn entry_key(account: Id, entry: EntryId) -> [u8; 32] {
    let mut key = [0; 32];
    key[..17].copy_from_slice(&barrier_key(account));
    key[0] = ENTRY_CAPTURE;
    key[17..].copy_from_slice(entry.as_bytes());
    key
}

pub(crate) fn catalog_key(account: Id) -> [u8; 17] {
    let mut key = barrier_key(account);
    key[0] = CATALOG_CEILING;
    key
}

pub(crate) fn binding_key(account: Id) -> [u8; 17] {
    let mut key = barrier_key(account);
    key[0] = BINDING_CAPTURE;
    key
}

pub(crate) fn capture_generation(bytes: Option<&[u8]>) -> Result<Option<u64>> {
    bytes
        .map(|bytes| {
            let bytes: [u8; 8] = bytes
                .try_into()
                .map_err(|_| Error::corrupt("captured authority generation must be eight bytes"))?;
            Ok(u64::from_le_bytes(bytes))
        })
        .transpose()
}

pub(crate) fn ceiling_entry_key(account: Id, entry: EntryId) -> [u8; 32] {
    let mut key = entry_key(account, entry);
    key[0] = ENTRY_CEILING;
    key
}

pub(crate) fn entry_barrier_key(account: Id, entry: EntryId) -> [u8; 32] {
    let mut key = entry_key(account, entry);
    key[0] = ENTRY_BARRIER;
    key
}

/// Full captures advance the volume generation and retire old entry barriers.
/// An entry capture clears only its own exact generation in that volume epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EntryBarrier {
    pub(crate) volume_generation: u64,
    pub(crate) barrier: Barrier,
}
impl EntryBarrier {
    pub(crate) fn current(self, volume_generation: u64) -> Result<Barrier> {
        if self.volume_generation > volume_generation {
            return Err(Error::corrupt("entry authority generation exceeds its volume generation"));
        }
        Ok(if self.volume_generation == volume_generation {
            self.barrier
        } else {
            Barrier::default()
        })
    }
    pub(crate) fn decode(bytes: Option<&[u8]>) -> Result<Self> {
        let Some(bytes) = bytes else { return Ok(Self::default()) };
        if bytes.len() != 17 {
            return Err(Error::corrupt("entry authority barrier must be 17 bytes"));
        }
        Ok(Self {
            volume_generation: u64::from_le_bytes(bytes[..8].try_into().expect("checked width")),
            barrier: Barrier::decode(Some(&bytes[8..]))?,
        })
    }
    pub(crate) fn encode(self) -> [u8; 17] {
        let mut bytes = [0; 17];
        bytes[..8].copy_from_slice(&self.volume_generation.to_le_bytes());
        bytes[8..].copy_from_slice(&self.barrier.encode());
        bytes
    }
}
