//! A grant is distinct from any handle or execution generation using it.
use std::{
    io,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug)]
pub struct Authority {
    write: bool,
    revoked: AtomicBool,
    publication: Mutex<()>,
}
impl Authority {
    pub fn new(write: bool) -> Self {
        Self { write, revoked: AtomicBool::new(false), publication: Mutex::new(()) }
    }
    pub fn check(&self, write: bool) -> io::Result<()> {
        if self.revoked.load(Ordering::Acquire) || write && !self.write {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        } else {
            Ok(())
        }
    }
    /// Ordinary handle close/reload never calls this. Only explicit grant changes
    /// revoke authority. The cutoff serializes against entry into publication.
    pub fn revoke(&self) -> io::Result<()> {
        let _cutoff =
            self.publication.lock().map_err(|_| io::Error::other("publication lock poisoned"))?;
        self.revoked.store(true, Ordering::Release);
        Ok(())
    }
    pub fn admit_publication(&self) -> io::Result<()> {
        let cutoff =
            self.publication.lock().map_err(|_| io::Error::other("publication lock poisoned"))?;
        self.check(true)?;
        drop(cutoff);
        Ok(())
    }
    /// Hold only across bounded guest-memory delivery, never backend I/O.
    pub fn delivery(&self) -> io::Result<MutexGuard<'_, ()>> {
        let cutoff =
            self.publication.lock().map_err(|_| io::Error::other("publication lock poisoned"))?;
        self.check(false)?;
        Ok(cutoff)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert grant ordering and revocation.")]
mod tests {
    use super::*;
    #[test]
    fn publication_and_revocation_have_one_cutoff() {
        let authority = std::sync::Arc::new(Authority::new(true));
        authority.admit_publication().unwrap();
        // The admitted publication may finish after revocation. Admission does
        // not hold an I/O-duration lock needed by revocation or guest delivery.
        authority.revoke().unwrap();
        assert!(authority.check(false).is_err());
        assert!(authority.delivery().is_err());
        assert!(authority.admit_publication().is_err());
        assert!(Authority::new(false).admit_publication().is_err());
    }
}
