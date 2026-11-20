//! Store failures retain safe diagnostic context without including record contents.
use std::{borrow::Cow, fmt};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Store(kvdb::Error),
    NotFound(&'static str),
    AlreadyExists(&'static str),
    AccessDenied(&'static str),
    Unsupported(&'static str),
    ParticipantRevoked {
        volume: super::Id,
        account: super::Id,
    },
    InvalidInput(&'static str),
    /// The volume uses a different on-disk/replicated schema, not damaged local data.
    UnsupportedVolumeFormat,
    /// Invalid stored or received data; this alone does not establish local disk damage.
    Corrupt(Cow<'static, str>),
    /// Failed process state, including poisoned locks, without evidence of disk damage.
    Internal(&'static str),
    LocalContention(&'static str),
    LimitExceeded,
    System(String),
    Credentials(String),
    /// Context added at a local file or retained-profile read boundary.
    LocalContext {
        operation: String,
        source: Box<Error>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store(error) => write!(f, "store error: {error}"),
            Error::NotFound(kind) => write!(f, "{kind} not found"),
            Error::AlreadyExists(kind) => write!(f, "{kind} already exists"),
            Error::AccessDenied(right) => write!(f, "access denied: {right}"),
            Error::Unsupported(message) => write!(f, "unsupported: {message}"),
            Error::ParticipantRevoked { .. } => {
                f.write_str("access denied: revoked volume participant")
            }
            Error::InvalidInput(message) => f.write_str(message),
            Error::UnsupportedVolumeFormat => f.write_str(Self::UNSUPPORTED_VOLUME_FORMAT),
            Error::Corrupt(reason) => write!(f, "corrupt Archipelago store: {reason}"),
            Error::Internal(reason) => write!(f, "local store interrupted: {reason}"),
            Error::LocalContention(message) => f.write_str(message),
            Error::LimitExceeded => f.write_str("resource limit exceeded"),
            Error::System(message) | Error::Credentials(message) => f.write_str(message),
            Error::LocalContext { operation, source } => write!(f, "{operation}: {source}"),
        }
    }
}

impl Error {
    pub const UNSUPPORTED_VOLUME_FORMAT: &'static str = "This session uses a data format this version of Playbit can’t open. Your saved data has been kept.";

    #[cfg(feature = "local-store")]
    pub(crate) fn local_context(self, operation: String) -> Self {
        // Logical failures retain their typed status for host and retry decisions.
        match self {
            Self::Corrupt(_)
            | Self::Store(_)
            | Self::Internal(_)
            | Self::System(_)
            | Self::Credentials(_)
            | Self::LocalContext { .. } => Self::LocalContext { operation, source: Box::new(self) },
            _ => self,
        }
    }
    pub fn corrupt(reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Corrupt(reason.into())
    }

    pub(crate) fn read_failure(operation: &'static str, error: std::io::Error) -> Self {
        let reason = format!("{operation}: {error}");
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::corrupt(reason)
        } else {
            Self::System(reason)
        }
    }

    /// Serde's Display can include the input value (including credentials).
    /// Preserve the record type, failure category and location instead.
    pub(crate) fn decode<T>(error: serde_json::Error) -> Self {
        if error.is_io() {
            return Self::System(format!("read {}: {error}", std::any::type_name::<T>()));
        }
        let reason = match error.classify() {
            serde_json::error::Category::Io => "record read failed",
            serde_json::error::Category::Syntax => "invalid JSON syntax",
            serde_json::error::Category::Data => "record does not match the expected schema",
            serde_json::error::Category::Eof => "truncated JSON",
        };
        Self::corrupt(format!(
            "decode {}: {reason} at line {}, column {}",
            std::any::type_name::<T>(),
            error.line(),
            error.column(),
        ))
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::LocalContext { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<kvdb::Error> for Error {
    fn from(error: kvdb::Error) -> Self {
        Error::Store(error)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_diagnostics_identify_the_record_and_location_without_echoing_values() {
        // Serde includes strings in schema errors; diagnostics must keep them private.
        let error = super::Error::decode::<u64>(
            serde_json::from_slice::<u64>(br#""private-recovery-secret""#).expect_err("wrong type"),
        )
        .to_string();
        assert!(error.contains("decode u64"));
        assert!(error.contains("expected schema"));
        assert!(error.contains("line 1, column"));
        assert!(!error.contains("private-recovery-secret"));
        let error = super::Error::decode::<Vec<u64>>(
            serde_json::from_slice::<Vec<u64>>(b"[1,").expect_err("truncated array"),
        )
        .to_string();
        assert!(error.contains("truncated JSON"));
    }
}
