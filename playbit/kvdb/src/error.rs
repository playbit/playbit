use core::fmt;

/// kvdb error type.
///
/// Storage backends report failures through [`Error::Storage`] with a
/// backend-defined code; everything else is a kvdb-level condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Key not found (only returned by operations that require presence).
    NotFound,
    /// A database already exists in this storage.
    Exists,
    /// Invalid argument (e.g. branch factor out of range).
    InvalidArgument,
    /// Invalid key (e.g. empty).
    InvalidKey,
    /// Key exceeds [`crate::KEY_SIZE_MAX`].
    KeySize,
    /// Value exceeds [`crate::VALUE_SIZE_MAX`].
    ValueSize,
    /// A mutating operation was attempted on a read-only transaction.
    ReadOnly,
    /// A write transaction is already active (single-writer store).
    Busy,
    /// Stored data failed validation (unknown layout, truncated record, …).
    Corrupt,
    /// The database was created with an incompatible format version.
    Version,
    /// Operation requires a syncable database but this database is plain,
    /// or vice versa.
    Mode,
    /// The peers' hash keys differ; these replicas cannot sync.
    HashKeyMismatch,
    /// The peers' branching factors differ; these replicas cannot sync.
    BranchFactorMismatch,
    /// Malformed or protocol-violating sync message.
    Protocol,
    /// Peer reported an error during sync.
    Remote,
    /// Out of memory or an internal capacity was exceeded.
    Capacity,
    /// No representable clock remains for the next event.
    ClockExhausted,
    /// Backend failure with a backend-defined code.
    Storage(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => f.write_str("not found"),
            Error::Exists => f.write_str("database already exists"),
            Error::InvalidArgument => f.write_str("invalid argument"),
            Error::InvalidKey => f.write_str("invalid key"),
            Error::KeySize => f.write_str("key too large"),
            Error::ValueSize => f.write_str("value too large"),
            Error::ReadOnly => f.write_str("read-only transaction"),
            Error::Busy => f.write_str("write transaction already active"),
            Error::Corrupt => f.write_str("corrupt database"),
            Error::Version => f.write_str("incompatible database version"),
            Error::Mode => f.write_str("operation not supported in this database mode"),
            Error::HashKeyMismatch => f.write_str("replica hash keys differ"),
            Error::BranchFactorMismatch => f.write_str("replica branching factors differ"),
            Error::Protocol => f.write_str("malformed sync message"),
            Error::Remote => f.write_str("remote peer error"),
            Error::Capacity => f.write_str("capacity exceeded"),
            Error::ClockExhausted => f.write_str("logical clock exhausted"),
            Error::Storage(code) => write!(f, "storage backend error {code}"),
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[cfg(test)]
mod tests;
