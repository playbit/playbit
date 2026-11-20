//! Status describes the retained view, never a fresh lookup of its path.
use super::{
    host::{Directory, Reader},
    publication::HostWriter,
    staging::Recovery,
};
use playbit_api::abi::files::*;
use std::{fs::Metadata, io, os::unix::fs::MetadataExt};

fn timestamp(seconds: i64, nanos: i64) -> Option<i64> {
    if !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    // i128 avoids rejecting boundary values whose microseconds fit after the
    // fractional part is added; euclidean division rounds pre-epoch time down.
    i64::try_from(i128::from(seconds) * 1_000_000 + i128::from(nanos) / 1_000).ok()
}
fn host(metadata: &Metadata, kind: u32, immutable: bool) -> FileStatus {
    let mut status = FileStatus { kind, ..FileStatus::default() };
    if kind == FILE_KIND_FILE {
        status.file_size = metadata.len();
        status.flags |= FILE_SIZE_VALID;
        if immutable {
            status.flags |= FILE_CONTENTS_IMMUTABLE;
        }
    }
    if let Some(time) = timestamp(metadata.mtime(), metadata.mtime_nsec()) {
        status.modification_time = time;
        status.flags |= FILE_MODIFICATION_TIME_VALID;
    }
    status
}
impl Reader {
    pub fn status(&self) -> io::Result<FileStatus> {
        Ok(host(&self.metadata()?, FILE_KIND_FILE, self.immutable()?))
    }
}
impl Directory {
    pub fn status(&self) -> io::Result<FileStatus> {
        self.check()?;
        let status = host(&self.file.metadata()?, FILE_KIND_DIRECTORY, false);
        self.check()?;
        Ok(status)
    }
}
impl HostWriter {
    pub fn status(&self) -> io::Result<FileStatus> {
        Ok(FileStatus::private(self.length()?, false))
    }
}
impl Recovery {
    pub fn status(&self) -> io::Result<FileStatus> {
        Ok(FileStatus::private(self.length()?, true))
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests check retained views and timestamp boundaries.")]
mod tests {
    use super::*;
    use crate::engine::files::{
        authority::Authority, host::PrivateObjects, snapshot::SnapshotArea,
    };
    use std::{
        fs::{self, File},
        os::unix::fs::PermissionsExt,
        sync::Arc,
    };
    #[test]
    fn signed_microseconds_floor_and_overflow_are_explicit() {
        assert_eq!(timestamp(-1, 999_999_999), Some(-1));
        assert_eq!(timestamp(0, 999), Some(0));
        assert_eq!(timestamp(-1, 0), Some(-1_000_000));
        assert_eq!(timestamp(i64::MAX, 0), None);
        assert_eq!(timestamp(i64::MIN, 0), None);
        assert_eq!(timestamp(0, -1), None);
        assert_eq!(timestamp(0, 1_000_000_000), None);
        let seconds = i64::MIN.div_euclid(1_000_000);
        let nanos = i64::MIN.rem_euclid(1_000_000) * 1000;
        assert_eq!(timestamp(seconds, nanos), Some(i64::MIN));
        assert_eq!(timestamp(seconds, nanos - 1), None);
    }
    #[test]
    fn host_status_follows_bound_objects_and_snapshot_source_metadata() {
        let tree = tempfile::tempdir().unwrap();
        let private_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(private_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        super::super::privacy::restrict_new(&File::open(private_dir.path()).unwrap()).unwrap();
        let path = tree.path().canonicalize().unwrap();
        fs::write(path.join("file"), b"original").unwrap();
        let private = Arc::new(PrivateObjects::default());
        let authority = Arc::new(Authority::new(false));
        let root = Directory::root(&path, private.clone(), authority.clone()).unwrap();
        let directory = root.status().unwrap();
        assert_eq!(directory.kind, FILE_KIND_DIRECTORY);
        assert_eq!(directory.flags & (FILE_SIZE_VALID | FILE_CONTENTS_IMMUTABLE), 0);
        assert_eq!(directory.file_size, 0);
        let area =
            SnapshotArea::new(Arc::new(File::open(private_dir.path()).unwrap()), private).unwrap();
        let reader = root.reader(b"file").unwrap();
        let before = reader.status().unwrap();
        let snapshot = root.reader(b"file").unwrap().snapshot(&area).unwrap();
        let captured = snapshot.status().unwrap();
        assert_eq!(before.file_size, 8);
        assert_eq!(captured.file_size, before.file_size);
        assert_eq!(captured.modification_time, before.modification_time);
        fs::rename(path.join("file"), path.join("moved")).unwrap();
        fs::write(path.join("file"), b"replacement has another length").unwrap();
        assert_eq!(reader.status().unwrap(), before);
        assert_eq!(snapshot.status().unwrap(), captured);
        authority.revoke().unwrap();
        assert!(reader.status().is_err());
        assert!(snapshot.status().is_err());
        assert!(root.status().is_err());
    }
}
