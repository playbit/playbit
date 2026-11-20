//! Installation-local presentation preferences. The engine retains the profile
//! writer; these records never enter account or volume replication.
use super::*;

fn key(root: Option<Id>) -> String {
    match root {
        Some(root) => format!("settings/ui-zoom/app/{root}"),
        None => "settings/ui-zoom/management".into(),
    }
}

impl LocalStore {
    pub fn ui_zoom(&self, root: Option<Id>) -> Result<u32> {
        let tx = self.metadata.begin_read()?;
        let Some(value) = tx.value_reader(key(root).as_bytes())? else {
            return Ok(profile::UI_ZOOM_ONE);
        };
        if value.len() != 4 {
            return Err(Error::corrupt("UI zoom must be a four-byte fixed-precision scale"));
        }
        let mut bytes = [0; 4];
        use kvdb::storage::ValueRead;
        value.read_exact_at(0, &mut bytes)?;
        let scale = u32::from_le_bytes(bytes);
        profile::validate_ui_zoom(scale)
            .map_err(|_| Error::corrupt("stored UI scale is outside 50.00–200.00%"))?;
        Ok(scale)
    }

    pub fn set_ui_zoom(&self, root: Option<Id>, scale: u32) -> Result<()> {
        profile::validate_ui_zoom(scale)?;
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(key(root).as_bytes(), &scale.to_le_bytes())?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn zoom_is_local_per_app_and_survives_reopening() {
        // Persist fractional and boundary values, then reopen the durable profile.
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let first = Id::from_bytes([1; 16]);
        let second = Id::from_bytes([2; 16]);
        for scale in [5_000, 10_000, 12_345, 18_765, 20_000] {
            {
                let store = LocalStore::open(directory.path()).unwrap();
                assert_eq!(store.ui_zoom(Some(second)).unwrap(), 10_000);
                store.set_ui_zoom(None, 12_500).unwrap();
                store.set_ui_zoom(Some(first), scale).unwrap();
                assert!(store.set_ui_zoom(Some(first), 20_001).is_err());
            }
            let store = LocalStore::open(directory.path()).unwrap();
            assert_eq!(store.ui_zoom(Some(first)).unwrap(), scale);
            assert_eq!(store.ui_zoom(None).unwrap(), 12_500);
            assert_eq!(LocalStore::open(other.path()).unwrap().ui_zoom(None).unwrap(), 10_000);
        }
    }

    #[test]
    fn damaged_zoom_is_reported_without_replacing_the_record() {
        // Reject both bad lengths and unsupported values at the bounded read boundary.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        for bytes in [&[1][..], &[0, 0, 0, 0][..], &[255, 255, 255, 255][..]] {
            let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
            tx.put(key(None).as_bytes(), bytes).unwrap();
            tx.commit().unwrap();
            assert!(store.ui_zoom(None).is_err());
        }
    }
}
