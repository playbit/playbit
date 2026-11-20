//! Desktop credential-file ownership and atomic replacement. Hosts without
//! local-store supply protected persistence separately.
use super::{ClientError, ClientIdentity};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

// JSON expands certificate bytes; one identity is capped before parsing or writing.
const MAX_IDENTITY_BYTES: usize = 64 * 1024;

impl ClientIdentity {
    pub fn load(path: &Path) -> Result<Self, ClientError> {
        super::metadata::load(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), ClientError> {
        super::metadata::save(self, path)
    }
}

pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>, ClientError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_identity_file(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MAX_IDENTITY_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IDENTITY_BYTES {
        return Err(io::Error::other("identity exceeds storage limit").into());
    }
    Ok(Some(bytes))
}

pub(super) fn write(path: &Path, encoded: &[u8]) -> Result<(), ClientError> {
    if encoded.len() >= MAX_IDENTITY_BYTES {
        return Err(io::Error::other("identity exceeds storage limit").into());
    }
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temporary_path = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary_path)?;
        {
            // Duration only: failed writes may be partial.
            let _timing = crate::trace::Span::new("identity.write");
            file.write_all(encoded)?;
            file.write_all(b"\n")?;
        }
        {
            let _timing = crate::trace::Span::new("identity.sync");
            rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(std::io::Error::from)?;
        }
        {
            let _timing = crate::trace::Span::new("identity.replace");
            fs::rename(&temporary_path, path)?;
        }
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub(super) fn sync_parent(path: &Path) -> Result<(), ClientError> {
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
        let file = fs::File::open(parent)?;
        let _timing = crate::trace::Span::new("identity.directory_sync");
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(io::Error::from)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_identity_file(metadata: &fs::Metadata) -> Result<(), ClientError> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0 {
        return Err(ClientError::InsecureIdentityFile);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_identity_file(metadata: &fs::Metadata) -> Result<(), ClientError> {
    if !metadata.is_file() {
        return Err(ClientError::InsecureIdentityFile);
    }
    Ok(())
}
