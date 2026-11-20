//! Local diagnostic text is prepared before IPC erases the underlying error type.
//! Startup failures carry at most 8 KiB; successful readiness is unchanged.
use std::{error::Error, fmt, io, path::Path};

pub(crate) const MAX_STARTUP_FAILURE_BYTES: usize = 8 * 1024;
const MAX_REASON_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
enum Kind {
    Other,
    Corrupt,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartupFailure {
    kind: Kind,
    message: String,
}

impl StartupFailure {
    pub(crate) fn new(profile: &Path, error: &(dyn Error + 'static)) -> Self {
        let kind =
            if super::is_corrupt_store_open_error(error) { Kind::Corrupt } else { Kind::Other };
        Self { kind, message: diagnostic(profile, error, true) }
    }

    pub fn write(&self, mut output: impl io::Write) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_STARTUP_FAILURE_BYTES {
            // Even six-byte JSON escapes fit this fallback without losing classification.
            let message = format!(
                "{}… (diagnostic truncated)",
                self.message.chars().take(1024).collect::<String>()
            );
            bytes = serde_json::to_vec(&Self { kind: self.kind, message })?;
        }
        assert!(bytes.len() <= MAX_STARTUP_FAILURE_BYTES, "startup diagnostic fits its frame");
        output.write_all(super::ipc::ENGINE_FAILED)?;
        output.write_all(&(bytes.len() as u32).to_le_bytes())?;
        output.write_all(&bytes)?;
        output.flush()
    }
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Native UI retains its existing concise recovery classification.
        match self.kind {
            Kind::Corrupt => f.write_str(super::ipc::CORRUPT_STORE_ERROR),
            Kind::Other => f.write_str(super::ipc::PROFILE_OPEN_ERROR),
        }
    }
}
impl Error for StartupFailure {}

fn store_error<'a>(
    mut error: &'a (dyn Error + 'static),
) -> Option<(&'a crate::store::Error, bool)> {
    let mut local = false;
    // Error chains are internal and shallow; cap traversal even for foreign errors.
    for _ in 0..super::ERROR_CHAIN_LIMIT {
        if let Some(store) = error.downcast_ref::<crate::store::Error>() {
            if matches!(store, crate::store::Error::LocalContext { .. }) {
                local = true;
            } else {
                return Some((store, local));
            }
        }
        error = error.source()?;
    }
    None
}

/// Format at the error owner, while both the profile and error type are available.
pub(crate) fn message(profile: &Path, error: &(dyn Error + 'static)) -> String {
    diagnostic(profile, error, false)
}

fn diagnostic(profile: &Path, error: &(dyn Error + 'static), opening_profile: bool) -> String {
    use crate::store::Error as StoreError;
    let mut text = error.to_string();
    // Remote/IPC boundaries may have erased the concrete store error. Known
    // compatibility and readiness guidance never calls for replacing a profile.
    if text == StoreError::UNSUPPORTED_VOLUME_FORMAT
        || text == crate::network::ap::service::INCOMPATIBLE_PROTOCOL
        || text == crate::network::ap::service::SHARED_SERVICE_UPGRADE_REQUIRED
        || matches!(
            text.as_str(),
            "waiting for initial database synchronization"
                | "waiting for initial volume synchronization"
        )
    {
        return text;
    }
    let Some((store, local)) = store_error(error) else { return text };
    if !matches!(
        store,
        StoreError::Corrupt(_)
            | StoreError::Store(_)
            | StoreError::Internal(_)
            | StoreError::System(_)
            | StoreError::Credentials(_)
    ) {
        return text;
    }
    let mut end = text.len().min(MAX_REASON_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        text.truncate(end);
        text.push('…');
    }
    text.push_str(&format!("\nProfile: {profile:?}"));
    if (opening_profile || local) && super::is_corrupt_store_open_error(error) {
        text.push_str("\nRecovery: quit Playbit and stop its engine, then back up this profile directory before moving it aside. Reopen Playbit and sign in again. Resetting removes local changes that have not synced; your online account is unchanged.");
    } else if matches!(store, StoreError::Internal(_)) {
        text.push_str("\nRecovery: restart Playbit and its engine. Check whether your last action was saved before repeating it. Keep the profile for diagnosis if this recurs.");
    } else if matches!(store, StoreError::Credentials(_)) {
        text.push_str(&format!("\n{}", super::ipc::PROFILE_OPEN_ERROR));
    }
    text.push_str("\nHint: set PLAYBIT_PROFILE=/path/to/new-profile to use a separate store.");
    match crate::paths::Paths::diagnostic_log(profile) {
        Ok(path) => text.push_str(&format!("\nEngine log: {path:?}")),
        Err(error) => text.push_str(&format!("\nEngine log path unavailable: {error}")),
    }
    text
}

/// The CLI shows startup details; native UI uses StartupFailure's short Display.
pub fn cli_message(mut error: &(dyn Error + 'static)) -> String {
    let original = error;
    for _ in 0..super::ERROR_CHAIN_LIMIT {
        if let Some(failure) = error.downcast_ref::<StartupFailure>() {
            return failure.message.clone();
        }
        let Some(source) = error.source() else { break };
        error = source;
    }
    original.to_string()
}

/// Send one bounded failed-readiness frame to the launching client.
pub fn write_startup_failure(
    profile: &Path,
    error: &(dyn Error + 'static),
    output: impl io::Write,
) -> io::Result<()> {
    StartupFailure::new(profile, error).write(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Error as StoreError;

    #[test]
    fn store_recovery_is_specific_to_corruption_and_respects_the_selected_profile() {
        // Exercise classification before string IPC, with a nondefault profile.
        let root = tempfile::tempdir().expect("fixture directory");
        let profile = root.path().join("alternate profile");
        let error = StoreError::corrupt("account signing seed is missing")
            .local_context("read profile identity from profile.kvdb".into());
        let text = message(&profile, &error);
        assert!(text.contains("account signing seed is missing"));
        assert!(text.contains("read profile identity from profile.kvdb"));
        assert!(text.contains(&format!("{profile:?}")));
        assert!(text.contains("back up"));
        assert!(text.contains("have not synced"));
        assert!(text.contains("PLAYBIT_PROFILE="));
        assert!(text.contains("logs/engine.log"));
        assert!(!profile.exists(), "diagnostics must not initialize a profile");
        for error in [
            StoreError::corrupt("received invitation body is invalid"),
            StoreError::Internal("profile lock is poisoned"),
            StoreError::Store(kvdb::Error::Busy),
            StoreError::System("permission denied".into()),
            StoreError::System(
                "waiting for initial database synchronization: missing backing".into(),
            ),
            StoreError::Credentials("local account key is missing".into()),
        ] {
            let text = message(&profile, &error);
            assert!(text.contains(&error.to_string()));
            assert!(!text.contains("moving it aside"));
            assert!(text.contains("PLAYBIT_PROFILE="));
        }
        assert_eq!(message(&profile, &StoreError::NotFound("volume")), "volume not found");
        let busy = StoreError::LocalContention("database is busy")
            .local_context("open existing local database profile.kvdb".into());
        assert!(matches!(busy, StoreError::LocalContention(_)));
    }

    #[test]
    fn known_guidance_keeps_its_classification_across_ipc() {
        let error =
            StoreError::UnsupportedVolumeFormat.local_context("read received volume".into());
        assert!(matches!(error, StoreError::UnsupportedVolumeFormat));
        assert_eq!(message(Path::new("/profile"), &error), StoreError::UNSUPPORTED_VOLUME_FORMAT);
        for text in [
            StoreError::UNSUPPORTED_VOLUME_FORMAT,
            crate::network::ap::service::INCOMPATIBLE_PROTOCOL,
            crate::network::ap::service::SHARED_SERVICE_UPGRADE_REQUIRED,
            "waiting for initial database synchronization",
            "waiting for initial volume synchronization",
        ] {
            let remote = StoreError::System(text.into());
            assert_eq!(message(Path::new("/profile"), &remote), text);
        }
    }

    #[test]
    fn startup_wire_payload_is_bounded_even_with_json_escapes() {
        // Maliciously long diagnostics cannot expand past the receive allocation limit.
        let failure = StartupFailure { kind: Kind::Other, message: "\0".repeat(20_000) };
        let mut bytes = Vec::new();
        failure.write(&mut bytes).expect("bounded diagnostic");
        let header = super::super::ipc::ENGINE_FAILED.len();
        let length =
            u32::from_le_bytes(bytes[header..header + 4].try_into().expect("length")) as usize;
        assert!(length <= MAX_STARTUP_FAILURE_BYTES);
        assert_eq!(bytes.len(), header + 4 + length);
        let decoded: StartupFailure = serde_json::from_slice(&bytes[header + 4..]).expect("JSON");
        assert!(decoded.message.ends_with("(diagnostic truncated)"));
    }
}
