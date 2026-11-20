//! Real redb file I/O with tracing and the shared fsync storage backend.

use kvdb::redb::FileBackend;
use redb::{Database, DatabaseError, StorageBackend};
use std::{
    fs::OpenOptions,
    io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct TracedFileBackend {
    inner: FileBackend,
    closed: AtomicBool,
}

impl TracedFileBackend {
    fn new(inner: FileBackend) -> Self {
        Self { inner, closed: AtomicBool::new(false) }
    }
}

impl Drop for TracedFileBackend {
    fn drop(&mut self) {
        // redb closes initialized databases explicitly. Construction failures
        // also own the acquired lock and must release it while duplicates live.
        if let Err(error) = self.close() {
            crate::trace::mark("storage.file.close_failed");
            crate::diagnostic!("Playbit database lock cleanup failed: {error}");
        }
    }
}

impl StorageBackend for TracedFileBackend {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let _timing = crate::trace::Span::new("storage.file.read");
        self.inner.read(offset, out)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        // redb's 4-KiB data-page writes are the useful durable I/O detail.
        // Its frequent 320-byte metadata writes remain inside the caller's
        // storage span; instrumenting each one overflows a hot producer ring.
        let _timing =
            (data.len() >= 4096).then(|| crate::trace::Span::new("storage.file.large_write"));
        self.inner.write(offset, data)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let _timing = crate::trace::Span::new("storage.file.resize");
        self.inner.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        let _timing = crate::trace::Span::new("storage.file.sync");
        self.inner.sync_data()
    }

    fn close(&self) -> io::Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(error) = self.inner.close() {
            self.closed.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}

pub(crate) fn open(path: &Path, cache_bytes: usize) -> Result<Database, DatabaseError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    database(existing_backend(FileBackend::new(file)?)?, cache_bytes)
}

fn existing_backend(inner: FileBackend) -> Result<FileBackend, DatabaseError> {
    // create_with_backend permits initialization; existing-file open must not.
    // Validate while holding redb's file lock, then explicitly release that
    // lock on rejection. Dropping one descriptor alone may leave a duplicate
    // holding the same open-file-description lock until it is also closed.
    let error = match inner.len() {
        Ok(length) if length > 0 => return Ok(inner),
        Ok(_) => io::Error::from(io::ErrorKind::InvalidData),
        Err(error) => error,
    };
    // Best-effort: a failed close here must not shadow the rejection reason above.
    if let Err(close_error) = inner.close() {
        crate::trace::mark("storage.file.close_failed");
        crate::diagnostic!("Playbit database lock cleanup failed: {close_error}");
    }
    Err(error.into())
}

pub(crate) fn create(path: &Path, cache_bytes: usize) -> Result<Database, DatabaseError> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    database(FileBackend::new(file)?, cache_bytes)
}

fn database(inner: FileBackend, cache_bytes: usize) -> Result<Database, DatabaseError> {
    Database::builder()
        .set_cache_size(cache_bytes)
        .create_with_backend(TracedFileBackend::new(inner))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use redb::{ReadableDatabase, StorageError, TableDefinition};
    use std::fs;

    const CACHE_BYTES: usize = 512 * 1024;
    const TABLE: TableDefinition<&str, u64> = TableDefinition::new("probe");

    fn error_kind(error: DatabaseError) -> io::ErrorKind {
        match error {
            DatabaseError::Storage(StorageError::Io(error)) => error.kind(),
            other => panic!("unexpected database error: {other}"),
        }
    }

    #[test]
    fn opening_missing_empty_and_invalid_files_does_not_initialize_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        assert_eq!(error_kind(open(&path, CACHE_BYTES).unwrap_err()), io::ErrorKind::NotFound);
        assert!(!path.exists());

        fs::write(&path, []).unwrap();
        assert_eq!(error_kind(open(&path, CACHE_BYTES).unwrap_err()), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), b"");

        fs::write(&path, b"invalid database").unwrap();
        assert_eq!(error_kind(open(&path, CACHE_BYTES).unwrap_err()), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), b"invalid database");
    }

    #[test]
    fn rejected_empty_file_releases_lock_while_a_duplicate_descriptor_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        let duplicate = file.try_clone().unwrap();
        let inner = FileBackend::new(file).unwrap();
        assert_eq!(error_kind(existing_backend(inner).unwrap_err()), io::ErrorKind::InvalidData);
        assert_eq!(duplicate.metadata().unwrap().len(), 0);
        // This requires immediate exclusive-lock reacquisition, not eventual
        // last-descriptor closure. Both open paths use existing_backend.
        assert_eq!(error_kind(open(&path, CACHE_BYTES).unwrap_err()), io::ErrorKind::InvalidData);

        assert_eq!(fs::read(&path).unwrap(), b"");
        assert_eq!(duplicate.metadata().unwrap().len(), 0);
    }

    #[test]
    fn rejected_builder_releases_lock_while_a_duplicate_descriptor_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        fs::write(&path, b"invalid database").unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let duplicate = file.try_clone().unwrap();
        let inner = existing_backend(FileBackend::new(file).unwrap()).unwrap();
        assert_eq!(
            error_kind(database(inner, CACHE_BYTES).unwrap_err()),
            io::ErrorKind::InvalidData
        );
        assert_eq!(error_kind(open(&path, CACHE_BYTES).unwrap_err()), io::ErrorKind::InvalidData);

        assert_eq!(fs::read(&path).unwrap(), b"invalid database");
        assert_eq!(duplicate.metadata().unwrap().len(), 16);
    }

    #[test]
    fn dropping_a_closed_wrapper_preserves_a_separately_reacquired_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        fs::write(&path, []).unwrap();
        let open_file = || OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let first = TracedFileBackend::new(FileBackend::new(open_file()).unwrap());
        first.close().unwrap();
        let second = FileBackend::new(open_file()).unwrap();
        drop(first);
        assert!(matches!(FileBackend::new(open_file()), Err(DatabaseError::DatabaseAlreadyOpen)));
        second.close().unwrap();
        let third = FileBackend::new(open_file()).unwrap();
        third.close().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn committed_data_survives_close_open_and_create_on_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        // Creation accepts an existing empty file, matching Builder::create.
        fs::write(&path, []).unwrap();
        let database = create(&path, CACHE_BYTES).unwrap();
        let transaction = database.begin_write().unwrap();
        {
            let mut table = transaction.open_table(TABLE).unwrap();
            table.insert("value", 42).unwrap();
        }
        transaction.commit().unwrap();
        drop(database);

        #[allow(
            clippy::type_complexity,
            reason = "Compare both opener functions with the same test table."
        )]
        let openers: [fn(&Path, usize) -> Result<Database, DatabaseError>; 2] = [open, create];
        for open_database in openers {
            let database = open_database(&path, CACHE_BYTES).unwrap();
            let transaction = database.begin_read().unwrap();
            let table = transaction.open_table(TABLE).unwrap();
            assert_eq!(table.get("value").unwrap().unwrap().value(), 42);
        }
    }

    #[test]
    fn exclusive_lock_is_preserved_and_released() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        let database = create(&path, CACHE_BYTES).unwrap();
        assert!(matches!(open(&path, CACHE_BYTES), Err(DatabaseError::DatabaseAlreadyOpen)));
        assert!(matches!(create(&path, CACHE_BYTES), Err(DatabaseError::DatabaseAlreadyOpen)));
        // The unwrapped backend must respect the same lock.
        assert!(matches!(Database::open(&path), Err(DatabaseError::DatabaseAlreadyOpen)));
        drop(database);
        let unwrapped = Database::open(&path).unwrap();
        assert!(matches!(open(&path, CACHE_BYTES), Err(DatabaseError::DatabaseAlreadyOpen)));
        drop(unwrapped);
        drop(open(&path, CACHE_BYTES).unwrap());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod concurrent_tests {
    use super::*;
    use redb::{ReadableDatabase, ReadableTableMetadata, TableDefinition};
    use std::sync::{Arc, Barrier};
    const TABLE: TableDefinition<u64, u64> = TableDefinition::new("commits");

    #[test]
    fn concurrent_commits_reopen_atomically_and_release_file_locks() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(4));
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let path = directory.path().join(worker.to_string());
                drop(create(&path, CACHE_BYTES).unwrap());
                let database = open(&path, CACHE_BYTES).unwrap();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    assert!(matches!(
                        Database::open(&path),
                        Err(DatabaseError::DatabaseAlreadyOpen)
                    ));
                    for n in 0..20 {
                        let tx = database.begin_write().unwrap();
                        {
                            let mut table = tx.open_table(TABLE).unwrap();
                            table.insert(n * 2, n).unwrap();
                            table.insert(n * 2 + 1, n).unwrap();
                        }
                        barrier.wait();
                        tx.commit().unwrap();
                        let read = database.begin_read().unwrap();
                        let table = read.open_table(TABLE).unwrap();
                        assert_eq!(table.get(n * 2).unwrap().unwrap().value(), n);
                        assert_eq!(table.get(n * 2 + 1).unwrap().unwrap().value(), n);
                    }
                });
            }
        });
        for worker in 0..4 {
            let database = open(&directory.path().join(worker.to_string()), CACHE_BYTES).unwrap();
            let read = database.begin_read().unwrap();
            let table = read.open_table(TABLE).unwrap();
            assert_eq!(table.len().unwrap(), 40);
            for n in 0..20 {
                assert_eq!(table.get(n * 2).unwrap().unwrap().value(), n);
                assert_eq!(table.get(n * 2 + 1).unwrap().unwrap().value(), n);
            }
        }
    }

    const CACHE_BYTES: usize = 512 * 1024;
    #[test]
    fn sync_keeps_the_open_inode_when_its_path_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        let moved = directory.path().join("moved");
        drop(create(&path, CACHE_BYTES).unwrap());
        let database = open(&path, CACHE_BYTES).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        drop(create(&path, CACHE_BYTES).unwrap());
        let transaction = database.begin_write().unwrap();
        transaction.open_table(TABLE).unwrap().insert(1, 42).unwrap();
        transaction.commit().unwrap();
        drop(database);
        let retained = open(&moved, CACHE_BYTES).unwrap();
        assert_eq!(
            retained
                .begin_read()
                .unwrap()
                .open_table(TABLE)
                .unwrap()
                .get(1)
                .unwrap()
                .unwrap()
                .value(),
            42
        );
        let replacement = open(&path, CACHE_BYTES).unwrap();
        assert!(replacement.begin_read().unwrap().open_table(TABLE).is_err());
    }
}
