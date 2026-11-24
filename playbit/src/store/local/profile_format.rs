//! Admission precedes mutable redb open, recovery and cleanup. Every native
//! writer retains the same exclusive local profile lock for its lifetime.
use super::*;
use std::io::{Read, Write};

pub(super) const FILE: &str = "control/profile-format.json";
const PENDING: &str = "control/profile-format.pending";

fn read_header(path: &Path, fresh: bool) -> Result<Option<Vec<u8>>> {
    let file =
        fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path.join(FILE));
    let mut file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !fresh || path.join("control/profile-init.kvdb").try_exists().map_err(io)? {
                return Err(profile::format::unsupported());
            }
            return Ok(None);
        }
        Err(error) => return Err(io(error)),
    };
    if !file.metadata().map_err(io)?.is_file() {
        return Err(Error::corrupt("profile format is not an ordinary file"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(profile::format::MAX_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(io)?;
    profile::format::validate(&bytes)?;
    Ok(Some(bytes))
}

pub(super) fn inspect(path: &Path, fresh: bool) -> Result<bool> {
    Ok(read_header(path, fresh)?.is_some())
}

pub(super) fn admit(path: &Path, fresh: bool) -> Result<()> {
    if inspect(path, fresh)? {
        return Ok(());
    }
    // Only an explicitly empty profile reaches initial publication.
    publish_header(path, profile::format::BASELINE)
}

fn publish_header(path: &Path, bytes: &[u8]) -> Result<()> {
    // A private spool is durable before rename; the directory is durable before
    // any logical database descriptor may require the newly published feature.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join(PENDING))
        .map_err(io)?;
    #[cfg(test)]
    fault(Boundary::Write)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    #[cfg(test)]
    fault(Boundary::Rename)?;
    fs::rename(path.join(PENDING), path.join(FILE)).map_err(io)?;
    #[cfg(test)]
    fault(Boundary::DirectorySync)?;
    initialization::sync_directory(&path.join("control"))
}

/// Caller holds the profile mutation lock. Publish the outer compatibility
/// barrier before committing any logical descriptor or local-only scope state.
pub(super) fn enable_local_app_scopes(store: &LocalStore) -> Result<()> {
    enable_app_scopes(store, profile::format::LOCAL_APP_SCOPES)
}

pub(super) fn enable_hosted_app_scopes(store: &LocalStore) -> Result<()> {
    enable_app_scopes(store, profile::format::HOSTED_APP_SCOPES)
}

pub(super) fn enable_app_local_erasure(store: &LocalStore) -> Result<()> {
    enable_app_scopes(store, profile::format::APP_LOCAL_ERASURE)
}

pub(super) fn enable_shared_sessions(store: &LocalStore) -> Result<()> {
    enable_app_scopes(store, profile::format::SHARED_SESSION)
}

#[cfg(any(feature = "connected-engine", test))]
pub(super) fn enable_world_projection(store: &LocalStore) -> Result<()> {
    enable_app_scopes(store, profile::format::WORLD_PROJECTION)
}

fn enable_app_scopes(store: &LocalStore, feature: &str) -> Result<()> {
    use kvdb::storage::ValueRead;
    let header = read_header(&store.path, false)?.ok_or_else(profile::format::unsupported)?;
    let descriptor = {
        let tx = store.metadata.begin_read()?;
        let value =
            tx.value_reader(profile::format::KEY)?.ok_or_else(profile::format::unsupported)?;
        if value.len() > profile::format::MAX_BYTES {
            return Err(Error::corrupt("profile format descriptor exceeds its bound"));
        }
        let mut bytes = vec![0; value.len()];
        value.read_exact_at(0, &mut bytes)?;
        bytes
    };
    let next_header = profile::format::promote_app_scopes(&header, feature)?;
    let next_descriptor = profile::format::promote_app_scopes(&descriptor, feature)?;
    if header == next_header && descriptor == next_descriptor {
        return Ok(());
    }
    // Even an already-renamed header is republished after an interrupted
    // directory sync, before completing a still-baseline database descriptor.
    publish_header(&store.path, &next_header)?;
    #[cfg(test)]
    fault(Boundary::MetadataCommit)?;
    if descriptor != next_descriptor {
        let mut tx = store.metadata.begin_write(now_unix_secs())?;
        tx.put(profile::format::KEY, &next_descriptor)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Write,
    Rename,
    DirectorySync,
    MetadataCommit,
}
#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<Option<Boundary>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn fault(boundary: Boundary) -> Result<()> {
    if FAILURE.with(|failure| failure.get() == Some(boundary)) {
        return Err(io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
