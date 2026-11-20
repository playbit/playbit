//! Engine-owned immutable captures. NextAt is an indexed read with no cursor
//! mutation: only the guest frontend can know whether Take delivered output.
use super::{
    authority::Authority,
    listing::{Entry, MAX_BYTES, MAX_ENTRIES},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
const MAX_LISTS: usize = 64;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Closed,
    Revoked,
    LimitExceeded,
    Internal,
    InvalidIndex,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Default)]
struct Usage {
    lists: usize,
    bytes: usize,
}
#[derive(Default, Clone)]
pub struct Budget(Arc<Mutex<Usage>>);
pub struct Capture {
    budget: Budget,
    bytes: usize,
}
impl Budget {
    /// Reserve before starting backend capture, including abandoned work.
    pub fn reserve(&self) -> Result<Capture> {
        let mut usage = self.0.lock().map_err(|_| Error::Internal)?;
        if usage.lists == MAX_LISTS || usage.bytes + MAX_BYTES > MAX_TOTAL_BYTES {
            return Err(Error::LimitExceeded);
        }
        usage.lists += 1;
        usage.bytes += MAX_BYTES;
        Ok(Capture { budget: self.clone(), bytes: MAX_BYTES })
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let mut usage = self.budget.0.lock().expect("listing budget is not poisoned");
        assert!(usage.lists > 0 && usage.bytes >= self.bytes);
        usage.lists -= 1;
        usage.bytes -= self.bytes;
    }
}
pub struct List {
    entries: Vec<Entry>,
    closed: AtomicBool,
    authority: Arc<Authority>,
    _storage: Capture,
}
impl Capture {
    pub fn finish(mut self, entries: Vec<Entry>, authority: Arc<Authority>) -> Result<Arc<List>> {
        if entries.len() > MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        let bytes = entries.iter().try_fold(
            entries
                .capacity()
                .checked_mul(std::mem::size_of::<Entry>())
                .ok_or(Error::LimitExceeded)?,
            |total, entry| total.checked_add(entry.name.capacity()).ok_or(Error::LimitExceeded),
        )?;
        if bytes > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        {
            let mut usage = self.budget.0.lock().map_err(|_| Error::Internal)?;
            usage.bytes -= self.bytes - bytes;
            self.bytes = bytes;
        }
        Ok(Arc::new(List { entries, closed: AtomicBool::new(false), authority, _storage: self }))
    }
}
impl List {
    /// Capture indices come from the frontend's pending reservation. Repeating
    /// a request after disposal or a short Take never skips an entry. The IPC
    /// result owns this bounded copy, independently of the capture's lifetime.
    pub fn next_at(&self, index: u32) -> Result<Option<Entry>> {
        let _delivery = self.authority.delivery().map_err(|_| Error::Revoked)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if index as usize > self.entries.len() {
            return Err(Error::InvalidIndex);
        }
        Ok(self.entries.get(index as usize).cloned())
    }
    pub fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Err(Error::Closed);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert captured listing ownership and retry behavior."
)]
mod tests {
    use super::super::listing::Kind;
    use super::*;
    fn listing(budget: &Budget, authority: &Arc<Authority>) -> Arc<List> {
        budget
            .reserve()
            .unwrap()
            .finish(
                vec![
                    Entry { kind: Kind::File, name: b"first".to_vec() },
                    Entry { kind: Kind::Directory, name: b"second".to_vec() },
                ],
                authority.clone(),
            )
            .unwrap()
    }
    #[test]
    fn indexed_delivery_is_repeatable_and_closure_or_revocation_withholds_metadata() {
        let budget = Budget::default();
        let authority = Arc::new(Authority::new(false));
        let list = listing(&budget, &authority);
        for _ in 0..2 {
            assert_eq!(list.next_at(0).unwrap().unwrap().name, b"first");
            assert_eq!(list.next_at(1).unwrap().unwrap().name, b"second");
            assert_eq!(list.next_at(2), Ok(None));
        }
        assert_eq!(list.next_at(3), Err(Error::InvalidIndex));
        list.close().unwrap();
        assert_eq!(list.next_at(0), Err(Error::Closed));
        assert_eq!(budget.0.lock().unwrap().lists, 1);
        drop(list);
        assert_eq!(budget.0.lock().unwrap().lists, 0);
        let list = listing(&budget, &authority);
        authority.revoke().unwrap();
        assert_eq!(list.next_at(0), Err(Error::Revoked));
        assert_eq!(list.next_at(2), Err(Error::Revoked));
    }
    #[test]
    fn pending_captures_and_uncollected_results_keep_their_budget() {
        let budget = Budget::default();
        let captures =
            (0..MAX_TOTAL_BYTES / MAX_BYTES).map(|_| budget.reserve().unwrap()).collect::<Vec<_>>();
        assert!(matches!(budget.reserve(), Err(Error::LimitExceeded)));
        drop(captures);
        let authority = Arc::new(Authority::new(false));
        let lists = (0..MAX_LISTS).map(|_| listing(&budget, &authority)).collect::<Vec<_>>();
        assert!(matches!(budget.reserve(), Err(Error::LimitExceeded)));
        drop(lists);
        assert!(budget.reserve().is_ok());
    }
}
