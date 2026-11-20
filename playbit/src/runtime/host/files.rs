//! Owned File service messages. Handles are opaque engine capabilities scoped
//! to the authenticated execution context, never persistent entry identities.
use super::{HostError, HostResult};
pub mod capacity;
use playbit_api::abi::files::*;

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    any(feature = "app-host", target_arch = "wasm32"),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Call {
    pub file: u64,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    any(feature = "app-host", target_arch = "wasm32"),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Action {
    Open { path: Vec<u8>, mode: u32 },
    Read { offset: u64, length: u32 },
    Write { offset: u64, bytes: Vec<u8> },
    Status,
    Close { flags: u32 },
    ListOpen,
    ListNext { index: u32 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    any(feature = "app-host", target_arch = "wasm32"),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Output {
    File { handle: u64 },
    Read { bytes: Vec<u8> },
    Written,
    Status { kind: u32, flags: u32, size: u64, modification_time: i64 },
    Close { error: i32, publication: u32, recovery: Option<u64> },
    List { handle: u64 },
    Entry { kind: u32, name: Vec<u8> },
    End,
}
pub const NOT_PUBLISHED: u32 = 0;
pub const PUBLISHED: u32 = 1;
pub const PUBLICATION_UNKNOWN: u32 = 2;
pub const PUBLISHED_DURABILITY_UNKNOWN: u32 = 3;

impl From<FileStatus> for Output {
    fn from(status: FileStatus) -> Self {
        Self::Status {
            kind: status.kind,
            flags: status.flags,
            size: status.file_size,
            modification_time: status.modification_time,
        }
    }
}
impl Action {
    pub fn capability_rights(&self) -> crate::capability::Rights {
        use crate::capability::Rights;
        match self {
            Self::Close { .. } | Self::Status => Rights::NONE,
            Self::Write { .. } => Rights::WRITE,
            Self::Open { mode, .. } if mode & FILE_WRITE != 0 => Rights::WRITE,
            _ => Rights::READ,
        }
    }

    /// All range/path/flag errors are rejected before sealing or queue admission.
    /// Type-dependent checks (including SYNC on a reader) belong to the owner.
    pub fn validate(&self) -> HostResult<()> {
        match self {
            Self::Open { path, mode } => {
                if !valid_open_mode(*mode) || path.is_empty() || path.len() > MAX_FILE_PATH_BYTES {
                    return Err(HostError::InvalidArgument);
                }
                let mut count = 0;
                for component in path.split(|byte| *byte == b'/') {
                    count += 1;
                    if matches!(component, b"" | b"." | b"..")
                        || component.contains(&0)
                        || component.len() > 255
                        || count > 64
                    {
                        return Err(HostError::InvalidArgument);
                    }
                }
            }
            Self::Read { offset, length }
                if *length > MAX_FILE_READ
                    || offset
                        .checked_add(u64::from(*length))
                        .is_none_or(|end| end > MAX_FILE_READ_EXTENT) =>
            {
                return Err(HostError::LimitExceeded);
            }
            Self::Write { offset, bytes }
                if bytes.len() > MAX_FILE_WRITE as usize
                    || offset
                        .checked_add(bytes.len() as u64)
                        .is_none_or(|end| end > MAX_FILE_EXTENT) =>
            {
                return Err(HostError::LimitExceeded);
            }
            Self::Close { flags } if !valid_close_flags(*flags, true) => {
                return Err(HostError::InvalidArgument);
            }
            Self::ListNext { index } if *index > 16 * 1024 => {
                return Err(HostError::InvalidArgument);
            }
            _ => {}
        }
        Ok(())
    }
    pub fn retained_bytes(&self) -> HostResult<usize> {
        self.validate()?;
        // Include owned input capacity, maximum result payload and fixed records.
        let payload = match self {
            Self::Open { path, .. } => path.capacity(),
            Self::Write { bytes, .. } => bytes.capacity(),
            Self::Read { length, .. } => *length as usize,
            Self::ListNext { .. } => 255,
            _ => 0,
        };
        payload.checked_add(256).ok_or(HostError::LimitExceeded)
    }
}
impl Call {
    /// Correlate a transport completion before exposing capabilities or bytes.
    /// No malformed response can become a success of another operation kind.
    pub fn validate_output(&self, output: &Output) -> HostResult<()> {
        self.action.validate()?;
        let valid = match (&self.action, output) {
            (Action::Open { .. }, Output::File { handle })
            | (Action::ListOpen, Output::List { handle }) => *handle != 0,
            (Action::Read { length, .. }, Output::Read { bytes }) => {
                bytes.len() <= *length as usize
            }
            (Action::Write { .. }, Output::Written) => true,
            (Action::Status, Output::Status { kind, flags, size, modification_time }) => {
                let flags = *flags;
                matches!(*kind, FILE_KIND_FILE | FILE_KIND_DIRECTORY)
                    && flags
                        & !(FILE_SIZE_VALID
                            | FILE_MODIFICATION_TIME_VALID
                            | FILE_CONTENTS_IMMUTABLE)
                        == 0
                    && (flags & FILE_SIZE_VALID != 0 || *size == 0)
                    && (flags & FILE_MODIFICATION_TIME_VALID != 0 || *modification_time == 0)
                    && (*kind != FILE_KIND_DIRECTORY
                        || flags & (FILE_SIZE_VALID | FILE_CONTENTS_IMMUTABLE) == 0)
            }
            (Action::Close { .. }, Output::Close { error, publication, recovery }) => {
                let successful = *error == 0
                    && matches!(*publication, NOT_PUBLISHED | PUBLISHED)
                    && recovery.is_none();
                let failed = HostError::try_from(*error).is_ok_and(|error| {
                    !matches!(error, HostError::WouldBlock | HostError::BufferTooSmall)
                }) && (recovery.is_none() || *publication == NOT_PUBLISHED)
                    && matches!(
                        *publication,
                        NOT_PUBLISHED | PUBLICATION_UNKNOWN | PUBLISHED_DURABILITY_UNKNOWN
                    );
                (successful || failed) && recovery.is_none_or(|handle| handle != 0)
            }
            (Action::ListNext { .. }, Output::Entry { kind, name }) => {
                matches!(
                    *kind,
                    FILE_KIND_FILE | FILE_KIND_DIRECTORY | FILE_KIND_SYMLINK | FILE_KIND_OTHER
                ) && !name.is_empty()
                    && name.len() <= 255
                    && !matches!(name.as_slice(), b"." | b"..")
                    && !name.iter().any(|byte| matches!(byte, 0 | b'/'))
            }
            (Action::ListNext { .. }, Output::End) => true,
            _ => false,
        };
        if valid { Ok(()) } else { Err(HostError::Internal) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_checks_complete_ranges_and_empty_operations_without_mutation() {
        assert_eq!(Action::Read { offset: MAX_FILE_EXTENT, length: 0 }.validate(), Ok(()));
        assert_eq!(Action::Read { offset: MAX_FILE_READ_EXTENT - 1, length: 1 }.validate(), Ok(()));
        assert_eq!(
            Action::Read { offset: MAX_FILE_READ_EXTENT, length: 1 }.validate(),
            Err(HostError::LimitExceeded)
        );
        assert_eq!(Action::Write { offset: MAX_FILE_EXTENT, bytes: vec![] }.validate(), Ok(()));
        assert_eq!(
            Action::Read { offset: u64::MAX, length: 1 }.validate(),
            Err(HostError::LimitExceeded)
        );
        assert_eq!(
            Action::Write { offset: MAX_FILE_EXTENT, bytes: vec![1] }.validate(),
            Err(HostError::LimitExceeded)
        );
        for path in [b"".as_slice(), b"/file", b"a/../b", b"a//b", b"a\0b"] {
            assert_eq!(
                Action::Open { path: path.to_vec(), mode: FILE_READ }.validate(),
                Err(HostError::InvalidArgument)
            );
        }
        for mode in 0..32 {
            assert_eq!(
                valid_open_mode(mode),
                [FILE_READ, FILE_DIRECTORY, FILE_WRITE, FILE_WRITE | FILE_EXCLUSIVE]
                    .contains(&mode)
            );
        }
        assert_eq!(
            Action::Close { flags: FILE_CLOSE_SYNC | FILE_CLOSE_DISCARD }.validate(),
            Err(HostError::InvalidArgument)
        );
    }
}

/// A helper root admitted before the runtime starts. This record is trusted
/// host metadata, never a guest-selected path or reconstructed capability.
#[derive(Clone)]
pub struct Root {
    /// Previous helper retired by the host before transferring its exact credit.
    /// Only replace_roots may consume this receipt; it is never guest input.
    pub replaces: Option<u64>,
    pub backing: u32,
    pub volume: [u8; 16],
    pub grant: [u8; 16],
    pub engine: u64,
    pub writable: bool,
    /// Reserved from Host::file_capacity before the backend root is created.
    pub credit: capacity::Credit,
}
