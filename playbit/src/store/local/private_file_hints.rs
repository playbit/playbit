//! Recent, process-local notices that an owner can serve a private file.
use super::*;
use crate::store::{
    EntryId,
    volume_index::{EntryAddress, FileAvailability},
};
use std::collections::VecDeque;

const RECENT_HINTS: usize = 32;

#[derive(Default)]
pub(super) struct FileHints {
    through: u64,
    recent: VecDeque<(Id, Option<(EntryId, EntryId)>)>,
}

impl FileHints {
    fn record(&mut self, volume: Id, file: Option<(EntryId, EntryId)>) {
        self.through = self.through.checked_add(1).expect("file hint sequence exhausted");
        if self.recent.len() == RECENT_HINTS {
            self.recent.pop_front();
        }
        self.recent.push_back((volume, file));
    }

    pub(super) fn observation(&self, address: EntryAddress, revision: EntryId) -> FileAvailability {
        let evicted = self.through - self.recent.len() as u64;
        let matched = self.recent.iter().rposition(|(volume, file)| {
            *volume == address.volume && file.is_none_or(|key| key == (address.entry, revision))
        });
        FileAvailability {
            through: self.through,
            invalidated: evicted + matched.map_or(0, |i| i as u64 + 1),
        }
    }
}

impl LocalStore {
    /// A fresh subscription may have missed earlier notices. Later hints identify
    /// the exact revision. Neither establishes local residency or grants Read.
    pub(crate) fn private_file_available(&self, volume: Id, file: Option<(EntryId, EntryId)>) {
        self.private_file_hints
            .lock()
            .expect("private file availability lock")
            .record(volume, file);
        self.changes.observed_changed(Some(volume));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_cover_exact_revisions_reconnects_and_the_retained_window() -> Result<()> {
        let volume = Id::from_bytes([1; 16]);
        let other_volume = Id::from_bytes([2; 16]);
        let entry = EntryId::from_bytes([1; 15])?;
        let revision = EntryId::from_bytes([2; 15])?;
        let other = EntryId::from_bytes([3; 15])?;
        let address = EntryAddress { volume, entry };
        let mut hints = FileHints::default();
        let observation = |hints: &FileHints| hints.observation(address, revision);
        assert_eq!(observation(&hints), FileAvailability::default());
        hints.record(volume, Some((entry, other)));
        hints.record(volume, Some((other, revision)));
        hints.record(other_volume, None);
        hints.record(other_volume, Some((entry, revision)));
        assert_eq!(observation(&hints), FileAvailability { through: 4, invalidated: 0 });
        hints.record(volume, Some((entry, revision)));
        assert_eq!(observation(&hints), FileAvailability { through: 5, invalidated: 5 });
        hints.record(volume, None);
        for sequence in 7..=39 {
            hints.record(other_volume, None);
            assert_eq!(
                observation(&hints),
                FileAvailability {
                    through: sequence,
                    invalidated: 6.max(sequence.saturating_sub(32))
                }
            );
        }
        assert_eq!(hints.recent.len(), RECENT_HINTS);
        Ok(())
    }
}
