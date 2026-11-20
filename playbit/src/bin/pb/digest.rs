//! Bounded content identity for the immutable release artifact publisher.
use playbit::engine::Result;
use ring::digest::{Context, SHA256};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Serialize)]
pub(super) struct Digest {
    pub hash: String,
    pub sha256: String,
    pub bytes: u64,
}

pub(super) fn file(path: &Path) -> Result<Digest> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_BYTES {
        return Err("Artifact must be a nonempty regular file no larger than 256 MiB".into());
    }
    let mut content = blake3::Hasher::new();
    let mut checksum = Context::new(&SHA256);
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > before.len() || bytes > MAX_BYTES {
            return Err("Artifact changed while hashing".into());
        }
        content.update(&buffer[..count]);
        checksum.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    if bytes != before.len()
        || before.len() != after.len()
        || (before.mtime(), before.mtime_nsec(), before.ctime(), before.ctime_nsec())
            != (after.mtime(), after.mtime_nsec(), after.ctime(), after.ctime_nsec())
    {
        return Err("Artifact changed while hashing".into());
    }
    Ok(Digest {
        hash: playbit::identifier::encode(content.finalize().as_bytes()),
        sha256: checksum.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_identity_is_canonical_and_unbounded_inputs_never_start() {
        let directory = tempfile::tempdir().expect("artifact directory");
        let path = directory.path().join("artifact");
        std::fs::write(&path, b"abc").expect("artifact");
        let digest = file(&path).expect("content identity");
        assert_eq!(digest.bytes, 3);
        assert_eq!(
            digest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            playbit::identifier::decode::<32>(&digest.hash).expect("address"),
            *blake3::hash(b"abc").as_bytes()
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).expect("link");
        assert!(file(&link).is_err());
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("artifact")
            .set_len(MAX_BYTES + 1)
            .expect("sparse oversized file");
        assert!(file(&path).is_err());
    }
}
