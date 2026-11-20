//! Device-local connection intent. This preference never enters replication.
use super::*;
use kvdb::storage::ValueRead;

const KEY: &[u8] = b"settings/work-offline";

impl LocalStore {
    pub fn work_offline(&self) -> Result<bool> {
        let tx = self.metadata.begin_read()?;
        let Some(value) = tx.value_reader(KEY)? else { return Ok(false) };
        if value.len() != 1 {
            return Err(Error::corrupt("Work Offline preference must contain one byte"));
        }
        let mut bytes = [0];
        value.read_exact_at(0, &mut bytes)?;
        match bytes[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::corrupt("Invalid Work Offline preference")),
        }
    }

    pub fn set_work_offline(&self, enabled: bool) -> Result<()> {
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(KEY, &[u8::from(enabled)])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated preference storage.")]
mod tests {
    use super::*;

    #[test]
    fn offline_intent_survives_reopening_and_is_local_to_the_profile() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        {
            let store = LocalStore::open(directory.path()).unwrap();
            assert!(!store.work_offline().unwrap());
            store.set_work_offline(true).unwrap();
        }
        let store = LocalStore::open(directory.path()).unwrap();
        assert!(store.work_offline().unwrap());
        assert!(!LocalStore::open(other.path()).unwrap().work_offline().unwrap());
        store.set_work_offline(false).unwrap();
        assert!(!store.work_offline().unwrap());
    }

    #[test]
    fn malformed_offline_intent_is_reported_without_replacing_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        for bytes in [&[][..], &[2][..], &[1, 0][..]] {
            let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
            tx.put(KEY, bytes).unwrap();
            tx.commit().unwrap();
            assert!(store.work_offline().is_err());
            assert_eq!(
                store.metadata.begin_read().unwrap().get(KEY).unwrap().as_deref(),
                Some(bytes)
            );
        }
    }
}
